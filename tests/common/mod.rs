// SPDX-FileCopyrightText: 2026 Lorena Jiménez, Luis Liñán
// SPDX-License-Identifier: MIT

//! Helpers shared by the test binaries: run the real `bt` with real ffmpeg and
//! `tests/fake-yt-dlp.sh`, and talk to it.

// Each test binary uses only some of these.
#![allow(dead_code)]

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
pub struct Bt {
    /// The `bt` process.
    child: Child,
    /// The address it listens on.
    pub addr: String,
    /// Its cache, state file and logs.
    dir: TempDir,
    /// How long each fake Song lasts.
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
    pub fn start(seconds: u32) -> Self {
        Self::start_with(seconds, Vec::new())
    }

    /// Starts `bt` like [`Bt::start`], with the extra environment variables `env`.
    pub fn start_with(seconds: u32, env: Vec<(&'static str, String)>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (child, addr) = Self::spawn(&dir, seconds, "127.0.0.1:0", &env);
        Bt {
            child,
            addr,
            dir,
            seconds,
            env,
        }
    }

    /// Kills `bt` and starts it again with the same files, on the same address.
    ///
    /// `between` gets their directory and runs while `bt` is down.
    pub fn restart(&mut self, between: impl FnOnce(&Path)) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        between(self.dir.path());
        (self.child, self.addr) = Self::spawn(&self.dir, self.seconds, &self.addr, &self.env);
    }

    /// Spawns `bt` with its files in `dir` and the extra variables `env`, and waits until it
    /// listens on `addr`.
    ///
    /// Its stderr (and its ffmpegs') goes to a file, as ffmpeg always complains when `bt` is
    /// killed. Returns the process and the address it listens on.
    ///
    /// # Panics
    ///
    /// Panics with that stderr if `bt` exits before listening.
    fn spawn(dir: &TempDir, seconds: u32, addr: &str, env: &[(&str, String)]) -> (Child, String) {
        let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-yt-dlp.sh");
        let log = File::options()
            .create(true)
            .append(true)
            .open(dir.path().join("stderr.log"))
            .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_bt"))
            .env("BT_ADDR", addr)
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
    pub fn ytdlp_log(&self) -> PathBuf {
        self.dir.path().join("yt-dlp.log")
    }

    /// Waits (up to 15 s) for the state file to match `want`.
    ///
    /// The file is written in the background, so it can lag behind the published State.
    ///
    /// # Panics
    ///
    /// Panics with `what` if it doesn't match in time.
    pub async fn saved(&self, what: &str, want: impl Fn(&Value) -> bool) {
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
    pub async fn ws(&self) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", self.addr))
            .await
            .expect("connect /ws");
        Ws(ws)
    }
}

/// A Listener's WebSocket.
pub struct Ws(
    /// The connection.
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
);

/// Sends commands and waits for messages.
impl Ws {
    /// Sends `msg` as JSON.
    pub async fn send(&mut self, msg: Value) {
        self.0.send(Message::text(msg.to_string())).await.unwrap();
    }

    /// Waits for the next message.
    pub async fn recv(&mut self) -> Value {
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
    pub async fn until(&mut self, what: &str, want: impl Fn(&Value) -> bool) -> Value {
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
    pub async fn add(&mut self, url: &str) {
        self.send(json!({"type": "add", "url": url})).await;
    }
}

/// Returns what the `bt` with its files in `dir` has written to stderr, across restarts.
pub fn stderr(dir: &TempDir) -> String {
    let log = std::fs::read(dir.path().join("stderr.log")).unwrap_or_default();
    String::from_utf8_lossy(&log).into_owned()
}

/// Returns the status of the Entry `i`, or an empty string if there is none.
pub fn status(state: &Value, i: usize) -> &str {
    state["queue"][i]["status"].as_str().unwrap_or_default()
}

/// Requests `path` and reads at least `body_bytes` of the response's body.
///
/// Returns the lower-cased header block and the body bytes read so far.
pub fn get(addr: &str, path: &str, body_bytes: usize) -> (String, Vec<u8>) {
    get_with(addr, path, "", body_bytes)
}

/// Requests `path` like [`get`], with extra headers.
///
/// `headers` are raw header lines, each ending in `\r\n`.
pub fn get_with(addr: &str, path: &str, headers: &str, body_bytes: usize) -> (String, Vec<u8>) {
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
