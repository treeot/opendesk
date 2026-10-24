//! The desktop viewer's retry rules and error wording, for the browser.
//!
//! A copy of the viewer half of `host_core::reconnect` (host-core does not
//! build for wasm). `tests/interop.rs` checks the two stay in step.

use std::time::Duration;

/// How long a viewer keeps retrying after a session drops transiently.
pub const VIEWER_RECONNECT_WINDOW: Duration = Duration::from_secs(60);
/// Upper bound for the per-attempt backoff.
pub const VIEWER_BACKOFF_CAP: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    Transient,
    CleanUserDisconnect,
    AuthFailure,
    PeerUnavailable,
    RelayRefused,
    KeyNotConfirmed,
}

/// Backoff for the `attempt`-th retry (0-based): 500ms, 1s, 2s, 4s, 4s…
pub fn backoff_delay(attempt: u32) -> Duration {
    Duration::from_millis(500u64.saturating_mul(2u64.saturating_pow(attempt.min(3)))).min(VIEWER_BACKOFF_CAP)
}

/// Delay before the next retry, or `None` once `elapsed` has used up the window.
pub fn viewer_retry_delay(attempt: u32, elapsed: Duration) -> Option<Duration> {
    let remaining = VIEWER_RECONNECT_WINDOW.checked_sub(elapsed).filter(|remaining| !remaining.is_zero())?;
    Some(backoff_delay(attempt).min(remaining))
}

/// An unavailable peer, a relay refusal or an unconfirmed key ends a first
/// connect at once, but is retried after a session ran: the host
/// re-registers right after a drop.
pub fn should_retry_viewer(reason: DisconnectReason, session_started: bool) -> bool {
    match reason {
        DisconnectReason::PeerUnavailable | DisconnectReason::RelayRefused | DisconnectReason::KeyNotConfirmed => session_started,
        reason => matches!(reason, DisconnectReason::Transient),
    }
}

pub const PEER_UNAVAILABLE_MESSAGE: &str = "That device is offline or already in a session.";
pub const RELAY_LIMITED_MESSAGE: &str = "The relay is limiting connections from your network. Wait a minute and connect again.";
pub const RELAY_REFUSED_MESSAGE: &str = "The relay didn't accept the connection request. Check the ID, and update OpenDesk if it keeps happening.";

pub fn connect_refusal_message(code: &str) -> Option<&'static str> {
    use protocol::control::*;
    match code {
        ERROR_PEER_UNAVAILABLE => Some(PEER_UNAVAILABLE_MESSAGE),
        ERROR_CONNECT_RATE | ERROR_RELAY_BUSY => Some(RELAY_LIMITED_MESSAGE),
        ERROR_INVALID_CONNECT | ERROR_INVALID_CONTROL => Some(RELAY_REFUSED_MESSAGE),
        _ => None,
    }
}

/// The host closed after the key exchange but before confirming the key
/// (beta.5 hosts do this on a wrong password).
pub const KEY_NOT_CONFIRMED: &str = "connection closed during KEX confirmation";
pub const HOST_ENDED_MESSAGE: &str = "The host ended the session.";

pub mod reject_code {
    pub const DENIED: &str = "denied";
    pub const TWOFA_REQUIRED: &str = "2fa_required";
    pub const TWOFA_INVALID: &str = "2fa_invalid";
    pub const CAPTURE_UNAVAILABLE: &str = "capture_unavailable";
    pub const NO_ANSWER: &str = "no_answer";
    pub const NO_DISPLAY: &str = "no_display";
}

/// What the viewer shows when the host has no screen to share.
pub const NO_DISPLAY_MESSAGE: &str = "That computer has no screen connected. Plug in a monitor or an HDMI dummy plug, or turn on \"Let viewers add virtual displays\" in OpenDesk there (on Windows, also click \"Install virtual display driver\").";

const REJECTION_PREFIX: &str = "session rejected by host: ";

pub fn rejection_error(code: &str) -> String {
    format!("{REJECTION_PREFIX}{code}")
}

pub fn rejection_code(error: &str) -> Option<&str> {
    error.strip_prefix(REJECTION_PREFIX)
}

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
        DisconnectReason::Transient
    }
}

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

pub fn stop_message(error: &str, reason: DisconnectReason, session_started: bool) -> String {
    match reason {
        DisconnectReason::AuthFailure | DisconnectReason::KeyNotConfirmed => auth_failure_message(error, session_started),
        _ => error.to_owned(),
    }
}

/// What the viewer does after an attempt fails, as in the desktop viewer's
/// connect loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextStep {
    /// Wait `delay`, then connect again. `notice` says why.
    Retry { delay: Duration, notice: String },
    /// Give up and show `message`.
    Stop { message: String },
}

/// `attempt` counts retries in the current outage and `outage` is how long
/// it has lasted; both restart once an attempt streams video.
pub fn after_failure(message: &str, session_started: bool, attempt: u32, outage: Duration) -> NextStep {
    let reason = classify_viewer_error(message);
    if !should_retry_viewer(reason, session_started) {
        return NextStep::Stop { message: stop_message(message, reason, session_started) };
    }
    match viewer_retry_delay(attempt, outage) {
        Some(delay) => NextStep::Retry {
            delay,
            notice: format!("viewer session dropped ({message}); reconnecting (attempt {})", attempt + 1),
        },
        None => NextStep::Stop {
            message: format!("{message}; gave up reconnecting after {}s", VIEWER_RECONNECT_WINDOW.as_secs()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_password_stops_with_the_password_hint() {
        let NextStep::Stop { message } = after_failure("key confirmation failed", false, 0, Duration::ZERO) else { panic!() };
        assert!(message.contains("check the password"), "{message}");
    }

    #[test]
    fn a_hang_up_before_key_confirmation_stops_a_first_connect() {
        let NextStep::Stop { message } = after_failure(KEY_NOT_CONFIRMED, false, 0, Duration::ZERO) else { panic!() };
        assert!(message.contains("password is probably wrong"), "{message}");
        assert!(matches!(after_failure(KEY_NOT_CONFIRMED, true, 0, Duration::ZERO), NextStep::Retry { .. }));
    }

    #[test]
    fn unavailable_peer_stops_a_first_connect_but_retries_a_reconnect() {
        assert_eq!(
            after_failure(PEER_UNAVAILABLE_MESSAGE, false, 0, Duration::ZERO),
            NextStep::Stop { message: PEER_UNAVAILABLE_MESSAGE.into() }
        );
        assert!(matches!(after_failure(PEER_UNAVAILABLE_MESSAGE, true, 0, Duration::ZERO), NextStep::Retry { .. }));
    }

    #[test]
    fn host_bye_is_a_clean_stop() {
        assert_eq!(
            after_failure(HOST_ENDED_MESSAGE, true, 0, Duration::ZERO),
            NextStep::Stop { message: HOST_ENDED_MESSAGE.into() }
        );
    }

    #[test]
    fn transient_drops_retry_until_the_window_is_used_up() {
        let NextStep::Retry { delay, notice } = after_failure("connection closed during session", true, 2, Duration::from_secs(1)) else { panic!() };
        assert_eq!(delay, Duration::from_secs(2));
        assert!(notice.contains("attempt 3"), "{notice}");
        let NextStep::Stop { message } = after_failure("connection closed during session", true, 9, VIEWER_RECONNECT_WINDOW) else { panic!() };
        assert!(message.ends_with("gave up reconnecting after 60s"), "{message}");
    }

    #[test]
    fn a_used_2fa_code_on_reconnect_asks_for_a_fresh_one() {
        let error = rejection_error(reject_code::TWOFA_INVALID);
        let NextStep::Stop { message } = after_failure(&error, true, 0, Duration::ZERO) else { panic!() };
        assert!(message.starts_with("Reconnect needs a fresh 2FA code"), "{message}");
    }
}
