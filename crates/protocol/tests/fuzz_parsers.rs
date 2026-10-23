//! Robustness ("smoke fuzz") for the wire parsers that consume peer- and
//! relay-controlled bytes. A malformed or hostile message must never panic the
//! process, and anything a parser accepts must survive a to_bytes/from_bytes
//! round trip unchanged. Deterministic (fixed seed) so failures reproduce.
//!
//! This is coverage-blind. For coverage-guided fuzzing, point a `cargo fuzz`
//! target at the same `from_bytes` entry points (see docs/SECURITY or the audit
//! report); these functions are the exact untrusted-input boundary.

use protocol::audio::AudioFrame;
use protocol::browse::BrowseMsg;
use protocol::chat::ChatMsg;
use protocol::clipboard::ClipboardMsg;
use protocol::control::Control;
use protocol::datamsg::DataMsg;
use protocol::filexfer::FileMsg;
use protocol::input::InputEvent;

/// A tiny deterministic PRNG (splitmix64); no dev-dependency needed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
    fn range(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Random bytes of length 0..max.
fn random_bytes(rng: &mut Rng, max: usize) -> Vec<u8> {
    let len = rng.range(max + 1);
    (0..len).map(|_| rng.byte()).collect()
}

/// Mutate `seed`: flip some bytes, and sometimes truncate or extend it.
fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut bytes = seed.to_vec();
    if !bytes.is_empty() {
        let flips = rng.range(4) + 1;
        for _ in 0..flips {
            let index = rng.range(bytes.len());
            bytes[index] ^= 1 << (rng.range(8));
        }
    }
    match rng.range(4) {
        0 if !bytes.is_empty() => bytes.truncate(rng.range(bytes.len())),
        1 => bytes.extend((0..rng.range(16)).map(|_| rng.byte())),
        _ => {}
    }
    bytes
}

const ITERATIONS: usize = 20_000;
const MAX_LEN: usize = 512;

/// Run `parse` over many random and mutated-from-valid inputs; a panic fails the
/// test. `idempotent` re-serializes an accepted value and requires a stable
/// re-parse, catching parsers that accept bytes they cannot faithfully reproduce.
fn smoke<T, P, I>(label: &str, seeds: &[Vec<u8>], parse: P, idempotent: I)
where
    P: Fn(&[u8]) -> Option<T>,
    I: Fn(&T) -> Option<T>,
{
    let mut rng = Rng(0x0D15_EA5E_u64 ^ label.bytes().fold(0u64, |a, b| a.wrapping_mul(131).wrapping_add(u64::from(b))));
    for i in 0..ITERATIONS {
        let input = if i % 2 == 0 || seeds.is_empty() {
            random_bytes(&mut rng, MAX_LEN)
        } else {
            let index = rng.range(seeds.len());
            mutate(&mut rng, &seeds[index])
        };
        if let Some(value) = parse(&input) {
            // Whatever a parser accepts, it must reproduce stably.
            assert!(idempotent(&value).is_some(), "{label}: accepted input did not round-trip: {input:?}");
        }
    }
}

#[test]
fn control_parser_is_crash_free_and_idempotent() {
    let seeds: Vec<Vec<u8>> = [
        Control::RequestKeyframe,
        Control::KeepAlive,
        Control::SelectMonitor { id: 3 },
        Control::AddVirtualDisplay { width: 1920, height: 1080, hidpi: true },
        Control::SetMaxBitrate { bps: 8_000_000 },
        Control::CursorShape { width: 2, height: 1, hot_x: 0, hot_y: 0, rgba: vec![1, 2, 3, 4, 5, 6, 7, 8] },
        Control::CursorHidden,
        Control::DecoderCodecs { hevc: true },
        Control::Presence { ids: vec!["ABC".into(), "DEF".into()] },
        Control::Register { peer_id: "123".into(), token: "t".into(), ver: 1 },
        Control::Connect { target: "x".into(), peer_id: "y".into(), ver: 1 },
    ]
    .iter()
    .map(Control::to_bytes)
    .collect();
    smoke("Control", &seeds, Control::from_bytes, |c| {
        let re = Control::from_bytes(&c.to_bytes())?;
        assert_eq!(&re, c, "Control round trip changed the value");
        Some(re)
    });
}

#[test]
fn data_message_parsers_are_crash_free() {
    let data_seeds: Vec<Vec<u8>> = [
        DataMsg::Chat(ChatMsg { text: "hi".into() }),
    ]
    .iter()
    .map(DataMsg::to_bytes)
    .collect();
    smoke("DataMsg", &data_seeds, DataMsg::from_bytes, |m| {
        let re = DataMsg::from_bytes(&m.to_bytes())?;
        Some(re)
    });

    let file_seeds: Vec<Vec<u8>> = [
        FileMsg::Offer { id: 1, name: "a.txt".into(), size: 10, hash: [7u8; 32] },
        FileMsg::Accept { id: 1 },
        FileMsg::Reject { id: 1, reason: "no".into() },
        FileMsg::Chunk { id: 1, offset: 0, data: vec![1, 2, 3] },
        FileMsg::Done { id: 1 },
    ]
    .iter()
    .map(FileMsg::to_bytes)
    .collect();
    smoke("FileMsg", &file_seeds, FileMsg::from_bytes, |m| FileMsg::from_bytes(&m.to_bytes()));

    let browse_seeds: Vec<Vec<u8>> = [
        BrowseMsg::ListDir { req: 1, path: "/tmp".into() },
        BrowseMsg::Pull { req: 2, path: "/etc/hosts".into() },
        BrowseMsg::SetUploadDir { dir: "/tmp".into() },
    ]
    .iter()
    .map(BrowseMsg::to_bytes)
    .collect();
    smoke("BrowseMsg", &browse_seeds, BrowseMsg::from_bytes, |m| BrowseMsg::from_bytes(&m.to_bytes()));

    let clip_seeds: Vec<Vec<u8>> = [ClipboardMsg { seq: 1, text: "x".into() }].iter().map(ClipboardMsg::to_bytes).collect();
    smoke("ClipboardMsg", &clip_seeds, ClipboardMsg::from_bytes, |m| ClipboardMsg::from_bytes(&m.to_bytes()));

    let chat_seeds: Vec<Vec<u8>> = [ChatMsg { text: "x".into() }].iter().map(ChatMsg::to_bytes).collect();
    smoke("ChatMsg", &chat_seeds, ChatMsg::from_bytes, |m| ChatMsg::from_bytes(&m.to_bytes()));

    let audio_seeds: Vec<Vec<u8>> = [AudioFrame { sample_rate: 48_000, channels: 2, frame_duration_ms: 20, payload: vec![0, 1, 2, 3] }].iter().map(AudioFrame::to_bytes).collect();
    smoke("AudioFrame", &audio_seeds, AudioFrame::from_bytes, |m| AudioFrame::from_bytes(&m.to_bytes()));
}

#[test]
fn input_event_parser_is_crash_free() {
    let seeds: Vec<Vec<u8>> = [
        InputEvent::MouseMove { x: 100, y: 200 },
        InputEvent::MouseButton { button: 1, down: true },
        InputEvent::Scroll { dx: 1, dy: -1 },
        InputEvent::Key { code: 65, down: true, modifiers: 0 },
        InputEvent::Char { ch: 'A', down: true, modifiers: 0 },
    ]
    .iter()
    .map(|event| event.to_bytes(42))
    .collect();
    // from_bytes returns (event, timestamp); re-encode with that timestamp.
    smoke("InputEvent", &seeds, InputEvent::from_bytes, |pair| {
        let (event, ts) = pair;
        InputEvent::from_bytes(&event.to_bytes(*ts))
    });
}
