use std::time::Duration;

/// How long a viewer keeps retrying after a session (or the initial attempt)
/// drops transiently. Time-based rather than attempt-based so a host that is
/// re-registering with the relay has a realistic chance to come back.
pub const VIEWER_RECONNECT_WINDOW: Duration = Duration::from_secs(30);

/// Upper bound for the viewer's per-attempt backoff.
pub const VIEWER_BACKOFF_CAP: Duration = Duration::from_secs(4);

/// How long the viewer keeps retrying when it knows the host is restarting
/// (it told us so). A reboot takes far longer than a transient drop.
pub const VIEWER_RESTART_WINDOW: Duration = Duration::from_secs(180);

/// First delay after a failed host registration; doubles per consecutive
/// failure up to [`HOST_REREGISTER_MAX_DELAY`].
pub const HOST_REREGISTER_BASE_DELAY: Duration = Duration::from_millis(500);
pub const HOST_REREGISTER_MAX_DELAY: Duration = Duration::from_secs(10);

/// A registration that stayed parked on the relay at least this long counts
/// as healthy when it ends (the relay drops parked hosts after its pairing
/// timeout, 60s by default), so the host re-registers immediately.
pub const HOST_STABLE_REGISTRATION: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    Transient,
    CleanUserDisconnect,
    AuthFailure,
    /// The relay says the target ID is offline or already in a session.
    PeerUnavailable,
    /// The relay refused the connect request (rate limit, busy, bad request).
    RelayRefused,
    /// The host hung up before confirming the key. beta.5 and older hosts do
    /// this on a wrong password; otherwise it is a drop like any other.
    KeyNotConfirmed,
}

/// Viewer backoff for the `attempt`-th retry (0-based): 500ms, 1s, 2s, 4s, 4s…
pub fn backoff_delay(attempt: u32) -> Duration {
    Duration::from_millis(500u64.saturating_mul(2u64.saturating_pow(attempt.min(3)))).min(VIEWER_BACKOFF_CAP)
}

/// Delay before the next viewer retry, or `None` once `elapsed` (time since the
/// first failure of the current outage) has used up the reconnect window. The
/// delay is clamped so the final attempt still starts inside the window.
pub fn viewer_retry_delay(attempt: u32, elapsed: Duration) -> Option<Duration> {
    viewer_retry_delay_within(attempt, elapsed, VIEWER_RECONNECT_WINDOW)
}

/// Like [`viewer_retry_delay`] but against an explicit window, so a viewer that
/// knows the host is restarting can wait the longer [`VIEWER_RESTART_WINDOW`].
pub fn viewer_retry_delay_within(attempt: u32, elapsed: Duration, window: Duration) -> Option<Duration> {
    let remaining = window.checked_sub(elapsed).filter(|remaining| !remaining.is_zero())?;
    Some(backoff_delay(attempt).min(remaining))
}

pub fn should_reconnect(reason: DisconnectReason) -> bool {
    matches!(reason, DisconnectReason::Transient)
}

/// Viewer retry rule. An unavailable peer, a relay refusal or an unconfirmed
/// key ends a first connect at once, but is retried after a session ran: the
/// host re-registers right after a drop, and a drop can land anywhere.
pub fn should_retry_viewer(reason: DisconnectReason, session_started: bool) -> bool {
    match reason {
        DisconnectReason::PeerUnavailable | DisconnectReason::RelayRefused | DisconnectReason::KeyNotConfirmed => session_started,
        reason => should_reconnect(reason),
    }
}

/// Viewer error text when the relay reports the target as unavailable.
pub const PEER_UNAVAILABLE_MESSAGE: &str = "That device is offline or already in a session.";

/// Viewer error text when the relay limits connections from this network.
pub const RELAY_LIMITED_MESSAGE: &str = "The relay is limiting connections from your network. Wait a minute and connect again.";

/// Viewer error text when the relay can't read the connect request.
pub const RELAY_REFUSED_MESSAGE: &str = "The relay didn't accept the connection request. Check the ID, and update OpenDesk if it keeps happening.";

/// Viewer error text for a relay refusing a `connect` with `code`, or `None`
/// for codes this build doesn't know.
pub fn connect_refusal_message(code: &str) -> Option<&'static str> {
    use protocol::control::*;
    match code {
        ERROR_PEER_UNAVAILABLE => Some(PEER_UNAVAILABLE_MESSAGE),
        ERROR_CONNECT_RATE | ERROR_RELAY_BUSY => Some(RELAY_LIMITED_MESSAGE),
        ERROR_INVALID_CONNECT | ERROR_INVALID_CONTROL => Some(RELAY_REFUSED_MESSAGE),
        _ => None,
    }
}

/// The viewer's error when the host closes the connection after the key
/// exchange but before confirming the key.
pub const KEY_NOT_CONFIRMED: &str = "connection closed during KEX confirmation";

/// Stable `Control::Error` codes a host sends when it rejects a viewer after
/// the key exchange.
pub mod reject_code {
    /// The host user declined the connection request.
    pub const DENIED: &str = "denied";
    /// The host requires a 2FA code and none was sent.
    pub const TWOFA_REQUIRED: &str = "2fa_required";
    /// The 2FA code was wrong, expired, or already used (replay).
    pub const TWOFA_INVALID: &str = "2fa_invalid";
    /// The host accepted the viewer but cannot capture its screen (missing
    /// OS permission). Retrying will not help until someone fixes the host.
    pub const CAPTURE_UNAVAILABLE: &str = "capture_unavailable";
    /// Nobody answered the connection request on the host in time.
    pub const NO_ANSWER: &str = "no_answer";
    /// The host has no screen (headless) and could not add a virtual one.
    pub const NO_DISPLAY: &str = "no_display";
}

/// What the viewer shows when the host has no screen to share.
pub const NO_DISPLAY_MESSAGE: &str = "That computer has no screen connected. Plug in a monitor or an HDMI dummy plug, or turn on \"Let viewers add virtual displays\" in OpenDesk there (on Windows, also click \"Install virtual display driver\").";

const REJECTION_PREFIX: &str = "session rejected by host: ";

/// Viewer error text for an explicit host rejection carrying `code`.
pub fn rejection_error(code: &str) -> String {
    format!("{REJECTION_PREFIX}{code}")
}

/// The host's rejection code, when `error` came from an explicit rejection.
pub fn rejection_code(error: &str) -> Option<&str> {
    error.strip_prefix(REJECTION_PREFIX)
}

/// Viewer error text when the host deliberately ended the session with a
/// sealed `Control::Bye`. It is a clean end: the viewer must not reconnect.
pub const HOST_ENDED_MESSAGE: &str = "The host ended the session.";

/// Only explicit authentication signals are fatal: a failed PAKE (wrong
/// password) or the host's explicit rejection. A host's explicit `Bye` is a
/// clean end. Everything else — including a connection that closes after the
/// key exchange but before video — is treated as transient and retried
/// within the reconnect window.
pub fn classify_viewer_error(error: &str) -> DisconnectReason {
    if error == HOST_ENDED_MESSAGE {
        return DisconnectReason::CleanUserDisconnect;
    }
    if error == PEER_UNAVAILABLE_MESSAGE {
        return DisconnectReason::PeerUnavailable;
    }
    if error == RELAY_LIMITED_MESSAGE || error == RELAY_REFUSED_MESSAGE {
        return DisconnectReason::RelayRefused;
    }
    if error == KEY_NOT_CONFIRMED {
        return DisconnectReason::KeyNotConfirmed;
    }
    let lower = error.to_ascii_lowercase();
    if lower.contains("invalid spake2") || lower.contains("key confirmation failed")
        || lower.contains("session rejected") {
        DisconnectReason::AuthFailure
    } else if lower.contains("connection closed") || lower.contains("transport")
        || lower.contains("channel") || lower.contains("relay") {
        DisconnectReason::Transient
    } else if lower.contains("user") || lower.contains("clean") {
        DisconnectReason::CleanUserDisconnect
    } else {
        // Errors reaching this classifier come from a failed session attempt;
        // unknown attempt failures are safer to treat as transient than to
        // strand a viewer after a relay/transport implementation changes its
        // wording.
        DisconnectReason::Transient
    }
}

/// User-facing text for an authentication failure that stopped reconnecting.
/// `after_session` is true when an earlier session in this connection ran,
/// i.e. the failure happened on an automatic reconnect.
pub fn auth_failure_message(error: &str, after_session: bool) -> String {
    match rejection_code(error) {
        Some(reject_code::DENIED) => "The host declined the connection request.".to_owned(),
        Some(reject_code::TWOFA_REQUIRED) => {
            "The host requires a two-factor code. Enter the current code from your authenticator and connect again.".to_owned()
        }
        Some(reject_code::TWOFA_INVALID) if after_session => {
            "Reconnect needs a fresh 2FA code: the previous code was already used. Enter the current code and connect again.".to_owned()
        }
        Some(reject_code::TWOFA_INVALID) => {
            "The 2FA code was rejected (wrong, expired, or already used). Enter the current code and connect again.".to_owned()
        }
        Some(reject_code::NO_ANSWER) => "Nobody answered the connection request on the other device. Try again when someone is there.".to_owned(),
        Some(reject_code::CAPTURE_UNAVAILABLE) => {
            "The other device can't share its screen. On that device, check that a display is on and Screen Recording is allowed for OpenDesk, restart it, and connect again.".to_owned()
        }
        Some(reject_code::NO_DISPLAY) => NO_DISPLAY_MESSAGE.to_owned(),
        Some(other) => format!("The host rejected the session ({other})."),
        None if error == KEY_NOT_CONFIRMED => {
            "The other device hung up while checking the password, so the password is probably wrong. It changes after every session: get the current one and connect again.".to_owned()
        }
        None => format!("{error}; authentication failed—check the password and connect again (it changes after every session)"),
    }
}

/// What the viewer shows once [`should_retry_viewer`] says to stop.
pub fn stop_message(error: &str, reason: DisconnectReason, session_started: bool) -> String {
    match reason {
        DisconnectReason::AuthFailure | DisconnectReason::KeyNotConfirmed => auth_failure_message(error, session_started),
        _ => error.to_owned(),
    }
}

/// Backoff for the host's relay re-registration loop. Pure state so the
/// schedule is unit-testable; the caller sleeps for [`Self::delay`] before each
/// registration attempt and reports how the attempt ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostRegistrationBackoff {
    consecutive_failures: u32,
    /// Set by [`Self::record_refusal`]: the least the next delay may be.
    at_least: Duration,
}

impl HostRegistrationBackoff {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// 0 after a healthy registration, then 500ms, 1s, 2s, 4s, 8s, 10s, 10s…
    pub fn delay(&self) -> Duration {
        host_reregister_delay(self.consecutive_failures).max(self.at_least)
    }

    /// Connecting or registering failed outright.
    pub fn record_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.at_least = Duration::ZERO;
    }

    /// The relay refused the registration for a reason that won't clear for
    /// a while, such as its limit on new IDs: wait at least `wait`.
    pub fn record_refusal(&mut self, wait: Duration) {
        self.record_failure();
        self.at_least = wait;
    }

    /// A registration was accepted and parked for `parked_for` before the relay
    /// closed it without a pairing. Short-lived registrations (e.g. the relay
    /// rejecting the ID) back off; long-lived ones re-register immediately.
    pub fn record_parked_then_closed(&mut self, parked_for: Duration) {
        if parked_for >= HOST_STABLE_REGISTRATION {
            *self = Self::new();
        } else {
            self.record_failure();
        }
    }

    /// A viewer paired with this registration.
    pub fn record_paired(&mut self) {
        *self = Self::new();
    }
}

/// Host re-registration delay after `consecutive_failures` failed attempts.
pub fn host_reregister_delay(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return Duration::ZERO;
    }
    let exponent = (consecutive_failures - 1).min(16);
    HOST_REREGISTER_BASE_DELAY
        .saturating_mul(1u32 << exponent)
        .min(HOST_REREGISTER_MAX_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_four_second_cap() {
        assert_eq!(backoff_delay(0), Duration::from_millis(500));
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_delay(4), Duration::from_secs(4));
        assert_eq!(backoff_delay(u32::MAX), Duration::from_secs(4));
    }

    #[test]
    fn the_restart_window_keeps_retrying_well_past_the_transient_one() {
        // Just after the transient window: a normal drop gives up, a restart
        // keeps going.
        let past_transient = VIEWER_RECONNECT_WINDOW + Duration::from_secs(1);
        assert_eq!(viewer_retry_delay_within(9, past_transient, VIEWER_RECONNECT_WINDOW), None);
        assert!(viewer_retry_delay_within(9, past_transient, VIEWER_RESTART_WINDOW).is_some());
        // The restart window itself still ends.
        assert_eq!(viewer_retry_delay_within(9, VIEWER_RESTART_WINDOW, VIEWER_RESTART_WINDOW), None);
    }

    #[test]
    fn viewer_retries_are_bounded_by_time_window_not_attempt_count() {
        // Simulate the schedule assuming each attempt fails instantly.
        let mut elapsed = Duration::ZERO;
        let mut attempt = 0;
        while let Some(delay) = viewer_retry_delay(attempt, elapsed) {
            elapsed += delay;
            attempt += 1;
        }
        assert_eq!(elapsed, VIEWER_RECONNECT_WINDOW);
        // 0.5+1+2+4 = 7.5s, then 4s steps: far more than the old 5 attempts.
        assert_eq!(attempt, 10);
    }

    #[test]
    fn viewer_retry_window_clamps_and_expires() {
        assert_eq!(viewer_retry_delay(0, Duration::ZERO), Some(Duration::from_millis(500)));
        assert_eq!(viewer_retry_delay(5, Duration::from_secs(10)), Some(Duration::from_secs(4)));
        assert_eq!(viewer_retry_delay(5, Duration::from_secs(29)), Some(Duration::from_secs(1)));
        assert_eq!(viewer_retry_delay(0, VIEWER_RECONNECT_WINDOW), None);
        assert_eq!(viewer_retry_delay(0, Duration::from_secs(45)), None);
    }

    #[test]
    fn unavailable_peer_ends_a_first_connect_but_not_a_reconnect() {
        let reason = classify_viewer_error(PEER_UNAVAILABLE_MESSAGE);
        assert_eq!(reason, DisconnectReason::PeerUnavailable);
        assert!(!should_retry_viewer(reason, false));
        assert!(should_retry_viewer(reason, true));
        assert!(should_retry_viewer(DisconnectReason::Transient, false));
        assert!(!should_retry_viewer(DisconnectReason::AuthFailure, true));
    }

    /// A beta.5 host hangs up on a wrong password without confirming the
    /// key. On a first connect that is what it almost always means; on a
    /// reconnect it is still treated as a drop.
    #[test]
    fn a_hang_up_before_key_confirmation_stops_a_first_connect() {
        let error = "connection closed during KEX confirmation";
        let reason = classify_viewer_error(error);
        assert!(!should_retry_viewer(reason, false));
        let message = stop_message(error, reason, false);
        assert!(message.contains("password is probably wrong") && message.contains("changes after every session"), "{message}");
        assert!(should_retry_viewer(reason, true));
        assert!(should_retry_viewer(classify_viewer_error("connection closed during KEX"), false));
    }

    #[test]
    fn relay_refusals_of_a_connect_are_explained() {
        use protocol::control::*;
        for code in [ERROR_CONNECT_RATE, ERROR_RELAY_BUSY, ERROR_INVALID_CONNECT, ERROR_INVALID_CONTROL] {
            let message = connect_refusal_message(code).unwrap();
            let reason = classify_viewer_error(message);
            assert!(!should_retry_viewer(reason, false), "{code}");
            assert!(should_retry_viewer(reason, true), "{code}");
            assert_eq!(stop_message(message, reason, false), message);
        }
        assert_eq!(connect_refusal_message(ERROR_PEER_UNAVAILABLE), Some(PEER_UNAVAILABLE_MESSAGE));
        assert_eq!(connect_refusal_message("from_a_newer_relay"), None);
    }

    #[test]
    fn capture_failure_explains_what_to_fix_on_the_host() {
        let error = rejection_error(reject_code::CAPTURE_UNAVAILABLE);
        assert_eq!(classify_viewer_error(&error), DisconnectReason::AuthFailure);
        assert!(auth_failure_message(&error, false).contains("can't share its screen"));
    }

    #[test]
    fn a_host_without_a_screen_says_how_to_add_one() {
        let error = rejection_error(reject_code::NO_DISPLAY);
        assert_eq!(classify_viewer_error(&error), DisconnectReason::AuthFailure);
        let message = auth_failure_message(&error, false);
        assert!(message.contains("no screen connected") && message.contains("HDMI dummy plug"), "{message}");
        assert!(message.contains("Install virtual display driver") && !message.contains("Screen Recording"), "{message}");
    }

    #[test]
    fn wrong_password_message_mentions_rotation() {
        assert!(auth_failure_message("key confirmation failed", false).contains("changes after every session"));
    }

    #[test]
    fn classifier_only_retries_transient_disconnects() {
        assert!(should_reconnect(DisconnectReason::Transient));
        assert!(!should_reconnect(DisconnectReason::CleanUserDisconnect));
        assert!(!should_reconnect(DisconnectReason::AuthFailure));
    }

    #[test]
    fn viewer_error_mapping_preserves_retry_boundary() {
        assert_eq!(classify_viewer_error("connection closed during KEX"), DisconnectReason::Transient);
        assert_eq!(classify_viewer_error("relay connection timed out"), DisconnectReason::Transient);
        assert_eq!(classify_viewer_error("relay closed while connecting"), DisconnectReason::Transient);
        // A half-open pairing socket that is timed out must retry, not strand the viewer.
        assert_eq!(classify_viewer_error("relay did not answer the connection request"), DisconnectReason::Transient);
        assert_eq!(classify_viewer_error("invalid SPAKE2 message"), DisconnectReason::AuthFailure);
        assert_eq!(classify_viewer_error("key confirmation failed"), DisconnectReason::AuthFailure);
        assert_eq!(classify_viewer_error("session rejected"), DisconnectReason::AuthFailure);
        assert_eq!(classify_viewer_error("user ended session"), DisconnectReason::CleanUserDisconnect);
        assert_eq!(classify_viewer_error(HOST_ENDED_MESSAGE), DisconnectReason::CleanUserDisconnect);
        assert!(!should_reconnect(classify_viewer_error(HOST_ENDED_MESSAGE)));
        assert_eq!(classify_viewer_error("connection closed: host stopped responding"), DisconnectReason::Transient);
    }

    #[test]
    fn explicit_rejection_codes_are_auth_failures() {
        for code in [reject_code::DENIED, reject_code::TWOFA_REQUIRED, reject_code::TWOFA_INVALID, "future_code"] {
            let error = rejection_error(code);
            assert_eq!(rejection_code(&error), Some(code));
            assert_eq!(classify_viewer_error(&error), DisconnectReason::AuthFailure, "{code}");
            assert!(!should_reconnect(classify_viewer_error(&error)));
        }
    }

    #[test]
    fn drops_without_explicit_signal_stay_transient() {
        // Closed after the key exchange but before any video: a network drop,
        // not a rejection, unless the host said so explicitly.
        assert_eq!(classify_viewer_error("connection closed during session"), DisconnectReason::Transient);
        assert_eq!(classify_viewer_error("session ended before the data channel was ready"), DisconnectReason::Transient);
        assert_eq!(rejection_code("connection closed during session"), None);
    }

    #[test]
    fn auth_failure_message_is_specific_per_code() {
        assert!(auth_failure_message(&rejection_error(reject_code::DENIED), false).contains("declined"));
        assert!(auth_failure_message(&rejection_error(reject_code::TWOFA_REQUIRED), false).contains("requires a two-factor code"));
        assert!(auth_failure_message(&rejection_error(reject_code::TWOFA_INVALID), true).contains("Reconnect needs a fresh 2FA code"));
        assert!(auth_failure_message(&rejection_error(reject_code::TWOFA_INVALID), false).contains("already used"));
        assert!(auth_failure_message(&rejection_error(reject_code::NO_ANSWER), false).contains("Nobody answered"));
        assert!(auth_failure_message("key confirmation failed", false).contains("check the password"));
    }

    #[test]
    fn host_reregister_delay_doubles_to_ten_second_cap() {
        let schedule: Vec<_> = (0..8).map(host_reregister_delay).collect();
        assert_eq!(
            schedule,
            [0, 500, 1_000, 2_000, 4_000, 8_000, 10_000, 10_000].map(Duration::from_millis)
        );
        assert_eq!(host_reregister_delay(u32::MAX), HOST_REREGISTER_MAX_DELAY);
    }

    #[test]
    fn host_backoff_resets_on_pairing_and_stable_registration() {
        let mut backoff = HostRegistrationBackoff::new();
        assert_eq!(backoff.delay(), Duration::ZERO);
        backoff.record_failure();
        backoff.record_failure();
        assert_eq!(backoff.delay(), Duration::from_secs(1));
        // Relay rejected the registration immediately: keep backing off.
        backoff.record_parked_then_closed(Duration::from_millis(5));
        assert_eq!(backoff.delay(), Duration::from_secs(2));
        // Relay's pairing timeout closed a healthy parked registration.
        backoff.record_parked_then_closed(Duration::from_secs(60));
        assert_eq!(backoff.delay(), Duration::ZERO);
        backoff.record_failure();
        backoff.record_paired();
        assert_eq!(backoff.consecutive_failures(), 0);
    }

    #[test]
    fn a_rate_limited_host_waits_out_the_limit() {
        let mut backoff = HostRegistrationBackoff::new();
        backoff.record_refusal(Duration::from_secs(180));
        assert_eq!(backoff.delay(), Duration::from_secs(180));
        assert_eq!(backoff.consecutive_failures(), 1);
        // Another relay failing in the ordinary way goes back to the schedule.
        backoff.record_failure();
        assert_eq!(backoff.delay(), Duration::from_secs(1));
        backoff.record_refusal(Duration::from_secs(180));
        backoff.record_paired();
        assert_eq!(backoff.delay(), Duration::ZERO);
    }
}
