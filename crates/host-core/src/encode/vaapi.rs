//! Hardware H.264 encoding on Linux through VAAPI, using the `h264_vaapi`
//! encoder in libavcodec (ffmpeg).
//!
//! EXPERIMENTAL: this backend is built only when the crate is compiled with
//! `--features vaapi` on Linux, and it needs a working VAAPI driver (Intel
//! Quick Sync, AMD, or a compatible GPU) plus the ffmpeg development
//! libraries. It is not exercised by CI and needs validation on real Linux
//! hardware. If anything here fails at runtime the caller falls back to the
//! openh264 software encoder, so a broken VAAPI setup degrades rather than
//! breaks hosting.
//!
//! Pipeline: the captured BGRA frame is converted to NV12 in software (reusing
//! [`super::nv12`]), uploaded into a VAAPI surface with
//! `av_hwframe_transfer_data`, and encoded. libavcodec's `h264_vaapi` emits an
//! Annex B byte stream with in-band SPS/PPS on keyframes, matching the
//! bitstream contract in the parent module.

use std::ffi::CStr;
use std::ptr;

use ffmpeg_next::ffi;

use super::nv12::{bgra_to_nv12, nv12_len};
use super::{EncodedFrame, RawFrame};

/// Why a VAAPI encoder could not be created or failed while encoding.
#[derive(Debug)]
pub struct VaapiError(String);

impl std::fmt::Display for VaapiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl VaapiError {
    fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }

    /// Format an ffmpeg error code as a message.
    fn code(context: &str, err: i32) -> Self {
        let mut buf = [0i8; ffi::AV_ERROR_MAX_STRING_SIZE as usize];
        // SAFETY: buf is large enough per ffmpeg's own contract.
        let text = unsafe {
            ffi::av_strerror(err, buf.as_mut_ptr(), buf.len());
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        };
        Self(format!("{context}: {text} ({err})"))
    }
}

pub struct VaapiEncoder {
    codec_ctx: *mut ffi::AVCodecContext,
    hw_device: *mut ffi::AVBufferRef,
    hw_frames: *mut ffi::AVBufferRef,
    sw_frame: *mut ffi::AVFrame,
    hw_frame: *mut ffi::AVFrame,
    packet: *mut ffi::AVPacket,
    width: usize,
    height: usize,
    pts: i64,
    force_keyframe: bool,
}

// The raw pointers are owned exclusively by one encoder instance, which is used
// from a single thread (the encode task). None of them are shared.
unsafe impl Send for VaapiEncoder {}

impl VaapiEncoder {
    pub fn new(
        width: u16,
        height: u16,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self, VaapiError> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(VaapiError::new("VAAPI needs non-zero, even dimensions"));
        }
        // SAFETY: a long unsafe block of ffmpeg setup. Each allocation is
        // checked and freed on the error path via `Drop` once `self` is built,
        // or by the local cleanup before `self` exists.
        unsafe {
            let codec = ffi::avcodec_find_encoder_by_name(c"h264_vaapi".as_ptr());
            if codec.is_null() {
                return Err(VaapiError::new("h264_vaapi encoder not available in libavcodec"));
            }

            let mut hw_device: *mut ffi::AVBufferRef = ptr::null_mut();
            let status = ffi::av_hwdevice_ctx_create(
                &mut hw_device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                ptr::null(),
                ptr::null_mut(),
                0,
            );
            if status < 0 {
                return Err(VaapiError::code("av_hwdevice_ctx_create", status));
            }

            // Guards that free what we have so far if a later step fails.
            let mut guard = PartialSetup {
                hw_device,
                hw_frames: ptr::null_mut(),
                codec_ctx: ptr::null_mut(),
                sw_frame: ptr::null_mut(),
                hw_frame: ptr::null_mut(),
                packet: ptr::null_mut(),
            };

            let hw_frames = ffi::av_hwframe_ctx_alloc(hw_device);
            if hw_frames.is_null() {
                return Err(VaapiError::new("av_hwframe_ctx_alloc returned null"));
            }
            guard.hw_frames = hw_frames;
            {
                let frames_ctx = (*hw_frames).data as *mut ffi::AVHWFramesContext;
                (*frames_ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
                (*frames_ctx).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
                (*frames_ctx).width = width as i32;
                (*frames_ctx).height = height as i32;
                (*frames_ctx).initial_pool_size = 4;
            }
            let status = ffi::av_hwframe_ctx_init(hw_frames);
            if status < 0 {
                return Err(VaapiError::code("av_hwframe_ctx_init", status));
            }

            let codec_ctx = ffi::avcodec_alloc_context3(codec);
            if codec_ctx.is_null() {
                return Err(VaapiError::new("avcodec_alloc_context3 returned null"));
            }
            guard.codec_ctx = codec_ctx;
            (*codec_ctx).width = width as i32;
            (*codec_ctx).height = height as i32;
            (*codec_ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*codec_ctx).time_base = ffi::AVRational { num: 1, den: fps.max(1) as i32 };
            (*codec_ctx).framerate = ffi::AVRational { num: fps.max(1) as i32, den: 1 };
            (*codec_ctx).bit_rate = bitrate as i64;
            // Low-latency: one frame out per frame in, no lookahead reordering.
            (*codec_ctx).max_b_frames = 0;
            (*codec_ctx).gop_size = (fps.max(1) * 10) as i32;
            (*codec_ctx).hw_frames_ctx = ffi::av_buffer_ref(hw_frames);
            if (*codec_ctx).hw_frames_ctx.is_null() {
                return Err(VaapiError::new("av_buffer_ref(hw_frames) returned null"));
            }

            let status = ffi::avcodec_open2(codec_ctx, codec, ptr::null_mut());
            if status < 0 {
                return Err(VaapiError::code("avcodec_open2", status));
            }

            let sw_frame = ffi::av_frame_alloc();
            let hw_frame = ffi::av_frame_alloc();
            let packet = ffi::av_packet_alloc();
            if sw_frame.is_null() || hw_frame.is_null() || packet.is_null() {
                return Err(VaapiError::new("could not allocate ffmpeg frames/packet"));
            }
            guard.sw_frame = sw_frame;
            guard.hw_frame = hw_frame;
            guard.packet = packet;

            // Software NV12 frame we fill each time and upload.
            (*sw_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
            (*sw_frame).width = width as i32;
            (*sw_frame).height = height as i32;
            let status = ffi::av_frame_get_buffer(sw_frame, 0);
            if status < 0 {
                return Err(VaapiError::code("av_frame_get_buffer", status));
            }

            // Hardware frame backed by a VAAPI surface from the pool.
            let status = ffi::av_hwframe_get_buffer(hw_frames, hw_frame, 0);
            if status < 0 {
                return Err(VaapiError::code("av_hwframe_get_buffer", status));
            }

            // Take ownership out of the guard into the struct; no further frees
            // happen in this function.
            let encoder = Self {
                codec_ctx: guard.codec_ctx,
                hw_device: guard.hw_device,
                hw_frames: guard.hw_frames,
                sw_frame: guard.sw_frame,
                hw_frame: guard.hw_frame,
                packet: guard.packet,
                width: width as usize,
                height: height as usize,
                pts: 0,
                force_keyframe: true,
            };
            guard.disarm();
            Ok(encoder)
        }
    }

    pub fn force_intra_frame(&mut self) {
        self.force_keyframe = true;
    }

    pub fn set_bitrate(&mut self, bps: u32) {
        // Best effort: many VAAPI drivers only honour this at open time.
        // SAFETY: codec_ctx is valid for the encoder's lifetime.
        unsafe {
            (*self.codec_ctx).bit_rate = bps as i64;
        }
    }

    pub fn encode(&mut self, frame: &RawFrame) -> Result<Option<EncodedFrame>, VaapiError> {
        if frame.width as usize != self.width || frame.height as usize != self.height {
            return Err(VaapiError::new("frame dimensions changed"));
        }
        // SAFETY: all pointers are valid for the encoder's lifetime and the
        // software frame is writable (we keep exclusive ownership).
        unsafe {
            let status = ffi::av_frame_make_writable(self.sw_frame);
            if status < 0 {
                return Err(VaapiError::code("av_frame_make_writable", status));
            }
            self.fill_nv12(frame)?;

            let status = ffi::av_hwframe_transfer_data(self.hw_frame, self.sw_frame, 0);
            if status < 0 {
                return Err(VaapiError::code("av_hwframe_transfer_data", status));
            }

            (*self.hw_frame).pts = self.pts;
            self.pts += 1;
            (*self.hw_frame).pict_type = if self.force_keyframe {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };

            let status = ffi::avcodec_send_frame(self.codec_ctx, self.hw_frame);
            if status < 0 {
                return Err(VaapiError::code("avcodec_send_frame", status));
            }

            // Collect however many packets came out (normally one, since there
            // are no B-frames) into a single Annex B access unit.
            let mut out: Vec<u8> = Vec::new();
            let mut keyframe = false;
            loop {
                let status = ffi::avcodec_receive_packet(self.codec_ctx, self.packet);
                if status == ffi::AVERROR(ffi::EAGAIN) || status == ffi::AVERROR_EOF {
                    break;
                }
                if status < 0 {
                    return Err(VaapiError::code("avcodec_receive_packet", status));
                }
                let data = (*self.packet).data;
                let size = (*self.packet).size as usize;
                if !data.is_null() && size > 0 {
                    out.extend_from_slice(std::slice::from_raw_parts(data, size));
                }
                if (*self.packet).flags & ffi::AV_PKT_FLAG_KEY != 0 {
                    keyframe = true;
                }
                ffi::av_packet_unref(self.packet);
            }

            if keyframe {
                self.force_keyframe = false;
            }
            if out.is_empty() {
                Ok(None)
            } else {
                Ok(Some(EncodedFrame { data: out, keyframe }))
            }
        }
    }

    /// Copy the BGRA frame into the software NV12 frame's planes, honouring the
    /// line sizes ffmpeg chose (which may be padded wider than `width`).
    unsafe fn fill_nv12(&mut self, frame: &RawFrame) -> Result<(), VaapiError> {
        let mut packed = vec![0u8; nv12_len(self.width, self.height)];
        if !bgra_to_nv12(&frame.data, self.width, self.height, &mut packed) {
            return Err(VaapiError::new("BGRA to NV12 conversion failed"));
        }
        let y_stride = (*self.sw_frame).linesize[0] as usize;
        let uv_stride = (*self.sw_frame).linesize[1] as usize;
        let y_dst = (*self.sw_frame).data[0];
        let uv_dst = (*self.sw_frame).data[1];
        if y_dst.is_null() || uv_dst.is_null() {
            return Err(VaapiError::new("NV12 frame has no data planes"));
        }
        // Y plane: height rows of width bytes.
        for row in 0..self.height {
            let src = &packed[row * self.width..row * self.width + self.width];
            ptr::copy_nonoverlapping(src.as_ptr(), y_dst.add(row * y_stride), self.width);
        }
        // UV plane: height/2 rows of width bytes (interleaved).
        let uv_offset = self.width * self.height;
        for row in 0..self.height / 2 {
            let src = &packed[uv_offset + row * self.width..uv_offset + row * self.width + self.width];
            ptr::copy_nonoverlapping(src.as_ptr(), uv_dst.add(row * uv_stride), self.width);
        }
        Ok(())
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        // SAFETY: each pointer was allocated by the matching ffmpeg alloc and
        // is freed once here.
        unsafe {
            ffi::av_packet_free(&mut self.packet);
            ffi::av_frame_free(&mut self.hw_frame);
            ffi::av_frame_free(&mut self.sw_frame);
            ffi::avcodec_free_context(&mut self.codec_ctx);
            ffi::av_buffer_unref(&mut self.hw_frames);
            ffi::av_buffer_unref(&mut self.hw_device);
        }
    }
}

/// Frees partially built ffmpeg state if `new` returns early, then is disarmed
/// once ownership moves into the encoder.
struct PartialSetup {
    hw_device: *mut ffi::AVBufferRef,
    hw_frames: *mut ffi::AVBufferRef,
    codec_ctx: *mut ffi::AVCodecContext,
    sw_frame: *mut ffi::AVFrame,
    hw_frame: *mut ffi::AVFrame,
    packet: *mut ffi::AVPacket,
}

impl PartialSetup {
    fn disarm(mut self) {
        self.hw_device = ptr::null_mut();
        self.hw_frames = ptr::null_mut();
        self.codec_ctx = ptr::null_mut();
        self.sw_frame = ptr::null_mut();
        self.hw_frame = ptr::null_mut();
        self.packet = ptr::null_mut();
    }
}

impl Drop for PartialSetup {
    fn drop(&mut self) {
        // SAFETY: the free functions all accept null and we null each field as
        // it moves into the encoder, so disarmed fields are no-ops.
        unsafe {
            if !self.packet.is_null() {
                ffi::av_packet_free(&mut self.packet);
            }
            if !self.hw_frame.is_null() {
                ffi::av_frame_free(&mut self.hw_frame);
            }
            if !self.sw_frame.is_null() {
                ffi::av_frame_free(&mut self.sw_frame);
            }
            if !self.codec_ctx.is_null() {
                ffi::avcodec_free_context(&mut self.codec_ctx);
            }
            if !self.hw_frames.is_null() {
                ffi::av_buffer_unref(&mut self.hw_frames);
            }
            if !self.hw_device.is_null() {
                ffi::av_buffer_unref(&mut self.hw_device);
            }
        }
    }
}
