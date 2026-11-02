//! Durable structured audit events for host sessions.
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

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
        }
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
