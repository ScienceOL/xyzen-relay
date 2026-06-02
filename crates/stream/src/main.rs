//! `xyzen-stream`: ultra-thin video stream fan-out.
//!
//! Two endpoints over WebSocket (axum):
//!   * `POST /ws/stream/:peer_id`  — publisher. Pushes Binary frames.
//!   * `GET  /ws/view/:peer_id`    — subscriber(s). Receives the same frames.
//!
//! For PoC v0 there's no auth: anyone with the URL can publish or watch.
//! Auth comes via control-plane session tokens in v1.
//!
//! Storage model: a `RoomMap` keyed by peer_id holds a `tokio::sync::broadcast`
//! sender. Publishers send into it; each subscriber gets its own receiver.
//! When the publisher disconnects, the room is dropped and subscribers see
//! a clean close.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    Path, State,
};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, Mutex};
use tower_http::trace::TraceLayer;

#[derive(Debug, Parser)]
#[command(
    name = "xyzen-stream",
    version,
    about = "binary stream fan-out for xyzen"
)]
struct Args {
    #[arg(long, env = "XYZEN_STREAM_PORT", default_value_t = 21130)]
    port: u16,
}

type RoomMap = Arc<Mutex<HashMap<String, broadcast::Sender<Bytes>>>>;

#[derive(Clone)]
struct AppState {
    rooms: RoomMap,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let state = AppState {
        rooms: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/ws/stream/:peer_id", get(stream_publisher))
        .route("/ws/view/:peer_id", get(stream_viewer))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let bind = format!("0.0.0.0:{}", args.port);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    tracing::info!("xyzen-stream listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Look up (or create) the broadcast channel for `peer_id`.
async fn room_for(rooms: &RoomMap, peer_id: &str) -> broadcast::Sender<Bytes> {
    let mut map = rooms.lock().await;
    map.entry(peer_id.to_string())
        .or_insert_with(|| broadcast::channel(256).0)
        .clone()
}

async fn stream_publisher(
    Path(peer_id): Path<String>,
    State(s): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_publisher(socket, peer_id, s.rooms).await {
            tracing::warn!("publisher ended: {e}");
        }
    })
}

async fn handle_publisher(mut ws: WebSocket, peer_id: String, rooms: RoomMap) -> Result<()> {
    tracing::info!("publisher connected for peer_id={peer_id}");
    let tx = room_for(&rooms, &peer_id).await;

    while let Some(item) = ws.next().await {
        match item? {
            Message::Binary(bytes) => {
                let n_recv = tx.receiver_count();
                let _ = tx.send(Bytes::from(bytes));
                tracing::trace!("peer_id={peer_id} frame={} bytes fanout={n_recv}", "?",);
            }
            Message::Close(_) => break,
            // Ignore text / ping / pong / fragment.
            _ => {}
        }
    }

    // Drop the room if no one is publishing or watching anymore.
    let mut map = rooms.lock().await;
    if let Some(s) = map.get(&peer_id) {
        if s.receiver_count() == 0 {
            map.remove(&peer_id);
            tracing::info!("publisher exited, room dropped for peer_id={peer_id}");
        } else {
            tracing::info!(
                "publisher exited, {} viewers still subscribed to peer_id={peer_id}",
                s.receiver_count()
            );
        }
    }
    Ok(())
}

async fn stream_viewer(
    Path(peer_id): Path<String>,
    State(s): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_viewer(socket, peer_id, s.rooms).await {
            tracing::warn!("viewer ended: {e}");
        }
    })
}

async fn handle_viewer(ws: WebSocket, peer_id: String, rooms: RoomMap) -> Result<()> {
    tracing::info!("viewer connected for peer_id={peer_id}");
    let tx = room_for(&rooms, &peer_id).await;
    let mut rx = tx.subscribe();

    let (mut sink, mut stream) = ws.split();

    // Reader task: just discard inbound frames (viewer doesn't talk back yet),
    // but watch for close so we can shut down cleanly.
    let close_watch = tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            match item {
                Ok(Message::Close(_)) | Err(_) => break,
                _ => continue,
            }
        }
    });

    loop {
        tokio::select! {
            biased;
            _ = &mut Box::pin(async {
                // Tiny shim so close_watch can be polled too.
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }) => {
                // Heartbeat ping every minute keeps idle WS alive through proxies.
                if sink.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
            res = rx.recv() => {
                match res {
                    Ok(bytes) => {
                        if sink.send(Message::Binary(bytes.to_vec())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("viewer lagged, dropped {n} frames");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    close_watch.abort();
    Ok(())
}
