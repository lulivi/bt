//! End-to-end tests: run the real `bt` binary with real ffmpeg.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// A running `bt`, killed on drop.
struct Bt {
    child: Child,
    addr: String,
}

impl Drop for Bt {
    fn drop(&mut self) {
        _ = self.child.kill();
        _ = self.child.wait();
    }
}

fn start_bt() -> Bt {
    let mut child = Command::new(env!("CARGO_BIN_EXE_bt"))
        .env("BT_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn bt");
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let addr = loop {
        let line = lines.next().expect("bt exited before listening").unwrap();
        if let Some((_, addr)) = line.split_once("listening on ") {
            break addr.trim().to_string();
        }
    };
    // Keep draining the log so bt never blocks on a full pipe.
    std::thread::spawn(move || lines.for_each(drop));
    Bt { child, addr }
}

/// Splits a raw HTTP response into its lower-cased header block and the body bytes read so far.
fn get(addr: &str, path: &str, body_bytes: usize) -> (String, Vec<u8>) {
    let mut tcp = TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        tcp,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nRange: bytes=0-1\r\n\r\n"
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
    let bt = start_bt();
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
    let bt = start_bt();
    let (head, body) = get(&bt.addr, "/", 1);
    assert!(head.contains("content-type: text/html"), "{head}");
    assert!(String::from_utf8_lossy(&body).contains("<!doctype html>"));
}
