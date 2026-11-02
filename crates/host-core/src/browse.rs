//! Listing the host's directories for the remote file manager. The viewer can
//! browse anywhere the host user can, so access is governed by the session's
//! file permission and by the OS, not by a sandbox. This module only reads
//! directory metadata; file contents move over the existing file-transfer path.

use protocol::browse::DirEntry;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

fn component(name: &str) -> Result<&std::ffi::OsStr, String> {
    let path = std::path::Path::new(name);
    if name.is_empty()
        || path.components().count() != 1
        || !matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err("name must be a single path component".into());
    }
    Ok(path.as_os_str())
}

/// Create a child directory without allowing the supplied name to escape `dir`.
pub fn make_dir(dir: &str, name: &str) -> Result<(), String> {
    let path = PathBuf::from(dir).join(component(name)?);
    std::fs::create_dir(path).map_err(|error| error.to_string())
}

/// Delete one file or directory without following a directory symlink.
pub fn delete(path: &str) -> Result<(), String> {
    let path = PathBuf::from(path);
    let metadata = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        std::fs::remove_file(path)
    } else if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        return Err("unsupported file type".into());
    }
    .map_err(|error| error.to_string())
}

/// Rename a path to a sibling with a single-component name.
pub fn rename(path: &str, name: &str) -> Result<(), String> {
    let path = PathBuf::from(path);
    let parent = path
        .parent()
        .ok_or_else(|| "path has no parent directory".to_string())?
        .to_path_buf();
    std::fs::rename(path, parent.join(component(name)?)).map_err(|error| error.to_string())
}

/// The host's home directory, used when the viewer asks for the empty path.
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// List `path` (empty means the host's home directory). Returns the absolute
/// path that was read and its entries, directories first then by name, or an
/// error string describing why it could not be read.
pub fn list_dir(path: &str) -> Result<(String, Vec<DirEntry>), String> {
    let dir = if path.is_empty() {
        home_dir().ok_or_else(|| "this computer has no home directory".to_string())?
    } else {
        PathBuf::from(path)
    };
    let shown = std::fs::canonicalize(&dir).unwrap_or(dir);
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&shown).map_err(|error| error.to_string())? {
        let Ok(entry) = entry else { continue };
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        let size = meta
            .as_ref()
            .map_or(0, |m| if is_dir { 0 } else { m.len() });
        let modified = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_secs());
        entries.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir,
            size,
            modified,
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok((shown.to_string_lossy().into_owned(), entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_entries_with_directories_first() {
        let dir = std::env::temp_dir().join(format!("opendesk-browse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();

        let (_shown, entries) = list_dir(&dir.to_string_lossy()).unwrap();
        assert_eq!(entries[0].name, "sub");
        assert!(entries[0].is_dir);
        let file = entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert!(!file.is_dir);
        assert_eq!(file.size, 5);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_path_is_an_error_not_a_panic() {
        assert!(list_dir("/no/such/opendesk/path/ever").is_err());
    }

    #[test]
    fn the_empty_path_lists_the_home_directory() {
        // Home exists on the test machines; just check it does not error.
        assert!(list_dir("").is_ok());
    }
}
