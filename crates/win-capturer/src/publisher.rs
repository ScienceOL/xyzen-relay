//! Publisher singleton + public API.
//!
//! The shape mirrors `mac-capturer::publisher` so the runner's
//! `executor::stream` can call the same functions on Mac and Windows
//! once the runner imports the right crate per cfg.
//!
//! Threading model
//!   * The capture session runs on a dedicated thread (WGC requires
//!     a single-threaded apartment for COM).
//!   * Encoded NALs hop back via an `mpsc::Sender` the caller owns.
//!   * Tunables (bitrate / fps / resolution / display) live in a
//!     global `Mutex<State>` and are read every frame so live
//!     retunes apply within one frame interval.
//!   * The `H264Encoder` itself is `!Send` (holds COM interfaces) and
//!     never leaves the capture thread — it's instantiated inside
//!     `run_capture_thread` and reborn whenever a tunable change
//!     forces a rebuild.
//!
//! Live-tunable semantics (matches mac-capturer)
//!   * bitrate change   — **deferred**: takes effect on the next
//!     encoder rebuild (next iteration of the outer capture loop).
//!     The platform H264 MFT does not expose a runtime bitrate setter
//!     comparable to VideoToolbox's `kVTCompressionPropertyKey_AverageBitRate`,
//!     and tearing down + re-creating the encoder on every slider tick
//!     would be visible to the viewer. The set_* call flips
//!     `needs_rebuild`; the loop tears the session down on its next
//!     frame and rebuilds with the new value (~200ms glitch).
//!   * fps / resolution / display — same: flip `needs_rebuild`,
//!     loop rebuilds.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::encoder::H264Encoder;
use crate::Nal;

/// Mutable settings the public API exposes. Lives behind a global
/// Mutex so the on_frame_arrived callback can read it cheaply on each
/// frame and notice "user changed something, time to rebuild".
#[derive(Default)]
struct State {
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    display_id: u32,
    /// Cleared when stop() runs so the public getters report 0
    /// rather than stale "last known" values.
    running: bool,
    /// Latched true when the capture thread observes a settings change
    /// it can't apply live (display swap, resolution swap, bitrate
    /// change). The thread drains the flag on its next iteration and
    /// rebuilds the pipeline.
    needs_rebuild: bool,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// Single shutdown signal the capture thread polls. AtomicBool keeps
/// the polling cheap; we only need ordering relative to other writes
/// to `state()`.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Channel the capture thread sends NALs through. Re-created on each
/// `start()` call. We stash it in a OnceLock<Mutex<Option<...>>> so
/// the on_frame_arrived handler — running on the capture thread —
/// can clone it without taking ownership.
static SENDER: OnceLock<Mutex<Option<crate::NalSender>>> = OnceLock::new();

fn sender_slot() -> &'static Mutex<Option<crate::NalSender>> {
    SENDER.get_or_init(|| Mutex::new(None))
}

/// Start the capture pipeline. Spawns a dedicated thread that runs
/// the WGC capture session + drives the H264 encoder. The returned
/// receiver yields `Nal` units until the publisher is stopped or the
/// process exits.
///
/// `display_id == 0` means "primary display"; non-zero is the index
/// returned by `list_displays()` (1-based for parity with mac-capturer).
pub fn start(
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    display_id: u32,
) -> Result<crate::NalReceiver, &'static str> {
    {
        let mut s = state().lock().map_err(|_| "state mutex poisoned")?;
        if s.running {
            return Err("publisher already running");
        }
        s.width = width;
        s.height = height;
        s.fps = fps;
        s.bitrate_kbps = bitrate_kbps;
        s.display_id = display_id;
        s.running = true;
        s.needs_rebuild = false;
    }
    SHUTDOWN.store(false, Ordering::SeqCst);

    let (tx, rx) = std::sync::mpsc::channel::<Nal>();
    *sender_slot().lock().map_err(|_| "sender mutex poisoned")? = Some(tx);

    std::thread::Builder::new()
        .name("xyzen-win-capturer".into())
        .spawn(move || {
            if let Err(e) = run_capture_thread() {
                log::warn!("[win-capturer] capture thread exited: {e:?}");
            }
            SHUTDOWN.store(false, Ordering::SeqCst);
            *sender_slot().lock().expect("sender mutex") = None;
            let mut s = state().lock().expect("state mutex");
            s.running = false;
        })
        .map_err(|_| "thread::spawn failed")?;

    Ok(rx)
}

/// Stop the publisher. Idempotent; safe to call before `start`.
pub fn stop() {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Switch the running publisher to a different monitor.
pub fn select_display(display_id: u32) -> Result<(), &'static str> {
    let mut s = state().lock().map_err(|_| "state mutex poisoned")?;
    if !s.running {
        return Err("capturer not started");
    }
    if s.display_id == display_id {
        return Ok(());
    }
    s.display_id = display_id;
    s.needs_rebuild = true;
    Ok(())
}

pub fn active_display_id() -> u32 {
    state().lock().map(|s| s.display_id).unwrap_or(0)
}

pub fn set_bitrate_kbps(kbps: u32) -> Result<(), &'static str> {
    let mut s = state().lock().map_err(|_| "state mutex poisoned")?;
    if !s.running {
        return Err("capturer not started");
    }
    s.bitrate_kbps = kbps;
    s.needs_rebuild = true;
    Ok(())
}

pub fn bitrate_kbps() -> u32 {
    state().lock().map(|s| s.bitrate_kbps).unwrap_or(0)
}

pub fn set_fps(fps: u32) -> Result<(), &'static str> {
    let mut s = state().lock().map_err(|_| "state mutex poisoned")?;
    if !s.running {
        return Err("capturer not started");
    }
    s.fps = fps;
    s.needs_rebuild = true;
    Ok(())
}

pub fn fps() -> u32 {
    state().lock().map(|s| s.fps).unwrap_or(0)
}

/// Switch output resolution. Will rebuild the encoder + frame pool —
/// expect ~200ms freeze on the viewer side.
pub fn set_resolution(width: u32, height: u32) -> Result<(), &'static str> {
    let mut s = state().lock().map_err(|_| "state mutex poisoned")?;
    if !s.running {
        return Err("capturer not started");
    }
    s.width = width;
    s.height = height;
    s.needs_rebuild = true;
    Ok(())
}

pub fn resolution() -> (u32, u32) {
    state()
        .lock()
        .map(|s| (s.width, s.height))
        .unwrap_or((0, 0))
}

// ─── Capture thread ────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct CaptureSnapshot {
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    display_id: u32,
}

fn run_capture_thread() -> Result<(), Box<dyn std::error::Error>> {
    // COM + MediaFoundation init for this thread. Both are
    // idempotent across multiple iterations of the outer loop, but
    // we only init/teardown once per thread lifetime.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, 0)?;
    }

    // Outer loop drives rebuilds. When the encoder needs to be
    // rebuilt (display swap, resolution change, etc.) the inner
    // capture session exits and we come back up here.
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        let snapshot = {
            let s = state().lock().expect("state mutex");
            CaptureSnapshot {
                width: s.width,
                height: s.height,
                fps: s.fps,
                bitrate_kbps: s.bitrate_kbps,
                display_id: s.display_id,
            }
        };
        // Clear the rebuild flag; subsequent set_* calls during this
        // run will re-set it.
        state().lock().expect("state mutex").needs_rebuild = false;

        // Resolve the requested monitor — fall back to primary on a
        // bad id.
        let monitor = match snapshot.display_id {
            0 => Monitor::primary()?,
            n => Monitor::from_index(n as usize).or_else(|_| Monitor::primary())?,
        };

        // ColorFormat::Bgra8 + windows-capture's nopadding helper
        // gives us tightly-packed BGRA the encoder accepts.
        let settings: Settings<CaptureSnapshot, Monitor> = Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            snapshot,
        );

        // Run the capture session in this thread. The handler's
        // `on_frame_arrived` will be called per frame, on this thread,
        // until `InternalCaptureControl::stop()` is called or the
        // session errors out.
        if let Err(e) = ScreenshareHandler::start(settings) {
            log::warn!("[win-capturer] capture run failed: {e:?}");
            // Brief pause to avoid tight-loop in pathological cases
            // (denied permission, monitor unplugged mid-session).
            std::thread::sleep(std::time::Duration::from_millis(500));
        }

        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
    }

    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    Ok(())
}

/// windows-capture's GraphicsCaptureApiHandler — one method per
/// lifecycle event. The encoder lives here (capture thread only;
/// `IMFTransform` is `!Send` so it can't escape) and is rebuilt
/// every time the outer loop re-enters with new settings.
struct ScreenshareHandler {
    encoder: H264Encoder,
    snapshot: CaptureSnapshot,
    /// Reusable pack buffer for stripping GPU stride padding. Keeps
    /// us off the allocator on every frame.
    pack_buf: Vec<u8>,
}

impl GraphicsCaptureApiHandler for ScreenshareHandler {
    type Flags = CaptureSnapshot;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let snapshot = ctx.flags;
        let mut encoder = H264Encoder::new();
        encoder.configure(
            snapshot.width,
            snapshot.height,
            snapshot.fps,
            snapshot.bitrate_kbps.saturating_mul(1000),
        )?;
        Ok(Self {
            encoder,
            snapshot,
            pack_buf: Vec::new(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame<'_>,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // Cheap exit when shutting down or the user requested a rebuild
        // — both call paths end the session and let the outer loop
        // re-enter with fresh settings.
        if SHUTDOWN.load(Ordering::SeqCst) {
            capture_control.stop();
            return Ok(());
        }
        let needs_rebuild = state().lock().map(|s| s.needs_rebuild).unwrap_or(false);
        if needs_rebuild {
            capture_control.stop();
            return Ok(());
        }

        // Snapshot dimensions BEFORE calling buffer() — the windows-
        // capture API consumes a `&mut self` for buffer access, so we
        // can't call width()/height() after.
        let frame_w = frame.width();
        let frame_h = frame.height();

        // Encoder + frame dimension mismatch → tear down and rebuild.
        // Common after a monitor topology change (DPI scale, rotation).
        if frame_w != self.snapshot.width || frame_h != self.snapshot.height {
            state().lock().expect("state mutex").needs_rebuild = true;
            capture_control.stop();
            return Ok(());
        }

        let buffer = frame.buffer()?;
        // Tightly-packed BGRA, allocator-free across frames.
        let bgra = buffer.as_nopadding_buffer(&mut self.pack_buf);

        if let Err(e) = self.encoder.submit_frame(bgra) {
            log::warn!("[win-capturer] submit_frame: {e:?}");
            return Ok(());
        }
        match self.encoder.drain() {
            Ok(nals) => {
                let sender_guard = sender_slot().lock().ok();
                let sender = sender_guard.as_ref().and_then(|g| g.as_ref()).cloned();
                drop(sender_guard);
                if let Some(tx) = sender {
                    for nal in nals {
                        // tx.send returns Err only when the receiver
                        // was dropped, i.e. the runner-side WS died.
                        // End the session in that case.
                        if tx.send(nal).is_err() {
                            SHUTDOWN.store(true, Ordering::SeqCst);
                            capture_control.stop();
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                log::warn!("[win-capturer] drain: {e:?}");
            }
        }

        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        log::info!("[win-capturer] capture session closed");
        Ok(())
    }
}
