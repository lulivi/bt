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
