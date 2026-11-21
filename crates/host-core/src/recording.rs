//! Host-side session recording: tees the encoded video and audio a session
//! already sends into Matroska files, without re-encoding.
//!
//! A [`RecordingTap`] hands copies of each sent access unit and Opus packet
//! to a writer thread, so disk I/O never stalls the media task. The writer
//! starts a file at the first keyframe and a new one (`-part2`, `-part3`, ...)
//! whenever the codec or resolution changes, or a file outgrows
//! [`SEGMENT_MAX_BYTES`]. Files close when every tap is dropped, which
//! happens when the session ends.

pub mod mkv;

use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
        Arc,
    },
    time::Instant,
};

use mkv::{MkvWriter, TrackSetup, VideoCodec};

/// Start a new file at the next keyframe once one passes this size.
pub const SEGMENT_MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// Samples buffered toward the writer thread; past this the tap drops video
/// until the next keyframe rather than block the session.
const QUEUE: usize = 256;
/// Bytes of the video header in front of each access unit (see
/// `encode::video_header`).
const VIDEO_HEADER_LEN: usize = 18;
/// Extension of every host recording file.
pub const EXTENSION: &str = "mkv";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingEvent {
    /// A file was created and its first keyframe written.
    Started { path: PathBuf },
    /// A file was closed (session end, or a new segment began).
    Stopped { path: PathBuf, bytes: u64 },
    /// Writing failed; recording stops for the rest of the session.
    Failed { path: Option<PathBuf>, error: String },
}

/// Receives recording events on the writer thread.
pub type RecordingEvents = Arc<dyn Fn(RecordingEvent) + Send + Sync>;

enum Sample {
    Video { ms: u64, unit: Vec<u8> },
    Audio { ms: u64, packet: Vec<u8> },
}

/// The session's handle to a recording. Clones share one writer; the files
/// are finished when the last clone is dropped.
#[derive(Clone)]
pub struct RecordingTap {
    tx: SyncSender<Sample>,
    started: Instant,
    /// Video was dropped on a full queue; skip until the next keyframe.
    gap: Arc<AtomicBool>,
}

impl RecordingTap {
    /// Starts the writer thread. Files go in `dir`, named `<base_name>.mkv`
    /// (later segments `<base_name>-part<n>.mkv`).
    pub fn start(dir: PathBuf, base_name: String, events: RecordingEvents) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        std::thread::Builder::new()
            .name("opendesk-session-recording".into())
            .spawn(move || {
                let mut writer = SegmentWriter { dir, base_name, events, part: 0, open: None, failed: false };
                for sample in rx {
                    writer.write(sample);
                }
                writer.close();
            })?;
        Ok(Self { tx, started: Instant::now(), gap: Arc::new(AtomicBool::new(false)) })
    }

    fn now_ms(&self) -> u64 { self.started.elapsed().as_millis() as u64 }

    /// A video plaintext exactly as sent: the 18-byte header, then an Annex B
    /// access unit.
    pub fn video(&self, plaintext: &[u8]) {
        let keyframe = plaintext.get(13).is_some_and(|flags| flags & 1 == 1);
        if self.gap.load(Ordering::Relaxed) && !keyframe { return; }
        match self.tx.try_send(Sample::Video { ms: self.now_ms(), unit: plaintext.to_vec() }) {
            Ok(()) => self.gap.store(false, Ordering::Relaxed),
            Err(TrySendError::Full(_)) => self.gap.store(true, Ordering::Relaxed),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// One Opus packet as sent to the viewer.
    pub fn audio(&self, frame: &protocol::audio::AudioFrame) {
        let _ = self.tx.try_send(Sample::Audio { ms: self.now_ms(), packet: frame.payload.clone() });
    }
}

struct OpenFile {
    mkv: MkvWriter<File>,
    path: PathBuf,
    codec: u8,
    width: u16,
    height: u16,
    /// Session time of the file's first keyframe; file time 0.
    origin_ms: u64,
}

struct SegmentWriter {
    dir: PathBuf,
    base_name: String,
    events: RecordingEvents,
    part: u32,
    open: Option<OpenFile>,
    failed: bool,
}

impl SegmentWriter {
    fn write(&mut self, sample: Sample) {
        if self.failed { return; }
        let result = match sample {
            Sample::Video { ms, unit } => self.video(ms, &unit),
            Sample::Audio { ms, packet } => match &mut self.open {
                Some(open) => open.mkv.audio(ms.saturating_sub(open.origin_ms), &packet),
                None => Ok(()),
            },
        };
        if let Err(error) = result {
            self.failed = true;
            let path = self.open.take().map(|open| open.path);
            (self.events)(RecordingEvent::Failed { path, error: error.to_string() });
        }
    }

    fn video(&mut self, ms: u64, unit: &[u8]) -> io::Result<()> {
        if unit.len() <= VIDEO_HEADER_LEN { return Ok(()); }
        let codec_byte = unit[12];
        let keyframe = unit[13] & 1 == 1;
        let width = u16::from_be_bytes([unit[14], unit[15]]);
        let height = u16::from_be_bytes([unit[16], unit[17]]);
        let data = &unit[VIDEO_HEADER_LEN..];
        let codec = match codec_byte {
            1 => VideoCodec::H264,
            2 => VideoCodec::Hevc,
            _ => return Ok(()),
        };
        if keyframe {
            let changed = self.open.as_ref().is_none_or(|open| {
                open.codec != codec_byte || open.width != width || open.height != height
                    || open.mkv.written() >= SEGMENT_MAX_BYTES
            });
            if changed {
                if let Some(codec_private) = mkv::codec_private(codec, data) {
                    self.close();
                    let setup = TrackSetup { codec, width, height, codec_private, audio: true };
                    self.open_file(&setup, codec_byte, ms)?;
                }
            }
        }
        let Some(open) = &mut self.open else { return Ok(()) };
        // A non-matching delta frame belongs to a stream the open file can't
        // describe; wait for that stream's keyframe.
        if open.codec != codec_byte || open.width != width || open.height != height { return Ok(()); }
        open.mkv.video(ms.saturating_sub(open.origin_ms), keyframe, &mkv::length_prefixed(data))
    }

    fn open_file(&mut self, setup: &TrackSetup, codec: u8, origin_ms: u64) -> io::Result<()> {
        self.part += 1;
        let (file, path) = create_new(&self.dir, &segment_name(&self.base_name, self.part))?;
        let mkv = MkvWriter::new(file, setup)?;
        (self.events)(RecordingEvent::Started { path: path.clone() });
        self.open = Some(OpenFile { mkv, path, codec, width: setup.width, height: setup.height, origin_ms });
        Ok(())
    }

    fn close(&mut self) {
        let Some(open) = self.open.take() else { return };
        let bytes = open.mkv.written();
        let finished = open.mkv.finish().and_then(|file| file.sync_all());
        match finished {
            Ok(()) => (self.events)(RecordingEvent::Stopped { path: open.path, bytes }),
            Err(error) => (self.events)(RecordingEvent::Failed { path: Some(open.path), error: error.to_string() }),
        }
    }
}

/// `base.mkv` for the first part, `base-part<n>.mkv` after.
pub fn segment_name(base_name: &str, part: u32) -> String {
    if part <= 1 { format!("{base_name}.{EXTENSION}") } else { format!("{base_name}-part{part}.{EXTENSION}") }
}

/// Creates `name` in `dir` without overwriting or following anything that is
/// already there, adding `-2`, `-3`, ... to the stem until a name is free.
fn create_new(dir: &Path, name: &str) -> io::Result<(File, PathBuf)> {
    fs::create_dir_all(dir)?;
    let stem = name.strip_suffix(&format!(".{EXTENSION}")).unwrap_or(name);
    for attempt in 1..1000 {
        let candidate = if attempt == 1 { name.to_owned() } else { format!("{stem}-{attempt}.{EXTENSION}") };
        let path = dir.join(candidate);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free recording file name"))
}

/// The UTC date and time of `unix_seconds` as `YYYY-MM-DD_HH-MM-SS`, for file
/// names (no separators that are illegal on Windows).
pub fn file_timestamp(unix_seconds: u64) -> String {
    let days = (unix_seconds / 86_400) as i64;
    let seconds = unix_seconds % 86_400;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}_{:02}-{:02}-{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::mkv::tests::{find, parse};
    use super::*;
    use crate::encode::{video_header, Codec, Encoder, H264Encoder, PixelFormat, RawFrame};
    use std::sync::Mutex;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("opendesk-host-recording-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn recorder(dir: &Path) -> (RecordingTap, Arc<Mutex<Vec<RecordingEvent>>>, mpsc::Receiver<()>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done_rx) = mpsc::channel();
        let sink = Arc::clone(&seen);
        let done_tx = Mutex::new(done_tx);
        let events: RecordingEvents = Arc::new(move |event| {
            let stopped = matches!(event, RecordingEvent::Stopped { .. } | RecordingEvent::Failed { .. });
            sink.lock().unwrap().push(event);
            if stopped { let _ = done_tx.lock().unwrap().send(()); }
        });
        (RecordingTap::start(dir.to_owned(), "opendesk-host-test".into(), events).unwrap(), seen, done_rx)
    }

    /// Real openh264 frames at `width` x `height`, wrapped like a session sends them.
    fn encoded_units(width: u16, height: u16, count: usize, seq: &mut u64) -> Vec<Vec<u8>> {
        let mut encoder = H264Encoder::new(width, height, 15, 500_000).expect("openh264");
        (0..count)
            .map(|index| {
                let shade = (index * 20) as u8;
                let raw = RawFrame {
                    width,
                    height,
                    data: vec![shade; usize::from(width) * usize::from(height) * 4],
                    format: PixelFormat::Bgra,
                    timestamp_us: index as u32 * 66_000,
                };
                let encoded = encoder.encode(&raw).expect("encode");
                let mut unit = video_header(*seq, 0, encoded.keyframe, width, height, Codec::H264);
                unit.extend_from_slice(&encoded.data);
                *seq += 1;
                unit
            })
            .collect()
    }

    #[test]
    fn records_a_real_h264_stream_and_splits_on_a_resolution_change() {
        let dir = temp_dir("split");
        let (tap, seen, done) = recorder(&dir);
        let mut seq = 0;
        let audio = protocol::audio::AudioFrame { sample_rate: 48_000, channels: 2, frame_duration_ms: 20, payload: vec![0xFC, 0xFF, 0xFE] };
        // Audio before the first keyframe has no file to go in yet.
        tap.audio(&audio);
        for unit in encoded_units(64, 48, 5, &mut seq) {
            tap.video(&unit);
            tap.audio(&audio);
            // Real spacing, so the file has a duration.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        for unit in encoded_units(96, 64, 3, &mut seq) {
            tap.video(&unit);
        }
        drop(tap);
        done.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        done.recv_timeout(std::time::Duration::from_secs(10)).unwrap();

        let events = seen.lock().unwrap().clone();
        let first = dir.join("opendesk-host-test.mkv");
        let second = dir.join("opendesk-host-test-part2.mkv");
        assert!(matches!(&events[0], RecordingEvent::Started { path } if *path == first), "{events:?}");
        assert!(matches!(&events[1], RecordingEvent::Stopped { path, .. } if *path == first), "{events:?}");
        assert!(matches!(&events[2], RecordingEvent::Started { path } if *path == second), "{events:?}");
        assert!(matches!(&events[3], RecordingEvent::Stopped { path, bytes } if *path == second && *bytes > 0), "{events:?}");

        let data = fs::read(&first).unwrap();
        let top = parse(&data);
        let tracks = parse(find(&top, 0x1654_AE6B).unwrap());
        let video = parse(tracks[0].body);
        assert_eq!(find(&video, 0x86), Some(&b"V_MPEG4/ISO/AVC"[..]));
        let avcc = find(&video, 0x63A2).unwrap();
        assert_eq!(avcc[0], 1, "avcC version");
        let blocks: Vec<&[u8]> = top.iter().filter(|element| element.id == 0xA3).map(|element| element.body).collect();
        assert_eq!(blocks.iter().filter(|block| block[0] == 0x81).count(), 5);
        assert_eq!(blocks.iter().filter(|block| block[0] == 0x82).count(), 5);
        assert_eq!(blocks[0][3], 0x80, "starts on a keyframe");
        // The first sample's first NAL is length-prefixed, not a start code.
        let first_len = u32::from_be_bytes(blocks[0][4..8].try_into().unwrap()) as usize;
        assert!(first_len > 0 && 8 + first_len <= blocks[0].len());

        let second_data = fs::read(&second).unwrap();
        let second_top = parse(&second_data);
        let tracks = parse(find(&second_top, 0x1654_AE6B).unwrap());
        let video = parse(tracks[0].body);
        let dims = parse(find(&video, 0xE0).unwrap());
        assert_eq!(find(&dims, 0xB0), Some(&[96u8][..]));

        if let Ok(keep) = std::env::var("OPENDESK_KEEP_RECORDING") {
            fs::copy(&first, keep).unwrap();
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn video_before_the_first_keyframe_is_skipped() {
        let dir = temp_dir("delta");
        let (tap, seen, _done) = recorder(&dir);
        let mut unit = video_header(0, 0, false, 64, 48, Codec::H264);
        unit.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A]);
        tap.video(&unit);
        drop(tap);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(seen.lock().unwrap().is_empty());
        assert!(!dir.join("opendesk-host-test.mkv").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn names_never_overwrite_an_existing_file() {
        let dir = temp_dir("names");
        let (_, first) = create_new(&dir, "a.mkv").unwrap();
        let (_, second) = create_new(&dir, "a.mkv").unwrap();
        assert_eq!(first.file_name().unwrap(), "a.mkv");
        assert_eq!(second.file_name().unwrap(), "a-2.mkv");
        assert_eq!(segment_name("x", 1), "x.mkv");
        assert_eq!(segment_name("x", 3), "x-part3.mkv");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn file_timestamps_are_utc_calendar_dates() {
        assert_eq!(file_timestamp(0), "1970-01-01_00-00-00");
        assert_eq!(file_timestamp(951_782_400), "2000-02-29_00-00-00");
        assert_eq!(file_timestamp(1_795_000_000), "2026-11-18_11-06-40");
    }
}
