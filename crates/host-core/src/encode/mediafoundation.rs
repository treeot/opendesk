//! Hardware H.264 encoding with Media Foundation (Windows).
//!
//! Media Foundation exposes the GPU vendors' encoders (NVIDIA NVENC, Intel
//! Quick Sync, AMD AMF) as hardware H.264 encoder MFTs. The first one that
//! `MFTEnumEx` returns (sorted by merit) and accepts our configuration is
//! used: low-latency mode, Baseline profile (so no B-frames), CBR at the
//! requested bitrate. Input is BGRA as captured (RGB32) when the MFT takes
//! it, otherwise NV12 converted in software.
//!
//! Hardware MFTs are usually asynchronous: they announce through events when
//! they want input (`METransformNeedInput`) and have output
//! (`METransformHaveOutput`). Each `encode` call submits one frame and waits
//! for its output, so the encoder behaves synchronously like openh264 and
//! VideoToolbox. If the MFT pipelines (asks for the next frame before it
//! outputs this one), its output is returned by a later call instead. Waits
//! time out, so a stuck driver turns into an error (and the openh264
//! fallback) rather than a hung session.
//!
//! Media Foundation emits Annex B; the output is normalized to the contract
//! documented in the parent module (4-byte start codes, SPS and PPS in front
//! of every IDR, taken from `MF_MT_MPEG_SEQUENCE_HEADER` when they are not
//! in-band).
//!
//! COM is initialized (multithreaded apartment) on every thread that calls
//! into the encoder, because the session loop runs on a multi-threaded
//! runtime and the encoder may move between worker threads.

use std::{
    collections::VecDeque,
    fmt,
    mem::ManuallyDrop,
    ptr,
    time::{Duration, Instant},
};

use windows::{
    core::{Interface, GUID, HRESULT},
    Win32::{
        Foundation::{E_FAIL, E_INVALIDARG, E_UNEXPECTED, VARIANT_TRUE},
        Media::MediaFoundation::{
            eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base, eAVEncH265VProfile_Main_420_8,
            CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
            CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
            CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
            IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform, METransformHaveOutput,
            METransformNeedInput, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
            MFMediaType_Video, MFShutdown, MFStartup, MFTEnumEx, MFT_FRIENDLY_NAME_Attribute,
            MFVideoFormat_ARGB32, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoFormat_RGB32,
            MFVideoInterlace_Progressive, MFSTARTUP_LITE, MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
            MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_STREAMING,
            MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
            MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
            MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT, MF_E_NOTACCEPTING,
            MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
            MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE,
            MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
            MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
            MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
        },
        System::{
            Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED},
            Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4},
        },
    },
};

use super::{
    normalize_access_unit,
    nv12::{bgra_to_nv12, nv12_len},
    Codec, EncodedFrame, ParameterSets, PixelFormat, RawFrame,
};

/// Safety-net keyframe interval in seconds, beyond the session loop's own
/// periodic (10 s) and requested keyframes so the two do not coincide.
const MAX_KEYFRAME_INTERVAL_SECS: u32 = 20;
/// How long an asynchronous MFT may take to ask for input or deliver output
/// before it is considered broken (generous: the first frame can include
/// driver warm-up).
const EVENT_TIMEOUT: Duration = Duration::from_secs(2);
/// Event polling period. Rust's `sleep` uses high-resolution timers on
/// Windows, so this adds at most about a millisecond of latency.
const EVENT_POLL_INTERVAL: Duration = Duration::from_micros(250);
/// Media Foundation time unit: 100 ns.
const TICKS_PER_SECOND: i64 = 10_000_000;

/// A Media Foundation call failed with an `HRESULT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MfError {
    pub context: &'static str,
    pub code: i32,
}

impl MfError {
    fn new(context: &'static str, code: HRESULT) -> Self {
        Self {
            context,
            code: code.0,
        }
    }
}

impl fmt::Display for MfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Hex, as HRESULTs are documented.
        write!(
            f,
            "{} failed with HRESULT {:#010X}",
            self.context,
            self.code.cast_unsigned()
        )
    }
}

impl std::error::Error for MfError {}

trait Context<T> {
    fn context(self, context: &'static str) -> Result<T, MfError>;
}

impl<T> Context<T> for windows::core::Result<T> {
    fn context(self, context: &'static str) -> Result<T, MfError> {
        self.map_err(|error| MfError::new(context, error.code()))
    }
}

/// Initializes COM (multithreaded apartment) on the current thread for its
/// lifetime. Media Foundation objects are free-threaded, so they can be used
/// from every thread that is in the MTA.
struct ComApartment {
    initialized: bool,
}

impl ComApartment {
    fn new() -> Self {
        // SAFETY: plain COM initialization with no reserved pointer. Success
        // (S_OK, or S_FALSE if already initialized) is balanced by
        // CoUninitialize in Drop, on this same thread (thread-local).
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // RPC_E_CHANGED_MODE: the thread is already a single-threaded
        // apartment. Free-threaded MF objects still work; nothing to undo.
        Self {
            initialized: hr.is_ok(),
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances the successful CoInitializeEx in `new`, on the
            // same thread (this is a thread-local destructor).
            unsafe { CoUninitialize() };
        }
    }
}

thread_local! {
    static COM: ComApartment = ComApartment::new();
}

/// Makes sure COM is initialized on the calling thread.
fn ensure_com() {
    COM.with(|_| {});
}

/// Keeps Media Foundation started (`MFStartup` is reference counted).
struct MfRuntime;

impl MfRuntime {
    fn start() -> Result<Self, MfError> {
        ensure_com();
        // SAFETY: MF_VERSION is the SDK version this binding targets; each
        // successful start is balanced by MFShutdown in Drop.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.context("MFStartup")?;
        Ok(Self)
    }
}

impl Drop for MfRuntime {
    fn drop(&mut self) {
        ensure_com();
        // SAFETY: balances the successful MFStartup in `start`.
        let _ = unsafe { MFShutdown() };
    }
}

/// Pixel format fed to the MFT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputFormat {
    /// BGRA as captured (`MFVideoFormat_RGB32` / `ARGB32`); the encoder
    /// converts to YUV itself.
    Bgra(GUID),
    /// NV12 converted in software.
    Nv12,
}

/// An event from an asynchronous MFT.
enum Event {
    NeedInput,
    HaveOutput,
    Other,
}

pub struct MfEncoder {
    transform: IMFTransform,
    /// Media Foundation's interface for encoder settings, if exposed.
    codec_api: Option<ICodecAPI>,
    /// Event queue of an asynchronous MFT; `None` for a synchronous one.
    events: Option<IMFMediaEventGenerator>,
    activate: IMFActivate,
    input_id: u32,
    output_id: u32,
    input_format: InputFormat,
    /// Whether the MFT allocates output samples itself.
    provides_samples: bool,
    output_buffer_size: u32,
    codec: Codec,
    width: u16,
    height: u16,
    frame_duration: i64,
    frame_index: i64,
    /// How long to wait for a frame's output when the MFT may instead be
    /// waiting for more input.
    output_grace: Duration,
    force_keyframe: bool,
    /// Whether a keyframe has been emitted; earlier output is dropped.
    started: bool,
    /// `METransformNeedInput` events not yet answered with input.
    need_input: u32,
    /// Encoded access units, oldest first.
    ready: VecDeque<EncodedFrame>,
    parameter_sets: ParameterSets,
    /// Declared last so Media Foundation shuts down after the objects above
    /// are released.
    _runtime: MfRuntime,
}

// SAFETY: Media Foundation objects (MFTs, activation objects, samples, media
// types) are free-threaded COM objects. The encoder only touches them through
// `&mut self` (never concurrently) and initializes COM in the multithreaded
// apartment on every thread that calls into it (`ensure_com`).
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    pub fn new(
        width: u16,
        height: u16,
        fps: u32,
        bitrate: u32,
        codec: Codec,
    ) -> Result<Self, MfError> {
        let invalid = |context| MfError::new(context, E_INVALIDARG);
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(invalid("frame dimensions"));
        }
        if fps == 0 {
            return Err(invalid("frame rate"));
        }

        let _runtime = MfRuntime::start()?;
        let mut last_error = MfError::new(
            match codec {
                Codec::H264 => "MFTEnumEx: no hardware H.264 encoder",
                Codec::Hevc => "MFTEnumEx: no hardware HEVC encoder",
            },
            E_FAIL,
        );
        for activate in hardware_encoders(codec)? {
            let name = friendly_name(&activate);
            match Self::create(activate, width, height, fps, bitrate, codec) {
                Ok(encoder) => {
                    log::info!(
                        "Media Foundation encoder: {name} ({} input, {})",
                        match encoder.input_format {
                            InputFormat::Bgra(_) => "BGRA",
                            InputFormat::Nv12 => "NV12",
                        },
                        if encoder.events.is_some() {
                            "async"
                        } else {
                            "sync"
                        }
                    );
                    return Ok(encoder);
                }
                Err(error) => {
                    log::info!("Media Foundation encoder {name} unusable: {error}");
                    last_error = error;
                }
            }
        }
        Err(last_error)
    }

    fn create(
        activate: IMFActivate,
        width: u16,
        height: u16,
        fps: u32,
        bitrate: u32,
        codec: Codec,
    ) -> Result<Self, MfError> {
        let runtime = MfRuntime::start()?;
        // SAFETY: `activate` is a valid activation object from MFTEnumEx.
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.context("IMFActivate::ActivateObject")?;
        let frame_duration = TICKS_PER_SECOND / i64::from(fps);
        // Built right away so that Drop shuts the MFT down on any error.
        let mut encoder = Self {
            codec_api: None,
            events: None,
            transform,
            activate,
            input_id: 0,
            output_id: 0,
            input_format: InputFormat::Nv12,
            provides_samples: false,
            output_buffer_size: 0,
            codec,
            width,
            height,
            frame_duration,
            frame_index: 0,
            output_grace: Duration::from_millis(1_000 / u64::from(fps))
                .max(Duration::from_millis(20)),
            force_keyframe: true,
            started: false,
            need_input: 0,
            ready: VecDeque::new(),
            parameter_sets: ParameterSets::new(codec),
            _runtime: runtime,
        };

        // SAFETY: the MFT is valid; GetAttributes may be called before the
        // async unlock.
        if let Ok(attributes) = unsafe { encoder.transform.GetAttributes() } {
            // SAFETY: valid attribute store; UINT32 keys with UINT32 values.
            let is_async =
                unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.is_ok_and(|v| v != 0);
            if is_async {
                // SAFETY: as above. An async MFT rejects every other call
                // until it is unlocked.
                unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                    .context("MF_TRANSFORM_ASYNC_UNLOCK")?;
                encoder.events = Some(
                    encoder
                        .transform
                        .cast::<IMFMediaEventGenerator>()
                        .context("IMFMediaEventGenerator")?,
                );
            }
            // SAFETY: as above. Optional hint, same GUID as
            // CODECAPI_AVLowLatencyMode.
            let _ = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) };
        }

        // Queried after the async unlock, like everything else.
        encoder.codec_api = encoder.transform.cast::<ICodecAPI>().ok();

        let (mut inputs, mut outputs) = ([0u32], [0u32]);
        // SAFETY: the MFT is valid and both arrays have room for the one
        // stream an encoder has. E_NOTIMPL means the IDs are 0 and 0.
        if unsafe { encoder.transform.GetStreamIDs(&mut inputs, &mut outputs) }.is_ok() {
            (encoder.input_id, encoder.output_id) = (inputs[0], outputs[0]);
        }

        encoder.configure_codec_api(fps, bitrate);
        encoder.set_output_type(fps, bitrate)?;
        encoder.set_input_type(fps)?;
        encoder.refresh_output_info()?;

        // SAFETY: the MFT is valid and its media types are set.
        unsafe {
            encoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING")?;
            encoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("MFT_MESSAGE_NOTIFY_START_OF_STREAM")?;
        }
        Ok(encoder)
    }

    /// Best-effort encoder settings; not every MFT supports every property.
    fn configure_codec_api(&self, fps: u32, bitrate: u32) {
        let Some(api) = &self.codec_api else {
            log::debug!("Media Foundation encoder has no ICodecAPI; using defaults");
            return;
        };
        let settings = [
            (
                "AVLowLatencyMode",
                CODECAPI_AVLowLatencyMode,
                variant_bool_true(),
            ),
            (
                "AVEncCommonRateControlMode",
                CODECAPI_AVEncCommonRateControlMode,
                variant_u32(eAVEncCommonRateControlMode_CBR.0.cast_unsigned()),
            ),
            (
                "AVEncCommonMeanBitRate",
                CODECAPI_AVEncCommonMeanBitRate,
                variant_u32(bitrate),
            ),
            (
                "AVEncMPVDefaultBPictureCount",
                CODECAPI_AVEncMPVDefaultBPictureCount,
                variant_u32(0),
            ),
            (
                "AVEncMPVGOPSize",
                CODECAPI_AVEncMPVGOPSize,
                variant_u32(fps.saturating_mul(MAX_KEYFRAME_INTERVAL_SECS)),
            ),
        ];
        for (name, key, value) in settings {
            // SAFETY: the API is valid; each value has the VARIANT type the
            // property documents (VT_BOOL / VT_UI4).
            if let Err(error) = unsafe { api.SetValue(&key, &value) } {
                log::debug!("Media Foundation CODECAPI_{name} not applied: {error}");
            }
        }
    }

    fn set_output_type(&self, fps: u32, bitrate: u32) -> Result<(), MfError> {
        let (subtype, profile, context) = match self.codec {
            Codec::H264 => (
                MFVideoFormat_H264,
                eAVEncH264VProfile_Base.0.cast_unsigned(),
                "H.264 output media type",
            ),
            // HEVC takes its profile through the same MF_MT_MPEG2_PROFILE key.
            Codec::Hevc => (
                MFVideoFormat_HEVC,
                eAVEncH265VProfile_Main_420_8.0.cast_unsigned(),
                "HEVC output media type",
            ),
        };
        // SAFETY: creates an empty media type.
        let media_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType")?;
        // SAFETY: valid media type; every key gets the value type Media
        // Foundation documents for it (GUID or UINT32).
        attempt(|| unsafe {
            media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            media_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
            media_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)
        })
        .and_then(|()| self.set_video_attributes(&media_type, fps))
        .context(context)?;
        // SAFETY: the MFT and the media type are valid. Encoders need the
        // output type before the input type.
        unsafe { self.transform.SetOutputType(self.output_id, &media_type, 0) }
            .context("IMFTransform::SetOutputType")
    }

    /// Picks BGRA input if the MFT takes it, else NV12.
    fn set_input_type(&mut self, fps: u32) -> Result<(), MfError> {
        let mut last_error = None;
        for format in [
            InputFormat::Bgra(MFVideoFormat_RGB32),
            InputFormat::Bgra(MFVideoFormat_ARGB32),
            InputFormat::Nv12,
        ] {
            let (subtype, stride) = match format {
                InputFormat::Bgra(subtype) => (subtype, u32::from(self.width) * 4),
                InputFormat::Nv12 => (MFVideoFormat_NV12, u32::from(self.width)),
            };
            // SAFETY: creates an empty media type, then sets documented keys
            // with their documented value types on it. A positive default
            // stride means top-down rows, as captured.
            let result = unsafe {
                MFCreateMediaType().and_then(|media_type| {
                    media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                    media_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
                    media_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, stride)?;
                    self.set_video_attributes(&media_type, fps)?;
                    self.transform.SetInputType(self.input_id, &media_type, 0)
                })
            };
            match result {
                Ok(()) => {
                    self.input_format = format;
                    return Ok(());
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.map_or(
            MfError::new("IMFTransform::SetInputType", E_FAIL),
            |error| {
                MfError::new(
                    "IMFTransform::SetInputType (RGB32, ARGB32, NV12)",
                    error.code(),
                )
            },
        ))
    }

    /// Frame size, frame rate, square pixels, progressive.
    fn set_video_attributes(
        &self,
        media_type: &IMFMediaType,
        fps: u32,
    ) -> windows::core::Result<()> {
        // SAFETY: valid media type; these keys take packed UINT64 (hi, lo)
        // sizes/ratios and a UINT32 enum.
        unsafe {
            media_type.SetUINT64(
                &MF_MT_FRAME_SIZE,
                pack(u32::from(self.width), u32::from(self.height)),
            )?;
            media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            media_type.SetUINT32(
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0.cast_unsigned(),
            )
        }
    }

    /// Reads the output buffer requirements and the sequence header of the
    /// current output type.
    fn refresh_output_info(&mut self) -> Result<(), MfError> {
        // SAFETY: the MFT is valid and its output type is set.
        let info = unsafe { self.transform.GetOutputStreamInfo(self.output_id) }
            .context("IMFTransform::GetOutputStreamInfo")?;
        let provides = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
            | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
            .cast_unsigned();
        self.provides_samples = info.dwFlags & provides != 0;
        // An encoded frame is far smaller than the raw NV12 frame.
        let raw_size = nv12_len(usize::from(self.width), usize::from(self.height));
        self.output_buffer_size = info.cbSize.max(u32::try_from(raw_size).unwrap_or(u32::MAX));

        // SAFETY: the MFT is valid; the blob is copied into a buffer of the
        // size Media Foundation reported for it.
        let header = unsafe {
            self.transform
                .GetOutputCurrentType(self.output_id)
                .and_then(|media_type| {
                    let size = media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER)?;
                    let mut blob = vec![0u8; size as usize];
                    let mut written = 0u32;
                    media_type.GetBlob(
                        &MF_MT_MPEG_SEQUENCE_HEADER,
                        &mut blob,
                        Some(ptr::from_mut(&mut written)),
                    )?;
                    blob.truncate(written as usize);
                    Ok(blob)
                })
        };
        // Usually only available once streaming starts; in-band parameter
        // sets are picked up from the output either way.
        if let Ok(header) = header {
            self.parameter_sets.update_from_annex_b(&header);
        }
        Ok(())
    }

    pub fn force_intra_frame(&mut self) {
        self.force_keyframe = true;
    }

    /// Encodes one frame. `Ok(None)` means the frame was rejected (wrong
    /// size/format) or produced no output yet; `Err` means the encoder is
    /// unusable and the caller should fall back to another encoder.
    pub fn encode(&mut self, frame: &RawFrame) -> Result<Option<EncodedFrame>, MfError> {
        let (width, height) = (usize::from(frame.width), usize::from(frame.height));
        if !matches!(frame.format, PixelFormat::Bgra)
            || frame.width != self.width
            || frame.height != self.height
            || frame.data.len() != width * height * 4
        {
            return Ok(None);
        }
        ensure_com();

        let sample = self.input_sample(frame)?;
        if self.force_keyframe {
            self.request_keyframe();
        }
        match self.events.clone() {
            Some(events) => self.encode_async(&events, &sample)?,
            None => self.encode_sync(&sample)?,
        }
        Ok(self.ready.pop_front())
    }

    /// Changes the mean bitrate of the running encoder. Hardware MFTs that
    /// support dynamic rate control apply it from the next frame; others
    /// keep their bitrate (and the change is only logged).
    pub fn set_bitrate(&mut self, bitrate: u32) {
        let Some(api) = &self.codec_api else {
            return;
        };
        // SAFETY: the API is valid; the property takes a VT_UI4.
        if let Err(error) =
            unsafe { api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate)) }
        {
            log::debug!("Media Foundation bitrate change not applied: {error}");
        }
    }

    fn request_keyframe(&self) {
        let Some(api) = &self.codec_api else {
            return;
        };
        // SAFETY: the API is valid; the property takes a VT_UI4 (non-zero
        // forces the next frame to be a keyframe).
        if let Err(error) =
            unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1)) }
        {
            log::debug!("Media Foundation keyframe request failed: {error}");
        }
    }

    /// Wraps the frame in a Media Foundation sample in the negotiated format.
    fn input_sample(&mut self, frame: &RawFrame) -> Result<IMFSample, MfError> {
        let (width, height) = (usize::from(frame.width), usize::from(frame.height));
        let size = match self.input_format {
            InputFormat::Bgra(_) => frame.data.len(),
            InputFormat::Nv12 => nv12_len(width, height),
        };
        let size_u32 = u32::try_from(size).map_err(|_| MfError::new("frame size", E_INVALIDARG))?;
        // SAFETY: allocates a system-memory buffer of `size` bytes.
        let buffer = unsafe { MFCreateMemoryBuffer(size_u32) }.context("MFCreateMemoryBuffer")?;
        let mut data: *mut u8 = ptr::null_mut();
        let mut max_len = 0u32;
        // SAFETY: valid buffer; `data` and `max_len` are valid out pointers.
        unsafe { buffer.Lock(&mut data, Some(ptr::from_mut(&mut max_len)), None) }
            .context("IMFMediaBuffer::Lock")?;
        let written = !data.is_null() && max_len as usize >= size && {
            // SAFETY: the buffer is locked and holds at least `size` writable
            // bytes at `data` (checked above), which nothing else accesses
            // until Unlock.
            let dst = unsafe { std::slice::from_raw_parts_mut(data, size) };
            match self.input_format {
                InputFormat::Bgra(_) => {
                    dst.copy_from_slice(&frame.data);
                    true
                }
                InputFormat::Nv12 => bgra_to_nv12(&frame.data, width, height, dst),
            }
        };
        // SAFETY: balances the successful Lock above.
        unsafe { buffer.Unlock() }.context("IMFMediaBuffer::Unlock")?;
        if !written {
            return Err(MfError::new("input buffer layout", E_UNEXPECTED));
        }

        // SAFETY: valid buffer and a new, valid sample. Sample times come
        // from a frame counter so they stay strictly increasing (capture
        // timestamps are u32 microseconds and wrap).
        let sample = attempt(|| unsafe {
            buffer.SetCurrentLength(size_u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.frame_index * self.frame_duration)?;
            sample.SetSampleDuration(self.frame_duration)?;
            Ok(sample)
        })
        .context("input sample")?;
        self.frame_index += 1;
        Ok(sample)
    }

    /// Synchronous MFT: feed the input, then drain all available output.
    fn encode_sync(&mut self, sample: &IMFSample) -> Result<(), MfError> {
        loop {
            // SAFETY: the MFT and sample are valid; streaming has begun.
            match unsafe { self.transform.ProcessInput(self.input_id, sample, 0) } {
                Ok(()) => break,
                // Output must be collected before it takes more input.
                Err(error) if error.code() == MF_E_NOTACCEPTING => {
                    if !self.process_output()? {
                        return Err(MfError::new("IMFTransform::ProcessInput", error.code()));
                    }
                }
                Err(error) => return Err(MfError::new("IMFTransform::ProcessInput", error.code())),
            }
        }
        while self.process_output()? {}
        Ok(())
    }

    /// Asynchronous MFT: wait until it asks for input, feed the frame, then
    /// wait for its output.
    fn encode_async(
        &mut self,
        events: &IMFMediaEventGenerator,
        sample: &IMFSample,
    ) -> Result<(), MfError> {
        let timeout = Instant::now() + EVENT_TIMEOUT;
        while self.need_input == 0 {
            match wait_event(events, timeout)? {
                Some(Event::NeedInput) => self.need_input += 1,
                Some(Event::HaveOutput) => {
                    self.process_output()?;
                }
                Some(Event::Other) => {}
                None => return Err(MfError::new("waiting for METransformNeedInput", E_FAIL)),
            }
        }
        // SAFETY: the MFT and sample are valid, and the MFT asked for input.
        unsafe { self.transform.ProcessInput(self.input_id, sample, 0) }
            .context("IMFTransform::ProcessInput")?;
        self.need_input -= 1;

        // After an input the MFT must either produce output or ask for more
        // input, so while no input request is pending its output is due.
        // Once one is, the MFT may be waiting for more frames before it
        // outputs this one (pipelining), so only wait a short grace period;
        // the output is then collected by a later call.
        let submitted = Instant::now();
        let (grace, timeout) = (submitted + self.output_grace, submitted + EVENT_TIMEOUT);
        loop {
            let input_pending = self.need_input > 0;
            match wait_event(events, if input_pending { grace } else { timeout })? {
                Some(Event::HaveOutput) => {
                    self.process_output()?;
                    return Ok(());
                }
                Some(Event::NeedInput) => self.need_input += 1,
                Some(Event::Other) => {}
                None if input_pending => return Ok(()),
                None => return Err(MfError::new("waiting for METransformHaveOutput", E_FAIL)),
            }
        }
    }

    /// Collects one output sample. Returns `false` if the MFT needs more
    /// input first.
    fn process_output(&mut self) -> Result<bool, MfError> {
        // One output format change (the MFT settled its parameters) is
        // expected; renegotiate and retry.
        for _ in 0..2 {
            let sample = if self.provides_samples {
                None
            } else {
                Some(self.output_sample()?)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_id,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            // SAFETY: the MFT is valid; `buffers` describes its one output
            // stream, with a caller-allocated sample unless the MFT provides
            // samples; `status` is a valid out pointer.
            let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
            let [buffer] = buffers;
            // We own one reference to whatever sample and event collection
            // the call left in the buffer; taking them releases both.
            let sample = ManuallyDrop::into_inner(buffer.pSample);
            drop(ManuallyDrop::into_inner(buffer.pEvents));
            match result {
                Ok(()) => {
                    let Some(sample) = sample else {
                        return Ok(false);
                    };
                    let data = sample_bytes(&sample)?;
                    self.push_output(&data)?;
                    return Ok(true);
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.renegotiate_output()?;
                }
                Err(error) => {
                    return Err(MfError::new("IMFTransform::ProcessOutput", error.code()))
                }
            }
        }
        Err(MfError::new(
            "IMFTransform::ProcessOutput (format change)",
            MF_E_TRANSFORM_STREAM_CHANGE,
        ))
    }

    fn output_sample(&self) -> Result<IMFSample, MfError> {
        // SAFETY: creates a sample holding one new system-memory buffer.
        attempt(|| unsafe {
            let sample = MFCreateSample()?;
            let buffer = MFCreateMemoryBuffer(self.output_buffer_size)?;
            sample.AddBuffer(&buffer)?;
            Ok(sample)
        })
        .context("output sample")
    }

    fn renegotiate_output(&mut self) -> Result<(), MfError> {
        // SAFETY: the MFT is valid; it offers its new output type first.
        attempt(|| unsafe {
            let media_type = self.transform.GetOutputAvailableType(self.output_id, 0)?;
            self.transform.SetOutputType(self.output_id, &media_type, 0)
        })
        .context("output format change")?;
        self.refresh_output_info()
    }

    fn push_output(&mut self, data: &[u8]) -> Result<(), MfError> {
        let codec = self.codec;
        let frame = normalize_access_unit(data, codec, &mut self.parameter_sets)
            .map_err(|()| MfError::new("encoder output not Annex B or missing parameter sets", E_UNEXPECTED))?;
        let Some(frame) = frame else {
            return Ok(());
        };
        if frame.keyframe {
            self.force_keyframe = false;
            self.started = true;
        }
        // A decoder can only start from a keyframe.
        if self.started {
            self.ready.push_back(frame);
        }
        Ok(())
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        ensure_com();
        // SAFETY: the MFT and activation object are valid. Shutting down via
        // the activation object releases the hardware encoder's resources
        // (required for asynchronous MFTs); errors are irrelevant here.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// The hardware encoders for `codec`, best first.
fn hardware_encoders(codec: Codec) -> Result<Vec<IMFActivate>, MfError> {
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: match codec {
            Codec::H264 => MFVideoFormat_H264,
            Codec::Hevc => MFVideoFormat_HEVC,
        },
    };
    let mut array: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: `output` outlives the call; `array` and `count` are valid out
    // pointers.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(ptr::from_ref(&output)),
            &mut array,
            &mut count,
        )
    }
    .context("MFTEnumEx")?;
    if array.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: MFTEnumEx returned `count` activation pointers in a
    // CoTaskMemAlloc'd array. Each reference is moved out exactly once
    // (`take`), then the array itself is freed.
    let activates = unsafe {
        let activates = std::slice::from_raw_parts_mut(array, count as usize)
            .iter_mut()
            .filter_map(Option::take)
            .collect();
        CoTaskMemFree(Some(array.cast_const().cast()));
        activates
    };
    Ok(activates)
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut name = [0u16; 256];
    let mut len = 0u32;
    // SAFETY: valid activation object; `name` is a writable buffer of its
    // length and `len` a valid out pointer.
    match unsafe {
        activate.GetString(
            &MFT_FRIENDLY_NAME_Attribute,
            &mut name,
            Some(ptr::from_mut(&mut len)),
        )
    } {
        Ok(()) => String::from_utf16_lossy(&name[..(len as usize).min(name.len())]),
        Err(_) => "unnamed hardware encoder".to_owned(),
    }
}

/// Next event of an asynchronous MFT, or `None` once `deadline` passes.
///
/// Polls instead of blocking in `GetEvent`, so that a stuck driver makes the
/// encoder fail (and the session fall back to openh264) rather than hang.
fn wait_event(
    events: &IMFMediaEventGenerator,
    deadline: Instant,
) -> Result<Option<Event>, MfError> {
    loop {
        if let Some(event) = next_event(events)? {
            return Ok(Some(event));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(EVENT_POLL_INTERVAL);
    }
}

/// The next queued event of an asynchronous MFT, if any.
fn next_event(events: &IMFMediaEventGenerator) -> Result<Option<Event>, MfError> {
    // SAFETY: valid event generator; NO_WAIT returns immediately.
    let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
        Ok(event) => event,
        Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(None),
        Err(error) => {
            return Err(MfError::new(
                "IMFMediaEventGenerator::GetEvent",
                error.code(),
            ))
        }
    };
    // SAFETY: valid event.
    let (kind, status) = unsafe { (event.GetType(), event.GetStatus()) };
    // A failed status (e.g. on MEError) means the encoder broke.
    status
        .context("IMFMediaEvent::GetStatus")?
        .ok()
        .context("hardware encoder event")?;
    let kind = kind.context("IMFMediaEvent::GetType")?;
    Ok(Some(if kind == METransformNeedInput.0.cast_unsigned() {
        Event::NeedInput
    } else if kind == METransformHaveOutput.0.cast_unsigned() {
        Event::HaveOutput
    } else {
        Event::Other
    }))
}

/// Copies the payload of an output sample.
fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>, MfError> {
    // SAFETY: valid sample; the returned buffer is retained.
    let buffer =
        unsafe { sample.ConvertToContiguousBuffer() }.context("ConvertToContiguousBuffer")?;
    let mut data: *mut u8 = ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: valid buffer; `data` and `len` are valid out pointers.
    unsafe { buffer.Lock(&mut data, None, Some(ptr::from_mut(&mut len))) }
        .context("IMFMediaBuffer::Lock")?;
    let bytes = if data.is_null() {
        Vec::new()
    } else {
        // SAFETY: the locked buffer holds `len` valid bytes at `data`; they
        // are copied out before Unlock.
        unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec()
    };
    // SAFETY: balances the successful Lock above.
    unsafe { buffer.Unlock() }.context("IMFMediaBuffer::Unlock")?;
    Ok(bytes)
}

/// Runs a sequence of Media Foundation calls that use `?`, so the whole
/// sequence can be given one error context.
fn attempt<T>(calls: impl FnOnce() -> windows::core::Result<T>) -> windows::core::Result<T> {
    calls()
}

/// Packs two `UINT32`s the way `MFSetAttributeSize`/`Ratio` do.
fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

fn variant_u32(value: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: value },
            }),
        },
    }
}

fn variant_bool_true() -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: VARIANT_TRUE,
                },
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use openh264::{decoder::Decoder, formats::YUVSource};

    use super::*;
    use crate::encode::tests::{nal_types, synthetic_frame};

    /// Creates a hardware encoder, or `None` (test skipped) where Media
    /// Foundation has no hardware H.264 encoder, e.g. in a VM or on CI.
    fn hardware_encoder(width: u16, height: u16) -> Option<MfEncoder> {
        match MfEncoder::new(width, height, 15, 1_000_000, Codec::H264) {
            Ok(encoder) => Some(encoder),
            Err(error) => {
                eprintln!("skipping Media Foundation test: {error}");
                None
            }
        }
    }

    /// Frames an async MFT may hold back while pipelining.
    const MAX_LAG: usize = 2;

    fn encode_and_decode(width: u16, height: u16, frames: u32) {
        let Some(mut encoder) = hardware_encoder(width, height) else {
            return;
        };
        let mut decoder = Decoder::new().expect("openh264 decoder");
        let mut outputs = Vec::new();
        let mut decoded = 0usize;
        let mut encode_time = Duration::ZERO;
        let mut bytes = 0;
        for index in 0..frames {
            if index == frames / 2 {
                encoder.force_intra_frame();
            }
            let frame = synthetic_frame(width, height, index);
            let started = Instant::now();
            let output = encoder.encode(&frame).expect("Media Foundation encode");
            encode_time += started.elapsed();
            let Some(output) = output else {
                continue;
            };
            bytes += output.data.len();

            let types = nal_types(&output.data);
            assert_eq!(output.data[..4], [0, 0, 0, 1], "Annex B start code");
            assert_eq!(
                output.keyframe,
                types.contains(&5),
                "frame {index}: {types:?}"
            );
            if outputs.is_empty() {
                assert!(output.keyframe, "stream starts with a keyframe: {types:?}");
            }
            if output.keyframe {
                assert_eq!(types[..2], [7, 8], "SPS/PPS lead keyframe: {types:?}");
                // profile_idc 66 = Baseline, matching the viewer's avc1.42E01E.
                assert_eq!(output.data[5], 66, "Baseline profile");
            } else {
                assert!(
                    !types.iter().any(|t| matches!(t, 5 | 7 | 8)),
                    "delta frame: {types:?}"
                );
            }
            outputs.push((index, output.keyframe));

            if let Some(yuv) = decoder.decode(&output.data).expect("valid H.264") {
                assert_eq!(yuv.dimensions(), (usize::from(width), usize::from(height)));
                decoded += 1;
            }
        }
        decoded += decoder.flush_remaining().expect("flush").len();
        let frames = frames as usize;
        assert!(
            outputs.len() + MAX_LAG >= frames,
            "{} access units for {frames} frames",
            outputs.len()
        );
        assert_eq!(
            decoded,
            outputs.len(),
            "every access unit decodes to a frame"
        );
        // The forced keyframe shows up within the pipeline lag.
        assert!(
            outputs.iter().any(|&(index, key)| key
                && index >= frames as u32 / 2
                && index <= frames as u32 / 2 + MAX_LAG as u32),
            "forced keyframe: {outputs:?}"
        );
        eprintln!(
            "Media Foundation {width}x{height}: {:?}/frame, {} bytes/frame",
            encode_time / frames as u32,
            bytes / outputs.len().max(1)
        );
    }

    #[test]
    fn mediafoundation_stream_decodes_with_openh264() {
        encode_and_decode(640, 360, 30);
    }

    #[test]
    fn mediafoundation_handles_non_macroblock_aligned_sizes() {
        encode_and_decode(1366, 770, 8);
    }

    #[test]
    fn mediafoundation_rejects_mismatched_frames() {
        let Some(mut encoder) = hardware_encoder(64, 64) else {
            return;
        };
        let frame = synthetic_frame(32, 32, 0);
        assert_eq!(encoder.encode(&frame).map(|out| out.is_none()), Ok(true));
    }
}
