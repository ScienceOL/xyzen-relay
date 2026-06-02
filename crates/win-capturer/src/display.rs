//! Display enumeration. Wraps WGC's `Monitor::enumerate` so we expose
//! the same shape `mac-capturer::list_displays` returns: a flat list
//! of `{id, width, height, is_primary}` so the renderer's settings
//! popover can label them and let the user pick.
//!
//! The "id" we hand out is the `HMONITOR`-equivalent `index` from the
//! windows-capture wrapper. It's stable for the OS session — the
//! renderer round-trips it back via `select_display(id)`.

use windows_capture::monitor::Monitor;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub width: i64,
    pub height: i64,
    pub is_primary: bool,
}

/// Enumerate every monitor visible to WGC. Returns an empty Vec on
/// failure (errors logged) so the caller doesn't have to special-case
/// the early-boot path.
pub fn list_displays() -> Vec<DisplayInfo> {
    // The windows-capture crate's `Monitor` doesn't expose
    // `is_primary` directly, but `Monitor::primary()` returns the
    // single primary one. Resolve its device name once and compare
    // against the enumerate list — cheap, no race conditions during
    // the snapshot of monitor topology this function returns.
    let primary_name = Monitor::primary()
        .ok()
        .and_then(|m| m.device_name().ok());

    match Monitor::enumerate() {
        Ok(monitors) => monitors
            .into_iter()
            .enumerate()
            .filter_map(|(idx, m)| {
                let width = m.width().ok()? as i64;
                let height = m.height().ok()? as i64;
                let device_name = m.device_name().ok();
                let is_primary = match (&primary_name, &device_name) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                };
                Some(DisplayInfo {
                    // 1-based id so 0 stays reserved for "default
                    // monitor" in xz_capturer_start, matching the
                    // mac-capturer convention.
                    id: (idx as u32) + 1,
                    width,
                    height,
                    is_primary,
                })
            })
            .collect(),
        Err(e) => {
            log::warn!("[win-capturer] Monitor::enumerate failed: {e}");
            Vec::new()
        }
    }
}
