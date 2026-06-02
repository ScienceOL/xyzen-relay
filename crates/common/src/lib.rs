//! Shared types and helpers across xyzen-relay crates.

use serde::{Deserialize, Serialize};

/// User-facing config for the rendezvous server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RendezvousConfig {
    /// Public IP that we hand out to clients as their relay endpoint.
    pub relay_addr: String,
    /// UDP/TCP port the rendezvous server binds (default 21116).
    pub port: u16,
    /// TCP port the relay listens on (default 21117).
    pub relay_port: u16,
}

impl Default for RendezvousConfig {
    fn default() -> Self {
        Self {
            relay_addr: "127.0.0.1".to_string(),
            port: 21116,
            relay_port: 21117,
        }
    }
}
