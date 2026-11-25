//! The state actor.
//!
//! One task owns the Queue, applies commands one at a time and publishes every new State. It
//! also tells the mixer what to play.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::radio::{Clock, Deck, Decoder};
use crate::youtube;

/// What every Listener sees, sent in full on every change.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub queue: Vec<Entry>,
    pub current_index: Option<usize>,
    pub playing: bool,
    /// Stream time of the last next/prev.
    ///
    /// Clients mute their buffered (old) audio until their
    /// playback reaches it, so the change is heard cleanly and in sync.
    pub jump_at: Option<f64>,
}

/// One place in the Queue that refers to a Song.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub uid: Uuid,
    pub video_id: String,
    pub title: String,
    pub duration: Option<f64>,
    pub status: Status,
    /// The audio to play, once ready; never sent to Listeners.
    #[serde(skip)]
    pub file: Option<PathBuf>,
}

/// How far an Entry is from being playable.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Downloading,
    Ready,
    Error,
}

/// A Listener's change to the Radio.
#[derive(Debug)]
pub enum Command {
    /// Append a Song; the ID is already validated.
    Add(String),
    Play,
    Pause,
    Next,
    Prev,
}

/// What the yt-dlp worker reports back.
enum Fetched {
    Metadata(Uuid, youtube::Metadata),
    Downloading(Uuid),
    Done(Uuid, anyhow::Result<PathBuf>),
}

/// The programs and the directory the actor uses.
pub struct Config {
    pub ffmpeg: String,
    pub ytdlp: String,
    pub cache_dir: PathBuf,
}

/// Spawns the state actor and its yt-dlp worker.
///
/// The actor starts from `state`, plays through `deck` and moves on when an Entry's uid arrives
/// on `ended`. Returns its command inbox and the published [`State`].
pub fn spawn(
    config: Config,
    state: State,
    deck: Arc<Mutex<Deck>>,
    clock: Clock,
    mut ended: mpsc::UnboundedReceiver<Uuid>,
) -> (mpsc::Sender<Command>, watch::Receiver<State>) {
    let (cmd_tx, mut commands) = mpsc::channel(64);
    let (fetch_tx, mut fetched) = mpsc::unbounded_channel();
    let (job_tx, jobs) = mpsc::unbounded_channel();
    tokio::spawn(worker(config.ytdlp, config.cache_dir, jobs, fetch_tx));

    let mut actor = Actor {
        state,
        ffmpeg: config.ffmpeg,
        deck,
        clock,
    };
    actor.resume(&job_tx);
    let (state_tx, state_rx) = watch::channel(actor.state.clone());
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(cmd) = commands.recv() => match cmd {
                    Command::Add(video_id) => {
                        let uid = actor.add(video_id.clone());
                        _ = job_tx.send((uid, video_id));
                    }
                    Command::Play => actor.set_playing(true),
                    Command::Pause => actor.set_playing(false),
                    Command::Next => {
                        let target = next(&ready(&actor.state.queue), actor.state.current_index);
                        actor.load(target);
                        actor.state.jump_at = Some(actor.clock.now());
                    }
                    Command::Prev => {
                        let target =
                            prev(&ready(&actor.state.queue), actor.state.current_index, actor.played());
                        actor.load(target);
                        actor.state.jump_at = Some(actor.clock.now());
                    }
                },
                Some(event) = fetched.recv() => actor.fetched(event),
                Some(uid) = ended.recv() => actor.ended(uid),
                else => break,
            }
            state_tx.send_if_modified(|published| {
                let changed = *published != actor.state;
                if changed {
                    published.clone_from(&actor.state);
                }
                changed
            });
        }
    });
    (cmd_tx, state_rx)
}

/// Fetches metadata and audio for the Entries received on `jobs`.
///
/// Works on one Entry at a time, in the order received, and reports progress on `out`.
async fn worker(
    ytdlp: String,
    cache_dir: PathBuf,
    mut jobs: mpsc::UnboundedReceiver<(Uuid, String)>,
    out: mpsc::UnboundedSender<Fetched>,
) {
    while let Some((uid, id)) = jobs.recv().await {
        let result = async {
            let meta = youtube::metadata(&ytdlp, &id).await?;
            _ = out.send(Fetched::Metadata(uid, meta));
            _ = out.send(Fetched::Downloading(uid));
            youtube::download(&ytdlp, &cache_dir, &id).await
        }
        .await;
        if let Err(e) = &result {
            tracing::warn!("{id}: {e:#}");
        }
        _ = out.send(Fetched::Done(uid, result));
    }
}

/// The state actor's data: the [`State`] it owns and what it needs to play it.
struct Actor {
    state: State,
    ffmpeg: String,
    deck: Arc<Mutex<Deck>>,
    clock: Clock,
}

/// How commands, the worker and the mixer change the [`State`].
impl Actor {
    /// Resumes from a restored [`State`].
    ///
    /// Sends every pending Entry to the worker through `jobs`, in Queue order, and loads the
    /// Current entry. If that one has no audio yet, it stays current and [`Actor::fetched`]
    /// loads it later.
    fn resume(&mut self, jobs: &mpsc::UnboundedSender<(Uuid, String)>) {
        for entry in &self.state.queue {
            if entry.status == Status::Pending {
                _ = jobs.send((entry.uid, entry.video_id.clone()));
            }
        }
        let current = self.state.current_index;
        if current.is_some_and(|i| self.state.queue[i].file.is_some()) {
            self.load(current);
        }
    }

    /// Appends a pending Entry for `video_id` to the Queue.
    ///
    /// Returns its uid.
    fn add(&mut self, video_id: String) -> Uuid {
        let uid = Uuid::new_v4();
        self.state.queue.push(Entry {
            uid,
            title: video_id.clone(),
            video_id,
            duration: None,
            status: Status::Pending,
            file: None,
        });
        uid
    }

    /// Applies what the worker reports about an Entry.
    ///
    /// An Entry that becomes ready is loaded if the Radio is idle or it already is the Current
    /// entry (restored without its audio). If the Current entry fails, the Radio moves on to
    /// the next ready one, or goes idle.
    fn fetched(&mut self, event: Fetched) {
        let uid = match &event {
            Fetched::Metadata(uid, _) | Fetched::Downloading(uid) | Fetched::Done(uid, _) => *uid,
        };
        let Some(index) = self.state.queue.iter().position(|e| e.uid == uid) else {
            return;
        };
        let entry = &mut self.state.queue[index];
        match event {
            Fetched::Metadata(_, meta) => {
                entry.title = meta.title;
                entry.duration = meta.duration;
            }
            Fetched::Downloading(_) => entry.status = Status::Downloading,
            Fetched::Done(_, Ok(file)) => {
                entry.status = Status::Ready;
                entry.file = Some(file);
                if self
                    .state
                    .current_index
                    .is_none_or(|current| current == index)
                {
                    // Idle: this Entry becomes current. Paused, it waits for `play`.
                    self.load(Some(index));
                }
            }
            Fetched::Done(_, Err(_)) => {
                entry.status = Status::Error;
                if self.state.current_index == Some(index) {
                    self.load(after(&ready(&self.state.queue), index));
                }
            }
        }
    }

    /// Plays or pauses the Radio.
    ///
    /// Pausing makes the mixer send silence; the decoder waits where it was.
    fn set_playing(&mut self, playing: bool) {
        self.state.playing = playing;
        self.deck.lock().unwrap().playing = playing;
    }

    /// Returns how much of the Current entry has been broadcast.
    fn played(&self) -> Duration {
        let deck = self.deck.lock().unwrap();
        deck.decoder.as_ref().map_or(Duration::ZERO, |d| d.played())
    }

    /// Moves on to the next ready Entry, or goes idle, when the mixer finishes `uid`.
    ///
    /// Ignored if `uid` is no longer the Current entry.
    fn ended(&mut self, uid: Uuid) {
        let Some(current) = self.state.current_index else {
            return;
        };
        if self.state.queue[current].uid != uid {
            return; // a stale report from an Entry already replaced
        }
        self.load(after(&ready(&self.state.queue), current));
    }

    /// Makes `index` the Current entry and loads it from the start.
    ///
    /// Goes idle with `None`, or if the Entry has no audio or its decoder doesn't start.
    fn load(&mut self, index: Option<usize>) {
        let decoder = index.and_then(|i| {
            let entry = &self.state.queue[i];
            let file = entry.file.as_ref()?;
            Decoder::spawn(&self.ffmpeg, file, entry.uid)
                .inspect_err(|e| tracing::error!("{e:#}"))
                .ok()
        });
        self.state.current_index = index.filter(|_| decoder.is_some());
        let old = {
            let mut deck = self.deck.lock().unwrap();
            deck.playing = self.state.playing;
            std::mem::replace(&mut deck.decoder, decoder)
        };
        drop(old); // kill the old decoder outside the mixer's lock
    }
}

/// Returns whether each Entry of `queue` is ready.
fn ready(queue: &[Entry]) -> Vec<bool> {
    queue.iter().map(|e| e.status == Status::Ready).collect()
}

/// Returns the first ready Entry after `current`, for when an Entry ends on its own.
///
/// Doesn't wrap: after the last one, the Queue has ended.
fn after(ready: &[bool], current: usize) -> Option<usize> {
    (current + 1..ready.len()).find(|&i| ready[i])
}

/// Returns the Entry to play on `next`.
///
/// That is the following ready Entry, wrapping to the first; from idle, the first ready one.
/// With only the Current entry ready, that one restarts.
fn next(ready: &[bool], current: Option<usize>) -> Option<usize> {
    let n = ready.len();
    match current {
        None => ready.iter().position(|&r| r),
        Some(i) => (1..=n).map(|k| (i + k) % n).find(|&j| ready[j]),
    }
}

/// How long the Current entry must have played before `prev` restarts it instead.
const RESTART_AFTER: Duration = Duration::from_secs(3);

/// Returns the Entry to play on `prev`.
///
/// If the Current entry has `played` for longer than [`RESTART_AFTER`], it restarts. Otherwise
/// it is the preceding ready Entry, wrapping to the last; from idle, the last ready one.
fn prev(ready: &[bool], current: Option<usize>, played: Duration) -> Option<usize> {
    let n = ready.len();
    match current {
        None => ready.iter().rposition(|&r| r),
        Some(i) if played > RESTART_AFTER => Some(i),
        Some(i) => (1..=n).map(|k| (i + n - k) % n).find(|&j| ready[j]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EARLY: Duration = Duration::from_secs(1);
    const LATE: Duration = Duration::from_secs(4);
    // Entries 1 and 3 aren't ready (pending, downloading or error).
    const R: [bool; 5] = [true, false, true, false, true];

    #[test]
    fn next_skips_unready_and_wraps() {
        assert_eq!(next(&R, Some(0)), Some(2));
        assert_eq!(next(&R, Some(2)), Some(4));
        assert_eq!(next(&R, Some(4)), Some(0));
        assert_eq!(next(&[true], Some(0)), Some(0));
        assert_eq!(next(&[false, true, false], Some(1)), Some(1));
    }

    #[test]
    fn next_from_idle_plays_the_first_ready() {
        assert_eq!(next(&R, None), Some(0));
        assert_eq!(next(&[false, true], None), Some(1));
        assert_eq!(next(&[false, false], None), None);
        assert_eq!(next(&[], None), None);
    }

    #[test]
    fn prev_early_goes_back_skipping_unready_and_wraps() {
        assert_eq!(prev(&R, Some(4), EARLY), Some(2));
        assert_eq!(prev(&R, Some(2), EARLY), Some(0));
        assert_eq!(prev(&R, Some(0), EARLY), Some(4));
        assert_eq!(prev(&[true], Some(0), EARLY), Some(0));
        assert_eq!(prev(&R, Some(2), RESTART_AFTER), Some(0));
    }

    #[test]
    fn prev_late_restarts_the_current() {
        assert_eq!(prev(&R, Some(2), LATE), Some(2));
        assert_eq!(prev(&R, Some(0), LATE), Some(0));
    }

    #[test]
    fn prev_from_idle_plays_the_last_ready() {
        assert_eq!(prev(&R, None, Duration::ZERO), Some(4));
        assert_eq!(prev(&[true, false], None, Duration::ZERO), Some(0));
        assert_eq!(prev(&[false], None, Duration::ZERO), None);
    }

    #[test]
    fn the_queue_ending_on_its_own_does_not_wrap() {
        assert_eq!(after(&R, 0), Some(2));
        assert_eq!(after(&R, 4), None);
        assert_eq!(after(&[true, false], 0), None);
    }
}
