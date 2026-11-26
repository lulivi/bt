// SPDX-FileCopyrightText: 2026 Lorena Jiménez, Luis Liñán
// SPDX-License-Identifier: MIT

//! The Radio.
//!
//! A mixer thread feeds PCM in real time to one long-lived ffmpeg encoder, whose MP3 output is
//! broadcast to every Listener.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::body::Bytes;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

/// A piece of the MP3 broadcast.
pub type Chunk = Bytes;

/// How much audio the mixer sends at a time.
const TICK: Duration = Duration::from_millis(20);
/// The PCM in a [`TICK`]: 20 ms of 44.1 kHz stereo s16le: 882 frames × 2 channels × 2 bytes.
const TICK_BYTES: usize = 3528;
/// Decoded PCM buffered ahead of the mixer, in ticks (5 s).
const DECODER_BUFFER: usize = 250;

/// Stream time: how much audio the Radio has broadcast since it started.
///
/// Counted by the mixer. Listeners use it to find a moment (like a jump) in their own copy of the stream.
#[derive(Clone, Default)]
pub struct Clock(Arc<AtomicU64>);

/// Reads the stream time.
impl Clock {
    /// Returns the stream time in seconds.
    pub fn now(&self) -> f64 {
        (self.0.load(Ordering::Relaxed) * TICK.as_millis() as u64) as f64 / 1000.0
    }
}

/// What the mixer plays.
///
/// Set by the state actor and read by the mixer every tick.
#[derive(Default)]
pub struct Deck {
    /// Decodes the Current entry; `None` when idle or it has no audio.
    pub decoder: Option<Decoder>,
    /// Whether the mixer plays the decoder; otherwise it sends silence.
    pub playing: bool,
}

/// One ffmpeg decoding one Entry's file to PCM.
///
/// The PCM is read ahead into a bounded buffer.
pub struct Decoder {
    /// The Entry being decoded.
    pub uid: Uuid,
    /// The ffmpeg process.
    child: Child,
    /// The decoded audio, one [`TICK`] per item.
    pcm: Receiver<Vec<u8>>,
    /// Where in the Entry decoding started.
    start_at: Duration,
    /// How many ticks have been broadcast.
    played_ticks: u32,
}

/// Starts decoding.
impl Decoder {
    /// Starts `ffmpeg` decoding `file` from `start_at`, for the Entry `uid`.
    ///
    /// # Errors
    ///
    /// Fails if ffmpeg or the thread that reads from it can't be started.
    pub fn spawn(ffmpeg: &str, file: &Path, uid: Uuid, start_at: Duration) -> anyhow::Result<Self> {
        let mut command = Command::new(ffmpeg);
        command.args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
        if !start_at.is_zero() {
            command.args(["-ss", &format!("{:.9}", start_at.as_secs_f64())]);
        }
        let mut child = command
            .arg("-i")
            .arg(file)
            .args(["-f", "s16le", "-ar", "44100", "-ac", "2", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("starting decoder {ffmpeg}"))?;
        let mut out = child.stdout.take().expect("piped stdout");
        let (tx, pcm) = sync_channel(DECODER_BUFFER);
        thread::Builder::new()
            .name("decoder".into())
            .spawn(move || {
                loop {
                    let mut tick = vec![0u8; TICK_BYTES];
                    let mut filled = 0;
                    while filled < TICK_BYTES {
                        match out.read(&mut tick[filled..]) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => filled += n,
                        }
                    }
                    // A short last tick is padded with silence; an empty one ends the track.
                    // A send error means the Decoder was dropped.
                    if filled == 0 || tx.send(tick).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            uid,
            child,
            pcm,
            start_at,
            played_ticks: 0,
        })
    }
}

/// Reports progress.
impl Decoder {
    /// Returns the position in the Entry, including any seek before the decoder started.
    pub fn played(&self) -> Duration {
        self.start_at.saturating_add(TICK * self.played_ticks)
    }
}

/// Stops ffmpeg with its Decoder.
impl Drop for Decoder {
    fn drop(&mut self) {
        _ = self.child.kill();
        _ = self.child.wait();
    }
}

/// Starts the encoder and the mixer.
///
/// When a decoder runs out, the mixer sends its Entry's uid on `ended`. If the encoder exits, so
/// does bt. Returns the MP3 broadcast, the deck the mixer plays and the stream clock.
///
/// # Errors
///
/// Fails if `ffmpeg` or the threads can't be started.
pub fn start(
    ffmpeg: &str,
    ended: mpsc::UnboundedSender<Uuid>,
) -> anyhow::Result<(broadcast::Sender<Chunk>, Arc<Mutex<Deck>>, Clock)> {
    let mut encoder = Command::new(ffmpeg)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
        .args(["-f", "s16le", "-ar", "44100", "-ac", "2", "-i", "pipe:0"])
        .args(["-c:a", "libmp3lame", "-b:a", "128k", "-f", "mp3", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("starting encoder {ffmpeg}"))?;
    let mut pcm_in = encoder.stdin.take().expect("piped stdin");
    let mut mp3_out = encoder.stdout.take().expect("piped stdout");

    // ffmpeg flushes about one MP3 frame (~420 B) per read, so 2048 chunks ≈ 55 s of
    // 128 kbit/s audio of slack before a listener counts as lagging and is dropped.
    let (tx, _) = broadcast::channel(2048);

    let sender = tx.clone();
    thread::Builder::new()
        .name("encoder-out".into())
        .spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match mp3_out.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    // No receivers is fine: nobody is listening right now.
                    Ok(n) => _ = sender.send(Bytes::copy_from_slice(&buf[..n])),
                }
            }
            let status = encoder.wait();
            tracing::error!("encoder exited ({status:?}); stopping");
            std::process::exit(1);
        })?;

    let deck = Arc::new(Mutex::new(Deck::default()));
    let mixer_deck = deck.clone();
    let clock = Clock::default();
    let ticks = clock.0.clone();
    thread::Builder::new().name("mixer".into()).spawn(move || {
        let silence = vec![0u8; TICK_BYTES];
        let start = Instant::now();
        for n in 1u32.. {
            let pcm = next_tick(&mut mixer_deck.lock().unwrap(), &ended);
            if pcm_in
                .write_all(pcm.as_deref().unwrap_or(&silence))
                .is_err()
            {
                break; // the encoder thread reports and exits
            }
            ticks.store(n.into(), Ordering::Relaxed);
            // Aim at absolute instants so scheduling jitter never accumulates.
            let target = start + TICK * n;
            if let Some(wait) = target.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
        }
    })?;

    Ok((tx, deck, clock))
}

/// Takes the PCM for this tick from the deck.
///
/// Returns `None` for silence: paused, idle or the decoder lagging. When the decoder has run
/// out, it is removed and its Entry's uid sent on `ended`.
fn next_tick(deck: &mut Deck, ended: &mpsc::UnboundedSender<Uuid>) -> Option<Vec<u8>> {
    if !deck.playing {
        return None;
    }
    let decoder = deck.decoder.as_mut()?;
    match decoder.pcm.try_recv() {
        Ok(pcm) => {
            decoder.played_ticks += 1;
            Some(pcm)
        }
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => {
            let done = deck.decoder.take().expect("checked above");
            _ = ended.send(done.uid);
            None
        }
    }
}
