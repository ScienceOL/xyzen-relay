//! `xyzen-rendezvous`: minimal hbbs replacement.
//!
//! First-milestone scope (matches `rustdesk-server-demo`):
//! * UDP: respond to `RegisterPeer` and `RegisterPk`.
//! * TCP: on `PunchHoleRequest`, look the peer up in the registry, ask it
//!   over UDP to relay; when the peer comes back with `RelayResponse`,
//!   tell the requester which relay to use.
//!
//! No NAT hole-punching, no encryption, no persistence yet.

mod audit;
mod registry;
mod server;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "xyzen-rendezvous", version, about = "RustDesk-compatible rendezvous server")]
struct Args {
    /// Public address handed to clients as their relay server.
    #[arg(long, env = "XYZEN_RELAY_ADDR", default_value = "127.0.0.1")]
    relay_addr: String,

    /// Bind port for both UDP and TCP rendezvous (RustDesk default 21116).
    #[arg(long, env = "XYZEN_RDV_PORT", default_value_t = 21116)]
    port: u16,

    /// WebSocket port (RustDesk default 21118 — used by web/RN clients).
    #[arg(long, env = "XYZEN_RDV_WS_PORT", default_value_t = 21118)]
    ws_port: u16,

    /// Optional audit ingest URL on the control plane
    /// (e.g. http://127.0.0.1:21120/v1/_internal/audit).
    /// When unset, events are only logged.
    #[arg(long, env = "XYZEN_AUDIT_URL")]
    audit_url: Option<String>,

    /// Bearer token for the control-plane audit ingest endpoint.
    #[arg(long, env = "XYZEN_CONTROL_TOKEN")]
    audit_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let auditor = audit::Auditor::new(args.audit_url, args.audit_token);
    server::run(&args.relay_addr, args.port, args.ws_port, auditor).await
}
