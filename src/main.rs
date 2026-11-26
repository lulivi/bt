// SPDX-FileCopyrightText: 2026 Lorena Jiménez, Luis Liñán
// SPDX-License-Identifier: MIT

mod radio;
mod state;
mod youtube;

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::stream;
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, watch};

#[derive(Embed)]
#[folder = "web/"]
struct Web;

#[derive(Clone)]
struct App {
    mp3: broadcast::Sender<radio::Chunk>,
    commands: mpsc::Sender<state::Command>,
    state: watch::Receiver<state::State>,
    clock: radio::Clock,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let var = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.into());
    let addr: SocketAddr = var("BT_ADDR", "127.0.0.1:3000")
        .parse()
        .context("BT_ADDR must be <ip>:<port>")?;
    let config = state::Config {
        ffmpeg: var("BT_FFMPEG", "ffmpeg"),
        ytdlp: var("BT_YTDLP", "yt-dlp"),
        cache_dir: PathBuf::from(var("BT_CACHE_DIR", "cache")),
    };
    std::fs::create_dir_all(&config.cache_dir)
        .with_context(|| format!("creating {}", config.cache_dir.display()))?;

    let (ended_tx, ended_rx) = mpsc::unbounded_channel();
    let (mp3, deck, clock) = radio::start(&config.ffmpeg, ended_tx)?;
    let (commands, state) = state::spawn(config, deck, clock.clone(), ended_rx);

    let app = Router::new()
        .route("/stream", get(stream_mp3))
        .route("/ws", get(ws))
        .fallback(get(static_file))
        .with_state(App {
            mp3,
            commands,
            state,
            clock,
        });
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

/// The endless MP3 broadcast. `Range` is ignored on purpose: there is no length to satisfy.
async fn stream_mp3(State(app): State<App>) -> Response {
    // A listener that falls behind the broadcast (`Lagged`) is dropped; it reconnects on its own.
    let chunks = stream::unfold(app.mp3.subscribe(), |mut rx| async move {
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

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ClientMsg {
    Add {
        url: String,
    },
    Play,
    Pause,
    Next,
    Prev,
    Shuffle,
    #[serde(rename_all = "camelCase")]
    Seek {
        position_sec: f64,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ServerMsg<'a> {
    /// The full State, plus the stream time now so clients can follow the stream clock.
    State {
        clock: f64,
        #[serde(flatten)]
        state: &'a state::State,
    },
    Error {
        message: &'a str,
    },
}

async fn ws(upgrade: WebSocketUpgrade, State(app): State<App>) -> Response {
    upgrade
        .max_message_size(4096)
        .on_upgrade(|socket| listener(socket, app))
}

/// One Listener's WebSocket: the full State on connect and on every change; commands in.
async fn listener(mut socket: WebSocket, mut app: App) {
    // `changed()` never fires for the value already there, so send it first.
    let mut out = state_msg(&mut app);
    loop {
        if socket.send(out).await.is_err() {
            return;
        }
        out = loop {
            tokio::select! {
                changed = app.state.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break state_msg(&mut app);
                }
                msg = socket.recv() => {
                    let Some(Ok(msg)) = msg else { return };
                    let Message::Text(text) = msg else { continue };
                    if let Err(message) = handle(&app, &text).await {
                        break to_text(&ServerMsg::Error { message });
                    }
                }
            }
        };
    }
}

async fn handle(app: &App, text: &str) -> Result<(), &'static str> {
    let cmd = match serde_json::from_str(text).map_err(|_| "Unknown command")? {
        ClientMsg::Add { url } => {
            state::Command::Add(youtube::video_id(&url).ok_or("That is not a YouTube video link")?)
        }
        ClientMsg::Play => state::Command::Play,
        ClientMsg::Pause => state::Command::Pause,
        ClientMsg::Next => state::Command::Next,
        ClientMsg::Prev => state::Command::Prev,
        ClientMsg::Shuffle => state::Command::Shuffle,
        ClientMsg::Seek { position_sec } => state::Command::Seek(position_sec),
    };
    app.commands
        .send(cmd)
        .await
        .map_err(|_| "Server is shutting down")
}

fn state_msg(app: &mut App) -> Message {
    let state = app.state.borrow_and_update();
    to_text(&ServerMsg::State {
        clock: app.clock.now(),
        state: &state,
    })
}

fn to_text(msg: &ServerMsg) -> Message {
    Message::text(serde_json::to_string(msg).expect("serializable"))
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
