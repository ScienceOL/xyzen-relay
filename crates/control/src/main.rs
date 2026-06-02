//! `xyzen-control`: HTTP control plane for xyzen-relay.
//!
//! Single-binary v1: hosts the rendezvous + relay listeners alongside an
//! HTTP API. We split to separate processes (and swap sqlite → postgres)
//! when we need to scale horizontally.
//!
//! Authentication: every protected route requires a bearer token matching
//! `--token` / `XYZEN_CONTROL_TOKEN`. Xyzen's backend holds this secret;
//! end users never see it.

mod audit;
mod db;
mod http;
mod ids;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "xyzen-control", version, about = "xyzen-relay control plane")]
struct Args {
    /// Bind port for the HTTP control API.
    #[arg(long, env = "XYZEN_CONTROL_PORT", default_value_t = 21120)]
    port: u16,

    /// Shared bearer token clients must present.
    #[arg(long, env = "XYZEN_CONTROL_TOKEN")]
    token: String,

    /// Path to the SQLite database file.
    #[arg(long, env = "XYZEN_CONTROL_DB", default_value = "xyzen-control.sqlite")]
    db: String,
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
    let pool = db::connect(&args.db).await?;
    db::migrate(&pool).await?;

    http::serve(args.port, args.token, pool).await
}
