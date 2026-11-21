//! Operational counters, rendered in the Prometheus text format for the
//! optional `/metrics` endpoint. Plain atomics: counting costs a few
//! nanoseconds on the forwarding path and nothing when nobody scrapes.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(crate) struct Metrics {
    accepted: AtomicU64,
    pairings: AtomicU64,
    sessions: AtomicU64,
    relayed_bytes: AtomicU64,
    /// Keyed by (stage, reason). Both come from a fixed set of static strings,
    /// so the map stays small no matter what clients send.
    rejected: Mutex<BTreeMap<(&'static str, &'static str), u64>>,
}

/// Counts one paired session until dropped.
pub(crate) struct SessionGauge(Arc<Metrics>);

impl Drop for SessionGauge {
    fn drop(&mut self) { self.0.sessions.fetch_sub(1, Ordering::Relaxed); }
}

impl Metrics {
    pub(crate) fn accepted(&self) { self.accepted.fetch_add(1, Ordering::Relaxed); }

    pub(crate) fn relayed(&self, bytes: usize) { self.relayed_bytes.fetch_add(bytes as u64, Ordering::Relaxed); }

    pub(crate) fn rejected(&self, stage: &'static str, reason: &'static str) {
        *self.rejected.lock().expect("metrics lock poisoned").entry((stage, reason)).or_default() += 1;
    }

    pub(crate) fn session_started(self: &Arc<Self>) -> SessionGauge {
        self.pairings.fetch_add(1, Ordering::Relaxed);
        self.sessions.fetch_add(1, Ordering::Relaxed);
        SessionGauge(Arc::clone(self))
    }

    /// The Prometheus text exposition, with the gauges the caller reads from
    /// the connection limits and the registry.
    pub(crate) fn render(&self, connections: usize, id_bindings: usize) -> String {
        let mut out = String::new();
        let mut metric = |name: &str, kind: &str, help: &str, value: u64| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}");
        };
        metric("relay_connections", "gauge", "Open WebSocket connections.", connections as u64);
        metric("relay_sessions", "gauge", "Paired host and viewer sessions being relayed.", self.sessions.load(Ordering::Relaxed));
        metric("relay_id_bindings", "gauge", "Host IDs bound to an owner, online or within their lease.", id_bindings as u64);
        metric("relay_connections_accepted_total", "counter", "WebSocket connections accepted.", self.accepted.load(Ordering::Relaxed));
        metric("relay_pairings_total", "counter", "Viewers paired with a host.", self.pairings.load(Ordering::Relaxed));
        metric("relay_relayed_bytes_total", "counter", "Bytes forwarded between paired peers.", self.relayed_bytes.load(Ordering::Relaxed));
        let _ = writeln!(out, "# HELP relay_rejected_total Connections refused or dropped by the relay, by stage and reason.\n# TYPE relay_rejected_total counter");
        for ((stage, reason), count) in self.rejected.lock().expect("metrics lock poisoned").iter() {
            let _ = writeln!(out, "relay_rejected_total{{stage=\"{stage}\",reason=\"{reason}\"}} {count}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::Metrics;
    use std::sync::Arc;

    #[test]
    fn renders_prometheus_text() {
        let metrics = Arc::new(Metrics::default());
        metrics.accepted();
        metrics.relayed(1500);
        metrics.rejected("pairing", "id_taken");
        metrics.rejected("pairing", "id_taken");
        metrics.rejected("connection", "capacity");
        let session = metrics.session_started();
        let text = metrics.render(3, 7);
        for line in [
            "relay_connections 3",
            "relay_id_bindings 7",
            "relay_sessions 1",
            "relay_connections_accepted_total 1",
            "relay_pairings_total 1",
            "relay_relayed_bytes_total 1500",
            "# TYPE relay_rejected_total counter",
            "relay_rejected_total{stage=\"pairing\",reason=\"id_taken\"} 2",
            "relay_rejected_total{stage=\"connection\",reason=\"capacity\"} 1",
        ] {
            assert!(text.lines().any(|candidate| candidate == line), "missing {line:?} in:\n{text}");
        }
        drop(session);
        assert!(metrics.render(0, 0).contains("relay_sessions 0\n"));
    }
}
