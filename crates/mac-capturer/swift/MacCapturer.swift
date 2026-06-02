// MacCapturer.swift
//
// Native screen capture + H264 encoding pipeline for xyzen-capturer.
//
//   ScreenCaptureKit (macOS 12.3+) → CVPixelBuffer
//     → VTCompressionSession (H264 baseline 4.0, 30fps, 1s GOP)
//     → Annex-B NAL bytes → Rust callback
//
// The pipeline is intentionally synchronous on a single dispatch queue.
// Rust receives a flat byte buffer per NAL unit (with 4-byte start code),
// which is exactly what xyzen-capturer's WebSocket publisher expects.
//
// We expose a small C ABI so build.rs can `swiftc -emit-library`
// and Rust can `extern "C"` it with `#[link]`.

import Foundation
@preconcurrency import ScreenCaptureKit
import VideoToolbox
import CoreMedia
import CoreVideo

// MARK: - C ABI

/// One NAL unit, callback-delivered. The buffer is **not** owned by the
/// callee — copy out before returning.
public typealias XzNalCallback = @convention(c) (
    _ ctx: UnsafeMutableRawPointer?,
    _ data: UnsafePointer<UInt8>,
    _ length: Int,
    _ ptsUs: Int64
) -> Void

/// One-display-per-process singleton. We hold on to the running
/// MacCapturer so `xz_capturer_select_display` can swap the SCStream's
/// content filter without tearing down the encoder + WS publisher.
nonisolated(unsafe) private var sharedCapturer: MacCapturer?

@_cdecl("xz_capturer_start")
public func xz_capturer_start(
    width: Int32,
    height: Int32,
    fps: Int32,
    bitrateKbps: Int32,
    displayId: UInt32,           // 0 = default (first display)
    ctx: UnsafeMutableRawPointer?,
    cb: XzNalCallback
) -> Int32 {
    let s = MacCapturer(
        width: Int(width),
        height: Int(height),
        fps: Int(fps),
        bitrateKbps: Int(bitrateKbps),
        ctx: ctx,
        cb: cb,
        initialDisplayId: displayId == 0 ? nil : CGDirectDisplayID(displayId)
    )
    do {
        try s.start()
    } catch {
        NSLog("xyzen mac-capturer: start failed: \(error)")
        return 1
    }
    sharedCapturer = s
    return 0
}

/// Switch the running capturer to a different display. Returns 0 on
/// success, non-zero if no capturer is running or SCKit refused the
/// new content filter.
@_cdecl("xz_capturer_select_display")
public func xz_capturer_select_display(displayId: UInt32) -> Int32 {
    guard let s = sharedCapturer else { return 1 }
    do {
        try s.selectDisplay(CGDirectDisplayID(displayId))
        return 0
    } catch {
        NSLog("xyzen mac-capturer: select_display failed: \(error)")
        return 2
    }
}

/// Snapshot of the currently active display (set after `start` /
/// after a successful `select_display`).
@_cdecl("xz_capturer_active_display_id")
public func xz_capturer_active_display_id() -> UInt32 {
    return sharedCapturer?.currentDisplayId ?? 0
}

/// Live-update the encoder's target bitrate (kbps). VideoToolbox
/// applies the new ceiling on the very next frame — no encoder
/// rebuild, no GOP boundary needed.
///
/// Returns 0 on success, non-zero if the capturer isn't running.
@_cdecl("xz_capturer_set_bitrate")
public func xz_capturer_set_bitrate(kbps: Int32) -> Int32 {
    guard let s = sharedCapturer, let enc = s.encoderHandle else { return 1 }
    // Clamp to a sane range: 100 kbps floor (anything below that is
    // unusable for screens), 100 Mbps ceiling (above that there's no
    // image-quality benefit and the network/decoder become the bottleneck).
    let clamped = max(100, min(100_000, Int(kbps)))
    let bps = clamped * 1000
    let st = VTSessionSetProperty(
        enc,
        key: kVTCompressionPropertyKey_AverageBitRate,
        value: NSNumber(value: bps)
    )
    if st != noErr { return 2 }
    s.currentBitrateKbps = clamped
    return 0
}

/// Currently configured bitrate (kbps), or `0` if no capturer running.
@_cdecl("xz_capturer_bitrate_kbps")
public func xz_capturer_bitrate_kbps() -> Int32 {
    return Int32(sharedCapturer?.currentBitrateKbps ?? 0)
}

/// Live-update the target frame rate. Updates SCKit's
/// `minimumFrameInterval` and VT's `ExpectedFrameRate` /
/// `MaxKeyFrameInterval` so the GOP cadence stays at 1 second.
/// Returns 0 on success.
@_cdecl("xz_capturer_set_fps")
public func xz_capturer_set_fps(fps: Int32) -> Int32 {
    guard let s = sharedCapturer else { return 1 }
    let clamped = max(5, min(120, Int(fps)))
    do {
        try s.setFps(clamped)
        return 0
    } catch {
        NSLog("xyzen mac-capturer: setFps failed: \(error)")
        return 2
    }
}

/// Currently configured fps, or `0` if no capturer running.
@_cdecl("xz_capturer_fps")
public func xz_capturer_fps() -> Int32 {
    return Int32(sharedCapturer?.currentFps ?? 0)
}

/// Live-update output resolution. Tears down the VTCompressionSession
/// and brings up a new one at the new size; the SCStream stays running.
/// Expect a brief (~100-200ms) freeze on the viewer side as the new
/// SPS+PPS+IDR propagate. Returns 0 on success.
@_cdecl("xz_capturer_set_resolution")
public func xz_capturer_set_resolution(width: Int32, height: Int32) -> Int32 {
    guard let s = sharedCapturer else { return 1 }
    let w = max(160, min(7680, Int(width)))
    let h = max(120, min(4320, Int(height)))
    do {
        try s.setResolution(width: w, height: h)
        return 0
    } catch {
        NSLog("xyzen mac-capturer: setResolution failed: \(error)")
        return 2
    }
}

/// Returns `width << 16 | height` packed into a single Int32 — caller
/// extracts the two halves. Cheap to read; avoids a second FFI call.
@_cdecl("xz_capturer_resolution")
public func xz_capturer_resolution() -> Int32 {
    guard let s = sharedCapturer else { return 0 }
    let w = Int32(min(0xFFFF, s.currentWidth))
    let h = Int32(min(0xFFFF, s.currentHeight))
    return (w << 16) | h
}

/// Fill `out` (utf-8) with a JSON array of available displays:
/// `[{"id":<u32>,"width":<int>,"height":<int>,"is_primary":<bool>}, …]`.
/// Returns the number of bytes written, or `-1` if `out` is too small
/// (caller should retry with `cap` doubled).
@_cdecl("xz_capturer_list_displays_json")
public func xz_capturer_list_displays_json(
    out: UnsafeMutablePointer<UInt8>,
    cap: Int
) -> Int {
    let infos = listDisplaysSync()
    let body = infos.map { d -> String in
        // Trust each value type; `JSONSerialization` would also work but
        // hand-roll keeps this allocation-light.
        let primary = d.isPrimary ? "true" : "false"
        return "{\"id\":\(d.id),\"width\":\(d.width),\"height\":\(d.height),\"is_primary\":\(primary)}"
    }.joined(separator: ",")
    let json = "[\(body)]"
    let bytes = Array(json.utf8)
    if bytes.count > cap { return -1 }
    for (i, b) in bytes.enumerated() { out[i] = b }
    return bytes.count
}

// MARK: - Implementation

@available(macOS 12.3, *)
final class MacCapturer: NSObject, SCStreamDelegate, SCStreamOutput, @unchecked Sendable {
    /// Live capture parameters. All four can change at runtime:
    ///   - bitrate: cheap (VT property, applies next frame)
    ///   - fps: cheap (SCKit minimumFrameInterval + VT ExpectedFrameRate)
    ///   - width/height: expensive (rebuild VTCompressionSession + SCKit
    ///     restart). Done by tearing the encoder down and bringing it
    ///     back up while the SCStream stays running.
    private var width: Int
    private var height: Int
    private var fps: Int
    private let ctx: UnsafeMutableRawPointer?
    private let cb: XzNalCallback
    private var stream: SCStream?
    private var encoder: VTCompressionSession?
    private let queue = DispatchQueue(label: "ai.xyzen.capturer", qos: .userInteractive)
    private var hasEmittedSpsPps = false

    /// Caller's preferred display, or `nil` to pick the first one returned
    /// by SCShareableContent (which is `displays[0]`, typically — but not
    /// always — the primary display).
    private var preferredDisplayId: CGDirectDisplayID?
    /// Display the SCStream is currently filtering on. Reset whenever
    /// `selectDisplay` swaps the content filter.
    var currentDisplayId: CGDirectDisplayID = 0
    /// Read-write so `xz_capturer_set_bitrate` can mutate the live encoder.
    /// Tracked here so callers can `xz_capturer_bitrate_kbps()` it back
    /// without re-poking VT.
    var currentBitrateKbps: Int = 0
    /// Public accessor for the C ABI bridge — VT properties live on the
    /// CompressionSession and the singleton entry points need a way to
    /// reach it.
    var encoderHandle: VTCompressionSession? { encoder }

    init(width: Int, height: Int, fps: Int, bitrateKbps: Int,
         ctx: UnsafeMutableRawPointer?, cb: @escaping XzNalCallback,
         initialDisplayId: CGDirectDisplayID? = nil) {
        self.width = width
        self.height = height
        self.fps = fps
        self.currentBitrateKbps = bitrateKbps
        self.ctx = ctx
        self.cb = cb
        self.preferredDisplayId = initialDisplayId
    }

    var currentWidth: Int { width }
    var currentHeight: Int { height }
    var currentFps: Int { fps }

    func start() throws {
        NSLog("xyzen mac-capturer: start()")
        // Build the encoder before SCKit so we don't drop frames during init.
        try makeEncoder()
        NSLog("xyzen mac-capturer: encoder ready")
        try startCapture()
        NSLog("xyzen mac-capturer: capture session started")
    }

    private func makeEncoder() throws {
        // HEVC, not H264. On 4K / 5K source content H264 is fundamentally
        // bitrate-starved at any reasonable network budget (5K = 14.7 MP
        // per frame; H264 high profile @16 Mbps = 0.06 bit/pixel and the
        // image looks like a YouTube buffer-recovery frame). HEVC delivers
        // ~40-50 % better quality at the same bitrate on screen content
        // because of its larger CTU sizes and better intra prediction —
        // exactly what a stationary text-heavy desktop benefits from.
        //
        // Hardware support: every Apple Silicon Mac has VideoToolbox
        // HEVC encoding. WebCodecs HEVC decoding is on-by-default in
        // Chrome ≥ 124 (macOS) and ≥ 130 (Windows), Safari ≥ 16, Edge.
        // Firefox is the only desktop holdout — we'd add an H264
        // fallback for it later if anyone asks.
        // Force VT to use the platform HEVC encoder when present
        // (Apple Silicon / Intel + T2 / discrete GPU). VT falls back
        // to its software encoder otherwise; both speak the same
        // Annex-B wire format so consumers don't care.
        let encoderSpec: [String: Any] = [
            kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder as String: kCFBooleanTrue,
        ]
        var session: VTCompressionSession?
        let status = VTCompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            width: Int32(width),
            height: Int32(height),
            codecType: kCMVideoCodecType_HEVC,
            encoderSpecification: encoderSpec as CFDictionary,
            imageBufferAttributes: nil,
            compressedDataAllocator: nil,
            outputCallback: nil,
            refcon: nil,
            compressionSessionOut: &session
        )
        guard status == noErr, let session else {
            throw NSError(domain: "MacCapturer", code: Int(status),
                          userInfo: [NSLocalizedDescriptionKey: "VTCompressionSessionCreate(HEVC)"])
        }
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_AllowFrameReordering,
                             value: kCFBooleanFalse)
        // HEVC Main profile + AutoLevel. WebCodecs / Safari decoders
        // accept this exact tier. Main10 would give 10-bit colour for
        // perfect text antialiasing, but no major web decoder will
        // accept Main10 streams without extra config — stick with 8-bit.
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_ProfileLevel,
                             value: kVTProfileLevel_HEVC_Main_AutoLevel)
        // Screen-content hint — same as before; VT spends bits on edge
        // sharpness, not denoising. macOS 14+; older OS silently ignores.
        VTSessionSetProperty(session, key: "ScreenSourceContentSubtype" as CFString,
                             value: kCFBooleanTrue)
        let bps = self.currentBitrateKbps * 1000
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_AverageBitRate,
                             value: NSNumber(value: bps))
        // Tight cap above the average so a brief panic-frame can't blow
        // the network budget. 1.5× over 1 second is the standard recipe.
        let dataLimitBytes = bps / 8 * 3 / 2
        let dataLimit: [Any] = [
            NSNumber(value: dataLimitBytes),
            NSNumber(value: 1.0),
        ]
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_DataRateLimits,
                             value: dataLimit as CFArray)
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_ExpectedFrameRate,
                             value: NSNumber(value: fps))
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_MaxKeyFrameInterval,
                             value: NSNumber(value: fps))                  // 1s GOP
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                             value: NSNumber(value: 1.0))
        VTCompressionSessionPrepareToEncodeFrames(session)
        self.encoder = session
    }

    private func startCapture() throws {
        // Use the older block-based SCShareableContent API so we don't need
        // a MainActor / RunLoop. We're being called from a thread with no
        // run loop, so async/await would never resume.
        let content = try fetchShareableContent()
        guard let display = pickDisplay(from: content) else {
            throw NSError(domain: "MacCapturer", code: -1,
                          userInfo: [NSLocalizedDescriptionKey: "no display"])
        }
        NSLog("xyzen mac-capturer: got display id=%u %dx%d",
              UInt32(display.displayID), display.width, display.height)
        self.currentDisplayId = display.displayID

        let filter = SCContentFilter(display: display, excludingWindows: [])
        let cfg = SCStreamConfiguration()
        cfg.width = self.width
        cfg.height = self.height
        cfg.minimumFrameInterval = CMTime(value: 1, timescale: Int32(self.fps))
        // Smaller queueDepth = lower glass-to-glass latency. SCKit docs
        // say below 3 risks dropped frames under load; 3 is the floor.
        cfg.queueDepth = 3
        cfg.showsCursor = true
        cfg.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange

        let stream = SCStream(filter: filter, configuration: cfg, delegate: self)
        try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: self.queue)

        let startSema = DispatchSemaphore(value: 0)
        var startError: Error?
        stream.startCapture { err in
            startError = err
            startSema.signal()
        }
        if startSema.wait(timeout: .now() + 5) == .timedOut {
            throw NSError(domain: "MacCapturer", code: -3,
                          userInfo: [NSLocalizedDescriptionKey: "SCStream.startCapture timeout"])
        }
        if let e = startError { throw e }
        self.stream = stream
    }

    /// Swap the running stream to a different display. Reuses the
    /// existing encoder + WS publisher; the only thing that changes is
    /// the `SCContentFilter` the SCStream is reading from.
    func selectDisplay(_ id: CGDirectDisplayID) throws {
        guard let stream = self.stream else {
            throw NSError(domain: "MacCapturer", code: -10,
                          userInfo: [NSLocalizedDescriptionKey: "stream not started"])
        }
        if id == self.currentDisplayId { return }
        let content = try fetchShareableContent()
        guard let display = content.displays.first(where: { $0.displayID == id }) else {
            throw NSError(domain: "MacCapturer", code: -11,
                          userInfo: [NSLocalizedDescriptionKey: "display \(id) not available"])
        }
        let filter = SCContentFilter(display: display, excludingWindows: [])

        // No explicit force-keyframe call — when the new display has
        // different dimensions, VideoToolbox emits a fresh SPS+PPS+IDR
        // automatically on its first frame. If dimensions match exactly
        // the viewer briefly shows the old display until the next GOP
        // (≤1s) — acceptable for v1.
        let sema = DispatchSemaphore(value: 0)
        var updateError: Error?
        stream.updateContentFilter(filter) { err in
            updateError = err
            sema.signal()
        }
        if sema.wait(timeout: .now() + 5) == .timedOut {
            throw NSError(domain: "MacCapturer", code: -12,
                          userInfo: [NSLocalizedDescriptionKey: "updateContentFilter timeout"])
        }
        if let e = updateError { throw e }
        self.currentDisplayId = id
        NSLog("xyzen mac-capturer: switched to display id=%u %dx%d",
              UInt32(id), display.width, display.height)
    }

    private func pickDisplay(from content: SCShareableContent) -> SCDisplay? {
        if let id = preferredDisplayId,
           let d = content.displays.first(where: { $0.displayID == id }) {
            return d
        }
        return content.displays.first
    }

    /// Live-update target fps. Cheap: SCKit accepts a new
    /// minimumFrameInterval mid-stream, and VT happily reschedules its
    /// keyframe cadence on the next frame.
    func setFps(_ newFps: Int) throws {
        guard newFps != fps else { return }
        guard let stream = self.stream else {
            throw NSError(domain: "MacCapturer", code: -20,
                          userInfo: [NSLocalizedDescriptionKey: "stream not started"])
        }

        let cfg = SCStreamConfiguration()
        cfg.width = self.width
        cfg.height = self.height
        cfg.minimumFrameInterval = CMTime(value: 1, timescale: Int32(newFps))
        cfg.queueDepth = 3
        cfg.showsCursor = true
        cfg.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange

        let sema = DispatchSemaphore(value: 0)
        var updateError: Error?
        stream.updateConfiguration(cfg) { err in
            updateError = err
            sema.signal()
        }
        if sema.wait(timeout: .now() + 5) == .timedOut {
            throw NSError(domain: "MacCapturer", code: -21,
                          userInfo: [NSLocalizedDescriptionKey: "updateConfiguration timeout"])
        }
        if let e = updateError { throw e }

        if let enc = self.encoder {
            VTSessionSetProperty(enc,
                                 key: kVTCompressionPropertyKey_ExpectedFrameRate,
                                 value: NSNumber(value: newFps))
            VTSessionSetProperty(enc,
                                 key: kVTCompressionPropertyKey_MaxKeyFrameInterval,
                                 value: NSNumber(value: newFps))
        }
        self.fps = newFps
        NSLog("xyzen mac-capturer: fps -> %d", newFps)
    }

    /// Tear down the encoder, rebuild at the new size, and reconfigure
    /// SCKit to deliver matching pixel buffers. The SCStream itself
    /// stays running; only the compression session is recreated.
    /// The viewer will see a brief (≤200ms) freeze while the new
    /// SPS+PPS+IDR get through.
    func setResolution(width newW: Int, height newH: Int) throws {
        if newW == width && newH == height { return }
        guard let stream = self.stream else {
            throw NSError(domain: "MacCapturer", code: -30,
                          userInfo: [NSLocalizedDescriptionKey: "stream not started"])
        }

        // 1) Tell SCKit to deliver buffers at the new size.
        let cfg = SCStreamConfiguration()
        cfg.width = newW
        cfg.height = newH
        cfg.minimumFrameInterval = CMTime(value: 1, timescale: Int32(self.fps))
        cfg.queueDepth = 3
        cfg.showsCursor = true
        cfg.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange

        let sema = DispatchSemaphore(value: 0)
        var updateError: Error?
        stream.updateConfiguration(cfg) { err in
            updateError = err
            sema.signal()
        }
        if sema.wait(timeout: .now() + 5) == .timedOut {
            throw NSError(domain: "MacCapturer", code: -31,
                          userInfo: [NSLocalizedDescriptionKey: "updateConfiguration timeout"])
        }
        if let e = updateError { throw e }

        // 2) Bring the encoder down before swapping width/height so the
        //    `didOutputSampleBuffer` callback (which reads `self.encoder`)
        //    short-circuits during the gap.
        if let old = self.encoder {
            VTCompressionSessionInvalidate(old)
        }
        self.encoder = nil
        self.width = newW
        self.height = newH

        // 3) Build a fresh encoder at the new size with the *current*
        //    bitrate/fps settings preserved.
        try makeEncoder()
        NSLog("xyzen mac-capturer: resolution -> %dx%d", newW, newH)
    }

    // MARK: SCStreamOutput

    nonisolated func stream(_ stream: SCStream, didOutputSampleBuffer sb: CMSampleBuffer,
                            of type: SCStreamOutputType) {
        // SCKit emits idle "no-update" samples without an image buffer when
        // the screen content hasn't changed. We just skip them silently.
        guard type == .screen, sb.isValid,
              let pixel = CMSampleBufferGetImageBuffer(sb) else { return }
        guard let encoder = self.encoder else { return }

        let pts = CMSampleBufferGetPresentationTimeStamp(sb)
        var flags = VTEncodeInfoFlags()
        VTCompressionSessionEncodeFrame(
            encoder,
            imageBuffer: pixel,
            presentationTimeStamp: pts,
            duration: .invalid,
            frameProperties: nil,
            infoFlagsOut: &flags,
            outputHandler: { [weak self] status, _, sample in
                guard status == noErr, let sample, let self else { return }
                self.handleEncoded(sample)
            }
        )
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        NSLog("xyzen mac-capturer: stream stopped: \(error)")
    }

    // MARK: encode → annex-b

    private func handleEncoded(_ sample: CMSampleBuffer) {
        guard CMSampleBufferDataIsReady(sample),
              let block = CMSampleBufferGetDataBuffer(sample) else { return }

        // Extract VPS + SPS + PPS once, prepended to the first IDR. HEVC
        // has three parameter sets (vs H264's two); the format
        // description lists them in order so we just iterate over the
        // count and emit each as its own Annex-B NAL unit. We only
        // attach this prefix to keyframes — late-joiners decode from
        // the next IDR which always carries fresh parameter sets.
        let isKey = isKeyframe(sample)
        var prefix = Data()
        if isKey, let fmt = CMSampleBufferGetFormatDescription(sample) {
            var setCount = 0
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                fmt, parameterSetIndex: 0, parameterSetPointerOut: nil,
                parameterSetSizeOut: nil, parameterSetCountOut: &setCount, nalUnitHeaderLengthOut: nil)
            for i in 0..<setCount {
                var p: UnsafePointer<UInt8>?
                var size = 0
                CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                    fmt, parameterSetIndex: i,
                    parameterSetPointerOut: &p, parameterSetSizeOut: &size,
                    parameterSetCountOut: nil, nalUnitHeaderLengthOut: nil)
                if let p {
                    prefix.append(contentsOf: [0, 0, 0, 1])
                    prefix.append(p, count: size)
                }
            }
        }

        // The compressed buffer is in AVCC format: 4-byte big-endian length
        // prefixes followed by NAL bytes. Split and re-emit each as Annex-B.
        var lengthAtOffset = 0
        var totalLength = 0
        var dataPtr: UnsafeMutablePointer<Int8>?
        let s = CMBlockBufferGetDataPointer(block, atOffset: 0,
                                            lengthAtOffsetOut: &lengthAtOffset,
                                            totalLengthOut: &totalLength,
                                            dataPointerOut: &dataPtr)
        guard s == kCMBlockBufferNoErr, let dataPtr else { return }
        let bytes = UnsafeBufferPointer(start: UnsafeRawPointer(dataPtr).bindMemory(to: UInt8.self,
                                          capacity: totalLength), count: totalLength)
        let pts = CMSampleBufferGetPresentationTimeStamp(sample)
        let ptsUs = Int64(CMTimeGetSeconds(pts) * 1_000_000)

        var offset = 0
        // Emit SPS/PPS prefix as separate NALs first.
        if !prefix.isEmpty {
            emitAnnexBChunk(prefix, ptsUs: ptsUs)
        }
        while offset + 4 <= totalLength {
            let len = (Int(bytes[offset]) << 24) | (Int(bytes[offset+1]) << 16) |
                      (Int(bytes[offset+2]) << 8) | Int(bytes[offset+3])
            offset += 4
            if offset + len > totalLength { break }
            // Build an Annex-B NAL: 4-byte start code + the NAL.
            var out = Data(count: 4 + len)
            out.withUnsafeMutableBytes { (p: UnsafeMutableRawBufferPointer) in
                let dst = p.bindMemory(to: UInt8.self).baseAddress!
                dst[0] = 0; dst[1] = 0; dst[2] = 0; dst[3] = 1
                memcpy(dst + 4, UnsafeRawPointer(bytes.baseAddress!).advanced(by: offset), len)
            }
            emitAnnexBChunk(out, ptsUs: ptsUs)
            offset += len
        }
    }

    private func emitAnnexBChunk(_ data: Data, ptsUs: Int64) {
        // Prepend an 8-byte big-endian wallclock microsecond timestamp so
        // the viewer can compute end-to-end latency. This is OUR header,
        // not part of the H264 stream — viewer must strip the first 8
        // bytes before passing the NAL to its decoder.
        //
        // Wallclock vs PTS: the viewer's clock is independent (different
        // machine), so we ship `now()` not `pts`. PTS-vs-wallclock skew
        // would otherwise corrupt the latency reading.
        let wallUs = Int64(Date().timeIntervalSince1970 * 1_000_000)
        var stamped = Data(count: 8 + data.count)
        stamped.withUnsafeMutableBytes { (raw: UnsafeMutableRawBufferPointer) in
            let dst = raw.bindMemory(to: UInt8.self).baseAddress!
            for i in 0..<8 {
                dst[i] = UInt8((wallUs >> (8 * (7 - i))) & 0xFF)
            }
            data.copyBytes(to: dst.advanced(by: 8), count: data.count)
        }
        stamped.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            self.cb(self.ctx, base, raw.count, ptsUs)
        }
    }

    private func isKeyframe(_ sample: CMSampleBuffer) -> Bool {
        guard let attachments = CMSampleBufferGetSampleAttachmentsArray(
            sample, createIfNecessary: false) as? [[CFString: Any]],
              let first = attachments.first else {
            return false
        }
        if let depends = first[kCMSampleAttachmentKey_DependsOnOthers] as? Bool {
            return !depends
        }
        return false
    }
}

@available(macOS 12.3, *)
private func fetchShareableContent() throws -> SCShareableContent {
    let sema = DispatchSemaphore(value: 0)
    var foundContent: SCShareableContent?
    var contentError: Error?
    SCShareableContent.getWithCompletionHandler { content, err in
        foundContent = content
        contentError = err
        sema.signal()
    }
    if sema.wait(timeout: .now() + 5) == .timedOut {
        throw NSError(domain: "MacCapturer", code: -2,
                      userInfo: [NSLocalizedDescriptionKey: "SCShareableContent timeout"])
    }
    if let e = contentError { throw e }
    guard let content = foundContent else {
        throw NSError(domain: "MacCapturer", code: -3,
                      userInfo: [NSLocalizedDescriptionKey: "no content"])
    }
    return content
}

private struct DisplayInfo {
    let id: UInt32
    let width: Int
    let height: Int
    let isPrimary: Bool
}

@available(macOS 12.3, *)
private func listDisplaysSync() -> [DisplayInfo] {
    do {
        let content = try fetchShareableContent()
        let primaryId = CGMainDisplayID()
        return content.displays.map { d in
            DisplayInfo(
                id: UInt32(d.displayID),
                width: d.width,
                height: d.height,
                isPrimary: d.displayID == primaryId
            )
        }
    } catch {
        NSLog("xyzen mac-capturer: listDisplaysSync failed: \(error)")
        return []
    }
}
