//! Rendezvous TCP + UDP + WS loops.
//!
//! Three transports converge on a single dispatch:
//!   * UDP 21116 — native clients (single datagrams, no session).
//!   * TCP 21116 — native clients for the punch-hole handshake.
//!   * WS  21118 — browser / RN clients. Long-lived bidirectional.
//!
//! Each TCP/WS connection runs as a pair of tasks (reader + writer)
//! glued by an mpsc channel. Server-initiated pushes go via the channel
//! so we don't need to share the underlying stream.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::codec::Framed;

use xyzen_relay_proto::codec::RustDeskCodec;
use xyzen_relay_proto::hbb::{
    rendezvous_message::Union, RegisterPeerResponse, RegisterPkResponse, RelayResponse,
    RendezvousMessage, RequestRelay,
};

use crate::audit::{Auditor, Event};
use crate::registry::{PeerRegistry, Reachable};

/// Inbound message sink: a connection's writer task pulls from this.
type Outbox = mpsc::UnboundedSender<RendezvousMessage>;

/// Pending punch lookup: target_id -> requester's outbox.
type Pending = std::sync::Arc<Mutex<std::collections::HashMap<String, Outbox>>>;

pub async fn run(relay_addr: &str, port: u16, ws_port: u16, auditor: Auditor) -> Result<()> {
    let bind: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let ws_bind: SocketAddr = format!("0.0.0.0:{ws_port}").parse()?;
    let udp = UdpSocket::bind(bind)
        .await
        .with_context(|| format!("bind UDP {bind}"))?;
    let tcp = TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind TCP {bind}"))?;
    let ws = TcpListener::bind(ws_bind)
        .await
        .with_context(|| format!("bind WS {ws_bind}"))?;
    log::info!(
        "xyzen-rendezvous listening on udp+tcp {bind}, ws {ws_bind}, relay_addr={relay_addr}"
    );

    let registry = PeerRegistry::new();
    let udp = std::sync::Arc::new(udp);
    let pending: Pending = std::sync::Arc::new(Mutex::new(std::collections::HashMap::new()));

    let udp_task = tokio::spawn(udp_loop(udp.clone(), registry.clone(), auditor.clone()));
    let tcp_task = tokio::spawn(tcp_loop(
        tcp,
        relay_addr.to_string(),
        registry.clone(),
        udp.clone(),
        pending.clone(),
        auditor.clone(),
    ));
    let ws_task = tokio::spawn(ws_loop(
        ws,
        relay_addr.to_string(),
        registry.clone(),
        udp.clone(),
        pending,
        auditor,
    ));

    tokio::try_join!(flatten(udp_task), flatten(tcp_task), flatten(ws_task))?;
    Ok(())
}

async fn flatten<T>(h: tokio::task::JoinHandle<Result<T>>) -> Result<T> {
    match h.await {
        Ok(r) => r,
        Err(e) => Err(anyhow::anyhow!("task panicked: {e}")),
    }
}

// =========================================================================
// UDP — native client registration only.
// =========================================================================

async fn udp_loop(
    udp: std::sync::Arc<UdpSocket>,
    registry: PeerRegistry,
    auditor: Auditor,
) -> Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let (n, addr) = match udp.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("udp recv: {e}");
                continue;
            }
        };
        // RustDesk's UDP path is one protobuf message per datagram with no
        // length prefix — only the TCP/WS paths are framed.
        let msg = match RendezvousMessage::decode(&buf[..n]) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("udp protobuf decode from {addr}: {e}");
                continue;
            }
        };
        match msg.union {
            Some(Union::RegisterPeer(rp)) => {
                log::info!("register_peer (udp) id={} from {addr}", rp.id);
                let addr_s = addr.to_string();
                auditor.emit(Event {
                    kind: "register",
                    peer_id: Some(&rp.id),
                    controller_peer_id: None,
                    addr: Some(&addr_s),
                    meta: Some(serde_json::json!({"transport": "udp"})),
                });
                registry.upsert(rp.id, Reachable::Udp(addr)).await;
                let mut out = RendezvousMessage::default();
                out.union = Some(Union::RegisterPeerResponse(RegisterPeerResponse {
                    request_pk: false,
                }));
                send_udp(&udp, &out, addr).await;
            }
            Some(Union::RegisterPk(_)) => {
                log::info!("register_pk (udp) from {addr}");
                let mut out = RendezvousMessage::default();
                out.union = Some(Union::RegisterPkResponse(RegisterPkResponse {
                    result: 0,
                    keep_alive: 0,
                }));
                send_udp(&udp, &out, addr).await;
            }
            other => {
                log::debug!("udp ignore {:?} from {addr}", other.as_ref().map(tag));
            }
        }
    }
}

// =========================================================================
// TCP — native punch handshake. Short-lived: read first message, dispatch.
// =========================================================================

async fn tcp_loop(
    listener: TcpListener,
    relay_addr: String,
    registry: PeerRegistry,
    udp: std::sync::Arc<UdpSocket>,
    pending: Pending,
    auditor: Auditor,
) -> Result<()> {
    loop {
        let (sock, peer_addr) = listener.accept().await?;
        let registry = registry.clone();
        let udp = udp.clone();
        let pending = pending.clone();
        let relay_addr = relay_addr.clone();
        let auditor = auditor.clone();
        tokio::spawn(async move {
            let framed = Framed::new(sock, RustDeskCodec::new());
            if let Err(e) = handle_tcp(
                framed, peer_addr, relay_addr, registry, udp, pending, auditor,
            )
            .await
            {
                log::warn!("tcp {peer_addr}: {e}");
            }
        });
    }
}

async fn handle_tcp(
    mut framed: Framed<TcpStream, RustDeskCodec>,
    peer_addr: SocketAddr,
    relay_addr: String,
    registry: PeerRegistry,
    udp: std::sync::Arc<UdpSocket>,
    pending: Pending,
    auditor: Auditor,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<RendezvousMessage>();

    // Read the first message with a generous timeout — TCP path is
    // single-shot today.
    let frame = tokio::time::timeout(std::time::Duration::from_secs(15), framed.next())
        .await
        .map_err(|_| anyhow::anyhow!("first-frame timeout"))?;
    let bytes = match frame {
        Some(Ok(b)) => b,
        Some(Err(e)) => return Err(anyhow::anyhow!("decode: {e}")),
        None => return Ok(()),
    };
    let msg = RendezvousMessage::decode(&bytes[..])?;

    match msg.union {
        Some(Union::PunchHoleRequest(ph)) => {
            log::info!(
                "tcp punch_hole_request from {peer_addr} target_id={}",
                ph.id
            );
            let addr_s = peer_addr.to_string();
            auditor.emit(Event {
                kind: "punch_request",
                peer_id: Some(&ph.id),
                controller_peer_id: None,
                addr: Some(&addr_s),
                meta: Some(serde_json::json!({"transport": "tcp"})),
            });
            dispatch_punch(&ph.id, &tx, &registry, &udp, &relay_addr, &pending).await;
        }
        Some(Union::RelayResponse(_)) => {
            log::info!("tcp relay_response from {peer_addr}");
            let addr_s = peer_addr.to_string();
            auditor.emit(Event {
                kind: "relay_ack",
                peer_id: None,
                controller_peer_id: None,
                addr: Some(&addr_s),
                meta: Some(serde_json::json!({"transport": "tcp"})),
            });
            dispatch_relay_ack(&relay_addr, &pending).await;
            return Ok(());
        }
        other => {
            log::debug!("tcp ignore {:?} from {peer_addr}", other.as_ref().map(tag));
            return Ok(());
        }
    }

    // After PunchHoleRequest we wait for the server-side push (RelayResponse)
    // and forward it on this same TCP socket.
    while let Some(msg) = rx.recv().await {
        let mut buf = Vec::with_capacity(msg.encoded_len());
        if msg.encode(&mut buf).is_ok() {
            let _ = framed.send(Bytes::from(buf)).await;
        }
        break; // single push, then close
    }
    Ok(())
}

// =========================================================================
// WS — long-lived. Browser/RN clients live here.
// =========================================================================

async fn ws_loop(
    listener: TcpListener,
    relay_addr: String,
    registry: PeerRegistry,
    udp: std::sync::Arc<UdpSocket>,
    pending: Pending,
    auditor: Auditor,
) -> Result<()> {
    loop {
        let (sock, peer_addr) = listener.accept().await?;
        let registry = registry.clone();
        let udp = udp.clone();
        let pending = pending.clone();
        let relay_addr = relay_addr.clone();
        let auditor = auditor.clone();
        tokio::spawn(async move {
            let stream = match tokio_tungstenite::accept_async(sock).await {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("ws handshake {peer_addr}: {e}");
                    return;
                }
            };
            log::info!("ws connection from {peer_addr}");
            if let Err(e) = handle_ws(
                stream, peer_addr, relay_addr, registry, udp, pending, auditor,
            )
            .await
            {
                log::warn!("ws {peer_addr}: {e}");
            }
        });
    }
}

async fn handle_ws(
    ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    peer_addr: SocketAddr,
    relay_addr: String,
    registry: PeerRegistry,
    udp: std::sync::Arc<UdpSocket>,
    pending: Pending,
    auditor: Auditor,
) -> Result<()> {
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<RendezvousMessage>();
    let mut my_id: Option<String> = None;

    // Writer task: drain outgoing queue and push as Binary frames.
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let mut buf = Vec::with_capacity(msg.encoded_len());
            if msg.encode(&mut buf).is_err() {
                break;
            }
            if sink.send(WsMessage::Binary(buf)).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    while let Some(item) = stream.next().await {
        let frame = match item {
            Ok(WsMessage::Binary(b)) => b,
            Ok(WsMessage::Text(_))
            | Ok(WsMessage::Ping(_))
            | Ok(WsMessage::Pong(_))
            | Ok(WsMessage::Frame(_)) => continue,
            Ok(WsMessage::Close(_)) | Err(_) => break,
        };
        let msg = match RendezvousMessage::decode(&frame[..]) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("ws {peer_addr}: protobuf decode: {e}");
                continue;
            }
        };

        match msg.union {
            Some(Union::RegisterPeer(rp)) => {
                log::info!("register_peer (ws) id={} from {peer_addr}", rp.id);
                let addr_s = peer_addr.to_string();
                auditor.emit(Event {
                    kind: "register",
                    peer_id: Some(&rp.id),
                    controller_peer_id: None,
                    addr: Some(&addr_s),
                    meta: Some(serde_json::json!({"transport": "ws"})),
                });
                registry
                    .upsert(rp.id.clone(), Reachable::WsPush(tx.clone()))
                    .await;
                my_id = Some(rp.id);
                let mut out = RendezvousMessage::default();
                out.union = Some(Union::RegisterPeerResponse(RegisterPeerResponse {
                    request_pk: false,
                }));
                let _ = tx.send(out);
            }
            Some(Union::RegisterPk(rk)) => {
                log::info!("register_pk (ws) from {peer_addr}");
                if !rk.id.is_empty() {
                    registry
                        .upsert(rk.id.clone(), Reachable::WsPush(tx.clone()))
                        .await;
                    my_id = Some(rk.id);
                }
                let mut out = RendezvousMessage::default();
                out.union = Some(Union::RegisterPkResponse(RegisterPkResponse {
                    result: 0,
                    keep_alive: 0,
                }));
                let _ = tx.send(out);
            }
            Some(Union::PunchHoleRequest(ph)) => {
                log::info!("ws punch_hole_request from {peer_addr} target_id={}", ph.id);
                let addr_s = peer_addr.to_string();
                auditor.emit(Event {
                    kind: "punch_request",
                    peer_id: Some(&ph.id),
                    controller_peer_id: None,
                    addr: Some(&addr_s),
                    meta: Some(serde_json::json!({"transport": "ws"})),
                });
                dispatch_punch(&ph.id, &tx, &registry, &udp, &relay_addr, &pending).await;
            }
            Some(Union::RelayResponse(_)) => {
                log::info!("ws relay_response from {peer_addr}");
                let addr_s = peer_addr.to_string();
                auditor.emit(Event {
                    kind: "relay_ack",
                    peer_id: None,
                    controller_peer_id: None,
                    addr: Some(&addr_s),
                    meta: Some(serde_json::json!({"transport": "ws"})),
                });
                dispatch_relay_ack(&relay_addr, &pending).await;
            }
            other => {
                log::debug!("ws ignore {:?} from {peer_addr}", other.as_ref().map(tag));
            }
        }
    }

    if let Some(id) = my_id {
        registry.remove_if_ws(&id, &tx).await;
    }
    drop(tx);
    let _ = writer.await;
    Ok(())
}

// =========================================================================
// Dispatch helpers.
// =========================================================================

async fn dispatch_punch(
    target_id: &str,
    requester_tx: &Outbox,
    registry: &PeerRegistry,
    udp: &UdpSocket,
    relay_addr: &str,
    pending: &Pending,
) {
    let reach = match registry.get(target_id).await {
        Some(r) => r,
        None => {
            log::info!("target id={target_id} offline, dropping requester");
            return;
        }
    };
    let mut to_target = RendezvousMessage::default();
    to_target.union = Some(Union::RequestRelay(RequestRelay {
        relay_server: relay_addr.to_string(),
        ..Default::default()
    }));
    match reach {
        Reachable::Udp(addr) => send_udp(udp, &to_target, addr).await,
        Reachable::WsPush(target_tx) => {
            let _ = target_tx.send(to_target);
        }
    }
    pending
        .lock()
        .await
        .insert(target_id.to_string(), requester_tx.clone());
}

async fn dispatch_relay_ack(relay_addr: &str, pending: &Pending) {
    let mut map = pending.lock().await;
    let key = map.keys().next().cloned();
    if let Some(k) = key {
        if let Some(requester) = map.remove(&k) {
            let mut out = RendezvousMessage::default();
            out.union = Some(Union::RelayResponse(RelayResponse {
                relay_server: relay_addr.to_string(),
                ..Default::default()
            }));
            let _ = requester.send(out);
        }
    }
}

// =========================================================================
// Misc helpers.
// =========================================================================

async fn send_udp(udp: &UdpSocket, msg: &RendezvousMessage, addr: SocketAddr) {
    let mut buf = Vec::with_capacity(msg.encoded_len());
    if msg.encode(&mut buf).is_err() {
        return;
    }
    if let Err(e) = udp.send_to(&buf, addr).await {
        log::warn!("udp send to {addr}: {e}");
    }
}

fn tag(u: &Union) -> &'static str {
    match u {
        Union::RegisterPeer(_) => "register_peer",
        Union::RegisterPeerResponse(_) => "register_peer_response",
        Union::PunchHoleRequest(_) => "punch_hole_request",
        Union::PunchHole(_) => "punch_hole",
        Union::PunchHoleSent(_) => "punch_hole_sent",
        Union::PunchHoleResponse(_) => "punch_hole_response",
        Union::FetchLocalAddr(_) => "fetch_local_addr",
        Union::LocalAddr(_) => "local_addr",
        Union::ConfigureUpdate(_) => "configure_update",
        Union::RegisterPk(_) => "register_pk",
        Union::RegisterPkResponse(_) => "register_pk_response",
        Union::SoftwareUpdate(_) => "software_update",
        Union::RequestRelay(_) => "request_relay",
        Union::RelayResponse(_) => "relay_response",
        Union::TestNatRequest(_) => "test_nat_request",
        Union::TestNatResponse(_) => "test_nat_response",
        Union::PeerDiscovery(_) => "peer_discovery",
        Union::OnlineRequest(_) => "online_request",
        Union::OnlineResponse(_) => "online_response",
        Union::KeyExchange(_) => "key_exchange",
        Union::Hc(_) => "hc",
        Union::HttpProxyRequest(_) => "http_proxy_request",
        Union::HttpProxyResponse(_) => "http_proxy_response",
    }
}
