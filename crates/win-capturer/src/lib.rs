//! Windows-native screen capturer: WGC + MediaFoundation H264 encoder.
//!
//! Mirror of `xyzen-relay-mac-capturer`. Same public surface, same
//! wire format on the WebSocket: per-NAL 8-byte big-endian wallclock-µs
//! prefix + raw H264 Annex-B (00 00 00 01 + NAL bytes). Viewer code
//! (`useH264Stream` in the web app) needs zero changes.
//!
//! Capture path
//!   GraphicsCaptureItem  →  Direct3D11CaptureFramePool
//!     →  per frame BGRA texture  →  IMFTransform (H264 hw encoder)
//!         →  IMFSample            →  MFGetService → IMFMediaBuffer
//!             →  bytes (length-prefixed AVCC by default)
//!                 →  Annex-B converter → callback into Rust mpsc
//!
//! AVCC → Annex-B conversion
//!   MediaFoundation's H264 encoder emits each access unit as a
//!   sequence of length-prefixed NALs (4-byte big-endian length, then
//!   the NAL payload). Web/iOS/Android decoders that we ship to all
//!   want Annex-B start codes (00 00 00 01). This crate does the
//!   rewrite *inside* the publisher so the wire format is identical
//!   to what mac-capturer emits.
//!
//! SPS/PPS handling
//!   The MF encoder normally puts SPS+PPS only in the
//!   MF_MT_MPEG_SEQUENCE_HEADER attribute (out-of-band). We turn on
//!   `CODECAPI_AVEncH264SPSID` + `CODECAPI_AVEncMPVDefaultBPictureCount=0`
//!   and force in-band parameter sets via `MFT_OUTPUT_DATA_BUFFER`'s
//!   sequence-attached state, so every keyframe carries SPS+PPS
//!   inline — same as VideoToolbox does on Mac.

#![cfg(target_os = "windows")]

use std::sync::mpsc;

mod encoder;
mod publisher;
mod display;

pub use display::{list_displays, DisplayInfo};
pub use publisher::{
    active_display_id, bitrate_kbps, fps, resolution, select_display, set_bitrate_kbps,
    set_fps, set_resolution, start,
};

/// One H264 NAL unit, with the 4-byte Annex-B start code already prepended.
/// Keep the shape byte-identical to `xyzen-relay-mac-capturer::Nal` so the
/// runner's stream module can use the same channel signature on both
/// platforms with no per-OS plumbing.
#[derive(Debug, Clone)]
pub struct Nal {
    pub data: Vec<u8>,
    pub pts_us: i64,
}

// Re-exported by `start()` in publisher.rs.
pub(crate) type NalSender = mpsc::Sender<Nal>;
pub(crate) type NalReceiver = mpsc::Receiver<Nal>;
