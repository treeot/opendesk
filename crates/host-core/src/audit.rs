//! Durable structured audit events for host sessions.
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

/// Event names written to the log. Readers must tolerate names they do not
/// know, since newer hosts may add more.
pub mod kind {
    pub const SESSION_START: &str = "session_start";
    pub const SESSION_END: &str = "session_end";
    /// The viewer's password matched none the host accepts.
    pub const AUTH_FAILED: &str = "auth_failed";
    /// The password was right but the 2FA code was missing or wrong.
    pub const TWOFA_FAILED: &str = "twofa_failed";
    /// The host user declined the request, or nobody answered it.
    pub const CONNECTION_DENIED: &str = "connection_denied";
    /// The access rules turned the viewer away without asking anyone: its ID
    /// is not on the list, or it came outside the allowed hours.
    pub const CONNECTION_REFUSED: &str = "connection_refused";
    /// The host ended a session nobody had used for its idle timeout.
    pub const IDLE_DISCONNECT: &str = "idle_disconnect";
    /// A file the viewer offered to this computer.
    pub const FILE_UPLOAD_ACCEPTED: &str = "file_upload_accepted";
    pub const FILE_UPLOAD_REJECTED: &str = "file_upload_rejected";
    pub const FILE_UPLOAD_COMPLETED: &str = "file_upload_completed";
    pub const FILE_UPLOAD_FAILED: &str = "file_upload_failed";
    /// A file sent from this computer to the viewer, pulled or pushed.
    pub const FILE_DOWNLOAD_REJECTED: &str = "file_download_rejected";
    pub const FILE_DOWNLOAD_COMPLETED: &str = "file_download_completed";
    pub const FILE_DOWNLOAD_FAILED: &str = "file_download_failed";
    /// Remote file manager changes on this computer.
    pub const FILE_MKDIR: &str = "file_mkdir";
    pub const FILE_DELETE: &str = "file_delete";
    pub const FILE_RENAME: &str = "file_rename";
    pub const SECURE_ATTENTION: &str = "secure_attention";
    pub const RESTART_REQUEST: &str = "restart_request";
    pub const PRIVACY_MODE: &str = "privacy_mode";
    /// The host user allowed or blocked a viewer permission.
    pub const PERMISSION_CHANGED: &str = "permission_changed";
}

/// Where a session reports its audit events. The caller fills in what the
/// session cannot know (the peer ID) and writes them out.
pub type AuditSink = Arc<dyn Fn(AuditEvent) + Send + Sync>;

pub const MAX_BYTES: u64 = 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEvent {
    pub at: u64,
    pub event: String,
    pub peer_id: Option<String>,
    pub address: Option<String>,
    pub detail: Option<String>,
    pub duration_seconds: Option<u64>,
    pub filename: Option<String>,
    pub size_bytes: Option<u64>,
    /// A folder or file on this computer: an upload's destination folder, a
    /// sent file, or the target of a file manager change.
    #[serde(default)]
    pub path: Option<String>,
}
impl AuditEvent {
    pub fn new(event: impl Into<String>, peer_id: Option<String>) -> Self {
        Self {
            at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            event: event.into(),
            peer_id,
            address: None,
            detail: None,
            duration_seconds: None,
            filename: None,
            size_bytes: None,
            path: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_file(mut self, filename: impl Into<String>, size_bytes: Option<u64>) -> Self {
        self.filename = Some(filename.into());
        self.size_bytes = size_bytes;
        self
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

/// Hand `event` to `sink`, if there is one.
pub fn report(sink: Option<&AuditSink>, event: AuditEvent) {
    if let Some(sink) = sink {
        sink(event);
    }
}
pub fn append(path: impl AsRef<Path>, event: &AuditEvent) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path
        .metadata()
        .map(|m| m.len() >= MAX_BYTES)
        .unwrap_or(false)
    {
        let old = PathBuf::from(format!("{}.1", path.display()));
        let _ = fs::remove_file(&old);
        fs::rename(path, old)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, event).map_err(io::Error::other)?;
    file.write_all(b"\n")
}
pub fn recent(path: impl AsRef<Path>, limit: usize) -> io::Result<Vec<AuditEvent>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut entries = Vec::new();
    let lines: Vec<_> = io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .collect();
    for line in lines.into_iter().rev().take(limit) {
        if let Ok(event) = serde_json::from_str(&line) {
            entries.push(event);
        }
    }
    entries.reverse();
    Ok(entries)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_round_trips_json() {
        let e = AuditEvent::new("session_start", Some("peer".into()));
        assert_eq!(
            serde_json::from_str::<AuditEvent>(&serde_json::to_string(&e).unwrap()).unwrap(),
            e
        );
    }
    #[test]
    fn file_event_serializes_its_fields() {
        let e = AuditEvent::new(kind::FILE_UPLOAD_COMPLETED, Some("123456789".into()))
            .with_file("report.pdf", Some(2048))
            .with_path("/home/me/Downloads");
        let json: serde_json::Value = serde_json::to_value(&e).unwrap();
        assert_eq!(json["event"], "file_upload_completed");
        assert_eq!(json["filename"], "report.pdf");
        assert_eq!(json["size_bytes"], 2048);
        assert_eq!(json["path"], "/home/me/Downloads");
        assert_eq!(serde_json::from_value::<AuditEvent>(json).unwrap(), e);
    }
    #[test]
    fn security_events_serialize_with_detail() {
        for (name, detail) in [
            (kind::AUTH_FAILED, "wrong password"),
            (kind::TWOFA_FAILED, "invalid code"),
            (kind::SECURE_ATTENTION, "sent"),
            (kind::RESTART_REQUEST, "restarting"),
            (kind::PRIVACY_MODE, "on"),
            (kind::PERMISSION_CHANGED, "files blocked"),
        ] {
            let e = AuditEvent::new(name, None).with_detail(detail);
            let json: serde_json::Value = serde_json::to_value(&e).unwrap();
            assert_eq!(json["event"], name);
            assert_eq!(json["detail"], detail);
            assert!(json["path"].is_null());
        }
    }
    #[test]
    fn lines_written_before_path_existed_still_parse() {
        let old = r#"{"at":1,"event":"session_start","peer_id":"p","address":null,"detail":null,"duration_seconds":null,"filename":null,"size_bytes":null}"#;
        let e: AuditEvent = serde_json::from_str(old).unwrap();
        assert_eq!(e.event, "session_start");
        assert_eq!(e.path, None);
    }
    #[test]
    fn report_reaches_the_sink() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: AuditSink = {
            let seen = Arc::clone(&seen);
            Arc::new(move |event: AuditEvent| seen.lock().unwrap().push(event.event))
        };
        report(Some(&sink), AuditEvent::new(kind::PRIVACY_MODE, None));
        report(None, AuditEvent::new(kind::PRIVACY_MODE, None));
        assert_eq!(*seen.lock().unwrap(), vec![kind::PRIVACY_MODE.to_owned()]);
    }
    #[test]
    fn append_rotates_at_cap() {
        let d = std::env::temp_dir().join(format!("opendesk-audit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let p = d.join("audit.jsonl");
        fs::write(&p, vec![b'x'; MAX_BYTES as usize]).unwrap();
        append(&p, &AuditEvent::new("test", None)).unwrap();
        assert_eq!(
            fs::metadata(p.with_file_name("audit.jsonl.1"))
                .unwrap()
                .len(),
            MAX_BYTES
        );
        assert_eq!(recent(&p, 10).unwrap().len(), 1);
        let _ = fs::remove_dir_all(d);
    }
}
