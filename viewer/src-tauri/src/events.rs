//! Event payloads sent to the UI, and `emit` (which also logs lifecycle events).

use super::*;

#[derive(Serialize)]
pub(crate) struct Identity {
    pub(crate) id: String,
    pub(crate) password: String,
}
#[derive(Serialize, Clone)]
pub(crate) struct PasswordResponse {
    pub(crate) password: String,
}
#[derive(Serialize)]
pub(crate) struct AppConfig {
    pub(crate) relay_url: String,
    pub(crate) relay_urls: Vec<String>,
    pub(crate) recent_ids: Vec<String>,
    pub(crate) clipboard_sync: bool,
    pub(crate) allow_virtual_displays: bool,
}
#[derive(Serialize)]
pub(crate) struct UnattendedStatus {
    pub(crate) enabled: bool,
    pub(crate) has_password: bool,
    pub(crate) twofa_enabled: bool,
    /// Turned on and every secret it needs could be read from the keyring.
    pub(crate) ready: bool,
    /// Why unattended access is on but not working, for the user.
    pub(crate) problem: Option<String>,
}
#[derive(Serialize)]
pub(crate) struct PermissionStatus {
    pub(crate) screen_recording: bool,
    pub(crate) accessibility: bool,
}
#[derive(Serialize, Clone)]
pub(crate) struct StatusEvent {
    pub(crate) state: &'static str,
}
#[derive(Serialize, Clone)]
pub(crate) struct IncomingRequest {
    pub(crate) peer_id: String,
    pub(crate) fingerprint: String,
    /// The claimed peer ID is on the allow list. Informational only: the ID
    /// is not authenticated, so known devices still need approval.
    pub(crate) known: bool,
}
#[derive(Serialize, Clone)]
pub(crate) struct SessionStarted {
    pub(crate) role: &'static str,
    pub(crate) remote_id: String,
}
#[derive(Serialize, Clone)]
pub(crate) struct SessionEnded {
    pub(crate) reason: String,
}
/// A pending 2FA enrollment: shown to the user until they confirm a code.
#[derive(Serialize)]
pub(crate) struct TotpEnrollment {
    pub(crate) uri: String,
    pub(crate) secret: String,
    /// QR modules for `uri`, row-major, `qr_width` x `qr_width`; true = dark.
    pub(crate) qr_width: usize,
    pub(crate) qr_modules: Vec<bool>,
}
/// The device ID changed because a relay said it belonged to another device.
#[derive(Serialize, Clone)]
pub(crate) struct IdentityChangedEvent {
    pub(crate) id: String,
}
/// A relay that refuses this device's ID. Sent as the whole current list in
/// `identity_conflicts` whenever it changes.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct IdentityConflict {
    pub(crate) relay: String,
    /// "taken" (another device's ID there) or "in_use" (a copy of this one).
    pub(crate) kind: &'static str,
    pub(crate) message: String,
}
#[derive(Serialize, Clone)]
pub(crate) struct IdentityConflictsEvent {
    pub(crate) conflicts: Vec<IdentityConflict>,
}
#[derive(Serialize, Clone)]
pub(crate) struct ErrorEvent {
    pub(crate) message: String,
}
/// The host wants a 2FA code before it will let the viewer in. `reason` is
/// "required" (none was sent) or "invalid" (the code was wrong, expired, or
/// already used). The UI shows a popup for the code instead of an error.
#[derive(Serialize, Clone)]
pub(crate) struct TwofaPromptEvent {
    pub(crate) reason: &'static str,
}
/// Base64-encode bytes for an IPC event payload (avoids JSON `number[]`).
/// Video skips events entirely and streams raw bytes (see `VideoSink`).
pub(crate) fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
#[derive(Serialize, Clone)]
pub(crate) struct MonitorListEvent { pub(crate) monitors: Vec<MonitorInfo> }
/// The host lets the viewer add up to `max` virtual displays.
#[derive(Serialize, Clone)]
pub(crate) struct VirtualDisplaySupportEvent { pub(crate) max: u32 }
#[derive(Serialize, Clone)]
pub(crate) struct VirtualDisplayAddedEvent { pub(crate) id: u32 }
#[derive(Serialize, Clone)]
pub(crate) struct VirtualDisplayRefusedEvent { pub(crate) code: String }
/// The host can stream up to `max` extra monitors in their own windows.
#[derive(Serialize, Clone)]
pub(crate) struct MultiMonitorSupportEvent { pub(crate) max: u32 }
/// A per-monitor aux stream started or stopped.
#[derive(Serialize, Clone)]
pub(crate) struct MonitorStreamEvent { pub(crate) id: u32 }
/// The host refused an aux stream for monitor `id`.
#[derive(Serialize, Clone)]
pub(crate) struct MonitorStreamRefusedEvent { pub(crate) id: u32, pub(crate) code: String }
/// Privacy mode turned on or off on the host.
#[derive(Serialize, Clone)]
pub(crate) struct PrivacyModeEvent { pub(crate) on: bool }
/// The host's cursor image; `rgba` is base64 of straight-alpha RGBA.
#[derive(Serialize, Clone)]
pub(crate) struct CursorShapeEvent {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) hot_x: u16,
    pub(crate) hot_y: u16,
    pub(crate) rgba: String,
}
/// A remote directory listing for the file manager, or an error.
#[derive(Serialize, Clone)]
pub(crate) struct RemoteListingEvent {
    pub(crate) path: String,
    pub(crate) entries: Vec<protocol::browse::DirEntry>,
    pub(crate) error: Option<String>,
}
/// Something the host user should know about, shown as an info toast.
#[derive(Serialize, Clone)]
pub(crate) struct HostNoticeEvent { pub(crate) message: &'static str }
/// The viewer's media path ("direct" or "relay") and its latest measured
/// round trip. `rtt_ms` stays `None` until a real sample exists, and for
/// hosts too old to report one.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkStatsEvent { pub(crate) path: &'static str, pub(crate) rtt_ms: Option<u32> }

impl LinkStatsEvent {
    pub(crate) fn new(link: &host_core::transport::LinkStatus, now: Instant) -> Self {
        // Nearest ms, but a sub-millisecond LAN reads as 1 ms rather than 0.
        let rtt_ms = link.rtt(now).map(|rtt| u32::try_from((rtt.as_micros() + 500) / 1_000).unwrap_or(u32::MAX).max(1));
        Self { path: link.path.as_str(), rtt_ms }
    }
}
#[derive(Serialize, Clone)]
pub(crate) struct AudioFrameEvent { pub(crate) sample_rate: u32, pub(crate) channels: u8, pub(crate) samples: String }
#[derive(Serialize, Clone)]
#[allow(dead_code)]
pub(crate) struct FileOffer {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) from: String,
    /// Where the file will be saved, so the host sees a viewer-chosen upload
    /// directory before accepting (not just the default Downloads folder).
    pub(crate) dir: String,
}
#[derive(Serialize, Clone)]
pub(crate) struct FileProgress {
    pub(crate) id: u64,
    pub(crate) transferred: u64,
    pub(crate) total: u64,
    pub(crate) direction: &'static str,
}
#[derive(Serialize, Clone)]
pub(crate) struct FileDone {
    pub(crate) id: u64,
    /// Where a received file was saved; `None` for outgoing transfers.
    pub(crate) path: Option<String>,
}
#[derive(Serialize, Clone)]
pub(crate) struct FileError {
    pub(crate) id: u64,
    pub(crate) message: String,
}
/// An inbound chat message from the peer. The UI echoes locally sent messages
/// itself, so only the peer's messages ever reach it as this event.
#[derive(Serialize, Clone)]
pub(crate) struct ChatMessage {
    pub(crate) text: String,
}

/// The host-enforced permissions for the connected viewer. Sent whenever they
/// change so the control panel stays in sync. All default to enabled.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ViewerPermissionsEvent {
    pub(crate) control: bool,
    pub(crate) files: bool,
    pub(crate) clipboard: bool,
    pub(crate) audio: bool,
}

/// One line in the host's session activity log.
#[derive(Serialize, Clone)]
pub(crate) struct ActivityEvent {
    /// Unix milliseconds.
    pub(crate) at: u64,
    pub(crate) kind: String,
    pub(crate) detail: String,
}

/// Emit an `activity` line stamped with the current time.
pub(crate) fn emit_activity(app: &AppHandle, kind: &str, detail: impl Into<String>) {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0);
    emit(app, "activity", ActivityEvent { at, kind: kind.to_owned(), detail: detail.into() });
}

/// Lifecycle events that also go to the log file, so a bug report can include
/// what happened. Media and progress events are too frequent to log.
pub(crate) const LOGGED_EVENTS: &[&str] = &["status_changed", VIEWER_STATUS, "session_started", "session_ended", "incoming_request", "error", "twofa_prompt", "file_error", "identity_changed", "identity_conflicts", "chat_message", "activity"];

/// The outgoing (viewer) session's own status. `status_changed` is only ever
/// the host's presence, so a host hiccup never shows up in a viewer session.
pub(crate) const VIEWER_STATUS: &str = "viewer_status";

/// Last hosting presence sent to the UI, read by the tray.
static HOST_STATUS: std::sync::Mutex<&str> = std::sync::Mutex::new("offline");

pub(crate) fn emit_host_status(app: &AppHandle, state: &'static str) {
    remember_host_status(state);
    crate::startup::sync_tray(app, state);
    emit(app, "status_changed", StatusEvent { state });
}

fn remember_host_status(state: &'static str) {
    *HOST_STATUS.lock().unwrap_or_else(|error| error.into_inner()) = state;
}

pub(crate) fn host_status() -> &'static str {
    *HOST_STATUS.lock().unwrap_or_else(|error| error.into_inner())
}

pub(crate) fn emit<T: Serialize + Clone>(app: &AppHandle, event: &str, payload: T) {
    if LOGGED_EVENTS.contains(&event) {
        let details = serde_json::to_string(&payload).unwrap_or_default();
        if event.ends_with("error") { log::warn!("{event} {details}"); } else { log::info!("{event} {details}"); }
    }
    let _ = app.emit(event, payload);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_the_last_host_status() {
        remember_host_status("online");
        assert_eq!(host_status(), "online");
        remember_host_status("offline");
        assert_eq!(host_status(), "offline");
    }

    #[test]
    fn link_stats_report_the_path_and_only_real_rtt_samples() {
        use host_core::transport::{LinkStatus, MediaPath, RttMeter};
        let meter = Arc::new(RttMeter::default());
        let link = LinkStatus::new(MediaPath::Relay, Arc::clone(&meter));
        let now = Instant::now();
        assert_eq!(LinkStatsEvent::new(&link, now), LinkStatsEvent { path: "relay", rtt_ms: None });
        meter.record(Duration::from_micros(14_200), now);
        assert_eq!(LinkStatsEvent::new(&link, now).rtt_ms, Some(14));
        meter.record(Duration::from_micros(300), now);
        assert_eq!(LinkStatsEvent::new(&link, now).rtt_ms, Some(1));
        let direct = LinkStatus::new(MediaPath::Direct, Arc::new(RttMeter::default()));
        assert_eq!(serde_json::to_string(&LinkStatsEvent::new(&direct, now)).unwrap(), r#"{"path":"direct","rtt_ms":null}"#);
    }
}
