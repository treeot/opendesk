//! Session recordings, streamed to disk chunk by chunk.

use super::*;

/// Maximum size of one session recording on disk.
pub(crate) const MAX_RECORDING_SIZE: usize = 2 * 1024 * 1024 * 1024;

#[allow(dead_code)]
pub(crate) fn recording_path(suggested_name: &str) -> PathBuf {
    downloads_dir().join(sanitize_recording_name(suggested_name))
}

pub(crate) fn validate_recording_size_len(size: usize) -> Result<(), String> {
    if size > MAX_RECORDING_SIZE {
        return Err(format!(
            "recording exceeds the {} GiB maximum size",
            MAX_RECORDING_SIZE / (1024 * 1024 * 1024)
        ));
    }
    Ok(())
}

pub(crate) fn recording_name_with_suffix(name: &str, suffix: usize) -> String {
    if suffix == 1 {
        return name.to_owned();
    }
    let stem = name.strip_suffix(".webm").unwrap_or(name);
    format!("{stem} ({suffix}).webm")
}

pub(crate) fn next_recording_path(directory: &Path, suggested_name: &str) -> Result<PathBuf, String> {
    let sanitized_name = sanitize_recording_name(suggested_name);
    for suffix in 1.. {
        let path = directory.join(recording_name_with_suffix(&sanitized_name, suffix));
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!("refusing to write through symlink: {}", path.display()));
            }
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(path),
            Err(error) => return Err(error.to_string()),
        }
    }
    unreachable!("recording filename suffix range is unbounded")
}

pub(crate) fn sanitize_recording_name(suggested_name: &str) -> String {
    let name = Path::new(suggested_name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .replace('\\', "_");
    let sanitized: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('.');
    let sanitized = if sanitized.is_empty() { "recording" } else { sanitized };
    if sanitized.to_ascii_lowercase().ends_with(".webm") {
        sanitized.to_owned()
    } else {
        format!("{sanitized}.webm")
    }
}

/// A recording streamed to disk chunk by chunk, so long sessions never sit in
/// memory. The file is opened create-new (never follows an existing symlink
/// or overwrites a file) and capped at `MAX_RECORDING_SIZE`.
pub(crate) struct RecordingSink {
    pub(crate) file: fs::File,
    pub(crate) path: PathBuf,
    pub(crate) written: usize,
}

impl RecordingSink {
    pub(crate) fn create(directory: &Path, suggested_name: &str) -> Result<Self, String> {
        let mut path = next_recording_path(directory, suggested_name)?;
        let file = loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => break file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    // A competing writer won the name race; choose another name
                    // and retain create-new semantics so no file is clobbered.
                    path = next_recording_path(directory, suggested_name)?;
                }
                Err(error) => return Err(error.to_string()),
            }
        };
        if !file.metadata().map_err(|error| error.to_string())?.is_file() {
            return Err("recording target is not a regular file".into());
        }
        Ok(Self { file, path, written: 0 })
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) -> Result<(), String> {
        let total = self.written.checked_add(bytes.len()).ok_or("recording size overflow")?;
        validate_recording_size_len(total)?;
        io::Write::write_all(&mut self.file, bytes).map_err(|error| error.to_string())?;
        self.written = total;
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<PathBuf, String> {
        self.file.sync_all().map_err(|error| error.to_string())?;
        Ok(self.path)
    }
}

/// Recordings in progress, keyed by the id handed to the viewer UI.
#[derive(Default)]
pub(crate) struct Recordings {
    pub(crate) next_id: std::sync::atomic::AtomicU32,
    pub(crate) sinks: Mutex<HashMap<u32, RecordingSink>>,
}

impl Recordings {
    pub(crate) fn sinks(&self) -> std::sync::MutexGuard<'_, HashMap<u32, RecordingSink>> {
        self.sinks.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub(crate) fn recordings_dir() -> Result<PathBuf, String> {
    let directory = downloads_dir();
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let metadata = fs::symlink_metadata(&directory).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("recording Downloads path must be a regular directory".into());
    }
    fs::canonicalize(&directory).map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn start_recording(recordings: tauri::State<'_, Recordings>, suggested_name: String) -> Result<u32, String> {
    let sink = RecordingSink::create(&recordings_dir()?, &suggested_name)?;
    let id = recordings.next_id.fetch_add(1, Ordering::Relaxed);
    recordings.sinks().insert(id, sink);
    Ok(id)
}

/// `chunk` is base64, like the frame events, to keep large payloads off the
/// JSON number-array path.
#[tauri::command]
pub(crate) fn append_recording(recordings: tauri::State<'_, Recordings>, id: u32, chunk: String) -> Result<(), String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(chunk).map_err(|error| error.to_string())?;
    let mut sinks = recordings.sinks();
    let sink = sinks.get_mut(&id).ok_or("unknown recording")?;
    if let Err(error) = sink.append(&bytes) {
        // Keep what was written so far; stop accepting more for this id.
        if let Some(sink) = sinks.remove(&id) { let _ = sink.finish(); }
        return Err(error);
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn finish_recording(recordings: tauri::State<'_, Recordings>, state: tauri::State<'_, AppState>, id: u32) -> Result<String, String> {
    let sink = recordings.sinks().remove(&id).ok_or("unknown recording")?;
    let path = sink.finish()?.to_string_lossy().into_owned();
    // Keep the recordings folder under the user's size cap (oldest first).
    let cap = state.lock().config.config.recordings_max_bytes;
    if let Err(error) = prune_recordings(cap) { log::warn!("could not prune recordings: {error}"); }
    Ok(path)
}

#[tauri::command]
pub(crate) fn set_recordings_max_bytes(state: tauri::State<'_, AppState>, bytes: u64) -> Result<(), String> {
    state.lock().config.set_recordings_max_bytes(bytes).map_err(|error| error.to_string())?;
    prune_recordings(bytes)
}

/// The prefix every session recording's filename carries, so the list and the
/// delete command only ever touch OpenDesk's own files in the Downloads folder.
pub(crate) const RECORDING_PREFIX: &str = "opendesk-session-";

/// One finished recording on disk, newest first in [`list_recordings`].
#[derive(serde::Serialize)]
pub(crate) struct RecordingEntry {
    pub name: String,
    pub path: String,
    pub size: u64,
    /// Last-modified time in milliseconds since the Unix epoch.
    pub modified: u64,
}

/// True for a regular `.webm` file written by OpenDesk (not a directory, not a
/// symlink pointing elsewhere).
fn is_recording(name: &str) -> bool {
    name.starts_with(RECORDING_PREFIX) && name.ends_with(".webm")
}

#[tauri::command]
pub(crate) fn list_recordings() -> Result<Vec<RecordingEntry>, String> {
    let dir = recordings_dir()?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|error| error.to_string())? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_recording(&name) { continue; }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() { continue; }
        let modified = meta.modified().ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        entries.push(RecordingEntry { name, path: entry.path().to_string_lossy().into_owned(), size: meta.len(), modified });
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.modified));
    Ok(entries)
}

/// Delete the oldest recordings until the folder holds at most `max_bytes`,
/// always keeping the most recent one. 0 disables the cap. Best effort: a file
/// that can't be removed stops pruning rather than erroring the caller.
pub(crate) fn prune_recordings(max_bytes: u64) -> Result<(), String> {
    if max_bytes == 0 {
        return Ok(());
    }
    let dir = recordings_dir()?;
    let entries = list_recordings()?; // newest first
    let sizes: Vec<u64> = entries.iter().map(|entry| entry.size).collect();
    let to_delete = prune_count(&sizes, max_bytes);
    for oldest in entries.iter().rev().take(to_delete) {
        let target = PathBuf::from(&oldest.path);
        let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !is_recording(&name) { continue; }
        let Ok(meta) = fs::symlink_metadata(&target) else { continue };
        if meta.file_type().is_symlink() || !meta.is_file() { continue; }
        let Ok(canonical) = fs::canonicalize(&target) else { continue };
        if canonical.parent() != Some(dir.as_path()) { continue; }
        let _ = fs::remove_file(&canonical);
    }
    Ok(())
}

/// How many of the oldest recordings to delete to get the total at or under
/// `max_bytes`, always keeping at least the newest one. `sizes` is newest-first.
fn prune_count(sizes: &[u64], max_bytes: u64) -> usize {
    if max_bytes == 0 || sizes.len() <= 1 {
        return 0;
    }
    let mut total: u64 = sizes.iter().sum();
    let mut deleted = 0;
    for &size in sizes.iter().rev() {
        if total <= max_bytes || sizes.len() - deleted <= 1 {
            break;
        }
        total = total.saturating_sub(size);
        deleted += 1;
    }
    deleted
}

#[tauri::command]
pub(crate) fn delete_recording(path: String) -> Result<(), String> {
    let dir = recordings_dir()?;
    let target = PathBuf::from(&path);
    // Only a real recording directly inside the recordings folder may be
    // removed, so a crafted path can never delete anything else.
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !is_recording(&name) { return Err("not an OpenDesk recording".into()); }
    let meta = fs::symlink_metadata(&target).map_err(|error| error.to_string())?;
    if meta.file_type().is_symlink() || !meta.is_file() { return Err("not a recording file".into()); }
    let canonical = fs::canonicalize(&target).map_err(|error| error.to_string())?;
    if canonical.parent() != Some(dir.as_path()) { return Err("recording is outside the recordings folder".into()); }
    fs::remove_file(&canonical).map_err(|error| error.to_string())
}

#[cfg(test)]
mod recording_list_tests {
    use super::{is_recording, prune_count};

    #[test]
    fn only_opendesk_webm_files_are_recordings() {
        assert!(is_recording("opendesk-session-2026-10-24.webm"));
        assert!(!is_recording("opendesk-session-2026-10-24.mp4"));
        assert!(!is_recording("holiday.webm"));
        assert!(!is_recording("opendesk-session-"));
        assert!(!is_recording("evil-opendesk-session-x.webm"));
    }

    #[test]
    fn prune_count_removes_oldest_until_under_cap() {
        // newest-first sizes; oldest are at the end.
        assert_eq!(prune_count(&[30, 30, 30], 100), 0, "already under cap");
        assert_eq!(prune_count(&[30, 30, 30], 70), 1, "drop one oldest to reach 60");
        assert_eq!(prune_count(&[30, 30, 30], 50), 2, "drop two oldest to reach 30");
        assert_eq!(prune_count(&[30, 30, 30], 0), 0, "zero means no cap");
    }

    #[test]
    fn prune_count_always_keeps_the_newest() {
        // Even when the newest alone exceeds the cap, it is never deleted.
        assert_eq!(prune_count(&[500, 10, 10], 100), 2);
        assert_eq!(prune_count(&[500], 100), 0);
    }
}
