// SPDX-FileCopyrightText: 2026 Lorena Jiménez, Luis Liñán
// SPDX-License-Identifier: MIT

//! End-to-end tests: run the real `bt` binary with real ffmpeg and `tests/fake-yt-dlp.sh`.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

/// A running `bt` and its files, killed on drop.
struct Bt {
    child: Child,
    addr: String,
    dir: TempDir,
    seconds: u32,
    /// Extra environment variables `bt` runs with.
    env: Vec<(&'static str, String)>,
}

/// Kills `bt`, and prints its stderr if the test is failing.
impl Drop for Bt {
    fn drop(&mut self) {
        _ = self.child.kill();
        _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!("bt's stderr:\n{}", stderr(&self.dir));
        }
    }
}

/// Starts, stops and inspects `bt`.
impl Bt {
    /// Starts `bt` on a free port, with a fresh cache and state file.
    ///
    /// Each fake Song lasts `seconds`.
    fn start(seconds: u32) -> Self {
        Self::start_with(seconds, Vec::new())
    }

    /// Starts `bt` like [`Bt::start`], with the extra environment variables `env`.
    fn start_with(seconds: u32, env: Vec<(&'static str, String)>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (child, addr) = Self::spawn(&dir, seconds, &env);
        Bt {
            child,
            addr,
            dir,
            seconds,
            env,
        }
    }

    /// Kills `bt` and starts it again with the same files.
    ///
    /// `between` gets their directory and runs while `bt` is down.
    fn restart(&mut self, between: impl FnOnce(&Path)) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        between(self.dir.path());
        (self.child, self.addr) = Self::spawn(&self.dir, self.seconds, &self.env);
    }

    /// Spawns `bt` with its files in `dir` and the extra variables `env`, and waits until it
    /// listens.
    ///
    /// Its stderr (and its ffmpegs') goes to a file, as ffmpeg always complains when `bt` is
    /// killed. Returns the process and the address it listens on.
    ///
    /// # Panics
    ///
    /// Panics with that stderr if `bt` exits before listening.
    fn spawn(dir: &TempDir, seconds: u32, env: &[(&str, String)]) -> (Child, String) {
        let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-yt-dlp.sh");
        let log = File::options()
            .create(true)
            .append(true)
            .open(dir.path().join("stderr.log"))
            .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_bt"))
            .env("BT_ADDR", "127.0.0.1:0")
            .env("BT_CACHE_DIR", dir.path().join("cache"))
            .env("BT_STATE_FILE", dir.path().join("data/state.json"))
            .env("BT_YTDLP", fake)
            .env("FAKE_YTDLP_LOG", dir.path().join("yt-dlp.log"))
            .env("FAKE_YTDLP_SECONDS", seconds.to_string())
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .expect("spawn bt");
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let addr = loop {
            let line = lines
                .next()
                .unwrap_or_else(|| panic!("bt exited before listening:\n{}", stderr(dir)))
                .unwrap();
            if let Some((_, addr)) = line.split_once("listening on ") {
                break addr.trim().to_string();
            }
        };
        // Keep draining the log so bt never blocks on a full pipe.
        std::thread::spawn(move || lines.for_each(drop));
        (child, addr)
    }

    /// Returns the file where the fake yt-dlp logs its calls.
    fn ytdlp_log(&self) -> PathBuf {
        self.dir.path().join("yt-dlp.log")
    }

    /// Waits (up to 15 s) for the state file to match `want`.
    ///
    /// The file is written in the background, so it can lag behind the published State.
    ///
    /// # Panics
    ///
    /// Panics with `what` if it doesn't match in time.
    async fn saved(&self, what: &str, want: impl Fn(&Value) -> bool) {
        let path = self.dir.path().join("data/state.json");
        for _ in 0..300 {
            let json = std::fs::read(&path).unwrap_or_default();
            if serde_json::from_slice(&json).is_ok_and(|saved| want(&saved)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {what} to be saved");
    }

    /// Connects a new Listener.
    async fn ws(&self) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", self.addr))
            .await
            .expect("connect /ws");
        Ws(ws)
    }
}

/// A Listener's WebSocket.
struct Ws(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
);

/// Sends commands and waits for messages.
impl Ws {
    /// Sends `msg` as JSON.
    async fn send(&mut self, msg: Value) {
        self.0.send(Message::text(msg.to_string())).await.unwrap();
    }

    /// Waits for the next message.
    async fn recv(&mut self) -> Value {
        loop {
            let msg = self.0.next().await.expect("ws closed").expect("ws error");
            if let Message::Text(text) = msg {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    /// Waits (up to 15 s) for a message matching `want` and returns it.
    ///
    /// # Panics
    ///
    /// Panics with `what` if none arrives in time.
    async fn until(&mut self, what: &str, want: impl Fn(&Value) -> bool) -> Value {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let msg = self.recv().await;
                if want(&msg) {
                    return msg;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    /// Adds the link `url` to the Queue.
    async fn add(&mut self, url: &str) {
        self.send(json!({"type": "add", "url": url})).await;
    }
}

/// Returns what the `bt` with its files in `dir` has written to stderr, across restarts.
fn stderr(dir: &TempDir) -> String {
    let log = std::fs::read(dir.path().join("stderr.log")).unwrap_or_default();
    String::from_utf8_lossy(&log).into_owned()
}

/// Returns the status of the Entry `i`, or an empty string if there is none.
fn status(state: &Value, i: usize) -> &str {
    state["queue"][i]["status"].as_str().unwrap_or_default()
}

/// Requests `path` and reads at least `body_bytes` of the response's body.
///
/// Returns the lower-cased header block and the body bytes read so far.
fn get(addr: &str, path: &str, body_bytes: usize) -> (String, Vec<u8>) {
    get_with(addr, path, "", body_bytes)
}

/// Requests `path` like [`get`], with extra headers.
///
/// `headers` are raw header lines, each ending in `\r\n`.
fn get_with(addr: &str, path: &str, headers: &str, body_bytes: usize) -> (String, Vec<u8>) {
    let mut tcp = TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        tcp,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nRange: bytes=0-1\r\n{headers}\r\n"
    )
    .unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = tcp.read(&mut chunk).expect("read response");
        assert!(n > 0, "connection closed early");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n")
            && buf.len() >= end + 4 + body_bytes
        {
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            return (head, buf[end + 4..].to_vec());
        }
    }
}

#[test]
fn stream_is_endless_mp3() {
    let bt = Bt::start(2);
    let (head, body) = get(&bt.addr, "/stream", 16 * 1024);
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("content-type: audio/mpeg"), "{head}");
    assert!(head.contains("cache-control: no-store"), "{head}");
    assert!(!head.contains("content-length"), "{head}");
    // MP3 frame sync: 11 set bits. The body is chunked-encoded, which doesn't matter for a scan.
    let frames = body
        .windows(2)
        .filter(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0)
        .count();
    assert!(
        frames > 5,
        "only {frames} frame syncs in {} bytes",
        body.len()
    );
}

#[test]
fn serves_the_page() {
    let bt = Bt::start(2);
    let (head, body) = get(&bt.addr, "/", 1);
    assert!(head.contains("content-type: text/html"), "{head}");
    assert!(String::from_utf8_lossy(&body).contains("<!doctype html>"));
}

#[tokio::test]
async fn pasted_links_play_in_sequence() {
    let bt = Bt::start(2);
    let mut ws = bt.ws().await;
    let first = ws.recv().await;
    assert_eq!(first["type"], "state");
    assert_eq!(first["queue"], json!([]));
    assert_eq!(first["currentIndex"], Value::Null);
    assert_eq!(first["playing"], true);
    assert_eq!(first["jumpAt"], Value::Null);
    assert!(first["clock"].as_f64().is_some());

    ws.add("https://www.youtube.com/watch?v=aaaaaaaaaaa").await;
    let pending = ws.until("an entry", |s| s["queue"][0].is_object()).await;
    // `watch` coalesces updates, so a slow run may already show the download.
    assert!(
        ["pending", "downloading"].contains(&status(&pending, 0)),
        "{pending}"
    );
    assert_eq!(pending["queue"][0]["videoId"], "aaaaaaaaaaa");
    ws.add("https://youtu.be/unavailable").await;
    ws.add("https://youtu.be/bbbbbbbbbbb").await;

    let playing = ws
        .until("the first entry playing", |s| s["currentIndex"] == 0)
        .await;
    assert_eq!(status(&playing, 0), "ready");
    assert_eq!(playing["queue"][0]["title"], "Song aaaaaaaaaaa");
    assert_eq!(playing["queue"][0]["duration"], 2.0);

    // The unavailable video is marked and skipped; the third plays after the first ends.
    let next = ws
        .until("the third entry playing", |s| s["currentIndex"] == 2)
        .await;
    assert_eq!(status(&next, 1), "error");
    assert_eq!(status(&next, 2), "ready");

    // Every Listener sees the same state, including one connecting late.
    let mut late = bt.ws().await;
    assert_eq!(late.recv().await["currentIndex"], 2);

    // The end of the Queue goes idle.
    ws.until("idle", |s| s["currentIndex"].is_null()).await;
}

#[tokio::test]
async fn invalid_links_are_rejected_without_running_anything() {
    let bt = Bt::start(2);
    let mut ws = bt.ws().await;
    ws.recv().await;
    for url in [
        "https://evil.com/watch?v=aaaaaaaaaaa",
        "http://www.youtube.com/watch?v=aaaaaaaaaaa",
        "https://www.youtube.com/watch?v=aaaa;reboot",
    ] {
        ws.add(url).await;
        let msg = ws.recv().await;
        assert_eq!(msg["type"], "error", "{url}");
        assert!(msg["message"].is_string());
    }
    ws.send(json!({"type": "nonsense"})).await;
    assert_eq!(ws.recv().await["type"], "error");
    // Give a wrongly started yt-dlp time to show up.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!bt.ytdlp_log().exists(), "yt-dlp was called");
}

#[tokio::test]
async fn controls_change_playback_for_everyone() {
    // Long Songs, so nothing ends on its own during the test.
    let bt = Bt::start(60);
    let mut ws = bt.ws().await;
    let mut friend = bt.ws().await;
    ws.recv().await;
    friend.recv().await;
    for id in ["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"] {
        ws.add(&format!("https://youtu.be/{id}")).await;
    }
    ws.until("all ready", |s| (0..3).all(|i| status(s, i) == "ready"))
        .await;
    friend
        .until("the first playing", |s| s["currentIndex"] == 0)
        .await;

    ws.send(json!({"type": "pause"})).await;
    friend.until("paused", |s| s["playing"] == false).await;
    ws.send(json!({"type": "play"})).await;
    friend.until("playing", |s| s["playing"] == true).await;

    ws.send(json!({"type": "next"})).await;
    let moved = friend.until("next", |s| s["currentIndex"] == 1).await;
    let jump_at = moved["jumpAt"].as_f64().expect("next sets jumpAt");
    let clock = moved["clock"].as_f64().unwrap();
    assert!(
        jump_at > 0.0 && jump_at <= clock,
        "jumpAt {jump_at}, clock {clock}"
    );
    ws.send(json!({"type": "next"})).await;
    friend.until("next", |s| s["currentIndex"] == 2).await;
    ws.send(json!({"type": "next"})).await;
    friend
        .until("wrap to first", |s| s["currentIndex"] == 0)
        .await;
    // Just started, so prev goes back, wrapping to the last.
    ws.send(json!({"type": "prev"})).await;
    friend
        .until("wrap to last", |s| s["currentIndex"] == 2)
        .await;
    ws.send(json!({"type": "prev"})).await;
    let back = friend.until("prev", |s| s["currentIndex"] == 1).await;
    assert_eq!(back["playing"], true);
}

#[tokio::test]
async fn the_queue_survives_restarts() {
    let mut bt = Bt::start(60);
    let mut ws = bt.ws().await;
    ws.recv().await;
    for id in ["aaaaaaaaaaa", "bbbbbbbbbbb", "unavailable", "ccccccccccc"] {
        ws.add(&format!("https://youtu.be/{id}")).await;
    }
    ws.until("all fetched", |s| status(s, 3) == "ready").await;
    ws.send(json!({"type": "next"})).await;
    let before = ws.until("next", |s| s["currentIndex"] == 1).await;
    bt.saved("the second as current", |s| {
        s["currentIndex"] == 1 && s["queue"][3]["file"].is_string()
    })
    .await;

    // All the audio is there: same Queue, same Current entry, paused.
    bt.restart(|_| ());
    let mut ws = bt.ws().await;
    let state = ws.recv().await;
    assert_eq!(state["currentIndex"], 1);
    assert_eq!(state["playing"], false);
    assert_eq!(state["jumpAt"], Value::Null);
    for i in [0, 1, 3] {
        assert_eq!(state["queue"][i], before["queue"][i]);
    }
    assert!(state["queue"][0].get("file").is_none(), "{state}");
    // The unavailable one gets another try.
    if status(&state, 2) != "error" {
        ws.until("the retry failing", |s| status(s, 2) == "error")
            .await;
    }
    let log = std::fs::read_to_string(bt.ytdlp_log()).unwrap();
    assert_eq!(log.matches("-J --no-playlist -- unavailable").count(), 2);
    ws.send(json!({"type": "play"})).await;
    ws.until("playing", |s| s["playing"] == true).await;

    // Without the first and the current one's audio: both are fetched again, and the first,
    // ready sooner, doesn't take over.
    bt.restart(|dir| {
        std::fs::remove_file(dir.join("cache/aaaaaaaaaaa.wav")).unwrap();
        std::fs::remove_file(dir.join("cache/bbbbbbbbbbb.wav")).unwrap();
    });
    let mut ws = bt.ws().await;
    let state = ws
        .until("the current fetched again", |s| {
            assert_eq!(s["currentIndex"], 1, "{s}");
            assert_eq!(s["playing"], false, "{s}");
            status(s, 1) == "ready"
        })
        .await;
    assert_eq!(status(&state, 0), "ready");
    assert_eq!(state["queue"][1]["uid"], before["queue"][1]["uid"]);
}

#[tokio::test]
async fn a_current_entry_that_cannot_be_fetched_again_is_skipped() {
    let mut bt = Bt::start(2);
    let mut ws = bt.ws().await;
    ws.recv().await;
    ws.send(json!({"type": "pause"})).await;
    for id in ["aaaaaaaaaaa", "bbbbbbbbbbb"] {
        ws.add(&format!("https://youtu.be/{id}")).await;
    }
    bt.saved("both ready", |s| {
        s["currentIndex"] == 0 && s["queue"][1]["file"].is_string()
    })
    .await;
    bt.restart(|dir| {
        // The video was removed meanwhile.
        std::fs::remove_file(dir.join("cache/aaaaaaaaaaa.wav")).unwrap();
        let path = dir.join("data/state.json");
        let json = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, json.replace("aaaaaaaaaaa", "unavailable")).unwrap();
    });
    let mut ws = bt.ws().await;
    let state = ws.until("skipped", |s| s["currentIndex"] == 1).await;
    assert_eq!(status(&state, 0), "error");
    assert_eq!(state["playing"], false);
    // It really plays: the 2 s Song ends and the Radio goes idle.
    ws.send(json!({"type": "play"})).await;
    ws.until("idle", |s| s["currentIndex"].is_null()).await;
}

#[tokio::test]
async fn a_current_entry_fetched_again_plays() {
    let mut bt = Bt::start(2);
    let mut ws = bt.ws().await;
    ws.recv().await;
    ws.send(json!({"type": "pause"})).await;
    ws.add("https://youtu.be/aaaaaaaaaaa").await;
    bt.saved("it ready", |s| s["queue"][0]["file"].is_string())
        .await;
    bt.restart(|dir| std::fs::remove_file(dir.join("cache/aaaaaaaaaaa.wav")).unwrap());
    let mut ws = bt.ws().await;
    ws.until("fetched again", |s| status(s, 0) == "ready").await;
    ws.send(json!({"type": "play"})).await;
    ws.until("its end", |s| s["currentIndex"].is_null()).await;
}

#[tokio::test]
async fn progress_is_shared_on_state_changes() {
    let bt = Bt::start(60);
    let mut ws = bt.ws().await;
    assert_eq!(ws.recv().await["positionSec"], 0.0);

    for id in ["aaaaaaaaaaa", "bbbbbbbbbbb"] {
        ws.add(&format!("https://youtu.be/{id}")).await;
    }
    let playing = ws
        .until("both entries ready", |s| {
            s["currentIndex"] == 0 && status(s, 1) == "ready"
        })
        .await;
    let at = playing["at"].as_f64().unwrap();
    assert!(at <= playing["clock"].as_f64().unwrap());
    assert!(
        tokio::time::timeout(Duration::from_millis(1100), ws.recv())
            .await
            .is_err(),
        "progress should advance without WebSocket messages"
    );

    let mut late = bt.ws().await;
    let snapshot = late.recv().await;
    assert_eq!(snapshot["at"], playing["at"]);
    assert_eq!(snapshot["positionSec"], playing["positionSec"]);
    assert!(snapshot["clock"].as_f64().unwrap() > at);

    ws.send(json!({"type": "pause"})).await;
    let paused = late.until("paused", |s| s["playing"] == false).await;
    assert!(paused["positionSec"].as_f64().unwrap() >= 0.5);

    ws.send(json!({"type": "play"})).await;
    let resumed = late.until("resumed", |s| s["playing"] == true).await;
    assert!(resumed["positionSec"].as_f64().unwrap() >= paused["positionSec"].as_f64().unwrap());

    ws.send(json!({"type": "pause"})).await;
    late.until("paused again", |s| s["playing"] == false).await;
    ws.send(json!({"type": "next"})).await;
    let next = late.until("next song", |s| s["currentIndex"] == 1).await;
    assert_eq!(next["positionSec"], 0.0);
    assert_eq!(next["playing"], false);
}

#[tokio::test]
async fn seeking_while_paused_moves_playback_for_every_listener() {
    let bt = Bt::start(12);
    let mut ws = bt.ws().await;
    let mut friend = bt.ws().await;
    ws.recv().await;
    friend.recv().await;
    ws.send(json!({"type": "pause"})).await;
    for id in ["aaaaaaaaaaa", "bbbbbbbbbbb"] {
        ws.add(&format!("https://youtu.be/{id}")).await;
    }
    friend
        .until("both songs ready", |s| {
            s["currentIndex"] == 0 && status(s, 1) == "ready" && s["playing"] == false
        })
        .await;

    ws.send(json!({"type": "seek", "positionSec": 11.5})).await;
    for listener in [&mut ws, &mut friend] {
        let sought = listener
            .until("seek to 11.5 seconds", |s| s["positionSec"] == 11.5)
            .await;
        assert_eq!(sought["playing"], false);
        assert_eq!(sought["currentIndex"], 0);
        assert!(sought["jumpAt"].as_f64().is_some());
    }

    friend.send(json!({"type": "play"})).await;
    // Resuming from 11.5 s leaves only half a second of the first song.
    tokio::time::timeout(
        Duration::from_secs(4),
        ws.until("the next song", |s| s["currentIndex"] == 1),
    )
    .await
    .expect("first song should finish shortly after resuming");
}

/// An Access link token for the auth tests.
const TOKEN: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";

/// The public URL the auth tests configure.
const PUBLIC_URL: &str = "https://bt.example.com";

/// Starts `bt` with one Access link, `lorena`'s [`TOKEN`], for [`PUBLIC_URL`].
fn start_private() -> Bt {
    Bt::start_with(
        2,
        vec![
            ("BT_ACCESS_TOKENS", format!("lorena:{TOKEN}")),
            ("BT_PUBLIC_URL", PUBLIC_URL.into()),
        ],
    )
}

/// Opens `/ws` on `bt` with the extra headers `headers` (name and value).
///
/// Returns the HTTP status of the handshake: 101 if it was accepted.
///
/// # Panics
///
/// Panics if a header is invalid or the connection fails for any reason but an HTTP error.
async fn ws_status(bt: &Bt, headers: &[(&'static str, &str)]) -> u16 {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{}/ws", bt.addr)
        .into_client_request()
        .unwrap();
    for (k, v) in headers {
        req.headers_mut().insert(*k, v.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((_, res)) => res.status().as_u16(),
        Err(tokio_tungstenite::tungstenite::Error::Http(res)) => res.status().as_u16(),
        Err(e) => panic!("connect /ws: {e}"),
    }
}

#[test]
fn opening_an_access_link_sets_the_cookie() {
    let bt = start_private();
    let (head, _) = get(&bt.addr, &format!("/?token={TOKEN}"), 0);
    assert!(head.starts_with("http/1.1 303"), "{head}");
    assert!(head.contains("location: /\r\n"), "{head}");
    assert!(
        head.contains(&format!(
            "set-cookie: bt_token={TOKEN}; path=/; httponly; samesite=lax; max-age=31536000; secure"
        )),
        "{head}"
    );
    let cookie = format!("Cookie: bt_token={TOKEN}\r\n");
    let (head, body) = get_with(&bt.addr, "/", &cookie, 1);
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(String::from_utf8_lossy(&body).contains("<!doctype html>"));
    let (head, _) = get_with(&bt.addr, "/stream", &cookie, 1);
    assert!(head.starts_with("http/1.1 200"), "{head}");
}

#[test]
fn everything_needs_a_valid_access_link() {
    let bt = start_private();
    let wrong = "00000000-0000-0000-0000-000000000000";
    for (path, headers) in [
        ("/".to_string(), String::new()),
        ("/app.js".into(), String::new()),
        ("/stream".into(), String::new()),
        ("/ws".into(), String::new()),
        (format!("/?token={wrong}"), String::new()),
        ("/".into(), format!("Cookie: bt_token={wrong}\r\n")),
        (format!("/stream?token={TOKEN}"), String::new()),
    ] {
        let (head, body) = get_with(&bt.addr, &path, &headers, 1);
        assert!(head.starts_with("http/1.1 401"), "{path} {headers}: {head}");
        assert!(String::from_utf8_lossy(&body).contains("access link"));
    }
}

#[tokio::test]
async fn the_websocket_needs_the_cookie_and_the_public_origin() {
    let bt = start_private();
    let cookie = format!("bt_token={TOKEN}");
    assert_eq!(ws_status(&bt, &[("origin", PUBLIC_URL)]).await, 401);
    assert_eq!(ws_status(&bt, &[("cookie", &cookie)]).await, 403);
    let sibling = [
        ("cookie", cookie.as_str()),
        ("origin", "https://evil.example.com"),
    ];
    assert_eq!(ws_status(&bt, &sibling).await, 403);
    let ok = [("cookie", cookie.as_str()), ("origin", PUBLIC_URL)];
    assert_eq!(ws_status(&bt, &ok).await, 101);
}
