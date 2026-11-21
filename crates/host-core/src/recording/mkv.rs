//! A small streaming Matroska (.mkv) writer for already-encoded H.264/HEVC
//! video and Opus audio.
//!
//! The Segment and every Cluster are written with an unknown size, the same
//! live layout browsers use for MediaRecorder WebM, so a file cut off by a
//! crash or power loss still plays up to its last complete block. A clean
//! [`MkvWriter::finish`] only fills in the duration.

use std::io::{self, Seek, SeekFrom, Write};

const EBML: u32 = 0x1A45_DFA3;
const EBML_VERSION: u32 = 0x4286;
const EBML_READ_VERSION: u32 = 0x42F7;
const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
const DOC_TYPE: u32 = 0x4282;
const DOC_TYPE_VERSION: u32 = 0x4287;
const DOC_TYPE_READ_VERSION: u32 = 0x4285;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const MUXING_APP: u32 = 0x4D80;
const WRITING_APP: u32 = 0x5741;
const DURATION: u32 = 0x4489;
const VOID: u32 = 0xEC;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_UID: u32 = 0x73C5;
const TRACK_TYPE: u32 = 0x83;
const FLAG_LACING: u32 = 0x9C;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const AUDIO: u32 = 0xE1;
const SAMPLING_FREQUENCY: u32 = 0xB5;
const CHANNELS: u32 = 0x9F;
const SEEK_PRE_ROLL: u32 = 0x56BB;
const CLUSTER: u32 = 0x1F43_B675;
const TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;

/// The size field for a master element whose size is not known yet.
const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
/// A Duration element is 11 bytes (2 ID, 1 size, 8 float); the Void that
/// holds its place until `finish` is the same length.
const DURATION_SLOT: usize = 11;
/// Clusters start well before a block's 16-bit relative timestamp overflows.
const MAX_CLUSTER_SPAN_MS: u64 = 30_000;

pub const VIDEO_TRACK: u64 = 1;
pub const AUDIO_TRACK: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

impl VideoCodec {
    pub fn codec_id(self) -> &'static str {
        match self {
            VideoCodec::H264 => "V_MPEG4/ISO/AVC",
            VideoCodec::Hevc => "V_MPEGH/ISO/HEVC",
        }
    }
}

/// What the file's tracks carry; fixed for the life of one file.
#[derive(Debug, Clone)]
pub struct TrackSetup {
    pub codec: VideoCodec,
    pub width: u16,
    pub height: u16,
    /// The avcC or hvcC decoder configuration record.
    pub codec_private: Vec<u8>,
    /// Add a 48 kHz stereo Opus track.
    pub audio: bool,
}

pub struct MkvWriter<W: Write + Seek> {
    out: W,
    duration_at: u64,
    cluster_ms: Option<u64>,
    last_ms: u64,
    written: u64,
}

impl<W: Write + Seek> MkvWriter<W> {
    /// Writes the EBML header, the Segment start, Info and Tracks.
    pub fn new(mut out: W, setup: &TrackSetup) -> io::Result<Self> {
        let start = out.stream_position()?;
        let mut head = Vec::new();
        let mut ebml = Vec::new();
        uint(&mut ebml, EBML_VERSION, 1);
        uint(&mut ebml, EBML_READ_VERSION, 1);
        uint(&mut ebml, EBML_MAX_ID_LENGTH, 4);
        uint(&mut ebml, EBML_MAX_SIZE_LENGTH, 8);
        element(&mut ebml, DOC_TYPE, b"matroska");
        uint(&mut ebml, DOC_TYPE_VERSION, 4);
        uint(&mut ebml, DOC_TYPE_READ_VERSION, 2);
        element(&mut head, EBML, &ebml);
        id(&mut head, SEGMENT);
        head.extend_from_slice(&UNKNOWN_SIZE);

        let mut info = Vec::new();
        uint(&mut info, TIMESTAMP_SCALE, 1_000_000);
        element(&mut info, MUXING_APP, b"OpenDesk");
        element(&mut info, WRITING_APP, b"OpenDesk");
        let void_in_info = info.len();
        element(&mut info, VOID, &[0; DURATION_SLOT - 2]);
        id(&mut head, INFO);
        size(&mut head, info.len() as u64);
        let duration_at = start + (head.len() + void_in_info) as u64;
        head.extend_from_slice(&info);

        let mut tracks = Vec::new();
        let mut video = Vec::new();
        uint(&mut video, TRACK_NUMBER, VIDEO_TRACK);
        uint(&mut video, TRACK_UID, VIDEO_TRACK);
        uint(&mut video, TRACK_TYPE, 1);
        uint(&mut video, FLAG_LACING, 0);
        element(&mut video, CODEC_ID, setup.codec.codec_id().as_bytes());
        element(&mut video, CODEC_PRIVATE, &setup.codec_private);
        let mut dims = Vec::new();
        uint(&mut dims, PIXEL_WIDTH, u64::from(setup.width));
        uint(&mut dims, PIXEL_HEIGHT, u64::from(setup.height));
        element(&mut video, VIDEO, &dims);
        element(&mut tracks, TRACK_ENTRY, &video);
        if setup.audio {
            let mut audio = Vec::new();
            uint(&mut audio, TRACK_NUMBER, AUDIO_TRACK);
            uint(&mut audio, TRACK_UID, AUDIO_TRACK);
            uint(&mut audio, TRACK_TYPE, 2);
            uint(&mut audio, FLAG_LACING, 0);
            element(&mut audio, CODEC_ID, b"A_OPUS");
            element(&mut audio, CODEC_PRIVATE, &opus_head(2));
            uint(&mut audio, SEEK_PRE_ROLL, 80_000_000);
            let mut params = Vec::new();
            element(&mut params, SAMPLING_FREQUENCY, &48_000f64.to_be_bytes());
            uint(&mut params, CHANNELS, 2);
            element(&mut audio, AUDIO, &params);
            element(&mut tracks, TRACK_ENTRY, &audio);
        }
        element(&mut head, TRACKS, &tracks);
        out.write_all(&head)?;
        Ok(Self { out, duration_at, cluster_ms: None, last_ms: 0, written: head.len() as u64 })
    }

    /// One video access unit in length-prefixed (AVCC/HVCC) form, at `ms`
    /// milliseconds from the start of the file. A keyframe opens a new
    /// cluster, so a cut-off file always resumes cleanly at one.
    pub fn video(&mut self, ms: u64, keyframe: bool, sample: &[u8]) -> io::Result<()> {
        self.block(VIDEO_TRACK, ms, keyframe, keyframe, sample)
    }

    /// One Opus packet at `ms` milliseconds from the start of the file.
    pub fn audio(&mut self, ms: u64, packet: &[u8]) -> io::Result<()> {
        self.block(AUDIO_TRACK, ms, true, false, packet)
    }

    /// Bytes written so far.
    pub fn written(&self) -> u64 { self.written }

    fn block(&mut self, track: u64, ms: u64, keyframe: bool, new_cluster: bool, payload: &[u8]) -> io::Result<()> {
        // Timestamps never run backwards within the file.
        let ms = ms.max(self.last_ms);
        self.last_ms = ms;
        let mut bytes = Vec::with_capacity(payload.len() + 32);
        let cluster = match self.cluster_ms {
            Some(start) if !new_cluster && ms - start <= MAX_CLUSTER_SPAN_MS => start,
            _ => {
                id(&mut bytes, CLUSTER);
                bytes.extend_from_slice(&UNKNOWN_SIZE);
                uint(&mut bytes, TIMESTAMP, ms);
                self.cluster_ms = Some(ms);
                ms
            }
        };
        let relative = (ms - cluster) as i16;
        let mut body = Vec::with_capacity(payload.len() + 4);
        body.push(0x80 | track as u8);
        body.extend_from_slice(&relative.to_be_bytes());
        body.push(if keyframe { 0x80 } else { 0 });
        body.extend_from_slice(payload);
        element(&mut bytes, SIMPLE_BLOCK, &body);
        self.out.write_all(&bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// Writes the duration over its placeholder and flushes. The file is
    /// valid without this; it only lets players show the length up front.
    pub fn finish(mut self) -> io::Result<W> {
        let end = self.out.stream_position()?;
        let mut duration = Vec::with_capacity(DURATION_SLOT);
        element(&mut duration, DURATION, &(self.last_ms as f64).to_be_bytes());
        self.out.seek(SeekFrom::Start(self.duration_at))?;
        self.out.write_all(&duration)?;
        self.out.seek(SeekFrom::Start(end))?;
        self.out.flush()?;
        Ok(self.out)
    }
}

fn id(out: &mut Vec<u8>, id: u32) {
    let bytes = id.to_be_bytes();
    let skip = bytes.iter().take_while(|&&byte| byte == 0).count();
    out.extend_from_slice(&bytes[skip..]);
}

/// An EBML variable-length size, in the fewest bytes that hold it.
fn size(out: &mut Vec<u8>, value: u64) {
    let mut length = 1;
    // All ones is reserved for "unknown", so a length holds up to 2^(7n) - 2.
    while length < 8 && value >= (1u64 << (7 * length)) - 1 {
        length += 1;
    }
    let marked = value | (1u64 << (7 * length));
    out.extend_from_slice(&marked.to_be_bytes()[8 - length..]);
}

fn element(out: &mut Vec<u8>, element_id: u32, payload: &[u8]) {
    id(out, element_id);
    size(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

fn uint(out: &mut Vec<u8>, element_id: u32, value: u64) {
    let bytes = value.to_be_bytes();
    let skip = bytes.iter().take_while(|&&byte| byte == 0).count().min(7);
    element(out, element_id, &bytes[skip..]);
}

/// The Opus identification header Matroska stores as the codec private data.
fn opus_head(channels: u8) -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(channels);
    head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // mapping family: mono or stereo
    head
}

/// The NAL units of an Annex B byte stream, without start codes.
pub fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            starts.push(index + 3);
            index += 3;
        } else {
            index += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (position, &start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).map_or(data.len(), |&next| next - 3);
        let mut nal = &data[start..end];
        // A 4-byte start code leaves its leading zero on the previous unit.
        while let [rest @ .., 0] = nal { nal = rest; }
        if !nal.is_empty() { nals.push(nal); }
    }
    nals
}

/// Rewrites an Annex B access unit with 4-byte length prefixes.
pub fn length_prefixed(data: &[u8]) -> Vec<u8> {
    let nals = annexb_nals(data);
    let mut out = Vec::with_capacity(data.len() + nals.len());
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

fn nal_type(codec: VideoCodec, nal: &[u8]) -> u8 {
    match codec {
        VideoCodec::H264 => nal[0] & 0x1F,
        VideoCodec::Hevc => (nal[0] >> 1) & 0x3F,
    }
}

/// The decoder configuration record (avcC or hvcC) built from the parameter
/// sets in a keyframe, or `None` when the keyframe carries none.
pub fn codec_private(codec: VideoCodec, keyframe: &[u8]) -> Option<Vec<u8>> {
    let nals = annexb_nals(keyframe);
    let find = |kind: u8| nals.iter().copied().find(|nal| nal_type(codec, nal) == kind);
    match codec {
        VideoCodec::H264 => avcc(find(7)?, find(8)?),
        VideoCodec::Hevc => hvcc(find(32)?, find(33)?, find(34)?),
    }
}

fn avcc(sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    if sps.len() < 4 { return None; }
    let mut record = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
    record.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    record.extend_from_slice(sps);
    record.push(1);
    record.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    record.extend_from_slice(pps);
    Some(record)
}

/// Strips emulation prevention bytes (the 3 in 00 00 03).
fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &byte in nal {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.position / 8)?;
        let bit = (byte >> (7 - self.position % 8)) & 1;
        self.position += 1;
        Some(u32::from(bit))
    }

    fn exp_golomb(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 { return None; }
        }
        let mut value = 0;
        for _ in 0..zeros { value = (value << 1) | self.bit()?; }
        Some((1 << zeros) - 1 + value)
    }
}

fn hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    // Two bytes of NAL header, one of VPS id / sub-layers / nesting, then the
    // 12-byte general profile_tier_level.
    let raw = rbsp(sps);
    if raw.len() < 15 { return None; }
    let sub_layers = (raw[2] >> 1) & 0x07;
    let nested = raw[2] & 1;
    let mut chroma_format = 1;
    if sub_layers == 0 {
        // Past the profile, the SPS id and then chroma_format_idc.
        let mut bits = Bits { data: &raw[15..], position: 0 };
        if bits.exp_golomb().is_some() {
            if let Some(chroma) = bits.exp_golomb() { chroma_format = (chroma & 3) as u8; }
        }
    }
    let mut record = vec![1];
    record.extend_from_slice(&raw[3..15]);
    record.extend_from_slice(&[0xF0, 0x00, 0xFC, 0xFC | chroma_format, 0xF8, 0xF8, 0, 0]);
    record.push(((sub_layers + 1) << 3) | (nested << 2) | 3);
    record.push(3);
    for nal in [vps, sps, pps] {
        record.push(0x80 | nal_type(VideoCodec::Hevc, nal));
        record.extend_from_slice(&1u16.to_be_bytes());
        record.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        record.extend_from_slice(nal);
    }
    Some(record)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    /// One parsed EBML element: id, payload (`None` for unknown-size masters).
    pub(crate) struct Element<'a> {
        pub id: u32,
        pub body: &'a [u8],
    }

    fn read_vint(data: &[u8], keep_marker: bool) -> Option<(u64, usize)> {
        let first = *data.first()?;
        let length = first.leading_zeros() as usize + 1;
        if length > 8 || data.len() < length { return None; }
        let mut value = if keep_marker { u64::from(first) } else { u64::from(first) & ((1 << (8 - length)) - 1) };
        for &byte in &data[1..length] { value = (value << 8) | u64::from(byte); }
        Some((value, length))
    }

    /// Parses `data` as a flat run of elements. Unknown-size masters (the
    /// Segment and Clusters) contribute only their header, so their children
    /// show up at the same level, which is how a live reader sees them.
    pub(crate) fn parse(data: &[u8]) -> Vec<Element<'_>> {
        let mut elements = Vec::new();
        let mut at = 0;
        while at < data.len() {
            let (element_id, id_len) = read_vint(&data[at..], true).expect("element id");
            let (length, size_len) = read_vint(&data[at + id_len..], false).expect("element size");
            let start = at + id_len + size_len;
            if size_len == 8 && length == (1 << 56) - 1 {
                elements.push(Element { id: element_id as u32, body: &[] });
                at = start;
                continue;
            }
            let end = start + length as usize;
            assert!(end <= data.len(), "element {element_id:#x} runs past the end");
            elements.push(Element { id: element_id as u32, body: &data[start..end] });
            at = end;
        }
        elements
    }

    pub(crate) fn find<'a>(elements: &'a [Element<'a>], element_id: u32) -> Option<&'a [u8]> {
        elements.iter().find(|element| element.id == element_id).map(|element| element.body)
    }

    fn read_uint(body: &[u8]) -> u64 { body.iter().fold(0, |value, &byte| (value << 8) | u64::from(byte)) }

    /// A plausible H.264 keyframe: SPS, PPS and an IDR slice, 4-byte start codes.
    pub(crate) fn h264_keyframe() -> Vec<u8> {
        let mut unit = vec![0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1E, 0xDA, 0x02, 0x80];
        unit.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80]);
        unit.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33]);
        unit
    }

    #[test]
    fn sizes_and_ids_use_the_shortest_encoding() {
        let mut out = Vec::new();
        size(&mut out, 5);
        size(&mut out, 126);
        size(&mut out, 127);
        assert_eq!(out, [0x85, 0xFE, 0x40, 0x7F]);
        let mut out = Vec::new();
        id(&mut out, SIMPLE_BLOCK);
        id(&mut out, CODEC_PRIVATE);
        id(&mut out, CLUSTER);
        assert_eq!(out, [0xA3, 0x63, 0xA2, 0x1F, 0x43, 0xB6, 0x75]);
    }

    #[test]
    fn annexb_splits_three_and_four_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4];
        assert_eq!(annexb_nals(&data), vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]);
        assert_eq!(length_prefixed(&data[..7]), [0, 0, 0, 3, 0x67, 1, 2]);
    }

    #[test]
    fn avcc_record_comes_from_the_keyframe_parameter_sets() {
        let record = codec_private(VideoCodec::H264, &h264_keyframe()).unwrap();
        assert_eq!(&record[..6], &[1, 0x42, 0xC0, 0x1E, 0xFF, 0xE1]);
        assert_eq!(&record[6..8], &7u16.to_be_bytes());
        assert_eq!(&record[15..18], &[1, 0, 4]);
        assert!(codec_private(VideoCodec::H264, &[0, 0, 0, 1, 0x65, 0x88]).is_none(), "no SPS, no record");
    }

    #[test]
    fn hvcc_record_copies_the_profile_and_lists_three_arrays() {
        let vps = [0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF];
        // NAL header, then sub-layers 0 + nesting, Main profile, compat
        // flags, constraint flags (with an emulation prevention byte), level,
        // then sps id 0 and chroma_format_idc 1 (bits 1, 010).
        let sps = [0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x5D, 0xA0];
        let pps = [0x44, 0x01, 0xC1, 0x72];
        let mut unit = Vec::new();
        for nal in [&vps[..], &sps[..], &pps[..]] {
            unit.extend_from_slice(&[0, 0, 0, 1]);
            unit.extend_from_slice(nal);
        }
        let record = codec_private(VideoCodec::Hevc, &unit).unwrap();
        assert_eq!(record[0], 1);
        assert_eq!(record[1], 0x01, "general profile Main");
        assert_eq!(&record[2..6], &[0x60, 0, 0, 0]);
        assert_eq!(record[12], 0x5D, "level 3.1");
        assert_eq!(record[16], 0xFD, "4:2:0");
        assert_eq!(record[21] & 3, 3, "4-byte lengths");
        assert_eq!(record[22], 3);
        assert_eq!(record[23], 0x80 | 32);
    }

    #[test]
    fn writes_a_parseable_matroska_file() {
        let setup = TrackSetup {
            codec: VideoCodec::H264,
            width: 64,
            height: 48,
            codec_private: codec_private(VideoCodec::H264, &h264_keyframe()).unwrap(),
            audio: true,
        };
        let mut writer = MkvWriter::new(Cursor::new(Vec::new()), &setup).unwrap();
        let keyframe = length_prefixed(&h264_keyframe());
        writer.video(0, true, &keyframe).unwrap();
        writer.audio(5, &[0xFC, 1, 2]).unwrap();
        writer.video(33, false, &[0, 0, 0, 2, 0x41, 0x9A]).unwrap();
        writer.video(1000, true, &keyframe).unwrap();
        writer.video(40_000, false, &[0, 0, 0, 2, 0x41, 0x9A]).unwrap();
        let data = writer.finish().unwrap().into_inner();

        let top = parse(&data);
        let header = parse(find(&top, EBML).unwrap());
        assert_eq!(find(&header, DOC_TYPE), Some(&b"matroska"[..]));
        assert_eq!(top[1].id, SEGMENT);
        let info = parse(find(&top, INFO).unwrap());
        assert_eq!(read_uint(find(&info, TIMESTAMP_SCALE).unwrap()), 1_000_000);
        let duration = f64::from_be_bytes(find(&info, DURATION).unwrap().try_into().unwrap());
        assert_eq!(duration, 40_000.0);
        let tracks = parse(find(&top, TRACKS).unwrap());
        assert_eq!(tracks.len(), 2);
        let video = parse(tracks[0].body);
        assert_eq!(find(&video, CODEC_ID), Some(&b"V_MPEG4/ISO/AVC"[..]));
        assert_eq!(find(&video, CODEC_PRIVATE), Some(&setup.codec_private[..]));
        let dims = parse(find(&video, VIDEO).unwrap());
        assert_eq!(read_uint(find(&dims, PIXEL_WIDTH).unwrap()), 64);
        assert_eq!(read_uint(find(&dims, PIXEL_HEIGHT).unwrap()), 48);
        let audio = parse(tracks[1].body);
        assert_eq!(find(&audio, CODEC_ID), Some(&b"A_OPUS"[..]));

        // Keyframes open clusters, and so does a gap too long for 16 bits.
        let clusters: Vec<u64> = top.windows(2)
            .filter(|pair| pair[0].id == CLUSTER)
            .map(|pair| read_uint(pair[1].body))
            .collect();
        assert_eq!(clusters, [0, 1000, 40_000]);
        let blocks: Vec<&[u8]> = top.iter().filter(|element| element.id == SIMPLE_BLOCK).map(|element| element.body).collect();
        assert_eq!(blocks.len(), 5);
        assert_eq!(&blocks[0][..4], &[0x81, 0, 0, 0x80], "video keyframe at +0");
        assert_eq!(&blocks[1][..4], &[0x82, 0, 5, 0x80], "audio at +5");
        assert_eq!(&blocks[2][..4], &[0x81, 0, 33, 0], "delta frame at +33");
        assert_eq!(&blocks[0][4..], &keyframe[..]);
    }

    #[test]
    fn a_file_cut_off_mid_block_still_parses_up_to_the_cut() {
        let setup = TrackSetup { codec: VideoCodec::H264, width: 64, height: 48, codec_private: vec![1, 2, 3], audio: false };
        let mut writer = MkvWriter::new(Cursor::new(Vec::new()), &setup).unwrap();
        writer.video(0, true, &[0, 0, 0, 1, 0x65]).unwrap();
        let whole = writer.written() as usize;
        writer.video(33, false, &[0, 0, 0, 1, 0x41]).unwrap();
        let data = writer.out.into_inner();
        // No finish, and the last block half written: everything before it
        // is still intact.
        let top = parse(&data[..whole]);
        assert_eq!(top.iter().filter(|element| element.id == SIMPLE_BLOCK).count(), 1);
        assert!(data.len() > whole);
    }
}
