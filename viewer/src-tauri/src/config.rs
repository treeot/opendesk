use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::unattended;

/// Release builds bake in the project's public relay via
/// `OPENDESK_DEFAULT_RELAY` (set from a repo variable in release.yml); source
/// builds default to a local development relay.
const DEFAULT_RELAY_URL: &str = match option_env!("OPENDESK_DEFAULT_RELAY") {
    Some(url) => url,
    None => "ws://127.0.0.1:8080/ws",
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub id: String,
    #[serde(default)]
    pub registration_token: String,
    /// Hash of the computer `id` and the token belong to (see `machine`).
    /// When the config turns up on another computer, as a cloned disk or a
    /// restored backup, that computer gets its own identity.
    #[serde(default)]
    machine: Option<String>,
    /// Relays that have accepted `id`. Each relay binds IDs on its own, so
    /// this is per relay URL. While it is empty a collision with another
    /// device's ID is fixed by picking a new one; after that the ID is one
    /// people know, so it never changes on its own. `None` only while
    /// loading a config from before this field.
    #[serde(default)]
    registered_relays: Option<Vec<String>>,
    // Read only for migration: the single flag that preceded
    // registered_relays. Never serialized back to disk.
    #[serde(default, rename = "id_registered", skip_serializing)]
    legacy_id_registered: Option<bool>,
    pub relay_url: String,
    #[serde(default)]
    pub relay_urls: Vec<String>,
    pub allow_list: Vec<String>,
    pub recent_ids: Vec<String>,
    #[serde(default)]
    pub unattended_password_set: bool,
    // Read only for migration. This field is never serialized back to disk.
    #[serde(default, rename = "unattended_password", skip_serializing)]
    legacy_unattended_password: Option<String>,
    #[serde(default)]
    pub unattended_enabled: bool,
    #[serde(default)]
    pub unattended_twofa_enabled: bool,
    /// Clipboard sync during sessions. On by default so copy and paste works
    /// across the session like other remote desktops; the host can still block
    /// the viewer's clipboard per session, and turning it off stops sharing in
    /// both directions.
    #[serde(default = "default_true")]
    pub clipboard_sync: bool,
    /// Viewers may add virtual displays during a session (macOS, or Windows
    /// with the driver from Home), and a computer with no screen gets
    /// one. On by default: it gives a viewer
    /// nothing their control of the mouse and keyboard doesn't, and the
    /// displays go when the session ends.
    #[serde(default = "default_true")]
    pub allow_virtual_displays: bool,
    /// Keep at most this many bytes of session recordings on disk; the oldest
    /// are deleted after each new recording to stay under it. 0 means no cap.
    #[serde(default = "default_recordings_max_bytes")]
    pub recordings_max_bytes: u64,
}

fn default_true() -> bool { true }
/// 5 GiB of recordings by default.
fn default_recordings_max_bytes() -> u64 { 5 * 1024 * 1024 * 1024 }

impl Default for Config {
    fn default() -> Self {
        Self {
            id: generate_id(),
            registration_token: generate_registration_token(),
            machine: None,
            registered_relays: Some(Vec::new()),
            legacy_id_registered: None,
            relay_url: DEFAULT_RELAY_URL.to_string(),
            relay_urls: vec![DEFAULT_RELAY_URL.to_string()],
            allow_list: Vec::new(),
            recent_ids: Vec::new(),
            unattended_password_set: false,
            legacy_unattended_password: None,
            unattended_enabled: false,
            unattended_twofa_enabled: false,
            clipboard_sync: true,
            allow_virtual_displays: true,
            recordings_max_bytes: default_recordings_max_bytes(),
        }
    }
}

impl Config {
    fn take_legacy_unattended_password(&mut self) -> Option<String> {
        self.legacy_unattended_password.take()
    }

    /// Configs from before per-relay tracking belong to devices that already
    /// registered, so they count as registered on the relays they list,
    /// unless the old flag says the ID never registered anywhere.
    fn migrate_registered_relays(&mut self) {
        if self.registered_relays.is_none() {
            let never_registered = self.legacy_id_registered.take() == Some(false);
            self.registered_relays = Some(if never_registered { Vec::new() } else { self.relay_urls.clone() });
        }
    }

    pub fn registered_relays(&self) -> &[String] {
        self.registered_relays.as_deref().unwrap_or_default()
    }

    /// A new ID and token, registered nowhere yet.
    fn new_identity(&mut self) {
        self.id = generate_id();
        self.registration_token = generate_registration_token();
        self.registered_relays = Some(Vec::new());
    }

    /// Ties the identity to `machine`. A config first seen on this machine
    /// keeps its identity; one made on a different machine gets a new one,
    /// since the original is presumably still using it. Returns whether the
    /// identity changed. An unknown machine changes nothing.
    fn bind_to_machine(&mut self, machine: Option<String>) -> bool {
        let Some(machine) = machine else { return false };
        let moved = self.machine.as_ref().is_some_and(|saved| *saved != machine);
        if moved {
            self.new_identity();
        }
        self.machine = Some(machine);
        moved
    }
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
    pub config: Config,
}

impl ConfigStore {
    pub fn load(app_data_dir: impl AsRef<Path>) -> io::Result<Self> {
        Self::load_on(app_data_dir, crate::machine::fingerprint())
    }

    /// [`ConfigStore::load`] on the machine with fingerprint `machine`.
    fn load_on(app_data_dir: impl AsRef<Path>, machine: Option<String>) -> io::Result<Self> {
        let dir = app_data_dir.as_ref();
        fs::create_dir_all(dir)?;
        let path = dir.join("config.json");
        let mut config = match fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(config) => config,
                Err(error) => {
                    let backup = backup_corrupt_file(&path)?;
                    log::warn!(
                        "could not parse config file ({error}); backed it up to {}",
                        backup.display()
                    );
                    Config::default()
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(error) => return Err(error),
        };
        if let Some(password) = config.take_legacy_unattended_password() {
            // Migration is best effort. The plaintext is removed from the
            // in-memory config regardless, so save() cannot preserve it.
            config.unattended_password_set = unattended::set_password(&password)
                .inspect_err(|error| log::warn!("could not migrate the unattended access password: {error}"))
                .is_ok();
        }
        // Migrate legacy configs without relay_urls while retaining relay_url
        // as the primary/back-compat field.
        if config.relay_urls.is_empty() && !config.relay_url.trim().is_empty() {
            config.relay_urls.push(config.relay_url.clone());
        }
        if config.relay_url.trim().is_empty() {
            config.relay_url = config.relay_urls.first().cloned().unwrap_or_else(|| DEFAULT_RELAY_URL.to_string());
        }
        config.migrate_registered_relays();
        if config.bind_to_machine(machine) {
            log::info!("config was copied from another computer; this one gets its own device ID");
        }
        let store = Self { path, config };
        store.save()?;
        Ok(store)
    }

    pub fn save(&self) -> io::Result<()> {
        warn_for_insecure_relays(&self.config);
        let bytes = serde_json::to_vec_pretty(&self.config).map_err(io::Error::other)?;
        write_atomic(&self.path, &bytes)
    }

    pub fn ensure_identity(&mut self) -> String {
        if self.config.id.len() != 9 || !self.config.id.bytes().all(|b| b.is_ascii_digit()) {
            self.config.id = generate_id();
            self.config.registered_relays = Some(Vec::new());
            let _ = self.save();
        }
        self.config.id.clone()
    }

    /// Replaces an ID that a relay refused (another device's, or this
    /// device's own token in use elsewhere) with a new ID and token. Returns
    /// the new ID, or `None` when some relay has accepted the current ID:
    /// people may know it, so it is kept and the problem shown instead.
    pub fn replace_colliding_id(&mut self) -> Option<String> {
        if !self.config.registered_relays().is_empty() {
            return None;
        }
        Some(self.reset_identity())
    }

    /// A new ID and token on the user's request. Anyone who saved the old ID
    /// needs the new one.
    pub fn reset_identity(&mut self) -> String {
        self.config.new_identity();
        let _ = self.save();
        self.config.id.clone()
    }

    pub fn is_registered_on(&self, relay: &str) -> bool {
        self.config.registered_relays().iter().any(|known| known == relay)
    }

    pub fn mark_registered_on(&mut self, relay: &str) {
        if !self.is_registered_on(relay) {
            self.config.registered_relays.get_or_insert_with(Vec::new).push(relay.to_owned());
            let _ = self.save();
        }
    }

    pub fn ensure_registration_token(&mut self) -> String {
        if self.config.registration_token.len() != 64
            || !self.config.registration_token.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            self.config.registration_token = generate_registration_token();
            let _ = self.save();
        }
        self.config.registration_token.clone()
    }

    pub fn add_allow(&mut self, id: &str) -> io::Result<()> {
        add_unique(&mut self.config.allow_list, id);
        self.save()
    }

    pub fn is_allowed(&self, id: &str) -> bool {
        self.config.allow_list.iter().any(|allowed| allowed == id)
    }

    pub fn remove_allow(&mut self, id: &str) -> io::Result<()> {
        self.config.allow_list.retain(|allowed| allowed != id);
        self.save()
    }

    pub fn add_recent(&mut self, id: &str) -> io::Result<()> {
        add_recent(&mut self.config.recent_ids, id);
        self.save()
    }

    /// Replace the ordered relay list. Rejects an empty list and any URL that
    /// fails [`validate_relay_url`] before anything is changed.
    pub fn set_relays(&mut self, urls: Vec<String>) -> Result<(), String> {
        let Some(first) = urls.first() else {
            return Err("at least one relay URL is required".into());
        };
        for url in &urls {
            validate_relay_url(url)?;
        }
        self.config.relay_url = first.clone();
        self.config.relay_urls = urls;
        self.save().map_err(|error| error.to_string())
    }

    pub fn set_clipboard_sync(&mut self, enabled: bool) -> io::Result<()> {
        self.config.clipboard_sync = enabled;
        self.save()
    }

    pub fn set_allow_virtual_displays(&mut self, enabled: bool) -> io::Result<()> {
        self.config.allow_virtual_displays = enabled;
        self.save()
    }

    pub fn set_recordings_max_bytes(&mut self, bytes: u64) -> io::Result<()> {
        self.config.recordings_max_bytes = bytes;
        self.save()
    }
}

/// A relay URL must be `wss://`, or plain `ws://` only to a loopback or
/// private-network address (local development and LAN relays). Unencrypted
/// relays on the internet expose pairing metadata and are refused.
pub fn validate_relay_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("wss://") {
        return Ok(());
    }
    if !lower.starts_with("ws://") {
        return Err(format!("relay URL must start with wss:// (or ws:// for a local relay): {url}"));
    }
    if is_insecure_remote_relay(url) {
        return Err(format!(
            "unencrypted ws:// relays are only allowed on this computer or a private network; use wss:// for {url}"
        ));
    }
    Ok(())
}

/// Whether `url` is a plain `ws://` relay outside loopback and private
/// network ranges (10/8, 172.16/12, 192.168/16, 169.254/16, fc00::/7,
/// fe80::/10). Host names other than `localhost` count as remote because
/// their resolution is not known here.
pub fn is_insecure_remote_relay(url: &str) -> bool {
    let Some(rest) = url.get(..5).filter(|scheme| scheme.eq_ignore_ascii_case("ws://")).map(|_| &url[5..]) else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = if let Some(host) = host.strip_prefix('[') {
        host.split(']').next().unwrap_or(host)
    } else {
        host.rsplit_once(':').map_or(host, |(host, _)| host)
    };
    if host.eq_ignore_ascii_case("localhost") {
        return false;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(address)) => {
            !(address.is_loopback() || address.is_private() || address.is_link_local())
        }
        Ok(std::net::IpAddr::V6(address)) => {
            let first = address.segments()[0];
            let unique_local = first & 0xfe00 == 0xfc00;
            let link_local = first & 0xffc0 == 0xfe80;
            !(address.is_loopback() || unique_local || link_local)
        }
        Err(_) => true,
    }
}

fn warn_for_insecure_relays(config: &Config) {
    for relay in std::iter::once(&config.relay_url).chain(config.relay_urls.iter()) {
        if is_insecure_remote_relay(relay) {
            log::warn!("insecure remote relay configured: {relay}");
        }
    }
}

/// Replace `path` with `bytes` so a crash leaves either the old or the new
/// file, never a truncated one: write a temp file in the same directory,
/// fsync it, rename it over the original, then (on unix) fsync the directory
/// so the rename itself is durable.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory"))?;
    let file_name = path.file_name().and_then(|name| name.to_str()).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no valid file name")
    })?;
    let mut suffix = [0u8; 8];
    rand::fill(&mut suffix);
    let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
    let temp = dir.join(format!(".{file_name}.{suffix}.tmp"));
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
        return result;
    }
    #[cfg(unix)]
    if let Ok(dir) = fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

pub(crate) fn backup_corrupt_file(path: &Path) -> io::Result<PathBuf> {
    let file_name = path.file_name().and_then(|name| name.to_str()).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "corrupt file has no valid file name")
    })?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let mut backup = path.with_file_name(format!("{file_name}.corrupt-{timestamp}.bak"));
    let mut suffix = 1;
    while backup.exists() {
        backup = path.with_file_name(format!("{file_name}.corrupt-{timestamp}-{suffix}.bak"));
        suffix += 1;
    }
    fs::rename(path, &backup)?;
    Ok(backup)
}

pub fn generate_id() -> String {
    rand::random_range(100_000_000..=999_999_999).to_string()
}

pub fn generate_registration_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn add_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
    }
}

fn add_recent(values: &mut Vec<String>, value: &str) {
    values.retain(|existing| existing != value);
    values.insert(0, value.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    fn test_dir() -> PathBuf {
        let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("opendesk-config-{}-{suffix}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn only_an_id_that_never_registered_is_replaced_on_collision() {
        let dir = test_dir();
        let mut store = ConfigStore::load(&dir).unwrap();
        let (first, first_token) = (store.config.id.clone(), store.config.registration_token.clone());
        assert!(store.config.registered_relays().is_empty());
        let replaced = store.replace_colliding_id().expect("a fresh ID is replaced");
        assert_ne!(replaced, first);
        assert_ne!(store.config.registration_token, first_token, "the token may be shared with a copy");
        assert_eq!(store.config.id, replaced);
        store.mark_registered_on("wss://a/ws");
        assert_eq!(store.replace_colliding_id(), None, "a known ID is kept");
        assert_eq!(store.config.id, replaced);
        let reloaded = ConfigStore::load(&dir).unwrap();
        assert!(reloaded.is_registered_on("wss://a/ws"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn registration_is_tracked_per_relay() {
        let dir = test_dir();
        let mut store = ConfigStore::load(&dir).unwrap();
        store.set_relays(vec!["wss://a/ws".into(), "wss://b/ws".into()]).unwrap();
        store.mark_registered_on("wss://a/ws");
        store.mark_registered_on("wss://a/ws");
        assert_eq!(store.config.registered_relays(), ["wss://a/ws"]);
        assert!(store.is_registered_on("wss://a/ws"));
        assert!(!store.is_registered_on("wss://b/ws"), "a new relay starts unregistered");
        // Known on one relay: a collision on another never changes the ID.
        let id = store.config.id.clone();
        assert_eq!(store.replace_colliding_id(), None);
        assert_eq!(store.config.id, id);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn configs_from_before_per_relay_tracking_count_as_registered_on_their_relays() {
        let dir = test_dir();
        fs::write(dir.join("config.json"), r#"{"id":"123456789","relay_url":"wss://r/ws","relay_urls":["wss://r/ws","wss://s/ws"],"allow_list":[],"recent_ids":[]}"#).unwrap();
        let store = ConfigStore::load_on(&dir, None).unwrap();
        assert_eq!(store.config.registered_relays(), ["wss://r/ws", "wss://s/ws"]);
        let single: Config = serde_json::from_str(r#"{"id":"123456789","relay_url":"wss://r/ws","allow_list":[],"recent_ids":[],"id_registered":true}"#).unwrap();
        let mut single = single;
        single.relay_urls.push(single.relay_url.clone());
        single.migrate_registered_relays();
        assert_eq!(single.registered_relays(), ["wss://r/ws"]);
        let mut fresh: Config = serde_json::from_str(r#"{"id":"123456789","relay_url":"wss://r/ws","relay_urls":["wss://r/ws"],"allow_list":[],"recent_ids":[],"id_registered":false}"#).unwrap();
        fresh.migrate_registered_relays();
        assert!(fresh.registered_relays().is_empty());
        let saved = fs::read_to_string(dir.join("config.json")).unwrap();
        assert!(!saved.contains("id_registered"), "{saved}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_config_copied_to_another_machine_gets_a_new_identity() {
        let dir = test_dir();
        let mut store = ConfigStore::load_on(&dir, Some("machine-a".into())).unwrap();
        store.mark_registered_on("wss://a/ws");
        let (id, token) = (store.config.id.clone(), store.config.registration_token.clone());
        // Same machine: nothing changes.
        let same = ConfigStore::load_on(&dir, Some("machine-a".into())).unwrap();
        assert_eq!((same.config.id.as_str(), same.config.registration_token.as_str()), (id.as_str(), token.as_str()));
        assert!(same.is_registered_on("wss://a/ws"));
        // Unknown machine (no OS ID): nothing changes either.
        let unknown = ConfigStore::load_on(&dir, None).unwrap();
        assert_eq!(unknown.config.id, id);
        // Another machine: new ID and token, registered nowhere.
        let copy = ConfigStore::load_on(&dir, Some("machine-b".into())).unwrap();
        assert_ne!(copy.config.id, id);
        assert_ne!(copy.config.registration_token, token);
        assert!(copy.config.registered_relays().is_empty());
        // ...and that is now this config's machine.
        let again = ConfigStore::load_on(&dir, Some("machine-b".into())).unwrap();
        assert_eq!(again.config.id, copy.config.id);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_config_from_before_machine_binding_keeps_its_identity() {
        let dir = test_dir();
        fs::write(dir.join("config.json"), r#"{"id":"123456789","registration_token":"ab","relay_url":"wss://r/ws","allow_list":[],"recent_ids":[]}"#).unwrap();
        let store = ConfigStore::load_on(&dir, Some("machine-a".into())).unwrap();
        assert_eq!(store.config.id, "123456789");
        let saved = fs::read_to_string(dir.join("config.json")).unwrap();
        assert!(saved.contains("machine-a"), "{saved}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reset_identity_replaces_even_a_known_id() {
        let dir = test_dir();
        let mut store = ConfigStore::load(&dir).unwrap();
        store.mark_registered_on("wss://a/ws");
        let (id, token) = (store.config.id.clone(), store.config.registration_token.clone());
        let new_id = store.reset_identity();
        assert_ne!(new_id, id);
        assert_ne!(store.config.registration_token, token);
        assert!(store.config.registered_relays().is_empty());
        assert_eq!(ConfigStore::load(&dir).unwrap().config.id, new_id);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn generated_id_is_nine_digits() {
        let id = generate_id();
        assert_eq!(id.len(), 9);
        assert!(id.bytes().all(|byte| byte.is_ascii_digit()));
    }

    #[test]
    fn generated_registration_token_is_32_random_bytes_as_hex() {
        let token = generate_registration_token();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn allow_list_adds_and_contains_once() {
        let mut values = Vec::new();
        add_unique(&mut values, "123456789");
        add_unique(&mut values, "123456789");
        assert_eq!(values, vec!["123456789"]);
        assert!(values.iter().any(|id| id == "123456789"));
    }

    #[test]
    fn allow_list_removes_ids() {
        let mut values = vec!["123456789".to_string(), "987654321".to_string()];
        values.retain(|id| id != "123456789");
        assert_eq!(values, vec!["987654321"]);
    }

    #[test]
    fn recent_ids_are_deduplicated_and_most_recent_first() {
        let mut values = Vec::new();
        add_recent(&mut values, "111111111");
        add_recent(&mut values, "222222222");
        add_recent(&mut values, "111111111");
        assert_eq!(values, vec!["111111111", "222222222"]);
    }

    #[test]
    fn legacy_unattended_password_is_migrated_to_a_marker() {
        let mut config = Config {
            legacy_unattended_password: Some("fixed-secret".into()),
            unattended_enabled: true,
            ..Config::default()
        };
        let password = config.take_legacy_unattended_password();
        config.unattended_password_set = password.is_some();
        let encoded = serde_json::to_string(&config).unwrap();
        assert_eq!(password.as_deref(), Some("fixed-secret"));
        assert!(config.unattended_password_set);
        assert!(!encoded.contains("fixed-secret"));
        assert!(!encoded.contains("\"unattended_password\""));
    }

    #[test]
    fn unattended_access_marker_round_trips_without_a_secret() {
        let config = Config {
            unattended_password_set: true,
            unattended_enabled: true,
            ..Config::default()
        };
        let encoded = serde_json::to_string(&config).unwrap();
        let decoded: Config = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.unattended_password_set);
        assert!(decoded.unattended_enabled);
    }

    #[test]
    fn legacy_single_relay_migrates_to_list() {
        let mut config: Config = serde_json::from_str(r#"{"id":"123456789","relay_url":"wss://legacy","allow_list":[],"recent_ids":[]}"#).unwrap();
        if config.relay_urls.is_empty() { config.relay_urls.push(config.relay_url.clone()); }
        assert_eq!(config.relay_urls, vec!["wss://legacy"]);
    }

    #[test]
    fn corrupt_config_is_backed_up_and_replaced_with_default() {
        let dir = test_dir();
        let original = b"{\"id\": ";
        fs::write(dir.join("config.json"), original).unwrap();

        let store = ConfigStore::load(&dir).unwrap();

        assert_eq!(store.config.relay_url, DEFAULT_RELAY_URL);
        let backups: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.file_name().unwrap().to_string_lossy().starts_with("config.json.corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(&backups[0]).unwrap(), original);
        assert_ne!(fs::read(dir.join("config.json")).unwrap(), original);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_config_returns_default_without_backup() {
        let dir = test_dir();

        let store = ConfigStore::load(&dir).unwrap();

        assert_eq!(store.config.relay_url, DEFAULT_RELAY_URL);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn insecure_remote_relay_detection_allows_local_relays() {
        assert!(!is_insecure_remote_relay("ws://127.0.0.1:8080/ws"));
        assert!(!is_insecure_remote_relay("ws://localhost:8080/ws"));
        assert!(!is_insecure_remote_relay("ws://[::1]:8080/ws"));
        assert!(!is_insecure_remote_relay("wss://example.com/ws"));
        assert!(is_insecure_remote_relay("ws://example.com/ws"));
        assert!(is_insecure_remote_relay("WS://example.com/ws"));
        assert!(is_insecure_remote_relay("ws://8.8.8.8:8080/ws"));
        assert!(is_insecure_remote_relay("ws://[2001:db8::1]:8080/ws"));
        assert!(!is_insecure_remote_relay("ws://10.0.0.5:8080/ws"));
        assert!(!is_insecure_remote_relay("ws://192.168.1.20/ws"));
        assert!(!is_insecure_remote_relay("ws://172.16.4.1/ws"));
        assert!(!is_insecure_remote_relay("ws://[fd00::5]:8080/ws"));
    }

    #[test]
    fn relay_url_validation_rejects_remote_plain_websocket() {
        assert!(validate_relay_url("wss://relay.example.com/ws").is_ok());
        assert!(validate_relay_url("ws://127.0.0.1:8080/ws").is_ok());
        assert!(validate_relay_url("ws://192.168.1.20:8080/ws").is_ok());
        let error = validate_relay_url("ws://relay.example.com/ws").unwrap_err();
        assert!(error.contains("wss://"), "{error}");
        assert!(validate_relay_url("ws://203.0.113.9:8080/ws").is_err());
        assert!(validate_relay_url("https://relay.example.com").is_err());
        assert!(validate_relay_url("relay.example.com").is_err());
    }

    #[test]
    fn set_relays_validates_every_url_before_saving() {
        let dir = test_dir();
        let mut store = ConfigStore::load(&dir).unwrap();
        let before = store.config.relay_urls.clone();
        let result = store.set_relays(vec!["wss://a.example/ws".into(), "ws://b.example/ws".into()]);
        assert!(result.is_err());
        assert_eq!(store.config.relay_urls, before);
        assert!(store.set_relays(Vec::new()).is_err());
        store.set_relays(vec!["wss://a.example/ws".into(), "ws://10.1.2.3/ws".into()]).unwrap();
        assert_eq!(store.config.relay_url, "wss://a.example/ws");
        assert_eq!(ConfigStore::load(&dir).unwrap().config.relay_urls, ["wss://a.example/ws", "ws://10.1.2.3/ws"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn atomic_write_replaces_file_and_leaves_no_temp() {
        let dir = test_dir();
        let path = dir.join("config.json");
        fs::write(&path, b"old contents that are longer").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        write_atomic(&dir.join("fresh.json"), b"{}").unwrap();
        assert_eq!(fs::read(dir.join("fresh.json")).unwrap(), b"{}");
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // A failed write leaves the original untouched.
        assert!(write_atomic(&dir.join("missing-dir").join("x.json"), b"x").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn virtual_displays_are_allowed_by_default_even_in_old_configs() {
        assert!(Config::default().allow_virtual_displays);
        let old: Config = serde_json::from_str(r#"{"id":"123456789","relay_url":"wss://x","allow_list":[],"recent_ids":[]}"#).unwrap();
        assert!(old.allow_virtual_displays);
        let off = Config { allow_virtual_displays: false, ..Config::default() };
        let decoded: Config = serde_json::from_str(&serde_json::to_string(&off).unwrap()).unwrap();
        assert!(!decoded.allow_virtual_displays);
    }

    #[test]
    fn clipboard_sync_defaults_on_and_persists() {
        let dir = test_dir();
        let mut store = ConfigStore::load(&dir).unwrap();
        assert!(store.config.clipboard_sync);
        store.set_clipboard_sync(false).unwrap();
        assert!(!ConfigStore::load(&dir).unwrap().config.clipboard_sync);
        let legacy: Config = serde_json::from_str(r#"{"id":"123456789","relay_url":"wss://x","allow_list":[],"recent_ids":[]}"#).unwrap();
        assert!(legacy.clipboard_sync, "old configs get the new on-by-default");
        let _ = fs::remove_dir_all(dir);
    }
}
