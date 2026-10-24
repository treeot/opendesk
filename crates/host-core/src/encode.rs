//! H.264 video encoding.
//!
//! Bitstream contract (what `viewer/src/lib/decode.ts` feeds to WebCodecs,
//! configured as `avc1.42E01E` with no `description`):
//! - Annex B byte stream: every NAL unit is prefixed with a `00 00 00 01`
//!   start code (no AVCC length prefixes).
//! - Keyframes are IDR access units that carry SPS and PPS in-band, in front
//!   of the IDR slice, so a decoder can start from any keyframe.
//! - Baseline profile, no B-frames: one access unit out per frame in.
//! - `EncodedFrame::keyframe` is true exactly when the access unit holds an
//!   IDR slice; it becomes the WebCodecs chunk `type`.
//!
//! [`VideoEncoder`] picks the best backend for the platform and every backend
//! honours this contract.

#[cfg(target_os = "windows")]
mod mediafoundation;
#[cfg(any(target_os = "windows", all(target_os = "linux", feature = "vaapi"), test))]
mod nv12;
pub mod stream;
#[cfg(all(target_os = "linux", feature = "vaapi"))]
mod vaapi;
#[cfg(target_os = "macos")]
mod videotoolbox;

use openh264::{
    encoder::{BitRate, Encoder as OpenH264Encoder, EncoderConfig, FrameRate, FrameType},
    formats::{BgraSliceU8, YUVBuffer},
    OpenH264API,
};

#[derive(Clone)]
pub struct RawFrame {
    pub width: u16,
    pub height: u16,
    pub data: Vec<u8>,
    pub format: PixelFormat,
    pub timestamp_us: u32,
}

#[derive(Clone, Copy)]
pub enum PixelFormat {
    Bgra,
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

pub trait Encoder {
    fn encode(&mut self, frame: &RawFrame) -> Option<EncodedFrame>;

    /// The codec this encoder produces. Defaults to H.264, which every backend
    /// but the HEVC hardware paths uses.
    fn codec(&self) -> Codec {
        Codec::H264
    }

    /// Request that the next encoded frame be an intra frame when supported.
    fn force_intra_frame(&mut self) {}

    /// Change the target bitrate (bps) of a running encoder, from the next
    /// frame on, where the backend supports it.
    fn set_bitrate(&mut self, _bps: u32) {}
}

/// An H.264 encoder could not be created.
#[derive(Debug)]
pub struct EncoderInitError(String);

impl EncoderInitError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

impl std::fmt::Display for EncoderInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "video encoder initialization failed: {}", self.0)
    }
}

impl std::error::Error for EncoderInitError {}

pub struct H264Encoder {
    encoder: OpenH264Encoder,
    first_frame: bool,
    width: u16,
    height: u16,
    /// A bitrate change requested before openh264 initialized itself (it
    /// does so on the first encode).
    pending_bitrate: Option<u32>,
    /// The bitrate openh264 currently targets.
    bitrate: u32,
}

impl H264Encoder {
    pub fn new(width: u16, height: u16, fps: u32, bitrate: u32) -> Result<Self, EncoderInitError> {
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(fps as f32));
        let encoder = OpenH264Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|error| EncoderInitError(format!("openh264: {error}")))?;

        Ok(Self {
            encoder,
            first_frame: true,
            width,
            height,
            pending_bitrate: None,
            bitrate,
        })
    }

    pub fn force_intra_frame(&mut self) {
        self.encoder.force_intra_frame();
    }

    /// Sets openh264's target and maximum bitrate on the initialized encoder.
    fn apply_bitrate(&mut self, bps: u32) {
        use std::os::raw::{c_int, c_void};
        // openh264's ENCODER_OPTION_{BITRATE,MAX_BITRATE} and layer ids.
        const OPTION_BITRATE: c_int = 5;
        const OPTION_MAX_BITRATE: c_int = 6;
        const SPATIAL_LAYER_0: c_int = 0;
        const SPATIAL_LAYER_ALL: c_int = 4;
        /// Mirrors openh264's `SBitrateInfo { int iLayer; int iBitrate; }`.
        #[repr(C)]
        struct BitrateInfo {
            layer: c_int,
            bitrate: c_int,
        }
        let bitrate = c_int::try_from(bps).unwrap_or(c_int::MAX);
        // The maximum must never sit below the target: raise it first when
        // going up, lower it last when going down.
        let target = [(OPTION_BITRATE, SPATIAL_LAYER_ALL), (OPTION_BITRATE, SPATIAL_LAYER_0)];
        let maximum = [(OPTION_MAX_BITRATE, SPATIAL_LAYER_ALL), (OPTION_MAX_BITRATE, SPATIAL_LAYER_0)];
        let order = if bps < self.bitrate {
            [target, maximum]
        } else {
            [maximum, target]
        };
        for (option, layer) in order.into_iter().flatten() {
            let mut info = BitrateInfo { layer, bitrate };
            // SAFETY: the encoder is initialized (it encoded a frame), and both
            // options read one SBitrateInfo, which `BitrateInfo` mirrors; the
            // pointer is valid for the duration of the call.
            let status = unsafe { self.encoder.raw_api().set_option(option, (&raw mut info).cast::<c_void>()) };
            if status != 0 {
                log::debug!("openh264 bitrate option {option} (layer {layer}) not applied: {status}");
            }
        }
        self.bitrate = bps;
    }
}

impl Encoder for H264Encoder {
    fn force_intra_frame(&mut self) {
        self.encoder.force_intra_frame();
    }

    fn set_bitrate(&mut self, bps: u32) {
        if self.first_frame {
            self.pending_bitrate = Some(bps);
        } else {
            self.apply_bitrate(bps);
        }
    }

    fn encode(&mut self, frame: &RawFrame) -> Option<EncodedFrame> {
        if !matches!(frame.format, PixelFormat::Bgra)
            || frame.width != self.width
            || frame.height != self.height
            || frame.data.len() != usize::from(frame.width) * usize::from(frame.height) * 4
        {
            return None;
        }

        let source = BgraSliceU8::new(
            &frame.data,
            (usize::from(frame.width), usize::from(frame.height)),
        );
        let yuv = YUVBuffer::from_rgb_source(source);
        if self.first_frame {
            self.encoder.force_intra_frame();
        }

        let bitstream = self.encoder.encode(&yuv).ok()?;
        let keyframe = self.first_frame || bitstream.frame_type() == FrameType::IDR;
        let data = bitstream.to_vec();
        self.first_frame = false;
        if let Some(bps) = self.pending_bitrate.take() {
            self.apply_bitrate(bps);
        }

        Some(EncodedFrame { data, keyframe })
    }
}

/// Annex B start code written in front of every NAL unit.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
const START_CODE: [u8; 4] = [0, 0, 0, 1];
/// `nal_unit_type` of an IDR slice (H.264), kept for the tests.
#[cfg(test)]
const NAL_IDR: u8 = 5;

/// The `nal_unit_type` of a NAL unit, read by the codec's rule: H.264 keeps it
/// in the low 5 bits of the first byte, HEVC in bits 1..6 of a two-byte header.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn nal_unit_type(codec: Codec, nal: &[u8]) -> u8 {
    match codec {
        Codec::H264 => nal[0] & 0x1f,
        Codec::Hevc => (nal[0] >> 1) & 0x3f,
    }
}

/// Whether a NAL unit is a keyframe slice: H.264 IDR (5), or an HEVC IRAP
/// picture (BLA/IDR/CRA, types 16..=21).
#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn is_keyframe_nal(codec: Codec, nal: &[u8]) -> bool {
    match codec {
        Codec::H264 => nal_unit_type(codec, nal) == 5,
        Codec::Hevc => (16..=21).contains(&nal_unit_type(codec, nal)),
    }
}

/// Whether a NAL unit is a parameter set or access unit delimiter, which never
/// count as frame body: H.264 SPS/PPS/AUD (7/8/9), HEVC VPS/SPS/PPS/AUD
/// (32/33/34/35).
#[cfg(any(target_os = "windows", test))]
fn is_parameter_or_aud(codec: Codec, nal: &[u8]) -> bool {
    match codec {
        Codec::H264 => matches!(nal_unit_type(codec, nal), 7..=9),
        Codec::Hevc => matches!(nal_unit_type(codec, nal), 32..=35),
    }
}

/// Converts length-prefixed (AVCC / ISO 14496-15) NAL units into Annex B.
///
/// `length_size` is the size in bytes (1, 2 or 4) of each big-endian NAL
/// length prefix. The converted NAL units are appended to `out`. Returns
/// whether an IDR slice was present, or `None` if `avcc` is malformed (in
/// which case `out` may hold a partial conversion).
#[cfg(any(target_os = "macos", test))]
pub(crate) fn avcc_to_annex_b(avcc: &[u8], length_size: usize, codec: Codec, out: &mut Vec<u8>) -> Option<bool> {
    if !matches!(length_size, 1 | 2 | 4) {
        return None;
    }
    let mut idr = false;
    let mut rest = avcc;
    while !rest.is_empty() {
        if rest.len() < length_size {
            return None;
        }
        let (prefix, tail) = rest.split_at(length_size);
        let len = prefix
            .iter()
            .fold(0usize, |len, &byte| (len << 8) | usize::from(byte));
        if len == 0 || tail.len() < len {
            return None;
        }
        let (nal, tail) = tail.split_at(len);
        idr |= is_keyframe_nal(codec, nal);
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
        rest = tail;
    }
    Some(idr)
}

/// Appends parameter sets (SPS, PPS, ...) to `out` as Annex B NAL units.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
pub(crate) fn append_parameter_sets<'a>(
    sets: impl IntoIterator<Item = &'a [u8]>,
    out: &mut Vec<u8>,
) {
    for set in sets {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(set);
    }
}


/// Splits an Annex B byte stream into its NAL units (without start codes).
///
/// Accepts 3- and 4-byte start codes. Returns `None` if the stream does not
/// begin with a start code or holds an empty NAL unit.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn split_annex_b(stream: &[u8]) -> Option<Vec<&[u8]>> {
    /// Index of the next `00 00 01` at or after `from`.
    fn find_start_code(stream: &[u8], from: usize) -> Option<usize> {
        stream
            .get(from..)?
            .windows(3)
            .position(|window| window == [0, 0, 1])
            .map(|offset| from + offset)
    }

    // Only zero bytes may precede the first start code.
    let first = find_start_code(stream, 0)?;
    if stream[..first].iter().any(|&byte| byte != 0) {
        return None;
    }
    let mut nals = Vec::new();
    let mut start = first + 3;
    loop {
        let end = find_start_code(stream, start);
        let nal = &stream[start..end.unwrap_or(stream.len())];
        // A NAL unit never ends in a zero byte: trailing zeros belong to the
        // next 4-byte start code (or are trailing_zero_8bits).
        let len = nal.iter().rposition(|&byte| byte != 0)? + 1;
        nals.push(&nal[..len]);
        match end {
            Some(end) => start = end + 3,
            None => return Some(nals),
        }
    }
}

/// The most recent SPS and PPS an encoder produced, for IDR access units
/// that arrive without them in-band.
#[cfg(any(target_os = "windows", test))]
pub(crate) struct ParameterSets {
    codec: Codec,
    /// HEVC video parameter sets; always empty for H.264.
    vps: Vec<Vec<u8>>,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
}

#[cfg(any(target_os = "windows", test))]
impl ParameterSets {
    pub(crate) fn new(codec: Codec) -> Self {
        Self { codec, vps: Vec::new(), sps: Vec::new(), pps: Vec::new() }
    }

    /// Records the parameter sets found in `nals`, replacing the previous ones
    /// of each kind. HEVC also carries a VPS.
    fn update<'a>(&mut self, nals: impl IntoIterator<Item = &'a [u8]> + Clone) {
        let codec = self.codec;
        let collect = |kind: u8| -> Option<Vec<Vec<u8>>> {
            let mut found = nals
                .clone()
                .into_iter()
                .filter(|nal| nal_unit_type(codec, nal) == kind)
                .peekable();
            found.peek().is_some().then(|| found.map(<[u8]>::to_vec).collect())
        };
        match codec {
            Codec::H264 => {
                if let Some(sps) = collect(7) { self.sps = sps; }
                if let Some(pps) = collect(8) { self.pps = pps; }
            }
            Codec::Hevc => {
                if let Some(vps) = collect(32) { self.vps = vps; }
                if let Some(sps) = collect(33) { self.sps = sps; }
                if let Some(pps) = collect(34) { self.pps = pps; }
            }
        }
    }

    /// Whether every parameter set a keyframe needs is present.
    fn complete(&self) -> bool {
        let base = !self.sps.is_empty() && !self.pps.is_empty();
        match self.codec {
            Codec::H264 => base,
            Codec::Hevc => base && !self.vps.is_empty(),
        }
    }

    /// The parameter sets in the order they must precede a keyframe.
    fn ordered(&self) -> impl Iterator<Item = &[u8]> {
        self.vps.iter().chain(&self.sps).chain(&self.pps).map(Vec::as_slice)
    }

    /// Records the parameter sets from an Annex B sequence header (Media
    /// Foundation's `MF_MT_MPEG_SEQUENCE_HEADER`). Malformed input is ignored.
    pub(crate) fn update_from_annex_b(&mut self, header: &[u8]) {
        if let Some(nals) = split_annex_b(header) {
            self.update(nals.iter().copied());
        }
    }
}

/// Rewrites one encoded Annex B access unit to the module contract: 4-byte
/// start codes, no access unit delimiters, parameter sets only on IDR access
/// units and there in front (in-band ones, else the cached `sets`).
///
/// Returns `Err(())` if the stream is not Annex B or an IDR has no parameter
/// sets to go with it, and `Ok(None)` if nothing is left to send (the access
/// unit only held parameter sets or delimiters).
#[cfg(any(target_os = "windows", test))]
pub(crate) fn normalize_access_unit(
    stream: &[u8],
    codec: Codec,
    sets: &mut ParameterSets,
) -> Result<Option<EncodedFrame>, ()> {
    let nals = split_annex_b(stream).ok_or(())?;
    sets.update(nals.iter().copied());
    let is_body = |nal: &&[u8]| !is_parameter_or_aud(codec, nal);
    if !nals.iter().any(is_body) {
        return Ok(None);
    }
    let keyframe = nals.iter().any(|nal| is_keyframe_nal(codec, nal));
    let mut data = Vec::with_capacity(stream.len() + 64);
    if keyframe {
        if !sets.complete() {
            return Err(());
        }
        append_parameter_sets(sets.ordered(), &mut data);
    }
    append_parameter_sets(nals.into_iter().filter(is_body), &mut data);
    Ok(Some(EncodedFrame { data, keyframe }))
}

/// Which implementation a [`VideoEncoder`] is using.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderBackend {
    /// openh264, in software. Available everywhere.
    OpenH264,
    /// Apple VideoToolbox, hardware accelerated (macOS).
    VideoToolbox,
    /// A Media Foundation hardware encoder MFT (NVENC, Quick Sync, AMF;
    /// Windows).
    MediaFoundation,
    /// VAAPI hardware encoding via libavcodec (Linux; experimental).
    Vaapi,
}

impl std::fmt::Display for EncoderBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::OpenH264 => "openh264 (software)",
            Self::VideoToolbox => "VideoToolbox (hardware)",
            Self::MediaFoundation => "Media Foundation (hardware)",
            Self::Vaapi => "VAAPI (hardware)",
        })
    }
}

enum Backend {
    OpenH264(Box<H264Encoder>),
    #[cfg(target_os = "macos")]
    VideoToolbox(videotoolbox::VtEncoder),
    #[cfg(target_os = "windows")]
    MediaFoundation(mediafoundation::MfEncoder),
    #[cfg(all(target_os = "linux", feature = "vaapi"))]
    Vaapi(vaapi::VaapiEncoder),
}

/// H.264 encoder that uses the best backend available on this machine.
///
/// On macOS it tries a hardware VideoToolbox session, on Windows a Media
/// Foundation hardware encoder, and falls back to openh264 if the hardware
/// encoder cannot be created, or if it fails later (the fallback then starts
/// with a keyframe). Other platforms use openh264; new hardware backends
/// (VAAPI) slot in as more `Backend` variants. Setting
/// `OPENDESK_ENCODER=software` forces openh264.
// The settings are only read when rebuilding after a hardware failure.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
/// The codec a stream is encoded with. The frame header carries it as a byte
/// (`0` H.264, `1` HEVC) so the viewer configures the matching decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    /// The frame-header codec byte. H.264 is `1` to match legacy hosts (which
    /// always wrote `1`), so a new viewer reads their streams as H.264; HEVC is
    /// a new value older viewers never saw.
    pub fn as_byte(self) -> u8 {
        match self {
            Codec::H264 => 1,
            Codec::Hevc => 2,
        }
    }
}

pub struct VideoEncoder {
    backend: Backend,
    codec: Codec,
    width: u16,
    height: u16,
    fps: u32,
    bitrate: u32,
}

impl VideoEncoder {
    pub fn best_available(
        width: u16,
        height: u16,
        fps: u32,
        bitrate: u32,
        codec: Codec,
    ) -> Result<Self, EncoderInitError> {
        let force_software = std::env::var("OPENDESK_ENCODER")
            .is_ok_and(|value| value.eq_ignore_ascii_case("software"));
        if force_software {
            log::info!("OPENDESK_ENCODER=software: hardware encoding disabled");
        }
        // HEVC exists only on a hardware backend; without one, fall back to
        // H.264 (hardware if present, else openh264). The frame header carries
        // the codec actually used, so the viewer follows a fallback.
        let hardware = (!force_software).then(|| Self::hardware(width, height, fps, bitrate, codec)).flatten();
        let (backend, actual) = match hardware {
            Some(backend) => (backend, codec),
            None => {
                let h264 = (!force_software)
                    .then(|| Self::hardware(width, height, fps, bitrate, Codec::H264))
                    .flatten();
                match h264 {
                    Some(backend) => (backend, Codec::H264),
                    None => (Backend::OpenH264(Box::new(H264Encoder::new(width, height, fps, bitrate)?)), Codec::H264),
                }
            }
        };
        let encoder = Self {
            backend,
            codec: actual,
            width,
            height,
            fps,
            bitrate,
        };
        log::info!(
            "video encoder: {} {actual:?} {width}x{height} @ {fps} fps, {bitrate} bps",
            encoder.backend()
        );
        Ok(encoder)
    }

    #[cfg(target_os = "macos")]
    fn hardware(width: u16, height: u16, fps: u32, bitrate: u32, codec: Codec) -> Option<Backend> {
        match videotoolbox::VtEncoder::new(width, height, fps, bitrate, codec) {
            Ok(encoder) => Some(Backend::VideoToolbox(encoder)),
            Err(error) => {
                log::info!("VideoToolbox {codec:?} unavailable ({error}); falling back");
                None
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn hardware(width: u16, height: u16, fps: u32, bitrate: u32, codec: Codec) -> Option<Backend> {
        match mediafoundation::MfEncoder::new(width, height, fps, bitrate, codec) {
            Ok(encoder) => Some(Backend::MediaFoundation(encoder)),
            Err(error) => {
                log::info!(
                    "Media Foundation {codec:?} encoder unavailable ({error}); falling back"
                );
                None
            }
        }
    }

    #[cfg(all(target_os = "linux", feature = "vaapi"))]
    fn hardware(width: u16, height: u16, fps: u32, bitrate: u32, codec: Codec) -> Option<Backend> {
        // VAAPI here only produces H.264; an HEVC request falls back and
        // best_available retries with H.264.
        if codec != Codec::H264 {
            return None;
        }
        match vaapi::VaapiEncoder::new(width, height, fps, bitrate) {
            Ok(encoder) => Some(Backend::Vaapi(encoder)),
            Err(error) => {
                log::info!("VAAPI encoder unavailable ({error}); falling back");
                None
            }
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", all(target_os = "linux", feature = "vaapi"))))]
    fn hardware(_width: u16, _height: u16, _fps: u32, _bitrate: u32, _codec: Codec) -> Option<Backend> {
        None
    }

    pub fn backend(&self) -> EncoderBackend {
        match self.backend {
            Backend::OpenH264(_) => EncoderBackend::OpenH264,
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(_) => EncoderBackend::VideoToolbox,
            #[cfg(target_os = "windows")]
            Backend::MediaFoundation(_) => EncoderBackend::MediaFoundation,
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            Backend::Vaapi(_) => EncoderBackend::Vaapi,
        }
    }

    /// Replaces a failed hardware backend with openh264 and encodes `frame`
    /// with it. A fresh openh264 encoder always starts with a keyframe.
    #[cfg(any(target_os = "macos", target_os = "windows", all(target_os = "linux", feature = "vaapi")))]
    fn switch_to_software(&mut self, frame: &RawFrame) -> Option<EncodedFrame> {
        let mut software =
            match H264Encoder::new(self.width, self.height, self.fps, self.bitrate) {
                Ok(software) => software,
                Err(error) => {
                    log::warn!("{error}; dropping the frame");
                    return None;
                }
            };
        let encoded = software.encode(frame);
        self.backend = Backend::OpenH264(Box::new(software));
        // openh264 only does H.264, so the stream's codec changes with it.
        self.codec = Codec::H264;
        encoded
    }
}

impl Encoder for VideoEncoder {
    fn codec(&self) -> Codec {
        self.codec
    }

    fn encode(&mut self, frame: &RawFrame) -> Option<EncodedFrame> {
        match &mut self.backend {
            Backend::OpenH264(encoder) => encoder.encode(frame),
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(encoder) => match encoder.encode(frame) {
                Ok(encoded) => encoded,
                Err(error) => {
                    log::warn!("VideoToolbox encode failed ({error}); switching to openh264");
                    self.switch_to_software(frame)
                }
            },
            #[cfg(target_os = "windows")]
            Backend::MediaFoundation(encoder) => match encoder.encode(frame) {
                Ok(encoded) => encoded,
                Err(error) => {
                    log::warn!("Media Foundation encode failed ({error}); switching to openh264");
                    self.switch_to_software(frame)
                }
            },
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            Backend::Vaapi(encoder) => match encoder.encode(frame) {
                Ok(encoded) => encoded,
                Err(error) => {
                    log::warn!("VAAPI encode failed ({error}); switching to openh264");
                    self.switch_to_software(frame)
                }
            },
        }
    }

    fn force_intra_frame(&mut self) {
        match &mut self.backend {
            Backend::OpenH264(encoder) => encoder.force_intra_frame(),
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(encoder) => encoder.force_intra_frame(),
            #[cfg(target_os = "windows")]
            Backend::MediaFoundation(encoder) => encoder.force_intra_frame(),
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            Backend::Vaapi(encoder) => encoder.force_intra_frame(),
        }
    }

    fn set_bitrate(&mut self, bps: u32) {
        // A software fallback built later starts at this bitrate too.
        self.bitrate = bps;
        match &mut self.backend {
            Backend::OpenH264(encoder) => encoder.set_bitrate(bps),
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(encoder) => encoder.set_bitrate(bps),
            #[cfg(target_os = "windows")]
            Backend::MediaFoundation(encoder) => encoder.set_bitrate(bps),
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            Backend::Vaapi(encoder) => encoder.set_bitrate(bps),
        }
    }
}

pub fn video_header(seq: u64, ts_us: u32, keyframe: bool, w: u16, h: u16, codec: Codec) -> Vec<u8> {
    let mut header = Vec::with_capacity(18);
    header.extend_from_slice(&seq.to_be_bytes());
    header.extend_from_slice(&ts_us.to_be_bytes());
    header.push(codec.as_byte());
    header.push(u8::from(keyframe));
    header.extend_from_slice(&w.to_be_bytes());
    header.extend_from_slice(&h.to_be_bytes());
    header
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn video_header_is_18_bytes_h264_with_keyframe_flag() {
        let h = video_header(5, 1000, true, 1280, 720, Codec::H264);

        assert_eq!(h.len(), 18);
        assert_eq!(&h[0..8], &5u64.to_be_bytes());
        assert_eq!(&h[8..12], &1000u32.to_be_bytes());
        assert_eq!(h[12], 1);
        assert_eq!(h[13] & 1, 1);
        assert_eq!(&h[14..16], &1280u16.to_be_bytes());
        assert_eq!(&h[16..18], &720u16.to_be_bytes());
    }

    #[test]
    fn encoder_produces_keyframe_on_first_frame() {
        let mut enc = H264Encoder::new(64, 64, 15, 1_000_000).expect("openh264");
        let frame = RawFrame {
            width: 64,
            height: 64,
            data: vec![120u8; 64 * 64 * 4],
            format: PixelFormat::Bgra,
            timestamp_us: 0,
        };

        let output = enc.encode(&frame).expect("encodes");
        assert!(!output.data.is_empty());
        assert!(output.keyframe, "first frame must be a keyframe");
    }

    /// `nal_unit_type`s of an Annex B stream with 4-byte start codes.
    pub(crate) fn nal_types(annex_b: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0;
        while i + 4 < annex_b.len() {
            if annex_b[i..i + 4] == START_CODE {
                types.push(annex_b[i + 4] & 0x1f);
                i += 4;
            } else {
                i += 1;
            }
        }
        types
    }

    /// A moving BGRA test pattern so that delta frames carry real changes.
    pub(crate) fn synthetic_frame(width: u16, height: u16, index: u32) -> RawFrame {
        let (w, h) = (usize::from(width), usize::from(height));
        let mut data = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let shifted = x + index as usize * 4;
                data.extend_from_slice(&[
                    (shifted % 256) as u8,
                    (y % 256) as u8,
                    ((shifted ^ y) % 256) as u8,
                    255,
                ]);
            }
        }
        RawFrame {
            width,
            height,
            data,
            format: PixelFormat::Bgra,
            timestamp_us: index * 66_666,
        }
    }

    /// A BGRA frame of pseudo-random noise: incompressible, so the encoded
    /// size follows the bitrate.
    pub(crate) fn noise_frame(width: u16, height: u16, seed: u32) -> RawFrame {
        let mut state = u64::from(seed).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let data = (0..usize::from(width) * usize::from(height) * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        RawFrame { width, height, data, format: PixelFormat::Bgra, timestamp_us: seed * 33_333 }
    }

    /// Mean encoded size of `frames` noise frames.
    pub(crate) fn mean_size(encoder: &mut dyn Encoder, start: u32, frames: u32) -> usize {
        let total: usize = (start..start + frames)
            .map(|index| encoder.encode(&noise_frame(320, 240, index)).map_or(0, |out| out.data.len()))
            .sum();
        total / frames as usize
    }

    #[test]
    fn openh264_follows_a_live_bitrate_change() {
        let mut encoder = H264Encoder::new(320, 240, 30, 4_000_000).expect("openh264");
        let high = mean_size(&mut encoder, 0, 20);
        encoder.set_bitrate(300_000);
        let _settle = mean_size(&mut encoder, 20, 10);
        let low = mean_size(&mut encoder, 30, 20);
        assert!(low * 3 < high, "high {high} B/frame, low {low} B/frame");
        encoder.set_bitrate(4_000_000);
        // Noise overshoots even the lowest quality at 300 kbps, so openh264
        // first skips frames to pay that back; measure over a long window.
        let again = mean_size(&mut encoder, 50, 80);
        assert!(again > low * 2, "low {low} B/frame, raised {again} B/frame");
    }

    #[test]
    fn openh264_applies_a_bitrate_set_before_the_first_frame() {
        let mut encoder = H264Encoder::new(320, 240, 30, 4_000_000).expect("openh264");
        encoder.set_bitrate(300_000);
        let _settle = mean_size(&mut encoder, 0, 10);
        let low = mean_size(&mut encoder, 10, 20);
        let mut reference = H264Encoder::new(320, 240, 30, 4_000_000).expect("openh264");
        let _settle = mean_size(&mut reference, 0, 10);
        let high = mean_size(&mut reference, 10, 20);
        assert!(low * 3 < high, "high {high} B/frame, low {low} B/frame");
    }

    #[test]
    fn openh264_output_is_annex_b_with_parameter_sets_on_keyframes() {
        let mut enc = H264Encoder::new(64, 64, 15, 1_000_000).expect("openh264");
        let key = enc.encode(&synthetic_frame(64, 64, 0)).expect("encodes");
        assert_eq!(key.data[..4], START_CODE);
        let types = nal_types(&key.data);
        assert_eq!(
            types[..2],
            [7, 8],
            "SPS and PPS lead the keyframe: {types:?}"
        );
        assert!(types.contains(&NAL_IDR));

        let delta = enc.encode(&synthetic_frame(64, 64, 1)).expect("encodes");
        assert!(!delta.keyframe);
        assert!(!nal_types(&delta.data)
            .iter()
            .any(|t| matches!(t, 5 | 7 | 8)));
    }

    #[test]
    fn avcc_to_annex_b_replaces_length_prefixes_and_detects_idr() {
        let avcc = [0, 0, 0, 2, 0x06, 0xAA, 0, 0, 0, 3, 0x65, 0x88, 0x84];
        let mut out = Vec::new();
        assert_eq!(avcc_to_annex_b(&avcc, 4, Codec::H264, &mut out), Some(true));
        assert_eq!(out, [0, 0, 0, 1, 0x06, 0xAA, 0, 0, 0, 1, 0x65, 0x88, 0x84]);

        let mut out = Vec::new();
        assert_eq!(
            avcc_to_annex_b(&[0, 2, 0x41, 0x9A], 2, Codec::H264, &mut out),
            Some(false)
        );
        assert_eq!(out, [0, 0, 0, 1, 0x41, 0x9A]);

        let mut out = Vec::new();
        assert_eq!(avcc_to_annex_b(&[1, 0x65], 1, Codec::H264, &mut out), Some(true));
        assert_eq!(out, [0, 0, 0, 1, 0x65]);

        let mut out = Vec::new();
        assert_eq!(avcc_to_annex_b(&[], 4, Codec::H264, &mut out), Some(false));
        assert!(out.is_empty());
    }

    #[test]
    fn avcc_to_annex_b_rejects_malformed_input() {
        let mut out = Vec::new();
        // NAL length runs past the end of the buffer.
        assert_eq!(
            avcc_to_annex_b(&[0, 0, 0, 5, 0x65, 0x88], 4, Codec::H264, &mut out),
            None
        );
        // Truncated length prefix.
        assert_eq!(
            avcc_to_annex_b(&[0, 0, 0, 1, 0x41, 0, 0], 4, Codec::H264, &mut out),
            None
        );
        // Zero-length NAL unit.
        assert_eq!(avcc_to_annex_b(&[0, 0, 0, 0], 4, Codec::H264, &mut out), None);
        // Unsupported prefix size.
        assert_eq!(avcc_to_annex_b(&[0, 0, 1, 0x65], 3, Codec::H264, &mut out), None);
    }

    #[test]
    fn parameter_sets_are_written_with_start_codes() {
        let mut out = Vec::new();
        append_parameter_sets([&[0x67, 0x42][..], &[0x68, 0xCE][..]], &mut out);
        assert_eq!(out, [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE]);
    }

    #[test]
    fn split_annex_b_accepts_3_and_4_byte_start_codes() {
        let stream = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x65, 0x88, 0x80,
        ];
        assert_eq!(
            split_annex_b(&stream),
            Some(vec![&[0x67, 0x42][..], &[0x68, 0xCE], &[0x65, 0x88, 0x80]])
        );
        // Leading zero bytes and trailing_zero_8bits are not NAL data.
        assert_eq!(
            split_annex_b(&[0, 0, 0, 0, 1, 0x41, 0x9A, 0, 0]),
            Some(vec![&[0x41, 0x9A][..]])
        );
    }

    #[test]
    fn split_annex_b_rejects_non_annex_b_input() {
        // AVCC (length-prefixed) data.
        assert_eq!(split_annex_b(&[0, 0, 0, 2, 0x65, 0x88]), None);
        // Garbage before the first start code.
        assert_eq!(split_annex_b(&[7, 0, 0, 1, 0x65]), None);
        // Empty NAL units.
        assert_eq!(split_annex_b(&[0, 0, 1, 0, 0, 1, 0x65]), None);
        assert_eq!(split_annex_b(&[0, 0, 0, 1]), None);
        assert_eq!(split_annex_b(&[]), None);
    }

    #[test]
    fn normalize_puts_parameter_sets_first_and_drops_delimiters() {
        let mut sets = ParameterSets::new(Codec::H264);
        // AUD, SEI, SPS, PPS, IDR with mixed start codes.
        let idr = [
            0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x06, 0x05, 0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xCE,
            0, 0, 1, 0x65, 0x88,
        ];
        let frame = normalize_access_unit(&idr, Codec::H264, &mut sets)
            .expect("Annex B")
            .expect("frame");
        assert!(frame.keyframe);
        assert_eq!(
            frame.data,
            [
                0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x06, 0x05, 0, 0, 0, 1,
                0x65, 0x88
            ]
        );

        let delta = normalize_access_unit(&[0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x41, 0x9A], Codec::H264, &mut sets)
            .expect("Annex B")
            .expect("frame");
        assert!(!delta.keyframe);
        assert_eq!(delta.data, [0, 0, 0, 1, 0x41, 0x9A]);
    }

    #[test]
    fn normalize_supplies_cached_parameter_sets_to_bare_idr() {
        let mut sets = ParameterSets::new(Codec::H264);
        // Without any parameter sets an IDR is undecodable.
        assert_eq!(
            normalize_access_unit(&[0, 0, 0, 1, 0x65, 0x88], Codec::H264, &mut sets).map(|f| f.is_some()),
            Err(())
        );

        sets.update_from_annex_b(&[0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE]);
        let frame = normalize_access_unit(&[0, 0, 0, 1, 0x65, 0x88], Codec::H264, &mut sets)
            .expect("Annex B")
            .expect("frame");
        assert!(frame.keyframe);
        assert_eq!(nal_types(&frame.data), [7, 8, 5]);
        assert_eq!(frame.data[..6], [0, 0, 0, 1, 0x67, 0x42]);

        // An access unit with only new parameter sets updates the cache.
        assert_eq!(
            normalize_access_unit(&[0, 0, 1, 0x67, 0x4D, 0, 0, 1, 0x68, 0xEE], Codec::H264, &mut sets)
                .map(|f| f.is_some()),
            Ok(false)
        );
        let frame = normalize_access_unit(&[0, 0, 1, 0x65, 0x88], Codec::H264, &mut sets)
            .expect("Annex B")
            .expect("frame");
        assert_eq!(
            frame.data,
            [0, 0, 0, 1, 0x67, 0x4D, 0, 0, 0, 1, 0x68, 0xEE, 0, 0, 0, 1, 0x65, 0x88]
        );

        // Not Annex B.
        assert!(normalize_access_unit(&[0, 0, 0, 2, 0x41, 0x9A], Codec::H264, &mut sets).is_err());
    }

    #[test]
    fn normalize_hevc_prepends_vps_sps_pps_on_irap() {
        // HEVC nal_unit_type is in bits 1..6 of a two-byte header: VPS 32
        // (0x40), SPS 33 (0x42), PPS 34 (0x44), IDR_W_RADL 19 (0x26), a trailing
        // delta slice 1 (0x02).
        let hevc_types = |stream: &[u8]| -> Vec<u8> {
            split_annex_b(stream).unwrap().iter().map(|nal| (nal[0] >> 1) & 0x3f).collect()
        };
        let mut sets = ParameterSets::new(Codec::Hevc);
        // An IRAP without parameter sets (no VPS/SPS/PPS cached) is undecodable.
        assert_eq!(
            normalize_access_unit(&[0, 0, 0, 1, 0x26, 0x01, 0xDD], Codec::Hevc, &mut sets).map(|f| f.is_some()),
            Err(())
        );

        let access_unit = [
            0, 0, 0, 1, 0x40, 0x01, 0xAA, // VPS
            0, 0, 0, 1, 0x42, 0x01, 0xBB, // SPS
            0, 0, 0, 1, 0x44, 0x01, 0xCC, // PPS
            0, 0, 0, 1, 0x26, 0x01, 0xDD, // IDR slice
        ];
        let frame = normalize_access_unit(&access_unit, Codec::Hevc, &mut sets)
            .expect("Annex B")
            .expect("frame");
        assert!(frame.keyframe);
        assert_eq!(hevc_types(&frame.data), [32, 33, 34, 19]);

        // A later bare IDR gets the cached VPS/SPS/PPS prepended, in order.
        let bare = normalize_access_unit(&[0, 0, 0, 1, 0x26, 0x01, 0xEE], Codec::Hevc, &mut sets)
            .expect("Annex B")
            .expect("frame");
        assert_eq!(hevc_types(&bare.data), [32, 33, 34, 19]);

        // A delta slice is not a keyframe and carries no parameter sets.
        let delta = normalize_access_unit(&[0, 0, 0, 1, 0x02, 0x01, 0x11], Codec::Hevc, &mut sets)
            .expect("Annex B")
            .expect("frame");
        assert!(!delta.keyframe);
        assert_eq!(hevc_types(&delta.data), [1]);
    }

    #[test]
    fn best_available_encoder_starts_with_a_keyframe() {
        let mut enc = VideoEncoder::best_available(128, 96, 15, 1_000_000, Codec::H264).expect("encoder");
        let output = enc.encode(&synthetic_frame(128, 96, 0)).expect("encodes");
        assert!(
            output.keyframe,
            "{} must start with a keyframe",
            enc.backend()
        );
        assert_eq!(nal_types(&output.data)[..2], [7, 8]);
    }
}
