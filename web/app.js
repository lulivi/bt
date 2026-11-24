"use strict";

const audio = document.getElementById("radio");
const connectBtn = document.getElementById("connect");
const audioStatus = document.getElementById("audio-status");

// --- Audio: one endless stream; reconnect it when it breaks or stalls. ---
let wantAudio = false;
let backoff = 1000;
let retryTimer = null;
let lastTime = -1;
let lastProgress = Date.now();

function startStream(fresh) {
  audio.src = fresh ? "/stream?t=" + Date.now() : "/stream";
  lastProgress = Date.now();
  audio.play().catch(() => {});
}

function scheduleReconnect() {
  if (!wantAudio || retryTimer) return;
  audioStatus.textContent = "Reconnecting audio…";
  retryTimer = setTimeout(() => {
    retryTimer = null;
    startStream(true);
  }, backoff);
  backoff = Math.min(backoff * 2, 30000);
}

connectBtn.addEventListener("click", () => {
  wantAudio = true;
  connectBtn.hidden = true;
  startStream(false);
});

audio.addEventListener("error", scheduleReconnect);
audio.addEventListener("playing", () => {
  audioStatus.textContent = "Connected";
});

// `stalled` fires spuriously in some browsers, so watch currentTime instead.
setInterval(() => {
  if (!wantAudio) return;
  if (audio.currentTime !== lastTime) {
    lastTime = audio.currentTime;
    lastProgress = Date.now();
    backoff = 1000;
  } else if (Date.now() - lastProgress > 10000) {
    scheduleReconnect();
  }
}, 1000);

// Stay near the live edge. The browser plays from its own buffer, so what is buffered ahead
// is how late this Listener hears the Radio (and its controls). Network hiccups grow that
// buffer for good unless we catch up: gently by playing faster, or by jumping ahead.
// Browsers append to `buffered` in ~1 s steps, so the real delay is the lowest reading
// over a few seconds, not the latest one.
// Tune these if phones stutter (raise LAG_FLOOR) or controls still feel slow (lower them).
const LAG_FLOOR = 0.3; // seconds: never play faster with less than this buffered
const LAG_CATCH_UP = 0.8; // seconds of delay above which we play faster
const LAG_JUMP = 4; // seconds of delay above which we jump to the live edge
const CATCH_UP_RATE = 1.1;
const LAG_WINDOW = 4000; // ms of readings the delay is taken over
const lagEl = document.getElementById("lag");
let lagReadings = [];

setInterval(() => {
  if (!wantAudio || audio.paused || audio.buffered.length === 0) return;
  const edge = audio.buffered.end(audio.buffered.length - 1);
  const ahead = edge - audio.currentTime;
  const now = Date.now();
  lagReadings = lagReadings.filter((r) => now - r.at < LAG_WINDOW);
  lagReadings.push({ at: now, ahead });
  const delay = Math.min(...lagReadings.map((r) => r.ahead));

  if (delay > LAG_JUMP) {
    try {
      audio.currentTime = edge - 1;
      lagReadings = [];
    } catch {
      // Not seekable in this browser: playing faster still gets there.
    }
  }
  if (ahead < LAG_FLOOR) audio.playbackRate = 1;
  else if (delay > LAG_CATCH_UP) audio.playbackRate = CATCH_UP_RATE;
  else audio.playbackRate = 1;
  lagEl.textContent = "Delay: " + delay.toFixed(1) + " s";
}, 250);

// --- Shared state over a WebSocket; the server sends the full state on every change. ---
const nowEl = document.getElementById("now");
const queueEl = document.getElementById("queue");
const errorEl = document.getElementById("error");
const wsStatus = document.getElementById("ws-status");
const urlInput = document.getElementById("url");
const toggleBtn = document.getElementById("toggle");
let playing = true;

let ws = null;
let wsBackoff = 1000;
let wsTimer = null;

function connectWs() {
  clearTimeout(wsTimer);
  wsTimer = null;
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  ws = new WebSocket(proto + "//" + location.host + "/ws");
  ws.onopen = () => {
    wsBackoff = 1000;
    wsStatus.textContent = "";
  };
  ws.onmessage = (event) => {
    const msg = JSON.parse(event.data);
    if (msg.type === "state") render(msg);
    else if (msg.type === "error") errorEl.textContent = msg.message;
  };
  ws.onclose = () => {
    wsStatus.textContent = "Reconnecting…";
    wsTimer = setTimeout(connectWs, wsBackoff);
    wsBackoff = Math.min(wsBackoff * 2, 30000);
  };
}

function send(msg) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(msg));
}

function formatDuration(seconds) {
  if (seconds == null) return "";
  const s = Math.round(seconds);
  return Math.floor(s / 60) + ":" + String(s % 60).padStart(2, "0");
}

function render(state) {
  const current = state.currentIndex == null ? null : state.queue[state.currentIndex];
  nowEl.textContent = current ? current.title : "Nothing";
  playing = state.playing;
  toggleBtn.textContent = playing ? "⏸" : "▶";
  toggleBtn.setAttribute("aria-label", playing ? "Pause" : "Play");
  queueEl.replaceChildren(
    ...state.queue.map((entry, i) => {
      const li = document.createElement("li");
      li.className = "status-" + entry.status;
      if (i === state.currentIndex) {
        li.classList.add("current");
        li.setAttribute("aria-current", "true");
      }
      const title = document.createElement("span");
      title.className = "title";
      title.textContent = entry.title;
      const meta = document.createElement("span");
      meta.className = "meta";
      meta.textContent = [formatDuration(entry.duration), entry.status].filter(Boolean).join(" · ");
      li.append(title, meta);
      return li;
    }),
  );
}

document.getElementById("add").addEventListener("submit", (event) => {
  event.preventDefault();
  errorEl.textContent = "";
  send({ type: "add", url: urlInput.value });
  urlInput.value = "";
});

// Controls change the shared Radio for everyone. They never touch the local <audio>:
// pausing it would desync this Listener and let a locked phone drop the audio session.
toggleBtn.addEventListener("click", () => send({ type: playing ? "pause" : "play" }));
document.getElementById("prev").addEventListener("click", () => send({ type: "prev" }));
document.getElementById("next").addEventListener("click", () => send({ type: "next" }));

// A phone coming back from a locked screen: reconnect now instead of waiting for backoff.
document.addEventListener("visibilitychange", () => {
  // The old socket may report OPEN while actually dead, so always replace it.
  if (document.visibilityState !== "visible") return;
  if (ws) {
    ws.onclose = null;
    ws.close();
  }
  connectWs();
});

connectWs();
