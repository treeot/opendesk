//! Hardware H.264 and HEVC encoding with Apple VideoToolbox.
//!
//! The session is configured for real-time screen sharing: hardware encoder
//! required, low-latency rate control when available, Baseline (H.264) or Main
//! (HEVC) profile, no
//! frame reordering (no B-frames), no frame delay, average bitrate. Each `encode` call submits one frame
//! and waits for it (`VTCompressionSessionCompleteFrames`), so the encoder
//! behaves synchronously like openh264. VideoToolbox emits AVCC samples with
//! SPS/PPS (plus VPS for HEVC) in the format description; they are rewritten to the Annex B
//! contract documented in the parent module.

use std::{
    collections::VecDeque,
    ffi::c_void,
    fmt,
    ptr::{self, NonNull},
    sync::Mutex,
};

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    kCMTimeInvalid, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC, CMFormatDescription,
    CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
};
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA,
    kCVReturnInvalidArgument, CVPixelBuffer, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxFrameDelayCount,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_Baseline_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, VTCompressionSession,
    VTEncodeInfoFlags, VTSessionSetProperty,
};

use super::{append_parameter_sets, avcc_to_annex_b, Codec, EncodedFrame, PixelFormat, RawFrame};

/// Safety-net keyframe interval in seconds, beyond the session loop's own
/// periodic (10 s) and requested keyframes so the two do not coincide.
const MAX_KEYFRAME_INTERVAL_SECS: i32 = 20;

/// A VideoToolbox call failed with an `OSStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VtError {
    pub context: &'static str,
    pub status: i32,
}

impl fmt::Display for VtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed with OSStatus {}", self.context, self.status)
    }
}

impl std::error::Error for VtError {}

fn check(context: &'static str, status: i32) -> Result<(), VtError> {
    if status == 0 {
        Ok(())
    } else {
        Err(VtError { context, status })
    }
}

/// What the output callback produced for one submitted frame.
type CallbackResult = Result<Option<EncodedFrame>, VtError>;

/// Shared between the encoder and the VideoToolbox output callback, which may
/// run on a VideoToolbox thread.
type Outputs = Mutex<VecDeque<CallbackResult>>;

pub struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    /// Handed to VideoToolbox as the callback refcon. Boxed so its address is
    /// stable; it outlives the session because `Drop` invalidates the session
    /// (which guarantees no further callbacks) before the box is freed.
    outputs: Box<Outputs>,
    width: u16,
    height: u16,
    fps: i32,
    frame_index: i64,
    force_keyframe: bool,
    codec: Codec,
}

// SAFETY: VTCompressionSession, its pixel buffer pool and the pixel buffers
// are reference-counted CoreFoundation objects that VideoToolbox documents as
// usable from any thread; the encoder only touches them through `&mut self`
// (never concurrently), and the state shared with the callback is behind a
// Mutex.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    pub fn new(
        width: u16,
        height: u16,
        fps: u32,
        bitrate: u32,
        codec: Codec,
    ) -> Result<Self, VtError> {
        let invalid = |context| VtError {
            context,
            status: -50, /* paramErr */
        };
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(invalid("frame dimensions"));
        }
        let fps = i32::try_from(fps)
            .ok()
            .filter(|&fps| fps > 0)
            .ok_or(invalid("frame rate"))?;
        let bitrate = i32::try_from(bitrate).map_err(|_| invalid("bitrate"))?;

        Self::create(width, height, fps, bitrate, codec, true).or_else(|error| {
            log::debug!("VideoToolbox low-latency mode unavailable ({error}); using default mode");
            Self::create(width, height, fps, bitrate, codec, false)
        })
    }

    fn create(
        width: u16,
        height: u16,
        fps: i32,
        bitrate: i32,
        codec: Codec,
        low_latency: bool,
    ) -> Result<Self, VtError> {
        // SAFETY: these are immutable CFString constants exported by the
        // VideoToolbox/CoreVideo frameworks that this crate links against
        // (EnableLowLatencyRateControl needs macOS 11.3; the host already
        // requires 12.3 for ScreenCaptureKit).
        let (require_hw, low_latency_key, pixel_format_key, width_key, height_key, iosurface_key) = unsafe {
            (
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferWidthKey,
                kCVPixelBufferHeightKey,
                kCVPixelBufferIOSurfacePropertiesKey,
            )
        };
        // Require hardware: fail (and let the caller fall back to openh264)
        // rather than get Apple's software encoder. Low-latency rate control
        // cuts per-frame encode latency several-fold (about 13 ms to 5 ms at
        // 1080p on Apple silicon) and is meant for real-time communication.
        let encoder_spec = CFDictionary::<CFString, CFType>::from_slices(
            &[require_hw, low_latency_key],
            &[CFBoolean::new(true), CFBoolean::new(low_latency)],
        );
        // Accept BGRA as captured; the hardware does the YUV conversion.
        let empty = CFDictionary::<CFString, CFType>::empty();
        let source_attributes = CFDictionary::<CFString, CFType>::from_slices(
            &[pixel_format_key, width_key, height_key, iosurface_key],
            &[
                &CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32),
                &CFNumber::new_i32(i32::from(width)),
                &CFNumber::new_i32(i32::from(height)),
                &empty,
            ],
        );

        let outputs: Box<Outputs> = Box::default();
        let mut raw_session: *mut VTCompressionSession = ptr::null_mut();
        // SAFETY: the dictionaries are valid CF dictionaries of the documented
        // key/value types; `output_callback` matches VTCompressionOutputCallback;
        // the refcon points at `outputs`, which `VtEncoder` keeps alive until
        // after the session is invalidated; `raw_session` is a valid out pointer.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                i32::from(width),
                i32::from(height),
                match codec {
                    Codec::H264 => kCMVideoCodecType_H264,
                    Codec::Hevc => kCMVideoCodecType_HEVC,
                },
                Some(encoder_spec.as_opaque()),
                Some(source_attributes.as_opaque()),
                None,
                Some(output_callback),
                ptr::from_ref::<Outputs>(&outputs).cast_mut().cast(),
                NonNull::from(&mut raw_session),
            )
        };
        check("VTCompressionSessionCreate", status)?;
        let raw_session = NonNull::new(raw_session).ok_or(VtError {
            context: "VTCompressionSessionCreate",
            status: -50,
        })?;
        // SAFETY: VTCompressionSessionCreate follows the CF Create rule, so we
        // own the +1 reference it returned.
        let session = unsafe { CFRetained::from_raw(raw_session) };

        let encoder = Self {
            session,
            outputs,
            width,
            height,
            fps,
            frame_index: 0,
            force_keyframe: true,
            codec,
        };
        encoder.configure(fps, bitrate)?;
        // SAFETY: the session is valid and fully configured.
        check("VTCompressionSessionPrepareToEncodeFrames", unsafe {
            encoder.session.prepare_to_encode_frames()
        })?;
        Ok(encoder)
    }

    fn configure(&self, fps: i32, bitrate: i32) -> Result<(), VtError> {
        // SAFETY: immutable CFString constants exported by VideoToolbox.
        let profile_level = unsafe {
            match self.codec {
                Codec::H264 => kVTProfileLevel_H264_Baseline_AutoLevel,
                Codec::Hevc => kVTProfileLevel_HEVC_Main_AutoLevel,
            }
        };
        // SAFETY: immutable CFString constants exported by VideoToolbox.
        let keys = unsafe {
            (
                kVTCompressionPropertyKey_RealTime,
                kVTCompressionPropertyKey_AllowFrameReordering,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTCompressionPropertyKey_AverageBitRate,
                kVTCompressionPropertyKey_ExpectedFrameRate,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                kVTCompressionPropertyKey_MaxFrameDelayCount,
            )
        };
        let (real_time, reordering, profile, bitrate_key, fps_key, gop_key, delay_key) = keys;
        // Required: these define the bitstream contract and latency.
        self.set(real_time, CFBoolean::new(true))?;
        self.set(reordering, CFBoolean::new(false))?;
        self.set(profile, profile_level)?;
        self.set(bitrate_key, &CFNumber::new_i32(bitrate))?;
        // Best effort: tuning hints some encoders may not support.
        for (key, value) in [
            (fps_key, CFNumber::new_i32(fps)),
            (gop_key, CFNumber::new_i32(fps * MAX_KEYFRAME_INTERVAL_SECS)),
            (delay_key, CFNumber::new_i32(0)),
        ] {
            if let Err(error) = self.set(key, &value) {
                log::debug!("VideoToolbox property {key} not applied: {error}");
            }
        }
        Ok(())
    }

    fn set(&self, key: &CFString, value: &CFType) -> Result<(), VtError> {
        // SAFETY: the session is valid and every key is paired with a value of
        // the type VideoToolbox documents for it.
        let status = unsafe { VTSessionSetProperty(&self.session, key, Some(value)) };
        check("VTSessionSetProperty", status)
    }

    pub fn force_intra_frame(&mut self) {
        self.force_keyframe = true;
    }

    /// Changes the average bitrate of the live session; VideoToolbox
    /// applies it from the next frame.
    pub fn set_bitrate(&mut self, bps: u32) {
        // SAFETY: immutable CFString constant exported by VideoToolbox.
        let key = unsafe { kVTCompressionPropertyKey_AverageBitRate };
        if let Err(error) = self.set(key, &CFNumber::new_i32(i32::try_from(bps).unwrap_or(i32::MAX))) {
            log::debug!("VideoToolbox bitrate change not applied: {error}");
        }
    }

    /// Encodes one frame. `Ok(None)` means the frame was rejected (wrong
    /// size/format) or dropped by the encoder; `Err` means the session is
    /// unusable and the caller should fall back to another encoder.
    pub fn encode(&mut self, frame: &RawFrame) -> Result<Option<EncodedFrame>, VtError> {
        let row_bytes = usize::from(frame.width) * 4;
        if !matches!(frame.format, PixelFormat::Bgra)
            || frame.width != self.width
            || frame.height != self.height
            || frame.data.len() != row_bytes * usize::from(frame.height)
        {
            return Ok(None);
        }

        let pixel_buffer = self.pixel_buffer()?;
        copy_bgra(&pixel_buffer, &frame.data, row_bytes)?;

        // Presentation times come from a frame counter so they stay strictly
        // increasing (capture timestamps are u32 microseconds and wrap).
        let pts = CMTime {
            value: self.frame_index,
            timescale: self.fps,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        self.frame_index += 1;
        // SAFETY: immutable CFString constant exported by VideoToolbox.
        let force_key = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
        let force =
            CFDictionary::<CFString, CFType>::from_slices(&[force_key], &[CFBoolean::new(true)]);
        let frame_properties = self.force_keyframe.then(|| force.as_opaque());
        // SAFETY: the session and pixel buffer are valid; kCMTimeInvalid is an
        // immutable CoreMedia constant; the refcon and info out pointers may be
        // null.
        let status = unsafe {
            self.session.encode_frame(
                &pixel_buffer,
                pts,
                kCMTimeInvalid,
                frame_properties,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        check("VTCompressionSessionEncodeFrame", status)?;
        // Block until this frame's output callback has run, so each call
        // returns its own access unit. Without frame reordering this does not
        // add latency beyond the encode itself.
        // SAFETY: the session is valid; kCMTimeInvalid means "all pending".
        check("VTCompressionSessionCompleteFrames", unsafe {
            self.session.complete_frames(kCMTimeInvalid)
        })?;

        let mut outputs = self
            .outputs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut latest = None;
        while let Some(result) = outputs.pop_front() {
            latest = result?;
        }
        if latest.as_ref().is_some_and(|encoded| encoded.keyframe) {
            self.force_keyframe = false;
        }
        Ok(latest)
    }

    fn pixel_buffer(&self) -> Result<CFRetained<CVPixelBuffer>, VtError> {
        let missing = VtError {
            context: "VTCompressionSessionGetPixelBufferPool",
            status: -12902,
        };
        // SAFETY: the session is valid; the returned pool is retained.
        let pool = unsafe { self.session.pixel_buffer_pool() }.ok_or(missing)?;
        let mut raw: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: the pool is valid and `raw` is a valid out pointer.
        let status =
            unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut raw)) };
        check("CVPixelBufferPoolCreatePixelBuffer", status)?;
        let raw = NonNull::new(raw).ok_or(missing)?;
        // SAFETY: the Create rule hands us a +1 reference.
        Ok(unsafe { CFRetained::from_raw(raw) })
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // SAFETY: the session is valid. Invalidation flushes nothing and
        // guarantees the output callback is not called again, so `outputs`
        // can be freed afterwards.
        unsafe { self.session.invalidate() };
    }
}

/// Copies tightly packed BGRA rows into the (possibly padded) pixel buffer.
fn copy_bgra(buffer: &CVPixelBuffer, data: &[u8], row_bytes: usize) -> Result<(), VtError> {
    let flags = CVPixelBufferLockFlags(0);
    // SAFETY: the buffer is a valid pixel buffer; locking for read/write.
    check("CVPixelBufferLockBaseAddress", unsafe {
        CVPixelBufferLockBaseAddress(buffer, flags)
    })?;
    let base = CVPixelBufferGetBaseAddress(buffer).cast::<u8>();
    let stride = CVPixelBufferGetBytesPerRow(buffer);
    let rows = data.len() / row_bytes;
    let fits = !base.is_null()
        && stride >= row_bytes
        && CVPixelBufferGetWidth(buffer) * 4 >= row_bytes
        && CVPixelBufferGetHeight(buffer) >= rows;
    if fits {
        for (row, src) in data.chunks_exact(row_bytes).enumerate() {
            // SAFETY: the base address is locked and the buffer holds at least
            // `rows` rows of `stride >= row_bytes` bytes (checked above), so
            // the destination range is in bounds and does not overlap `data`.
            unsafe { ptr::copy_nonoverlapping(src.as_ptr(), base.add(row * stride), row_bytes) };
        }
    }
    // SAFETY: balances the successful lock above with the same flags.
    unsafe { CVPixelBufferUnlockBaseAddress(buffer, flags) };
    if fits {
        Ok(())
    } else {
        Err(VtError {
            context: "CVPixelBuffer layout",
            status: kCVReturnInvalidArgument,
        })
    }
}

/// VideoToolbox output callback: converts each encoded sample to Annex B and
/// queues it for `VtEncoder::encode`.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: i32,
    _info: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: `refcon` is the `Outputs` box registered at session creation,
    // which stays alive until after the session is invalidated.
    let outputs = unsafe { &*refcon.cast_const().cast::<Outputs>() };
    let result = if status != 0 {
        Err(VtError {
            context: "VideoToolbox encode callback",
            status,
        })
    } else {
        // SAFETY: on success VideoToolbox passes a valid sample buffer (or
        // null for a dropped frame) that is retained for the callback's
        // duration.
        Ok(unsafe { sample.as_ref() }.and_then(sample_to_annex_b))
    };
    outputs
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push_back(result);
}

/// Converts an encoded AVCC sample to an Annex B access unit, prepending the
/// SPS/PPS from the format description when it contains an IDR slice.
fn sample_to_annex_b(sample: &CMSampleBuffer) -> Option<EncodedFrame> {
    // SAFETY: `sample` is a valid sample buffer; the getters return retained
    // objects.
    let (block, format) = unsafe { (sample.data_buffer()?, sample.format_description()?) };
    // SAFETY: `block` is a valid block buffer.
    let len = unsafe { block.data_length() };
    let mut avcc = vec![0u8; len];
    // SAFETY: `avcc` has room for exactly `len` bytes, the buffer's length;
    // CopyDataBytes handles non-contiguous block buffers.
    let status =
        unsafe { block.copy_data_bytes(0, len, NonNull::from(avcc.as_mut_slice()).cast()) };
    if status != 0 {
        return None;
    }

    let (sets, length_size, codec) = parameter_sets(&format)?;
    let mut body = Vec::with_capacity(len + 16);
    let keyframe = avcc_to_annex_b(&avcc, length_size, codec, &mut body)?;
    if !keyframe {
        return Some(EncodedFrame {
            data: body,
            keyframe,
        });
    }
    let mut data = Vec::with_capacity(body.len() + sets.iter().map(|s| s.len() + 4).sum::<usize>());
    append_parameter_sets(sets.iter().map(Vec::as_slice), &mut data);
    data.extend_from_slice(&body);
    Some(EncodedFrame { data, keyframe })
}

/// Reads one parameter set (or, with null out pointers, just the count and
/// length-prefix size) from a format description with the codec's getter.
///
/// # Safety
/// `format` must be a valid format description of `codec`, and every non-null
/// out pointer must be valid for writes.
unsafe fn parameter_set_at(
    codec: Codec,
    format: &CMFormatDescription,
    index: usize,
    pointer: *mut *const u8,
    size: *mut usize,
    count: *mut usize,
    length_size: *mut i32,
) -> i32 {
    // SAFETY: forwarded from the caller's contract.
    unsafe {
        match codec {
            Codec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format, index, pointer, size, count, length_size,
            ),
            Codec::Hevc => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                format, index, pointer, size, count, length_size,
            ),
        }
    }
}

/// Returns the parameter sets (H.264: SPS, PPS; HEVC: VPS, SPS, PPS) and the
/// NAL length-prefix size from a format description. The codec follows the
/// description's media subtype, so the output callback needs no extra state.
fn parameter_sets(format: &CMFormatDescription) -> Option<(Vec<Vec<u8>>, usize, Codec)> {
    // SAFETY: `format` is a valid format description.
    let subtype = unsafe { format.media_sub_type() };
    let codec = if subtype == kCMVideoCodecType_HEVC {
        Codec::Hevc
    } else {
        Codec::H264
    };
    let mut count = 0usize;
    let mut length_size = 0i32;
    // SAFETY: `format` is a valid format description of `codec`; null pointer
    // and size outputs are allowed, the count/length outputs are valid pointers.
    let status = unsafe {
        parameter_set_at(
            codec,
            format,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut count,
            &mut length_size,
        )
    };
    if status != 0 {
        return None;
    }
    let mut sets = Vec::with_capacity(count);
    for index in 0..count {
        let mut pointer: *const u8 = ptr::null();
        let mut size = 0usize;
        // SAFETY: as above; `pointer`/`size` are valid out pointers.
        let status = unsafe {
            parameter_set_at(
                codec,
                format,
                index,
                &mut pointer,
                &mut size,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status != 0 || pointer.is_null() {
            return None;
        }
        // SAFETY: CoreMedia returned `size` bytes at `pointer`, owned by
        // `format`, which is borrowed (and so retained) for this whole call;
        // the bytes are copied out immediately.
        sets.push(unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec());
    }
    Some((sets, usize::try_from(length_size).ok()?, codec))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use openh264::{decoder::Decoder, formats::YUVSource};

    use super::*;
    use crate::encode::tests::{nal_types, noise_frame, synthetic_frame};

    /// Creates a hardware session, or `None` (test skipped) where VideoToolbox
    /// has no hardware H.264 encoder, e.g. in a VM or on CI.
    fn hardware_encoder(width: u16, height: u16) -> Option<VtEncoder> {
        match VtEncoder::new(width, height, 15, 1_000_000, Codec::H264) {
            Ok(encoder) => Some(encoder),
            Err(error) => {
                eprintln!("skipping VideoToolbox test: {error}");
                None
            }
        }
    }

    fn encode_and_decode(width: u16, height: u16, frames: u32) {
        let Some(mut encoder) = hardware_encoder(width, height) else {
            return;
        };
        let mut decoder = Decoder::new().expect("openh264 decoder");
        let mut decoded = 0u32;
        let mut encode_time = Duration::ZERO;
        let mut bytes = 0;
        for index in 0..frames {
            if index == frames / 2 {
                encoder.force_intra_frame();
            }
            let frame = synthetic_frame(width, height, index);
            let started = Instant::now();
            let output = encoder
                .encode(&frame)
                .expect("VideoToolbox encode")
                .expect("one access unit per frame");
            encode_time += started.elapsed();
            bytes += output.data.len();

            let types = nal_types(&output.data);
            assert_eq!(output.data[..4], [0, 0, 0, 1], "Annex B start code");
            let expect_key = index == 0 || index == frames / 2;
            assert_eq!(output.keyframe, expect_key, "frame {index}: {types:?}");
            if output.keyframe {
                assert_eq!(types[..2], [7, 8], "SPS/PPS lead keyframe: {types:?}");
                assert!(types.contains(&5), "keyframe holds an IDR slice: {types:?}");
                // profile_idc 66 = Baseline, matching the viewer's avc1.42E01E.
                assert_eq!(output.data[5], 66, "Baseline profile");
            } else {
                assert!(
                    !types.iter().any(|t| matches!(t, 5 | 7 | 8)),
                    "delta frame: {types:?}"
                );
            }

            if let Some(yuv) = decoder.decode(&output.data).expect("valid H.264") {
                assert_eq!(yuv.dimensions(), (usize::from(width), usize::from(height)));
                decoded += 1;
            }
        }
        decoded += decoder.flush_remaining().expect("flush").len() as u32;
        assert_eq!(decoded, frames, "every access unit decodes to a frame");
        eprintln!(
            "VideoToolbox {width}x{height}: {:?}/frame, {} bytes/frame",
            encode_time / frames,
            bytes / frames as usize
        );
    }

    #[test]
    fn videotoolbox_stream_decodes_with_openh264() {
        encode_and_decode(640, 360, 30);
    }

    #[test]
    fn videotoolbox_handles_non_macroblock_aligned_sizes() {
        encode_and_decode(1366, 770, 8);
    }

    #[test]
    fn videotoolbox_follows_a_live_bitrate_change() {
        let mut encoder = match VtEncoder::new(320, 240, 30, 4_000_000, Codec::H264) {
            Ok(encoder) => encoder,
            Err(error) => return eprintln!("skipping VideoToolbox test: {error}"),
        };
        let mean = |encoder: &mut VtEncoder, start: u32, frames: u32| {
            let total: usize = (start..start + frames)
                .map(|index| {
                    let frame = noise_frame(320, 240, index);
                    encoder.encode(&frame).expect("VideoToolbox encode").map_or(0, |out| out.data.len())
                })
                .sum();
            total / frames as usize
        };
        let high = mean(&mut encoder, 0, 30);
        encoder.set_bitrate(300_000);
        let _settle = mean(&mut encoder, 30, 15);
        let low = mean(&mut encoder, 45, 30);
        assert!(low * 3 < high, "high {high} B/frame, low {low} B/frame");
    }

    #[test]
    fn videotoolbox_rejects_mismatched_frames() {
        let Some(mut encoder) = hardware_encoder(64, 64) else {
            return;
        };
        let frame = synthetic_frame(32, 32, 0);
        assert_eq!(encoder.encode(&frame).map(|out| out.is_none()), Ok(true));
    }
}
