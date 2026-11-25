//! The state file: what survives a restart, written after every change.

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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Saved {
    queue: Vec<SavedEntry>,
    current_index: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedEntry {
    uid: Uuid,
    video_id: String,
    title: String,
    duration: Option<f64>,
    file: Option<PathBuf>,
}

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

impl Saved {
    /// The State to start from. Entries without audio are pending again, and a restored Queue
    /// starts paused so nothing plays until a Listener asks for it.
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

fn parse(json: &[u8]) -> anyhow::Result<Saved> {
    let saved: Saved = serde_json::from_slice(json)?;
    for e in &saved.queue {
        // IDs become yt-dlp arguments and file names.
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

/// The State saved in `path`, or a fresh one. A file that makes no sense is set aside as
/// `<path>.bad`; one that can't be read is an error.
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

/// Saves every published State. Off the actor, so a slow disk never delays commands; changes
/// made during a write are saved together by the next one.
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

/// Replaces `path` atomically: a crash leaves the old file or the new one, never half of one.
fn write(path: &Path, saved: &Saved) -> anyhow::Result<()> {
    let tmp = with_suffix(path, ".tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(saved)?)?;
    // Without this, a power cut could leave the renamed file empty.
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

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
