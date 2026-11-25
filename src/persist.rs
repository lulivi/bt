//! The state file: what survives a restart.
//!
//! [`load`] reads it on startup and [`save_changes`] rewrites it after every change.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use uuid::Uuid;

use crate::state::{Entry, State, Status};
use crate::youtube;

/// The contents of the state file.
///
/// Holds only what can't be worked out again on startup. Missing fields get their default and
/// unknown ones are ignored, so files from other versions still load.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Saved {
    /// The Entries, in order.
    queue: Vec<SavedEntry>,
    /// Position of the Current entry in `queue`; `None` when idle.
    current_index: Option<usize>,
}

/// An [`Entry`] as saved in the state file.
///
/// Unlike the [`Entry`] sent to Listeners, it has the audio `file` and no status, which
/// [`Saved::restore`] derives from that file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedEntry {
    /// Identifies the Entry, across restarts too.
    uid: Uuid,
    /// The Song; checked by [`parse`].
    video_id: String,
    /// The Song's title, or its video ID until the metadata is fetched.
    title: String,
    /// Length in seconds; `None` until known.
    duration: Option<f64>,
    /// The downloaded audio; `None` until ready.
    file: Option<PathBuf>,
}

/// Takes the part of a [`State`] that is saved.
impl From<&State> for Saved {
    fn from(state: &State) -> Self {
        let queue = state.queue.iter().map(|e| SavedEntry {
            uid: e.uid,
            video_id: e.video_id.clone(),
            title: e.title.clone(),
            duration: e.duration,
            file: e.file.clone(),
        });
        Saved {
            queue: queue.collect(),
            current_index: state.current_index,
        }
    }
}

/// Turns saved contents back into a [`State`].
impl Saved {
    /// Builds the [`State`] to start from.
    ///
    /// Entries whose file `exists` are ready and all others are pending again. A Queue with
    /// Entries starts paused, so nothing plays until a Listener asks for it.
    fn restore(self, exists: impl Fn(&Path) -> bool) -> State {
        let restored = |e: SavedEntry| {
            let file = e.file.filter(|f| exists(f));
            Entry {
                uid: e.uid,
                video_id: e.video_id,
                title: e.title,
                duration: e.duration,
                status: match file {
                    Some(_) => Status::Ready,
                    None => Status::Pending,
                },
                file,
            }
        };
        State {
            playing: self.queue.is_empty(),
            queue: self.queue.into_iter().map(restored).collect(),
            current_index: self.current_index,
            jump_at: None, // stream time starts over
        }
    }
}

/// Parses and validates the contents of a state file.
///
/// # Errors
///
/// Fails if `json` isn't a valid [`Saved`], a video ID is invalid (IDs become yt-dlp arguments
/// and file names) or the current index is outside the Queue.
fn parse(json: &[u8]) -> anyhow::Result<Saved> {
    let saved: Saved = serde_json::from_slice(json)?;
    for e in &saved.queue {
        ensure!(
            youtube::is_video_id(&e.video_id),
            "invalid video ID {:?}",
            e.video_id
        );
    }
    if let Some(i) = saved.current_index {
        ensure!(i < saved.queue.len(), "currentIndex {i} is out of range");
    }
    Ok(saved)
}

/// Loads the [`State`] saved in `path`.
///
/// Returns a fresh one if there is no file, or if its contents are invalid; in that case the
/// file is moved to `<path>.bad`, replacing any earlier one.
///
/// # Errors
///
/// Fails if the file exists but can't be read, or is invalid and can't be moved.
pub fn load(path: &Path) -> anyhow::Result<State> {
    let saved = match fs::read(path) {
        Ok(json) => parse(&json),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Saved::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let saved = match saved {
        Ok(saved) => saved,
        Err(e) => {
            let bad = with_suffix(path, ".bad");
            tracing::error!(
                "invalid state file, moving it to {} and starting empty: {e:#}",
                bad.display()
            );
            fs::rename(path, &bad).with_context(|| format!("moving {}", path.display()))?;
            Saved::default()
        }
    };
    Ok(saved.restore(Path::is_file))
}

/// Saves the published `states` to `path` until the state actor stops.
///
/// Runs apart from the actor, so a slow disk never delays commands. Changes made during a write
/// are saved together by the next one, and changes to nothing that is saved (like pausing) are
/// skipped. A failed write is logged and tried again on the next change.
pub async fn save_changes(path: PathBuf, mut states: watch::Receiver<State>) {
    let mut last = Saved::from(&*states.borrow());
    while states.changed().await.is_ok() {
        let saved = Saved::from(&*states.borrow_and_update());
        if saved == last {
            continue; // e.g. play or pause: nothing that is saved
        }
        let (to, data) = (path.clone(), saved.clone());
        match tokio::task::spawn_blocking(move || write(&to, &data)).await {
            Ok(Ok(())) => last = saved,
            // Keeps `last`, so the next change tries again.
            Ok(Err(e)) => tracing::error!("saving {}: {e:#}", path.display()),
            Err(e) => tracing::error!("saving {}: {e}", path.display()),
        }
    }
}

/// Replaces the file at `path` with `saved`, atomically.
///
/// Writes and syncs `<path>.tmp`, then renames it, so a crash leaves the old file or the new
/// one, never half of one.
///
/// # Errors
///
/// Fails if the temporary file can't be written or renamed; `path` is then unchanged.
fn write(path: &Path, saved: &Saved) -> anyhow::Result<()> {
    let tmp = with_suffix(path, ".tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(saved)?)?;
    // Without this, a power cut could leave the renamed file empty.
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Appends `suffix` to the file name of `path`, keeping its extension.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path);
    name.push(suffix);
    name.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{
        "queue": [
            {"uid": "00000000-0000-0000-0000-000000000001", "videoId": "aaaaaaaaaaa",
             "title": "A", "duration": 2.0, "file": "here.webm"},
            {"uid": "00000000-0000-0000-0000-000000000002", "videoId": "bbbbbbbbbbb",
             "title": "B", "duration": null, "file": "gone.webm"},
            {"uid": "00000000-0000-0000-0000-000000000003", "videoId": "ccccccccccc",
             "title": "C", "duration": null, "file": null}
        ],
        "currentIndex": 1,
        "someFutureField": true
    }"#;

    #[test]
    fn restores_paused_with_missing_audio_pending() {
        let saved = parse(JSON.as_bytes()).unwrap();
        let state = saved.restore(|f| f == Path::new("here.webm"));
        let statuses: Vec<_> = state.queue.iter().map(|e| e.status).collect();
        assert_eq!(
            statuses,
            [Status::Ready, Status::Pending, Status::Pending],
            "{state:?}"
        );
        assert_eq!(state.queue[0].file.as_deref(), Some(Path::new("here.webm")));
        assert_eq!(state.queue[1].file, None);
        assert_eq!(state.current_index, Some(1));
        assert!(!state.playing);
    }

    #[test]
    fn an_empty_queue_starts_playing() {
        assert!(Saved::default().restore(|_| true).playing);
        assert!(parse(b"{}").unwrap().restore(|_| true).playing);
    }

    #[test]
    fn rejects_invalid_files() {
        for bad in [
            String::new(),
            "{".into(),
            JSON.replace("aaaaaaaaaaa", "../../x.y.z"),
            JSON.replace("aaaaaaaaaaa", "--exec"),
            JSON.replace("\"currentIndex\": 1", "\"currentIndex\": 3"),
        ] {
            assert!(parse(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_invalid_file_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "nonsense").unwrap();
        assert_eq!(load(&path).unwrap().queue, []);
        assert!(!path.exists());
        let bad = fs::read_to_string(dir.path().join("state.json.bad")).unwrap();
        assert_eq!(bad, "nonsense");
    }

    #[test]
    fn written_files_load_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let saved = parse(JSON.as_bytes()).unwrap();
        write(&path, &saved).unwrap();
        assert_eq!(parse(&fs::read(&path).unwrap()).unwrap(), saved);
        assert!(!dir.path().join("state.json.tmp").exists());
    }

    #[test]
    fn a_failed_read_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_err()); // a directory, not a file
    }
}
