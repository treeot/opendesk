mod client_ip;
mod ratemap;
mod registry;
mod session;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use client_ip::TrustedProxies;
use futures_util::{SinkExt, StreamExt};
use ratemap::RateMap;
use registry::{CloseOutbound, Outbound, Registry, RegistryConfig};
use session::Bridge;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::watch;

const DEFAULT_MAX_CONNECTIONS: usize = 10_000;
const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 50;
// Control frames after pairing (ICE candidates, auth, monitor selection) are
// bursty during session setup, so they get a generous bucket; the byte cap
// still bounds total traffic. Before pairing the relay reads only the first
// message, and that is limited per IP by the attempt and connect buckets.
const DEFAULT_SESSION_CONTROL_BURST: u64 = 64;
const DEFAULT_SESSION_CONTROL_PER_SEC: u64 = 16;
const DEFAULT_BYTES_PER_SEC: u64 = 64 * 1024 * 1024;
const DEFAULT_BYTE_BURST: u64 = 8 * 1024 * 1024;
const DEFAULT_HANDSHAKE_TIMEOUT_SECS: u64 = 15;
const DEFAULT_PAIR_TIMEOUT_SECS: u64 = 60;
// Applies only to paired sockets, where both peers send a keepalive every
// ~15 s. A parked host registration waits under the pair timeout instead and
// is never subject to this.
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 60;
// The relay is a blind forwarder: a single WebSocket message carries either a
// tiny Register/Connect signal OR an opaque payload frame it pipes between
// peers. The cap therefore has to clear the largest legitimate forwarded frame
// (the host caps a video frame at 4 MiB), so it cannot be a signaling-sized
// value without dropping real media/file frames. 8 MiB bounds a single message
// well below tungstenite's 64 MiB default while passing all legitimate
// traffic; the retained-memory DoS is closed by strict Register field
// validation (9-digit id, 64-hex token) plus the per-IP limits, not by this.
const DEFAULT_MAX_SIGNAL_MESSAGE: usize = 8 * 1024 * 1024;
/// A pre-pairing signal (Register/Connect/Presence) is well under 1 KiB; this
/// bound rejects an oversized first message before it is buffered or parsed.
const SIGNAL_MESSAGE_MAX: usize = 64 * 1024;
const DEFAULT_CONNECTION_ATTEMPT_BURST: u32 = 20;
const DEFAULT_CONNECTION_ATTEMPT_REFILL_SECS: u64 = 10;
const DEFAULT_SWEEP_SECS: u64 = 60;
/// Frames queued toward one peer before the other peer's reads pause. Kept small
/// to bound both latency and the memory one pair can pin: at the forwarded-frame
/// cap this is the worst-case queued bytes per direction, so a slow or stalled
/// receiver cannot make the relay hold hundreds of MiB per session. A complete
/// fix would budget by bytes rather than frame count.
const PEER_QUEUE: usize = 8;

#[derive(Clone)]
pub struct RelayState {
    registry: Registry,
    limits: Arc<ConnectionLimits>,
    config: RelayConfig,
    proxies: Arc<TrustedProxies>,
    shutdown: watch::Receiver<bool>,
    _shutdown_keepalive: Option<watch::Sender<bool>>,
}

#[derive(Clone, Debug)]
struct RelayConfig {
    session_control_burst: u64,
    session_control_per_sec: u64,
    bytes_per_sec: u64,
    byte_burst: u64,
    handshake_timeout: Duration,
    pair_timeout: Duration,
    idle_timeout: Duration,
    max_signal_message: usize,
    connection_attempt_burst: u32,
    connection_attempt_refill: Duration,
    sweep_interval: Duration,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            session_control_burst: DEFAULT_SESSION_CONTROL_BURST,
            session_control_per_sec: DEFAULT_SESSION_CONTROL_PER_SEC,
            bytes_per_sec: DEFAULT_BYTES_PER_SEC,
            byte_burst: DEFAULT_BYTE_BURST,
            handshake_timeout: Duration::from_secs(DEFAULT_HANDSHAKE_TIMEOUT_SECS),
            pair_timeout: Duration::from_secs(DEFAULT_PAIR_TIMEOUT_SECS),
            idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
            max_signal_message: DEFAULT_MAX_SIGNAL_MESSAGE,
            connection_attempt_burst: DEFAULT_CONNECTION_ATTEMPT_BURST,
            connection_attempt_refill: Duration::from_secs(DEFAULT_CONNECTION_ATTEMPT_REFILL_SECS),
            sweep_interval: Duration::from_secs(DEFAULT_SWEEP_SECS),
        }
    }
}

impl RelayConfig {
    fn from_env() -> Self {
        Self {
            session_control_burst: env_u64("RELAY_SESSION_CONTROL_BURST", DEFAULT_SESSION_CONTROL_BURST),
            session_control_per_sec: env_u64("RELAY_SESSION_CONTROL_PER_SEC", DEFAULT_SESSION_CONTROL_PER_SEC),
            bytes_per_sec: env_u64("RELAY_MAX_BYTES_PER_SEC", DEFAULT_BYTES_PER_SEC),
            byte_burst: env_u64("RELAY_BYTE_BURST", DEFAULT_BYTE_BURST),
            handshake_timeout: Duration::from_secs(env_u64("RELAY_HANDSHAKE_TIMEOUT_SECS", DEFAULT_HANDSHAKE_TIMEOUT_SECS)),
            pair_timeout: Duration::from_secs(env_u64("RELAY_PAIR_TIMEOUT_SECS", DEFAULT_PAIR_TIMEOUT_SECS)),
            idle_timeout: Duration::from_secs(env_u64("RELAY_IDLE_TIMEOUT_SECS", DEFAULT_IDLE_TIMEOUT_SECS)),
            max_signal_message: env_usize("RELAY_MAX_SIGNAL_MESSAGE_BYTES", DEFAULT_MAX_SIGNAL_MESSAGE),
            connection_attempt_burst: env_u64("RELAY_CONNECTION_ATTEMPT_BURST", DEFAULT_CONNECTION_ATTEMPT_BURST as u64) as u32,
            connection_attempt_refill: Duration::from_secs(env_u64("RELAY_CONNECTION_ATTEMPT_REFILL_SECS", DEFAULT_CONNECTION_ATTEMPT_REFILL_SECS)),
            sweep_interval: Duration::from_secs(env_u64("RELAY_SWEEP_SECS", DEFAULT_SWEEP_SECS)),
        }
    }
}

fn registry_config_from_env() -> RegistryConfig {
    let default = RegistryConfig::default();
    // RELAY_OWNERSHIP_GRACE_SECS is the older name for the lease.
    let lease = env_u64("RELAY_ID_LEASE_SECS", env_u64("RELAY_OWNERSHIP_GRACE_SECS", default.lease.as_secs()));
    RegistryConfig {
        lease: Duration::from_secs(lease),
        unpaired_lease: Duration::from_secs(env_u64("RELAY_UNPAIRED_ID_LEASE_SECS", default.unpaired_lease.as_secs())),
        max_bindings: env_usize("RELAY_MAX_ID_BINDINGS", default.max_bindings),
        max_rate_entries: env_usize("RELAY_MAX_RATE_ENTRIES", default.max_rate_entries),
        connect_burst: env_u64("RELAY_CONNECT_BURST", default.connect_burst as u64) as u32,
        connect_refill: Duration::from_secs(env_u64("RELAY_CONNECT_REFILL_SECS", default.connect_refill.as_secs())),
        new_id_burst: env_u64("RELAY_NEW_ID_BURST", default.new_id_burst as u64) as u32,
        new_id_refill: Duration::from_secs(env_u64("RELAY_NEW_ID_REFILL_SECS", default.new_id_refill.as_secs())),
        takeover_limit: default.takeover_limit,
        takeover_window: default.takeover_window,
        state_file: std::env::var_os("RELAY_STATE_FILE").filter(|value| !value.is_empty()).map(Into::into),
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).filter(|v| *v > 0).unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize { env_u64(name, default as u64) as usize }

struct ConnectionLimits {
    max: usize,
    per_ip: usize,
    active: AtomicUsize,
    /// Bounded by `max`: an entry exists only while that IP has a connection open.
    ips: Mutex<HashMap<IpAddr, usize>>,
    attempts: RateMap<IpAddr>,
}

impl ConnectionLimits {
    fn new(max: usize, per_ip: usize, attempt_burst: u32, attempt_refill: Duration, max_rate_entries: usize) -> Self {
        Self { max, per_ip, active: AtomicUsize::new(0), ips: Mutex::new(HashMap::new()), attempts: RateMap::new(attempt_burst, attempt_refill, max_rate_entries) }
    }

    fn from_env(config: &RelayConfig, registry: &RegistryConfig) -> Self {
        Self::new(
            env_usize("RELAY_MAX_CONNECTIONS", DEFAULT_MAX_CONNECTIONS),
            env_usize("RELAY_MAX_CONNECTIONS_PER_IP", DEFAULT_MAX_CONNECTIONS_PER_IP),
            config.connection_attempt_burst,
            config.connection_attempt_refill,
            registry.max_rate_entries,
        )
    }

    fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<ConnectionGuard> {
        // Count per IPv4 address or IPv6 /64, so one IPv6 allocation cannot open
        // unlimited connections (or fill the attempt map) by rotating addresses.
        let key = registry::rate_key(ip);
        if !self.attempts.allow(key) { return None; }
        let mut ips = self.ips.lock().expect("connection limits lock poisoned");
        let current = ips.get(&key).copied().unwrap_or(0);
        if self.active.load(Ordering::Relaxed) >= self.max || current >= self.per_ip { return None; }
        self.active.fetch_add(1, Ordering::Relaxed);
        *ips.entry(key).or_default() += 1;
        Some(ConnectionGuard { limits: Arc::clone(self), ip: key })
    }

    fn active(&self) -> usize { self.active.load(Ordering::Relaxed) }
}

struct ConnectionGuard { limits: Arc<ConnectionLimits>, ip: IpAddr }

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.limits.active.fetch_sub(1, Ordering::Relaxed);
        let mut ips = self.limits.ips.lock().expect("connection limits lock poisoned");
        if let Some(count) = ips.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 { ips.remove(&self.ip); }
        }
    }
}

#[derive(Clone, Debug)]
struct TokenBucket { tokens: f64, capacity: f64, refill_per_sec: f64, last: Instant }

impl TokenBucket {
    fn new(capacity: u64, refill_per_sec: u64) -> Self {
        Self { tokens: capacity as f64, capacity: capacity as f64, refill_per_sec: refill_per_sec as f64, last: Instant::now() }
    }

    fn allow(&mut self, amount: usize) -> bool {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.last).as_secs_f64() * self.refill_per_sec).min(self.capacity);
        self.last = now;
        if self.tokens >= amount as f64 { self.tokens -= amount as f64; true } else { false }
    }
}

/// Handle to a running relay's shared state, for stats and persistence.
#[derive(Clone)]
pub struct RelayHandle { registry: Registry, limits: Arc<ConnectionLimits> }

impl RelayHandle {
    pub fn active_connections(&self) -> usize { self.limits.active() }

    /// Saves ID bindings to `RELAY_STATE_FILE` if any changed. Call on shutdown.
    pub fn persist(&self) -> std::io::Result<bool> { self.registry.persist_if_dirty() }
}

pub fn app() -> Router {
    let (tx, rx) = watch::channel(false);
    build(rx, Some(tx)).0
}

pub fn app_with_shutdown(shutdown: watch::Receiver<bool>) -> Router { build(shutdown, None).0 }

/// Like [`app_with_shutdown`], also returning a [`RelayHandle`].
pub fn relay_with_shutdown(shutdown: watch::Receiver<bool>) -> (Router, RelayHandle) { build(shutdown, None) }

fn build(shutdown: watch::Receiver<bool>, keepalive: Option<watch::Sender<bool>>) -> (Router, RelayHandle) {
    let config = RelayConfig::from_env();
    let registry_config = registry_config_from_env();
    let limits = Arc::new(ConnectionLimits::from_env(&config, &registry_config));
    let registry = Registry::new(registry_config);
    if let Some(path) = registry.state_file() {
        match registry.load(path) {
            Ok(count) => log::info!("loaded {count} id bindings from {}", path.display()),
            Err(error) => log::warn!("could not load relay state from {}: {error}", path.display()),
        }
    }
    let state = RelayState { registry: registry.clone(), limits: Arc::clone(&limits), config, proxies: Arc::new(TrustedProxies::from_env()), shutdown, _shutdown_keepalive: keepalive };
    spawn_maintenance(&state);
    (app_with_state(state), RelayHandle { registry, limits })
}

/// Every `sweep_interval`: drop expired bindings and idle rate buckets, save
/// the state file if it changed, and log the connection count.
fn spawn_maintenance(state: &RelayState) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
    let registry = state.registry.clone();
    let limits = Arc::clone(&state.limits);
    let mut shutdown = state.shutdown.clone();
    let period = state.config.sweep_interval;
    runtime.spawn(async move {
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        let mut last_logged = None;
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                result = shutdown.changed() => { if result.is_err() || *shutdown.borrow() { break; } continue; }
            }
            let now = Instant::now();
            let stats = registry.sweep(now);
            limits.attempts.sweep(now);
            let snapshot = (limits.active(), registry.binding_count());
            if last_logged != Some(snapshot) || stats.bindings_removed > 0 {
                log::info!("event=stats active_connections={} id_bindings={} expired_bindings={}", snapshot.0, snapshot.1, stats.bindings_removed);
                last_logged = Some(snapshot);
            }
            let saver = registry.clone();
            match tokio::task::spawn_blocking(move || saver.persist_if_dirty()).await {
                Ok(Err(error)) => log::warn!("could not save relay state: {error}"),
                Err(error) => log::warn!("relay state save task failed: {error}"),
                Ok(Ok(_)) => {}
            }
        }
    });
}

fn app_with_state(state: RelayState) -> Router {
    Router::new().route("/ws", get(ws_handler)).route("/health", get(health_handler)).with_state(state)
}

async fn health_handler() -> Response {
    ([(axum::http::header::CONTENT_TYPE, "application/json")], r#"{"status":"ok"}"#).into_response()
}

async fn ws_handler(ws: WebSocketUpgrade, ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap, State(state): State<RelayState>) -> Response {
    let client = state.proxies.client_ip(addr.ip(), &headers);
    let guard = state.limits.try_acquire(client);
    if guard.is_none() { log_event("connection_rejected", client, "capacity"); }
    ws.max_message_size(state.config.max_signal_message).max_frame_size(state.config.max_signal_message)
        .on_upgrade(move |socket| handle(socket, state, client, guard))
}

async fn handle(socket: WebSocket, state: RelayState, addr: IpAddr, guard: Option<ConnectionGuard>) {
    let Some(_guard) = guard else {
        let mut socket = socket;
        let _ = socket.send(Message::Binary(session::error_frame(protocol::control::ERROR_RELAY_BUSY).into())).await;
        let _ = socket.send(Message::Close(Some(CloseFrame { code: 1013, reason: "relay capacity".into() }))).await;
        return;
    };
    log::info!("event=connection_open remote={addr} reason=accepted active_connections={}", state.limits.active());
    let (mut sink, mut stream) = socket.split();
    // Kept short: a deep queue here is seconds of stale video when the
    // receiving peer is slower than the sender. A full queue backpressures
    // the sending peer, whose host bounds video in flight.
    // Byte-budgeted toward this peer: the sender acquires budget per frame and
    // the writer below releases it as it drains, so a slow/stalled receiver
    // backpressures the sender instead of growing memory without bound.
    let budget = Arc::new(tokio::sync::Semaphore::new(registry::MAX_QUEUED_BYTES));
    let (raw_tx, mut my_rx) = mpsc::channel::<Vec<u8>>(PEER_QUEUE);
    let my_tx = registry::Outbound::new(raw_tx, Arc::clone(&budget));
    let (close_tx, close_rx) = watch::channel::<Option<CloseFrame>>(None);
    let mut writer_close_rx = close_rx.clone();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(bytes) = my_rx.recv() => {
                    let cost = registry::Outbound::permits_for(bytes.len());
                    let sent = sink.send(Message::Binary(bytes.into())).await.is_ok();
                    // Return the frame's budget once it is off the queue.
                    budget.add_permits(cost);
                    if !sent { break; }
                }
                result = writer_close_rx.changed() => {
                    if result.is_err() { break; }
                    let close = writer_close_rx.borrow().clone();
                    if let Some(close) = close {
                        // Flush frames queued before the close (such as an
                        // error code) so the select race cannot drop them.
                        let _ = tokio::time::timeout(Duration::from_secs(1), async {
                            while let Ok(bytes) = my_rx.try_recv() {
                                if sink.send(Message::Binary(bytes.into())).await.is_err() { return; }
                            }
                            let _ = sink.send(Message::Close(Some(close))).await;
                        }).await;
                    }
                    break;
                }
                else => break,
            }
        }
    });

    let mut shutdown = state.shutdown.clone();
    let first = tokio::select! {
        _ = shutdown.changed() => { writer.abort(); log_event("connection_close", addr, "shutdown"); return; }
        result = tokio::time::timeout(state.config.handshake_timeout, stream.next()) => result.ok().flatten().and_then(Result::ok),
    };
    let Some(Message::Binary(first)) = first else { writer.abort(); log_event("connection_close", addr, "handshake"); return; };
    // The first message is always a signal (Register/Connect/Presence), never a
    // forwarded frame, so it is tiny. Reject an oversized one before parsing it,
    // so a pre-auth connection cannot make the relay buffer or deserialize
    // megabytes (e.g. a huge Presence array) per socket.
    if first.len() > SIGNAL_MESSAGE_MAX { writer.abort(); log_event("connection_rejected", addr, "signal_too_large"); return; }
    let mut bytes = TokenBucket::new(state.config.byte_burst, state.config.bytes_per_sec);
    if !bytes.allow(first.len()) { writer.abort(); log_event("connection_rejected", addr, "byte_rate"); return; }

    let bridge = session::classify(&first, my_tx.clone(), close_tx.clone(), &state.registry, addr);
    let (peer_tx, peer_close_tx, registered_peer) = match bridge {
        Ok(Bridge::ViewerPaired { host_tx, host_close_tx, requester_id, target_id }) => {
            let _ = host_tx.send(session::paired_frame("host", &requester_id)).await;
            let _ = my_tx.send(session::paired_frame("viewer", &target_id)).await;
            log_event("pairing_success", addr, "viewer");
            (host_tx, host_close_tx, None)
        }
        Ok(Bridge::Presence { online }) => {
            // Answer the query, then close: a presence socket never pairs.
            let _ = my_tx.send(session::presence_frame(online)).await;
            let _ = close_tx.send(Some(CloseFrame { code: 1000, reason: "presence".into() }));
            log_event("connection_close", addr, "presence");
            let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
            return;
        }
        Ok(Bridge::HostRegistered { peer_id, notify_rx }) => {
            let pairing = tokio::time::timeout(state.config.pair_timeout, notify_rx);
            tokio::pin!(pairing);
            // Watch the parked socket so a host that goes away is dropped at
            // once: viewers don't pair with a dead socket, and the host's
            // next registration doesn't count as a takeover.
            let mut early = Vec::new();
            let mut watching = true;
            let peer = loop {
                tokio::select! {
                    biased;
                    result = &mut pairing => break result.ok().and_then(Result::ok),
                    _ = shutdown.changed() => { state.registry.unregister(&peer_id, &my_tx); writer.abort(); log_event("connection_close", addr, "shutdown"); return; }
                    left = watch_parked_host(&mut stream, &mut early), if watching => {
                        if left { state.registry.unregister(&peer_id, &my_tx); writer.abort(); log_event("connection_close", addr, "host_left"); return; }
                        // Too much early data: leave the rest in the socket.
                        watching = false;
                    }
                }
            };
            let Some((peer, peer_close_tx, requester_id)) = peer else { state.registry.unregister(&peer_id, &my_tx); writer.abort(); log_event("pairing_failure", addr, "pair_timeout"); return; };
            let _ = my_tx.send(session::paired_frame("host", &requester_id)).await;
            log_event("pairing_success", addr, "host");
            for frame in early {
                if !bytes.allow(frame.len()) || peer.send(frame).await.is_err() { break; }
            }
            (peer, peer_close_tx, Some(peer_id))
        }
        Err(error) => {
            use protocol::control::*;
            let reason = match error {
                session::ClassifyError::ConnectRateLimited => ERROR_CONNECT_RATE,
                session::ClassifyError::InvalidRegistration => ERROR_INVALID_REGISTER,
                session::ClassifyError::IdTaken => ERROR_ID_TAKEN,
                session::ClassifyError::IdInUse => ERROR_ID_IN_USE,
                session::ClassifyError::RegistryFull => ERROR_REGISTRY_FULL,
                session::ClassifyError::NewIdRateLimited => ERROR_NEW_ID_RATE,
                session::ClassifyError::InvalidConnect => ERROR_INVALID_CONNECT,
                session::ClassifyError::PeerUnavailable => ERROR_PEER_UNAVAILABLE,
                session::ClassifyError::InvalidControl => ERROR_INVALID_CONTROL,
            };
            // Always say why: a bare close leaves the app guessing.
            let _ = my_tx.send(session::error_frame(reason)).await;
            let _ = close_tx.send(Some(CloseFrame { code: 1008, reason: reason.into() }));
            log_event("pairing_failure", addr, reason);
            // Give the writer a moment to flush the error and close frames.
            let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
            return;
        }
    };
    let mut control = TokenBucket::new(state.config.session_control_burst, state.config.session_control_per_sec);
    forward_rest(&mut stream, peer_tx, close_rx, &mut bytes, &mut control, &state, &mut shutdown, addr).await;
    close_peer(&peer_close_tx);
    if let Some(peer_id) = registered_peer { state.registry.unregister(&peer_id, &my_tx); }
    writer.abort();
    log::info!("event=connection_close remote={addr} reason=eof_or_timeout active_connections={}", state.limits.active().saturating_sub(1));
}

#[allow(clippy::too_many_arguments)]
async fn forward_rest(stream: &mut futures_util::stream::SplitStream<WebSocket>, peer_tx: Outbound, mut close_rx: watch::Receiver<Option<CloseFrame>>, bytes: &mut TokenBucket, control: &mut TokenBucket, state: &RelayState, shutdown: &mut watch::Receiver<bool>, addr: IpAddr) {
    loop {
        let next = tokio::select! {
            _ = close_rx.changed() => break,
            _ = shutdown.changed() => break,
            result = tokio::time::timeout(state.config.idle_timeout, stream.next()) => match result { Ok(value) => value, Err(_) => { log_event("connection_close", addr, "idle_timeout"); break; } },
        };
        let Some(Ok(msg)) = next else { break; };
        if let Message::Binary(frame) = msg {
            if !bytes.allow(frame.len()) { log_event("connection_rejected", addr, "byte_rate"); break; }
            if frame.first() == Some(&(protocol::Tag::Control as u8)) && !control.allow(1) { log_event("connection_rejected", addr, "signaling_rate"); break; }
            let queued = tokio::select! {
                _ = shutdown.changed() => false,
                result = tokio::time::timeout(state.config.idle_timeout, peer_tx.send(Vec::from(frame))) => matches!(result, Ok(Ok(()))),
            };
            if !queued { break; }
        }
    }
}

/// Data a parked host may send before it is paired (some start their
/// handshake right away), held for the viewer.
const MAX_EARLY_BYTES: usize = 256 * 1024;

/// Reads a parked host's socket, keeping data frames in `early`. Returns
/// `true` when the host went away, `false` once `early` is full.
async fn watch_parked_host(stream: &mut futures_util::stream::SplitStream<WebSocket>, early: &mut Vec<Vec<u8>>) -> bool {
    loop {
        match stream.next().await {
            Some(Ok(Message::Binary(frame))) => {
                let held: usize = early.iter().map(Vec::len).sum();
                if held + frame.len() > MAX_EARLY_BYTES { return false; }
                early.push(Vec::from(frame));
            }
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return true,
            Some(Ok(_)) => {}
        }
    }
}

fn close_peer(peer_close_tx: &CloseOutbound) {
    let _ = peer_close_tx.send(Some(CloseFrame { code: 1000, reason: "peer disconnected".into() }));
}

fn log_event(event: &str, addr: IpAddr, reason: &str) { log::info!("event={event} remote={addr} reason={reason}"); }

pub async fn spawn_test_server() -> String { spawn_test_server_with_handle().await.0 }

/// Starts a relay on an ephemeral loopback port, returning its address and a
/// [`RelayHandle`] for stats.
pub async fn spawn_test_server_with_handle() -> (String, RelayHandle) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = watch::channel(false);
    let (router, handle) = build(rx, Some(tx));
    tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap() });
    (addr, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn token_bucket_allows_burst_then_refills() {
        let mut bucket = TokenBucket::new(2, 20);
        assert!(bucket.allow(1)); assert!(bucket.allow(1)); assert!(!bucket.allow(1));
        std::thread::sleep(Duration::from_millis(75));
        assert!(bucket.allow(1));
    }

    #[test]
    fn connection_limits_enforce_global_and_per_ip_caps() {
        let limits = Arc::new(ConnectionLimits::new(2, 1, 20, Duration::from_secs(10), 100));
        let first = limits.try_acquire(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        assert!(limits.try_acquire(IpAddr::V4(Ipv4Addr::LOCALHOST)).is_none());
        let second = limits.try_acquire(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))).unwrap();
        assert_eq!(limits.active(), 2);
        assert!(limits.try_acquire(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 3))).is_none());
        drop(first); drop(second);
        assert_eq!(limits.active(), 0);
    }

    #[test]
    fn connection_attempt_limiter_survives_reconnect() {
        let limits = Arc::new(ConnectionLimits::new(2, 2, 1, Duration::from_secs(60), 100));
        let first = limits.try_acquire(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        drop(first);
        assert!(limits.try_acquire(IpAddr::V4(Ipv4Addr::LOCALHOST)).is_none());
    }

    #[test]
    fn connection_attempt_map_is_capped() {
        let limits = Arc::new(ConnectionLimits::new(10, 10, 5, Duration::from_secs(60), 1));
        let _first = limits.try_acquire(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1))).unwrap();
        assert!(limits.try_acquire(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 2))).is_none());
        assert!(limits.try_acquire(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1))).is_some(), "known keys still work");
    }

    #[test]
    fn idle_timeout_and_session_control_defaults() {
        let config = RelayConfig::default();
        // Several missed ~15 s keepalives before a paired socket is dropped.
        assert_eq!(config.idle_timeout, Duration::from_secs(60));
        let mut control = TokenBucket::new(config.session_control_burst, config.session_control_per_sec);
        assert!((0..48).all(|_| control.allow(1)), "ICE setup bursts must fit");
    }

    #[test]
    fn configured_deadline_is_positive() { assert!(RelayConfig::default().pair_timeout > Duration::ZERO); }
}
