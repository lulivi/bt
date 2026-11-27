// SPDX-FileCopyrightText: 2026 Lorena Jiménez, Luis Liñán
// SPDX-License-Identifier: MIT

//! Browser tests: the real page in headless Chrome, driven over the DevTools protocol.
//!
//! Chrome is `google-chrome`, or the binary in `CHROME`.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{Bt, status};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

/// A headless Chrome with one page open, killed on drop.
struct Chrome {
    /// The Chrome process.
    child: Child,
    /// Its throwaway profile.
    _profile: TempDir,
    /// The DevTools connection to the browser.
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    /// The DevTools session of the page.
    session: String,
    /// The id of the last command sent.
    id: u64,
}

/// Kills Chrome.
impl Drop for Chrome {
    fn drop(&mut self) {
        _ = self.child.kill();
        _ = self.child.wait();
    }
}

/// Starts Chrome and drives its page.
impl Chrome {
    /// Starts Chrome and opens a blank page that runs `init` before every page's own scripts.
    ///
    /// # Panics
    ///
    /// Panics if Chrome can't be started.
    async fn start(init: &str) -> Self {
        let profile = tempfile::tempdir().unwrap();
        let bin = std::env::var("CHROME").unwrap_or_else(|_| "google-chrome".into());
        let mut child = Command::new(&bin)
            .args([
                "--headless=new",
                "--no-sandbox",
                "--no-first-run",
                "--mute-audio",
                "--autoplay-policy=no-user-gesture-required",
                "--remote-debugging-port=0",
            ])
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("starting {bin} ({e}); set CHROME to a Chrome binary"));
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let url = loop {
            let line = lines.next().expect("Chrome exited early").unwrap();
            if let Some((_, url)) = line.split_once("DevTools listening on ") {
                break url.trim().to_string();
            }
        };
        std::thread::spawn(move || lines.for_each(drop));
        let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let mut chrome = Chrome {
            child,
            _profile: profile,
            ws,
            session: String::new(),
            id: 0,
        };
        let target = chrome
            .call(false, "Target.createTarget", json!({"url": "about:blank"}))
            .await;
        let attached = chrome
            .call(
                false,
                "Target.attachToTarget",
                json!({"targetId": target["targetId"], "flatten": true}),
            )
            .await;
        chrome.session = attached["sessionId"].as_str().unwrap().to_string();
        chrome.call(true, "Page.enable", json!({})).await;
        chrome
            .call(
                true,
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": init}),
            )
            .await;
        chrome
    }

    /// Sends a DevTools command, to the page if `page`, else to the browser; returns its result.
    ///
    /// # Panics
    ///
    /// Panics if the command fails.
    async fn call(&mut self, page: bool, method: &str, params: Value) -> Value {
        self.id += 1;
        let mut msg = json!({"id": self.id, "method": method, "params": params});
        if page {
            msg["sessionId"] = json!(self.session);
        }
        self.ws.send(Message::text(msg.to_string())).await.unwrap();
        loop {
            let Message::Text(text) = self.ws.next().await.unwrap().unwrap() else {
                continue;
            };
            let reply: Value = serde_json::from_str(&text).unwrap();
            if reply["id"] == self.id {
                assert!(reply["error"].is_null(), "{method}: {reply}");
                return reply["result"].clone();
            }
        }
    }

    /// Evaluates `js` in the page and returns its value.
    async fn eval(&mut self, js: &str) -> Value {
        let result = self
            .call(
                true,
                "Runtime.evaluate",
                json!({"expression": js, "returnByValue": true}),
            )
            .await;
        result["result"]["value"].clone()
    }

    /// Waits up to `timeout` for `js` to be true in the page.
    ///
    /// # Panics
    ///
    /// Panics with `what` if it isn't in time.
    async fn until(&mut self, what: &str, timeout: Duration, js: &str) {
        let start = Instant::now();
        while self.eval(js).await != json!(true) {
            assert!(start.elapsed() < timeout, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Starts `bt` playing the first of two long Songs, and a Listener in Chrome hearing it.
///
/// Chrome runs `init` before the page's scripts.
async fn listening(init: &str) -> (Bt, common::Ws, Chrome) {
    let bt = Bt::start(60);
    let mut ws = bt.ws().await;
    ws.add("https://youtu.be/aaaaaaaaaaa").await;
    ws.add("https://youtu.be/bbbbbbbbbbb").await;
    ws.until("both ready", |s| (0..2).all(|i| status(s, i) == "ready"))
        .await;
    let mut chrome = Chrome::start(init).await;
    chrome
        .call(
            true,
            "Page.navigate",
            json!({"url": format!("http://{}/", bt.addr)}),
        )
        .await;
    let wait = Duration::from_secs(15);
    chrome
        .until("the page", wait, "typeof connectBtn === 'object'")
        .await;
    chrome.eval("connectBtn.click()").await;
    chrome
        .until(
            "playing",
            wait,
            "audio.currentTime > 1 && !audio.muted && hearing() !== null",
        )
        .await;
    (bt, ws, chrome)
}

#[tokio::test]
async fn a_jump_is_heard_at_once_through_mse() {
    let (_bt, mut ws, mut chrome) = listening("").await;
    assert_eq!(
        chrome.eval("audio.src.startsWith('blob:')").await,
        json!(true)
    );
    ws.send(json!({"type": "next"})).await;
    let state = ws.until("the jump", |s| s["currentIndex"] == 1).await;
    let jump_at = state["jumpAt"].as_f64().unwrap();
    chrome
        .until(
            "the new Song to be heard",
            Duration::from_secs(1),
            &format!("!audio.muted && hearing() >= {jump_at}"),
        )
        .await;
}

#[tokio::test]
async fn without_mse_a_jump_reconnects_the_stream() {
    let (_bt, mut ws, mut chrome) = listening("delete window.MediaSource;").await;
    assert_eq!(
        chrome.eval("audio.src.endsWith('/stream')").await,
        json!(true)
    );
    ws.send(json!({"type": "next"})).await;
    ws.until("the jump", |s| s["currentIndex"] == 1).await;
    chrome
        .until(
            "the reconnected stream to play",
            Duration::from_secs(15),
            "audio.src.includes('/stream?t=') && audio.currentTime > 0 && !audio.muted",
        )
        .await;
}

#[tokio::test]
async fn pause_mutes_at_once_and_resume_unmutes() {
    let (_bt, mut ws, mut chrome) = listening("").await;
    // Muted before the server has even heard of it.
    assert_eq!(
        chrome.eval("toggleBtn.click(); audio.muted").await,
        json!(true)
    );
    ws.until("paused", |s| s["playing"] == false).await;
    let wait = Duration::from_secs(5);
    chrome.until("the page to see it", wait, "!playing").await;
    // The stream goes on (silent), so a locked phone keeps its audio session.
    let t = chrome.eval("audio.currentTime").await.as_f64().unwrap();
    chrome
        .until(
            "the stream to go on",
            wait,
            &format!("!audio.paused && audio.currentTime > {t} + 0.5 && audio.muted"),
        )
        .await;
    chrome.eval("toggleBtn.click()").await;
    chrome
        .until("unmuted", wait, "playing && !audio.muted")
        .await;
}

#[tokio::test]
async fn the_page_reconnects_when_bt_restarts() {
    let (mut bt, _ws, mut chrome) = listening("").await;
    chrome
        .eval("window.oldWs = ws; window.oldFeed = feed")
        .await;
    bt.restart(|_| {});
    chrome
        .until(
            "the WebSocket and the audio to come back",
            Duration::from_secs(20),
            "ws !== oldWs && ws.readyState === WebSocket.OPEN && feed !== oldFeed \
             && audio.currentTime > 0.5 && audio.muted === !playing",
        )
        .await;
}

#[tokio::test]
async fn seeking_with_the_progress_bar_moves_playback() {
    let (_bt, mut ws, mut chrome) = listening("").await;
    let wait = Duration::from_secs(5);
    chrome
        .until(
            "the bar",
            wait,
            "!trackProgress.disabled && trackProgress.max == 60",
        )
        .await;
    chrome
        .eval("trackProgress.value = 30; trackProgress.dispatchEvent(new Event('change'))")
        .await;
    ws.until("the seek", |s| {
        s["positionSec"]
            .as_f64()
            .is_some_and(|p| (30.0..31.0).contains(&p))
    })
    .await;
    chrome
        .until(
            "the time to follow",
            wait,
            "playbackTime.textContent.startsWith('0:3') && playbackTime.textContent.endsWith('/ 1:00')",
        )
        .await;
}

#[tokio::test]
async fn pasted_links_show_in_the_queue() {
    let (_bt, _ws, mut chrome) = listening("").await;
    let wait = Duration::from_secs(15);
    // One upcoming Entry: nothing to shuffle.
    assert_eq!(chrome.eval("shuffleBtn.disabled").await, json!(true));
    chrome
        .eval("urlInput.value = 'https://youtu.be/ccccccccccc'; document.getElementById('add').requestSubmit()")
        .await;
    chrome
        .until(
            "the new Entry",
            wait,
            "queueEl.children.length === 3 && queueEl.lastChild.classList.contains('status-ready') \
             && queueEl.lastChild.textContent.includes('Song ccccccccccc')",
        )
        .await;
    assert_eq!(chrome.eval("shuffleBtn.disabled").await, json!(false));
    chrome
        .eval("urlInput.value = 'https://example.com/x'; document.getElementById('add').requestSubmit()")
        .await;
    chrome
        .until("the error", wait, "errorEl.textContent !== ''")
        .await;
    assert_eq!(chrome.eval("queueEl.children.length").await, json!(3));
}
