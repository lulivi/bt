//! The state actor: one task owns the Queue, applies commands one at a time and publishes
//! every new State. It also tells the mixer what to play.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::radio::{Deck, Decoder};
use crate::youtube;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub queue: Vec<Entry>,
    pub current_index: Option<usize>,
    pub playing: bool,
    /// Bumped whenever a Listener moves playback (next/prev), so every client can drop its
    /// buffered audio and jump to the live edge instead of hearing the change late.
    pub jump: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub uid: Uuid,
    pub video_id: String,
    pub title: String,
    pub duration: Option<f64>,
    pub status: Status,
    #[serde(skip)]
    file: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Downloading,
    Ready,
    Error,
}

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

pub struct Config {
    pub ffmpeg: String,
    pub ytdlp: String,
    pub cache_dir: PathBuf,
}

/// Spawns the actor; returns its command inbox and the published State.
pub fn spawn(
    config: Config,
    deck: Arc<Mutex<Deck>>,
    mut ended: mpsc::UnboundedReceiver<Uuid>,
) -> (mpsc::Sender<Command>, watch::Receiver<State>) {
    let (cmd_tx, mut commands) = mpsc::channel(64);
    let (state_tx, state_rx) = watch::channel(State {
        playing: true,
        ..State::default()
    });
    let (fetch_tx, mut fetched) = mpsc::unbounded_channel();
    let (job_tx, jobs) = mpsc::unbounded_channel();
    tokio::spawn(worker(config.ytdlp, config.cache_dir, jobs, fetch_tx));

    let mut actor = Actor {
        state: state_tx.borrow().clone(),
        ffmpeg: config.ffmpeg,
        deck,
    };
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
                        actor.state.jump += 1;
                    }
                    Command::Prev => {
                        let target =
                            prev(&ready(&actor.state.queue), actor.state.current_index, actor.played());
                        actor.load(target);
                        actor.state.jump += 1;
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

/// Fetches metadata and downloads, one Entry at a time, in Queue order.
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

struct Actor {
    state: State,
    ffmpeg: String,
    deck: Arc<Mutex<Deck>>,
}

impl Actor {
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
                if self.state.current_index.is_none() {
                    // Idle: this Entry becomes current. Paused, it waits for `play`.
                    self.load(Some(index));
                }
            }
            Fetched::Done(_, Err(_)) => entry.status = Status::Error,
        }
    }

    /// Pausing makes the mixer send silence; the decoder waits where it was.
    fn set_playing(&mut self, playing: bool) {
        self.state.playing = playing;
        self.deck.lock().unwrap().playing = playing;
    }

    fn played(&self) -> Duration {
        let deck = self.deck.lock().unwrap();
        deck.decoder.as_ref().map_or(Duration::ZERO, |d| d.played())
    }

    /// The mixer finished an Entry: move on to the next ready one, or go idle.
    fn ended(&mut self, uid: Uuid) {
        let Some(current) = self.state.current_index else {
            return;
        };
        if self.state.queue[current].uid != uid {
            return; // a stale report from an Entry already replaced
        }
        self.load(after(&ready(&self.state.queue), current));
    }

    /// Makes `index` the Current entry and plays it from the start; `None` goes idle.
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

fn ready(queue: &[Entry]) -> Vec<bool> {
    queue.iter().map(|e| e.status == Status::Ready).collect()
}

/// The first ready Entry after `current`, without wrapping (the Queue ending on its own).
fn after(ready: &[bool], current: usize) -> Option<usize> {
    (current + 1..ready.len()).find(|&i| ready[i])
}

/// `next`: the following ready Entry, wrapping to the first; from idle, the first ready one.
/// With only the current Entry ready, that one restarts.
fn next(ready: &[bool], current: Option<usize>) -> Option<usize> {
    let n = ready.len();
    match current {
        None => ready.iter().position(|&r| r),
        Some(i) => (1..=n).map(|k| (i + k) % n).find(|&j| ready[j]),
    }
}

/// How long the Current entry must have played before `prev` restarts it instead.
const RESTART_AFTER: Duration = Duration::from_secs(3);

/// `prev`: past [`RESTART_AFTER`], restart the Current entry; before it, the preceding ready
/// Entry, wrapping to the last; from idle, the last ready one.
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
