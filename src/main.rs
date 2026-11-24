mod radio;

use std::io::IsTerminal;
use std::net::SocketAddr;

use anyhow::Context;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::stream;
use rust_embed::Embed;
use tokio::sync::broadcast;

#[derive(Embed)]
#[folder = "web/"]
struct Web;

struct Config {
    addr: SocketAddr,
    ffmpeg: String,
}

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        let var =
            |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.into());
        Ok(Self {
            addr: var("BT_ADDR", "127.0.0.1:3000")
                .parse()
                .context("BT_ADDR must be <ip>:<port>")?,
            ffmpeg: var("BT_FFMPEG", "ffmpeg"),
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let config = Config::from_env()?;
    let mp3 = radio::start(&config.ffmpeg)?;

    let app = Router::new()
        .route("/stream", get(stream_mp3))
        .fallback(get(static_file))
        .with_state(mp3);
    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("binding {}", config.addr))?;
    tracing::info!("listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

/// The endless MP3 broadcast. `Range` is ignored on purpose: there is no length to satisfy.
async fn stream_mp3(State(mp3): State<broadcast::Sender<radio::Chunk>>) -> Response {
    // A listener that falls behind the broadcast (`Lagged`) is dropped; it reconnects on its own.
    let chunks = stream::unfold(mp3.subscribe(), |mut rx| async move {
        let chunk = rx.recv().await.ok()?;
        Some((Ok::<_, std::convert::Infallible>(chunk), rx))
    });
    (
        [
            (header::CONTENT_TYPE, "audio/mpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(chunks),
    )
        .into_response()
}

async fn static_file(uri: Uri) -> Response {
    let path = match uri.path().trim_start_matches('/') {
        "" => "index.html",
        p => p,
    };
    let Some(file) = Web::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        _ => "application/octet-stream",
    };
    ([(header::CONTENT_TYPE, mime)], file.data).into_response()
}
