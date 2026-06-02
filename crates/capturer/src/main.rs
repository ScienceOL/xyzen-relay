//! `xyzen-capturer`: native screen-capture publisher.
//!
//! v1: ScreenCaptureKit + VideoToolbox via `xyzen-relay-mac-capturer`
//! (macOS only). Each H264 NAL unit is forwarded as a single Binary
//! WebSocket frame to the stream server.
//!
//! Linux/Windows backends will land in this same crate behind cfg flags;
//! they all produce a `mpsc::Receiver<Nal>` so the WS-publishing tail
//! is platform-agnostic.

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::SinkExt;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Parser)]
#[command(name = "xyzen-capturer", about = "screen → H264 → xyzen-stream WS")]
struct Args {
    /// Stream peer id (the room name on xyzen-stream).
    #[arg(long, env = "XYZEN_PEER_ID")]
    peer_id: String,

    /// xyzen-stream publisher URL. Path part is auto-suffixed with the peer id.
    #[arg(
        long,
        env = "XYZEN_STREAM_URL",
        default_value = "ws://127.0.0.1:21130/ws/stream"
    )]
    stream_url: String,

    /// Frame width — capturer scales to this.
    #[arg(long, default_value_t = 1920)]
    width: u32,

    /// Frame height.
    #[arg(long, default_value_t = 1080)]
    height: u32,

    /// Frames per second.
    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// Bitrate hint, in kbps.
    #[arg(long, default_value_t = 4000)]
    bitrate_kbps: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let url = format!("{}/{}", args.stream_url.trim_end_matches('/'), args.peer_id);
    log::info!("connecting publisher to {url}");
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("ws connect {url}"))?;
    log::info!("publisher connected, starting capturer");
    let (mut sink, _) = futures_util::StreamExt::split(ws);

    let rx = start_native(args.width, args.height, args.fps, args.bitrate_kbps)?;

    let mut total: u64 = 0;
    let mut nals: u64 = 0;
    let mut last_log = std::time::Instant::now();

    // The Swift side delivers NALs on a background dispatch queue. We pull
    // them via a std::sync::mpsc receiver, then push to the WS via tokio.
    // `recv` blocks the current thread, so spawn a blocking task to bridge.
    let (mtx, mut mrx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    std::thread::spawn(move || {
        while let Ok(nal) = rx.recv() {
            if mtx.blocking_send(nal.data).is_err() {
                break;
            }
        }
    });

    while let Some(nal) = mrx.recv().await {
        let n = nal.len() as u64;
        if let Err(e) = sink.send(Message::Binary(nal)).await {
            log::warn!("ws send: {e}");
            break;
        }
        total += n;
        nals += 1;
        if last_log.elapsed().as_secs() >= 2 {
            let secs = last_log.elapsed().as_secs_f64();
            let kbps = ((total as f64 * 8.0) / 1024.0 / secs).round() as u64;
            let nps = (nals as f64 / secs).round() as u64;
            log::info!("{kbps} kbps, {nps} NAL/s, {} bytes total", total);
            total = 0;
            nals = 0;
            last_log = std::time::Instant::now();
        }
    }

    let _ = sink.close().await;
    Ok(())
}

#[cfg(target_os = "macos")]
fn start_native(
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<std::sync::mpsc::Receiver<xyzen_relay_mac_capturer::Nal>> {
    // display_id=0 selects the primary display, matching mac-capturer's
    // convention. The standalone capturer binary is a PoC and doesn't
    // expose a picker — production runners go through xyzen-runner's
    // executor::stream which threads the user-selected display through.
    xyzen_relay_mac_capturer::start(width, height, fps, bitrate_kbps, 0)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(not(target_os = "macos"))]
fn start_native(
    _w: u32,
    _h: u32,
    _fps: u32,
    _b: u32,
) -> Result<std::sync::mpsc::Receiver<DummyNal>> {
    anyhow::bail!("xyzen-capturer: only macOS is supported in v1");
}

#[cfg(not(target_os = "macos"))]
struct DummyNal {
    data: Vec<u8>,
}
