//! In-memory peer registry.
//!
//! A peer can be reachable over either UDP (native client) or a long-lived
//! WS push channel (browser/RN client). Real deployments will swap this
//! for Redis or Postgres.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{mpsc, RwLock};

use xyzen_relay_proto::hbb::RendezvousMessage;

/// How we reach a registered peer to push them a server-initiated message.
#[derive(Debug, Clone)]
pub enum Reachable {
    /// Native client: send a framed `RendezvousMessage` over UDP.
    Udp(SocketAddr),
    /// Web/RN client: push into the writer-task channel of their WS conn.
    WsPush(mpsc::UnboundedSender<RendezvousMessage>),
}

#[derive(Debug, Clone)]
pub struct PeerEntry {
    pub reach: Reachable,
    #[allow(dead_code)]
    pub last_seen: Instant,
}

#[derive(Debug, Default, Clone)]
pub struct PeerRegistry {
    inner: Arc<RwLock<HashMap<String, PeerEntry>>>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn upsert(&self, id: String, reach: Reachable) {
        let mut map = self.inner.write().await;
        map.insert(
            id,
            PeerEntry {
                reach,
                last_seen: Instant::now(),
            },
        );
    }

    pub async fn get(&self, id: &str) -> Option<Reachable> {
        self.inner.read().await.get(id).map(|p| p.reach.clone())
    }

    /// Drop a peer if its current entry's WS channel matches the one we
    /// hold (i.e. the writer task is shutting down). Used on disconnect.
    pub async fn remove_if_ws(&self, id: &str, sender: &mpsc::UnboundedSender<RendezvousMessage>) {
        let mut map = self.inner.write().await;
        let drop_it = matches!(map.get(id), Some(e) if matches!(&e.reach, Reachable::WsPush(s) if s.same_channel(sender)));
        if drop_it {
            map.remove(id);
        }
    }
}
