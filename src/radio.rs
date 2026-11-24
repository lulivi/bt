//! The Radio: a mixer thread feeding PCM in real time to one long-lived ffmpeg encoder,
//! whose MP3 output is broadcast to every listener.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::body::Bytes;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

pub type Chunk = Bytes;

const TICK: Duration = Duration::from_millis(20);
/// 20 ms of 44.1 kHz stereo s16le: 882 frames × 2 channels × 2 bytes.
const TICK_BYTES: usize = 3528;
/// Decoded PCM buffered ahead of the mixer, in ticks (5 s).
const DECODER_BUFFER: usize = 250;

/// What the mixer plays. Owned by the state actor, read by the mixer every tick.
#[derive(Default)]
pub struct Deck {
    pub decoder: Option<Decoder>,
    pub playing: bool,
}

/// One ffmpeg decoding one Entry's file to PCM, read ahead into a bounded buffer.
pub struct Decoder {
    pub uid: Uuid,
    child: Child,
    pcm: Receiver<Vec<u8>>,
}

impl Decoder {
    pub fn spawn(ffmpeg: &str, file: &Path, uid: Uuid) -> anyhow::Result<Self> {
        let mut child = Command::new(ffmpeg)
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
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
        Ok(Self { uid, child, pcm })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        _ = self.child.kill();
        _ = self.child.wait();
    }
}

/// Starts the encoder and the mixer. Returns the MP3 broadcast and the deck the mixer plays;
/// when the current decoder runs out, the mixer sends its Entry's uid on `ended`.
pub fn start(
    ffmpeg: &str,
    ended: mpsc::UnboundedSender<Uuid>,
) -> anyhow::Result<(broadcast::Sender<Chunk>, Arc<Mutex<Deck>>)> {
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

    // ~256 chunks of ≤4 KiB ≈ 60 s of 128 kbit/s audio of slack before a listener lags.
    let (tx, _) = broadcast::channel(256);

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
            // Aim at absolute instants so scheduling jitter never accumulates.
            let target = start + TICK * n;
            if let Some(wait) = target.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
        }
    })?;

    Ok((tx, deck))
}

/// The PCM for this tick, or `None` for silence (paused, idle, or the decoder lagging).
fn next_tick(deck: &mut Deck, ended: &mpsc::UnboundedSender<Uuid>) -> Option<Vec<u8>> {
    if !deck.playing {
        return None;
    }
    let decoder = deck.decoder.as_mut()?;
    match decoder.pcm.try_recv() {
        Ok(pcm) => Some(pcm),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => {
            let done = deck.decoder.take().expect("checked above");
            _ = ended.send(done.uid);
            None
        }
    }
}
