// SPDX-FileCopyrightText: 2026 Lorena Jiménez, Luis Liñán
// SPDX-License-Identifier: MIT

//! The state actor.
//!
//! One task owns the Queue, applies commands one at a time and publishes every new State. It
//! also tells the mixer what to play.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::RngExt;
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::radio::{Clock, Deck, Decoder};
use crate::youtube;

/// What every Listener sees, sent in full on every change.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    /// The Entries, in order.
    pub queue: Vec<Entry>,
    /// Position of the Current entry in `queue`; `None` when idle.
    pub current_index: Option<usize>,
    /// Whether the Radio is not paused; it can be true while idle.
    pub playing: bool,
    /// What repeats when the Current entry finishes.
    pub r#loop: LoopMode,
    /// Playback position in the Current entry, in seconds.
    pub position_sec: f64,
    /// Server stream time at which `position_sec` was sampled.
    pub at: f64,
    /// Stream time of the last next/prev/seek.
    ///
    /// Clients mute their buffered (old) audio until their
    /// playback reaches it, so the change is heard cleanly and in sync.
    pub jump_at: Option<f64>,
}

/// What repeats when the Current entry finishes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoopMode {
    /// Play the Queue in order and go idle at the end.
    #[default]
    Off,
    /// Start the Queue over at the end.
    Queue,
    /// Replay the Current entry.
    Song,
}

impl LoopMode {
    /// Reads a loop mode, including the original boolean setting.
    ///
    /// # Errors
    /// Fails if the input is neither a loop mode nor a boolean.
    pub fn read<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The current and original formats.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Setting {
            /// A named loop mode.
            Mode(LoopMode),
            /// Whether Queue looping was enabled.
            Enabled(bool),
        }

        Ok(match Setting::deserialize(deserializer)? {
            Setting::Mode(mode) => mode,
            Setting::Enabled(true) => Self::Queue,
            Setting::Enabled(false) => Self::Off,
        })
    }

    /// Advances to the next mode on a press of the Loop button.
    fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Queue,
            Self::Queue => Self::Song,
            Self::Song => Self::Off,
        }
    }
}

/// Changes to the [`State`] that don't need the Actor.
impl State {
    /// Changes the order of the Entries after the Current entry when at least two remain.
    fn shuffle_upcoming(&mut self, rng: &mut impl rand::Rng) {
        let Some(current) = self.current_index else {
            return;
        };
        let upcoming = &mut self.queue[current + 1..];
        if upcoming.len() < 2 {
            return;
        }

        loop {
            // A Fisher–Yates pass changes the order as soon as a swap fixes an Entry
            // in a new position. Retry passes that leave every Entry in place.
            let mut changed = false;
            for index in (1..upcoming.len()).rev() {
                let other = rng.random_range(0..=index);
                if index != other {
                    upcoming.swap(index, other);
                    changed = true;
                }
            }
            if changed {
                return;
            }
        }
    }
}

/// One place in the Queue that refers to a Song.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// Identifies the Entry; the same Song can be in several.
    pub uid: Uuid,
    /// The Song.
    pub video_id: String,
    /// The Song's title, or its video ID until the metadata is fetched.
    pub title: String,
    /// Length in seconds; `None` until known.
    pub duration: Option<f64>,
    /// How far it is from being playable.
    pub status: Status,
    /// The audio to play, once ready; never sent to Listeners.
    #[serde(skip)]
    pub file: Option<PathBuf>,
}

/// How far an Entry is from being playable.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Waiting for the worker, which a restored Entry without audio does again.
    Pending,
    /// The metadata is known and the audio is being downloaded.
    Downloading,
    /// The audio can be played.
    Ready,
    /// The metadata or audio couldn't be fetched; tried again after a restart.
    Error,
}

/// A Listener's change to the Radio.
#[derive(Debug)]
pub enum Command {
    /// Append a Song; the ID is already validated.
    Add(String),
    /// Resume the Current entry where it was.
    Play,
    /// Broadcast silence until `Play`.
    Pause,
    /// Move to the following ready Entry, as in [`next`].
    Next,
    /// Restart the Current entry or move to the preceding ready one, as in [`prev`].
    Prev,
    /// Shuffle the Entries after the Current entry.
    Shuffle,
    /// Set the Loop mode.
    SetLoop(LoopMode),
    /// Advance to the next Loop mode.
    CycleLoop,
    /// Jump to a position in the Current entry, in seconds.
    Seek(f64),
}

/// What the yt-dlp worker reports back.
enum Fetched {
    /// The metadata of the Entry with this uid.
    Metadata(Uuid, youtube::Metadata),
    /// The Entry's audio is being downloaded.
    Downloading(Uuid),
    /// The Entry's audio file, or why it couldn't be fetched.
    Done(Uuid, anyhow::Result<PathBuf>),
}

/// The programs and the directory the actor uses.
pub struct Config {
    /// The ffmpeg binary.
    pub ffmpeg: String,
    /// The yt-dlp binary.
    pub ytdlp: String,
    /// Where audio is downloaded.
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
    actor.state.position_sec = actor.played().as_secs_f64();
    actor.state.at = actor.clock.now();
    let (state_tx, state_rx) = watch::channel(actor.state.clone());
    tokio::spawn(async move {
        loop {
            let mut entry_finished = false;
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
                    Command::Shuffle => actor.state.shuffle_upcoming(&mut rand::rng()),
                    Command::SetLoop(mode) => actor.state.r#loop = mode,
                    Command::CycleLoop => actor.state.r#loop = actor.state.r#loop.cycle(),
                    Command::Seek(position_sec) => actor.seek(position_sec),
                },
                Some(event) = fetched.recv() => actor.fetched(event),
                Some(uid) = ended.recv() => entry_finished = actor.ended(uid),
                else => break,
            }
            state_tx.send_if_modified(|published| {
                // Replaying the same Entry needs a fresh position sample.
                if !entry_finished && *published == actor.state {
                    return false;
                }
                actor.state.position_sec = actor.played().as_secs_f64();
                actor.state.at = actor.clock.now();
                published.clone_from(&actor.state);
                true
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
    /// The current State, published after every command or event.
    state: State,
    /// The ffmpeg binary.
    ffmpeg: String,
    /// What the mixer plays, shared with it.
    deck: Arc<Mutex<Deck>>,
    /// Stream time, to record jumps.
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
                    self.load(after(&ready(&self.state.queue), index, false));
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

    /// Replays or moves on when the mixer finishes `uid`, according to the Loop mode.
    ///
    /// Returns whether the end report was applied.
    fn ended(&mut self, uid: Uuid) -> bool {
        let Some(current) = self.state.current_index else {
            return false;
        };
        if self.state.queue[current].uid != uid || self.deck.lock().unwrap().decoder.is_some() {
            return false; // a stale report from an Entry already replaced
        }
        let target = match self.state.r#loop {
            LoopMode::Song => Some(current),
            mode => after(&ready(&self.state.queue), current, mode == LoopMode::Queue),
        };
        self.load(target);
        true
    }

    /// Makes `index` the Current entry and loads it from the start.
    ///
    /// Goes idle with `None`, or if the Entry has no audio or its decoder doesn't start.
    fn load(&mut self, index: Option<usize>) {
        let decoder = index.and_then(|i| self.open_decoder(i, Duration::ZERO));
        self.state.current_index = index.filter(|_| decoder.is_some());
        self.replace_decoder(decoder);
    }

    /// Changes the Current entry's position while preserving its play or pause state.
    fn seek(&mut self, position_sec: f64) {
        let Some(index) = self.state.current_index else {
            return;
        };
        let Some(position) = seek_position(position_sec, self.state.queue[index].duration) else {
            return;
        };
        if let Some(decoder) = self.open_decoder(index, position) {
            self.replace_decoder(Some(decoder));
            self.state.position_sec = position.as_secs_f64();
            self.state.jump_at = Some(self.clock.now());
        }
    }

    /// Starts a decoder at `position` for the Entry at `index`.
    fn open_decoder(&self, index: usize, position: Duration) -> Option<Decoder> {
        let entry = &self.state.queue[index];
        let file = entry.file.as_ref()?;
        Decoder::spawn(&self.ffmpeg, file, entry.uid, position)
            .inspect_err(|e| tracing::error!("{e:#}"))
            .ok()
    }

    /// Switches what the mixer plays without holding its lock while stopping the old decoder.
    fn replace_decoder(&self, decoder: Option<Decoder>) {
        let old = {
            let mut deck = self.deck.lock().unwrap();
            deck.playing = self.state.playing;
            std::mem::replace(&mut deck.decoder, decoder)
        };
        drop(old); // kill the old decoder outside the mixer's lock
    }
}

/// Clamps a seek to the known duration of the Song.
fn seek_position(position_sec: f64, duration: Option<f64>) -> Option<Duration> {
    let duration = duration.filter(|duration| duration.is_finite() && *duration > 0.0)?;
    Duration::try_from_secs_f64(position_sec.clamp(0.0, duration)).ok()
}

/// Returns whether each Entry of `queue` is ready.
fn ready(queue: &[Entry]) -> Vec<bool> {
    queue.iter().map(|e| e.status == Status::Ready).collect()
}

/// Returns the next ready Entry, optionally starting over at the end of the Queue.
fn after(ready: &[bool], current: usize, wrap: bool) -> Option<usize> {
    let following = (current + 1..ready.len()).find(|&i| ready[i]);
    if wrap && following.is_none() {
        return ready.iter().position(|&r| r);
    }
    following
}

/// Returns the Entry to play on `next`.
///
/// That is the following ready Entry, wrapping to the first; from idle, the first ready one.
/// With only the Current entry ready, that one restarts.
fn next(ready: &[bool], current: Option<usize>) -> Option<usize> {
    match current {
        None => ready.iter().position(|&r| r),
        Some(i) => after(ready, i, true),
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
    use rand::{SeedableRng, rngs::StdRng};

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
        assert_eq!(after(&R, 0, false), Some(2));
        assert_eq!(after(&R, 4, false), None);
        assert_eq!(after(&[true, false], 0, false), None);
    }

    #[test]
    fn loop_starts_over_at_the_first_ready_entry() {
        assert_eq!(after(&R, 0, true), Some(2));
        assert_eq!(after(&R, 4, true), Some(0));
        assert_eq!(after(&[false, true, true], 2, true), Some(1));
        assert_eq!(after(&[true], 0, true), Some(0));
        assert_eq!(after(&[false], 0, true), None);
    }

    fn shuffle_state(current_index: Option<usize>) -> State {
        let queue = (0..6)
            .map(|i| Entry {
                uid: Uuid::from_u128(i),
                video_id: "aaaaaaaaaaa".into(),
                title: "A Song".into(),
                duration: None,
                status: Status::Pending,
                file: None,
            })
            .collect();
        State {
            queue,
            current_index,
            playing: true,
            jump_at: Some(1.0),
            ..State::default()
        }
    }

    #[test]
    fn shuffle_preserves_the_prefix_and_keeps_all_entries() {
        let original = shuffle_state(Some(1));
        let mut shuffled = original.clone();
        shuffled.shuffle_upcoming(&mut StdRng::seed_from_u64(42));

        assert_eq!(shuffled.current_index, original.current_index);
        assert_eq!(shuffled.playing, original.playing);
        assert_eq!(shuffled.jump_at, original.jump_at);
        assert_eq!(shuffled.queue[..2], original.queue[..2]);
        assert_ne!(shuffled.queue[2..], original.queue[2..]);
        shuffled.queue[2..].sort_by_key(|entry| entry.uid);
        assert_eq!(shuffled.queue, original.queue);
    }

    #[test]
    fn each_shuffle_changes_the_upcoming_order() {
        let mut rng = StdRng::seed_from_u64(42);
        for current in 0..4 {
            let mut state = shuffle_state(Some(current));
            for _ in 0..32 {
                let before = state.queue.clone();
                state.shuffle_upcoming(&mut rng);
                assert_eq!(state.queue[..=current], before[..=current]);
                assert_ne!(state.queue[current + 1..], before[current + 1..]);
            }
        }
    }

    #[test]
    fn shuffle_leaves_the_queue_alone_when_fewer_than_two_entries_are_upcoming() {
        for current in [None, Some(4), Some(5)] {
            let original = shuffle_state(current);
            let mut shuffled = original.clone();
            shuffled.shuffle_upcoming(&mut StdRng::seed_from_u64(42));
            assert_eq!(shuffled, original);
        }
    }

    #[test]
    fn seek_stays_within_the_known_duration() {
        assert_eq!(seek_position(-2.0, Some(10.0)), Some(Duration::ZERO));
        assert_eq!(
            seek_position(4.25, Some(10.0)),
            Some(Duration::from_millis(4250))
        );
        assert_eq!(
            seek_position(12.0, Some(10.0)),
            Some(Duration::from_secs(10))
        );
        assert_eq!(seek_position(5.0, None), None);
        assert_eq!(seek_position(5.0, Some(0.0)), None);
        assert_eq!(seek_position(f64::NAN, Some(10.0)), None);
    }
}
