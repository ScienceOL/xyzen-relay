//! macOS-native screen capturer: ScreenCaptureKit + VideoToolbox.
//!
//! Replacement for the ffmpeg subprocess pipeline. Streams raw H264
//! Annex-B NAL units back through a callback, one NAL per call.

#![cfg(target_os = "macos")]

use std::os::raw::{c_int, c_void};
use std::sync::mpsc;

extern "C" {
    fn xz_capturer_start(
        width: i32,
        height: i32,
        fps: i32,
        bitrate_kbps: i32,
        display_id: u32,
        ctx: *mut c_void,
        cb: extern "C" fn(*mut c_void, *const u8, isize, i64),
    ) -> c_int;
    fn xz_capturer_select_display(display_id: u32) -> c_int;
    fn xz_capturer_active_display_id() -> u32;
    fn xz_capturer_list_displays_json(out: *mut u8, cap: isize) -> isize;
    fn xz_capturer_set_bitrate(kbps: i32) -> c_int;
    fn xz_capturer_bitrate_kbps() -> i32;
    fn xz_capturer_set_fps(fps: i32) -> c_int;
    fn xz_capturer_fps() -> i32;
    fn xz_capturer_set_resolution(width: i32, height: i32) -> c_int;
    fn xz_capturer_resolution() -> i32;
}

/// One H264 NAL unit, with the 4-byte Annex-B start code already prepended.
#[derive(Debug, Clone)]
pub struct Nal {
    pub data: Vec<u8>,
    pub pts_us: i64,
}

/// Start the capture pipeline. The returned receiver yields one `Nal` per
/// encoded NAL unit until the process exits. Pass `display_id = 0` for
/// the default display; use `list_displays()` to enumerate.
pub fn start(
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    display_id: u32,
) -> Result<mpsc::Receiver<Nal>, &'static str> {
    let (tx, rx) = mpsc::channel::<Nal>();
    let boxed: Box<mpsc::Sender<Nal>> = Box::new(tx);
    let ctx = Box::into_raw(boxed) as *mut c_void;

    let rc = unsafe {
        xz_capturer_start(
            width as i32,
            height as i32,
            fps as i32,
            bitrate_kbps as i32,
            display_id,
            ctx,
            on_nal,
        )
    };
    if rc != 0 {
        unsafe { drop(Box::from_raw(ctx as *mut mpsc::Sender<Nal>)) };
        return Err("xz_capturer_start failed");
    }
    Ok(rx)
}

/// Switch the running capturer to a different display. Cheap — does not
/// tear down the encoder or WS publisher; just swaps SCKit's content
/// filter. The next encoded keyframe will reflect the new display.
pub fn select_display(display_id: u32) -> Result<(), &'static str> {
    let rc = unsafe { xz_capturer_select_display(display_id) };
    match rc {
        0 => Ok(()),
        1 => Err("capturer not started"),
        _ => Err("select_display failed"),
    }
}

/// Currently active display id, or `0` if no capturer is running.
pub fn active_display_id() -> u32 {
    unsafe { xz_capturer_active_display_id() }
}

/// Set the encoder's average bitrate ceiling (kbps). Cheap — VT applies
/// it on the next frame.
pub fn set_bitrate_kbps(kbps: u32) -> Result<(), &'static str> {
    let rc = unsafe { xz_capturer_set_bitrate(kbps as i32) };
    match rc {
        0 => Ok(()),
        1 => Err("capturer not started"),
        _ => Err("set_bitrate failed"),
    }
}

/// Currently configured bitrate (kbps), or 0 if no capturer running.
pub fn bitrate_kbps() -> u32 {
    let v = unsafe { xz_capturer_bitrate_kbps() };
    if v < 0 {
        0
    } else {
        v as u32
    }
}

/// Set the target framerate. Updates SCKit + VT in one shot.
pub fn set_fps(fps: u32) -> Result<(), &'static str> {
    let rc = unsafe { xz_capturer_set_fps(fps as i32) };
    match rc {
        0 => Ok(()),
        1 => Err("capturer not started"),
        _ => Err("set_fps failed"),
    }
}

pub fn fps() -> u32 {
    let v = unsafe { xz_capturer_fps() };
    if v < 0 {
        0
    } else {
        v as u32
    }
}

/// Switch output resolution. Briefly freezes the viewer (~200ms) while
/// the encoder rebuilds.
pub fn set_resolution(width: u32, height: u32) -> Result<(), &'static str> {
    let rc = unsafe { xz_capturer_set_resolution(width as i32, height as i32) };
    match rc {
        0 => Ok(()),
        1 => Err("capturer not started"),
        _ => Err("set_resolution failed"),
    }
}

/// Returns `(width, height)`, or `(0, 0)` if no capturer is running.
pub fn resolution() -> (u32, u32) {
    let v = unsafe { xz_capturer_resolution() };
    if v <= 0 {
        return (0, 0);
    }
    let w = ((v as u32) >> 16) & 0xFFFF;
    let h = (v as u32) & 0xFFFF;
    (w, h)
}

/// One display visible to the capturer. Mirrors what we get back from
/// `SCShareableContent.displays`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub width: i64,
    pub height: i64,
    pub is_primary: bool,
}

/// Enumerate every display SCShareableContent reports. Returns an empty
/// vec on failure (errors are logged on the Swift side).
pub fn list_displays() -> Vec<DisplayInfo> {
    // 8 KB is far more than enough for typical setups (a 12-monitor wall
    // would be ≈ 1 KB).
    let mut buf = vec![0u8; 8192];
    let n = unsafe { xz_capturer_list_displays_json(buf.as_mut_ptr(), buf.len() as isize) };
    if n < 0 {
        return Vec::new();
    }
    let json = match std::str::from_utf8(&buf[..n as usize]) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str(json).unwrap_or_default()
}

extern "C" fn on_nal(ctx: *mut c_void, data: *const u8, len: isize, pts_us: i64) {
    if ctx.is_null() || data.is_null() || len <= 0 {
        return;
    }
    let slice = unsafe { std::slice::from_raw_parts(data, len as usize) };
    let nal = Nal {
        data: slice.to_vec(),
        pts_us,
    };
    let tx = unsafe { &*(ctx as *const mpsc::Sender<Nal>) };
    let _ = tx.send(nal);
}
