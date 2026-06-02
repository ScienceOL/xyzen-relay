//! `xyzen-stream`: ultra-thin video stream fan-out.
//!
//! Two endpoints over WebSocket (axum):
//!   * `POST /ws/stream/:peer_id`  — publisher. Pushes Binary frames.
//!   * `GET  /ws/view/:peer_id`    — subscriber(s). Receives the same frames.
//!
//! For PoC v0 there's no auth: anyone with the URL can publish or watch.
//! Auth comes via control-plane session tokens in v1.
//!
//! Storage model: a `RoomMap` keyed by peer_id holds a `RoomState` with
//! a `tokio::sync::broadcast` sender plus a small "warm-up" cache of
//! the most recent keyframe burst. Publishers send into the broadcast;
//! each subscriber gets its own receiver. Viewers also receive the
//! warm-up cache the moment they subscribe so the first paint is
//! instant — without it, a static screen on the publisher (e.g. an
//! idle macOS desktop) would leave the viewer staring at black until
//! the next IDR rolls around (or, on a fully idle SCKit dirty-only
//! capture, forever).
//!
//! When the publisher disconnects, the room is dropped and subscribers
//! see a clean close.

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

type RoomMap = Arc<Mutex<HashMap<String, Arc<RoomState>>>>;

#[derive(Clone)]
struct AppState {
    rooms: RoomMap,
}

/// Per-peer room. Holds the live broadcast sink plus a warm-up window
/// (parameter sets + most recent keyframe + delta frames since that
/// keyframe). New viewers receive the warm-up window before joining
/// the live broadcast so the first paint is instant even when the
/// publisher's screen is currently idle.
struct RoomState {
    /// Broadcast sender — receivers join by `tx.subscribe()` and read
    /// every frame sent after that point. Channel capacity is generous
    /// (256) so a brief stall on one slow viewer doesn't make every
    /// other viewer skip a frame.
    tx: broadcast::Sender<Bytes>,
    /// Warm-up cache: parameter sets (VPS/SPS/PPS) the publisher has
    /// emitted, plus the most recent IDR access unit and every delta
    /// frame after it. On viewer subscribe we replay this in order so
    /// the WebCodecs decoder has a self-contained chunk to start on.
    /// Mutex is plain (not async) — every access is synchronous and
    /// short, holding it across the WS send would be a footgun.
    warmup: std::sync::Mutex<WarmupWindow>,
}

/// What we keep around to bootstrap a fresh viewer:
///   * `param_sets` — the latest sticky NAL bytes for each
///     parameter-set type (VPS/SPS/PPS for HEVC, SPS/PPS for H264).
///     These almost never change for a given session; we take the most
///     recent one we saw and prepend it to every replay so the decoder
///     can configure even if the captured IDR's bundled set differs by
///     a byte.
///   * `keyframe` — the most recent self-contained IDR burst (the
///     keyframe NAL itself plus any parameter-set NALs that arrived
///     adjacent to it, in their original order).
///   * `since_keyframe` — every delta NAL since that keyframe. We
///     replay these so the new viewer's decoder reaches the live
///     broadcast in lockstep.
///
/// Capped sizes prevent a stuck-in-streaming room from growing
/// unbounded. If `since_keyframe` would exceed the cap we drop the
/// stash entirely and wait for the next IDR — better than holding a
/// stale fragment that can't be decoded standalone.
#[derive(Default)]
struct WarmupWindow {
    /// Codec the room is locked to once the first parameter-set NAL
    /// disambiguates it. Stays `None` until we see something
    /// recognisable; after that classification of every subsequent
    /// frame is unambiguous.
    codec: Option<Codec>,
    /// One blob per parameter-set NAL type we've seen. Keyed by raw
    /// NAL type (HEVC 32/33/34 or H264 7/8). Stored independently of
    /// `keyframe.nals` so we still have a usable codec config even if
    /// the publisher emits a parameter-set update between IDRs.
    param_sets: HashMap<u8, Bytes>,
    /// The publisher's last IDR + the parameter-set NALs that arrived
    /// alongside it (typically one frame's worth of bytes).
    keyframe: Vec<Bytes>,
    keyframe_total_bytes: usize,
    /// Every non-IDR / non-parameter-set NAL we've seen since the last
    /// keyframe. Replayed in order on viewer subscribe so the decoder
    /// catches up to live position. Cleared on the next keyframe.
    since_keyframe: Vec<Bytes>,
    since_keyframe_total_bytes: usize,
}

/// Which video codec this room's publisher is using. Locked at the
/// first parameter-set NAL we see and never reconsidered for the
/// lifetime of the room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    Hevc,
    H264,
}

/// Per-room cap on `since_keyframe` byte size. With our 1s GOP this
/// would only realistically grow if a publisher mid-stream stopped
/// emitting IDRs (encoder bug, GOP cadence drift). Cap is generous —
/// 16 MiB lets even a dense 30fps GOP at 16Mbps fit comfortably —
/// but bounded to prevent a faulty encoder from leaking memory.
const WARMUP_DELTA_MAX_BYTES: usize = 16 * 1024 * 1024;
/// Same idea for the keyframe slot; an IDR is normally one frame, so
/// 8 MiB is enormous. Useful only as a pathological-input guard.
const WARMUP_KEY_MAX_BYTES: usize = 8 * 1024 * 1024;

/// First-byte position in a Mac/Win-capturer payload. The capturer
/// prepends an 8-byte big-endian wallclock-µs timestamp ahead of every
/// NAL so the viewer can compute glass-to-glass latency. The Annex-B
/// start code begins at byte 8.
const NAL_TIMESTAMP_PREFIX: usize = 8;

/// Codec & NAL classification just enough for the warm-up cache to
/// recognise parameter sets vs IDR vs delta. Keeping this in-relay
/// (rather than asking the publisher to label frames) means new
/// publishers don't need any wire-level changes — the relay sniffs
/// NAL types directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NalKind {
    /// HEVC VPS / SPS / PPS, or H264 SPS / PPS — sticky bytes the
    /// decoder needs to configure. Carries both the codec it implies
    /// (so callers can lock the room to that codec) and the raw NAL
    /// type (so callers can key the param-set map).
    ParamSet { codec: Codec, nal_type: u8 },
    /// HEVC IDR (any IRAP type) or H264 IDR slice — self-contained
    /// keyframe.
    Keyframe,
    /// Anything else — a P/B slice or other dependent NAL.
    Delta,
    /// Couldn't recognise a start code or read enough bytes to decide.
    /// Treated as Delta for cache purposes.
    Unknown,
}

/// Classify a stamped NAL. When `known_codec` is `Some`, classification
/// is unambiguous because the room has already locked to a codec.
/// When `None`, we sniff for HEVC parameter-set or IRAP byte patterns
/// and fall through to H264; this matches the only ambiguity that
/// matters in practice — the *first* NAL of a session, which is
/// always a parameter set.
fn classify_nal(stamped: &[u8], known_codec: Option<Codec>) -> NalKind {
    if stamped.len() <= NAL_TIMESTAMP_PREFIX {
        return NalKind::Unknown;
    }
    let body = &stamped[NAL_TIMESTAMP_PREFIX..];
    let off = if body.starts_with(&[0, 0, 0, 1]) {
        4
    } else if body.starts_with(&[0, 0, 1]) {
        3
    } else {
        return NalKind::Unknown;
    };
    if body.len() <= off {
        return NalKind::Unknown;
    }
    let first = body[off];
    let h264_type = first & 0x1f;
    let hevc_type = (first >> 1) & 0x3f;

    if let Some(codec) = known_codec {
        return match codec {
            Codec::Hevc => match hevc_type {
                32 | 33 | 34 => NalKind::ParamSet {
                    codec: Codec::Hevc,
                    nal_type: hevc_type,
                },
                16..=23 => NalKind::Keyframe,
                _ => NalKind::Delta,
            },
            Codec::H264 => match h264_type {
                7 | 8 => NalKind::ParamSet {
                    codec: Codec::H264,
                    nal_type: h264_type,
                },
                5 => NalKind::Keyframe,
                _ => NalKind::Delta,
            },
        };
    }

    // Discovery mode: only parameter sets are unambiguous, so we lock
    // the codec on the first one we see and treat everything else as
    // unknown until then. Sniff HEVC first because its parameter-set
    // type ids (32/33/34) would alias to H264 reserved codes (which
    // we wouldn't classify), and only fall through to H264 for its
    // own parameter-set types.
    match hevc_type {
        32 | 33 | 34 => {
            return NalKind::ParamSet {
                codec: Codec::Hevc,
                nal_type: hevc_type,
            }
        }
        _ => {}
    }
    match h264_type {
        7 | 8 => NalKind::ParamSet {
            codec: Codec::H264,
            nal_type: h264_type,
        },
        // Without a confirmed codec we can't tell IDR-5 (H264) from
        // a hypothetical HEVC type-2 slice; treat as unknown so we
        // wait for a parameter set before caching anything.
        _ => NalKind::Unknown,
    }
}

impl WarmupWindow {
    /// Snapshot the current cache as a flat list of frames to replay,
    /// in the order a fresh decoder needs to see them: parameter sets
    /// first, then the keyframe burst, then every delta frame since.
    fn snapshot(&self) -> Vec<Bytes> {
        if self.keyframe.is_empty() {
            // No IDR seen yet — even with parameter sets we can't
            // build a self-contained chunk, so don't replay anything.
            // Viewer will simply wait for the publisher's next IDR.
            return Vec::new();
        }
        let mut out = Vec::with_capacity(
            self.param_sets.len() + self.keyframe.len() + self.since_keyframe.len(),
        );
        // Parameter sets aren't ordered amongst themselves at the
        // codec level (decoder maps by id). Iterate in deterministic
        // order so logs are stable.
        let mut keys: Vec<u8> = self.param_sets.keys().copied().collect();
        keys.sort_unstable();
        for k in keys {
            if let Some(b) = self.param_sets.get(&k) {
                out.push(b.clone());
            }
        }
        out.extend(self.keyframe.iter().cloned());
        out.extend(self.since_keyframe.iter().cloned());
        out
    }

    /// Record an inbound NAL, updating whichever slot it belongs to.
    /// Returns `true` if a fresh keyframe just landed (caller may log).
    fn record(&mut self, frame: Bytes) -> bool {
        match classify_nal(&frame, self.codec) {
            NalKind::ParamSet { codec, nal_type } => {
                if self.codec.is_none() {
                    self.codec = Some(codec);
                }
                self.param_sets.insert(nal_type, frame);
                false
            }
            NalKind::Keyframe => {
                // New keyframe → discard the prior IDR and every
                // delta we accumulated for it. From now on, viewers
                // joining will replay starting from this keyframe.
                self.keyframe.clear();
                self.keyframe_total_bytes = 0;
                self.since_keyframe.clear();
                self.since_keyframe_total_bytes = 0;
                let len = frame.len();
                if len <= WARMUP_KEY_MAX_BYTES {
                    self.keyframe_total_bytes = len;
                    self.keyframe.push(frame);
                }
                true
            }
            NalKind::Delta | NalKind::Unknown => {
                if self.keyframe.is_empty() {
                    // No anchor to attach this delta to — nothing to
                    // do until the next keyframe arrives.
                    return false;
                }
                let projected = self
                    .since_keyframe_total_bytes
                    .saturating_add(frame.len());
                if projected > WARMUP_DELTA_MAX_BYTES {
                    // Overflow guard: drop the cache rather than ship
                    // stale partial state to the next viewer.
                    self.since_keyframe.clear();
                    self.since_keyframe_total_bytes = 0;
                    self.keyframe.clear();
                    self.keyframe_total_bytes = 0;
                    tracing::warn!(
                        "warmup cache exceeded {WARMUP_DELTA_MAX_BYTES} bytes \
                         since last keyframe; dropping until next IDR"
                    );
                    return false;
                }
                self.since_keyframe_total_bytes = projected;
                self.since_keyframe.push(frame);
                false
            }
        }
    }
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

/// Look up (or create) the room state for `peer_id`. The returned
/// `Arc` is shared with the in-map entry so publisher writes and
/// viewer reads see the same warm-up cache.
async fn room_for(rooms: &RoomMap, peer_id: &str) -> Arc<RoomState> {
    let mut map = rooms.lock().await;
    map.entry(peer_id.to_string())
        .or_insert_with(|| {
            Arc::new(RoomState {
                tx: broadcast::channel(256).0,
                warmup: std::sync::Mutex::new(WarmupWindow::default()),
            })
        })
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
    let room = room_for(&rooms, &peer_id).await;

    while let Some(item) = ws.next().await {
        match item? {
            Message::Binary(bytes) => {
                let frame = Bytes::from(bytes);
                // Update the warm-up cache before fan-out so a viewer
                // that subscribes mid-frame either sees the cached
                // version OR the live one — never neither.
                let became_keyframe = match room.warmup.lock() {
                    Ok(mut w) => w.record(frame.clone()),
                    Err(p) => p.into_inner().record(frame.clone()),
                };
                if became_keyframe {
                    tracing::debug!("peer_id={peer_id} cached new keyframe");
                }
                let n_recv = room.tx.receiver_count();
                let _ = room.tx.send(frame);
                tracing::trace!("peer_id={peer_id} fanout={n_recv}");
            }
            Message::Close(_) => break,
            // Ignore text / ping / pong / fragment.
            _ => {}
        }
    }

    // Drop the room if no one is publishing or watching anymore.
    // Crucially, dropping the room also drops the warm-up cache so a
    // future publisher session for the same peer doesn't replay stale
    // frames captured by the prior process.
    let mut map = rooms.lock().await;
    if let Some(s) = map.get(&peer_id) {
        if s.tx.receiver_count() == 0 {
            map.remove(&peer_id);
            tracing::info!("publisher exited, room dropped for peer_id={peer_id}");
        } else {
            tracing::info!(
                "publisher exited, {} viewers still subscribed to peer_id={peer_id}",
                s.tx.receiver_count()
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
    let room = room_for(&rooms, &peer_id).await;
    // Subscribe FIRST, snapshot SECOND. This ordering matters: if we
    // snapshot before subscribing, a new keyframe could land between
    // the two calls and we'd send the cached frame followed by a
    // delta-only stream that doesn't include the matching key — the
    // decoder would error on first decode. Subscribing first means
    // any new frame after the snapshot is still in our channel
    // backlog and will be delivered after the warm-up replay.
    let mut rx = room.tx.subscribe();
    let warmup_frames: Vec<Bytes> = match room.warmup.lock() {
        Ok(w) => w.snapshot(),
        Err(p) => p.into_inner().snapshot(),
    };

    let (mut sink, mut stream) = ws.split();

    // Replay the warm-up cache before joining the live broadcast.
    // The decoder treats this as a self-contained "configure + key
    // frame + deltas to current" sequence, so the first paint lands
    // before the publisher emits its next live frame.
    if !warmup_frames.is_empty() {
        tracing::info!(
            "peer_id={peer_id} replaying {} warmup frames",
            warmup_frames.len()
        );
        for frame in warmup_frames {
            if sink.send(Message::Binary(frame.to_vec())).await.is_err() {
                // Viewer hung up before we finished priming — bail.
                return Ok(());
            }
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Stamp 8 bytes of zero (timestamp prefix the capturer prepends)
    /// in front of a NAL so `classify_nal` sees the same shape it
    /// will see at runtime.
    fn stamp(nal: &[u8]) -> Bytes {
        let mut out = vec![0u8; NAL_TIMESTAMP_PREFIX];
        out.extend_from_slice(nal);
        Bytes::from(out)
    }

    #[test]
    fn classify_recognises_hevc_param_sets_and_idr() {
        // Discovery mode: relay hasn't seen anything yet. First NAL
        // is the VPS (type 32) → 0x40 → locks codec to HEVC.
        let vps = [0u8, 0, 0, 1, 0x40, 0x01];
        assert_eq!(
            classify_nal(&stamp(&vps), None),
            NalKind::ParamSet {
                codec: Codec::Hevc,
                nal_type: 32
            }
        );

        // Steady-state HEVC: subsequent param sets and IDR.
        let sps = [0u8, 0, 0, 1, 0x42, 0x01];
        assert_eq!(
            classify_nal(&stamp(&sps), Some(Codec::Hevc)),
            NalKind::ParamSet {
                codec: Codec::Hevc,
                nal_type: 33
            }
        );
        let pps = [0u8, 0, 0, 1, 0x44, 0x01];
        assert_eq!(
            classify_nal(&stamp(&pps), Some(Codec::Hevc)),
            NalKind::ParamSet {
                codec: Codec::Hevc,
                nal_type: 34
            }
        );
        // HEVC IDR_W_RADL = 19 → 0x26.
        let idr = [0u8, 0, 0, 1, 0x26, 0x01];
        assert_eq!(
            classify_nal(&stamp(&idr), Some(Codec::Hevc)),
            NalKind::Keyframe
        );
        // TRAIL_R (type 1) → 0x02 → delta.
        let p = [0u8, 0, 0, 1, 0x02, 0x01];
        assert_eq!(
            classify_nal(&stamp(&p), Some(Codec::Hevc)),
            NalKind::Delta
        );
    }

    #[test]
    fn classify_recognises_h264() {
        // Discovery mode: H264 SPS = 7 → 0x67.
        let sps = [0u8, 0, 0, 1, 0x67, 0x42];
        assert_eq!(
            classify_nal(&stamp(&sps), None),
            NalKind::ParamSet {
                codec: Codec::H264,
                nal_type: 7
            }
        );

        // Steady-state H264: PPS, IDR, P slice.
        let pps = [0u8, 0, 0, 1, 0x68, 0xCE];
        assert_eq!(
            classify_nal(&stamp(&pps), Some(Codec::H264)),
            NalKind::ParamSet {
                codec: Codec::H264,
                nal_type: 8
            }
        );
        let idr = [0u8, 0, 0, 1, 0x65, 0x88];
        assert_eq!(
            classify_nal(&stamp(&idr), Some(Codec::H264)),
            NalKind::Keyframe
        );
        let p = [0u8, 0, 0, 1, 0x41, 0x9A];
        assert_eq!(
            classify_nal(&stamp(&p), Some(Codec::H264)),
            NalKind::Delta
        );
    }

    #[test]
    fn classify_in_discovery_holds_off_on_non_param_sets() {
        // Without a known codec, an H264 IDR (0x65) reads as HEVC
        // type 50 — not classifiable. We deliberately return Unknown
        // so the warm-up cache waits for an unambiguous parameter
        // set before recording anything.
        let h264_idr = [0u8, 0, 0, 1, 0x65, 0x88];
        assert_eq!(classify_nal(&stamp(&h264_idr), None), NalKind::Unknown);
    }

    #[test]
    fn classify_handles_short_start_code_and_garbage() {
        // 3-byte start code variant — first param-set seen → locks
        // codec to HEVC.
        let short = [0u8, 0, 1, 0x40, 0x01];
        assert_eq!(
            classify_nal(&stamp(&short), None),
            NalKind::ParamSet {
                codec: Codec::Hevc,
                nal_type: 32
            }
        );

        // No start code → unknown.
        let garbage = [0xff, 0xee, 0xdd];
        assert_eq!(classify_nal(&stamp(&garbage), None), NalKind::Unknown);

        // Frame shorter than the timestamp prefix.
        let too_short: [u8; 4] = [0; 4];
        assert_eq!(classify_nal(&too_short, None), NalKind::Unknown);
    }

    #[test]
    fn warmup_snapshot_is_empty_until_first_keyframe() {
        let mut w = WarmupWindow::default();
        let vps = [0u8, 0, 0, 1, 0x40, 0x01];
        let sps = [0u8, 0, 0, 1, 0x42, 0x01];
        let p = [0u8, 0, 0, 1, 0x02, 0x01];
        w.record(stamp(&vps));
        w.record(stamp(&sps));
        w.record(stamp(&p));
        // No keyframe yet — replaying would give the decoder
        // something it can't anchor on, so we don't.
        assert!(w.snapshot().is_empty());
    }

    #[test]
    fn warmup_snapshot_includes_param_sets_keyframe_and_deltas() {
        let mut w = WarmupWindow::default();
        let vps = stamp(&[0u8, 0, 0, 1, 0x40, 0x01]);
        let sps = stamp(&[0u8, 0, 0, 1, 0x42, 0x01]);
        let pps = stamp(&[0u8, 0, 0, 1, 0x44, 0x01]);
        let idr = stamp(&[0u8, 0, 0, 1, 0x26, 0x01, 0xaa]);
        let p1 = stamp(&[0u8, 0, 0, 1, 0x02, 0x01, 0xbb]);
        let p2 = stamp(&[0u8, 0, 0, 1, 0x02, 0x01, 0xcc]);
        w.record(vps.clone());
        w.record(sps.clone());
        w.record(pps.clone());
        assert!(w.record(idr.clone())); // returns true for keyframe
        w.record(p1.clone());
        w.record(p2.clone());

        let snap = w.snapshot();
        // Param sets first (sorted by nal_type: 32, 33, 34), then
        // keyframe, then deltas in order.
        assert_eq!(snap.len(), 6);
        assert_eq!(snap[0], vps);
        assert_eq!(snap[1], sps);
        assert_eq!(snap[2], pps);
        assert_eq!(snap[3], idr);
        assert_eq!(snap[4], p1);
        assert_eq!(snap[5], p2);
    }

    #[test]
    fn warmup_drops_prior_idr_when_new_keyframe_arrives() {
        let mut w = WarmupWindow::default();
        // Lock codec to HEVC by sending a VPS first; otherwise the
        // following IDR-shaped bytes classify as Unknown.
        let vps = stamp(&[0u8, 0, 0, 1, 0x40, 0x01]);
        w.record(vps);
        let idr_a = stamp(&[0u8, 0, 0, 1, 0x26, 0x01, 0xaa]);
        let p_a = stamp(&[0u8, 0, 0, 1, 0x02, 0x01, 0xbb]);
        let idr_b = stamp(&[0u8, 0, 0, 1, 0x26, 0x01, 0xff]);
        w.record(idr_a.clone());
        w.record(p_a.clone());
        w.record(idr_b.clone());
        let snap = w.snapshot();
        // Only the second IDR survives, behind the still-cached VPS.
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[1], idr_b);
    }

    #[test]
    fn warmup_overflow_drops_cache_until_next_keyframe() {
        let mut w = WarmupWindow::default();
        // Lock codec via a VPS first.
        let vps = stamp(&[0u8, 0, 0, 1, 0x40, 0x01]);
        w.record(vps);
        let idr = stamp(&[0u8, 0, 0, 1, 0x26, 0x01]);
        w.record(idr);
        // Push deltas totalling more than the cap. Each delta is
        // 1 MiB so 17 of them blow the 16 MiB ceiling.
        let mut payload = vec![0u8; 8 + 4 + (1 << 20)];
        payload[8..12].copy_from_slice(&[0, 0, 0, 1]);
        payload[12] = 0x02;
        let big = Bytes::from(payload);
        for _ in 0..17 {
            w.record(big.clone());
        }
        // The keyframe slot got cleared on overflow, so the snapshot
        // returns nothing even though param_sets is still populated.
        assert!(w.snapshot().is_empty());
    }
}
