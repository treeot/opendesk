//! Session recordings, streamed to disk chunk by chunk.

use super::*;

/// Maximum size of one session recording on disk.
pub(crate) const MAX_RECORDING_SIZE: usize = 2 * 1024 * 1024 * 1024;

/// The folder session recordings are written to: an `OpenDesk` subfolder of the
/// user's videos folder, keeping them tidy and out of the TCC-guarded Downloads.
pub(crate) fn recordings_base_dir() -> PathBuf {
    videos_dir().join("OpenDesk")
}

#[allow(dead_code)]
pub(crate) fn recording_path(suggested_name: &str) -> PathBuf {
    recordings_base_dir().join(sanitize_recording_name(suggested_name))
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
    let directory = recordings_base_dir();
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let metadata = fs::symlink_metadata(&directory).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("recordings path must be a regular directory".into());
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
pub(crate) fn set_recordings_max_bytes(app: AppHandle, state: tauri::State<'_, AppState>, bytes: u64) -> Result<(), String> {
    state.lock().config.set_recordings_max_bytes(bytes).map_err(|error| error.to_string())?;
    prune_recordings(bytes)?;
    // The cap applies to each folder: the viewer's and the host's own.
    crate::host_recording::host_folder(&app)?.prune(bytes)
}

/// The prefix every session recording's filename carries, so the list and the
/// delete command only ever touch OpenDesk's own files in the recordings folder.
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

/// A folder of OpenDesk recordings: the viewer's own (`.webm`) or this
/// host's recordings of incoming sessions (`.mkv`). Listing, pruning and
/// deleting only ever touch files with its prefix and extension that are
/// regular files directly inside it.
pub(crate) struct RecordingFolder {
    pub(crate) dir: PathBuf,
    pub(crate) prefix: &'static str,
    pub(crate) extension: &'static str,
}

impl RecordingFolder {
    pub(crate) fn viewer() -> Result<Self, String> {
        Ok(Self { dir: recordings_dir()?, prefix: RECORDING_PREFIX, extension: ".webm" })
    }

    pub(crate) fn holds(&self, name: &str) -> bool {
        name.starts_with(self.prefix) && name.ends_with(self.extension)
    }

    pub(crate) fn list(&self) -> Result<Vec<RecordingEntry>, String> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(|error| error.to_string())? {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if !self.holds(&name) { continue; }
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

    /// Delete the oldest recordings until the folder holds at most
    /// `max_bytes`, always keeping the most recent one. 0 disables the cap.
    /// Best effort: a file that can't be removed is skipped.
    pub(crate) fn prune(&self, max_bytes: u64) -> Result<(), String> {
        if max_bytes == 0 {
            return Ok(());
        }
        let entries = self.list()?; // newest first
        let sizes: Vec<u64> = entries.iter().map(|entry| entry.size).collect();
        let to_delete = prune_count(&sizes, max_bytes);
        for oldest in entries.iter().rev().take(to_delete) {
            let _ = self.delete(&oldest.path);
        }
        Ok(())
    }

    /// Only a real recording directly inside this folder may be removed, so
    /// a crafted path can never delete anything else.
    pub(crate) fn delete(&self, path: &str) -> Result<(), String> {
        let target = PathBuf::from(path);
        let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !self.holds(&name) { return Err("not an OpenDesk recording".into()); }
        let meta = fs::symlink_metadata(&target).map_err(|error| error.to_string())?;
        if meta.file_type().is_symlink() || !meta.is_file() { return Err("not a recording file".into()); }
        let canonical = fs::canonicalize(&target).map_err(|error| error.to_string())?;
        if canonical.parent() != Some(self.dir.as_path()) { return Err("recording is outside the recordings folder".into()); }
        fs::remove_file(&canonical).map_err(|error| error.to_string())
    }
}

/// `source` is `"host"` for this computer's recordings of incoming
/// sessions; anything else lists the viewer's own recordings.
#[tauri::command]
pub(crate) fn list_recordings(app: AppHandle, source: Option<String>) -> Result<Vec<RecordingEntry>, String> {
    if source.as_deref() == Some("host") {
        crate::host_recording::host_folder(&app)?.list()
    } else {
        RecordingFolder::viewer()?.list()
    }
}

/// Keep the viewer's recordings under `max_bytes`; see [`RecordingFolder::prune`].
pub(crate) fn prune_recordings(max_bytes: u64) -> Result<(), String> {
    RecordingFolder::viewer()?.prune(max_bytes)
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
pub(crate) fn delete_recording(app: AppHandle, path: String) -> Result<(), String> {
    let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if name.starts_with(crate::host_recording::HOST_RECORDING_PREFIX) {
        crate::host_recording::host_folder(&app)?.delete(&path)
    } else {
        RecordingFolder::viewer()?.delete(&path)
    }
}

#[cfg(test)]
mod recording_list_tests {
    use super::{prune_count, RecordingFolder, RECORDING_PREFIX};

    #[test]
    fn only_opendesk_webm_files_are_recordings() {
        let folder = RecordingFolder { dir: std::path::PathBuf::new(), prefix: RECORDING_PREFIX, extension: ".webm" };
        let is_recording = |name: &str| folder.holds(name);
        assert!(is_recording("opendesk-session-2026-10-24.webm"));
        assert!(!is_recording("opendesk-session-2026-10-24.mp4"));
        assert!(!is_recording("holiday.webm"));
        assert!(!is_recording("opendesk-session-"));
        assert!(!is_recording("evil-opendesk-session-x.webm"));
    }

    #[test]
    fn prune_and_delete_stay_inside_the_folder() {
        let dir = std::env::temp_dir().join(format!("opendesk-recording-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let folder = RecordingFolder { dir: dir.clone(), prefix: "opendesk-host-", extension: ".mkv" };
        for (name, size) in [("opendesk-host-a.mkv", 30), ("opendesk-host-b.mkv", 30), ("other.mkv", 500)] {
            std::fs::write(dir.join(name), vec![0u8; size]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(folder.list().unwrap().len(), 2);
        folder.prune(40).unwrap();
        assert!(!dir.join("opendesk-host-a.mkv").exists(), "the oldest goes first");
        assert!(dir.join("opendesk-host-b.mkv").exists());
        assert!(dir.join("other.mkv").exists(), "never touches foreign files");
        assert!(folder.delete(&dir.join("other.mkv").to_string_lossy()).is_err());
        let _ = std::fs::remove_dir_all(dir);
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
