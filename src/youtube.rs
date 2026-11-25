//! Everything YouTube: turning pasted links into video IDs, and running yt-dlp on those IDs.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;
use tokio::process::Command;
use url::Url;

const HOSTS: [&str; 4] = [
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
];

/// The video ID of a YouTube video link, or `None` for anything else.
///
/// Only an ID passing [`is_video_id`] ever leaves this function, so it is safe to pass to
/// yt-dlp as an argument (after `--`, since real IDs may start with `-`).
pub fn video_id(input: &str) -> Option<String> {
    let url = Url::parse(input.trim()).ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.port().is_some() {
        return None;
    }
    let host = url.host_str()?;
    let segments: Vec<&str> = url.path_segments()?.collect();
    let id = match (host, segments.as_slice()) {
        ("youtu.be", [id]) => (*id).to_string(),
        (h, ["watch"]) if HOSTS.contains(&h) => url
            .query_pairs()
            .find(|(k, _)| k == "v")
            .map(|(_, v)| v.into_owned())?,
        (h, ["shorts", id]) if HOSTS.contains(&h) => (*id).to_string(),
        _ => return None,
    };
    is_video_id(&id).then_some(id)
}

/// `^[A-Za-z0-9_-]{11}$`
pub fn is_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub struct Metadata {
    pub title: String,
    pub duration: Option<f64>,
}

/// `yt-dlp -J --no-playlist -- <id>`
pub async fn metadata(ytdlp: &str, id: &str) -> anyhow::Result<Metadata> {
    #[derive(Deserialize)]
    struct Info {
        title: String,
        duration: Option<f64>,
    }
    let out = run(Command::new(ytdlp).args(["-J", "--no-playlist", "--", id])).await?;
    let info: Info = serde_json::from_slice(&out).context("parsing yt-dlp metadata")?;
    Ok(Metadata {
        title: info.title,
        duration: info.duration,
    })
}

/// Downloads the audio into `cache` and returns the file to play.
pub async fn download(ytdlp: &str, cache: &Path, id: &str) -> anyhow::Result<PathBuf> {
    let template = cache.join(format!("{id}.%(ext)s"));
    let out = run(Command::new(ytdlp)
        .args(["-f", "bestaudio", "-o"])
        .arg(template)
        .args(["--print", "after_move:filepath", "--", id]))
    .await?;
    let out = String::from_utf8(out).context("yt-dlp printed a non-UTF-8 path")?;
    let path = PathBuf::from(out.lines().last().unwrap_or_default().trim());
    if !path.is_file() {
        bail!("yt-dlp did not produce {}", path.display());
    }
    Ok(path)
}

async fn run(cmd: &mut Command) -> anyhow::Result<Vec<u8>> {
    let out = cmd
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("running yt-dlp")?;
    if !out.status.success() {
        bail!(
            "yt-dlp failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::video_id;

    const ID: &str = "dQw4w9WgXcQ";

    #[test]
    fn accepts_video_links() {
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?v=dQw4w9WgXcQ&t=42s",
            "https://m.youtube.com/watch?feature=share&v=dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RDAMVM",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?si=abc",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "  https://WWW.YOUTUBE.COM/watch?v=dQw4w9WgXcQ  ",
        ] {
            assert_eq!(video_id(url).as_deref(), Some(ID), "{url}");
        }
        // Real IDs may start with a dash; yt-dlp gets it after `--`.
        assert_eq!(
            video_id("https://youtu.be/-abcdefghij").as_deref(),
            Some("-abcdefghij")
        );
    }

    #[test]
    fn rejects_everything_else() {
        for url in [
            "",
            "dQw4w9WgXcQ",
            "http://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "ftp://youtube.com/watch?v=dQw4w9WgXcQ",
            "https://evil.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com.evil.com/watch?v=dQw4w9WgXcQ",
            "https://evil.com/youtu.be/dQw4w9WgXcQ",
            "https://notyoutube.com/watch?v=dQw4w9WgXcQ",
            "https://user@youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com:8443/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/watch",
            "https://www.youtube.com/watch?v=",
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQX",
            "https://www.youtube.com/playlist?list=PL123",
            "https://www.youtube.com/@channel",
            "https://youtu.be/dQw4w9WgXcQ/extra",
            "https://youtu.be/",
            // Shell and option injection.
            "https://www.youtube.com/watch?v=dQw4w9WgX;Q",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ;rm -rf /",
            "https://www.youtube.com/watch?v=$(reboot)ab",
            "https://www.youtube.com/watch?v=`id`aaaaaa",
            "https://www.youtube.com/watch?v=--exec rm",
            "https://www.youtube.com/watch?v=--exec=rm-x",
            "https://youtu.be/dQw4w9WgXc%3B",
            "https://youtu.be/dQw4w9WgXc%0A",
            // Path traversal.
            "https://youtu.be/../etc/passwd",
            "https://youtu.be/..%2F..%2Fetc",
            "https://www.youtube.com/shorts/../../dQw4w9WgXcQ",
        ] {
            assert_eq!(video_id(url), None, "{url}");
        }
    }
}
