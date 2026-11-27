// SPDX-FileCopyrightText: 2026 Lorena Jiménez, Luis Liñán
// SPDX-License-Identifier: MIT

//! bt, a shared radio: the web server, its configuration and the Listeners' connections.

mod auth;
mod config;
mod persist;
mod radio;
mod state;
mod youtube;

use std::io::IsTerminal;
use std::sync::Arc;

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

/// The web page, embedded in the binary.
#[derive(Embed)]
#[folder = "web/"]
struct Web;

/// What every request handler shares.
#[derive(Clone)]
struct App {
    /// The MP3 broadcast, to subscribe to.
    mp3: broadcast::Sender<radio::Chunk>,
    /// The state actor's inbox.
    commands: mpsc::Sender<state::Command>,
    /// The published State.
    state: watch::Receiver<state::State>,
    /// Stream time, sent with every State.
    clock: radio::Clock,
}

/// Starts the Radio, the state actor and the web server, configured by the config file and the `BT_*` variables.
///
/// # Errors
///
/// Fails if the configuration is invalid, the files can't be set up, ffmpeg doesn't start or the
/// address can't be bound.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let settings = config::load(|name| std::env::var(name).ok())?;
    match &settings.file {
        Some(file) => tracing::info!("settings from {}", file.display()),
        None => tracing::info!("no config file: settings from defaults and BT_* variables"),
    }
    let config = state::Config {
        ffmpeg: settings.ffmpeg,
        ytdlp: settings.ytdlp,
        cache_dir: settings.cache_dir,
    };
    std::fs::create_dir_all(&config.cache_dir)
        .with_context(|| format!("creating {}", config.cache_dir.display()))?;
    let state_file = settings.state_file;
    if let Some(dir) = state_file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let restored = persist::load(&state_file)?;

    let (ended_tx, ended_rx) = mpsc::unbounded_channel();
    let (mp3, deck, clock) = radio::start(&config.ffmpeg, ended_tx)?;
    let (commands, state) = state::spawn(config, restored, deck, clock.clone(), ended_rx);
    tokio::spawn(persist::save_changes(state_file, state.clone()));

    let mut app = Router::new()
        .route("/stream", get(stream_mp3))
        .route("/ws", get(ws))
        .fallback(get(static_file))
        .with_state(App {
            mp3,
            commands,
            state,
            clock,
        });
    match settings.access {
        Some(access) => {
            for (name, link) in access.links() {
                tracing::info!("access link for {name}: {link}");
            }
            app = app.layer(axum::middleware::from_fn_with_state(
                Arc::new(access),
                auth::check,
            ));
        }
        None => tracing::warn!("no Access links are configured: anyone can use bt"),
    }
    let addr = settings.addr;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("listening on {}", listener.local_addr()?);
    // Not a graceful shutdown: `/stream` and `/ws` never close, so it would wait forever, and the
    // Queue is already saved after every change.
    tokio::select! {
        served = axum::serve(listener, app) => served?,
        signal = stop_signal() => tracing::info!("{}, stopping", signal?),
    }
    Ok(())
}

/// Waits for SIGINT (Ctrl-C) or SIGTERM (`docker stop`, `systemctl stop`) and returns its name.
///
/// # Errors
///
/// Fails if the signal handlers can't be installed.
async fn stop_signal() -> anyhow::Result<&'static str> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut int = signal(SignalKind::interrupt())?;
    let mut term = signal(SignalKind::terminate())?;
    Ok(tokio::select! {
        _ = int.recv() => "SIGINT",
        _ = term.recv() => "SIGTERM",
    })
}

/// Serves the endless MP3 broadcast.
///
/// `Range` is ignored on purpose: there is no length to satisfy.
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

/// A message from a Listener.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ClientMsg {
    /// Append the Song in this link to the Queue.
    Add {
        /// The link as pasted, not yet validated.
        url: String,
    },
    /// Resume the Radio.
    Play,
    /// Silence the Radio.
    Pause,
    /// Move to the next Entry.
    Next,
    /// Restart the Current entry or move to the previous one.
    Prev,
    /// Shuffle the Entries after the Current entry.
    Shuffle,
    /// Set the Loop mode.
    SetLoop {
        /// The requested mode.
        #[serde(deserialize_with = "state::LoopMode::read")]
        r#loop: state::LoopMode,
    },
    /// Advance to the next Loop mode.
    CycleLoop,
    /// Jump to a position in the Current entry.
    #[serde(rename_all = "camelCase")]
    Seek {
        /// The position, in seconds.
        position_sec: f64,
    },
}

/// A message to a Listener.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ServerMsg<'a> {
    /// The full State, plus the stream time now so clients can follow the stream clock.
    State {
        /// Stream time now, in seconds.
        clock: f64,
        /// The State, its fields inlined in the message.
        #[serde(flatten)]
        state: &'a state::State,
    },
    /// Why the Listener's last message was rejected.
    Error {
        /// The text to show.
        message: &'a str,
    },
}

/// Upgrades a request to a Listener's WebSocket.
///
/// Messages over 4 KB are refused.
async fn ws(upgrade: WebSocketUpgrade, State(app): State<App>) -> Response {
    upgrade
        .max_message_size(4096)
        .on_upgrade(|socket| listener(socket, app))
}

/// Serves one Listener's WebSocket until it closes.
///
/// Sends the full State on connect and on every change, and handles the messages received.
/// Errors go to this Listener only.
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

/// Handles one message from a Listener, sending its command to the state actor.
///
/// # Errors
///
/// Returns the text to show the Listener if the message is unknown, the link isn't a YouTube
/// video or the server is shutting down.
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
        ClientMsg::SetLoop { r#loop } => state::Command::SetLoop(r#loop),
        ClientMsg::CycleLoop => state::Command::CycleLoop,
        ClientMsg::Seek { position_sec } => state::Command::Seek(position_sec),
    };
    app.commands
        .send(cmd)
        .await
        .map_err(|_| "Server is shutting down")
}

/// Builds the message with the current State, marking it as seen.
fn state_msg(app: &mut App) -> Message {
    let state = app.state.borrow_and_update();
    to_text(&ServerMsg::State {
        clock: app.clock.now(),
        state: &state,
    })
}

/// Serializes `msg` as a WebSocket text message.
///
/// # Panics
///
/// Panics if `msg` can't be serialized, which none of its variants cause.
fn to_text(msg: &ServerMsg) -> Message {
    Message::text(serde_json::to_string(msg).expect("serializable"))
}

/// Serves a file of the embedded web page, or 404.
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
