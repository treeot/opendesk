//! The viewer side of an OpenDesk session, for the browser.
//!
//! A sans-IO state machine: the page owns the WebSocket and the clock, this
//! crate owns pairing, SPAKE2, key confirmation and sealing. It follows
//! `host_core::roles::run_viewer_session` step for step and uses `protocol`
//! for all crypto, so a browser is just another viewer to the host.
//!
//! Media always goes through the relay. A browser cannot join the UDP direct
//! path, so the host's ICE signals are ignored and the host falls back to
//! the relay when its ICE deadline passes.

pub mod filexfer;
pub mod reconnect;
#[cfg(target_arch = "wasm32")]
mod wasm;

use filexfer::{FileEvent, Transfers};
use protocol::{
    audio::AudioFrame,
    clipboard::{ClipboardMsg, MAX_CLIPBOARD},
    control::Control,
    crypto::{Opener, Sealer},
    datamsg::DataMsg,
    filexfer::FileMsg,
    frame, kex, pake, Tag,
};
use reconnect::{rejection_error, HOST_ENDED_MESSAGE};

const KEX_MSG: u8 = 0x01;
const KEX_CONFIRM: u8 = 0x02;
/// The host asks to redo the exchange with its next password (it accepts more
/// than one). Mirrors host-core; older hosts never send it.
const KEX_RETRY: u8 = 0x03;
/// A viewer tries at most this many passwords, matching the host's largest
/// accepted set (unattended + session).
const MAX_KEX_ATTEMPTS: usize = 2;
/// Both SPAKE2 exchanges must finish this soon after pairing.
pub const KEX_DEADLINE_MS: f64 = 10_000.0;
/// How often the viewer sends a sealed keepalive once authenticated.
pub const KEEPALIVE_INTERVAL_MS: f64 = 15_000.0;
/// The host is gone after this long without a control frame.
pub const DEAD_PEER_TIMEOUT_MS: f64 = 45_000.0;
/// How long to wait for a requested keyframe before asking again.
const KEYFRAME_RETRY_MS: f64 = 500.0;
/// Bytes of video header before the access unit (see `encode::video_header`).
pub const VIDEO_HEADER_LEN: usize = 18;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A binary WebSocket message for the relay.
    Send(Vec<u8>),
    /// Key confirmation passed. The host may still ask its user to accept.
    Authenticated,
    /// Opened video: the 18-byte header, then an H.264 access unit.
    Video(Vec<u8>),
    /// Opened audio: one Opus packet (48 kHz, 20 ms) and its channel count.
    Audio(Vec<u8>, u8),
    /// The host's monitors as a JSON array of `MonitorInfo`.
    Monitors(String),
    /// Inbound clipboard text from the host (already capped at 1 MB).
    Clipboard(String),
    /// A file-transfer update for the page (offers, progress, results).
    File(FileEvent),
    /// The session is over. The text matches the desktop viewer's errors,
    /// so [`reconnect::after_failure`] can decide what to do next.
    Ended(String),
}

/// The relay `connect` frame that starts a session with `target`.
pub fn connect_frame(target: &str, viewer_id: &str) -> Result<Vec<u8>, String> {
    let nine_digits = |id: &str| id.len() == 9 && id.bytes().all(|byte| byte.is_ascii_digit());
    if !nine_digits(target) {
        return Err("target ID must be 9 digits".into());
    }
    if !nine_digits(viewer_id) {
        return Err("peer ID must be 9 digits".into());
    }
    let connect = Control::Connect { target: target.into(), peer_id: viewer_id.into(), ver: 1 };
    Ok(frame::wrap(Tag::Control, &connect.to_bytes()))
}

/// Viewer-side continuity check of the video sequence, as in host-core: after
/// a gap, frames wait for the next keyframe and the host is asked for one.
#[derive(Debug)]
struct VideoGate {
    next_seq: Option<u64>,
    awaiting_keyframe: bool,
    last_request: Option<f64>,
}

impl VideoGate {
    fn new() -> Self { Self { next_seq: None, awaiting_keyframe: true, last_request: None } }

    fn accept(&mut self, seq: u64, keyframe: bool) -> bool {
        if self.next_seq.is_some_and(|next| seq < next) {
            return false;
        }
        if keyframe {
            self.awaiting_keyframe = false;
        } else if self.awaiting_keyframe || self.next_seq.is_some_and(|next| seq != next) {
            self.awaiting_keyframe = true;
            return false;
        }
        self.next_seq = Some(seq + 1);
        true
    }

    fn should_request(&mut self, now_ms: f64) -> bool {
        if !self.awaiting_keyframe || self.last_request.is_some_and(|at| now_ms - at < KEYFRAME_RETRY_MS) {
            return false;
        }
        self.last_request = Some(now_ms);
        true
    }
}

fn video_seq(plaintext: &[u8]) -> Option<(u64, bool)> {
    let header = plaintext.get(..VIDEO_HEADER_LEN)?;
    Some((u64::from_be_bytes(header[..8].try_into().ok()?), header[13] & 1 == 1))
}

/// Keys and timers of an authenticated session.
struct Live {
    sealer: Sealer,
    opener: Opener,
    control_sealer: Sealer,
    control_opener: Opener,
    data_sealer: Sealer,
    data_opener: Opener,
    clip_seq: u64,
    transfers: Transfers,
    gate: VideoGate,
    next_keepalive: f64,
    peer_deadline: f64,
}

impl Live {
    fn control(&mut self, control: &Control) -> Vec<u8> {
        frame::wrap(Tag::Control, &self.control_sealer.seal(&[Tag::Control as u8], &control.to_bytes()))
    }

    /// Seal a `DataMsg` for the reliable Data channel.
    fn data(&mut self, message: &DataMsg) -> Vec<u8> {
        frame::wrap(Tag::Data, &self.data_sealer.seal(&[Tag::Data as u8], &message.to_bytes()))
    }

    /// Seal one file-transfer message for the Data channel.
    fn file(&mut self, message: FileMsg) -> Vec<u8> {
        self.data(&DataMsg::FileTransfer(message))
    }
}

enum Phase {
    Pairing,
    KexMessage(pake::PakeState),
    KexConfirm([u8; 32]),
    /// Our password was not the one the host tried; waiting to see if it offers
    /// another before giving up.
    KexAwaitRetry,
    Session(Box<Live>),
    Ended,
}

pub struct ViewerSession {
    password: String,
    auth_code: String,
    phase: Phase,
    kex_deadline: f64,
    kex_attempts: usize,
}

impl ViewerSession {
    /// A session that has sent [`connect_frame`] and waits for the relay to
    /// pair it. An empty `auth_code` means none.
    pub fn new(password: &str, auth_code: &str) -> Self {
        Self { password: password.to_owned(), auth_code: auth_code.trim().to_owned(), phase: Phase::Pairing, kex_deadline: f64::INFINITY, kex_attempts: 0 }
    }

    /// Begin (or redo) the key exchange with our one password, bounded by
    /// [`MAX_KEX_ATTEMPTS`]. Sends our SPAKE2 message.
    fn start_kex_attempt(&mut self, now_ms: f64) -> Vec<Event> {
        let (state, my_message) = pake::start(self.password.as_bytes(), false);
        self.kex_attempts += 1;
        self.phase = Phase::KexMessage(state);
        self.kex_deadline = now_ms + KEX_DEADLINE_MS;
        vec![Event::Send(kex_frame(KEX_MSG, &my_message))]
    }

    pub fn is_ended(&self) -> bool { matches!(self.phase, Phase::Ended) }

    pub fn is_authenticated(&self) -> bool { matches!(self.phase, Phase::Session(_)) }

    fn end(&mut self, message: impl Into<String>) -> Vec<Event> {
        self.phase = Phase::Ended;
        vec![Event::Ended(message.into())]
    }

    /// Handle one binary message from the relay.
    pub fn receive(&mut self, message: &[u8], now_ms: f64) -> Vec<Event> {
        match std::mem::replace(&mut self.phase, Phase::Ended) {
            Phase::Ended => Vec::new(),
            Phase::Pairing => self.on_pairing(message, now_ms),
            Phase::KexMessage(state) => {
                match frame::unwrap(message) {
                    Some((Tag::Kex, [KEX_MSG, peer @ ..])) => {
                        let Some(shared) = pake::finish(state, peer) else { return self.end("invalid SPAKE2 message") };
                        let confirm = pake::confirm_tag(&shared, false);
                        self.phase = Phase::KexConfirm(shared);
                        vec![Event::Send(kex_frame(KEX_CONFIRM, &confirm))]
                    }
                    _ => {
                        self.phase = Phase::KexMessage(state);
                        Vec::new()
                    }
                }
            }
            Phase::KexConfirm(shared) => {
                match frame::unwrap(message) {
                    Some((Tag::Kex, [KEX_CONFIRM, tag @ ..])) => {
                        if pake::verify_peer(&shared, true, tag) {
                            return self.authenticated(&shared, now_ms);
                        }
                        // Our password was wrong for this attempt. Wait for the
                        // host to offer another one, unless we are out of tries.
                        if self.kex_attempts < MAX_KEX_ATTEMPTS {
                            self.phase = Phase::KexAwaitRetry;
                            Vec::new()
                        } else {
                            self.end("key confirmation failed")
                        }
                    }
                    Some((Tag::Kex, [KEX_RETRY, ..])) if self.kex_attempts < MAX_KEX_ATTEMPTS => self.start_kex_attempt(now_ms),
                    _ => {
                        self.phase = Phase::KexConfirm(shared);
                        Vec::new()
                    }
                }
            }
            Phase::KexAwaitRetry => {
                match frame::unwrap(message) {
                    Some((Tag::Kex, [KEX_RETRY, ..])) if self.kex_attempts < MAX_KEX_ATTEMPTS => self.start_kex_attempt(now_ms),
                    Some((Tag::Kex, [KEX_RETRY, ..])) => self.end("key confirmation failed"),
                    _ => {
                        self.phase = Phase::KexAwaitRetry;
                        Vec::new()
                    }
                }
            }
            Phase::Session(mut live) => {
                let events = self.on_session(&mut live, message, now_ms);
                // `end` already set Phase::Ended; otherwise the session goes on.
                if !events.iter().any(|event| matches!(event, Event::Ended(_))) { self.phase = Phase::Session(live); }
                events
            }
        }
    }

    fn on_pairing(&mut self, message: &[u8], now_ms: f64) -> Vec<Event> {
        let Some((Tag::Control, body)) = frame::unwrap(message) else { return self.end("invalid pairing message") };
        match Control::from_bytes(body) {
            Some(Control::Paired { .. }) => self.start_kex_attempt(now_ms),
            Some(Control::Error { code }) => self.end(reconnect::connect_refusal_message(&code).unwrap_or("invalid pairing message")),
            _ => self.end("invalid pairing message"),
        }
    }

    fn authenticated(&mut self, shared: &[u8; 32], now_ms: f64) -> Vec<Event> {
        let (send_key, recv_key) = kex::derive_keys(shared, false);
        let (control_send_key, control_recv_key) = kex::derive_control_keys(shared, false);
        let (data_send_key, data_recv_key) = kex::derive_data_keys(shared, false);
        let mut live = Box::new(Live {
            sealer: Sealer::new(send_key),
            opener: Opener::new(recv_key),
            control_sealer: Sealer::new(control_send_key),
            control_opener: Opener::strict(control_recv_key),
            data_sealer: Sealer::new(data_send_key),
            data_opener: Opener::strict(data_recv_key),
            clip_seq: 0,
            transfers: Transfers::default(),
            gate: VideoGate::new(),
            next_keepalive: now_ms + KEEPALIVE_INTERVAL_MS,
            peer_deadline: now_ms + DEAD_PEER_TIMEOUT_MS,
        });
        // Always sent, empty when there is no code, so a 2FA host answers
        // at once; other hosts ignore it. It must be the first control frame.
        let auth = live.control(&Control::AuthCode { code: self.auth_code.clone() });
        self.phase = Phase::Session(live);
        vec![Event::Send(auth), Event::Authenticated]
    }

    fn on_session(&mut self, live: &mut Live, message: &[u8], now_ms: f64) -> Vec<Event> {
        match frame::unwrap(message) {
            Some((Tag::Control, sealed)) => {
                // Plaintext ICE signals fail to open and are dropped here.
                let Some(plaintext) = live.control_opener.open(&[Tag::Control as u8], sealed) else { return Vec::new() };
                live.peer_deadline = now_ms + DEAD_PEER_TIMEOUT_MS;
                match Control::from_bytes(&plaintext) {
                    Some(Control::Error { code }) => self.end(rejection_error(&code)),
                    Some(Control::Bye { .. }) => self.end(HOST_ENDED_MESSAGE),
                    Some(Control::MonitorList { monitors }) => {
                        vec![Event::Monitors(serde_json::to_string(&monitors).unwrap_or_else(|_| "[]".into()))]
                    }
                    _ => Vec::new(),
                }
            }
            Some((Tag::Video, sealed)) => {
                let Some(plaintext) = live.opener.open(&[Tag::Video as u8], sealed) else { return Vec::new() };
                let Some((seq, keyframe)) = video_seq(&plaintext) else { return Vec::new() };
                let mut events = Vec::new();
                if live.gate.accept(seq, keyframe) {
                    events.push(Event::Video(plaintext));
                }
                if live.gate.should_request(now_ms) {
                    events.push(Event::Send(live.control(&Control::RequestKeyframe)));
                }
                events
            }
            Some((Tag::Data, sealed)) => {
                let Some(plaintext) = live.data_opener.open(&[Tag::Data as u8], sealed) else { return Vec::new() };
                live.peer_deadline = now_ms + DEAD_PEER_TIMEOUT_MS;
                match DataMsg::from_bytes(&plaintext) {
                    Some(DataMsg::Clipboard(clip)) => vec![Event::Clipboard(clip.text)],
                    Some(DataMsg::FileTransfer(message)) => {
                        let (sends, file_events) = live.transfers.on_message(message);
                        let mut events: Vec<Event> = sends.into_iter().map(|msg| Event::Send(live.file(msg))).collect();
                        events.extend(file_events.into_iter().map(Event::File));
                        events
                    }
                    // Chat is not surfaced in the browser viewer.
                    _ => Vec::new(),
                }
            }
            Some((Tag::Audio, sealed)) => {
                // Audio shares the media opener (one host-side sealer seals
                // both video and audio), so opening here keeps replay state.
                let Some(plaintext) = live.opener.open(&[Tag::Audio as u8], sealed) else { return Vec::new() };
                live.peer_deadline = now_ms + DEAD_PEER_TIMEOUT_MS;
                match AudioFrame::from_bytes(&plaintext) {
                    Some(frame) => vec![Event::Audio(frame.payload, frame.channels)],
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    /// The relay socket closed.
    pub fn closed(&mut self) -> Vec<Event> {
        let message = match self.phase {
            Phase::Ended => return Vec::new(),
            Phase::Pairing => "relay closed while connecting",
            Phase::KexMessage(_) => "connection closed during KEX",
            // A host that rejects our confirm closes at once, and its own
            // confirm can be lost in that close (beta.5 aborts its writer).
            // So a close here almost always means a wrong password. The
            // desktop viewer reads it as a drop and retries for 30 s.
            Phase::KexConfirm(_) => "key confirmation failed",
            Phase::KexAwaitRetry => "key confirmation failed",
            Phase::Session(_) => "connection closed during session",
        };
        self.end(message)
    }

    /// Run timers: keepalives and the KEX and dead-peer deadlines. Call it
    /// every second or so.
    pub fn tick(&mut self, now_ms: f64) -> Vec<Event> {
        match &mut self.phase {
            Phase::KexMessage(_) if now_ms >= self.kex_deadline => self.end("connection closed during KEX"),
            Phase::KexConfirm(_) if now_ms >= self.kex_deadline => self.end("connection closed during KEX confirmation"),
            Phase::KexAwaitRetry if now_ms >= self.kex_deadline => self.end("key confirmation failed"),
            Phase::Session(live) => {
                if now_ms >= live.peer_deadline {
                    return self.end(format!(
                        "connection closed: host stopped responding for {}s",
                        (DEAD_PEER_TIMEOUT_MS / 1000.0) as u64
                    ));
                }
                if now_ms >= live.next_keepalive {
                    live.next_keepalive = now_ms + KEEPALIVE_INTERVAL_MS;
                    return vec![Event::Send(live.control(&Control::KeepAlive))];
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn live(&mut self) -> Option<&mut Live> {
        match &mut self.phase {
            Phase::Session(live) => Some(live),
            _ => None,
        }
    }

    /// Seal an encoded `protocol::input::InputEvent`. `None` before the
    /// session is authenticated.
    pub fn input(&mut self, event: &[u8]) -> Option<Vec<u8>> {
        let live = self.live()?;
        Some(frame::wrap(Tag::Input, &live.sealer.seal(&[Tag::Input as u8], event)))
    }

    /// Seal local clipboard text for the host. Text only, capped at 1 MB:
    /// oversize text is dropped (`None`). `None` before authentication.
    pub fn clipboard(&mut self, text: &str) -> Option<Vec<u8>> {
        if text.len() > MAX_CLIPBOARD {
            return None;
        }
        let live = self.live()?;
        live.clip_seq += 1;
        let message = DataMsg::Clipboard(ClipboardMsg { seq: live.clip_seq, text: text.to_owned() });
        Some(live.data(&message))
    }

    /// Offer a local file to the host under the page-chosen `id` (unique and
    /// below 2^53). Returns the sealed Offer, or `None`.
    pub fn send_file(&mut self, id: u64, name: &str, data: Vec<u8>) -> Option<Vec<u8>> {
        let live = self.live()?;
        let offer = live.transfers.send_file(id, name, data)?;
        Some(live.file(offer))
    }

    /// Accept an incoming offer. Returns the sealed Accept, or `None`.
    pub fn accept_file(&mut self, id: u64) -> Option<Vec<u8>> {
        let live = self.live()?;
        let accept = live.transfers.accept(id)?;
        Some(live.file(accept))
    }

    /// Reject an incoming offer. Returns the sealed Reject, or `None`.
    pub fn reject_file(&mut self, id: u64) -> Option<Vec<u8>> {
        let live = self.live()?;
        let reject = live.transfers.reject(id)?;
        Some(live.file(reject))
    }

    /// Ask the host for a keyframe, e.g. after a decoder error.
    pub fn request_keyframe(&mut self) -> Option<Vec<u8>> {
        Some(self.live()?.control(&Control::RequestKeyframe))
    }

    pub fn select_monitor(&mut self, id: u32) -> Option<Vec<u8>> {
        Some(self.live()?.control(&Control::SelectMonitor { id }))
    }

    /// A sealed goodbye so the host ends its session at once. Ends this one.
    pub fn bye(&mut self) -> Option<Vec<u8>> {
        let bye = self.live()?.control(&Control::Bye { session: String::new() });
        self.phase = Phase::Ended;
        Some(bye)
    }
}

fn kex_frame(sub: u8, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(1 + payload.len());
    body.push(sub);
    body.extend_from_slice(payload);
    frame::wrap(Tag::Kex, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paired() -> Vec<u8> {
        frame::wrap(Tag::Control, &Control::Paired { session: "s1".into(), peer_role: "viewer".into(), peer_id: "123456789".into() }.to_bytes())
    }

    fn sent(events: &[Event]) -> Vec<Vec<u8>> {
        events.iter().filter_map(|event| match event { Event::Send(bytes) => Some(bytes.clone()), _ => None }).collect()
    }

    /// A host-side mirror of roles.rs, enough to authenticate a viewer.
    struct FakeHost { shared: [u8; 32], control_sealer: Sealer, control_opener: Opener, sealer: Sealer, opener: Opener, data_sealer: Sealer, data_opener: Opener }

    fn authenticate(viewer: &mut ViewerSession, host_password: &str) -> Result<FakeHost, Vec<Event>> {
        let out = viewer.receive(&paired(), 0.0);
        let viewer_kex = sent(&out).remove(0);
        let (state, host_msg) = pake::start(host_password.as_bytes(), true);
        let (Tag::Kex, [KEX_MSG, viewer_msg @ ..]) = frame::unwrap(&viewer_kex).unwrap() else { panic!("expected a KEX message") };
        let shared = pake::finish(state, viewer_msg).unwrap();
        let out = viewer.receive(&kex_frame(KEX_MSG, &host_msg), 1.0);
        let viewer_confirm = sent(&out).remove(0);
        let (Tag::Kex, [KEX_CONFIRM, tag @ ..]) = frame::unwrap(&viewer_confirm).unwrap() else { panic!("expected a confirm") };
        assert_eq!(pake::verify_peer(&shared, false, tag), host_password == viewer.password);
        let out = viewer.receive(&kex_frame(KEX_CONFIRM, &pake::confirm_tag(&shared, true)), 2.0);
        if !out.contains(&Event::Authenticated) {
            // Wrong password: the viewer now waits for a possible retry, so a
            // single-password host ends it by closing. Surface that.
            return Err(if viewer.is_ended() { out } else { viewer.closed() });
        }
        let (send, recv) = kex::derive_keys(&shared, true);
        let (control_send, control_recv) = kex::derive_control_keys(&shared, true);
        let (data_send, data_recv) = kex::derive_data_keys(&shared, true);
        let mut host = FakeHost { shared, control_sealer: Sealer::new(control_send), control_opener: Opener::new(control_recv), sealer: Sealer::new(send), opener: Opener::new(recv), data_sealer: Sealer::new(data_send), data_opener: Opener::new(data_recv) };
        let auth = sent(&out).remove(0);
        let (Tag::Control, sealed) = frame::unwrap(&auth).unwrap() else { panic!() };
        let plain = host.control_opener.open(&[Tag::Control as u8], sealed).unwrap();
        assert!(matches!(Control::from_bytes(&plain), Some(Control::AuthCode { .. })));
        assert!(out.contains(&Event::Authenticated));
        Ok(host)
    }

    impl FakeHost {
        fn control(&mut self, control: &Control) -> Vec<u8> {
            frame::wrap(Tag::Control, &self.control_sealer.seal(&[Tag::Control as u8], &control.to_bytes()))
        }
        fn video(&mut self, seq: u64, keyframe: bool) -> Vec<u8> {
            let mut plain = seq.to_be_bytes().to_vec();
            plain.extend_from_slice(&[0, 0, 0, 0, 1, u8::from(keyframe), 0, 64, 0, 64, 0, 0, 0, 1, 0x65]);
            frame::wrap(Tag::Video, &self.sealer.seal(&[Tag::Video as u8], &plain))
        }
        fn audio(&mut self, payload: Vec<u8>, channels: u8) -> Vec<u8> {
            let frame = AudioFrame { sample_rate: 48_000, channels, frame_duration_ms: 20, payload };
            frame::wrap(Tag::Audio, &self.sealer.seal(&[Tag::Audio as u8], &frame.to_bytes()))
        }
        fn open_control(&mut self, message: &[u8]) -> Option<Control> {
            let (Tag::Control, sealed) = frame::unwrap(message)? else { return None };
            Control::from_bytes(&self.control_opener.open(&[Tag::Control as u8], sealed)?)
        }
        fn data(&mut self, message: &DataMsg) -> Vec<u8> {
            frame::wrap(Tag::Data, &self.data_sealer.seal(&[Tag::Data as u8], &message.to_bytes()))
        }
        fn open_data(&mut self, message: &[u8]) -> Option<DataMsg> {
            let (Tag::Data, sealed) = frame::unwrap(message)? else { return None };
            DataMsg::from_bytes(&self.data_opener.open(&[Tag::Data as u8], sealed)?)
        }
    }

    /// Drive the viewer through one key-exchange round with `host_password`,
    /// returning whether the host confirmed it. On a mismatch the host sends a
    /// retry when `offer_retry` is set (it has another password to try).
    fn kex_round(viewer: &mut ViewerSession, host_password: &[u8], offer_retry: bool, now_ms: f64) -> bool {
        let out = if now_ms == 0.0 { viewer.receive(&paired(), now_ms) } else { viewer.receive(&kex_frame(KEX_RETRY, &[]), now_ms) };
        let viewer_kex = sent(&out).remove(0);
        let (Tag::Kex, [KEX_MSG, viewer_msg @ ..]) = frame::unwrap(&viewer_kex).unwrap() else { panic!("expected a KEX message") };
        let (state, host_msg) = pake::start(host_password, true);
        let shared = pake::finish(state, viewer_msg).unwrap();
        let out = viewer.receive(&kex_frame(KEX_MSG, &host_msg), now_ms + 0.1);
        let viewer_confirm = sent(&out).remove(0);
        let (Tag::Kex, [KEX_CONFIRM, tag @ ..]) = frame::unwrap(&viewer_confirm).unwrap() else { panic!("expected a confirm") };
        let matched = pake::verify_peer(&shared, false, tag);
        let out = viewer.receive(&kex_frame(KEX_CONFIRM, &pake::confirm_tag(&shared, true)), now_ms + 0.2);
        if matched {
            assert!(out.contains(&Event::Authenticated));
        } else if offer_retry {
            assert!(out.is_empty() && !viewer.is_ended(), "viewer should wait for the retry");
        }
        matched
    }

    #[test]
    fn browser_viewer_authenticates_with_the_hosts_second_password() {
        let mut viewer = ViewerSession::new("session-99", "");
        assert!(!kex_round(&mut viewer, b"unattended-123456", true, 0.0), "first password is not ours");
        assert!(kex_round(&mut viewer, b"session-99", false, 1.0), "second password matches");
        assert!(viewer.is_authenticated());
    }

    #[test]
    fn browser_viewer_gives_up_after_the_last_password() {
        let mut viewer = ViewerSession::new("neither-000000", "");
        assert!(!kex_round(&mut viewer, b"unattended-123456", true, 0.0));
        // Second (final) password also fails: the host does not offer a retry.
        assert!(!kex_round(&mut viewer, b"session-99", false, 1.0));
        assert!(viewer.is_ended());
    }

    #[test]
    fn connect_frame_validates_ids() {
        assert!(connect_frame("12345678", "123456789").is_err());
        let bytes = connect_frame("123456789", "987654321").unwrap();
        let (Tag::Control, body) = frame::unwrap(&bytes).unwrap() else { panic!() };
        assert_eq!(Control::from_bytes(body), Some(Control::Connect { target: "123456789".into(), peer_id: "987654321".into(), ver: 1 }));
    }

    #[test]
    fn unavailable_peer_uses_the_desktop_message() {
        use protocol::control::ERROR_PEER_UNAVAILABLE;
        let mut viewer = ViewerSession::new("pw", "");
        let error = frame::wrap(Tag::Control, &Control::Error { code: ERROR_PEER_UNAVAILABLE.into() }.to_bytes());
        assert_eq!(viewer.receive(&error, 0.0), vec![Event::Ended(reconnect::PEER_UNAVAILABLE_MESSAGE.into())]);
    }

    #[test]
    fn relay_refusals_use_the_desktop_messages() {
        let refused = |code: &str| {
            let mut viewer = ViewerSession::new("pw", "");
            viewer.receive(&frame::wrap(Tag::Control, &Control::Error { code: code.into() }.to_bytes()), 0.0)
        };
        assert_eq!(refused(protocol::control::ERROR_CONNECT_RATE), vec![Event::Ended(reconnect::RELAY_LIMITED_MESSAGE.into())]);
        assert_eq!(refused("from_a_newer_relay"), vec![Event::Ended("invalid pairing message".into())]);
    }

    #[test]
    fn wrong_password_fails_key_confirmation() {
        let mut viewer = ViewerSession::new("right", "");
        let Err(out) = authenticate(&mut viewer, "wrong") else { panic!("must not authenticate") };
        assert_eq!(out, vec![Event::Ended("key confirmation failed".into())]);
    }

    #[test]
    fn session_opens_video_seals_input_and_handles_control() {
        let mut viewer = ViewerSession::new("pw", "123456");
        let mut host = authenticate(&mut viewer, "pw").unwrap();
        assert_ne!(host.shared, [0; 32]);
        // Plaintext ICE from the host is ignored.
        let ice = frame::wrap(Tag::Control, &Control::Ice { session: "s1".into(), cand: "cred:a:b".into() }.to_bytes());
        assert!(viewer.receive(&ice, 3.0).is_empty());
        let monitors = host.control(&Control::MonitorList { monitors: vec![] });
        assert_eq!(viewer.receive(&monitors, 4.0), vec![Event::Monitors("[]".into())]);
        let out = viewer.receive(&host.video(0, true), 5.0);
        assert!(matches!(&out[..], [Event::Video(frame)] if frame.len() > VIDEO_HEADER_LEN));
        // A gap holds frames back and asks for a keyframe once per 500 ms.
        let out = viewer.receive(&host.video(2, false), 6.0);
        assert_eq!(out.len(), 1);
        assert_eq!(host.open_control(&sent(&out)[0]), Some(Control::RequestKeyframe));
        assert!(viewer.receive(&host.video(3, false), 7.0).is_empty());
        assert!(matches!(&viewer.receive(&host.video(4, true), 8.0)[..], [Event::Video(_)]));

        let input = protocol::input::InputEvent::MouseMove { x: 1, y: 2 }.to_bytes(9);
        let sealed = viewer.input(&input).unwrap();
        let (Tag::Input, body) = frame::unwrap(&sealed).unwrap() else { panic!() };
        assert_eq!(host.opener.open(&[Tag::Input as u8], body), Some(input));

        let bye = host.control(&Control::Bye { session: String::new() });
        assert_eq!(viewer.receive(&bye, 9.0), vec![Event::Ended(HOST_ENDED_MESSAGE.into())]);
        assert!(viewer.input(&[1]).is_none());
    }

    #[test]
    fn audio_frames_open_with_the_media_opener() {
        let mut viewer = ViewerSession::new("pw", "");
        let mut host = authenticate(&mut viewer, "pw").unwrap();
        // Video first (counter 0), then audio (counter 1): one media sealer.
        assert!(matches!(&viewer.receive(&host.video(0, true), 3.0)[..], [Event::Video(_)]));
        let audio = host.audio(vec![1, 2, 3, 4], 2);
        assert_eq!(viewer.receive(&audio, 4.0), vec![Event::Audio(vec![1, 2, 3, 4], 2)]);
    }

    #[test]
    fn clipboard_opens_inbound_and_seals_outbound() {
        use protocol::clipboard::ClipboardMsg;
        let mut viewer = ViewerSession::new("pw", "");
        let mut host = authenticate(&mut viewer, "pw").unwrap();
        // Inbound clipboard surfaces as a Clipboard event.
        let clip = host.data(&DataMsg::Clipboard(ClipboardMsg { seq: 1, text: "hello".into() }));
        assert_eq!(viewer.receive(&clip, 3.0), vec![Event::Clipboard("hello".into())]);
        // Outbound clipboard is sealed on the Data channel.
        let sealed = viewer.clipboard("copied").unwrap();
        assert_eq!(host.open_data(&sealed), Some(DataMsg::Clipboard(ClipboardMsg { seq: 1, text: "copied".into() })));
        // Oversize text is dropped.
        assert!(viewer.clipboard(&"x".repeat(protocol::clipboard::MAX_CLIPBOARD + 1)).is_none());
    }

    #[test]
    fn file_transfer_offers_open_and_incoming_offers_surface() {
        use protocol::filexfer::FileMsg;
        let mut viewer = ViewerSession::new("pw", "");
        let mut host = authenticate(&mut viewer, "pw").unwrap();
        // Outgoing: the viewer's offer opens on the host as a file Offer.
        let sealed = viewer.send_file(42, "note.txt", b"hello".to_vec()).unwrap();
        match host.open_data(&sealed) {
            Some(DataMsg::FileTransfer(FileMsg::Offer { id, name, size, .. })) => {
                assert_eq!((id, name.as_str(), size), (42, "note.txt", 5));
            }
            other => panic!("expected a file offer, got {other:?}"),
        }
        // Incoming: a host offer surfaces as a File(Offer) event.
        let offer = host.data(&DataMsg::FileTransfer(FileMsg::Offer { id: 9, name: "in.bin".into(), size: 3, hash: [0; 32] }));
        assert_eq!(
            viewer.receive(&offer, 3.0),
            vec![Event::File(filexfer::FileEvent::Offer { id: 9, name: "in.bin".into(), size: 3 })],
        );
        // Accepting seals an Accept the host can open.
        let accept = viewer.accept_file(9).unwrap();
        assert_eq!(host.open_data(&accept), Some(DataMsg::FileTransfer(FileMsg::Accept { id: 9 })));
    }

    #[test]
    fn host_rejection_carries_the_code() {
        let mut viewer = ViewerSession::new("pw", "");
        let mut host = authenticate(&mut viewer, "pw").unwrap();
        let reject = host.control(&Control::Error { code: "2fa_required".into() });
        assert_eq!(viewer.receive(&reject, 3.0), vec![Event::Ended("session rejected by host: 2fa_required".into())]);
    }

    #[test]
    fn timers_send_keepalives_and_notice_a_silent_host() {
        let mut viewer = ViewerSession::new("pw", "");
        let mut host = authenticate(&mut viewer, "pw").unwrap();
        assert!(viewer.tick(1_000.0).is_empty());
        let out = viewer.tick(2.0 + KEEPALIVE_INTERVAL_MS);
        assert_eq!(host.open_control(&sent(&out)[0]), Some(Control::KeepAlive));
        let out = viewer.tick(2.0 + DEAD_PEER_TIMEOUT_MS);
        assert_eq!(out, vec![Event::Ended("connection closed: host stopped responding for 45s".into())]);
    }

    #[test]
    fn kex_has_a_deadline_and_closes_name_the_phase() {
        let mut viewer = ViewerSession::new("pw", "");
        viewer.receive(&paired(), 0.0);
        assert!(viewer.tick(KEX_DEADLINE_MS - 1.0).is_empty());
        assert_eq!(viewer.tick(KEX_DEADLINE_MS), vec![Event::Ended("connection closed during KEX".into())]);
        let mut viewer = ViewerSession::new("pw", "");
        assert_eq!(viewer.closed(), vec![Event::Ended("relay closed while connecting".into())]);
        assert!(viewer.closed().is_empty());
    }

    #[test]
    fn bye_is_sealed_and_ends_the_session() {
        let mut viewer = ViewerSession::new("pw", "");
        assert!(viewer.bye().is_none());
        let mut host = authenticate(&mut viewer, "pw").unwrap();
        let bye = viewer.bye().unwrap();
        assert_eq!(host.open_control(&bye), Some(Control::Bye { session: String::new() }));
        assert!(viewer.is_ended());
    }
}
