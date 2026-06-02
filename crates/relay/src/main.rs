//! `xyzen-relay`: minimal hbbr replacement.
//!
//! PoC behaviour (parity with `rustdesk-server-demo`): pair the next two
//! incoming TCP connections, switch both into raw passthrough, and
//! shovel bytes between them. Real deployments will key by `uuid` so we
//! pair the *right* peers, but the demo doesn't and clients tolerate it
//! when there's only one session in flight.

//! Minimal hbbr replacement.
//!
//! Two listeners:
//! * 21117 — raw TCP. Pair the next two connections, byte-shovel between.
//! * 21119 — WebSocket. Same pairing, but each side's bytes ride a Binary
//!   frame so browser clients can participate.
//!
//! Cross-transport pairing (TCP <-> WS) is supported: if a TCP peer is
//! already waiting and a WS peer arrives, they're paired and the relay
//! transparently rewraps frames.

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::{tungstenite::Message as WsMessage, WebSocketStream};

#[derive(Debug, Parser)]
#[command(name = "xyzen-relay", version, about = "RustDesk-compatible TCP relay")]
struct Args {
    /// Raw TCP relay port (RustDesk default 21117).
    #[arg(long, env = "XYZEN_RELAY_PORT", default_value_t = 21117)]
    port: u16,
    /// WebSocket relay port (RustDesk default 21119, used by web/RN clients).
    #[arg(long, env = "XYZEN_RELAY_WS_PORT", default_value_t = 21119)]
    ws_port: u16,
}

enum Side {
    Tcp(TcpStream),
    Ws(WebSocketStream<TcpStream>),
}

type WaitSlot = std::sync::Arc<Mutex<Option<Side>>>;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let tcp_bind = format!("0.0.0.0:{}", args.port);
    let ws_bind = format!("0.0.0.0:{}", args.ws_port);
    let tcp = TcpListener::bind(&tcp_bind)
        .await
        .with_context(|| format!("bind {tcp_bind}"))?;
    let ws = TcpListener::bind(&ws_bind)
        .await
        .with_context(|| format!("bind {ws_bind}"))?;
    log::info!("xyzen-relay listening on tcp {tcp_bind}, ws {ws_bind}");

    let waiting: WaitSlot = std::sync::Arc::new(Mutex::new(None));

    let tcp_task = {
        let waiting = waiting.clone();
        tokio::spawn(async move {
            loop {
                let (sock, addr) = match tcp.accept().await {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("tcp accept: {e}");
                        continue;
                    }
                };
                log::info!("relay tcp from {addr}");
                pair_or_wait(Side::Tcp(sock), waiting.clone()).await;
            }
        })
    };

    let ws_task = {
        let waiting = waiting.clone();
        tokio::spawn(async move {
            loop {
                let (sock, addr) = match ws.accept().await {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("ws accept: {e}");
                        continue;
                    }
                };
                let stream = match tokio_tungstenite::accept_async(sock).await {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("ws handshake {addr}: {e}");
                        continue;
                    }
                };
                log::info!("relay ws from {addr}");
                pair_or_wait(Side::Ws(stream), waiting.clone()).await;
            }
        })
    };

    let _ = tokio::try_join!(tcp_task, ws_task)?;
    Ok(())
}

async fn pair_or_wait(side: Side, waiting: WaitSlot) {
    let partner = {
        let mut slot = waiting.lock().await;
        slot.take()
    };
    match partner {
        Some(other) => {
            tokio::spawn(async move {
                if let Err(e) = pump(side, other).await {
                    log::warn!("relay pair ended: {e}");
                }
            });
        }
        None => {
            waiting.lock().await.replace(side);
        }
    }
}

async fn pump(a: Side, b: Side) -> Result<()> {
    match (a, b) {
        (Side::Tcp(a), Side::Tcp(b)) => pump_tcp_tcp(a, b).await,
        (Side::Ws(a), Side::Ws(b)) => pump_ws_ws(a, b).await,
        (Side::Tcp(t), Side::Ws(w)) | (Side::Ws(w), Side::Tcp(t)) => pump_tcp_ws(t, w).await,
    }
}

async fn pump_tcp_tcp(a: TcpStream, b: TcpStream) -> Result<()> {
    let (mut ar, mut aw) = a.into_split();
    let (mut br, mut bw) = b.into_split();
    let f1 = tokio::io::copy(&mut ar, &mut bw);
    let f2 = tokio::io::copy(&mut br, &mut aw);
    tokio::try_join!(f1, f2).map(|_| ())?;
    Ok(())
}

async fn pump_ws_ws(a: WebSocketStream<TcpStream>, b: WebSocketStream<TcpStream>) -> Result<()> {
    let (mut a_tx, mut a_rx) = a.split();
    let (mut b_tx, mut b_rx) = b.split();
    let f1 = async {
        while let Some(m) = a_rx.next().await {
            let m = m?;
            if matches!(m, WsMessage::Close(_)) {
                break;
            }
            b_tx.send(m).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let f2 = async {
        while let Some(m) = b_rx.next().await {
            let m = m?;
            if matches!(m, WsMessage::Close(_)) {
                break;
            }
            a_tx.send(m).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::try_join!(f1, f2).map(|_| ())
}

async fn pump_tcp_ws(t: TcpStream, w: WebSocketStream<TcpStream>) -> Result<()> {
    let (mut tr, mut tw) = t.into_split();
    let (mut w_tx, mut w_rx) = w.split();
    let f1 = async {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            let n = tr.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            w_tx.send(WsMessage::Binary(buf[..n].to_vec())).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let f2 = async {
        while let Some(m) = w_rx.next().await {
            match m? {
                WsMessage::Binary(b) => tw.write_all(&b).await?,
                WsMessage::Close(_) => break,
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::try_join!(f1, f2).map(|_| ())
}
