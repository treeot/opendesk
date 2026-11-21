//! Relay settings, read from environment variables and checked up front. A
//! bad value stops the relay at startup with a message naming the variable,
//! rather than being silently replaced by a default.

use crate::client_ip::{TrustedProxies, DEFAULT_TRUSTED_PROXIES};
use crate::registry::RegistryConfig;
use crate::RelayConfig;
use std::net::SocketAddr;
use std::time::Duration;

const DEFAULT_PORT: u16 = 8080;
const DEFAULT_MAX_CONNECTIONS: usize = 10_000;
const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 50;
/// Under Docker's default 10 s stop timeout, so sessions get their close
/// frames and the state file is saved before a SIGKILL.
const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 8;

/// Every variable the relay reads, so a typo can be pointed out.
const KNOWN: &[&str] = &[
    "RELAY_SESSION_CONTROL_BURST", "RELAY_SESSION_CONTROL_PER_SEC", "RELAY_MAX_BYTES_PER_SEC", "RELAY_BYTE_BURST",
    "RELAY_HANDSHAKE_TIMEOUT_SECS", "RELAY_PAIR_TIMEOUT_SECS", "RELAY_IDLE_TIMEOUT_SECS", "RELAY_MAX_SIGNAL_MESSAGE_BYTES",
    "RELAY_CONNECTION_ATTEMPT_BURST", "RELAY_CONNECTION_ATTEMPT_REFILL_SECS", "RELAY_SWEEP_SECS",
    "RELAY_ID_LEASE_SECS", "RELAY_OWNERSHIP_GRACE_SECS", "RELAY_UNPAIRED_ID_LEASE_SECS", "RELAY_MAX_ID_BINDINGS",
    "RELAY_MAX_RATE_ENTRIES", "RELAY_CONNECT_BURST", "RELAY_CONNECT_REFILL_SECS", "RELAY_NEW_ID_BURST",
    "RELAY_NEW_ID_REFILL_SECS", "RELAY_STATE_FILE", "RELAY_MAX_CONNECTIONS", "RELAY_MAX_CONNECTIONS_PER_IP",
    "RELAY_TRUSTED_PROXIES", "RELAY_SHUTDOWN_GRACE_SECS", "RELAY_METRICS_ADDR",
];

/// Validated relay settings.
#[derive(Clone)]
pub struct Settings {
    pub(crate) relay: RelayConfig,
    pub(crate) registry: RegistryConfig,
    pub(crate) max_connections: usize,
    pub(crate) max_connections_per_ip: usize,
    pub(crate) proxies: TrustedProxies,
    port: u16,
    shutdown_grace: Duration,
    metrics_addr: Option<SocketAddr>,
}

/// Every problem found in the environment, one per line.
#[derive(Debug)]
pub struct ConfigError(Vec<String>);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid relay configuration:")?;
        for problem in &self.0 { write!(f, "\n  {problem}")?; }
        Ok(())
    }
}

impl std::error::Error for ConfigError {}

impl Settings {
    /// Reads the process environment. Unknown `RELAY_*` variables are logged,
    /// not rejected.
    pub fn from_env() -> Result<Self, ConfigError> {
        for (name, _) in std::env::vars_os() {
            let Some(name) = name.to_str() else { continue };
            if name.starts_with("RELAY_") && !KNOWN.contains(&name) { log::warn!("ignoring unknown setting {name}"); }
        }
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads settings through `lookup` instead of the process environment.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut env = Env { lookup: &lookup, errors: Vec::new() };
        let defaults = RelayConfig::default();
        let relay = RelayConfig {
            session_control_burst: env.u64("RELAY_SESSION_CONTROL_BURST", defaults.session_control_burst),
            session_control_per_sec: env.u64("RELAY_SESSION_CONTROL_PER_SEC", defaults.session_control_per_sec),
            bytes_per_sec: env.u64("RELAY_MAX_BYTES_PER_SEC", defaults.bytes_per_sec),
            byte_burst: env.u64("RELAY_BYTE_BURST", defaults.byte_burst),
            handshake_timeout: env.secs("RELAY_HANDSHAKE_TIMEOUT_SECS", defaults.handshake_timeout),
            pair_timeout: env.secs("RELAY_PAIR_TIMEOUT_SECS", defaults.pair_timeout),
            idle_timeout: env.secs("RELAY_IDLE_TIMEOUT_SECS", defaults.idle_timeout),
            max_signal_message: env.usize("RELAY_MAX_SIGNAL_MESSAGE_BYTES", defaults.max_signal_message),
            connection_attempt_burst: env.u32("RELAY_CONNECTION_ATTEMPT_BURST", defaults.connection_attempt_burst),
            connection_attempt_refill: env.secs("RELAY_CONNECTION_ATTEMPT_REFILL_SECS", defaults.connection_attempt_refill),
            sweep_interval: env.secs("RELAY_SWEEP_SECS", defaults.sweep_interval),
        };
        if relay.byte_burst < relay.max_signal_message as u64 {
            env.errors.push(format!(
                "RELAY_BYTE_BURST ({}) must be at least RELAY_MAX_SIGNAL_MESSAGE_BYTES ({}), or the largest frames can never be forwarded",
                relay.byte_burst, relay.max_signal_message,
            ));
        }

        let defaults = RegistryConfig::default();
        // RELAY_OWNERSHIP_GRACE_SECS is the older name for the lease.
        let old_lease = env.secs("RELAY_OWNERSHIP_GRACE_SECS", defaults.lease);
        let registry = RegistryConfig {
            lease: env.secs("RELAY_ID_LEASE_SECS", old_lease),
            unpaired_lease: env.secs("RELAY_UNPAIRED_ID_LEASE_SECS", defaults.unpaired_lease),
            max_bindings: env.usize("RELAY_MAX_ID_BINDINGS", defaults.max_bindings),
            max_rate_entries: env.usize("RELAY_MAX_RATE_ENTRIES", defaults.max_rate_entries),
            connect_burst: env.u32("RELAY_CONNECT_BURST", defaults.connect_burst),
            connect_refill: env.secs("RELAY_CONNECT_REFILL_SECS", defaults.connect_refill),
            new_id_burst: env.u32("RELAY_NEW_ID_BURST", defaults.new_id_burst),
            new_id_refill: env.secs("RELAY_NEW_ID_REFILL_SECS", defaults.new_id_refill),
            takeover_limit: defaults.takeover_limit,
            takeover_window: defaults.takeover_window,
            state_file: env.raw("RELAY_STATE_FILE").map(Into::into),
        };

        let proxy_list = env.raw("RELAY_TRUSTED_PROXIES").unwrap_or_else(|| DEFAULT_TRUSTED_PROXIES.to_owned());
        let (proxies, invalid) = TrustedProxies::parse(&proxy_list);
        if !invalid.is_empty() {
            env.errors.push(format!("RELAY_TRUSTED_PROXIES has invalid entries {invalid:?}; expected CIDRs like 10.0.0.0/8, or `none`"));
        }

        let port = match env.raw("PORT") {
            None => DEFAULT_PORT,
            Some(value) => match value.parse::<u16>() {
                Ok(port) if port > 0 => port,
                _ => { env.errors.push(format!("PORT must be a port number from 1 to 65535, got {value:?}")); DEFAULT_PORT }
            },
        };
        let metrics_addr = env.raw("RELAY_METRICS_ADDR").and_then(|value| match value.parse::<SocketAddr>() {
            Ok(addr) => Some(addr),
            Err(_) => { env.errors.push(format!("RELAY_METRICS_ADDR must be an address and port such as 127.0.0.1:9100, got {value:?}")); None }
        });

        let settings = Self {
            relay,
            registry,
            max_connections: env.usize("RELAY_MAX_CONNECTIONS", DEFAULT_MAX_CONNECTIONS),
            max_connections_per_ip: env.usize("RELAY_MAX_CONNECTIONS_PER_IP", DEFAULT_MAX_CONNECTIONS_PER_IP),
            proxies,
            port,
            shutdown_grace: env.secs("RELAY_SHUTDOWN_GRACE_SECS", Duration::from_secs(DEFAULT_SHUTDOWN_GRACE_SECS)),
            metrics_addr,
        };
        if env.errors.is_empty() { Ok(settings) } else { Err(ConfigError(env.errors)) }
    }

    /// The port the WebSocket and health endpoints listen on (`PORT`).
    pub fn port(&self) -> u16 { self.port }

    /// How long shutdown waits for sessions to close (`RELAY_SHUTDOWN_GRACE_SECS`).
    pub fn shutdown_grace(&self) -> Duration { self.shutdown_grace }

    /// Where to serve `/metrics`, if anywhere (`RELAY_METRICS_ADDR`).
    pub fn metrics_addr(&self) -> Option<SocketAddr> { self.metrics_addr }

    /// How long a new connection has to finish its HTTP request and send its
    /// first WebSocket message.
    pub fn handshake_timeout(&self) -> Duration { self.relay.handshake_timeout }
}

impl Default for Settings {
    fn default() -> Self { Self::from_lookup(|_| None).expect("defaults are valid") }
}

struct Env<'a> {
    lookup: &'a dyn Fn(&str) -> Option<String>,
    errors: Vec<String>,
}

impl Env<'_> {
    /// The trimmed value, treating an empty one as unset.
    fn raw(&self, name: &str) -> Option<String> {
        (self.lookup)(name).map(|value| value.trim().to_owned()).filter(|value| !value.is_empty())
    }

    fn u64(&mut self, name: &str, default: u64) -> u64 { self.bounded(name, default, u64::MAX) }

    fn u32(&mut self, name: &str, default: u32) -> u32 { self.bounded(name, default as u64, u32::MAX as u64) as u32 }

    fn usize(&mut self, name: &str, default: usize) -> usize { self.bounded(name, default as u64, usize::MAX as u64) as usize }

    fn secs(&mut self, name: &str, default: Duration) -> Duration {
        // Ten years: far beyond any sensible value, and safe to add to an Instant.
        Duration::from_secs(self.bounded(name, default.as_secs(), 10 * 365 * 24 * 3600))
    }

    /// A whole number from 1 to `max`. Zero is refused rather than read as
    /// "off": none of these settings has an off switch.
    fn bounded(&mut self, name: &str, default: u64, max: u64) -> u64 {
        let Some(value) = self.raw(name) else { return default };
        match value.parse::<u64>() {
            Ok(number) if (1..=max).contains(&number) => number,
            _ => {
                self.errors.push(format!("{name} must be a whole number from 1 to {max}, got {value:?}"));
                default
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Settings;
    use std::time::Duration;

    fn with(pairs: &[(&str, &str)]) -> Result<Settings, super::ConfigError> {
        let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Settings::from_lookup(move |name| pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
    }

    #[test]
    fn defaults_are_valid() {
        let settings = with(&[]).unwrap();
        assert_eq!(settings.port(), 8080);
        assert_eq!(settings.metrics_addr(), None);
        assert!(settings.shutdown_grace() < Duration::from_secs(10), "fits Docker's stop timeout");
    }

    #[test]
    fn values_are_read_and_blank_means_unset() {
        let settings = with(&[("PORT", "9000"), ("RELAY_IDLE_TIMEOUT_SECS", " 90 "), ("RELAY_MAX_ID_BINDINGS", ""), ("RELAY_METRICS_ADDR", "127.0.0.1:9100")]).unwrap();
        assert_eq!(settings.port(), 9000);
        assert_eq!(settings.relay.idle_timeout, Duration::from_secs(90));
        assert_eq!(settings.registry.max_bindings, 100_000);
        assert_eq!(settings.metrics_addr(), Some("127.0.0.1:9100".parse().unwrap()));
    }

    #[test]
    fn the_old_lease_name_still_works() {
        assert_eq!(with(&[("RELAY_OWNERSHIP_GRACE_SECS", "60")]).unwrap().registry.lease, Duration::from_secs(60));
        assert_eq!(with(&[("RELAY_OWNERSHIP_GRACE_SECS", "60"), ("RELAY_ID_LEASE_SECS", "120")]).unwrap().registry.lease, Duration::from_secs(120));
    }

    #[test]
    fn bad_values_are_all_reported() {
        let error = with(&[
            ("RELAY_IDLE_TIMEOUT_SECS", "soon"),
            ("RELAY_MAX_CONNECTIONS", "0"),
            ("RELAY_NEW_ID_BURST", "5000000000"),
            ("RELAY_TRUSTED_PROXIES", "10.0.0.0/8,nonsense"),
            ("PORT", "70000"),
            ("RELAY_METRICS_ADDR", "9100"),
        ]).err().unwrap().to_string();
        for name in ["RELAY_IDLE_TIMEOUT_SECS", "RELAY_MAX_CONNECTIONS", "RELAY_NEW_ID_BURST", "nonsense", "PORT", "RELAY_METRICS_ADDR"] {
            assert!(error.contains(name), "{name} missing from: {error}");
        }
    }

    #[test]
    fn byte_burst_must_fit_the_largest_frame() {
        let error = with(&[("RELAY_BYTE_BURST", "1000")]).err().unwrap().to_string();
        assert!(error.contains("RELAY_BYTE_BURST"), "{error}");
        assert!(with(&[("RELAY_BYTE_BURST", "1000"), ("RELAY_MAX_SIGNAL_MESSAGE_BYTES", "1000")]).is_ok());
    }
}
