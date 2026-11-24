//! The Radio: a mixer thread feeding PCM in real time to one long-lived ffmpeg encoder,
//! whose MP3 output is broadcast to every listener.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::body::Bytes;
use tokio::sync::broadcast;

pub type Chunk = Bytes;

const TICK: Duration = Duration::from_millis(20);
/// 20 ms of 44.1 kHz stereo s16le: 882 frames × 2 channels × 2 bytes.
const TICK_BYTES: usize = 3528;

/// Starts the encoder and the mixer; returns the MP3 broadcast.
pub fn start(ffmpeg: &str) -> anyhow::Result<broadcast::Sender<Chunk>> {
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

    thread::Builder::new().name("mixer".into()).spawn(move || {
        let silence = [0u8; TICK_BYTES];
        let start = Instant::now();
        for n in 1u32.. {
            if pcm_in.write_all(&silence).is_err() {
                break; // the encoder thread reports and exits
            }
            // Aim at absolute instants so scheduling jitter never accumulates.
            let target = start + TICK * n;
            if let Some(wait) = target.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
        }
    })?;

    Ok(tx)
}
