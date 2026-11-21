//! Opt-in recording of incoming sessions on this (host) computer, for
//! compliance. host-core tees the encoded stream into `.mkv` files; this
//! module picks the folder and names, audits each file, and keeps the folder
//! under the recordings size cap.

use super::*;
use host_core::recording::{RecordingEvent, RecordingEvents, RecordingTap};

/// Every host recording's file name starts with this, so the recordings
/// browser and the cap only ever touch OpenDesk's own files.
pub(crate) const HOST_RECORDING_PREFIX: &str = "opendesk-host-";

/// `recordings` under the app data folder, created on first use.
pub(crate) fn host_recordings_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let directory = app.path().app_data_dir().map_err(|error| error.to_string())?.join("recordings");
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let metadata = fs::symlink_metadata(&directory).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("host recordings path must be a regular directory".into());
    }
    fs::canonicalize(&directory).map_err(|error| error.to_string())
}

pub(crate) fn host_folder(app: &AppHandle) -> Result<RecordingFolder, String> {
    Ok(RecordingFolder { dir: host_recordings_dir(app)?, prefix: HOST_RECORDING_PREFIX, extension: ".mkv" })
}

/// `opendesk-host-<UTC date>_<UTC time>-<viewer ID>`. The ID comes from the
/// relay unauthenticated, so only its safe characters are kept.
pub(crate) fn base_name(peer_id: &str, unix_seconds: u64) -> String {
    let id: String = peer_id.chars().filter(|character| character.is_ascii_alphanumeric()).take(32).collect();
    let id = if id.is_empty() { "unknown".to_owned() } else { id };
    format!("{HOST_RECORDING_PREFIX}{}-UTC-{id}", host_core::recording::file_timestamp(unix_seconds))
}

/// A recording for the session with `peer_id`, when "Record incoming
/// sessions" is on. Recording problems never block the session; they are
/// audited and shown to the host user.
pub(crate) fn session_tap(app: &AppHandle, peer_id: &str) -> Option<RecordingTap> {
    if !app.state::<AppState>().lock().config.config.record_incoming_sessions {
        return None;
    }
    let audit = |event: AuditEvent| {
        let mut event = event;
        event.peer_id = Some(peer_id.to_owned());
        write_audit(app, &event);
    };
    let dir = match host_recordings_dir(app) {
        Ok(dir) => dir,
        Err(error) => {
            log::warn!("could not record this session: {error}");
            audit(AuditEvent::new(audit_kind::RECORDING_FAILED, None).with_detail(error.clone()));
            emit(app, "error", ErrorEvent { message: format!("Could not record this session: {error}") });
            return None;
        }
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
    let events_app = app.clone();
    let events_peer = peer_id.to_owned();
    let events: RecordingEvents = Arc::new(move |event| on_event(&events_app, &events_peer, event));
    match RecordingTap::start(dir, base_name(peer_id, now), events) {
        Ok(tap) => Some(tap),
        Err(error) => {
            log::warn!("could not record this session: {error}");
            audit(AuditEvent::new(audit_kind::RECORDING_FAILED, None).with_detail(error.to_string()));
            None
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Audits each file the recorder opens or closes; a closed file may push the
/// folder over the cap, so the oldest are pruned then.
fn on_event(app: &AppHandle, peer_id: &str, event: RecordingEvent) {
    let audited = match &event {
        RecordingEvent::Started { path } => {
            emit_activity(app, "session", format!("Recording this session to {}", file_name(path)));
            AuditEvent::new(audit_kind::RECORDING_STARTED, None).with_file(file_name(path), None).with_path(path.to_string_lossy())
        }
        RecordingEvent::Stopped { path, bytes } => {
            AuditEvent::new(audit_kind::RECORDING_STOPPED, None).with_file(file_name(path), Some(*bytes)).with_path(path.to_string_lossy())
        }
        RecordingEvent::Failed { path, error } => {
            log::warn!("session recording failed: {error}");
            emit(app, "error", ErrorEvent { message: format!("Session recording stopped: {error}") });
            let event = AuditEvent::new(audit_kind::RECORDING_FAILED, None).with_detail(error.clone());
            match path {
                Some(path) => event.with_file(file_name(path), None).with_path(path.to_string_lossy()),
                None => event,
            }
        }
    };
    let mut audited = audited;
    audited.peer_id = Some(peer_id.to_owned());
    write_audit(app, &audited);
    if matches!(event, RecordingEvent::Stopped { .. }) {
        let cap = app.state::<AppState>().lock().config.config.recordings_max_bytes;
        if let Err(error) = host_folder(app).and_then(|folder| folder.prune(cap)) {
            log::warn!("could not prune host recordings: {error}");
        }
    }
}

/// Applies from the next session on.
#[tauri::command]
pub(crate) fn set_record_incoming_sessions(state: tauri::State<'_, AppState>, enabled: bool) -> Result<(), String> {
    state.lock().config.set_record_incoming_sessions(enabled).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_carry_date_time_and_a_safe_viewer_id() {
        assert_eq!(base_name("123 456 789", 1_795_000_000), "opendesk-host-2026-11-18_11-06-40-UTC-123456789");
        assert_eq!(base_name("../../evil", 0), "opendesk-host-1970-01-01_00-00-00-UTC-evil");
        assert_eq!(base_name("", 0), "opendesk-host-1970-01-01_00-00-00-UTC-unknown");
        let folder = RecordingFolder { dir: PathBuf::new(), prefix: HOST_RECORDING_PREFIX, extension: ".mkv" };
        assert!(folder.holds(&host_core::recording::segment_name(&base_name("1", 0), 2)));
        assert!(!folder.holds("opendesk-session-x.webm"));
    }
}
