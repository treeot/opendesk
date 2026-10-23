use crate::ratemap::RateMap;
use axum::extract::ws::CloseFrame;
use dashmap::{mapref::entry::Entry, DashMap};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;
use tokio::sync::{mpsc::Sender, watch, Semaphore};

/// Most bytes queued toward one peer before the sender is backpressured. Bounds
/// per-session memory regardless of how large or how many frames are in flight,
/// unlike a frame-count channel where one slow receiver can pin frame_cap x
/// depth. Any single frame fits, since the forwarded-frame cap is below this.
pub(crate) const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;

/// A sender toward one peer, backpressured by queued bytes (not frame count).
/// Sending acquires byte permits that the peer's writer releases as it drains,
/// so a stalled or slow receiver blocks the sender instead of growing memory.
#[derive(Clone)]
pub struct Outbound {
    tx: Sender<Vec<u8>>,
    budget: Arc<Semaphore>,
}

impl Outbound {
    pub(crate) fn new(tx: Sender<Vec<u8>>, budget: Arc<Semaphore>) -> Self {
        Self { tx, budget }
    }

    /// Byte permits a frame costs, clamped so an over-cap frame still sends once
    /// the queue drains rather than deadlocking.
    pub(crate) fn permits_for(len: usize) -> usize {
        len.clamp(1, MAX_QUEUED_BYTES)
    }

    /// Queue `frame` toward the peer, waiting if the byte budget is full. `Err`
    /// means the peer is gone (receiver dropped).
    pub async fn send(&self, frame: Vec<u8>) -> Result<(), ()> {
        let cost = Self::permits_for(frame.len());
        let Ok(permit) = self.budget.clone().acquire_many_owned(cost as u32).await else { return Err(()) };
        // The permits are released by the peer's writer after it sends the frame.
        permit.forget();
        match self.tx.send(frame).await {
            Ok(()) => Ok(()),
            Err(_) => {
                // The frame never reached the queue, so give its budget back.
                self.budget.add_permits(cost);
                Err(())
            }
        }
    }

    pub(crate) fn same_channel(&self, other: &Outbound) -> bool {
        self.tx.same_channel(&other.tx)
    }

    #[cfg(test)]
    pub(crate) fn for_test(tx: Sender<Vec<u8>>) -> Self {
        Self::new(tx, Arc::new(Semaphore::new(MAX_QUEUED_BYTES)))
    }
}
pub type CloseOutbound = watch::Sender<Option<CloseFrame>>;

/// Delivered to a parked host once a viewer takes its registration: the
/// viewer's own sender, so the host can start forwarding its traffic there.
pub type PeerNotify = oneshot::Sender<(Outbound, CloseOutbound, String)>;

#[derive(Debug, PartialEq, Eq)]
pub enum Registration {
    Accepted,
    /// The ID is leased to a different token.
    IdTaken,
    /// The right token, but it keeps displacing another live connection
    /// with the same token: two copies of one device are fighting over it.
    InUse,
    /// The binding table is at its cap.
    Full,
    /// This source claimed too many new IDs recently.
    RateLimited,
}

#[derive(Clone, Debug)]
pub struct RegistryConfig {
    /// How long an ID stays bound to its token after the host was last seen,
    /// once it has successfully paired at least once.
    pub lease: Duration,
    /// How long an ID that has never paired stays bound. Short, so registering
    /// IDs and walking away cannot squat the registry for the full lease.
    pub unpaired_lease: Duration,
    pub max_bindings: usize,
    pub max_rate_entries: usize,
    pub connect_burst: u32,
    pub connect_refill: Duration,
    pub new_id_burst: u32,
    pub new_id_refill: Duration,
    /// Same-token takeovers of a live registration allowed per
    /// `takeover_window` before further ones are refused with `InUse`.
    pub takeover_limit: u32,
    pub takeover_window: Duration,
    pub state_file: Option<PathBuf>,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(30 * 24 * 3600),
            unpaired_lease: Duration::from_secs(24 * 3600),
            max_bindings: 100_000,
            max_rate_entries: 100_000,
            connect_burst: 6,
            connect_refill: Duration::from_secs(10),
            new_id_burst: 20,
            new_id_refill: Duration::from_secs(3600),
            takeover_limit: 3,
            takeover_window: Duration::from_secs(600),
            state_file: None,
        }
    }
}

#[derive(Clone)]
pub struct Registry {
    hosts: Arc<DashMap<String, RegistrationState>>,
    /// Number of entries in `hosts`, kept separately because `DashMap::len`
    /// would deadlock while a shard is held by `entry`.
    bindings: Arc<AtomicUsize>,
    connect_limits: Arc<RateMap<(IpAddr, String)>>,
    /// A global per-target connect bucket, so one actor cannot knock a specific
    /// host offline by spamming connects to it from many source addresses.
    target_connect_limits: Arc<RateMap<String>>,
    new_id_limits: Arc<RateMap<IpAddr>>,
    dirty: Arc<AtomicBool>,
    config: Arc<RegistryConfig>,
}

struct RegistrationState {
    /// SHA-256 of the host token; the raw token is never stored.
    token_hash: [u8; 32],
    owner_until: Instant,
    parked: Option<(Outbound, CloseOutbound, PeerNotify)>,
    active: Option<Outbound>,
    takeovers: Takeovers,
    /// Whether a viewer has ever paired with this id. Until then it holds only
    /// the short unpaired lease, so squatting an id costs that, not the full one.
    ever_paired: bool,
}

/// Counts same-token registrations that replaced a live parked one. A host
/// does that once after a network change leaves a stale socket behind; two
/// machines sharing one ID and token do it over and over.
#[derive(Clone, Copy, Default)]
struct Takeovers {
    count: u32,
    since: Option<Instant>,
}

impl Takeovers {
    /// Records one takeover at `now` and says whether it is within `limit`.
    fn record(&mut self, now: Instant, limit: u32, window: Duration) -> bool {
        if self.since.is_none_or(|since| now.duration_since(since) >= window) {
            *self = Self { count: 0, since: Some(now) };
        }
        self.count = self.count.saturating_add(1);
        self.count <= limit
    }
}

impl RegistrationState {
    fn online(&self) -> bool { self.parked.is_some() || self.active.is_some() }
}

#[derive(Serialize, Deserialize)]
struct StateFile { version: u32, bindings: Vec<StoredBinding> }

#[derive(Serialize, Deserialize)]
struct StoredBinding { id: String, token_sha256: String, expires_unix: u64 }

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepStats { pub bindings_removed: usize, pub buckets_removed: usize }

impl Registry {
    pub fn new(config: RegistryConfig) -> Self {
        Self {
            hosts: Arc::new(DashMap::new()),
            bindings: Arc::new(AtomicUsize::new(0)),
            connect_limits: Arc::new(RateMap::new(config.connect_burst, config.connect_refill, config.max_rate_entries)),
            target_connect_limits: Arc::new(RateMap::new(config.connect_burst, config.connect_refill, config.max_rate_entries)),
            new_id_limits: Arc::new(RateMap::new(config.new_id_burst, config.new_id_refill, config.max_rate_entries)),
            dirty: Arc::new(AtomicBool::new(false)),
            config: Arc::new(config),
        }
    }

    pub fn register(&self, peer_id: String, token: [u8; 32], source: IpAddr, tx: Outbound, close_tx: CloseOutbound, notify: PeerNotify) -> Registration {
        let now = Instant::now();
        let token_hash = hash_token(&token);
        // A fresh registration holds only the short unpaired lease; it extends
        // to the full lease once a viewer pairs (see `take`) or if this re-registers
        // an id that already paired.
        let mut state = RegistrationState { token_hash, owner_until: now + self.config.unpaired_lease, parked: Some((tx, close_tx, notify)), active: None, takeovers: Takeovers::default(), ever_paired: false };
        let result = match self.hosts.entry(peer_id) {
            Entry::Occupied(mut existing) => {
                let same_owner = constant_time_eq(&existing.get().token_hash, &token_hash);
                if !same_owner && existing.get().owner_until > now {
                    Registration::IdTaken
                } else if !same_owner && !self.new_id_limits.allow(rate_key(source)) {
                    Registration::RateLimited
                } else if same_owner && existing.get().parked.is_some()
                    && !existing.get_mut().takeovers.record(now, self.config.takeover_limit, self.config.takeover_window)
                {
                    // The live registration stays; the newcomer is told why.
                    Registration::InUse
                } else {
                    if same_owner {
                        state.takeovers = existing.get().takeovers;
                        // Keep the full lease for an id that already paired.
                        state.ever_paired = existing.get().ever_paired;
                        if state.ever_paired { state.owner_until = now + self.config.lease; }
                    }
                    existing.insert(state);
                    Registration::Accepted
                }
            }
            Entry::Vacant(slot) => {
                if self.bindings.load(Ordering::Relaxed) >= self.config.max_bindings {
                    Registration::Full
                } else if !self.new_id_limits.allow(rate_key(source)) {
                    Registration::RateLimited
                } else {
                    slot.insert(state);
                    self.bindings.fetch_add(1, Ordering::Relaxed);
                    Registration::Accepted
                }
            }
        };
        if result == Registration::Accepted { self.dirty.store(true, Ordering::Relaxed); }
        result
    }

    pub fn take(&self, peer_id: &str) -> Option<(Outbound, CloseOutbound, PeerNotify)> {
        self.hosts.get_mut(peer_id).and_then(|mut entry| {
            let parked = entry.parked.take();
            if let Some((tx, _, _)) = parked.as_ref() {
                entry.active = Some(tx.clone());
                // A successful pairing earns the full lease from here on.
                entry.ever_paired = true;
                entry.owner_until = Instant::now() + self.config.lease;
            }
            parked
        })
    }

    pub fn unregister(&self, peer_id: &str, tx: &Outbound) {
        if let Some(mut entry) = self.hosts.get_mut(peer_id) {
            let parked_matches = entry.parked.as_ref().is_some_and(|(current, _, _)| current.same_channel(tx));
            let active_matches = entry.active.as_ref().is_some_and(|current| current.same_channel(tx));
            if parked_matches || active_matches {
                entry.parked = None;
                entry.active = None;
                entry.owner_until = Instant::now() + self.config.lease;
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn allow_connect(&self, source: IpAddr, target: &str) -> bool {
        // Per (source /64, target) so one network cannot spend many targets'
        // buckets, and a global per-target bucket so many networks cannot gang
        // up to deny one host. Both must allow the attempt.
        self.connect_limits.allow((rate_key(source), target.to_owned()))
            && self.target_connect_limits.allow(target.to_owned())
    }

    pub fn binding_count(&self) -> usize { self.bindings.load(Ordering::Relaxed) }

    /// Whether a host for `id` is connected right now (parked or in a session).
    /// A lease kept for an offline host does not count as online.
    pub fn is_online(&self, id: &str) -> bool {
        self.hosts.get(id).is_some_and(|state| state.online())
    }

    /// Drops offline bindings whose lease has expired and rate buckets that
    /// have refilled.
    pub fn sweep(&self, now: Instant) -> SweepStats {
        let mut removed = 0;
        self.hosts.retain(|_, state| {
            let keep = state.online() || state.owner_until > now;
            if !keep { removed += 1; }
            keep
        });
        if removed > 0 {
            self.bindings.fetch_sub(removed, Ordering::Relaxed);
            self.dirty.store(true, Ordering::Relaxed);
        }
        SweepStats { bindings_removed: removed, buckets_removed: self.connect_limits.sweep(now) + self.target_connect_limits.sweep(now) + self.new_id_limits.sweep(now) }
    }

    pub fn state_file(&self) -> Option<&Path> { self.config.state_file.as_deref() }

    /// Writes the bindings to the state file if they changed since the last
    /// save. No-op without a state file.
    pub fn persist_if_dirty(&self) -> std::io::Result<bool> {
        let Some(path) = self.state_file() else { return Ok(false) };
        if !self.dirty.swap(false, Ordering::Relaxed) { return Ok(false); }
        let result = self.save(path);
        if result.is_err() { self.dirty.store(true, Ordering::Relaxed); }
        result.map(|()| true)
    }

    fn save(&self, path: &Path) -> std::io::Result<()> {
        let now = Instant::now();
        let now_unix = unix_now();
        let online_until = now_unix + self.config.lease.as_secs();
        let bindings = self.hosts.iter().filter_map(|entry| {
            let expires_unix = if entry.online() { online_until } else { now_unix + entry.owner_until.checked_duration_since(now)?.as_secs() };
            Some(StoredBinding { id: entry.key().clone(), token_sha256: hex(&entry.token_hash), expires_unix })
        }).collect();
        let json = serde_json::to_vec(&StateFile { version: 1, bindings }).map_err(std::io::Error::other)?;
        write_atomic(path, &json)
    }

    /// Loads bindings saved by [`Registry::persist_if_dirty`]. A missing file
    /// is not an error. Expired, malformed and over-cap entries are skipped.
    pub fn load(&self, path: &Path) -> std::io::Result<usize> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let file: StateFile = serde_json::from_slice(&bytes).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if file.version != 1 { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "unsupported state file version")); }
        let now = Instant::now();
        let now_unix = unix_now();
        let mut loaded = 0;
        for binding in file.bindings {
            if self.bindings.load(Ordering::Relaxed) >= self.config.max_bindings { break; }
            let Some(token_hash) = crate::session::decode_token(&binding.token_sha256) else { continue };
            if !crate::session::valid_peer_id(&binding.id) || binding.expires_unix <= now_unix { continue; }
            let remaining = Duration::from_secs(binding.expires_unix - now_unix).min(self.config.lease);
            if let Entry::Vacant(slot) = self.hosts.entry(binding.id) {
                // A persisted binding already earned its lease, so treat it as established.
                slot.insert(RegistrationState { token_hash, owner_until: now + remaining, parked: None, active: None, takeovers: Takeovers::default(), ever_paired: true });
                self.bindings.fetch_add(1, Ordering::Relaxed);
                loaded += 1;
            }
        }
        Ok(loaded)
    }
}

impl Default for Registry {
    fn default() -> Self { Self::new(RegistryConfig::default()) }
}

/// IPv6 clients usually control a whole /64, so new-ID limits apply per /64.
/// The key per-IP rate limits bucket on: a full IPv4 address, or an IPv6 /64,
/// since a single IPv6 allocation hands out an effectively unlimited number of
/// addresses. Shared so connection, connect, and new-ID limits all resist
/// address rotation the same way.
pub(crate) fn rate_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => IpAddr::V6((u128::from(v6) & !((1u128 << 64) - 1)).into()),
        other => other,
    }
}

fn hash_token(token: &[u8; 32]) -> [u8; 32] { Sha256::digest(token).into() }

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unix_now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs()) }

/// Writes `bytes` to a sibling temp file, syncs it and renames it over `path`.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    let mut file = options.open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

pub(crate) fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) { difference |= a ^ b; }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::{Outbound, Registration, Registry, RegistryConfig};
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{Duration, Instant};
    use tokio::sync::{mpsc, oneshot, watch};

    const SOURCE: IpAddr = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1));

    fn register_from(registry: &Registry, id: &str, token: [u8; 32], source: IpAddr) -> (Registration, Outbound) {
        let (tx, _rx) = mpsc::channel(1);
        let out = Outbound::for_test(tx);
        let (close_tx, _close_rx) = watch::channel(None);
        let (notify, _wait) = oneshot::channel();
        (registry.register(id.into(), token, source, out.clone(), close_tx, notify), out)
    }

    fn registration(registry: &Registry, id: &str, token: &str) -> (Registration, Outbound) {
        register_from(registry, id, token_from_byte(token), SOURCE)
    }

    fn token_from_byte(value: &str) -> [u8; 32] { [value.as_bytes().first().copied().unwrap_or(0); 32] }

    // Both leases set, so expiry tests cover a never-paired binding (which holds
    // only the unpaired lease) as well as a paired one.
    fn with_lease(lease: Duration) -> Registry { Registry::new(RegistryConfig { lease, unpaired_lease: lease, ..RegistryConfig::default() }) }

    #[test]
    fn default_lease_is_long() {
        assert!(RegistryConfig::default().lease >= Duration::from_secs(30 * 24 * 3600));
    }

    #[test]
    fn first_registration_binds_id_to_token() {
        let registry = Registry::default();
        let (result, _) = registration(&registry, "123456789", "first");
        assert_eq!(result, Registration::Accepted);
    }

    #[test]
    fn same_token_can_reregister() {
        let registry = Registry::default();
        assert_eq!(registration(&registry, "123456789", "same").0, Registration::Accepted);
        assert_eq!(registration(&registry, "123456789", "same").0, Registration::Accepted);
        assert_eq!(registry.binding_count(), 1);
    }

    #[test]
    fn different_token_cannot_replace_live_registration() {
        let registry = Registry::default();
        let (_, original_tx) = registration(&registry, "123456789", "owner");
        assert_eq!(registration(&registry, "123456789", "attacker").0, Registration::IdTaken);
        let (stored_tx, _, _) = registry.take("123456789").unwrap();
        assert!(original_tx.same_channel(&stored_tx));
    }

    #[test]
    fn ownership_lease_blocks_reclaim_until_expiry() {
        let registry = Registry::default();
        let (_, tx) = registration(&registry, "123456789", "old");
        registry.unregister("123456789", &tx);
        assert_eq!(registration(&registry, "123456789", "new").0, Registration::IdTaken);
    }

    #[test]
    fn ownership_can_be_reclaimed_after_lease_expiry() {
        let registry = with_lease(Duration::from_millis(1));
        assert_eq!(register_from(&registry, "123456789", [1; 32], SOURCE).0, Registration::Accepted);
        std::thread::sleep(Duration::from_millis(3));
        assert_eq!(register_from(&registry, "123456789", [2; 32], SOURCE).0, Registration::Accepted);
    }

    #[test]
    fn one_takeover_of_a_live_registration_is_fine() {
        // A host whose old socket is still parked after a network change.
        let registry = Registry::default();
        let (_, _stale) = registration(&registry, "123456789", "same");
        let (result, fresh) = registration(&registry, "123456789", "same");
        assert_eq!(result, Registration::Accepted);
        let (stored_tx, _, _) = registry.take("123456789").unwrap();
        assert!(fresh.same_channel(&stored_tx));
    }

    #[test]
    fn repeated_takeovers_with_one_token_are_refused_as_in_use() {
        let registry = Registry::new(RegistryConfig { takeover_limit: 2, ..RegistryConfig::default() });
        let (_, _first) = registration(&registry, "123456789", "same");
        assert_eq!(registration(&registry, "123456789", "same").0, Registration::Accepted);
        let (result, winner) = registration(&registry, "123456789", "same");
        assert_eq!(result, Registration::Accepted);
        assert_eq!(registration(&registry, "123456789", "same").0, Registration::InUse);
        assert_eq!(registration(&registry, "123456789", "same").0, Registration::InUse);
        // The live registration is left alone.
        let (stored_tx, _, _) = registry.take("123456789").unwrap();
        assert!(winner.same_channel(&stored_tx));
        // A different token is still just taken.
        assert_eq!(registration(&registry, "123456789", "other").0, Registration::IdTaken);
    }

    #[test]
    fn re_registering_after_the_old_one_closed_is_not_a_takeover() {
        let registry = Registry::new(RegistryConfig { takeover_limit: 1, ..RegistryConfig::default() });
        for _ in 0..5 {
            let (result, tx) = registration(&registry, "123456789", "same");
            assert_eq!(result, Registration::Accepted);
            registry.unregister("123456789", &tx);
        }
        // A taken (paired) registration is not live either.
        let (_, _parked) = registration(&registry, "123456789", "same");
        let _ = registry.take("123456789");
        assert_eq!(registration(&registry, "123456789", "same").0, Registration::Accepted);
    }

    #[test]
    fn takeover_count_resets_after_the_window() {
        let registry = Registry::new(RegistryConfig { takeover_limit: 1, takeover_window: Duration::from_millis(20), ..RegistryConfig::default() });
        let (_, _first) = registration(&registry, "123456789", "same");
        assert_eq!(registration(&registry, "123456789", "same").0, Registration::Accepted);
        assert_eq!(registration(&registry, "123456789", "same").0, Registration::InUse);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(registration(&registry, "123456789", "same").0, Registration::Accepted);
    }

    #[test]
    fn constant_time_compare_returns_correct_equality() {
        assert!(super::constant_time_eq(&[7; 32], &[7; 32]));
        assert!(!super::constant_time_eq(&[7; 32], &[8; 32]));
    }

    #[test]
    fn unregister_does_not_clobber_same_token_reregistration() {
        let registry = Registry::default();
        let (_, old_tx) = registration(&registry, "123456789", "same");
        let (_, new_tx) = registration(&registry, "123456789", "same");
        registry.unregister("123456789", &old_tx);
        let (stored_tx, _, _) = registry.take("123456789").unwrap();
        assert!(new_tx.same_channel(&stored_tx));
    }

    #[test]
    fn sweep_removes_only_expired_offline_bindings() {
        let registry = with_lease(Duration::from_millis(1));
        let (_, offline) = registration(&registry, "111111111", "a");
        registry.unregister("111111111", &offline);
        let (_, _parked) = registration(&registry, "222222222", "b");
        std::thread::sleep(Duration::from_millis(3));
        let stats = registry.sweep(Instant::now());
        assert_eq!(stats.bindings_removed, 1);
        assert_eq!(registry.binding_count(), 1);
        assert!(registry.take("222222222").is_some(), "online host survives the sweep");
    }

    #[test]
    fn binding_cap_rejects_new_ids_but_not_owners() {
        let registry = Registry::new(RegistryConfig { max_bindings: 1, ..RegistryConfig::default() });
        assert_eq!(registration(&registry, "111111111", "a").0, Registration::Accepted);
        assert_eq!(registration(&registry, "222222222", "b").0, Registration::Full);
        assert_eq!(registration(&registry, "111111111", "a").0, Registration::Accepted);
    }

    #[test]
    fn new_id_rate_limit_is_per_source() {
        let registry = Registry::new(RegistryConfig { new_id_burst: 2, ..RegistryConfig::default() });
        assert_eq!(registration(&registry, "111111111", "a").0, Registration::Accepted);
        assert_eq!(registration(&registry, "222222222", "a").0, Registration::Accepted);
        assert_eq!(registration(&registry, "333333333", "a").0, Registration::RateLimited);
        // Re-registering an owned ID costs nothing.
        assert_eq!(registration(&registry, "111111111", "a").0, Registration::Accepted);
        let other = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5));
        assert_eq!(register_from(&registry, "333333333", [b'a'; 32], other).0, Registration::Accepted);
        // Addresses in one IPv6 /64 share a bucket.
        let v6 = |last: u16| IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 1, 0, 0, 0, last));
        assert_eq!(register_from(&registry, "444444444", [1; 32], v6(1)).0, Registration::Accepted);
        assert_eq!(register_from(&registry, "555555555", [1; 32], v6(2)).0, Registration::Accepted);
        assert_eq!(register_from(&registry, "666666666", [1; 32], v6(3)).0, Registration::RateLimited);
    }

    #[test]
    fn connect_limit_map_is_capped() {
        let registry = Registry::new(RegistryConfig { max_rate_entries: 1, ..RegistryConfig::default() });
        assert!(registry.allow_connect(SOURCE, "111111111"));
        assert!(!registry.allow_connect(SOURCE, "222222222"));
    }

    #[test]
    fn unpaired_id_holds_only_the_short_lease() {
        let registry = Registry::new(RegistryConfig { lease: Duration::from_secs(3600), unpaired_lease: Duration::from_millis(20), ..RegistryConfig::default() });
        assert_eq!(registration(&registry, "123456789", "a").0, Registration::Accepted);
        // A different token is refused while the short unpaired lease still holds.
        assert_eq!(registration(&registry, "123456789", "b").0, Registration::IdTaken);
        std::thread::sleep(Duration::from_millis(30));
        // Once it lapses, a never-paired id is free for a new owner.
        assert_eq!(registration(&registry, "123456789", "b").0, Registration::Accepted);
    }

    #[test]
    fn pairing_extends_to_the_full_lease() {
        let registry = Registry::new(RegistryConfig { lease: Duration::from_secs(3600), unpaired_lease: Duration::from_millis(20), ..RegistryConfig::default() });
        registration(&registry, "123456789", "a");
        assert!(registry.take("123456789").is_some());
        std::thread::sleep(Duration::from_millis(30));
        // Having paired, it keeps the full lease past the unpaired window.
        assert_eq!(registration(&registry, "123456789", "b").0, Registration::IdTaken);
    }

    #[test]
    fn connect_limit_has_a_per_target_bucket_across_sources() {
        // A global per-target bucket (burst 2) blocks a host knockout even when
        // the attacker connects from a different source address each time.
        let registry = Registry::new(RegistryConfig { connect_burst: 2, max_rate_entries: 10_000, ..RegistryConfig::default() });
        let source = |n: u8| IpAddr::V4(Ipv4Addr::new(203, 0, 113, n));
        assert!(registry.allow_connect(source(1), "123456789"));
        assert!(registry.allow_connect(source(2), "123456789"));
        // Third attempt to the same target from a fresh source is denied by the
        // target bucket, not the per-source one.
        assert!(!registry.allow_connect(source(3), "123456789"));
        // A different target is unaffected.
        assert!(registry.allow_connect(source(3), "999999999"));
    }

    #[test]
    fn connect_limit_is_shared_within_an_ipv6_64() {
        // Two addresses in one /64 share the per-(source, target) bucket.
        let registry = Registry::new(RegistryConfig { connect_burst: 2, max_rate_entries: 10_000, ..RegistryConfig::default() });
        let v6 = |last: u16| IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 1, 0, 0, 0, last));
        assert!(registry.allow_connect(v6(1), "123456789"));
        assert!(registry.allow_connect(v6(2), "123456789"));
        assert!(!registry.allow_connect(v6(3), "123456789"));
    }

    #[test]
    fn bindings_survive_a_restart_via_state_file() {
        let dir = std::env::temp_dir().join(format!("relay-state-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let config = RegistryConfig { state_file: Some(path.clone()), ..RegistryConfig::default() };
        let registry = Registry::new(config.clone());
        let (_, tx) = registration(&registry, "123456789", "owner");
        registry.unregister("123456789", &tx);
        assert!(registry.persist_if_dirty().unwrap());
        assert!(!registry.persist_if_dirty().unwrap(), "clean registry is not rewritten");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&super::hex(&token_from_byte("owner"))), "raw token must not be stored");

        let restarted = Registry::new(config);
        assert_eq!(restarted.load(&path).unwrap(), 1);
        assert_eq!(registration(&restarted, "123456789", "squatter").0, Registration::IdTaken);
        assert_eq!(registration(&restarted, "123456789", "owner").0, Registration::Accepted);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_skips_expired_and_malformed_bindings() {
        let dir = std::env::temp_dir().join(format!("relay-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let good = "ab".repeat(32);
        std::fs::write(&path, format!(r#"{{"version":1,"bindings":[
            {{"id":"111111111","token_sha256":"{good}","expires_unix":1}},
            {{"id":"bad","token_sha256":"{good}","expires_unix":99999999999}},
            {{"id":"222222222","token_sha256":"zz","expires_unix":99999999999}},
            {{"id":"333333333","token_sha256":"{good}","expires_unix":99999999999}}]}}"#)).unwrap();
        let registry = Registry::default();
        assert_eq!(registry.load(&path).unwrap(), 1);
        assert_eq!(registry.binding_count(), 1);
        assert_eq!(registry.load(&dir.join("missing.json")).unwrap(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
