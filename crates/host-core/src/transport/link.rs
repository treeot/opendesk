//! What the viewer shows about the media path: which transport carries the
//! session and the latest round trip measured on it.
//!
//! On the direct path the RTT comes from the UDP feedback channel (the host
//! measures it and sends it in every sender report). On the relay it comes
//! from sealed `Control::Ping`/`Pong` frames. Peers without either never
//! produce a sample, and the RTT stays unknown.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A sample older than this is not shown: the peer stopped reporting.
pub const RTT_MAX_AGE: Duration = Duration::from_secs(3);
/// Samples above this are clock or echo garbage, not a round trip.
const RTT_LIMIT: Duration = Duration::from_secs(10);

/// The transport carrying a session's media.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPath {
    Direct,
    Relay,
}

impl MediaPath {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relay => "relay",
        }
    }
}

/// The latest round trip measured on a path, shared between whoever
/// measures it and whoever shows it.
#[derive(Debug, Default)]
pub struct RttMeter(Mutex<Option<(Duration, Instant)>>);

impl RttMeter {
    pub fn record(&self, rtt: Duration, now: Instant) {
        if rtt < RTT_LIMIT {
            *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((rtt, now));
        }
    }

    /// The latest sample, unless it is older than [`RTT_MAX_AGE`].
    pub fn latest(&self, now: Instant) -> Option<Duration> {
        let sample = *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        sample.filter(|(_, at)| now.saturating_duration_since(*at) <= RTT_MAX_AGE).map(|(rtt, _)| rtt)
    }
}

/// The share of media packets lost on a path, as the receiving side last
/// measured it over one report interval (before retransmission repairs it).
#[derive(Debug, Default)]
pub struct LossMeter(Mutex<Option<(f64, Instant)>>);

impl LossMeter {
    pub fn record(&self, fraction: f64, now: Instant) {
        if fraction.is_finite() {
            *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((fraction.clamp(0.0, 1.0), now));
        }
    }

    /// The latest sample, unless it is older than [`RTT_MAX_AGE`].
    pub fn latest(&self, now: Instant) -> Option<f64> {
        let sample = *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        sample.filter(|(_, at)| now.saturating_duration_since(*at) <= RTT_MAX_AGE).map(|(fraction, _)| fraction)
    }
}

/// Path and RTT of one session. The path is fixed for the session; a
/// reconnect builds a new session (and a new status) that may use the
/// other path.
#[derive(Debug, Clone)]
pub struct LinkStatus {
    pub path: MediaPath,
    rtt: Arc<RttMeter>,
    loss: Arc<LossMeter>,
}

impl LinkStatus {
    pub fn new(path: MediaPath, rtt: Arc<RttMeter>) -> Self { Self { path, rtt, loss: Arc::default() } }

    /// Also report the packet loss `loss` measures.
    pub fn with_loss(mut self, loss: Arc<LossMeter>) -> Self {
        self.loss = loss;
        self
    }

    pub fn rtt(&self, now: Instant) -> Option<Duration> { self.rtt.latest(now) }

    /// Latest packet loss fraction, `None` when the path doesn't measure it.
    pub fn loss(&self, now: Instant) -> Option<f64> { self.loss.latest(now) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_is_empty_until_a_sample_and_forgets_stale_ones() {
        let meter = RttMeter::default();
        let now = Instant::now();
        assert_eq!(meter.latest(now), None);
        meter.record(Duration::from_millis(14), now);
        assert_eq!(meter.latest(now + Duration::from_secs(1)), Some(Duration::from_millis(14)));
        assert_eq!(meter.latest(now + RTT_MAX_AGE + Duration::from_millis(1)), None);
    }

    #[test]
    fn loss_meter_clamps_and_forgets_stale_samples() {
        let meter = Arc::new(LossMeter::default());
        let now = Instant::now();
        let link = LinkStatus::new(MediaPath::Direct, Arc::default());
        assert_eq!(link.loss(now), None);
        let link = link.with_loss(Arc::clone(&meter));
        meter.record(0.25, now);
        assert_eq!(link.loss(now), Some(0.25));
        meter.record(f64::NAN, now);
        assert_eq!(link.loss(now), Some(0.25));
        meter.record(3.0, now);
        assert_eq!(link.loss(now), Some(1.0));
        assert_eq!(link.loss(now + RTT_MAX_AGE + Duration::from_millis(1)), None);
    }

    #[test]
    fn meter_ignores_absurd_samples() {
        let meter = RttMeter::default();
        let now = Instant::now();
        meter.record(Duration::from_secs(30), now);
        assert_eq!(meter.latest(now), None);
    }
}
