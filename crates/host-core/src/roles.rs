//! Host-role session orchestration.

use crate::{
    audio,
    capture::{ScapCapturer, ScreenCapturer},
    encode::{self, stream::{self, KeyframePolicy, STREAM_FPS}, Encoder, VideoEncoder},
    inject::{EnigoInjector, InputInjector, NoopInjector},
    transport::{self, RelayIceSignaling, RelayTransport, ReliableChannel, Transport, TransportReceiver, TransportSender},
    vdisplay::{self, DisplaySpec, SessionDisplays, VirtualDisplays},
};
use crate::reconnect::{classify_viewer_error, reject_code, rejection_error, DisconnectReason};
use protocol::{
    control::Control,
    crypto::{Opener, Sealer},
    input::InputEvent,
    kex::derive_keys,
    Tag,
};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, error::Error, fmt::Display, future::Future, sync::{atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering}, Arc, Mutex, OnceLock}, time::{Duration, Instant}};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;

const KEX_MSG: u8 = 0x01;
const KEX_CONFIRM: u8 = 0x02;
/// A paired peer must complete both SPAKE2 exchanges promptly; this bounds
/// resources held by connections that never authenticate.
const KEX_DEADLINE: Duration = Duration::from_secs(10);

/// ICE role invariant: the host controls nomination and the viewer accepts it.
/// These values MUST differ for ICE to nominate a connection.
const HOST_ICE_CONTROLLING: bool = true;
const VIEWER_ICE_CONTROLLING: bool = false;

/// Whether a successfully authenticated session may bypass manual approval.
pub fn should_auto_accept(unattended_enabled: bool, pake_succeeded: bool) -> bool {
    unattended_enabled && pake_succeeded
}

/// Allow-list approval only skips the UI dialog; PAKE authentication still runs first.
pub fn should_auto_accept_allowlisted(is_allowed: bool) -> bool { is_allowed }

pub fn should_auto_accept_with_2fa(unattended_enabled: bool, pake_succeeded: bool, twofa_enabled: bool, code_valid: bool) -> bool {
    unattended_enabled && pake_succeeded && (!twofa_enabled || code_valid)
}

pub fn totp_is_valid(secret: &str, code: &str, unix_time: u64) -> bool {
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) { return false; }
    // Fail closed on a malformed secret. Authenticator apps base32-decode the
    // enrolled secret to get the HMAC key; falling back to the raw ASCII bytes
    // would compute TOTP over the wrong key and could never match, so a decode
    // failure must reject rather than silently use a different key.
    let Some(decoded) = decode_base32(secret) else { return false; };
    [unix_time.saturating_sub(30), unix_time, unix_time.saturating_add(30)]
        .into_iter()
        .fold(false, |matched, time| {
            matched | constant_time_code_eq(
                &totp_lite::totp_custom::<totp_lite::Sha1>(30, 6, &decoded, time),
                code,
            )
        })
}

fn constant_time_code_eq(candidate: &str, supplied: &str) -> bool {
    if candidate.len() != 6 || supplied.len() != 6 { return false; }
    let mut difference = 0u8;
    for (left, right) in candidate.as_bytes().iter().zip(supplied.as_bytes()) {
        difference |= left ^ right;
    }
    difference == 0
}

static ACCEPTED_TOTP_STEPS: OnceLock<Mutex<HashMap<[u8; 32], u64>>> = OnceLock::new();

/// Validate a TOTP and atomically consume its matching step for this host's
/// secret. The process-wide map is keyed by a SHA-256 digest of the secret,
/// so reconnects to the same host share replay state; it is not durable across
/// process restarts and does not coordinate separate host processes.
fn totp_is_valid_once(secret: &str, code: &str, unix_time: u64) -> bool {
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) { return false; }
    let Some(decoded) = decode_base32(secret) else { return false; };
    let current_step = unix_time / 30;
    let candidates = [
        current_step.saturating_sub(1),
        current_step,
        current_step.saturating_add(1),
    ];
    let mut matched_step = None;
    for step in candidates {
        let candidate = totp_lite::totp_custom::<totp_lite::Sha1>(30, 6, &decoded, step * 30);
        if constant_time_code_eq(&candidate, code) && matched_step.is_none() {
            matched_step = Some(step);
        }
    }
    let Some(step) = matched_step else { return false; };
    let key: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
    let cache = ACCEPTED_TOTP_STEPS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut accepted = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if accepted.get(&key).is_some_and(|last| step <= *last) { return false; }
    accepted.insert(key, step);
    true
}

fn decode_base32(value: &str) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in value.bytes().filter(|byte| *byte != b'=') {
        let upper = byte.to_ascii_uppercase();
        let digit = match upper {
            b'A'..=b'Z' => upper - b'A',
            b'2'..=b'7' => upper - b'2' + 26,
            _ => return None,
        } as u32;
        buffer = (buffer << 5) | digit;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    (!output.is_empty()).then_some(output)
}

type SessionResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug)]
struct SessionError(&'static str);

impl Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl Error for SessionError {}

fn kex_body(sub: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + payload.len());
    v.push(sub);
    v.extend_from_slice(payload);
    v
}

fn parse_kex(body: &[u8]) -> Option<(u8, &[u8])> {
    body.split_first().map(|(s, rest)| (*s, rest))
}

/// Live reliable session data handle. The payloads remain sealed; Task 3 owns
/// the protocol-level sealing/opening and file-transfer dispatch.
pub struct DataHandle {
    pub sender: transport::reliable::ReliableSender,
    pub receiver: mpsc::Receiver<Vec<u8>>,
    pub sealer: Sealer,
    pub opener: Opener,
    pub control_sender: transport::reliable::ReliableSender,
    /// The ONE control-direction sealer of this session. The session loop
    /// shares it (keepalives) so every control frame uses one nonce sequence;
    /// a second `Sealer` on the same key would reuse nonces.
    pub control_sealer: SharedSealer,
    /// The path media takes and its measured round trip, for display.
    pub link: transport::LinkStatus,
}

/// How often a viewer pings the host while media goes over the relay.
pub const RELAY_PING_INTERVAL: Duration = Duration::from_secs(1);

/// A sealer shared between the session loop and the application.
pub type SharedSealer = Arc<Mutex<Sealer>>;

/// How often each peer sends a sealed `Control::KeepAlive` on the relay
/// socket. Well below the relay's idle timeout (~60 s).
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
/// A peer that sent no control traffic (keepalives included) for this long is
/// considered gone and the session ends. Three missed keepalives.
pub const DEAD_PEER_TIMEOUT: Duration = Duration::from_secs(45);

/// How long a connection request waits for the host user. The viewer sits
/// on its "Connecting" screen meanwhile, and the host takes no other viewer
/// until the request is answered: a minute is enough to notice the prompt
/// and walk over, without locking the host out for long when nobody is there.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long capture may stay down mid-session (display unplugged or asleep,
/// permission revoked) while it is restarted. Past that the session ends and
/// the viewer is told the host cannot share its screen.
pub const CAPTURE_RESTORE_WINDOW: Duration = Duration::from_secs(10);

/// Liveness timings of a session; tests shorten them.
#[derive(Debug, Clone, Copy)]
struct SessionTimings {
    keepalive: Duration,
    dead_peer: Duration,
    approval: Duration,
    capture_restore: Duration,
}

impl Default for SessionTimings {
    fn default() -> Self {
        Self { keepalive: KEEPALIVE_INTERVAL, dead_peer: DEAD_PEER_TIMEOUT, approval: APPROVAL_TIMEOUT, capture_restore: CAPTURE_RESTORE_WINDOW }
    }
}

/// How long a new virtual display gets to show up for capture.
const VIRTUAL_DISPLAY_WAIT: Duration = Duration::from_secs(5);
/// Time macOS needs to bring up a headless host's virtual display before
/// capture can start on it.
const HEADLESS_SETTLE: Duration = Duration::from_millis(1500);

/// Something the host user should hear about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayNotice {
    /// The host has no screen, so the session added a virtual one.
    HeadlessAdded,
    ViewerAdded,
    ViewerRemoved,
}

pub type DisplayNotify = Arc<dyn Fn(DisplayNotice) + Send + Sync>;

/// The host's displays as a session sees them.
#[derive(Clone)]
pub struct HostDisplays {
    pub backend: Arc<dyn VirtualDisplays>,
    /// The host setting "Let viewers add virtual displays". It also lets a
    /// host with no screen add one for itself.
    pub allow_virtual: bool,
    /// See [`crate::vdisplay::displays_present`].
    pub present: Arc<dyn Fn() -> Option<bool> + Send + Sync>,
    pub notify: Option<DisplayNotify>,
}

impl HostDisplays {
    /// This computer's displays.
    pub fn platform(allow_virtual: bool) -> Self {
        Self { backend: vdisplay::platform(), allow_virtual, present: Arc::new(vdisplay::displays_present), notify: None }
    }

    fn tell(&self, notice: DisplayNotice) {
        if let Some(notify) = &self.notify {
            notify(notice);
        }
    }
}

impl Default for HostDisplays {
    /// No virtual displays, and no idea whether a screen is attached.
    fn default() -> Self {
        Self { backend: Arc::new(vdisplay::Unsupported), allow_virtual: false, present: Arc::new(|| None), notify: None }
    }
}

fn seal_control(sealer: &SharedSealer, control: &Control) -> Vec<u8> {
    sealer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).seal(&[Tag::Control as u8], &control.to_bytes())
}

/// Send a sealed `Control::KeepAlive` every `interval` until the writer
/// closes or the returned guard is dropped.
fn spawn_keepalive(sender: transport::reliable::ReliableSender, sealer: SharedSealer, interval: Duration) -> AbortOnDrop<()> {
    AbortOnDrop(tokio::spawn(async move {
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let sealed = seal_control(&sealer, &Control::KeepAlive);
            if sender.send_frame(Tag::Control, &sealed).await.is_err() { break; }
        }
    }))
}

/// Best-effort sealed `Control::Bye` ending the session for the peer, bounded
/// (~200 ms) like [`send_rejection`] so a stalled writer cannot hold up the
/// caller. The short sleep lets the writer flush before it is torn down.
pub async fn send_bye(sender: &transport::reliable::ReliableSender, sealer: &SharedSealer) {
    let sealed = seal_control(sealer, &Control::Bye { session: String::new() });
    let sent = tokio::time::timeout(Duration::from_millis(150), sender.send_frame(Tag::Control, &sealed)).await;
    if matches!(sent, Ok(Ok(()))) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerSessionError {
    pub reason: DisconnectReason,
    pub message: String,
}

impl Display for ViewerSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.message) }
}
impl Error for ViewerSessionError {}

fn viewer_session_error(message: String) -> ViewerSessionError {
    ViewerSessionError { reason: classify_viewer_error(&message), message }
}

impl DataHandle {
    pub async fn send_data(&self, sealed: &[u8]) -> Result<(), transport::TransportError> {
        self.sender.send_frame(Tag::Data, sealed).await
    }
}

async fn recv_control(rx: &mut mpsc::Receiver<Vec<u8>>) -> Option<Vec<u8>> { rx.recv().await }

/// Best-effort sealed `Control::Error` telling the viewer why the host
/// rejected it after the key exchange. Bounded (~200ms total) so a stalled
/// writer can never hold up the host; the short sleep lets the WebSocket
/// writer flush before the session (and its writer task) is dropped.
async fn send_rejection(sender: &transport::reliable::ReliableSender, sealer: &SharedSealer, code: &str) {
    let sealed = seal_control(sealer, &Control::Error { code: code.to_owned() });
    let sent = tokio::time::timeout(Duration::from_millis(150), sender.send_frame(Tag::Control, &sealed)).await;
    if matches!(sent, Ok(Ok(()))) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Writes out everything already queued, then waits briefly so the relay
/// can forward it before the socket closes. Bounded (~200 ms) like
/// [`send_rejection`].
async fn drain_before_close(sender: &transport::reliable::ReliableSender) {
    if matches!(tokio::time::timeout(Duration::from_millis(150), sender.flush()).await, Ok(Ok(()))) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Splits ICE signaling from the other control frames, starting with
/// `early` (frames that arrived before the session started).
fn split_session_control(mut rx: mpsc::Receiver<Vec<u8>>, early: Vec<Vec<u8>>, session: String) -> (mpsc::Receiver<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
    let (ice_tx, ice_rx) = mpsc::channel(64);
    let (session_tx, session_rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let mut early = early.into_iter();
        while let Some(body) = match early.next() { Some(body) => Some(body), None => rx.recv().await } {
            let is_ice = matches!(Control::from_bytes(&body), Some(Control::Ice { session: body_session, .. }) if body_session == session);
            // Late ICE candidates after signaling is dropped are discarded;
            // only a closed session receiver ends the demux (which the host
            // loop observes as the peer being gone).
            if is_ice { let _ = ice_tx.send(body).await; } else if session_tx.send(body).await.is_err() { break; }
        }
    });
    (ice_rx, session_rx)
}

/// How a connection request was answered.
#[derive(Debug, PartialEq, Eq)]
enum Approval {
    Allowed,
    Denied,
    /// Nobody answered within [`SessionTimings::approval`].
    Expired,
    /// The viewer left (its socket closed or went silent) first.
    PeerGone,
}

/// Most control frames kept while a request waits; an authenticated viewer
/// sends a few dozen (ICE candidates, keepalives).
const MAX_EARLY_CONTROL: usize = 256;

/// Waits for the host user's answer while watching the viewer's socket, so
/// a viewer that leaves or a prompt nobody answers never blocks the host.
async fn await_approval(
    decision: impl Future<Output = bool>,
    control_rx: &mut mpsc::Receiver<Vec<u8>>,
    early: &mut Vec<Vec<u8>>,
    timings: SessionTimings,
) -> Approval {
    tokio::pin!(decision);
    let expires = tokio::time::Instant::now() + timings.approval;
    let mut peer_deadline = tokio::time::Instant::now() + timings.dead_peer;
    loop {
        tokio::select! {
            allowed = &mut decision => return if allowed { Approval::Allowed } else { Approval::Denied },
            body = control_rx.recv() => {
                let Some(body) = body else { return Approval::PeerGone };
                peer_deadline = tokio::time::Instant::now() + timings.dead_peer;
                if early.len() < MAX_EARLY_CONTROL { early.push(body); }
            }
            _ = tokio::time::sleep_until(expires) => return Approval::Expired,
            _ = tokio::time::sleep_until(peer_deadline) => return Approval::PeerGone,
        }
    }
}

fn even_frame(frame: encode::RawFrame) -> Option<encode::RawFrame> {
    let width = frame.width & !1;
    let height = frame.height & !1;
    if width == 0
        || height == 0
        || frame.data.len() != usize::from(frame.width) * usize::from(frame.height) * 4
    {
        return None;
    }
    if width == frame.width && height == frame.height {
        return Some(frame);
    }
    let source_width = usize::from(frame.width);
    let cropped_width = usize::from(width);
    let mut data = Vec::with_capacity(cropped_width * usize::from(height) * 4);
    for row in frame
        .data
        .chunks_exact(source_width * 4)
        .take(usize::from(height))
    {
        data.extend_from_slice(&row[..cropped_width * 4]);
    }
    Some(encode::RawFrame {
        width,
        height,
        data,
        format: frame.format,
        timestamp_us: frame.timestamp_us,
    })
}

/// Host-enforced gates for what a connected viewer may do, shared with the
/// caller (the app) so it can flip them live during a session. The host, not
/// the viewer, honours these, so a malicious viewer cannot bypass them. Every
/// gate defaults to enabled.
#[derive(Clone)]
pub struct SessionGates {
    /// Inject the viewer's remote mouse/keyboard input. When off, input is
    /// dropped host-side before it reaches the OS.
    pub control: Arc<AtomicBool>,
    /// Send host system audio to the viewer. When off, captured audio frames
    /// are dropped before they are sealed and sent.
    pub audio: Arc<AtomicBool>,
}

impl Default for SessionGates {
    fn default() -> Self {
        Self { control: Arc::new(AtomicBool::new(true)), audio: Arc::new(AtomicBool::new(true)) }
    }
}

/// Run a host session after relay pairing.
///
/// The encoder factory receives the dimensions of the first usable captured
/// frame. The session performs SPAKE2 confirmation before starting either
/// capture or input injection, then runs both directions concurrently.
pub async fn run_host_session<S, C, E, F, I>(
    ws: WebSocketStream<S>,
    password: &str,
    ice_session: &str,
    capturer: C,
    encoder_factory: F,
    injector: I,
) -> SessionResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: ScreenCapturer + Send + 'static,
    E: Encoder + Send + 'static,
    F: Fn(u16, u16, encode::Codec) -> Result<E, encode::EncoderInitError> + Clone + Send + 'static,
    I: InputInjector + Send + 'static,
{
    run_host_session_with_data(ws, password, ice_session, capturer, encoder_factory, injector, None).await
}

pub async fn run_host_session_with_data<S, C, E, F, I>(
    ws: WebSocketStream<S>, password: &str, ice_session: &str, capturer: C,
    encoder_factory: F, injector: I, data_ready: Option<oneshot::Sender<DataHandle>>,
) -> SessionResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: ScreenCapturer + Send + 'static,
    E: Encoder + Send + 'static,
    F: Fn(u16, u16, encode::Codec) -> Result<E, encode::EncoderInitError> + Clone + Send + 'static,
    I: InputInjector + Send + 'static,
{
    run_host_session_with_data_and_accept(
        ws, password, ice_session, capturer, encoder_factory, injector, false,
        || async { true }, data_ready,
    ).await
}

/// Host session variant with an approval callback invoked only after SPAKE2
/// and key confirmation succeed. Unattended mode can return true without
/// presenting a manual approval request; the default path awaits the callback.
#[allow(clippy::too_many_arguments)]
pub async fn run_host_session_with_data_and_accept<S, C, E, F, I, A, Fut>(
    ws: WebSocketStream<S>, password: &str, ice_session: &str, capturer: C,
    encoder_factory: F, injector: I, unattended_enabled: bool, approve: A,
    data_ready: Option<oneshot::Sender<DataHandle>>,
) -> SessionResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: ScreenCapturer + Send + 'static,
    E: Encoder + Send + 'static,
    F: Fn(u16, u16, encode::Codec) -> Result<E, encode::EncoderInitError> + Clone + Send + 'static,
    I: InputInjector + Send + 'static,
    A: FnOnce() -> Fut,
    Fut: Future<Output = bool>,
{
    run_host_session_with_data_and_accept_and_2fa(
        ws, password, ice_session, capturer, encoder_factory, injector,
        unattended_enabled, false, None, approve, data_ready,
    ).await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_host_session_with_data_and_accept_and_2fa<S, C, E, F, I, A, Fut>(
    ws: WebSocketStream<S>, password: &str, ice_session: &str, capturer: C,
    encoder_factory: F, injector: I, unattended_enabled: bool,
    twofa_enabled: bool, totp_secret: Option<&str>, approve: A,
    data_ready: Option<oneshot::Sender<DataHandle>>,
) -> SessionResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: ScreenCapturer + Send + 'static,
    E: Encoder + Send + 'static,
    F: Fn(u16, u16, encode::Codec) -> Result<E, encode::EncoderInitError> + Clone + Send + 'static,
    I: InputInjector + Send + 'static,
    A: FnOnce() -> Fut,
    Fut: Future<Output = bool>,
{
    run_host_session_with_data_and_accept_and_2fa_with_factory(
        ws, password, ice_session, once_capturer(capturer), encoder_factory,
        move || Box::new(injector) as Box<dyn InputInjector>,
        unattended_enabled, twofa_enabled, totp_secret, approve, data_ready, SessionTimings::default(), HostDisplays::default(),
        SessionGates::default(),
    ).await
}

/// Wraps a single pre-built capturer as a factory the session can call more
/// than once. The first call yields the capturer; later calls (an aux stream)
/// get `None`, so only capturers made from a real factory can open aux
/// streams.
fn once_capturer<C: Send + 'static>(capturer: C) -> impl Fn() -> Option<C> + Clone + Send + 'static {
    let cell = Arc::new(Mutex::new(Some(capturer)));
    move || cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()
}

#[allow(clippy::too_many_arguments)]
async fn run_host_session_with_data_and_accept_and_2fa_with_factory<S, C, E, F, IF, A, Fut, CF>(
    ws: WebSocketStream<S>, password: &str, ice_session: &str, capture_factory: CF,
    encoder_factory: F, injector_factory: IF, unattended_enabled: bool,
    twofa_enabled: bool, totp_secret: Option<&str>, approve: A,
    data_ready: Option<oneshot::Sender<DataHandle>>, timings: SessionTimings, displays: HostDisplays,
    gates: SessionGates,
)-> SessionResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: ScreenCapturer + Send + 'static,
    E: Encoder + Send + 'static,
    F: Fn(u16, u16, encode::Codec) -> Result<E, encode::EncoderInitError> + Clone + Send + 'static,
    IF: FnOnce() -> Box<dyn InputInjector> + Send + 'static,
    A: FnOnce() -> Fut,
    Fut: Future<Output = bool>,
    CF: Fn() -> Option<C> + Clone + Send + 'static,
{
    let mut reliable = ReliableChannel::spawn(ws);
    let sender = reliable.sender();
    let mut control_rx = reliable.take_control_rx().ok_or(SessionError("control receiver unavailable"))?;
    let data_rx = reliable.take_data_rx().ok_or(SessionError("data receiver unavailable"))?;
    let (state, my_msg) = protocol::pake::start(password.as_bytes(), true);
    sender.send_frame(Tag::Kex, &kex_body(KEX_MSG, &my_msg)).await?;
    let kex_deadline = tokio::time::Instant::now() + KEX_DEADLINE;
    let shared = loop {
        match tokio::time::timeout_at(kex_deadline, recv_control(&mut control_rx)).await.ok().flatten() {
            Some(body) => if let Some((KEX_MSG, msg)) = parse_kex(&body) {
                break protocol::pake::finish(state, msg).ok_or(SessionError("invalid SPAKE2 message"))?;
            },
            None => return Err(Box::new(SessionError("connection closed during KEX"))),
        }
    };
    let my_confirm = protocol::pake::confirm_tag(&shared, true);
    sender.send_frame(Tag::Kex, &kex_body(KEX_CONFIRM, &my_confirm)).await?;
    loop {
        match tokio::time::timeout_at(kex_deadline, recv_control(&mut control_rx)).await.ok().flatten() {
            Some(body) => if let Some((KEX_CONFIRM, tag)) = parse_kex(&body) {
                if !protocol::pake::verify_peer(&shared, false, tag) {
                    // Let our confirmation out first: without it the viewer
                    // reads a dropped connection and retries instead of
                    // reporting the wrong password.
                    drain_before_close(&sender).await;
                    return Err(Box::new(SessionError("key confirmation failed")));
                }
                break;
            },
            None => return Err(Box::new(SessionError("connection closed during KEX confirmation"))),
        }
    }
    let (control_send_key, control_recv_key) = protocol::kex::derive_control_keys(&shared, true);
    let control_sealer: SharedSealer = Arc::new(Mutex::new(Sealer::new(control_send_key)));
    let mut control_opener = Opener::strict(control_recv_key);
    // Keepalives start as soon as the peer is authenticated, so neither the
    // relay nor the viewer's dead-peer timer drops a session that is waiting
    // on the host user's approval.
    let _keepalive = spawn_keepalive(sender.clone(), Arc::clone(&control_sealer), timings.keepalive);
    let mut code_received = false;
    let code_valid = if unattended_enabled && twofa_enabled {
        let body = tokio::time::timeout(Duration::from_secs(30), recv_control(&mut control_rx)).await.ok().flatten();
        let code = body.and_then(|body| control_opener.open(&[Tag::Control as u8], &body)).and_then(|body| match Control::from_bytes(&body) {
            Some(Control::AuthCode { code }) if !code.is_empty() => Some(code),
            _ => None,
        });
        code_received = code.is_some();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |duration| duration.as_secs());
        code.as_deref().zip(totp_secret).is_some_and(|(code, secret)| totp_is_valid_once(secret, code, now))
    } else { true };
    // Control frames that arrive while the host user decides (ICE
    // candidates, keepalives) are kept for the session.
    let mut early_control = Vec::new();
    let rejection = if unattended_enabled && twofa_enabled && !code_valid {
        Some(if code_received { reject_code::TWOFA_INVALID } else { reject_code::TWOFA_REQUIRED })
    } else if should_auto_accept_with_2fa(unattended_enabled, true, twofa_enabled, code_valid) {
        None
    } else {
        match await_approval(approve(), &mut control_rx, &mut early_control, timings).await {
            Approval::Allowed => None,
            Approval::Denied => Some(reject_code::DENIED),
            Approval::Expired => {
                send_rejection(&sender, &control_sealer, reject_code::NO_ANSWER).await;
                return Err(Box::new(SessionError("nobody answered the connection request")));
            }
            Approval::PeerGone => {
                log::info!("viewer left before its request was answered");
                return Ok(());
            }
        }
    };
    if let Some(code) = rejection {
        // Tell the viewer explicitly why, so it stops retrying instead of
        // reading the close as a transient drop.
        send_rejection(&sender, &control_sealer, code).await;
        return Err(Box::new(SessionError("session rejected")));
    }
    // Dropped when the session ends, which removes its virtual displays.
    let mut virtual_displays = SessionDisplays::new(Arc::clone(&displays.backend), displays.allow_virtual);
    if (displays.present)() == Some(false) {
        match virtual_displays.add_headless() {
            Ok(_) => {
                displays.tell(DisplayNotice::HeadlessAdded);
                tokio::time::sleep(HEADLESS_SETTLE).await;
            }
            Err(error) => {
                log::warn!("this computer has no screen to share ({error})");
                send_rejection(&sender, &control_sealer, reject_code::NO_DISPLAY).await;
                return Err(Box::new(SessionError("this computer has no screen to share")));
            }
        }
    }
    let Some(capturer) = capture_factory() else {
        // Without this the viewer reads the close as a drop and reconnects.
        let (code, message) = if (displays.present)() == Some(false) {
            (reject_code::NO_DISPLAY, "this computer has no screen to share")
        } else {
            (reject_code::CAPTURE_UNAVAILABLE, capture_unavailable_message())
        };
        send_rejection(&sender, &control_sealer, code).await;
        return Err(Box::new(SessionError(message)));
    };
    reliable.enable_application_traffic();
    let (send_key, recv_key) = derive_keys(&shared, true);
    let sealer = Sealer::new(send_key);
    let opener = Opener::new(recv_key);
    // The Data channel uses a SEPARATE key (distinct HKDF label) from media, so
    // the two streams' independent nonce counters never collide under a shared
    // key. Sharing the media key here would be catastrophic AEAD nonce reuse.
    let (data_send_key, data_recv_key) = protocol::kex::derive_data_keys(&shared, true);
    let data_sealer = Sealer::new(data_send_key);
    let data_opener = Opener::strict(data_recv_key);
    let (ice_rx, mut session_control_rx) = split_session_control(control_rx, early_control, ice_session.to_owned());

    let signaling: RelayIceSignaling<S> = RelayIceSignaling::new(sender.clone(), ice_rx, ice_session);
    let relay: RelayTransport<S> = RelayTransport::new(sender.clone(), reliable.take_media_rx().ok_or(SessionError("media receiver unavailable"))?);
    let transport = transport::establish(
        HOST_ICE_CONTROLLING,
        signaling,
        relay,
        Some(transport::FeedbackKeys::derive(&shared, true)),
        transport::default_agent_config(),
        Duration::from_secs(3),
    )
    .await;
    // Send the monitor list first, then share the SAME control sealer with the
    // data channel. Host->viewer control frames must share one sealer (one
    // nonce sequence) on this key: a second Sealer::new on control_send_key
    // would restart the counter at 0 and reuse a nonce with MonitorList.
    let mut monitors = capturer.monitors();
    virtual_displays.annotate(&mut monitors);
    // Whether this capturer can run a second instance for a per-monitor aux
    // stream (multi-monitor windows). Read before the capturer moves into the
    // video pipeline.
    let aux_supported = capturer.supports_aux_streams();
    let monitor_list = Control::MonitorList { monitors: monitors.clone() };
    reliable.sender().send_frame(Tag::Control, &seal_control(&control_sealer, &monitor_list)).await?;
    // Old viewers ignore this; new ones only offer "Add display" after it.
    if let Some(max) = virtual_displays.viewer_limit() {
        reliable.sender().send_frame(Tag::Control, &seal_control(&control_sealer, &Control::VirtualDisplaySupport { max })).await?;
    }
    // Old viewers ignore this; new ones only offer Ctrl+Alt+Del after it.
    if crate::secure_attention::available() {
        reliable.sender().send_frame(Tag::Control, &seal_control(&control_sealer, &Control::SecureAttentionSupport)).await?;
    }
    // Old viewers ignore this; new ones only offer per-monitor windows after it.
    if aux_supported {
        reliable.sender().send_frame(Tag::Control, &seal_control(&control_sealer, &Control::MultiMonitorSupport { max: AUX_STREAM_MAX })).await?;
    }
    // Only when unattended is on (so the host comes back hosting after the
    // reboot) and the platform can restart without a prompt. Old viewers ignore
    // it and hide the restart control.
    if unattended_enabled && crate::restart::available() {
        reliable.sender().send_frame(Tag::Control, &seal_control(&control_sealer, &Control::RestartSupport)).await?;
    }
    // Old viewers ignore this; new ones only offer privacy mode after it.
    if crate::privacy::available() {
        reliable.sender().send_frame(Tag::Control, &seal_control(&control_sealer, &Control::PrivacyModeSupport)).await?;
    }
    let path = transport.path();
    let (transport_tx, transport_rx) = transport.into_split();
    if let Some(ready) = data_ready {
        let _ = ready.send(DataHandle {
            sender: reliable.sender(),
            receiver: data_rx,
            sealer: data_sealer,
            opener: data_opener,
            control_sender: reliable.sender(),
            control_sealer: Arc::clone(&control_sealer),
            link: transport::LinkStatus::new(path, transport_tx.rtt_meter()),
        });
    }
    let audio_rx = audio::spawn_capture();

    // Capture and encode run on their own thread, so a slow encode never
    // stalls control, keepalives, audio or input. It publishes the input
    // mapping dimensions once the first frame is in.
    let dims = Arc::new(AtomicU32::new(0));
    let rate = transport_tx.rate_control();
    // Clones kept for per-monitor aux streams, since the originals move into
    // the main video pipeline below.
    let aux_rate = Arc::clone(&rate);
    let aux_capture_factory = capture_factory.clone();
    let aux_encoder_factory = encoder_factory.clone();
    let aux_capture_restore = timings.capture_restore;
    let aux_sender = reliable.sender();
    let aux_control_sealer = Arc::clone(&control_sealer);
    // Encoded aux frames flow here, tagged with their monitor id; the media
    // task seals and sends them on the aux video channel.
    let (aux_tx, aux_rx) = mpsc::channel::<(u32, Vec<u8>)>(AUX_STREAM_QUEUE);
    let mut aux_streams: std::collections::HashMap<u32, AbortOnDrop<()>> = std::collections::HashMap::new();
    // Held for the session; dropping it (session end, error, or panic) always
    // restores local input and the screen.
    let mut privacy_guard: Option<crate::privacy::PrivacyMode> = None;
    let (video, mut video_events) = VideoSource::start(capturer, encoder_factory, Arc::clone(&dims), rate, timings.capture_restore).await?;
    // Input injection makes blocking OS calls, so it runs on its own thread
    // and never stalls capture/encode/send or audio. The injector is built on
    // that thread because platform injectors (Enigo) can be thread-affine.
    let display = Arc::new(AtomicU64::new(pack_display(None)));
    let inject_tx = spawn_injection_thread(injector_factory, Arc::clone(&dims), Arc::clone(&display))?;
    // Aborting the forwarder on exit drops the last injection sender, which
    // ends the injection thread together with the session.
    // Also read by the control loop so view-only blocks host-state changes
    // (privacy, restart, virtual displays, secure attention), not just input.
    let control_gate = Arc::clone(&gates.control);
    let mut input_forwarder = AbortOnDrop(tokio::spawn(forward_input(transport_rx, opener, inject_tx, gates.control)));
    let mut media = AbortOnDrop(tokio::spawn(send_media(transport_tx, sealer, audio_rx, Arc::clone(&video.units), aux_rx, gates.audio)));
    // The viewer keepalives on the relay socket even when media is direct, so
    // silence there means the viewer is gone (a direct transport may never
    // report a dead peer on its own).
    let mut peer_deadline = tokio::time::Instant::now() + timings.dead_peer;
    // Poll the host's cursor shape and forward changes so the viewer can draw a
    // matching pointer. u64::MAX stands for "hidden"; None for nothing sent yet.
    let mut cursor_interval = tokio::time::interval(Duration::from_millis(66));
    cursor_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_cursor: Option<u64> = None;
    loop {
        tokio::select! {
            _ = cursor_interval.tick() => {
                match crate::cursor::sample() {
                    crate::cursor::CursorSample::Visible(image) => {
                        let fingerprint = crate::cursor::fingerprint(&image);
                        if last_cursor != Some(fingerprint) {
                            last_cursor = Some(fingerprint);
                            let shape = Control::CursorShape { width: image.width, height: image.height, hot_x: image.hot_x, hot_y: image.hot_y, rgba: image.rgba };
                            let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &shape)).await;
                        }
                    }
                    crate::cursor::CursorSample::Hidden => {
                        if last_cursor != Some(u64::MAX) {
                            last_cursor = Some(u64::MAX);
                            let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &Control::CursorHidden)).await;
                        }
                    }
                    crate::cursor::CursorSample::Unavailable => {}
                }
            },
            _ = tokio::time::sleep_until(peer_deadline) => {
                log::warn!("viewer sent no control traffic for {}s; ending the session", timings.dead_peer.as_secs());
                return Ok(());
            },
            body = session_control_rx.recv() => {
                // The reliable (WebSocket) channel closed: the peer is gone.
                let Some(body) = body else { return Ok(()) };
                let Some(plaintext) = control_opener.open(&[Tag::Control as u8], &body) else { continue };
                peer_deadline = tokio::time::Instant::now() + timings.dead_peer;
                match Control::from_bytes(&plaintext) {
                    // Authenticated by the per-session control key, so the
                    // session field is informational only.
                    Some(Control::Bye { .. }) => return Ok(()),
                    // The new monitor's frames arrive later; the video
                    // pipeline adapts to their size.
                    Some(Control::SelectMonitor { id }) => {
                        // Input follows the switch; the capturer ignores ids
                        // it did not advertise, so input does too.
                        if monitors.iter().any(|monitor| monitor.id == id) {
                            display.store(pack_display(Some(id)), Ordering::Release);
                        }
                        video.send(VideoCommand::SelectMonitor(id));
                    }
                    // Point input at a monitor without switching the main
                    // capture: a per-monitor window owns input while focused.
                    Some(Control::SetInputMonitor { id }) => {
                        if monitors.iter().any(|monitor| monitor.id == id) {
                            display.store(pack_display(Some(id)), Ordering::Release);
                        }
                    }
                    Some(Control::AddVirtualDisplay { width, height, hidpi }) if control_gate.load(Ordering::Relaxed) => {
                        let spec = DisplaySpec::fitted(width, height, hidpi);
                        match virtual_displays.add(&spec) {
                            // Shared once capture can see it (VideoEvent::Shown).
                            Ok(id) => {
                                displays.tell(DisplayNotice::ViewerAdded);
                                video.send(VideoCommand::ShowDisplay(id));
                            }
                            Err(code) => {
                                let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &Control::VirtualDisplayRefused { code: code.into() })).await;
                            }
                        }
                    }
                    Some(Control::RemoveVirtualDisplay { id }) if control_gate.load(Ordering::Relaxed) => {
                        // Move off the display first, so capture never waits
                        // on one that is gone (but not for a refusal).
                        let allowed = virtual_displays.check_remove(id);
                        if allowed.is_ok() && unpack_display(display.load(Ordering::Acquire)) == Some(id) {
                            let main = monitors.iter().find(|monitor| monitor.is_primary && monitor.id != id).map(|monitor| monitor.id);
                            display.store(pack_display(main), Ordering::Release);
                            if let Some(main) = main { video.send(VideoCommand::SelectMonitor(main)); }
                        }
                        let reply = match allowed.and_then(|()| virtual_displays.remove(id)) {
                            Ok(()) => {
                                displays.tell(DisplayNotice::ViewerRemoved);
                                monitors.retain(|monitor| monitor.id != id);
                                Control::MonitorList { monitors: monitors.clone() }
                            }
                            Err(code) => Control::VirtualDisplayRefused { code: code.into() },
                        };
                        let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &reply)).await;
                    }
                    Some(Control::RequestKeyframe) => video.send(VideoCommand::RequestKeyframe),
                    Some(Control::SetMaxBitrate { bps }) => video.send(VideoCommand::MaxBitrate(bps)),
                    // The host tries HEVC when the viewer can decode it; the
                    // encoder falls back to H.264 where hardware can't, and the
                    // frame header tells the viewer which codec each frame uses.
                    Some(Control::DecoderCodecs { hevc }) => {
                        video.send(VideoCommand::SetCodec(if hevc { encode::Codec::Hevc } else { encode::Codec::H264 }));
                    }
                    Some(Control::RequestSecureAttention) => {
                        if !control_gate.load(Ordering::Relaxed) {
                            log::debug!("ignoring secure attention request: control is off");
                        } else if let Err(error) = crate::secure_attention::deliver() {
                            log::debug!("ignoring secure attention request: {error}");
                        }
                    }
                    // Turning privacy on is a control action (it can lock the host
                    // user out), so view-only blocks it; turning it off is always
                    // allowed so the host can be recovered.
                    Some(Control::SetPrivacyMode { on }) => {
                        if on {
                            if control_gate.load(Ordering::Relaxed) {
                                if privacy_guard.is_none() {
                                    match crate::privacy::enable() {
                                        Ok(guard) => privacy_guard = Some(guard),
                                        Err(error) => log::warn!("privacy mode could not start: {error}"),
                                    }
                                }
                            } else {
                                log::debug!("ignoring privacy-on request: control is off");
                            }
                        } else {
                            // Dropping the guard restores local input and the screen.
                            privacy_guard = None;
                        }
                        let now_on = privacy_guard.is_some();
                        let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &Control::PrivacyModeChanged { on: now_on })).await;
                    }
                    Some(Control::RequestRestart) => {
                        // Only honor it on the same terms it was advertised, so a
                        // viewer can't reboot a host that won't come back, and not
                        // while the host user has revoked control.
                        if !control_gate.load(Ordering::Relaxed) {
                            log::debug!("ignoring restart request: control is off");
                        } else if unattended_enabled && crate::restart::available() {
                            // Tell the viewer to expect a long gap before we return.
                            let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &Control::Restarting)).await;
                            if let Err(error) = crate::restart::restart() {
                                log::warn!("host restart request failed: {error}");
                            }
                        } else {
                            log::debug!("ignoring restart request: unattended off or unsupported platform");
                        }
                    }
                    Some(Control::OpenMonitorStream { id }) => {
                        use protocol::control::{MONITOR_STREAM_FAILED, MONITOR_STREAM_LIMIT, MONITOR_STREAM_UNKNOWN};
                        let refuse = |code: &str| Control::MonitorStreamRefused { id, code: code.into() };
                        if !aux_supported {
                            let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &refuse(MONITOR_STREAM_FAILED))).await;
                        } else if !monitors.iter().any(|monitor| monitor.id == id) {
                            let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &refuse(MONITOR_STREAM_UNKNOWN))).await;
                        } else if aux_streams.contains_key(&id) {
                            // Already streaming; re-affirm so the viewer shows it.
                            let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &Control::MonitorStreamOpened { id })).await;
                        } else if aux_streams.len() >= AUX_STREAM_MAX as usize {
                            let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &refuse(MONITOR_STREAM_LIMIT))).await;
                        } else {
                            // The task reports MonitorStreamOpened once the first
                            // frame is in, or MonitorStreamRefused if it can't.
                            let handle = spawn_aux_stream(
                                id, aux_capture_factory.clone(), aux_encoder_factory.clone(),
                                Arc::clone(&aux_rate), aux_capture_restore, aux_tx.clone(),
                                aux_sender.clone(), Arc::clone(&aux_control_sealer),
                            );
                            aux_streams.insert(id, handle);
                        }
                    }
                    Some(Control::CloseMonitorStream { id }) => {
                        // Dropping the handle aborts the task, which drops the
                        // capturer and encoder and stops that stream.
                        if aux_streams.remove(&id).is_some() {
                            let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &Control::MonitorStreamClosed { id })).await;
                        }
                    }
                    Some(Control::Ping { sent_us }) => {
                        let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &Control::Pong { sent_us })).await;
                    }
                    _ => {}
                }
            },
            Some(event) = video_events.recv() => match event {
                VideoEvent::Monitors(mut list) => {
                    virtual_displays.annotate(&mut list);
                    monitors = list;
                    // Capture left a vanished monitor for the main one.
                    if unpack_display(display.load(Ordering::Acquire)).is_some_and(|id| !monitors.iter().any(|monitor| monitor.id == id)) {
                        display.store(pack_display(None), Ordering::Release);
                    }
                    // Stop aux streams for monitors that are gone.
                    let gone: Vec<u32> = aux_streams.keys().copied().filter(|id| !monitors.iter().any(|monitor| monitor.id == *id)).collect();
                    for id in gone {
                        aux_streams.remove(&id);
                        let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &Control::MonitorStreamClosed { id })).await;
                    }
                    let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &Control::MonitorList { monitors: monitors.clone() })).await;
                }
                VideoEvent::Shown(id) => {
                    display.store(pack_display(Some(id)), Ordering::Release);
                    let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &Control::VirtualDisplayAdded { id })).await;
                }
                VideoEvent::NotShown(id) => {
                    log::warn!("virtual display {id} never showed up for capture; removing it");
                    let _ = virtual_displays.remove(id);
                    let refused = Control::VirtualDisplayRefused { code: protocol::control::VIRTUAL_DISPLAY_FAILED.into() };
                    let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &refused)).await;
                }
                VideoEvent::CaptureLost => {
                    log::warn!("screen capture stopped and could not be restarted; ending the session");
                    send_rejection(&sender, &control_sealer, reject_code::CAPTURE_UNAVAILABLE).await;
                    return Err(Box::new(SessionError("screen capture stopped and could not be restarted")));
                }
            },
            ended = &mut input_forwarder.0 => {
                return match ended {
                    Ok(InputForwardEnd::PeerGone) => Ok(()),
                    Ok(InputForwardEnd::InjectorGone) => Err(Box::new(SessionError("input injection thread stopped"))),
                    Err(_) => Err(Box::new(SessionError("input forwarding task failed"))),
                };
            },
            ended = &mut media.0 => {
                return match ended {
                    Ok(result) => result,
                    Err(_) => Err(Box::new(SessionError("media sending task failed"))),
                };
            },
        }
    }
}

/// Sends audio and encoded video to the viewer until the transport fails or
/// the video pipeline stops. Audio goes first; a video frame is only taken
/// from the encoder once the transport has room for it, so on a slow link
/// the encoder skips captures rather than queueing stale frames.
async fn send_media(
    mut transport_tx: Box<dyn TransportSender>,
    mut sealer: Sealer,
    mut audio_rx: mpsc::Receiver<protocol::audio::AudioFrame>,
    units: Arc<UnitSlot>,
    mut aux_rx: mpsc::Receiver<(u32, Vec<u8>)>,
    audio_allowed: Arc<AtomicBool>,
) -> SessionResult<()> {
    let mut video_ready = false;
    loop {
        tokio::select! {
            biased;
            Some(audio_frame) = audio_rx.recv() => {
                // Host-enforced mute: drain the frame but never send it so no
                // host audio reaches the viewer while the gate is off.
                if !audio_allowed.load(Ordering::Relaxed) { continue; }
                let sealed = sealer.seal(&[Tag::Audio as u8], &audio_frame.to_bytes());
                let _ = transport_tx.send(Tag::Audio, &sealed).await;
            },
            () = transport_tx.ready_for_video(), if !video_ready => video_ready = true,
            unit = units.take(), if video_ready => {
                let plaintext = unit?;
                video_ready = false;
                let sealed = sealer.seal(&[Tag::Video as u8], &plaintext);
                transport_tx.send(Tag::Video, &sealed).await?;
            },
            // Per-monitor aux streams share the main view's congestion
            // control. The monitor id rides inside the sealed body, so it is
            // authenticated. One media sealer seals every stream, so the shared
            // nonce counter never repeats.
            Some((id, plaintext)) = aux_rx.recv(), if video_ready => {
                video_ready = false;
                let mut body = Vec::with_capacity(4 + plaintext.len());
                body.extend_from_slice(&id.to_be_bytes());
                body.extend_from_slice(&plaintext);
                let sealed = sealer.seal(&[Tag::VideoAux as u8], &body);
                transport_tx.send(Tag::VideoAux, &sealed).await?;
            },
        }
    }
}

/// Most per-monitor aux streams one session will run at once, on top of the
/// main view. Each is a full capture+encode pipeline, so this caps the extra
/// CPU and bandwidth of multi-monitor windows.
const AUX_STREAM_MAX: u32 = 3;
/// Encoded aux frames buffered toward the media task. Small: the aux pipelines
/// drop to the newest frame rather than queue stale ones on a slow link.
const AUX_STREAM_QUEUE: usize = 4;

/// Starts a per-monitor aux stream: a second capture+encode pipeline pinned to
/// monitor `id`, whose frames flow to `aux_tx` tagged with that id. The task
/// reports `MonitorStreamOpened` once its first frame is in, or
/// `MonitorStreamRefused` if capture or the encoder can't start, and
/// `MonitorStreamClosed` if the stream ends on its own. Dropping the returned
/// handle aborts the task, which drops the capturer and encoder.
#[allow(clippy::too_many_arguments)]
fn spawn_aux_stream<C, E, F, CF>(
    id: u32,
    capture_factory: CF,
    encoder_factory: F,
    rate: Arc<transport::RateControl>,
    capture_restore: Duration,
    aux_tx: mpsc::Sender<(u32, Vec<u8>)>,
    sender: transport::reliable::ReliableSender,
    control_sealer: SharedSealer,
) -> AbortOnDrop<()>
where
    C: ScreenCapturer + Send + 'static,
    E: Encoder + Send + 'static,
    F: Fn(u16, u16, encode::Codec) -> Result<E, encode::EncoderInitError> + Send + 'static,
    CF: Fn() -> Option<C> + Send + 'static,
{
    AbortOnDrop(tokio::spawn(async move {
        let refuse = |code: &str| {
            let sender = sender.clone();
            let control_sealer = Arc::clone(&control_sealer);
            let reply = Control::MonitorStreamRefused { id, code: code.into() };
            async move { let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &reply)).await; }
        };
        let Some(capturer) = capture_factory() else {
            refuse(protocol::control::MONITOR_STREAM_FAILED).await;
            return;
        };
        let dims = Arc::new(AtomicU32::new(0));
        let (video, mut events) = match VideoSource::start(capturer, encoder_factory, dims, rate, capture_restore).await {
            Ok(pair) => pair,
            Err(_) => { refuse(protocol::control::MONITOR_STREAM_FAILED).await; return; }
        };
        // Pin this pipeline to its monitor; its first frames may show the
        // default display until the switch lands.
        video.send(VideoCommand::SelectMonitor(id));
        let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &Control::MonitorStreamOpened { id })).await;
        let units = Arc::clone(&video.units);
        loop {
            tokio::select! {
                unit = units.take() => match unit {
                    Ok(plaintext) => if aux_tx.send((id, plaintext)).await.is_err() { break },
                    Err(_) => break,
                },
                event = events.recv() => match event {
                    Some(VideoEvent::CaptureLost) | None => break,
                    _ => {}
                },
            }
        }
        // Reached only when the stream ends on its own (capture lost); a
        // viewer-initiated close aborts this task before here, and the control
        // loop sends its own MonitorStreamClosed.
        let _ = sender.send_frame(Tag::Control, &seal_control(&control_sealer, &Control::MonitorStreamClosed { id })).await;
        drop(video);
    }))
}

/// How long a new session waits for the first captured frame.
const FIRST_FRAME_DEADLINE: Duration = Duration::from_secs(10);
/// Longest the encode thread blocks before it re-checks commands and
/// shutdown.
const VIDEO_POLL: Duration = Duration::from_millis(20);

/// Validates and even-crops a captured frame, then scales it to the stream
/// size. Returns the (even) capture size, which maps input, and the frame
/// to encode.
fn stream_frame(frame: encode::RawFrame) -> Option<((u16, u16), encode::RawFrame)> {
    let frame = even_frame(frame)?;
    let capture = (frame.width, frame.height);
    let (width, height) = stream::stream_dims(frame.width, frame.height);
    if (width, height) == capture {
        return Some((capture, frame));
    }
    let data = stream::downscale_bgra(&frame.data, frame.width, frame.height, width, height)?;
    Some((capture, encode::RawFrame { width, height, data, format: frame.format, timestamp_us: frame.timestamp_us }))
}

/// Turns captured frames into sealed-ready video plaintext (header plus
/// access unit).
///
/// Frames larger than the stream size are scaled down. A frame of a new
/// stream size (display mode or monitor change) replaces the encoder, which
/// starts with a keyframe; the input mapping follows the capture size.
/// Keyframes are forced periodically and on (rate-limited) viewer request.
struct VideoPipeline<E, F> {
    encoder_factory: F,
    encoder: E,
    /// The last frame, already at stream size; repeated on a static screen.
    frame: encode::RawFrame,
    capture: (u16, u16),
    dims: Arc<AtomicU32>,
    seq: u64,
    started: Instant,
    keyframes: KeyframePolicy,
    /// Follows the transport's bitrate estimate (none in tests).
    bitrate: Option<stream::BitrateFollower>,
    /// The codec the factory should build; a change rebuilds the encoder.
    desired_codec: encode::Codec,
    /// Set when the encoder must be rebuilt before the next encode (a codec
    /// switch; size changes rebuild inline).
    rebuild: bool,
}

impl<E, F> VideoPipeline<E, F>
where
    E: Encoder,
    F: Fn(u16, u16, encode::Codec) -> Result<E, encode::EncoderInitError>,
{
    fn new(frame: encode::RawFrame, encoder_factory: F, dims: Arc<AtomicU32>) -> SessionResult<Self> {
        let (capture, frame) = stream_frame(frame)
            .ok_or(SessionError("screen capture returned an invalid or unusable frame"))?;
        let encoder = encoder_factory(frame.width, frame.height, encode::Codec::H264)?;
        dims.store(pack_dims(capture.0, capture.1), Ordering::Release);
        let now = Instant::now();
        Ok(Self { encoder_factory, encoder, frame, capture, dims, seq: 0, started: now, keyframes: KeyframePolicy::new(now), bitrate: None, desired_codec: encode::Codec::H264, rebuild: false })
    }

    fn with_rate(mut self, rate: Arc<transport::RateControl>) -> Self {
        self.bitrate = Some(stream::BitrateFollower::new(rate));
        self
    }

    /// A viewer asked for a keyframe; returns whether it was accepted.
    fn request_keyframe(&mut self) -> bool { self.keyframes.request(Instant::now()) }

    /// Cap the stream bitrate at `bps` (0 = automatic); takes effect next encode.
    fn set_max_bitrate(&mut self, bps: u32) {
        if let Some(bitrate) = &mut self.bitrate { bitrate.set_cap(bps); }
    }

    fn keyframe_due(&self) -> bool { self.keyframes.due(Instant::now()) }

    /// Ask for a codec on the next encode; a no-op when already building it.
    fn set_codec(&mut self, codec: encode::Codec) {
        if codec != self.desired_codec {
            self.desired_codec = codec;
            self.rebuild = true;
        }
    }

    /// Encodes `fresh` if it is a usable frame, else the previous frame.
    /// Returns `None` when the encoder produced no output.
    fn next(&mut self, fresh: Option<encode::RawFrame>) -> SessionResult<Option<Vec<u8>>> {
        if let Some((capture, frame)) = fresh.and_then(stream_frame) {
            if (frame.width, frame.height) != (self.frame.width, self.frame.height) {
                log::info!("stream size changed to {}x{} (capture {}x{})", frame.width, frame.height, capture.0, capture.1);
                self.encoder = (self.encoder_factory)(frame.width, frame.height, self.desired_codec)?;
                self.rebuild = false;
                if let Some(bitrate) = &mut self.bitrate { bitrate.encoder_replaced(); }
            }
            if capture != self.capture {
                self.dims.store(pack_dims(capture.0, capture.1), Ordering::Release);
                self.capture = capture;
            }
            self.frame = frame;
        }
        // A codec switch with no size change rebuilds here (a size change above
        // already rebuilt with the desired codec and cleared the flag).
        if self.rebuild {
            self.encoder = (self.encoder_factory)(self.frame.width, self.frame.height, self.desired_codec)?;
            self.rebuild = false;
            if let Some(bitrate) = &mut self.bitrate { bitrate.encoder_replaced(); }
        }
        let now = Instant::now();
        if self.keyframes.due(now) {
            self.encoder.force_intra_frame();
        }
        if let Some(bitrate) = &mut self.bitrate { bitrate.update(&mut self.encoder, self.frame.width, self.frame.height); }
        let Some(encoded) = self.encoder.encode(&self.frame) else { return Ok(None) };
        if encoded.keyframe {
            self.keyframes.keyframe_sent(now);
        }
        // Stamped at encode time: repeated frames need their own timestamps.
        // The header field is 32-bit microseconds and wraps after ~71 minutes.
        let timestamp_us = self.started.elapsed().as_micros() as u32;
        let mut plaintext = encode::video_header(
            self.seq, timestamp_us, encoded.keyframe, self.frame.width, self.frame.height, self.encoder.codec(),
        );
        plaintext.extend_from_slice(&encoded.data);
        self.seq += 1;
        Ok(Some(plaintext))
    }
}

/// Requests from the session loop to the encode thread.
enum VideoCommand {
    SelectMonitor(u32),
    /// Switch to a new display as soon as capture lists it.
    ShowDisplay(u32),
    RequestKeyframe,
    /// Cap the stream bitrate at this many bps; 0 is fully automatic.
    MaxBitrate(u32),
    /// Switch the stream codec (HEVC when the viewer supports it, else H.264).
    SetCodec(encode::Codec),
}

/// News from the encode thread for the session loop.
#[derive(Debug)]
enum VideoEvent {
    /// The host's displays changed; the viewer needs the new list.
    Monitors(Vec<protocol::control::MonitorInfo>),
    /// Capture stayed down past the restore window.
    CaptureLost,
    /// Capture switched to the display from `ShowDisplay`. A `Monitors`
    /// event listing it comes first.
    Shown(u32),
    /// The display from `ShowDisplay` never showed up.
    NotShown(u32),
}

/// Async-side handle of the capture/encode thread. Dropping it stops the
/// thread (which then drops the capturer and encoder) within one poll.
struct VideoSource {
    commands: std::sync::mpsc::Sender<VideoCommand>,
    units: Arc<UnitSlot>,
}

impl VideoSource {
    /// Starts the encode thread and waits until the first frame was captured
    /// and the encoder created (or either failed).
    async fn start<C, E, F>(
        capturer: C, encoder_factory: F, dims: Arc<AtomicU32>, rate: Arc<transport::RateControl>, capture_restore: Duration,
    ) -> SessionResult<(Self, mpsc::UnboundedReceiver<VideoEvent>)>
    where
        C: ScreenCapturer + Send + 'static,
        E: Encoder + 'static,
        F: Fn(u16, u16, encode::Codec) -> Result<E, encode::EncoderInitError> + Send + 'static,
    {
        let (commands, command_rx) = std::sync::mpsc::channel();
        let units = Arc::new(UnitSlot::default());
        let (ready_tx, ready_rx) = oneshot::channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let thread_units = Arc::clone(&units);
        std::thread::Builder::new()
            .name("opendesk-video-encode".into())
            .spawn(move || {
                let _close = CloseOnExit(&thread_units);
                let link = VideoLink { commands: &command_rx, units: &thread_units, events, capture_restore };
                run_video_thread(capturer, encoder_factory, dims, rate, link, ready_tx);
            })?;
        match ready_rx.await {
            Ok(Ok(())) => Ok((Self { commands, units }, event_rx)),
            Ok(Err(message)) => Err(message.into()),
            Err(_) => Err(Box::new(SessionError("video encode thread stopped"))),
        }
    }

    fn send(&self, command: VideoCommand) { let _ = self.commands.send(command); }
}

impl Drop for VideoSource {
    fn drop(&mut self) { self.units.close(); }
}

/// Closes the slot when the encode thread ends, even by panic, so the
/// media task notices.
struct CloseOnExit<'a>(&'a UnitSlot);

impl Drop for CloseOnExit<'_> {
    fn drop(&mut self) { self.0.close(); }
}

/// How the encode thread talks to the session loop.
struct VideoLink<'a> {
    commands: &'a std::sync::mpsc::Receiver<VideoCommand>,
    units: &'a UnitSlot,
    events: mpsc::UnboundedSender<VideoEvent>,
    capture_restore: Duration,
}

/// Body of the encode thread: encode each new capture as soon as it arrives
/// and the transport has taken the previous access unit; on a static screen
/// repeat the last frame slowly. While capture restarts the last frame
/// keeps going out; the session hears of display changes and of capture
/// that could not be restored.
fn run_video_thread<C, E, F>(
    mut capturer: C,
    encoder_factory: F,
    dims: Arc<AtomicU32>,
    rate: Arc<transport::RateControl>,
    link: VideoLink<'_>,
    ready: oneshot::Sender<Result<(), String>>,
) where
    C: ScreenCapturer,
    E: Encoder,
    F: Fn(u16, u16, encode::Codec) -> Result<E, encode::EncoderInitError>,
{
    let deadline = Instant::now() + FIRST_FRAME_DEADLINE;
    let first = loop {
        if let Some(frame) = capturer.wait_frame(VIDEO_POLL) {
            break frame;
        }
        if Instant::now() >= deadline {
            let _ = ready.send(Err("screen capture produced no frames".into()));
            return;
        }
        if ready.is_closed() {
            return;
        }
    };
    let mut video = match VideoPipeline::new(first, encoder_factory, dims).map(|video| video.with_rate(rate)) {
        Ok(video) => video,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    let VideoLink { commands, units, events, capture_restore } = link;
    let mut last_encode: Option<Instant> = None;
    let mut awaited = AwaitedDisplay::default();
    loop {
        loop {
            match commands.try_recv() {
                Ok(VideoCommand::SelectMonitor(id)) => { capturer.select_monitor(id); }
                Ok(VideoCommand::ShowDisplay(id)) => awaited.start(id, Instant::now()),
                Ok(VideoCommand::RequestKeyframe) => {
                    if !video.request_keyframe() { log::debug!("keyframe request ignored: one was just sent"); }
                }
                Ok(VideoCommand::MaxBitrate(bps)) => video.set_max_bitrate(bps),
                Ok(VideoCommand::SetCodec(codec)) => video.set_codec(codec),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }
        if let Some(monitors) = capturer.monitors_changed() {
            let _ = events.send(VideoEvent::Monitors(monitors));
        }
        for event in awaited.check(&mut capturer, Instant::now()) {
            let _ = events.send(event);
        }
        if capturer.stalled_since().is_some_and(|since| since.elapsed() >= capture_restore) {
            let _ = events.send(VideoEvent::CaptureLost);
            // Keep the unit slot open until the session has told the viewer.
            while commands.recv().is_ok() {}
            return;
        }
        // Encode only once the previous access unit is on its way: while the
        // link is busy the capturer keeps just the newest frame.
        match units.wait_space(VIDEO_POLL) {
            SlotSpace::Free => {}
            SlotSpace::Full => continue,
            SlotSpace::Closed => return,
        }
        let wait = stream::capture_wait(last_encode.map(|at| at.elapsed()), VIDEO_POLL);
        let fresh = if wait.is_zero() { capturer.next_frame() } else { capturer.wait_frame(wait) };
        if !stream::encode_due(fresh.is_some(), video.keyframe_due(), last_encode.map(|at| at.elapsed())) {
            continue;
        }
        match video.next(fresh) {
            Ok(Some(unit)) => units.put(unit),
            Ok(None) => {}
            Err(error) => {
                units.fail(error.to_string());
                return;
            }
        }
        last_encode = Some(Instant::now());
    }
}

/// A new display the encode thread switches to once capture lists it.
#[derive(Default)]
struct AwaitedDisplay {
    pending: Option<(u32, Instant)>,
    next_check: Option<Instant>,
}

impl AwaitedDisplay {
    const CHECK_EVERY: Duration = Duration::from_millis(250);

    fn start(&mut self, id: u32, now: Instant) {
        self.pending = Some((id, now + VIRTUAL_DISPLAY_WAIT));
        self.next_check = Some(now);
    }

    fn check<C: ScreenCapturer>(&mut self, capturer: &mut C, now: Instant) -> Vec<VideoEvent> {
        let Some((id, deadline)) = self.pending else { return Vec::new() };
        if self.next_check.is_some_and(|at| now < at) {
            return Vec::new();
        }
        self.next_check = Some(now + Self::CHECK_EVERY);
        let monitors = capturer.monitors();
        if monitors.iter().any(|monitor| monitor.id == id) && capturer.select_monitor(id) {
            self.pending = None;
            return vec![VideoEvent::Monitors(monitors), VideoEvent::Shown(id)];
        }
        if now >= deadline {
            self.pending = None;
            return vec![VideoEvent::NotShown(id)];
        }
        Vec::new()
    }
}

/// One-unit handoff from the encode thread to the media task.
#[derive(Default)]
struct UnitSlot {
    state: Mutex<SlotState>,
    /// Wakes the encode thread when the unit is taken or the slot closes.
    space: std::sync::Condvar,
    /// Wakes the media task when a unit arrives or the slot closes.
    filled: tokio::sync::Notify,
}

#[derive(Default)]
struct SlotState {
    unit: Option<Vec<u8>>,
    closed: bool,
    error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum SlotSpace {
    Free,
    Full,
    Closed,
}

impl UnitSlot {
    fn lock(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Waits up to `timeout` for the slot to be empty.
    fn wait_space(&self, timeout: Duration) -> SlotSpace {
        let (state, _) = self
            .space
            .wait_timeout_while(self.lock(), timeout, |state| state.unit.is_some() && !state.closed)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed { SlotSpace::Closed } else if state.unit.is_some() { SlotSpace::Full } else { SlotSpace::Free }
    }

    fn put(&self, unit: Vec<u8>) {
        self.lock().unit = Some(unit);
        self.filled.notify_one();
    }

    /// Closes the slot with an error for the media task.
    fn fail(&self, error: String) {
        self.lock().error.get_or_insert(error);
        self.close();
    }

    fn close(&self) {
        self.lock().closed = true;
        self.filled.notify_one();
        self.space.notify_all();
    }

    /// The next unit; an error once the slot closed. Cancel safe.
    async fn take(&self) -> SessionResult<Vec<u8>> {
        loop {
            let filled = self.filled.notified();
            {
                let mut state = self.lock();
                if let Some(unit) = state.unit.take() {
                    drop(state);
                    self.space.notify_all();
                    return Ok(unit);
                }
                if state.closed {
                    return Err(state.error.clone().unwrap_or_else(|| "video pipeline stopped".into()).into());
                }
            }
            filled.await;
        }
    }
}

/// Bounded queue between the input forwarder and the injection thread.
const INJECT_QUEUE: usize = 64;

fn pack_dims(width: u16, height: u16) -> u32 { (u32::from(width) << 16) | u32::from(height) }

fn unpack_dims(packed: u32) -> (u32, u32) { (packed >> 16, packed & 0xFFFF) }

/// The shared display as an atomic value: 0 is the main display, a selected
/// monitor id is tagged in the high half.
fn pack_display(display: Option<u32>) -> u64 { display.map_or(0, |id| (1 << 32) | u64::from(id)) }

fn unpack_display(packed: u64) -> Option<u32> { (packed >> 32 == 1).then_some(packed as u32) }

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) { self.0.abort(); }
}

/// Start the dedicated input-injection thread. It builds the injector itself
/// (keeping thread-affine backends on one OS thread), reads the current
/// capture dimensions and shared display at injection time so a monitor
/// switch applies to the next event, and exits once every sender is dropped.
fn spawn_injection_thread<IF>(injector_factory: IF, dims: Arc<AtomicU32>, display: Arc<AtomicU64>) -> SessionResult<mpsc::Sender<InputEvent>>
where
    IF: FnOnce() -> Box<dyn InputInjector> + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<InputEvent>(INJECT_QUEUE);
    std::thread::Builder::new()
        .name("opendesk-input-inject".into())
        .spawn(move || {
            let mut injector = injector_factory();
            let mut current = None;
            while let Some(event) = rx.blocking_recv() {
                let selected = unpack_display(display.load(Ordering::Acquire));
                if selected != current {
                    injector.select_display(selected);
                    current = selected;
                }
                let (width, height) = unpack_dims(dims.load(Ordering::Acquire));
                injector.inject(&event, width, height);
            }
        })?;
    Ok(tx)
}

#[derive(Debug, PartialEq, Eq)]
enum InputForwardEnd {
    /// The media transport closed: the viewer is gone.
    PeerGone,
    /// The injection thread stopped (its receiver was dropped).
    InjectorGone,
}

/// Open viewer input and queue it for the injection thread.
///
/// Drop policy when the injection queue is full: mouse moves are coalesced to
/// the single most recent pending position (absolute coordinates, so older
/// ones are redundant) and never block. Buttons, keys, chars and scrolls are
/// never dropped: the forwarder awaits queue capacity, which back-pressures
/// only this input path, never the media loop. A pending move is flushed
/// before any non-move event so clicks land at the latest pointer position.
async fn forward_input(
    mut transport_rx: Box<dyn TransportReceiver>,
    mut opener: Opener,
    inject_tx: mpsc::Sender<InputEvent>,
    control_allowed: Arc<AtomicBool>,
) -> InputForwardEnd {
    let mut pending_move: Option<InputEvent> = None;
    loop {
        tokio::select! {
            biased;
            permit = inject_tx.reserve(), if pending_move.is_some() => {
                let Ok(permit) = permit else { return InputForwardEnd::InjectorGone };
                if let Some(event) = pending_move.take() { permit.send(event); }
            }
            received = transport_rx.recv() => {
                let Some((tag, body)) = received else { return InputForwardEnd::PeerGone };
                if tag != Tag::Input { continue; }
                let Some(plaintext) = opener.open(&[Tag::Input as u8], &body) else { continue };
                let Some((event, _timestamp_us)) = InputEvent::from_bytes(&plaintext) else { continue };
                // Host-enforced control gate: decrypt to keep the opener's nonce
                // sequence in step, then drop the event so nothing is injected.
                if !control_allowed.load(Ordering::Relaxed) { pending_move = None; continue; }
                if matches!(event, InputEvent::MouseMove { .. }) {
                    if pending_move.is_some() {
                        pending_move = Some(event);
                        continue;
                    }
                    match inject_tx.try_send(event) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(event)) => pending_move = Some(event),
                        Err(mpsc::error::TrySendError::Closed(_)) => return InputForwardEnd::InjectorGone,
                    }
                } else {
                    if let Some(pending) = pending_move.take() {
                        if inject_tx.send(pending).await.is_err() { return InputForwardEnd::InjectorGone; }
                    }
                    if inject_tx.send(event).await.is_err() { return InputForwardEnd::InjectorGone; }
                }
            }
        }
    }
}

/// Run the viewer side of a transport after SPAKE2 and key confirmation.
///
/// The viewer is the ICE-controlled peer and therefore uses the opposite
/// SPAKE2 role and directional keys from [`run_host_session`]. No media is
/// established or processed until peer confirmation has succeeded.
pub async fn run_viewer_session<S>(
    ws: WebSocketStream<S>,
    password: &str,
    ice_session: &str,
    frame_sink: tokio::sync::mpsc::Sender<Vec<u8>>,
    input_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
) -> Result<(), ViewerSessionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    run_viewer_session_with_data(ws, password, ice_session, frame_sink, input_rx, None).await
}

pub async fn run_viewer_session_with_data<S>(
    ws: WebSocketStream<S>, password: &str, ice_session: &str,
    frame_sink: tokio::sync::mpsc::Sender<Vec<u8>>,
    input_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    data_ready: Option<oneshot::Sender<DataHandle>>,
) -> Result<(), ViewerSessionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    run_viewer_session_with_monitors(ws, password, ice_session, frame_sink, input_rx, data_ready, None, None).await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_viewer_session_with_monitors<S>(
    ws: WebSocketStream<S>, password: &str, ice_session: &str,
    frame_sink: tokio::sync::mpsc::Sender<Vec<u8>>,
    input_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    data_ready: Option<oneshot::Sender<DataHandle>>,
    monitor_sink: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    audio_sink: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
) -> Result<(), ViewerSessionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    run_viewer_session_with_monitors_and_auth_code(
        ws, password, ice_session, frame_sink, input_rx, data_ready,
        monitor_sink, audio_sink, None,
    ).await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_viewer_session_with_monitors_and_auth_code<S>(
    ws: WebSocketStream<S>, password: &str, ice_session: &str,
    frame_sink: tokio::sync::mpsc::Sender<Vec<u8>>,
    input_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    data_ready: Option<oneshot::Sender<DataHandle>>,
    monitor_sink: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    audio_sink: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    auth_code: Option<String>,
) -> Result<(), ViewerSessionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let result = run_viewer_session_inner(ws, password, ice_session, frame_sink, input_rx, data_ready, monitor_sink, audio_sink, None, auth_code, SessionTimings::default()).await;
    result.map_err(viewer_session_error)
}

/// Like [`run_viewer_session_with_monitors_and_auth_code`], plus a sink for
/// per-monitor aux video streams (multi-monitor windows). Each item is
/// `(monitor id, video plaintext)`; the id matches a `MonitorStreamOpened`.
#[allow(clippy::too_many_arguments)]
pub async fn run_viewer_session_with_aux<S>(
    ws: WebSocketStream<S>, password: &str, ice_session: &str,
    frame_sink: tokio::sync::mpsc::Sender<Vec<u8>>,
    input_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    data_ready: Option<oneshot::Sender<DataHandle>>,
    monitor_sink: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    audio_sink: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    aux_frame_sink: Option<tokio::sync::mpsc::Sender<(u32, Vec<u8>)>>,
    auth_code: Option<String>,
) -> Result<(), ViewerSessionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let result = run_viewer_session_inner(ws, password, ice_session, frame_sink, input_rx, data_ready, monitor_sink, audio_sink, aux_frame_sink, auth_code, SessionTimings::default()).await;
    result.map_err(viewer_session_error)
}

#[allow(clippy::too_many_arguments)]
async fn run_viewer_session_inner<S>(
    ws: WebSocketStream<S>, password: &str, ice_session: &str,
    frame_sink: tokio::sync::mpsc::Sender<Vec<u8>>,
    input_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    data_ready: Option<oneshot::Sender<DataHandle>>,
    monitor_sink: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    audio_sink: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    aux_frame_sink: Option<tokio::sync::mpsc::Sender<(u32, Vec<u8>)>>,
    auth_code: Option<String>,
    timings: SessionTimings,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut reliable = ReliableChannel::spawn(ws);
    let sender = reliable.sender();
    let mut control_rx = reliable.take_control_rx().ok_or_else(|| "control receiver unavailable".to_owned())?;
    let data_rx = reliable.take_data_rx().ok_or_else(|| "data receiver unavailable".to_owned())?;
    let (state, my_msg) = protocol::pake::start(password.as_bytes(), false);
    sender.send_frame(Tag::Kex, &kex_body(KEX_MSG, &my_msg)).await.map_err(|error| error.to_string())?;
    let kex_deadline = tokio::time::Instant::now() + KEX_DEADLINE;

    let shared = loop {
        match tokio::time::timeout_at(kex_deadline, recv_control(&mut control_rx)).await.ok().flatten() {
            Some(body) => if let Some((KEX_MSG, msg)) = parse_kex(&body) {
                break protocol::pake::finish(state, msg).ok_or_else(|| "invalid SPAKE2 message".to_owned())?;
            },
            None => return Err("connection closed during KEX".to_owned()),
        }
    };

    let my_confirm = protocol::pake::confirm_tag(&shared, false);
    sender.send_frame(Tag::Kex, &kex_body(KEX_CONFIRM, &my_confirm)).await.map_err(|error| error.to_string())?;
    loop {
        match tokio::time::timeout_at(kex_deadline, recv_control(&mut control_rx)).await.ok().flatten() {
            Some(body) => if let Some((KEX_CONFIRM, tag)) = parse_kex(&body) {
                if !protocol::pake::verify_peer(&shared, true, tag) {
                    drain_before_close(&sender).await;
                    return Err("key confirmation failed".to_owned());
                }
                break;
            },
            None => return Err("connection closed during KEX confirmation".to_owned()),
        }
    }
    reliable.enable_application_traffic();

    let (send_key, recv_key) = derive_keys(&shared, false);
    let sealer = Sealer::new(send_key);
    let opener = Opener::new(recv_key);
    // Separate Data-channel key (see host session) — avoids nonce reuse.
    let (data_send_key, data_recv_key) = protocol::kex::derive_data_keys(&shared, false);
    let data_sealer = Sealer::new(data_send_key);
    let data_opener = Opener::strict(data_recv_key);
    let (control_send_key, control_recv_key) = protocol::kex::derive_control_keys(&shared, false);
    let control_sealer: SharedSealer = Arc::new(Mutex::new(Sealer::new(control_send_key)));
    let mut control_opener = Opener::strict(control_recv_key);
    // Always send one (empty = no code) so a 2FA host can answer at once
    // instead of waiting for its timeout; other hosts ignore it.
    let auth = Control::AuthCode { code: auth_code.unwrap_or_default() };
    sender.send_frame(Tag::Control, &seal_control(&control_sealer, &auth)).await.map_err(|error| error.to_string())?;
    // Started after AuthCode so a 2FA host reads the code as the first
    // control frame.
    let _keepalive = spawn_keepalive(sender.clone(), Arc::clone(&control_sealer), timings.keepalive);
    let (ice_rx, mut session_control_rx) = split_session_control(control_rx, Vec::new(), ice_session.to_owned());
    // Open session control frames in one place (one opener, in order) and
    // split an explicit host rejection (`Control::Error`) out of the stream so
    // every phase below can react to it promptly.
    let (control_plain_tx, control_plain_rx) = mpsc::channel::<Vec<u8>>(64);
    let (rejected_tx, mut rejected_rx) = mpsc::channel::<String>(1);
    let _control_demux = AbortOnDrop(tokio::spawn(async move {
        while let Some(body) = session_control_rx.recv().await {
            let Some(plaintext) = control_opener.open(&[Tag::Control as u8], &body) else { continue };
            if let Some(Control::Error { code }) = Control::from_bytes(&plaintext) {
                let _ = rejected_tx.send(code).await;
                break;
            }
            if control_plain_tx.send(plaintext).await.is_err() { break; }
        }
    }));

    let signaling: RelayIceSignaling<S> = RelayIceSignaling::new(sender.clone(), ice_rx, ice_session);
    let relay: RelayTransport<S> = RelayTransport::new(sender.clone(), reliable.take_media_rx().ok_or_else(|| "media receiver unavailable".to_owned())?);
    let establish = transport::establish(
        VIEWER_ICE_CONTROLLING,
        signaling,
        relay,
        Some(transport::FeedbackKeys::derive(&shared, false)),
        transport::default_agent_config(),
        Duration::from_secs(3),
    );
    let transport = tokio::select! {
        biased;
        Some(code) = rejected_rx.recv() => return Err(rejection_error(&code)),
        transport = establish => transport,
    };
    let path = transport.path();
    let (transport_tx, transport_rx) = transport.into_split();
    if let Some(ready) = data_ready {
        let _ = ready.send(DataHandle {
            sender: reliable.sender(),
            receiver: data_rx,
            sealer: data_sealer,
            opener: data_opener,
            control_sender: reliable.sender(),
            control_sealer: Arc::clone(&control_sealer),
            link: transport::LinkStatus::new(path, transport_tx.rtt_meter()),
        });
    }

    // The relay carries no feedback, so the viewer measures its round trip
    // itself. A direct path reports RTT through UDP feedback instead.
    let relay_ping = (path == transport::MediaPath::Relay).then_some(RELAY_PING_INTERVAL);
    let host_control = HostControl { sender: reliable.sender(), sealer: Arc::clone(&control_sealer), relay_ping };
    run_viewer_media_loop_with_control(transport_tx, transport_rx, frame_sink, input_rx, sealer, opener, control_plain_rx, rejected_rx, monitor_sink, audio_sink, aux_frame_sink, Some(host_control), timings.dead_peer).await
}

/// Ask the host for a keyframe, e.g. after a decoder error. Uses the
/// session's reliable control channel and control sealer (a viewer gets
/// both from its [`DataHandle`]: `control_sender` and `control_sealer`).
/// The host ignores requests within 250 ms of its last keyframe.
pub async fn request_keyframe(sender: &transport::reliable::ReliableSender, sealer: &SharedSealer) -> Result<(), transport::TransportError> {
    sender.send_frame(Tag::Control, &seal_control(sealer, &Control::RequestKeyframe)).await
}

/// What the viewer media loop needs to send control frames to the host:
/// keyframe requests, and pings while media goes over the relay.
struct HostControl {
    sender: transport::reliable::ReliableSender,
    sealer: SharedSealer,
    relay_ping: Option<Duration>,
}

/// Relay round trips from sealed `Ping`/`Pong` frames. The send time
/// travels in the ping and comes back in the pong, so a late pong still
/// gives a correct sample. A host from beta.5 or older never answers.
struct RelayPinger {
    clock: Instant,
}

impl RelayPinger {
    fn new(clock: Instant) -> Self { Self { clock } }

    fn micros(&self, now: Instant) -> u64 { now.saturating_duration_since(self.clock).as_micros() as u64 }

    fn ping(&self, now: Instant) -> Control { Control::Ping { sent_us: self.micros(now) } }

    /// The round trip of the ping a pong answers; `None` if it claims to be
    /// from the future.
    fn rtt(&self, sent_us: u64, now: Instant) -> Option<Duration> {
        self.micros(now).checked_sub(sent_us).map(Duration::from_micros)
    }
}

/// How long the viewer waits for a requested keyframe before asking again.
const KEYFRAME_RETRY: Duration = Duration::from_millis(500);

/// Viewer-side continuity check of the video sequence. A frame after a gap
/// (lost UDP fragments, a dropped frame) references data the decoder never
/// got, so frames are held back until the next keyframe and the host is
/// asked for one.
#[derive(Debug)]
struct VideoGate {
    next_seq: Option<u64>,
    awaiting_keyframe: bool,
    last_request: Option<Instant>,
}

impl VideoGate {
    fn new() -> Self { Self { next_seq: None, awaiting_keyframe: true, last_request: None } }

    /// Whether frame `seq` can be decoded.
    fn accept(&mut self, seq: u64, keyframe: bool) -> bool {
        if self.next_seq.is_some_and(|next| seq < next) {
            // Late or duplicate: the decoder has moved past it.
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

    /// An accepted frame never reached the decoder.
    fn lost(&mut self) { self.awaiting_keyframe = true; }

    /// Whether to (re)send a keyframe request now.
    fn should_request(&mut self, now: Instant) -> bool {
        if !self.awaiting_keyframe || self.last_request.is_some_and(|at| now.duration_since(at) < KEYFRAME_RETRY) {
            return false;
        }
        self.last_request = Some(now);
        true
    }
}

/// (seq, keyframe) from a video plaintext header (see `encode::video_header`).
fn video_seq(plaintext: &[u8]) -> Option<(u64, bool)> {
    let header = plaintext.get(..18)?;
    Some((u64::from_be_bytes(header[..8].try_into().ok()?), header[13] & 1 == 1))
}

#[allow(dead_code)]
async fn run_viewer_media_loop(
    transport_tx: Box<dyn TransportSender>,
    transport_rx: Box<dyn TransportReceiver>,
    frame_sink: tokio::sync::mpsc::Sender<Vec<u8>>,
    input_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    sealer: Sealer,
    opener: Opener,
) -> Result<(), String> {
    let (_tx, rx) = mpsc::channel(1);
    let (_rejected_tx, rejected_rx) = mpsc::channel(1);
    run_viewer_media_loop_with_control(transport_tx, transport_rx, frame_sink, input_rx, sealer, opener, rx, rejected_rx, None, None, None, None, DEAD_PEER_TIMEOUT).await
}

#[allow(clippy::too_many_arguments)]
async fn run_viewer_media_loop_with_control(
    mut transport_tx: Box<dyn TransportSender>,
    mut transport_rx: Box<dyn TransportReceiver>,
    frame_sink: tokio::sync::mpsc::Sender<Vec<u8>>,
    mut input_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    mut sealer: Sealer,
    mut opener: Opener,
    mut control_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    mut rejected_rx: tokio::sync::mpsc::Receiver<String>,
    monitor_sink: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    audio_sink: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    aux_frame_sink: Option<tokio::sync::mpsc::Sender<(u32, Vec<u8>)>>,
    host_control: Option<HostControl>,
    dead_peer: Duration,
) -> Result<(), String> {
    let rtt = transport_tx.rtt_meter();
    let pinger = RelayPinger::new(Instant::now());
    let relay_ping = host_control.as_ref().and_then(|control| control.relay_ping);
    let mut ping_ticks = tokio::time::interval(relay_ping.unwrap_or(RELAY_PING_INTERVAL));
    ping_ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The host keepalives on the relay socket even when media is direct, so
    // silence there means the host is gone: a direct transport never reports
    // a dead peer on its own.
    let mut peer_deadline = tokio::time::Instant::now() + dead_peer;
    let mut gate = VideoGate::new();
    // One continuity gate per aux (per-monitor) stream. Aux frames are
    // best-effort: dropped on a full sink and recovered on the host's next
    // periodic keyframe, so no per-stream keyframe request is needed.
    let mut aux_gates: std::collections::HashMap<u32, VideoGate> = std::collections::HashMap::new();
    // A decodable frame the frame sink had no room for yet. Video never
    // blocks this loop: input and control keep flowing while it waits.
    let mut pending_video: Option<Vec<u8>> = None;
    loop {
        tokio::select! {
            // Input is polled before media so a video burst never delays it.
            biased;
            Some(code) = rejected_rx.recv() => return Err(rejection_error(&code)),
            plaintext = control_rx.recv() => {
                let Some(plaintext) = plaintext else {
                    // The relay socket closed: the host is gone (or rejected
                    // us, which the demux reports just before closing).
                    if let Ok(code) = rejected_rx.try_recv() { return Err(rejection_error(&code)); }
                    return Err("connection closed: the host's control channel closed".into());
                };
                peer_deadline = tokio::time::Instant::now() + dead_peer;
                match Control::from_bytes(&plaintext) {
                    Some(Control::Bye { .. }) => return Err(crate::reconnect::HOST_ENDED_MESSAGE.into()),
                    Some(Control::MonitorList { .. } | Control::VirtualDisplaySupport { .. } | Control::VirtualDisplayAdded { .. } | Control::VirtualDisplayRefused { .. } | Control::SecureAttentionSupport
                        | Control::MultiMonitorSupport { .. } | Control::MonitorStreamOpened { .. } | Control::MonitorStreamClosed { .. } | Control::MonitorStreamRefused { .. }
                        | Control::RestartSupport | Control::Restarting
                        | Control::PrivacyModeSupport | Control::PrivacyModeChanged { .. }) => {
                        if let Some(sink) = &monitor_sink { let _ = sink.send(plaintext).await; }
                    }
                    Some(Control::Pong { sent_us }) if relay_ping.is_some() => {
                        let now = Instant::now();
                        if let Some(sample) = pinger.rtt(sent_us, now) { rtt.record(sample, now); }
                    }
                    _ => {}
                }
            },
            input = input_rx.recv() => {
                let Some(bytes) = input else { return Ok(()) };
                let sealed = sealer.seal(&[Tag::Input as u8], &bytes);
                if transport_tx.send(Tag::Input, &sealed).await.is_err() { return Err("viewer input transport closed".into()) }
            }
            _ = ping_ticks.tick(), if relay_ping.is_some() => {
                if let Some(control) = &host_control {
                    let ping = seal_control(&control.sealer, &pinger.ping(Instant::now()));
                    let _ = control.sender.send_frame(Tag::Control, &ping).await;
                }
            }
            _ = tokio::time::sleep_until(peer_deadline) => {
                return Err(format!("connection closed: host stopped responding for {}s", dead_peer.as_secs()));
            },
            permit = frame_sink.reserve(), if pending_video.is_some() => {
                let Ok(permit) = permit else { return Err("viewer frame channel closed".into()) };
                if let Some(frame) = pending_video.take() { permit.send(frame); }
            }
            received = transport_rx.recv() => {
                let Some((tag, sealed)) = received else {
                    // The host's rejection and its close can race through
                    // separate channels; prefer the explicit reason if it is
                    // already on its way.
                    if let Ok(Some(code)) = tokio::time::timeout(Duration::from_millis(300), rejected_rx.recv()).await {
                        return Err(rejection_error(&code));
                    }
                    return Err("connection closed during session".into())
                };
                let Some(plaintext) = opener.open(&[tag as u8], &sealed) else { continue };
                match tag {
                    Tag::Video => {
                        let Some((seq, keyframe)) = video_seq(&plaintext) else { continue };
                        if gate.accept(seq, keyframe) {
                            if pending_video.is_some() && !keyframe {
                                // The sink is still behind: this delta cannot
                                // be queued without breaking its reference.
                                gate.lost();
                            } else {
                                // A keyframe supersedes a stale pending frame.
                                pending_video = None;
                                match frame_sink.try_send(plaintext) {
                                    Ok(()) => {}
                                    Err(mpsc::error::TrySendError::Full(frame)) => pending_video = Some(frame),
                                    Err(mpsc::error::TrySendError::Closed(_)) => return Err("viewer frame channel closed".into()),
                                }
                            }
                        }
                        if gate.should_request(Instant::now()) {
                            if let Some(requests) = &host_control {
                                log::debug!("video frames lost; requesting a keyframe");
                                let _ = request_keyframe(&requests.sender, &requests.sealer).await;
                            }
                        }
                    }
                    // A per-monitor aux stream: strip the monitor id, gate on
                    // its own sequence, and hand off best-effort.
                    Tag::VideoAux => if let Some(sink) = &aux_frame_sink {
                        if plaintext.len() < 4 { continue; }
                        let id = u32::from_be_bytes([plaintext[0], plaintext[1], plaintext[2], plaintext[3]]);
                        let body = &plaintext[4..];
                        let Some((seq, keyframe)) = video_seq(body) else { continue };
                        let aux_gate = aux_gates.entry(id).or_insert_with(VideoGate::new);
                        if aux_gate.accept(seq, keyframe) {
                            match sink.try_send((id, body.to_vec())) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => aux_gate.lost(),
                                Err(mpsc::error::TrySendError::Closed(_)) => {}
                            }
                        }
                    },
                    // Audio is dropped rather than waited for.
                    Tag::Audio => if let Some(sink) = &audio_sink {
                        if let Err(mpsc::error::TrySendError::Closed(_)) = sink.try_send(plaintext) { return Err("viewer audio channel closed".into()) }
                    },
                    _ => {}
                }
            }
        }
    }
}

fn capture_unavailable_message() -> &'static str {
    #[cfg(target_os = "macos")]
    return "Screen capture is unavailable. Grant this app macOS Screen Recording permission and try again.";

    #[cfg(target_os = "windows")]
    return "Screen capture is unavailable. Windows Graphics Capture could not be started.";

    #[cfg(target_os = "linux")]
    return "Screen capture is unavailable. Approve the Linux desktop portal capture request and try again.";
}

#[cfg(target_os = "macos")]
fn report_input_unavailable() {
    log::warn!("input injection unavailable: macOS Accessibility permission is required; grant it in System Settings (x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility), then restart the host");
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn report_input_unavailable() {
    // Enigo is backed by SendInput on Windows and XTest/uinput on Linux.
    log::warn!("input injection unavailable: the platform input backend could not be initialized");
}

/// Build the production injector. Called on the injection thread, so the
/// thread-affine Enigo handle is created and used on one OS thread.
fn default_injector() -> Box<dyn InputInjector> {
    match EnigoInjector::new() {
        Some(injector) => Box::new(injector),
        None => {
            report_input_unavailable();
            Box::new(NoopInjector::default())
        }
    }
}

/// The production encoder for a stream of `width` x `height`, at the stream
/// frame rate with a bitrate scaled to the stream size.
fn default_encoder(width: u16, height: u16, codec: encode::Codec) -> Result<VideoEncoder, encode::EncoderInitError> {
    VideoEncoder::best_available(width, height, STREAM_FPS, stream::stream_bitrate(width, height, STREAM_FPS), codec)
}

/// Run a host session with the production capture, H.264 encoder, and Enigo injector.
pub async fn run_default_host_session<S>(
    ws: WebSocketStream<S>,
    password: &str,
    ice_session: &str,
) -> SessionResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    run_host_session_with_data_and_accept_and_2fa_with_factory(
        ws, password, ice_session, ScapCapturer::create,
        default_encoder,
        default_injector, false, false, None, || async { true }, None, SessionTimings::default(), HostDisplays::default(),
        SessionGates::default(),
    )
    .await
}

pub async fn run_default_host_session_with_data<S>(
    ws: WebSocketStream<S>,
    password: &str,
    ice_session: &str,
    data_ready: oneshot::Sender<DataHandle>,
) -> SessionResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    run_default_host_session_with_data_and_accept(
        ws, password, ice_session, false, || async { true }, data_ready,
    ).await
}

pub async fn run_default_host_session_with_data_and_accept<S, A, Fut>(
    ws: WebSocketStream<S>,
    password: &str,
    ice_session: &str,
    unattended_enabled: bool,
    approve: A,
    data_ready: oneshot::Sender<DataHandle>,
) -> SessionResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    A: FnOnce() -> Fut,
    Fut: Future<Output = bool>,
{
    run_default_host_session_with_data_and_accept_and_2fa(
        ws, password, ice_session, unattended_enabled, false, None, approve, data_ready,
    ).await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_default_host_session_with_data_and_accept_and_2fa<S, A, Fut>(
    ws: WebSocketStream<S>, password: &str, ice_session: &str,
    unattended_enabled: bool, twofa_enabled: bool, totp_secret: Option<&str>,
    approve: A, data_ready: oneshot::Sender<DataHandle>,
) -> SessionResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    A: FnOnce() -> Fut,
    Fut: Future<Output = bool>,
{
    run_default_host_session_with_displays(
        ws, password, ice_session, unattended_enabled, twofa_enabled, totp_secret, approve, data_ready,
        HostDisplays::platform(false), SessionGates::default(),
    ).await
}

/// Like [`run_default_host_session_with_data_and_accept_and_2fa`], with
/// the host's virtual display settings and host-enforced viewer permission
/// gates (control/audio), which the caller may flip live during the session.
#[allow(clippy::too_many_arguments)]
pub async fn run_default_host_session_with_displays<S, A, Fut>(
    ws: WebSocketStream<S>, password: &str, ice_session: &str,
    unattended_enabled: bool, twofa_enabled: bool, totp_secret: Option<&str>,
    approve: A, data_ready: oneshot::Sender<DataHandle>, displays: HostDisplays,
    gates: SessionGates,
) -> SessionResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    A: FnOnce() -> Fut,
    Fut: Future<Output = bool>,
{
    run_host_session_with_data_and_accept_and_2fa_with_factory(
        ws,
        password,
        ice_session,
        ScapCapturer::create,
        default_encoder,
        default_injector,
        unattended_enabled,
        twofa_enabled,
        totp_secret,
        approve,
        Some(data_ready),
        SessionTimings::default(),
        displays,
        gates,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::DirectTransport;
    use protocol::kex::derive_keys;
    use std::sync::Arc;
    use tokio::time::{timeout, Duration};
    use webrtc_util::conn::conn_pipe::pipe;

    #[test]
    fn host_and_viewer_ice_roles_are_opposite() {
        // Compile-time invariant: host and viewer MUST take opposite ICE
        // controlling roles, or ICE never nominates a candidate pair.
        const _: () = assert!(HOST_ICE_CONTROLLING != VIEWER_ICE_CONTROLLING);
    }

    #[test]
    fn unattended_auto_accept_requires_successful_pake() {
        assert!(should_auto_accept(true, true));
        assert!(!should_auto_accept(true, false));
        assert!(!should_auto_accept(false, true));
        assert!(!should_auto_accept(false, false));
    }

    #[test]
    fn allow_list_auto_accept_depends_only_on_membership() {
        assert!(should_auto_accept_allowlisted(true));
        assert!(!should_auto_accept_allowlisted(false));
    }

    #[test]
    fn totp_accepts_current_and_adjacent_steps_but_not_wrong_or_expired_codes() {
        // A real authenticator base32-decodes the enrolled secret to get the
        // HMAC key, so the expected codes must be computed over the DECODED key.
        let secret = "JBSWY3DPEHPK3PXP"; // valid RFC 4648 base32
        let key = decode_base32(secret).expect("valid base32 secret");
        let now = 1_700_000_000;
        let current = totp_lite::totp_custom::<totp_lite::Sha1>(30, 6, &key, now);
        let previous = totp_lite::totp_custom::<totp_lite::Sha1>(30, 6, &key, now - 30);
        let next = totp_lite::totp_custom::<totp_lite::Sha1>(30, 6, &key, now + 30);
        let expired = totp_lite::totp_custom::<totp_lite::Sha1>(30, 6, &key, now - 60);
        assert!(totp_is_valid(secret, &current, now));
        assert!(totp_is_valid(secret, &previous, now));
        assert!(totp_is_valid(secret, &next, now));
        assert!(!totp_is_valid(secret, "000000", now));
        assert!(!totp_is_valid(secret, &expired, now));
    }

    /// Known-answer test: RFC 6238 Appendix B SHA-1 vectors (truncated to 6
    /// digits) for the ASCII seed "12345678901234567890", base32-encoded.
    /// Also pins SHA-256 (FIPS 180-2 "abc"), used for file digests and the
    /// TOTP replay-cache key.
    #[test]
    fn totp_and_sha256_match_known_answers() {
        let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        for (time, code) in [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
        ] {
            assert_eq!(totp_lite::totp_custom::<totp_lite::Sha1>(30, 6, &decode_base32(secret).unwrap(), time), code);
            assert!(totp_is_valid(secret, code, time));
        }
        let digest: [u8; 32] = Sha256::digest(b"abc").into();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn totp_replay_guard_consumes_a_step_but_accepts_a_fresh_step() {
        let secret = "GEZDGNBVGY3TQOJQ";
        let key = decode_base32(secret).unwrap();
        let now = 1_700_000_000;
        let current = totp_lite::totp_custom::<totp_lite::Sha1>(30, 6, &key, now);
        let fresh = totp_lite::totp_custom::<totp_lite::Sha1>(30, 6, &key, now + 30);
        assert!(totp_is_valid_once(secret, &current, now));
        assert!(!totp_is_valid_once(secret, &current, now));
        assert!(totp_is_valid_once(secret, &fresh, now + 30));
    }

    #[test]
    fn twofa_gate_requires_a_valid_code() {
        assert!(should_auto_accept_with_2fa(true, true, false, false));
        assert!(should_auto_accept_with_2fa(true, true, true, true));
        assert!(!should_auto_accept_with_2fa(true, true, true, false));
        assert!(!should_auto_accept_with_2fa(false, true, true, true));
    }

    #[tokio::test]
    async fn viewer_media_loop_opens_video_and_seals_input() {
        let shared = [23u8; 32];
        let (viewer_send, viewer_recv) = derive_keys(&shared, false);
        let (host_send, host_recv) = derive_keys(&shared, true);
        let mut host_video_sealer = Sealer::new(host_send);
        let mut host_input_opener = Opener::new(host_recv);

        let (viewer_conn, host_conn) = pipe();
        let (viewer_tx, viewer_rx) = DirectTransport::new(Arc::new(viewer_conn)).into_split();
        let (mut host_tx, mut host_rx) = DirectTransport::new(Arc::new(host_conn)).into_split();
        let (frame_sink, mut frames) = tokio::sync::mpsc::channel(1);
        let (input_tx, input_rx) = tokio::sync::mpsc::channel(1);

        let viewer = tokio::spawn(run_viewer_media_loop(
            viewer_tx,
            viewer_rx,
            frame_sink,
            input_rx,
            Sealer::new(viewer_send),
            Opener::new(viewer_recv),
        ));

        let mut video = encode::video_header(0, 0, true, 64, 48, encode::Codec::H264);
        video.extend_from_slice(b"h264");
        let sealed_video = host_video_sealer.seal(&[Tag::Video as u8], &video);
        host_tx.send(Tag::Video, &sealed_video).await.unwrap();
        assert_eq!(timeout(Duration::from_secs(1), frames.recv()).await.unwrap(), Some(video));

        let input = b"encoded input event".to_vec();
        input_tx.send(input.clone()).await.unwrap();
        let (tag, sealed_input) = timeout(Duration::from_secs(1), host_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tag, Tag::Input);
        assert_eq!(host_input_opener.open(&[Tag::Input as u8], &sealed_input), Some(input));

        drop(input_tx);
        assert!(timeout(Duration::from_secs(1), viewer).await.unwrap().unwrap().is_ok());
    }

    struct ChannelReceiver(mpsc::Receiver<(Tag, Vec<u8>)>);

    #[async_trait::async_trait]
    impl TransportReceiver for ChannelReceiver {
        async fn recv(&mut self) -> Option<(Tag, Vec<u8>)> { self.0.recv().await }
    }

    #[tokio::test]
    async fn forward_input_coalesces_moves_but_keeps_keys_in_order() {
        let shared = [5u8; 32];
        let (viewer_send, _) = derive_keys(&shared, false);
        let (_, host_recv) = derive_keys(&shared, true);
        let mut viewer_sealer = Sealer::new(viewer_send);
        let (wire_tx, wire_rx) = mpsc::channel(16);
        // Capacity 1 and no consumer yet: the queue is full after one event.
        let (inject_tx, mut inject_rx) = mpsc::channel(1);
        let forwarder = tokio::spawn(forward_input(Box::new(ChannelReceiver(wire_rx)), Opener::new(host_recv), inject_tx, Arc::new(AtomicBool::new(true))));

        let key = |code| InputEvent::Key { code, down: true, modifiers: 0 };
        let mv = |x| InputEvent::MouseMove { x, y: x };
        for event in [key(65), mv(1), mv(2), mv(3), key(66)] {
            let sealed = viewer_sealer.seal(&[Tag::Input as u8], &event.to_bytes(0));
            wire_tx.send((Tag::Input, sealed)).await.unwrap();
        }
        let mut received = Vec::new();
        for _ in 0..3 {
            received.push(timeout(Duration::from_secs(1), inject_rx.recv()).await.unwrap().unwrap());
        }
        // Stale moves collapse to the latest one, flushed before the next key.
        assert_eq!(received, vec![key(65), mv(3), key(66)]);

        drop(wire_tx);
        assert_eq!(timeout(Duration::from_secs(1), forwarder).await.unwrap().unwrap(), InputForwardEnd::PeerGone);
    }

    #[tokio::test]
    async fn forward_input_reports_a_stopped_injection_thread() {
        let (wire_tx, wire_rx) = mpsc::channel(4);
        let (inject_tx, inject_rx) = mpsc::channel(1);
        drop(inject_rx);
        let shared = [6u8; 32];
        let (viewer_send, _) = derive_keys(&shared, false);
        let (_, host_recv) = derive_keys(&shared, true);
        let forwarder = tokio::spawn(forward_input(Box::new(ChannelReceiver(wire_rx)), Opener::new(host_recv), inject_tx, Arc::new(AtomicBool::new(true))));
        let event = InputEvent::Key { code: 65, down: true, modifiers: 0 };
        let sealed = Sealer::new(viewer_send).seal(&[Tag::Input as u8], &event.to_bytes(0));
        wire_tx.send((Tag::Input, sealed)).await.unwrap();
        assert_eq!(timeout(Duration::from_secs(1), forwarder).await.unwrap().unwrap(), InputForwardEnd::InjectorGone);
    }

    #[test]
    fn session_gates_default_to_enabled() {
        let gates = SessionGates::default();
        assert!(gates.control.load(Ordering::Relaxed));
        assert!(gates.audio.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn forward_input_drops_events_while_control_is_gated_off() {
        let shared = [9u8; 32];
        let (viewer_send, _) = derive_keys(&shared, false);
        let (_, host_recv) = derive_keys(&shared, true);
        let mut viewer_sealer = Sealer::new(viewer_send);
        let (wire_tx, wire_rx) = mpsc::channel(16);
        let (inject_tx, mut inject_rx) = mpsc::channel(4);
        let control = Arc::new(AtomicBool::new(false));
        let forwarder = tokio::spawn(forward_input(Box::new(ChannelReceiver(wire_rx)), Opener::new(host_recv), inject_tx, Arc::clone(&control)));

        let key = |code| InputEvent::Key { code, down: true, modifiers: 0 };
        // Gated off: this event must never reach the injector.
        let sealed = viewer_sealer.seal(&[Tag::Input as u8], &key(65).to_bytes(0));
        wire_tx.send((Tag::Input, sealed)).await.unwrap();
        assert!(timeout(Duration::from_millis(200), inject_rx.recv()).await.is_err());

        // Turning the gate back on resumes injection.
        control.store(true, Ordering::Relaxed);
        let sealed = viewer_sealer.seal(&[Tag::Input as u8], &key(66).to_bytes(0));
        wire_tx.send((Tag::Input, sealed)).await.unwrap();
        assert_eq!(timeout(Duration::from_secs(1), inject_rx.recv()).await.unwrap().unwrap(), key(66));

        drop(wire_tx);
        assert_eq!(timeout(Duration::from_secs(1), forwarder).await.unwrap().unwrap(), InputForwardEnd::PeerGone);
    }

    #[test]
    fn injection_thread_uses_latest_dims_and_exits_when_senders_drop() {
        struct Probe(std::sync::mpsc::Sender<(u32, u32)>);
        impl InputInjector for Probe {
            fn inject(&mut self, _ev: &InputEvent, w: u32, h: u32) { let _ = self.0.send((w, h)); }
        }
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let dims = Arc::new(AtomicU32::new(pack_dims(640, 480)));
        let display = Arc::new(AtomicU64::new(pack_display(None)));
        let tx = spawn_injection_thread(move || Box::new(Probe(seen_tx)) as Box<dyn InputInjector>, Arc::clone(&dims), display).unwrap();
        let event = InputEvent::MouseButton { button: 1, down: true };
        tx.blocking_send(event).unwrap();
        assert_eq!(seen_rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap(), (640, 480));
        dims.store(pack_dims(1920, 1080), Ordering::Release);
        tx.blocking_send(event).unwrap();
        assert_eq!(seen_rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap(), (1920, 1080));
        drop(tx);
        // The injector (and its probe sender) drops when the thread exits.
        assert_eq!(seen_rx.recv_timeout(std::time::Duration::from_secs(1)), Err(std::sync::mpsc::RecvTimeoutError::Disconnected));
    }

    #[test]
    fn injection_thread_tells_the_injector_the_selected_display() {
        struct Probe(std::sync::mpsc::Sender<Option<u32>>);
        impl InputInjector for Probe {
            fn inject(&mut self, _ev: &InputEvent, _w: u32, _h: u32) {}
            fn select_display(&mut self, display: Option<u32>) { let _ = self.0.send(display); }
        }
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let dims = Arc::new(AtomicU32::new(pack_dims(640, 480)));
        let display = Arc::new(AtomicU64::new(pack_display(None)));
        let tx = spawn_injection_thread(move || Box::new(Probe(seen_tx)) as Box<dyn InputInjector>, dims, Arc::clone(&display)).unwrap();
        let event = InputEvent::MouseMove { x: 0, y: 0 };
        let wait = std::time::Duration::from_secs(1);
        // The main display is the injector's default: nothing to report yet.
        tx.blocking_send(event).unwrap();
        display.store(pack_display(Some(0)), Ordering::Release);
        tx.blocking_send(event).unwrap();
        tx.blocking_send(event).unwrap();
        assert_eq!(seen_rx.recv_timeout(wait).unwrap(), Some(0));
        display.store(pack_display(Some(u32::MAX)), Ordering::Release);
        tx.blocking_send(event).unwrap();
        assert_eq!(seen_rx.recv_timeout(wait).unwrap(), Some(u32::MAX));
        drop(tx);
        // Reported once per change, then the thread exits.
        assert_eq!(seen_rx.recv_timeout(wait), Err(std::sync::mpsc::RecvTimeoutError::Disconnected));
    }

    /// (seq, keyframe, width, height) from a video plaintext header.
    fn video_header_fields(plaintext: &[u8]) -> (u64, bool, u16, u16) {
        (
            u64::from_be_bytes(plaintext[0..8].try_into().unwrap()),
            plaintext[13] & 1 == 1,
            u16::from_be_bytes([plaintext[14], plaintext[15]]),
            u16::from_be_bytes([plaintext[16], plaintext[17]]),
        )
    }

    #[test]
    fn video_pipeline_follows_capture_size_changes_and_repeats_static_frames() {
        use crate::capture::{FakeCapturer, ScreenCapturer};
        let mut capturer = FakeCapturer::new(64, 48);
        let dims = Arc::new(AtomicU32::new(0));
        let first = even_frame(capturer.next_frame().unwrap()).unwrap();
        let mut video = VideoPipeline::new(
            first,
            |w, h, _codec| encode::H264Encoder::new(w, h, 15, 1_000_000),
            Arc::clone(&dims),
        ).unwrap();
        assert_eq!(unpack_dims(dims.load(Ordering::Acquire)), (64, 48));

        let first = video.next(None).unwrap().expect("first frame encodes");
        assert_eq!(video_header_fields(&first), (0, true, 64, 48));
        // No new capture: the last frame is repeated, not skipped.
        let repeat = video.next(None).unwrap().expect("repeated frame encodes");
        assert_eq!(video_header_fields(&repeat), (1, false, 64, 48));

        // The display changes size (an odd width is cropped to even).
        capturer.set_size(97, 32);
        let resized = video.next(capturer.next_frame()).unwrap().expect("resized frame encodes");
        assert_eq!(video_header_fields(&resized), (2, true, 96, 32));
        assert_eq!(unpack_dims(dims.load(Ordering::Acquire)), (96, 32));
        let repeat = video.next(None).unwrap().expect("repeated frame encodes");
        assert_eq!(video_header_fields(&repeat), (3, false, 96, 32));
    }

    #[test]
    fn video_pipeline_turns_encoder_init_failure_into_an_error() {
        use crate::capture::{FakeCapturer, ScreenCapturer};
        // Only 64x48 encoders can be created.
        let factory = |w, h, _codec| {
            if (w, h) == (64, 48) {
                encode::H264Encoder::new(w, h, 15, 1_000_000)
            } else {
                Err(encode::EncoderInitError::new("unsupported size"))
            }
        };
        let mut capturer = FakeCapturer::new(32, 32);
        let dims = Arc::new(AtomicU32::new(0));
        let small = even_frame(capturer.next_frame().unwrap()).unwrap();
        assert!(VideoPipeline::new(small, factory, Arc::clone(&dims)).is_err());

        capturer.set_size(64, 48);
        let frame = even_frame(capturer.next_frame().unwrap()).unwrap();
        let mut video = VideoPipeline::new(frame, factory, dims).unwrap();
        capturer.set_size(32, 32);
        assert!(video.next(capturer.next_frame()).is_err());
    }

    #[test]
    fn video_pipeline_scales_large_captures_but_maps_input_to_the_capture() {
        use crate::capture::{FakeCapturer, ScreenCapturer};
        let mut capturer = FakeCapturer::new(3840, 2160);
        let dims = Arc::new(AtomicU32::new(0));
        let mut video = VideoPipeline::new(
            capturer.next_frame().unwrap(),
            |w, h, _codec| encode::H264Encoder::new(w, h, STREAM_FPS, stream::stream_bitrate(w, h, STREAM_FPS)),
            Arc::clone(&dims),
        ).unwrap();
        let first = video.next(None).unwrap().expect("encodes");
        assert_eq!(video_header_fields(&first), (0, true, 1920, 1080));
        assert_eq!(unpack_dims(dims.load(Ordering::Acquire)), (3840, 2160));

        // A smaller capture is streamed as is.
        capturer.set_size(1280, 720);
        let small = video.next(capturer.next_frame()).unwrap().expect("encodes");
        assert_eq!(video_header_fields(&small), (1, true, 1280, 720));
        assert_eq!(unpack_dims(dims.load(Ordering::Acquire)), (1280, 720));
    }

    #[test]
    fn video_pipeline_honours_keyframe_requests_with_a_rate_limit() {
        use crate::capture::{FakeCapturer, ScreenCapturer};
        let mut capturer = FakeCapturer::new(64, 48);
        let mut video = VideoPipeline::new(
            capturer.next_frame().unwrap(),
            |w, h, _codec| encode::H264Encoder::new(w, h, STREAM_FPS, 1_000_000),
            Arc::new(AtomicU32::new(0)),
        ).unwrap();
        assert!(video_header_fields(&video.next(None).unwrap().unwrap()).1);
        // Too soon after the first keyframe: ignored.
        assert!(!video.request_keyframe());
        assert!(!video_header_fields(&video.next(None).unwrap().unwrap()).1);
        std::thread::sleep(stream::MIN_REQUESTED_KEYFRAME_SPACING);
        assert!(video.request_keyframe());
        assert!(video.keyframe_due());
        assert!(video_header_fields(&video.next(None).unwrap().unwrap()).1, "requested keyframe");
        assert!(!video.keyframe_due());
        assert!(!video_header_fields(&video.next(None).unwrap().unwrap()).1);
    }

    #[test]
    fn video_gate_drops_frames_after_a_gap_until_a_keyframe() {
        let mut gate = VideoGate::new();
        let now = Instant::now();
        // A stream must start at a keyframe.
        assert!(!gate.accept(0, false));
        assert!(gate.should_request(now));
        assert!(gate.accept(1, true));
        assert!(gate.accept(2, false));
        assert!(!gate.should_request(now + KEYFRAME_RETRY));
        // Frame 3 is lost: 4 and 5 reference it.
        assert!(!gate.accept(4, false));
        assert!(gate.should_request(now + KEYFRAME_RETRY));
        assert!(!gate.accept(5, false));
        assert!(!gate.should_request(now + KEYFRAME_RETRY), "requests are rate limited");
        assert!(gate.should_request(now + KEYFRAME_RETRY * 2), "an unanswered request is repeated");
        // A late frame 3 does not resume the stream either.
        assert!(!gate.accept(3, false));
        assert!(gate.accept(6, true));
        assert!(gate.accept(7, false));
        // Duplicates and stale frames are ignored without a request.
        assert!(!gate.accept(7, false));
        assert!(!gate.accept(1, true));
        assert!(!gate.should_request(now + KEYFRAME_RETRY * 3));
    }

    #[test]
    fn video_gate_waits_for_a_keyframe_after_a_local_drop() {
        let mut gate = VideoGate::new();
        assert!(gate.accept(0, true));
        gate.lost();
        assert!(!gate.accept(1, false));
        assert!(gate.should_request(Instant::now()));
        assert!(gate.accept(2, true));
    }

    #[test]
    fn video_seq_reads_the_header() {
        let mut plaintext = encode::video_header(42, 7, true, 64, 48, encode::Codec::H264);
        plaintext.extend_from_slice(b"au");
        assert_eq!(video_seq(&plaintext), Some((42, true)));
        assert_eq!(video_seq(&encode::video_header(43, 7, false, 64, 48, encode::Codec::H264)), Some((43, false)));
        assert_eq!(video_seq(&[0; 17]), None);
    }

    #[tokio::test]
    async fn unit_slot_hands_over_one_unit_at_a_time() {
        let slot = Arc::new(UnitSlot::default());
        assert_eq!(slot.wait_space(Duration::ZERO), SlotSpace::Free);
        slot.put(vec![1]);
        assert_eq!(slot.wait_space(Duration::from_millis(5)), SlotSpace::Full);
        assert_eq!(slot.take().await.unwrap(), vec![1]);
        assert_eq!(slot.wait_space(Duration::ZERO), SlotSpace::Free);

        // A waiting producer wakes as soon as the unit is taken.
        slot.put(vec![2]);
        let producer = {
            let slot = Arc::clone(&slot);
            std::thread::spawn(move || slot.wait_space(Duration::from_secs(5)))
        };
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(slot.take().await.unwrap(), vec![2]);
        assert_eq!(producer.join().unwrap(), SlotSpace::Free);

        slot.fail("encoder broke".into());
        assert_eq!(slot.wait_space(Duration::ZERO), SlotSpace::Closed);
        assert_eq!(slot.take().await.unwrap_err().to_string(), "encoder broke");
    }

    #[tokio::test]
    async fn unit_slot_take_waits_for_a_unit() {
        let slot = Arc::new(UnitSlot::default());
        let consumer = tokio::spawn({
            let slot = Arc::clone(&slot);
            async move { slot.take().await.unwrap() }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        slot.put(vec![9]);
        assert_eq!(timeout(Duration::from_secs(1), consumer).await.unwrap().unwrap(), vec![9]);
    }

    #[tokio::test]
    async fn host_confirmation_reaches_a_viewer_with_the_wrong_password() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{protocol::Role, Message};
        let (viewer_ws, host_ws) = tokio::io::duplex(64 * 1024);
        let mut viewer_ws = WebSocketStream::from_raw_socket(viewer_ws, Role::Client, None).await;
        let host_ws = WebSocketStream::from_raw_socket(host_ws, Role::Server, None).await;
        // The wrong-password viewer's message and confirmation arrive at
        // once, so the host fails verification right after queueing its own.
        let (_, msg) = protocol::pake::start(b"wrong", false);
        for body in [kex_body(KEX_MSG, &msg), kex_body(KEX_CONFIRM, &[0u8; 32])] {
            viewer_ws.send(Message::Binary(protocol::frame::wrap(Tag::Kex, &body).into())).await.unwrap();
        }
        let host = run_host_session(
            host_ws, "right", "s", crate::capture::FakeCapturer::new(32, 32),
            |width, height, _codec| crate::encode::H264Encoder::new(width, height, 15, 1_000_000),
            crate::inject::NoopInjector::default(),
        ).await;
        assert!(host.is_err());
        let mut confirmed = false;
        while let Ok(Some(Ok(message))) = timeout(Duration::from_secs(1), viewer_ws.next()).await {
            let data = message.into_data();
            if let Some((Tag::Kex, body)) = protocol::frame::unwrap(&data) {
                confirmed |= matches!(parse_kex(body), Some((KEX_CONFIRM, _)));
            }
        }
        assert!(confirmed, "the viewer must get the host's confirmation, not a bare close");
    }

    #[tokio::test]
    async fn viewer_requests_a_keyframe_after_a_sequence_gap() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::protocol::Role;
        let shared = [31u8; 32];
        let (viewer_send, viewer_recv) = derive_keys(&shared, false);
        let (host_send, _) = derive_keys(&shared, true);
        let mut host_video_sealer = Sealer::new(host_send);
        let (viewer_control_send, _) = protocol::kex::derive_control_keys(&shared, false);
        let (_, host_control_recv) = protocol::kex::derive_control_keys(&shared, true);

        // The viewer's control channel, read raw on the host end.
        let (viewer_ws, host_ws) = tokio::io::duplex(64 * 1024);
        let viewer_ws = WebSocketStream::from_raw_socket(viewer_ws, Role::Client, None).await;
        let mut host_ws = WebSocketStream::from_raw_socket(host_ws, Role::Server, None).await;
        let reliable = ReliableChannel::spawn(viewer_ws);
        let requests = HostControl { sender: reliable.sender(), sealer: Arc::new(Mutex::new(Sealer::new(viewer_control_send))), relay_ping: None };

        let (viewer_conn, host_conn) = pipe();
        let (viewer_tx, viewer_rx) = DirectTransport::new(Arc::new(viewer_conn)).into_split();
        let (mut host_tx, _host_rx) = DirectTransport::new(Arc::new(host_conn)).into_split();
        let (frame_sink, mut frames) = mpsc::channel(8);
        let (_input_tx, input_rx) = mpsc::channel(1);
        let (_control_tx, control_rx) = mpsc::channel(1);
        let (_rejected_tx, rejected_rx) = mpsc::channel(1);
        let viewer = tokio::spawn(run_viewer_media_loop_with_control(
            viewer_tx, viewer_rx, frame_sink, input_rx, Sealer::new(viewer_send), Opener::new(viewer_recv),
            control_rx, rejected_rx, None, None, None, Some(requests), DEAD_PEER_TIMEOUT,
        ));

        for (seq, keyframe) in [(0, true), (1, false), (3, false), (4, true)] {
            let plaintext = encode::video_header(seq, 0, keyframe, 64, 48, encode::Codec::H264);
            let sealed = host_video_sealer.seal(&[Tag::Video as u8], &plaintext);
            host_tx.send(Tag::Video, &sealed).await.unwrap();
        }
        let mut seqs = Vec::new();
        for _ in 0..3 {
            let frame = timeout(Duration::from_secs(1), frames.recv()).await.unwrap().unwrap();
            seqs.push(video_seq(&frame).unwrap().0);
        }
        assert_eq!(seqs, [0, 1, 4], "frame 3 follows a gap and is held back");

        let message = timeout(Duration::from_secs(1), host_ws.next()).await.unwrap().unwrap().unwrap();
        let (tag, body) = protocol::frame::unwrap(&message.into_data()).map(|(tag, body)| (tag, body.to_vec())).unwrap();
        assert_eq!(tag, Tag::Control);
        let plaintext = Opener::new(host_control_recv).open(&[Tag::Control as u8], &body).unwrap();
        assert_eq!(Control::from_bytes(&plaintext), Some(Control::RequestKeyframe));
        viewer.abort();
    }

    /// A viewer media loop whose control frames are read raw on the host
    /// end. Returns the loop, the viewer's RTT meter, a way to feed it host
    /// control plaintext, and a reader for the control frames it sends.
    struct PingRig {
        viewer: tokio::task::JoinHandle<Result<(), String>>,
        rtt: Arc<transport::RttMeter>,
        host_control: mpsc::Sender<Vec<u8>>,
        host_ws: WebSocketStream<tokio::io::DuplexStream>,
        host_opener: Opener,
        _input: mpsc::Sender<Vec<u8>>,
        _rejected: mpsc::Sender<String>,
        _reliable: ReliableChannel<tokio::io::DuplexStream>,
        // Dropping the far end would end the loop at once.
        _host_conn: Arc<dyn webrtc_util::conn::Conn + Send + Sync>,
    }

    impl PingRig {
        async fn start(relay_ping: Option<Duration>) -> Self {
            use tokio_tungstenite::tungstenite::protocol::Role;
            let shared = [33u8; 32];
            let (viewer_send, viewer_recv) = derive_keys(&shared, false);
            let (viewer_control_send, _) = protocol::kex::derive_control_keys(&shared, false);
            let (_, host_control_recv) = protocol::kex::derive_control_keys(&shared, true);
            let (viewer_ws, host_ws) = tokio::io::duplex(64 * 1024);
            let viewer_ws = WebSocketStream::from_raw_socket(viewer_ws, Role::Client, None).await;
            let host_ws = WebSocketStream::from_raw_socket(host_ws, Role::Server, None).await;
            let reliable = ReliableChannel::spawn(viewer_ws);
            let control = HostControl { sender: reliable.sender(), sealer: Arc::new(Mutex::new(Sealer::new(viewer_control_send))), relay_ping };
            let (viewer_conn, host_conn) = pipe();
            let (viewer_tx, viewer_rx) = DirectTransport::new(Arc::new(viewer_conn)).into_split();
            let rtt = viewer_tx.rtt_meter();
            let (frame_sink, _frames) = mpsc::channel(1);
            let (input, input_rx) = mpsc::channel(1);
            let (host_control, control_rx) = mpsc::channel(4);
            let (rejected, rejected_rx) = mpsc::channel(1);
            let viewer = tokio::spawn(run_viewer_media_loop_with_control(
                viewer_tx, viewer_rx, frame_sink, input_rx, Sealer::new(viewer_send), Opener::new(viewer_recv),
                control_rx, rejected_rx, None, None, None, Some(control), DEAD_PEER_TIMEOUT,
            ));
            Self { viewer, rtt, host_control, host_ws, host_opener: Opener::new(host_control_recv), _input: input, _rejected: rejected, _reliable: reliable, _host_conn: Arc::new(host_conn) }
        }

        /// The next control frame the viewer sent, if one comes soon.
        async fn next_control(&mut self) -> Option<Control> {
            use futures_util::StreamExt;
            let message = timeout(Duration::from_millis(300), self.host_ws.next()).await.ok()??.ok()?;
            let (tag, body) = protocol::frame::unwrap(&message.into_data()).map(|(tag, body)| (tag, body.to_vec()))?;
            assert_eq!(tag, Tag::Control);
            Control::from_bytes(&self.host_opener.open(&[Tag::Control as u8], &body)?)
        }
    }

    #[tokio::test]
    async fn viewer_measures_the_relay_rtt_with_ping_and_pong() {
        let mut rig = PingRig::start(Some(Duration::from_millis(50))).await;
        assert_eq!(rig.rtt.latest(Instant::now()), None);
        let Some(Control::Ping { sent_us }) = rig.next_control().await else { panic!("expected a ping") };
        tokio::time::sleep(Duration::from_millis(20)).await;
        rig.host_control.send(Control::Pong { sent_us }.to_bytes()).await.unwrap();
        let rtt = timeout(Duration::from_secs(1), async {
            loop {
                if let Some(rtt) = rig.rtt.latest(Instant::now()) { break rtt; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a pong gives an RTT");
        assert!(rtt >= Duration::from_millis(20) && rtt < Duration::from_secs(1), "{rtt:?}");
        rig.viewer.abort();
    }

    #[tokio::test]
    async fn an_old_host_that_never_answers_leaves_the_rtt_unknown() {
        let mut rig = PingRig::start(Some(Duration::from_millis(50))).await;
        for _ in 0..3 {
            assert!(matches!(rig.next_control().await, Some(Control::Ping { .. })));
        }
        // A beta.5 host keeps the session going (keepalives) but never pongs.
        rig.host_control.send(Control::KeepAlive.to_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(rig.rtt.latest(Instant::now()), None);
        assert!(!rig.viewer.is_finished());
        rig.viewer.abort();
    }

    #[tokio::test]
    async fn viewer_does_not_ping_when_media_is_direct() {
        let mut rig = PingRig::start(None).await;
        assert_eq!(rig.next_control().await, None);
        // A stray pong is not about the media path.
        rig.host_control.send(Control::Pong { sent_us: 0 }.to_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(rig.rtt.latest(Instant::now()), None);
        rig.viewer.abort();
    }

    #[test]
    fn relay_pinger_rejects_pongs_from_the_future() {
        let start = Instant::now();
        let pinger = RelayPinger::new(start);
        let Control::Ping { sent_us } = pinger.ping(start + Duration::from_millis(5)) else { unreachable!() };
        assert_eq!(pinger.rtt(sent_us, start + Duration::from_millis(35)), Some(Duration::from_millis(30)));
        assert_eq!(pinger.rtt(sent_us, start), None);
    }

    #[tokio::test]
    async fn viewer_input_flows_while_the_frame_sink_is_full() {
        let shared = [32u8; 32];
        let (viewer_send, viewer_recv) = derive_keys(&shared, false);
        let (host_send, host_recv) = derive_keys(&shared, true);
        let mut host_video_sealer = Sealer::new(host_send);
        let mut host_input_opener = Opener::new(host_recv);
        let (viewer_conn, host_conn) = pipe();
        let (viewer_tx, viewer_rx) = DirectTransport::new(Arc::new(viewer_conn)).into_split();
        let (mut host_tx, mut host_rx) = DirectTransport::new(Arc::new(host_conn)).into_split();
        // Nobody drains the frame sink.
        let (frame_sink, _frames) = mpsc::channel(1);
        let (input_tx, input_rx) = mpsc::channel(4);
        let viewer = tokio::spawn(run_viewer_media_loop(
            viewer_tx, viewer_rx, frame_sink, input_rx, Sealer::new(viewer_send), Opener::new(viewer_recv),
        ));
        for seq in 0..10 {
            let sealed = host_video_sealer.seal(&[Tag::Video as u8], &encode::video_header(seq, 0, seq == 0, 64, 48, encode::Codec::H264));
            host_tx.send(Tag::Video, &sealed).await.unwrap();
        }
        input_tx.send(b"click".to_vec()).await.unwrap();
        let (tag, sealed) = timeout(Duration::from_secs(1), host_rx.recv()).await.expect("input is not blocked by video").unwrap();
        assert_eq!(tag, Tag::Input);
        assert_eq!(host_input_opener.open(&[Tag::Input as u8], &sealed), Some(b"click".to_vec()));
        viewer.abort();
    }

    mod liveness {
        use super::*;
        use crate::{capture::FakeCapturer, encode::H264Encoder, inject::NoopInjector};
        use futures_util::{SinkExt, StreamExt};
        use std::sync::atomic::AtomicBool;
        use tokio::io::DuplexStream;
        use tokio::task::JoinHandle;
        use tokio_tungstenite::tungstenite::{protocol::Role, Message};

        const PASSWORD: &str = "liveness-pw";
        const SESSION: &str = "liveness";

        /// In-process stand-in for the relay: forwards Binary messages between
        /// the peers and, like the relay, drops both sockets once either side
        /// has sent nothing for `idle`.
        struct TestRelay {
            idle_fired: Arc<AtomicBool>,
            blackhole: Arc<AtomicBool>,
            task: JoinHandle<()>,
        }

        impl TestRelay {
            async fn start(idle: Duration) -> (Self, WebSocketStream<DuplexStream>, WebSocketStream<DuplexStream>) {
                Self::start_with(idle, true).await
            }

            /// With `ice` false the relay drops ICE candidates, so the session
            /// falls back to relayed media.
            async fn start_with(idle: Duration, ice: bool) -> (Self, WebSocketStream<DuplexStream>, WebSocketStream<DuplexStream>) {
                let (host_end, host_relay) = tokio::io::duplex(8 * 1024 * 1024);
                let (viewer_end, viewer_relay) = tokio::io::duplex(8 * 1024 * 1024);
                let host_ws = WebSocketStream::from_raw_socket(host_end, Role::Client, None).await;
                let viewer_ws = WebSocketStream::from_raw_socket(viewer_end, Role::Client, None).await;
                let (mut to_host, mut from_host) = WebSocketStream::from_raw_socket(host_relay, Role::Server, None).await.split();
                let (mut to_viewer, mut from_viewer) = WebSocketStream::from_raw_socket(viewer_relay, Role::Server, None).await.split();
                let idle_fired = Arc::new(AtomicBool::new(false));
                let blackhole = Arc::new(AtomicBool::new(false));
                let (task_idle, task_blackhole) = (Arc::clone(&idle_fired), Arc::clone(&blackhole));
                let task = tokio::spawn(async move {
                    let mut host_seen = tokio::time::Instant::now();
                    let mut viewer_seen = host_seen;
                    loop {
                        tokio::select! {
                            message = from_host.next() => {
                                host_seen = tokio::time::Instant::now();
                                let Some(Ok(message)) = message else { break };
                                if message.is_binary() && !task_blackhole.load(Ordering::SeqCst) && (ice || !is_ice(&message)) && to_viewer.send(message).await.is_err() { break; }
                            }
                            message = from_viewer.next() => {
                                viewer_seen = tokio::time::Instant::now();
                                let Some(Ok(message)) = message else { break };
                                if message.is_binary() && !task_blackhole.load(Ordering::SeqCst) && (ice || !is_ice(&message)) && to_host.send(message).await.is_err() { break; }
                            }
                            _ = tokio::time::sleep_until(host_seen.min(viewer_seen) + idle) => {
                                task_idle.store(true, Ordering::SeqCst);
                                break;
                            }
                        }
                    }
                    // Dropping the relay halves closes both peers' sockets.
                    let _ = to_host.send(Message::Close(None)).await;
                    let _ = to_viewer.send(Message::Close(None)).await;
                });
                (Self { idle_fired, blackhole, task }, host_ws, viewer_ws)
            }

            /// Close both sockets now, as the relay does when a peer leaves.
            fn close(&self) { self.task.abort(); }

            /// Keep both sockets open but stop forwarding: a silently dead peer.
            fn blackhole(&self) { self.blackhole.store(true, Ordering::SeqCst); }
        }

        /// ICE candidates are the only unsealed control frames in a session.
        fn is_ice(message: &Message) -> bool {
            let data = message.clone().into_data();
            matches!(protocol::frame::unwrap(&data), Some((Tag::Control, body)) if matches!(Control::from_bytes(body), Some(Control::Ice { .. })))
        }

        fn spawn_host(ws: WebSocketStream<DuplexStream>, timings: SessionTimings, ready: Option<oneshot::Sender<DataHandle>>) -> JoinHandle<SessionResult<()>> {
            spawn_host_approving(ws, timings, ready, || async { true })
        }

        fn spawn_host_approving<A, Fut>(ws: WebSocketStream<DuplexStream>, timings: SessionTimings, ready: Option<oneshot::Sender<DataHandle>>, approve: A) -> JoinHandle<SessionResult<()>>
        where
            A: FnOnce() -> Fut + Send + 'static,
            Fut: Future<Output = bool> + Send + 'static,
        {
            tokio::spawn(async move {
                run_host_session_with_data_and_accept_and_2fa_with_factory(
                    ws, PASSWORD, SESSION, || Some(FakeCapturer::new(64, 64)),
                    |width, height, _codec| H264Encoder::new(width, height, 15, 1_000_000),
                    || Box::new(NoopInjector::default()) as Box<dyn InputInjector>,
                    false, false, None, approve, ready, timings, HostDisplays::default(), SessionGates::default(),
                ).await
            })
        }

        /// An approval that signals when it is asked and never answers.
        fn unanswered(asked: oneshot::Sender<()>) -> impl FnOnce() -> std::future::Pending<bool> + Send + 'static {
            move || {
                let _ = asked.send(());
                std::future::pending()
            }
        }

        struct Viewer {
            task: JoinHandle<Result<(), ViewerSessionError>>,
            frames: mpsc::Receiver<Vec<u8>>,
            _input: mpsc::Sender<Vec<u8>>,
        }

        fn spawn_viewer(ws: WebSocketStream<DuplexStream>, timings: SessionTimings) -> Viewer {
            let (frame_tx, frames) = mpsc::channel(64);
            let (input, input_rx) = mpsc::channel(4);
            let task = tokio::spawn(async move {
                run_viewer_session_inner(ws, PASSWORD, SESSION, frame_tx, input_rx, None, None, None, None, None, timings)
                    .await
                    .map_err(viewer_session_error)
            });
            Viewer { task, frames, _input: input }
        }

        async fn first_frame(viewer: &mut Viewer) {
            timeout(Duration::from_secs(20), viewer.frames.recv())
                .await
                .expect("timed out waiting for the first video frame")
                .expect("viewer frame sink closed");
        }

        fn drain_frames(viewer: &mut Viewer) { while viewer.frames.try_recv().is_ok() {} }

        #[tokio::test(flavor = "multi_thread")]
        async fn viewer_keyframe_request_gets_a_keyframe() {
            let timings = SessionTimings::default();
            let (_relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_secs(3600)).await;
            let host = spawn_host(host_ws, timings, None);
            let (frame_tx, mut frames) = mpsc::channel(64);
            let (_input, input_rx) = mpsc::channel(4);
            let (ready_tx, ready_rx) = oneshot::channel();
            let viewer = tokio::spawn(async move {
                run_viewer_session_inner(viewer_ws, PASSWORD, SESSION, frame_tx, input_rx, Some(ready_tx), None, None, None, None, timings).await
            });
            let handle = timeout(Duration::from_secs(20), ready_rx).await.unwrap().unwrap();
            let first = timeout(Duration::from_secs(20), frames.recv()).await.unwrap().unwrap();
            assert!(video_header_fields(&first).1, "the stream starts with a keyframe");

            // Past the host's rate limit, well before its periodic keyframe.
            tokio::time::sleep(stream::MIN_REQUESTED_KEYFRAME_SPACING + Duration::from_millis(50)).await;
            while frames.try_recv().is_ok() {}
            request_keyframe(&handle.control_sender, &handle.control_sealer).await.unwrap();
            timeout(Duration::from_secs(2), async {
                loop {
                    let frame = frames.recv().await.expect("viewer frame sink closed");
                    let (seq, keyframe, _, _) = video_header_fields(&frame);
                    if keyframe && seq > 0 { break; }
                }
            })
            .await
            .expect("the host answers a keyframe request");
            host.abort();
            viewer.abort();
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn relayed_viewer_measures_the_rtt_through_the_host() {
            let timings = SessionTimings::default();
            let (_relay, host_ws, viewer_ws) = TestRelay::start_with(Duration::from_secs(3600), false).await;
            let host = spawn_host(host_ws, timings, None);
            let (frame_tx, mut frames) = mpsc::channel(64);
            let (_input, input_rx) = mpsc::channel(4);
            let (ready_tx, ready_rx) = oneshot::channel();
            let viewer = tokio::spawn(async move {
                run_viewer_session_inner(viewer_ws, PASSWORD, SESSION, frame_tx, input_rx, Some(ready_tx), None, None, None, None, timings).await
            });
            let handle = timeout(Duration::from_secs(20), ready_rx).await.unwrap().unwrap();
            let rtt = timeout(Duration::from_secs(5), async {
                loop {
                    while frames.try_recv().is_ok() {}
                    if let Some(rtt) = handle.link.rtt(Instant::now()) { break rtt; }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the viewer measures an RTT");
            assert_eq!(handle.link.path, transport::MediaPath::Relay);
            assert!(rtt < Duration::from_secs(1), "{rtt:?}");
            host.abort();
            viewer.abort();
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn keepalive_holds_idle_relay_sockets_open() {
            let timings = SessionTimings { keepalive: Duration::from_millis(100), dead_peer: Duration::from_secs(5), ..SessionTimings::default() };
            let (relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_millis(600)).await;
            let host = spawn_host(host_ws, timings, None);
            let mut viewer = spawn_viewer(viewer_ws, timings);
            first_frame(&mut viewer).await;

            // Several idle timeouts: the viewer sends no input at all.
            for _ in 0..4 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                drain_frames(&mut viewer);
            }
            assert!(!relay.idle_fired.load(Ordering::SeqCst), "keepalives must keep both relay sockets active");
            assert!(!host.is_finished(), "host session ended despite keepalives");
            assert!(!viewer.task.is_finished(), "viewer session ended despite keepalives");
            host.abort();
            viewer.task.abort();
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn without_keepalive_the_idle_relay_drops_the_session() {
            // Control experiment for the test above: same relay, keepalives off.
            let timings = SessionTimings { keepalive: Duration::from_secs(3600), dead_peer: Duration::from_secs(3600), ..SessionTimings::default() };
            let (relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_millis(600)).await;
            let host = spawn_host(host_ws, timings, None);
            let mut viewer = spawn_viewer(viewer_ws, timings);
            // Setup (ICE) alone can idle a direction past the timeout, so do
            // not wait for video: the session must simply be dropped.
            timeout(Duration::from_secs(10), async {
                while !relay.idle_fired.load(Ordering::SeqCst) {
                    drain_frames(&mut viewer);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("the idle relay should drop a session without keepalives");
            host.abort();
            viewer.task.abort();
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn viewer_ends_promptly_when_host_control_channel_closes() {
            // Liveness timers far away: only the closed channel can end it.
            let timings = SessionTimings { keepalive: Duration::from_secs(3600), dead_peer: Duration::from_secs(3600), ..SessionTimings::default() };
            let (relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_secs(3600)).await;
            let host = spawn_host(host_ws, timings, None);
            let mut viewer = spawn_viewer(viewer_ws, timings);
            first_frame(&mut viewer).await;

            relay.close();
            let ended = timeout(Duration::from_secs(3), async {
                loop {
                    drain_frames(&mut viewer);
                    if viewer.task.is_finished() { break; }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            assert!(ended.is_ok(), "viewer kept running after the host's control channel closed");
            let error = viewer.task.await.unwrap().expect_err("a vanished host is an error");
            assert_eq!(error.reason, DisconnectReason::Transient, "a dropped host must be reconnectable: {error}");
            let host_result = timeout(Duration::from_secs(3), host).await.expect("host kept running after the relay closed");
            assert!(host_result.unwrap().is_ok());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn silent_peer_times_out_on_both_sides() {
            let timings = SessionTimings { keepalive: Duration::from_millis(100), dead_peer: Duration::from_millis(800), ..SessionTimings::default() };
            let (relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_secs(3600)).await;
            let host = spawn_host(host_ws, timings, None);
            let mut viewer = spawn_viewer(viewer_ws, timings);
            first_frame(&mut viewer).await;

            relay.blackhole();
            let ended = timeout(Duration::from_secs(4), async {
                loop {
                    drain_frames(&mut viewer);
                    if viewer.task.is_finished() { break; }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            assert!(ended.is_ok(), "viewer did not notice the silent host");
            let error = viewer.task.await.unwrap().expect_err("a silent host is an error");
            assert_eq!(error.reason, DisconnectReason::Transient, "a silent host must be reconnectable: {error}");
            let host_result = timeout(Duration::from_secs(4), host).await.expect("host did not notice the silent viewer");
            assert!(host_result.unwrap().is_ok());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn host_without_capture_tells_viewer_not_to_reconnect() {
            let timings = SessionTimings::default();
            let (_relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_secs(3600)).await;
            let host = tokio::spawn(async move {
                run_host_session_with_data_and_accept_and_2fa_with_factory(
                    host_ws, PASSWORD, SESSION, || None::<FakeCapturer>,
                    |width, height, _codec| H264Encoder::new(width, height, 15, 1_000_000),
                    || Box::new(NoopInjector::default()) as Box<dyn InputInjector>,
                    false, false, None, || async { true }, None, timings, HostDisplays::default(), SessionGates::default(),
                ).await
            });
            let viewer = spawn_viewer(viewer_ws, timings);
            let error = timeout(Duration::from_secs(10), viewer.task).await.expect("viewer kept waiting").unwrap().expect_err("no capture ends the session");
            assert_eq!(error.reason, DisconnectReason::AuthFailure, "{}", error.message);
            assert_eq!(crate::reconnect::rejection_code(&error.message), Some(reject_code::CAPTURE_UNAVAILABLE));
            assert!(host.await.unwrap().is_err());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn host_bye_ends_viewer_cleanly_without_reconnect() {
            let timings = SessionTimings::default();
            let (_relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_secs(3600)).await;
            let (ready_tx, ready_rx) = oneshot::channel();
            let host = spawn_host(host_ws, timings, Some(ready_tx));
            let mut viewer = spawn_viewer(viewer_ws, timings);
            let handle = timeout(Duration::from_secs(20), ready_rx).await.unwrap().unwrap();
            first_frame(&mut viewer).await;

            send_bye(&handle.control_sender, &handle.control_sealer).await;
            let ended = timeout(Duration::from_secs(3), async {
                loop {
                    drain_frames(&mut viewer);
                    if viewer.task.is_finished() { break; }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            assert!(ended.is_ok(), "viewer kept running after the host's Bye");
            let error = viewer.task.await.unwrap().expect_err("Bye ends the viewer session");
            assert_eq!(error.message, crate::reconnect::HOST_ENDED_MESSAGE);
            assert_eq!(error.reason, DisconnectReason::CleanUserDisconnect);
            assert!(!crate::reconnect::should_reconnect(error.reason));
            host.abort();
        }

        /// A capturer whose displays and health the test changes mid-session.
        #[derive(Clone, Default)]
        struct Scripted(Arc<Mutex<Script>>);

        #[derive(Default)]
        struct Script {
            stalled_since: Option<Instant>,
            monitors: Vec<protocol::control::MonitorInfo>,
            changed: Option<Vec<protocol::control::MonitorInfo>>,
        }

        impl Scripted {
            fn script(&self) -> std::sync::MutexGuard<'_, Script> { self.0.lock().unwrap() }
        }

        impl crate::capture::ScreenCapturer for Scripted {
            fn next_frame(&mut self) -> Option<encode::RawFrame> {
                if self.script().stalled_since.is_some() { return None; }
                FakeCapturer::new(64, 64).next_frame()
            }

            fn wait_frame(&mut self, timeout: Duration) -> Option<encode::RawFrame> {
                std::thread::sleep(timeout.min(Duration::from_millis(16)));
                self.next_frame()
            }

            fn monitors(&self) -> Vec<protocol::control::MonitorInfo> { self.script().monitors.clone() }

            fn stalled_since(&self) -> Option<Instant> { self.script().stalled_since }

            fn monitors_changed(&mut self) -> Option<Vec<protocol::control::MonitorInfo>> { self.script().changed.take() }

            fn select_monitor(&mut self, id: u32) -> bool { self.script().monitors.iter().any(|monitor| monitor.id == id) }
        }

        fn monitor(id: u32) -> protocol::control::MonitorInfo {
            protocol::control::MonitorInfo { id, name: format!("D{id}"), width: 0, height: 0, is_primary: id == 1, is_virtual: false }
        }

        fn spawn_scripted_host(ws: WebSocketStream<DuplexStream>, timings: SessionTimings, capturer: Scripted) -> JoinHandle<SessionResult<()>> {
            spawn_scripted_host_with(ws, timings, capturer, HostDisplays::default())
        }

        fn spawn_scripted_host_with(ws: WebSocketStream<DuplexStream>, timings: SessionTimings, capturer: Scripted, displays: HostDisplays) -> JoinHandle<SessionResult<()>> {
            tokio::spawn(async move {
                run_host_session_with_data_and_accept_and_2fa_with_factory(
                    ws, PASSWORD, SESSION, once_capturer(capturer),
                    |width, height, _codec| H264Encoder::new(width, height, 15, 1_000_000),
                    || Box::new(NoopInjector::default()) as Box<dyn InputInjector>,
                    false, false, None, || async { true }, None, timings, displays, SessionGates::default(),
                ).await
            })
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn lost_capture_ends_the_session_and_tells_the_viewer() {
            let timings = SessionTimings { capture_restore: Duration::from_millis(400), ..SessionTimings::default() };
            let (_relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_secs(3600)).await;
            let capturer = Scripted::default();
            let host = spawn_scripted_host(host_ws, timings, capturer.clone());
            let mut viewer = spawn_viewer(viewer_ws, timings);
            first_frame(&mut viewer).await;

            capturer.script().stalled_since = Some(Instant::now());
            let ended = timeout(Duration::from_secs(5), async {
                loop {
                    drain_frames(&mut viewer);
                    if viewer.task.is_finished() { break; }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            assert!(ended.is_ok(), "the viewer kept getting a frozen picture");
            let error = viewer.task.await.unwrap().expect_err("lost capture ends the session");
            assert_eq!(crate::reconnect::rejection_code(&error.message), Some(reject_code::CAPTURE_UNAVAILABLE), "{}", error.message);
            assert!(timeout(Duration::from_secs(3), host).await.expect("host kept running").unwrap().is_err());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_brief_capture_stall_keeps_the_session() {
            let timings = SessionTimings { capture_restore: Duration::from_secs(1), ..SessionTimings::default() };
            let (_relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_secs(3600)).await;
            let capturer = Scripted::default();
            let host = spawn_scripted_host(host_ws, timings, capturer.clone());
            let mut viewer = spawn_viewer(viewer_ws, timings);
            first_frame(&mut viewer).await;

            capturer.script().stalled_since = Some(Instant::now());
            tokio::time::sleep(Duration::from_millis(300)).await;
            capturer.script().stalled_since = None;
            for _ in 0..6 {
                tokio::time::sleep(Duration::from_millis(250)).await;
                drain_frames(&mut viewer);
            }
            assert!(!host.is_finished(), "a restored capture must not end the session");
            assert!(!viewer.task.is_finished());
            host.abort();
            viewer.task.abort();
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn changed_displays_reach_the_viewer() {
            let timings = SessionTimings::default();
            let (_relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_secs(3600)).await;
            let capturer = Scripted::default();
            capturer.script().monitors = vec![monitor(1)];
            let host = spawn_scripted_host(host_ws, timings, capturer.clone());
            let (frame_tx, mut frames) = mpsc::channel(64);
            let (monitor_tx, mut monitor_lists) = mpsc::channel(4);
            let (_input, input_rx) = mpsc::channel(4);
            let viewer = tokio::spawn(async move {
                run_viewer_session_inner(viewer_ws, PASSWORD, SESSION, frame_tx, input_rx, None, Some(monitor_tx), None, None, None, timings).await
            });
            let list = |body: Vec<u8>| match Control::from_bytes(&body) {
                Some(Control::MonitorList { monitors }) => monitors,
                other => panic!("expected a monitor list, got {other:?}"),
            };
            let first = timeout(Duration::from_secs(20), monitor_lists.recv()).await.unwrap().unwrap();
            assert_eq!(list(first), vec![monitor(1)]);
            timeout(Duration::from_secs(20), frames.recv()).await.unwrap().unwrap();

            capturer.script().changed = Some(vec![monitor(1), monitor(2)]);
            let second = timeout(Duration::from_secs(3), async {
                loop {
                    tokio::select! {
                        body = monitor_lists.recv() => break body.unwrap(),
                        _ = frames.recv() => {}
                    }
                }
            })
            .await
            .expect("the viewer never heard about the new display");
            assert_eq!(list(second), vec![monitor(1), monitor(2)]);
            host.abort();
            viewer.abort();
        }

        /// A viewer of a scripted host, with its control channel and the
        /// display messages the host sent.
        struct DisplayRig {
            host: JoinHandle<SessionResult<()>>,
            viewer: JoinHandle<Result<(), ViewerSessionError>>,
            capturer: Scripted,
            backend: Arc<vdisplay::fake::FakeDisplays>,
            notices: Arc<Mutex<Vec<DisplayNotice>>>,
            control: Option<DataHandle>,
            messages: mpsc::Receiver<Vec<u8>>,
            _frames: JoinHandle<()>,
        }

        impl DisplayRig {
            async fn start(present: Option<bool>, allow: bool, monitors: Vec<protocol::control::MonitorInfo>, backend: vdisplay::fake::FakeDisplays) -> Self {
                let timings = SessionTimings::default();
                let (_relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_secs(3600)).await;
                let capturer = Scripted::default();
                capturer.script().monitors = monitors;
                let backend = Arc::new(backend);
                let notices = Arc::new(Mutex::new(Vec::new()));
                let recorded = Arc::clone(&notices);
                let displays = HostDisplays {
                    backend: backend.clone(),
                    allow_virtual: allow,
                    present: Arc::new(move || present),
                    notify: Some(Arc::new(move |notice| recorded.lock().unwrap().push(notice))),
                };
                let host = spawn_scripted_host_with(host_ws, timings, capturer.clone(), displays);
                let (frame_tx, mut frames) = mpsc::channel(64);
                let (monitor_tx, messages) = mpsc::channel(16);
                let (input, input_rx) = mpsc::channel(4);
                let (ready_tx, ready_rx) = oneshot::channel();
                let viewer = tokio::spawn(async move {
                    let _input = input;
                    run_viewer_session_inner(viewer_ws, PASSWORD, SESSION, frame_tx, input_rx, Some(ready_tx), Some(monitor_tx), None, None, None, timings)
                        .await
                        .map_err(viewer_session_error)
                });
                let frames = tokio::spawn(async move { while frames.recv().await.is_some() {} });
                let control = timeout(Duration::from_secs(20), ready_rx).await.ok().and_then(Result::ok);
                Self { host, viewer, capturer, backend, notices, control, messages, _frames: frames }
            }

            async fn send(&self, control: Control) {
                let handle = self.control.as_ref().expect("session is up");
                handle.control_sender.send_frame(Tag::Control, &seal_control(&handle.control_sealer, &control)).await.unwrap();
            }

            async fn next(&mut self) -> Control {
                let body = timeout(Duration::from_secs(10), self.messages.recv()).await.expect("no message from the host").expect("sink closed");
                Control::from_bytes(&body).expect("a control message")
            }

            async fn end(self) -> Arc<vdisplay::fake::FakeDisplays> {
                self.viewer.abort();
                // The host notices the viewer's socket closing.
                let _ = timeout(Duration::from_secs(10), self.host).await;
                self.backend
            }
        }

        /// A display the viewer added, at the size the backend gave it.
        fn virtual_monitor(id: u32, width: u32, height: u32) -> protocol::control::MonitorInfo {
            protocol::control::MonitorInfo { is_virtual: true, width, height, ..monitor(id) }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn viewer_adds_switches_to_and_removes_a_virtual_display() {
            let mut rig = DisplayRig::start(Some(true), true, vec![monitor(1)], Default::default()).await;
            assert_eq!(rig.next().await, Control::MonitorList { monitors: vec![monitor(1)] });
            assert_eq!(rig.next().await, Control::VirtualDisplaySupport { max: vdisplay::MAX_PER_SESSION as u32 });

            // The fake hands out 100 first; capture lists it once it exists.
            rig.capturer.script().monitors = vec![monitor(1), monitor(100)];
            rig.send(Control::AddVirtualDisplay { width: 2561, height: 1441, hidpi: false }).await;
            assert_eq!(rig.next().await, Control::MonitorList { monitors: vec![monitor(1), virtual_monitor(100, 2560, 1440)] });
            assert_eq!(rig.next().await, Control::VirtualDisplayAdded { id: 100 });
            let (_, spec) = rig.backend.live()[0];
            assert_eq!((spec.width, spec.height), (2560, 1440), "sizes are evened");

            rig.send(Control::AddVirtualDisplay { width: 1280, height: 720, hidpi: false }).await;
            rig.capturer.script().monitors = vec![monitor(1), monitor(100), monitor(101)];
            assert!(matches!(rig.next().await, Control::MonitorList { .. }));
            assert_eq!(rig.next().await, Control::VirtualDisplayAdded { id: 101 });
            rig.send(Control::AddVirtualDisplay { width: 1280, height: 720, hidpi: false }).await;
            assert_eq!(rig.next().await, Control::VirtualDisplayRefused { code: protocol::control::VIRTUAL_DISPLAY_LIMIT.into() });

            rig.send(Control::RemoveVirtualDisplay { id: 1 }).await;
            assert_eq!(rig.next().await, Control::VirtualDisplayRefused { code: protocol::control::VIRTUAL_DISPLAY_UNKNOWN.into() });
            rig.send(Control::RemoveVirtualDisplay { id: 100 }).await;
            assert_eq!(rig.next().await, Control::MonitorList { monitors: vec![monitor(1), virtual_monitor(101, 1280, 720)] });
            assert_eq!(rig.backend.owned(), vec![101]);
            assert_eq!(*rig.notices.lock().unwrap(), [DisplayNotice::ViewerAdded, DisplayNotice::ViewerAdded, DisplayNotice::ViewerRemoved]);

            let backend = rig.end().await;
            assert!(backend.owned().is_empty(), "ending the session removes the viewer's displays");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_display_under_a_newer_one_is_refused_and_stays_shown() {
            let backend = vdisplay::fake::FakeDisplays::default();
            backend.state.lock().unwrap().newest_only = true;
            let mut rig = DisplayRig::start(Some(true), true, vec![monitor(1)], backend).await;
            assert!(matches!(rig.next().await, Control::MonitorList { .. }));
            assert!(matches!(rig.next().await, Control::VirtualDisplaySupport { .. }));
            rig.capturer.script().monitors = vec![monitor(1), monitor(100), monitor(101)];
            for id in [100, 101] {
                rig.send(Control::AddVirtualDisplay { width: 1920, height: 1080, hidpi: false }).await;
                assert!(matches!(rig.next().await, Control::MonitorList { .. }));
                assert_eq!(rig.next().await, Control::VirtualDisplayAdded { id });
            }
            rig.send(Control::SelectMonitor { id: 100 }).await;
            rig.send(Control::RemoveVirtualDisplay { id: 100 }).await;
            assert_eq!(rig.next().await, Control::VirtualDisplayRefused { code: protocol::control::VIRTUAL_DISPLAY_NOT_NEWEST.into() });
            assert_eq!(rig.backend.owned(), vec![100, 101]);
            rig.send(Control::RemoveVirtualDisplay { id: 101 }).await;
            assert!(matches!(rig.next().await, Control::MonitorList { monitors } if monitors.len() == 2));
            rig.send(Control::RemoveVirtualDisplay { id: 100 }).await;
            assert!(matches!(rig.next().await, Control::MonitorList { monitors } if monitors == vec![monitor(1)]));
            rig.end().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn hosts_that_do_not_allow_virtual_displays_never_offer_them() {
            let mut rig = DisplayRig::start(Some(true), false, vec![monitor(1)], Default::default()).await;
            assert!(matches!(rig.next().await, Control::MonitorList { .. }));
            rig.send(Control::AddVirtualDisplay { width: 1920, height: 1080, hidpi: false }).await;
            assert_eq!(rig.next().await, Control::VirtualDisplayRefused { code: protocol::control::VIRTUAL_DISPLAY_NOT_ALLOWED.into() });
            assert!(rig.backend.owned().is_empty());
            rig.end().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_headless_host_adds_a_display_for_the_session() {
            let mut rig = DisplayRig::start(Some(false), true, vec![monitor(100)], Default::default()).await;
            // The headless display is the host's, not the viewer's to remove.
            let sized = protocol::control::MonitorInfo { width: 1920, height: 1080, ..monitor(100) };
            assert!(matches!(rig.next().await, Control::MonitorList { monitors } if monitors == vec![sized]));
            assert_eq!(rig.backend.owned(), vec![100]);
            assert_eq!(rig.backend.live()[0].1, DisplaySpec::headless());
            assert_eq!(*rig.notices.lock().unwrap(), [DisplayNotice::HeadlessAdded]);
            let backend = rig.end().await;
            assert!(backend.owned().is_empty(), "the headless display goes with the session");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_headless_host_that_cannot_add_a_display_says_so() {
            for (allow, backend) in [(false, vdisplay::fake::FakeDisplays::default()), (true, vdisplay::fake::FakeDisplays::unsupported())] {
                let rig = DisplayRig::start(Some(false), allow, Vec::new(), backend).await;
                assert!(rig.control.is_none());
                let error = timeout(Duration::from_secs(10), rig.viewer).await.unwrap().unwrap().expect_err("no screen ends the session");
                assert_eq!(crate::reconnect::rejection_code(&error.message), Some(reject_code::NO_DISPLAY), "{}", error.message);
                assert!(crate::reconnect::stop_message(&error.message, error.reason, false).contains("no screen connected"));
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn pending_approval_ends_when_the_viewer_leaves() {
            let timings = SessionTimings { approval: Duration::from_secs(3600), ..SessionTimings::default() };
            let (relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_secs(3600)).await;
            let (asked_tx, asked) = oneshot::channel();
            let host = spawn_host_approving(host_ws, timings, None, unanswered(asked_tx));
            let viewer = spawn_viewer(viewer_ws, timings);
            timeout(Duration::from_secs(10), asked).await.expect("the host never asked for approval").unwrap();

            relay.close();
            let result = timeout(Duration::from_secs(3), host).await.expect("host kept waiting on a viewer that left");
            assert!(result.unwrap().is_ok(), "a viewer leaving is not a host error");
            viewer.task.abort();
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn unanswered_approval_expires_and_tells_the_viewer() {
            let timings = SessionTimings { approval: Duration::from_millis(300), ..SessionTimings::default() };
            let (_relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_secs(3600)).await;
            let (asked_tx, _asked) = oneshot::channel();
            let host = spawn_host_approving(host_ws, timings, None, unanswered(asked_tx));
            let viewer = spawn_viewer(viewer_ws, timings);
            let error = timeout(Duration::from_secs(10), viewer.task).await.expect("viewer kept waiting").unwrap().expect_err("an unanswered request ends the attempt");
            assert_eq!(error.reason, DisconnectReason::AuthFailure, "{}", error.message);
            assert_eq!(crate::reconnect::rejection_code(&error.message), Some(reject_code::NO_ANSWER));
            assert!(timeout(Duration::from_secs(3), host).await.expect("host kept waiting").unwrap().is_err());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn slow_approval_still_starts_the_session() {
            let timings = SessionTimings::default();
            let (_relay, host_ws, viewer_ws) = TestRelay::start(Duration::from_secs(3600)).await;
            // ICE candidates and keepalives arrive while the host user decides.
            let host = spawn_host_approving(host_ws, timings, None, || async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                true
            });
            let mut viewer = spawn_viewer(viewer_ws, timings);
            first_frame(&mut viewer).await;
            host.abort();
            viewer.task.abort();
        }
    }
}
