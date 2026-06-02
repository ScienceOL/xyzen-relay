//! MediaFoundation H264 hardware encoder, wrapped with a tight
//! "feed BGRA frame, get NAL units" interface.
//!
//! ## Why hand-roll this when windows-capture has a `VideoEncoder`?
//!
//! windows-capture's bundled encoder writes to a Sink Writer that
//! produces an MP4-container stream. We need raw H264 NAL units so
//! the wire format is byte-identical to what mac-capturer emits.
//! Demuxing fMP4 in the publisher would work but is needless overhead;
//! driving the H264 IMFTransform directly avoids the muxer entirely.
//!
//! ## Pipeline
//!
//! 1. WGC's Direct3D11CaptureFramePool hands us a frame; we ask for
//!    its tightly-packed CPU-side BGRA buffer (via the crate's
//!    `as_nopadding_buffer` helper).
//! 2. `bgra_to_nv12` rewrites the bytes in place into NV12 — the
//!    pixel format the platform H264 encoder always accepts. NV12 is
//!    Y plane (full size) followed by interleaved UV plane (half-width
//!    half-height).
//! 3. Wrap NV12 in an `IMFSample` with the right timestamp + duration.
//!    Push into the encoder's input port.
//! 4. Drain output port — each `IMFSample` is one access unit (frame).
//! 5. The frame's contiguous bytes are AVCC-format: repeated
//!    `<4-byte big-endian length><NAL payload>`. Convert each chunk
//!    in place to Annex-B `<00 00 00 01><NAL payload>` and emit.
//!
//! The Annex-B byte sequence is what the `useH264Stream` viewer hook
//! already parses, so this matches the Mac path exactly.

use std::time::SystemTime;

use windows::core::GUID;
use windows::Win32::Media::MediaFoundation::*;

use crate::Nal;

/// Errors that escape the encoder. Most MediaFoundation HRESULTs go
/// through `Error::Mf` so the caller can log them but keep the publish
/// loop alive (one bad frame shouldn't tear down the WS).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("MediaFoundation HRESULT {0:?}")]
    Mf(#[from] windows::core::Error),
    #[error("encoder not configured")]
    NotConfigured,
    #[error("no H264 encoder MFT found")]
    NoEncoderFound,
    #[error("BGRA buffer wrong size: got {got}, expected {expected}")]
    InputBufferSize { got: usize, expected: usize },
}

/// Owns an `IMFTransform` configured as the platform H264 encoder.
/// One instance per publish session — the runner restarts it when the
/// caller swaps display / fps / resolution.
pub struct H264Encoder {
    transform: Option<IMFTransform>,
    /// Input/output stream identifiers reported by the MFT. Most H264
    /// encoders use 0 for both, but the API lets MFTs expose multiple
    /// streams so we always look them up.
    input_stream_id: u32,
    output_stream_id: u32,
    /// Cached width/height for sample allocation. NV12 requires both
    /// to be even — `configure()` rounds up if needed.
    width: u32,
    height: u32,
    /// Frame interval in 100ns units. Used as the per-sample duration
    /// the encoder bills against its rate-control budget.
    frame_duration_100ns: i64,
    /// PTS of the next sample to emit, in 100ns units since `configure()`.
    /// Monotonic; not derived from wallclock to keep the encoder's
    /// internal pacer happy when the capture pool drops a beat.
    next_pts_100ns: i64,
    /// Wallclock at the moment `configure()` was called, used as the
    /// reference for the per-NAL `pts_us` we emit.
    epoch: SystemTime,
}

// SAFETY: `IMFTransform` is COM and `!Send` by default. The encoder is
// only ever accessed from the single capture thread (created there in
// `ScreenshareHandler::new`, used there in `on_frame_arrived`, dropped
// there when the session ends). The windows-capture trait wraps the
// handler in `Arc<Mutex<...>>` and demands `Send` even though it
// never crosses threads in our usage. Marking the encoder Send is the
// idiomatic escape hatch for thread-affine COM objects.
unsafe impl Send for H264Encoder {}

impl H264Encoder {
    pub fn new() -> Self {
        Self {
            transform: None,
            input_stream_id: 0,
            output_stream_id: 0,
            width: 0,
            height: 0,
            frame_duration_100ns: 0,
            next_pts_100ns: 0,
            epoch: SystemTime::UNIX_EPOCH,
        }
    }

    /// Initialise the IMFTransform and lock its input/output media
    /// types. Must be called before any frames are pushed.
    ///
    /// `bitrate_bps` is the long-term average; the encoder may briefly
    /// exceed it for I-frames. Match Mac's defaults (8 Mbps for 1440p)
    /// at the runner-side configuration layer, not here.
    pub fn configure(
        &mut self,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<(), Error> {
        // NV12 alignment: H264 needs even dimensions, so we round up if
        // the caller hands us a stray odd display size.
        let width = (width + 1) & !1;
        let height = (height + 1) & !1;

        unsafe {
            let transform = find_h264_encoder()?;
            let (input_id, output_id) = lookup_stream_ids(&transform)?;

            // Output type FIRST. Microsoft's H264 encoder requires
            // output be set before input.
            let output_type: IMFMediaType = MFCreateMediaType()?;
            output_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
            output_type.SetUINT32(
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0 as u32,
            )?;
            mf_set_size(&output_type, &MF_MT_FRAME_SIZE, width, height)?;
            mf_set_size(&output_type, &MF_MT_FRAME_RATE, fps, 1)?;
            mf_set_size(&output_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            // High profile = sharper text on screen content. WebCodecs
            // / iOS / Android all decode High fine.
            output_type.SetUINT32(
                &MF_MT_MPEG2_PROFILE,
                eAVEncH264VProfile_High.0 as u32,
            )?;
            transform.SetOutputType(output_id, &output_type, 0)?;

            // Input type: NV12 frames at the same resolution.
            let input_type: IMFMediaType = MFCreateMediaType()?;
            input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input_type.SetUINT32(
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0 as u32,
            )?;
            mf_set_size(&input_type, &MF_MT_FRAME_SIZE, width, height)?;
            mf_set_size(&input_type, &MF_MT_FRAME_RATE, fps, 1)?;
            mf_set_size(&input_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            transform.SetInputType(input_id, &input_type, 0)?;

            // Tell the encoder we're about to start streaming. Without
            // this the first ProcessInput returns MF_E_NOTACCEPTING.
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            self.transform = Some(transform);
            self.input_stream_id = input_id;
            self.output_stream_id = output_id;
            self.width = width;
            self.height = height;
            // Encoder duration is in MF time units (100ns). 1s = 10_000_000.
            self.frame_duration_100ns = 10_000_000 / i64::from(fps.max(1));
            self.next_pts_100ns = 0;
            self.epoch = SystemTime::now();
        }
        Ok(())
    }

    /// Push a BGRA32 frame into the encoder. Internally converts to
    /// NV12 (H264 encoders don't accept 32-bit packed RGB, even when
    /// they advertise multiple input subtypes).
    ///
    /// The buffer must be tightly packed: `width * height * 4` bytes,
    /// row-major, no stride padding.
    pub fn submit_frame(&mut self, bgra: &[u8]) -> Result<(), Error> {
        let transform = self.transform.as_ref().ok_or(Error::NotConfigured)?;
        let expected = (self.width as usize) * (self.height as usize) * 4;
        if bgra.len() < expected {
            return Err(Error::InputBufferSize {
                got: bgra.len(),
                expected,
            });
        }

        // BGRA → NV12. Y plane is `w*h`, UV plane is `w * h/2`.
        let y_size = (self.width as usize) * (self.height as usize);
        let uv_size = y_size / 2;
        let nv12_len = y_size + uv_size;

        unsafe {
            let mf_buffer: IMFMediaBuffer = MFCreateMemoryBuffer(nv12_len as u32)?;
            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut max_len = 0u32;
            let mut cur_len = 0u32;
            mf_buffer.Lock(&mut data_ptr, Some(&mut max_len), Some(&mut cur_len))?;
            let dst = std::slice::from_raw_parts_mut(data_ptr, nv12_len);
            bgra_to_nv12(bgra, self.width as usize, self.height as usize, dst);
            mf_buffer.SetCurrentLength(nv12_len as u32)?;
            mf_buffer.Unlock()?;

            let sample: IMFSample = MFCreateSample()?;
            sample.AddBuffer(&mf_buffer)?;
            sample.SetSampleTime(self.next_pts_100ns)?;
            sample.SetSampleDuration(self.frame_duration_100ns)?;
            self.next_pts_100ns = self
                .next_pts_100ns
                .saturating_add(self.frame_duration_100ns);

            transform.ProcessInput(self.input_stream_id, &sample, 0)?;
        }
        Ok(())
    }

    /// Drain encoded NAL units. Returns an empty Vec when no output is
    /// ready — the caller should `submit_frame` again before retrying.
    pub fn drain(&mut self) -> Result<Vec<Nal>, Error> {
        let transform = self.transform.as_ref().ok_or(Error::NotConfigured)?;
        let mut out_nals: Vec<Nal> = Vec::new();

        loop {
            let stream_info: MFT_OUTPUT_STREAM_INFO =
                unsafe { transform.GetOutputStreamInfo(self.output_stream_id)? };
            // If the encoder doesn't allocate samples we have to.
            let provides = (stream_info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
                != 0;

            // Pre-allocate a sample if the encoder won't.
            let allocated_sample: Option<IMFSample> = if provides {
                None
            } else {
                unsafe {
                    let buf: IMFMediaBuffer =
                        MFCreateMemoryBuffer(stream_info.cbSize.max(1))?;
                    let s: IMFSample = MFCreateSample()?;
                    s.AddBuffer(&buf)?;
                    Some(s)
                }
            };

            // MFT_OUTPUT_DATA_BUFFER's pSample field is `Option<IMFSample>`
            // wrapped in ManuallyDrop in the windows crate. We move the
            // optional sample in, then take it back out after ProcessOutput.
            let mut output_buffer = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_stream_id,
                pSample: std::mem::ManuallyDrop::new(allocated_sample),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };

            let mut status_flags: u32 = 0;
            let result = unsafe {
                transform.ProcessOutput(
                    0,
                    std::slice::from_mut(&mut output_buffer),
                    &mut status_flags,
                )
            };

            // Always reclaim the sample / events from the buffer wrapper
            // so they drop cleanly regardless of HRESULT.
            let produced =
                unsafe { std::mem::ManuallyDrop::take(&mut output_buffer.pSample) };
            let _events =
                unsafe { std::mem::ManuallyDrop::take(&mut output_buffer.pEvents) };

            match result {
                Ok(()) => {
                    if let Some(s) = produced {
                        let nals = sample_to_annexb(&s, self.epoch_now_us())?;
                        out_nals.extend(nals);
                    }
                }
                Err(e) => {
                    if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                        break;
                    }
                    return Err(Error::Mf(e));
                }
            }
        }

        Ok(out_nals)
    }

    fn epoch_now_us(&self) -> i64 {
        SystemTime::now()
            .duration_since(self.epoch)
            .ok()
            .and_then(|d| i64::try_from(d.as_micros()).ok())
            .unwrap_or(0)
    }

    pub fn now_us(&self) -> i64 {
        self.epoch_now_us()
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        if let Some(t) = &self.transform {
            unsafe {
                let _ = t.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = t.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
        }
    }
}

// ─── Helpers ───────────────────────────────────────────────────────

/// Convert one MediaFoundation AVCC blob (4-byte big-endian length
/// + NAL bytes, repeating) into a Vec of Annex-B NALs (each
/// prepended with the 4-byte start code).
pub fn avcc_to_annexb(avcc: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + 4 <= avcc.len() {
        let n = u32::from_be_bytes([
            avcc[off],
            avcc[off + 1],
            avcc[off + 2],
            avcc[off + 3],
        ]) as usize;
        off += 4;
        if off + n > avcc.len() {
            break;
        }
        let mut nal = Vec::with_capacity(4 + n);
        nal.extend_from_slice(&[0, 0, 0, 1]);
        nal.extend_from_slice(&avcc[off..off + n]);
        out.push(nal);
        off += n;
    }
    out
}

/// Pull an `IMFSample`'s contiguous bytes out, run them through
/// `avcc_to_annexb`, and tag each NAL with the supplied `pts_us`.
fn sample_to_annexb(sample: &IMFSample, pts_us: i64) -> Result<Vec<Nal>, Error> {
    unsafe {
        let buffer: IMFMediaBuffer = sample.ConvertToContiguousBuffer()?;
        let mut data_ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len = 0u32;
        let mut cur_len = 0u32;
        buffer.Lock(&mut data_ptr, Some(&mut max_len), Some(&mut cur_len))?;
        let bytes = std::slice::from_raw_parts(data_ptr, cur_len as usize).to_vec();
        buffer.Unlock()?;
        Ok(avcc_to_annexb(&bytes)
            .into_iter()
            .map(|data| Nal { data, pts_us })
            .collect())
    }
}

unsafe fn mf_set_size(
    t: &IMFMediaType,
    key: &GUID,
    hi: u32,
    lo: u32,
) -> windows::core::Result<()> {
    t.SetUINT64(key, ((hi as u64) << 32) | (lo as u64))
}

unsafe fn lookup_stream_ids(t: &IMFTransform) -> Result<(u32, u32), Error> {
    // The H264 encoder MFT uses fixed stream IDs (always 0, 0) but
    // GetStreamIDs() returns E_NOTIMPL for those. Treat that as
    // "they're zero" instead of failing.
    let mut input_ids = [0u32; 1];
    let mut output_ids = [0u32; 1];
    match t.GetStreamIDs(&mut input_ids, &mut output_ids) {
        Ok(()) => Ok((input_ids[0], output_ids[0])),
        Err(e) if e.code() == windows::Win32::Foundation::E_NOTIMPL => Ok((0, 0)),
        Err(e) => Err(Error::Mf(e)),
    }
}

unsafe fn find_h264_encoder() -> Result<IMFTransform, Error> {
    let mut output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = MFT_ENUM_FLAG_HARDWARE
        | MFT_ENUM_FLAG_SORTANDFILTER
        | MFT_ENUM_FLAG_LOCALMFT
        | MFT_ENUM_FLAG_TRANSCODE_ONLY;

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        flags,
        None,
        Some(&mut output_info),
        &mut activates,
        &mut count,
    )?;

    if count == 0 {
        // Fallback: no hardware encoder available — try sync software.
        let flags_sw = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags_sw,
            None,
            Some(&mut output_info),
            &mut activates,
            &mut count,
        )?;
    }

    if count == 0 || activates.is_null() {
        return Err(Error::NoEncoderFound);
    }

    let slice = std::slice::from_raw_parts(activates, count as usize);
    let first = slice[0].clone().ok_or(Error::NoEncoderFound)?;
    windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));

    let transform: IMFTransform = first.ActivateObject::<IMFTransform>()?;
    Ok(transform)
}

/// BGRA8 → NV12 software conversion. Output:
///   - Y plane: width * height bytes
///   - UV plane: width * (height / 2) bytes (interleaved Cb, Cr at
///     2x2 subsampling)
///
/// BT.601 limited-range — same as DirectShow's reference RGB→YUV
/// path. WebCodecs / Chrome assumes BT.601 by default for "video"
/// content, so this matches viewer expectations.
fn bgra_to_nv12(src: &[u8], w: usize, h: usize, dst: &mut [u8]) {
    let y_size = w * h;
    debug_assert!(dst.len() >= y_size + y_size / 2);
    debug_assert!(src.len() >= y_size * 4);

    for y in 0..h {
        let src_row = y * w * 4;
        let dst_row = y * w;
        for x in 0..w {
            let i = src_row + x * 4;
            let b = src[i] as i32;
            let g = src[i + 1] as i32;
            let r = src[i + 2] as i32;
            // Y' = 0.257R + 0.504G + 0.098B + 16, fixed-point.
            let y_val = (66 * r + 129 * g + 25 * b + 128) >> 8;
            dst[dst_row + x] = (y_val + 16).clamp(0, 255) as u8;
        }
    }

    let uv_off = y_size;
    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let mut sum_r = 0i32;
            let mut sum_g = 0i32;
            let mut sum_b = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let i = ((y + dy) * w + (x + dx)) * 4;
                    sum_b += src[i] as i32;
                    sum_g += src[i + 1] as i32;
                    sum_r += src[i + 2] as i32;
                }
            }
            let r = sum_r / 4;
            let g = sum_g / 4;
            let b = sum_b / 4;
            let u_val = (-38 * r - 74 * g + 112 * b + 128) >> 8;
            let v_val = (112 * r - 94 * g - 18 * b + 128) >> 8;
            let dst_idx = uv_off + (y / 2) * w + x;
            dst[dst_idx] = (u_val + 128).clamp(0, 255) as u8;
            dst[dst_idx + 1] = (v_val + 128).clamp(0, 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_two_nals_roundtrip() {
        let avcc: Vec<u8> = [
            0u8, 0, 0, 3, 0xaa, 0xbb, 0xcc, 0, 0, 0, 3, 0xdd, 0xee, 0xff,
        ]
        .to_vec();
        let nals = avcc_to_annexb(&avcc);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], vec![0, 0, 0, 1, 0xaa, 0xbb, 0xcc]);
        assert_eq!(nals[1], vec![0, 0, 0, 1, 0xdd, 0xee, 0xff]);
    }

    #[test]
    fn avcc_truncated_input_drops_tail() {
        let avcc: Vec<u8> = vec![0, 0, 0, 5, 0xaa, 0xbb];
        let nals = avcc_to_annexb(&avcc);
        assert!(nals.is_empty());
    }

    #[test]
    fn bgra_pure_white_yields_max_y() {
        let src = vec![0xffu8; 2 * 2 * 4];
        let mut dst = vec![0u8; 2 * 2 + 2];
        bgra_to_nv12(&src, 2, 2, &mut dst);
        for y in &dst[..4] {
            assert!(*y >= 230 && *y <= 240, "got {y}");
        }
        assert!((dst[4] as i32 - 128).abs() <= 2);
        assert!((dst[5] as i32 - 128).abs() <= 2);
    }

    #[test]
    fn bgra_pure_black_yields_min_y() {
        let src = vec![0u8; 2 * 2 * 4];
        let mut dst = vec![0u8; 2 * 2 + 2];
        bgra_to_nv12(&src, 2, 2, &mut dst);
        for y in &dst[..4] {
            assert!(*y <= 18, "got {y}");
        }
    }
}
