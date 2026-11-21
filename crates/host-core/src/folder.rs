//! Folder transfer built on the per-file transfer in [`crate::filexfer`].
//!
//! A folder travels as an envelope around ordinary files: one `FolderOffer`
//! (a single consent prompt for the whole tree), a `FolderDir` per directory,
//! then each file as a `FolderFile` offer that is hashed, chunked and
//! acknowledged exactly like a single file, and a closing `FolderDone`.
//!
//! The receiver treats every path from the peer as hostile: each component
//! must pass [`sanitize_filename`], nothing may be absolute, contain `..` or a
//! drive prefix, or go deeper than [`MAX_FOLDER_DEPTH`], and every directory
//! it walks through must be a real directory it created under a fresh root,
//! never a symlink. The sender skips symlinks and anything the receiver would
//! refuse.

use crate::filexfer::{
    await_ack, gate_message, next_reply, revoke, sanitize_filename, send_offered, ConsentDecision,
    FileError, FileReceiver, FileSink, FileSource, FlowTimeouts, Reply, TransferGate,
};
use protocol::filexfer::FileMsg;
use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

/// Most files one folder may hold.
pub const MAX_FOLDER_FILES: u32 = 10_000;
/// Most directories one folder may hold.
pub const MAX_FOLDER_DIRS: u32 = 10_000;
/// Most path components below the folder itself ("a/b/c.txt" is 3).
pub const MAX_FOLDER_DEPTH: usize = 32;
/// Longest relative path accepted from the peer, in bytes.
pub const MAX_RELATIVE_PATH: usize = 4096;

/// The receiver confirms directories in batches of this many, with an `Ack`
/// on the folder id carrying the count so far.
const DIR_ACK_EVERY: u64 = 32;

/// What the sender is told when the receiver accepts no folders at all.
const UNSUPPORTED_MESSAGE: &str =
    "The other computer cannot receive folders. Update OpenDesk there and try again.";

/// How long a folder sender waits for each step.
#[derive(Debug, Clone, Copy)]
pub struct FolderTimeouts {
    /// For the receiver to acknowledge the FolderOffer before its user is
    /// asked. An older peer drops the unknown message, so this is short.
    pub seen: Duration,
    pub flow: FlowTimeouts,
}

impl Default for FolderTimeouts {
    fn default() -> Self {
        Self { seen: Duration::from_secs(15), flow: FlowTimeouts::default() }
    }
}

/// Progress through a folder, reported after every chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FolderProgress {
    pub files_done: u32,
    pub files_total: u32,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// One file the sender will send.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    pub path: PathBuf,
    /// '/'-separated path below the folder.
    pub relative: String,
    pub size: u64,
}

/// Everything a folder send will contain, read before the offer.
#[derive(Debug, Clone)]
pub struct FolderPlan {
    pub name: String,
    /// '/'-separated directories below the folder, parents first.
    pub dirs: Vec<String>,
    pub files: Vec<PlannedFile>,
    pub total: u64,
    /// Symlinks, special files and names the receiver would refuse.
    pub skipped: u32,
}

/// Split a peer-supplied '/'-separated relative path into safe components,
/// or `None` if any part could escape or disguise its destination.
pub fn sanitize_relative_path(path: &str) -> Option<Vec<String>> {
    if path.is_empty() || path.len() > MAX_RELATIVE_PATH {
        return None;
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > MAX_FOLDER_DEPTH {
        return None;
    }
    // sanitize_filename refuses "", ".", "..", '\\', ':' (drive prefixes and
    // streams), control and format characters and reserved Windows names, so
    // a leading '/', "C:" or ".." component fails here.
    parts.into_iter().map(sanitize_filename).collect()
}

/// Walk `root` without following symlinks and list what a send would hold.
/// Errors if the folder is over [`MAX_FOLDER_FILES`], [`MAX_FOLDER_DIRS`] or
/// [`MAX_FOLDER_DEPTH`].
pub fn scan_folder(root: &Path) -> Result<FolderPlan, FileError> {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(sanitize_filename)
        .ok_or(FileError::BadName)?;
    if !root.is_dir() {
        return Err(FileError::Io(io::Error::new(io::ErrorKind::InvalidInput, "not a folder")));
    }
    let mut plan = FolderPlan { name, dirs: Vec::new(), files: Vec::new(), total: 0, skipped: 0 };
    // (directory on disk, its relative path, its depth)
    let mut pending = vec![(root.to_path_buf(), String::new(), 0usize)];
    while let Some((dir, relative, depth)) = pending.pop() {
        let mut entries = std::fs::read_dir(&dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        let mut subdirs = Vec::new();
        for entry in entries {
            // DirEntry::file_type does not follow symlinks.
            let file_type = entry.file_type()?;
            let Some(child) = entry.file_name().to_str().and_then(sanitize_filename) else {
                plan.skipped += 1;
                continue;
            };
            if file_type.is_symlink() || !(file_type.is_dir() || file_type.is_file()) {
                plan.skipped += 1;
                continue;
            }
            if depth + 1 > MAX_FOLDER_DEPTH {
                return Err(FileError::Limit(format!(
                    "This folder is nested more than {MAX_FOLDER_DEPTH} levels deep."
                )));
            }
            let child_relative = if relative.is_empty() { child } else { format!("{relative}/{child}") };
            if file_type.is_dir() {
                if plan.dirs.len() as u32 >= MAX_FOLDER_DIRS {
                    return Err(FileError::Limit(format!(
                        "This folder holds more than {MAX_FOLDER_DIRS} folders."
                    )));
                }
                plan.dirs.push(child_relative.clone());
                subdirs.push((entry.path(), child_relative, depth + 1));
            } else {
                if plan.files.len() as u32 >= MAX_FOLDER_FILES {
                    return Err(FileError::Limit(format!(
                        "This folder holds more than {MAX_FOLDER_FILES} files."
                    )));
                }
                let size = entry.metadata()?.len();
                plan.total = plan
                    .total
                    .checked_add(size)
                    .ok_or_else(|| FileError::Limit("This folder is too large.".into()))?;
                plan.files.push(PlannedFile { path: entry.path(), relative: child_relative, size });
            }
        }
        // Reverse so the stack visits subfolders in name order.
        pending.extend(subdirs.into_iter().rev());
    }
    Ok(plan)
}

/// What a finished folder send delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderSent {
    pub id: u64,
    pub files: u32,
    pub bytes: u64,
    pub skipped: u32,
}

pub struct FolderSender;

impl FolderSender {
    /// Offer the folder `plan` and, once accepted, send its tree.
    ///
    /// `on_offer` is called with the folder id right after the FolderOffer is
    /// sent. `replies` must receive the peer's replies for the folder id and
    /// every file id (the sink learns each id from the offer it sends). The
    /// gate is checked per chunk like a single file; stopping it ends the
    /// whole folder.
    pub async fn send<S, R>(
        plan: &FolderPlan,
        sink: &mut S,
        replies: &mut R,
        on_offer: impl FnOnce(u64),
        mut progress: impl FnMut(FolderProgress),
        timeouts: FolderTimeouts,
        gate: &TransferGate,
    ) -> Result<FolderSent, FileError>
    where
        S: FileSink + Send,
        R: FileSource + Send,
    {
        let files_total = u32::try_from(plan.files.len()).map_err(|_| FileError::Limit("too many files".into()))?;
        let id = crate::filexfer::next_transfer_id();
        sink.send(FileMsg::FolderOffer { id, name: plan.name.clone(), files: files_total, total: plan.total }).await;
        on_offer(id);

        // An up-to-date receiver acknowledges the offer at once; an older one
        // ignores it, so give up instead of waiting for an answer that will
        // never come.
        let seen = match gate.guard(tokio::time::timeout(timeouts.seen, next_reply(replies, id))).await {
            Ok(seen) => seen,
            Err(error) => return Err(revoke(sink, id, error).await),
        };
        let accepted = match seen {
            Err(_) => {
                sink.send(FileMsg::Error { id, message: UNSUPPORTED_MESSAGE.into() }).await;
                return Err(FileError::Unsupported(UNSUPPORTED_MESSAGE.into()));
            }
            Ok(Reply::Acked(_)) => false,
            Ok(Reply::Accepted) => true,
            Ok(Reply::Rejected) => return Err(FileError::Rejected),
            Ok(Reply::Failed(message)) => return Err(FileError::Peer(message)),
            Ok(Reply::Closed) => return Err(FileError::Protocol("session closed before the receiver answered".into())),
        };
        if !accepted {
            let answer = match gate.guard(tokio::time::timeout(timeouts.flow.accept, next_reply(replies, id))).await {
                Ok(answer) => answer,
                Err(error) => return Err(revoke(sink, id, error).await),
            };
            match answer {
                Err(_) => return Err(crate::filexfer::cancel(sink, id, "the receiver did not accept the folder in time").await),
                Ok(Reply::Accepted) => {}
                Ok(Reply::Acked(_)) => return Err(FileError::Protocol("acknowledgement before acceptance".into())),
                Ok(Reply::Rejected) => return Err(FileError::Rejected),
                Ok(Reply::Failed(message)) => return Err(FileError::Peer(message)),
                Ok(Reply::Closed) => return Err(FileError::Protocol("session closed before the receiver answered".into())),
            }
        }

        let mut state = FolderProgress { files_done: 0, files_total, bytes_done: 0, bytes_total: plan.total };
        progress(state);
        for (index, dir) in plan.dirs.iter().enumerate() {
            if let Err(error) = gate.check() {
                return Err(revoke(sink, id, error).await);
            }
            sink.send(FileMsg::FolderDir { folder: id, path: dir.clone() }).await;
            // Bounded like file data: never queue more than a batch of
            // directories the receiver has not confirmed.
            let sent = index as u64 + 1;
            if sent.is_multiple_of(DIR_ACK_EVERY) {
                while await_ack(sink, replies, (id, None), timeouts.flow.ack, gate).await? < sent {}
            }
        }
        for file in &plan.files {
            if let Err(error) = gate.check() {
                return Err(revoke(sink, id, error).await);
            }
            let relative = file.relative.clone();
            let offer = move |file_id, size, hash| FileMsg::FolderFile { folder: id, id: file_id, path: relative, size, hash };
            let base = state.bytes_done;
            let mut file_progress = |done: u64, _size: u64| {
                progress(FolderProgress { bytes_done: base.saturating_add(done), ..state });
            };
            let sent = send_offered(&file.path, offer, Some(id), sink, replies, &mut file_progress, timeouts.flow, gate).await;
            if let Err(error) = sent {
                // The file's own Error (if it got that far) already stopped the
                // receiver's copy of it; this ends the folder there too.
                let message = match &error {
                    FileError::Revoked | FileError::Cancelled => gate_message(&error).to_owned(),
                    other => other.to_string(),
                };
                sink.send(FileMsg::Error { id, message }).await;
                return Err(error);
            }
            let size = std::fs::metadata(&file.path).map_or(file.size, |meta| meta.len());
            state.files_done += 1;
            state.bytes_done = base.saturating_add(size);
            progress(state);
        }
        sink.send(FileMsg::FolderDone { id }).await;
        // Every file is already confirmed; this only closes the folder.
        await_ack(sink, replies, (id, None), timeouts.flow.ack, &TransferGate::open()).await?;
        Ok(FolderSent { id, files: state.files_done, bytes: state.bytes_done, skipped: plan.skipped })
    }
}

/// What a finished folder receive stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderReceived {
    /// The folder created for this transfer inside the destination.
    pub root: PathBuf,
    pub files: u32,
    pub bytes: u64,
}

pub struct FolderReceiver;

impl FolderReceiver {
    /// Receive one folder into a new directory inside `dest_dir`.
    ///
    /// `source` yields the FolderOffer first and then the whole folder's
    /// messages (see [`crate::datachan::IncomingTransfer`]). `consent` sees the
    /// FolderOffer and runs once for the whole folder. On any failure the
    /// partially received folder is deleted and the sender is told why.
    pub async fn receive<S, K, C, ConsentFuture>(
        source: &mut S,
        sink: &mut K,
        dest_dir: &Path,
        consent: C,
        mut progress: impl FnMut(FolderProgress),
        gate: &TransferGate,
    ) -> Result<FolderReceived, FileError>
    where
        S: FileSource + Send,
        K: FileSink + Send,
        C: FnOnce(FileMsg) -> ConsentFuture,
        ConsentFuture: Future<Output = ConsentDecision>,
    {
        let offer = source
            .recv()
            .await
            .ok_or_else(|| FileError::Protocol("source closed before offer".into()))?;
        let FileMsg::FolderOffer { id, ref name, files, total } = offer else {
            return Err(FileError::Protocol("expected a folder offer".into()));
        };
        // Seen: tells the sender this side understands folders, before the
        // user takes their time to answer.
        sink.send(FileMsg::Ack { id, offset: 0 }).await;
        let Some(name) = sanitize_filename(name) else {
            sink.send(FileMsg::Reject { id, reason: "invalid folder name".into() }).await;
            return Err(FileError::BadName);
        };
        if files > MAX_FOLDER_FILES {
            let message = format!("A folder may hold at most {MAX_FOLDER_FILES} files.");
            sink.send(FileMsg::Reject { id, reason: message.clone() }).await;
            return Err(FileError::Limit(message));
        }
        if consent(offer.clone()).await == ConsentDecision::Reject {
            sink.send(FileMsg::Reject { id, reason: "declined".into() }).await;
            return Err(FileError::Rejected);
        }
        if let Err(error) = gate.check() {
            sink.send(FileMsg::Reject { id, reason: gate_message(&error).into() }).await;
            return Err(error);
        }
        let root = match create_root(dest_dir, &name).await {
            Ok(root) => root,
            Err(error) => {
                sink.send(FileMsg::Reject { id, reason: error.to_string() }).await;
                return Err(error);
            }
        };
        sink.send(FileMsg::Accept { id }).await;
        let limits = Offered { id, files, total };
        let result = receive_tree(source, sink, &root, limits, &mut progress, gate).await;
        match result {
            Ok((files, bytes)) => {
                sink.send(FileMsg::Ack { id, offset: bytes }).await;
                Ok(FolderReceived { root, files, bytes })
            }
            Err(error) => {
                // remove_dir_all does not follow symlinks; the root is a fresh
                // directory this transfer created.
                let _ = tokio::fs::remove_dir_all(&root).await;
                let message = match &error {
                    FileError::Revoked | FileError::Cancelled => gate_message(&error).to_owned(),
                    other => other.to_string(),
                };
                sink.send(FileMsg::Error { id, message }).await;
                Err(error)
            }
        }
    }
}

/// What the user agreed to; the sender may not exceed it.
#[derive(Clone, Copy)]
struct Offered {
    id: u64,
    files: u32,
    total: u64,
}

async fn receive_tree<S, K>(
    source: &mut S,
    sink: &mut K,
    root: &Path,
    offered: Offered,
    progress: &mut impl FnMut(FolderProgress),
    gate: &TransferGate,
) -> Result<(u32, u64), FileError>
where
    S: FileSource + Send,
    K: FileSink + Send,
{
    let canonical_root = tokio::fs::canonicalize(root).await?;
    let mut state = FolderProgress { files_done: 0, files_total: offered.files, bytes_done: 0, bytes_total: offered.total };
    let mut dirs = 0u32;
    progress(state);
    loop {
        let message = gate
            .guard(source.recv())
            .await?
            .ok_or_else(|| FileError::Protocol("source closed mid-folder".into()))?;
        match message {
            FileMsg::FolderDir { folder, path } if folder == offered.id => {
                dirs += 1;
                if dirs > MAX_FOLDER_DIRS {
                    return Err(FileError::Limit(format!("A folder may hold at most {MAX_FOLDER_DIRS} folders.")));
                }
                let parts = sanitize_relative_path(&path).ok_or(FileError::BadName)?;
                ensure_dirs(root, &canonical_root, &parts).await?;
                if u64::from(dirs).is_multiple_of(DIR_ACK_EVERY) {
                    sink.send(FileMsg::Ack { id: offered.id, offset: u64::from(dirs) }).await;
                }
            }
            FileMsg::FolderFile { folder, id, path, size, hash } if folder == offered.id => {
                if state.files_done >= offered.files {
                    return Err(FileError::Limit("The sender sent more files than it offered.".into()));
                }
                let new_total = state.bytes_done.checked_add(size).filter(|total| *total <= offered.total);
                let Some(new_total) = new_total else {
                    return Err(FileError::Limit("The sender sent more data than it offered.".into()));
                };
                let parts = sanitize_relative_path(&path).ok_or(FileError::BadName)?;
                let (leaf, parents) = parts.split_last().ok_or(FileError::BadName)?;
                let dir = ensure_dirs(root, &canonical_root, parents).await?;
                let base = state;
                let file_progress = |done: u64, _size: u64| {
                    progress(FolderProgress { bytes_done: base.bytes_done.saturating_add(done), ..base });
                };
                FileReceiver::accept_and_store(source, sink, &dir, leaf, id, size, hash, file_progress, gate).await?;
                state.files_done += 1;
                state.bytes_done = new_total;
                progress(state);
            }
            FileMsg::FolderDone { id } if id == offered.id => {
                if state.files_done != offered.files {
                    return Err(FileError::Protocol("the folder ended before all its files arrived".into()));
                }
                return Ok((state.files_done, state.bytes_done));
            }
            FileMsg::Error { id, message } if id == offered.id => return Err(FileError::Peer(message)),
            _ => return Err(FileError::Protocol("unexpected folder message".into())),
        }
    }
}

/// Create `name` (or "name (1)", ...) inside `dest_dir` as a new, empty
/// directory. A fresh root means nothing in it can predate this transfer.
async fn create_root(dest_dir: &Path, name: &str) -> Result<PathBuf, FileError> {
    tokio::fs::create_dir_all(dest_dir).await?;
    for index in 0..1000u32 {
        let candidate = if index == 0 { name.to_owned() } else { format!("{name} ({index})") };
        let path = dest_dir.join(candidate);
        match tokio::fs::create_dir(&path).await {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(FileError::Io(io::Error::new(io::ErrorKind::AlreadyExists, "could not choose a folder name")))
}

/// Create the directories `parts` below `root`, refusing to pass through
/// anything that is not a real directory inside it.
async fn ensure_dirs(root: &Path, canonical_root: &Path, parts: &[String]) -> Result<PathBuf, FileError> {
    let mut current = root.to_path_buf();
    for part in parts {
        current.push(part);
        match tokio::fs::create_dir(&current).await {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let meta = tokio::fs::symlink_metadata(&current).await?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(FileError::BadName);
        }
    }
    // Defense in depth against anything the checks above missed (a junction,
    // a racing local process): the resolved directory must stay inside.
    if !tokio::fs::canonicalize(&current).await?.starts_with(canonical_root) {
        return Err(FileError::BadName);
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filexfer::FileSender;
    use async_trait::async_trait;
    use std::{
        fs,
        sync::{atomic::{AtomicBool, Ordering}, Arc},
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::{sync::mpsc, time::timeout};

    struct Sink(mpsc::Sender<FileMsg>);
    struct Source(mpsc::Receiver<FileMsg>);

    #[async_trait]
    impl FileSink for Sink {
        async fn send(&mut self, msg: FileMsg) { let _ = self.0.send(msg).await; }
    }

    #[async_trait]
    impl FileSource for Source {
        async fn recv(&mut self) -> Option<FileMsg> { self.0.recv().await }
    }

    /// (sender sink, sender replies, receiver source, receiver sink)
    fn link() -> (Sink, Source, Source, Sink) {
        let (data_tx, data_rx) = mpsc::channel(256);
        let (reply_tx, reply_rx) = mpsc::channel(256);
        (Sink(data_tx), Source(reply_rx), Source(data_rx), Sink(reply_tx))
    }

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("opendesk-folder-{label}-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn fast() -> FolderTimeouts {
        FolderTimeouts {
            seen: Duration::from_millis(300),
            flow: FlowTimeouts { accept: Duration::from_secs(5), ack: Duration::from_secs(5) },
        }
    }

    /// A small tree: two nested files, an empty folder and a large file.
    fn make_tree(base: &Path) -> PathBuf {
        let root = base.join("project");
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("readme.txt"), b"hello").unwrap();
        fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();
        fs::write(root.join("src/deep/data.bin"), (0..200_000u32).map(|n| n as u8).collect::<Vec<_>>()).unwrap();
        root
    }

    async fn accept(_: FileMsg) -> ConsentDecision { ConsentDecision::Accept }

    #[test]
    fn relative_paths_reject_escapes() {
        for path in [
            "", "/etc/passwd", "../x", "a/../../x", "a/./b", "a//b", "a/", "C:/x", "C:x", "c:\\x",
            "a\\..\\b", "\\\\server\\share", "CON", "a/nul.txt", "a/COM1/b", ".ssh/id", "a/b\0c",
            "a/\u{202E}gpj.exe",
        ] {
            assert_eq!(sanitize_relative_path(path), None, "{path:?}");
        }
        let deep = vec!["d"; MAX_FOLDER_DEPTH + 1].join("/");
        assert_eq!(sanitize_relative_path(&deep), None);
        let ok = vec!["d"; MAX_FOLDER_DEPTH].join("/");
        assert!(sanitize_relative_path(&ok).is_some());
        assert_eq!(
            sanitize_relative_path("src/deep/data.bin"),
            Some(vec!["src".into(), "deep".into(), "data.bin".into()])
        );
    }

    #[test]
    fn scan_lists_the_tree_in_order() {
        let base = temp_dir("scan");
        let root = make_tree(&base);
        fs::write(root.join(".hidden"), b"x").unwrap();
        let plan = scan_folder(&root).unwrap();
        assert_eq!(plan.name, "project");
        assert_eq!(plan.dirs, ["empty", "src", "src/deep"]);
        let files: Vec<_> = plan.files.iter().map(|file| file.relative.as_str()).collect();
        assert_eq!(files, ["readme.txt", "src/main.rs", "src/deep/data.bin"]);
        assert_eq!(plan.total, 5 + 12 + 200_000);
        assert_eq!(plan.skipped, 1, "the dotfile is skipped");
        fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_symlinks() {
        use std::os::unix::fs::symlink;
        let base = temp_dir("scan-links");
        let root = make_tree(&base);
        let outside = base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"secret").unwrap();
        symlink(&outside, root.join("linked-dir")).unwrap();
        symlink(outside.join("secret.txt"), root.join("linked.txt")).unwrap();
        let plan = scan_folder(&root).unwrap();
        assert!(plan.files.iter().all(|file| !file.relative.starts_with("linked")), "{:?}", plan.files);
        assert!(!plan.dirs.iter().any(|dir| dir.starts_with("linked")));
        assert_eq!(plan.skipped, 2);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn scan_enforces_depth() {
        let base = temp_dir("scan-depth");
        let root = base.join("deep");
        let mut path = root.clone();
        for _ in 0..=MAX_FOLDER_DEPTH {
            path.push("d");
        }
        fs::create_dir_all(&path).unwrap();
        assert!(matches!(scan_folder(&root), Err(FileError::Limit(_))));
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn folder_round_trip_preserves_the_tree() {
        let base = temp_dir("roundtrip");
        let root = make_tree(&base);
        let dest = base.join("dest");
        let plan = scan_folder(&root).unwrap();
        let (mut out, mut replies, mut incoming, mut answers) = link();
        let sender = tokio::spawn(async move {
            FolderSender::send(&plan, &mut out, &mut replies, |_| {}, |_| {}, fast(), &TransferGate::open()).await
        });
        let mut last = FolderProgress::default();
        let mut prompts = 0;
        let received = FolderReceiver::receive(&mut incoming, &mut answers, &dest, |offer| {
            prompts += 1;
            accept(offer)
        }, |state| last = state, &TransferGate::open())
        .await
        .unwrap();
        let sent = sender.await.unwrap().unwrap();
        assert_eq!(prompts, 1, "one consent prompt for the whole folder");
        assert_eq!(received.root, dest.join("project"));
        assert_eq!((sent.files, received.files), (3, 3));
        assert_eq!(sent.bytes, received.bytes);
        assert_eq!(last, FolderProgress { files_done: 3, files_total: 3, bytes_done: received.bytes, bytes_total: received.bytes });
        for file in ["readme.txt", "src/main.rs", "src/deep/data.bin"] {
            assert_eq!(fs::read(received.root.join(file)).unwrap(), fs::read(root.join(file)).unwrap(), "{file}");
        }
        assert!(received.root.join("empty").is_dir(), "empty folders are kept");
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn many_empty_folders_flow_in_acknowledged_batches() {
        let base = temp_dir("many-dirs");
        let root = base.join("dirs");
        for index in 0..100 {
            fs::create_dir_all(root.join(format!("d{index:03}"))).unwrap();
        }
        let dest = base.join("dest");
        let plan = scan_folder(&root).unwrap();
        // Channels far smaller than the directory count: only the batch acks
        // let the sender go on.
        let (data_tx, data_rx) = mpsc::channel(8);
        let (reply_tx, reply_rx) = mpsc::channel(8);
        let sender = tokio::spawn(async move {
            FolderSender::send(&plan, &mut Sink(data_tx), &mut Source(reply_rx), |_| {}, |_| {}, fast(), &TransferGate::open()).await
        });
        let received = FolderReceiver::receive(&mut Source(data_rx), &mut Sink(reply_tx), &dest, accept, |_| {}, &TransferGate::open()).await.unwrap();
        assert_eq!(sender.await.unwrap().unwrap().files, 0);
        assert_eq!(fs::read_dir(&received.root).unwrap().count(), 100);
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn existing_folder_name_gets_a_new_root() {
        let base = temp_dir("rename-root");
        let root = make_tree(&base);
        let dest = base.join("dest");
        fs::create_dir_all(dest.join("project")).unwrap();
        fs::write(dest.join("project/readme.txt"), b"keep me").unwrap();
        let plan = scan_folder(&root).unwrap();
        let (mut out, mut replies, mut incoming, mut answers) = link();
        let sender = tokio::spawn(async move {
            FolderSender::send(&plan, &mut out, &mut replies, |_| {}, |_| {}, fast(), &TransferGate::open()).await
        });
        let received = FolderReceiver::receive(&mut incoming, &mut answers, &dest, accept, |_| {}, &TransferGate::open()).await.unwrap();
        sender.await.unwrap().unwrap();
        assert_eq!(received.root, dest.join("project (1)"));
        assert_eq!(fs::read(dest.join("project/readme.txt")).unwrap(), b"keep me");
        fs::remove_dir_all(base).unwrap();
    }

    /// Drive the receiver with hand-written messages from a malicious sender.
    async fn receive_scripted(dest: &Path, script: Vec<FileMsg>) -> (Result<FolderReceived, FileError>, Vec<FileMsg>) {
        let (tx, rx) = mpsc::channel(64);
        let (reply_tx, mut reply_rx) = mpsc::channel(64);
        for message in script {
            tx.send(message).await.unwrap();
        }
        drop(tx);
        let result = FolderReceiver::receive(&mut Source(rx), &mut Sink(reply_tx), dest, accept, |_| {}, &TransferGate::open()).await;
        let mut replies = Vec::new();
        while let Ok(reply) = reply_rx.try_recv() {
            replies.push(reply);
        }
        (result, replies)
    }

    fn offer(files: u32, total: u64) -> FileMsg {
        FileMsg::FolderOffer { id: 1, name: "evil".into(), files, total }
    }

    fn file(path: &str, data: &[u8]) -> Vec<FileMsg> {
        use sha2::{Digest, Sha256};
        vec![
            FileMsg::FolderFile { folder: 1, id: 2, path: path.into(), size: data.len() as u64, hash: Sha256::digest(data).into() },
            FileMsg::Chunk { id: 2, offset: 0, data: data.to_vec() },
            FileMsg::Done { id: 2 },
        ]
    }

    /// Every entry under `dir`, relative, for asserting nothing escaped.
    fn listing(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(next) = stack.pop() {
            for entry in fs::read_dir(&next).unwrap() {
                let path = entry.unwrap().path();
                out.push(path.strip_prefix(dir).unwrap().to_string_lossy().into_owned());
                if path.is_dir() && !path.is_symlink() {
                    stack.push(path);
                }
            }
        }
        out.sort();
        out
    }

    #[tokio::test]
    async fn hostile_paths_never_land_outside_the_destination() {
        for bad in ["../escape.txt", "/tmp/abs.txt", "a/../../escape.txt", "C:/win.txt", "C:win.txt", "a\\..\\..\\x.txt", "NUL", "dir/aux.txt", ".bashrc", "a//b.txt"] {
            let base = temp_dir("hostile");
            let dest = base.join("dest");
            let mut script = vec![offer(1, 4)];
            script.extend(file(bad, b"evil"));
            script.push(FileMsg::FolderDone { id: 1 });
            let (result, replies) = receive_scripted(&dest, script).await;
            assert!(matches!(result, Err(FileError::BadName)), "{bad:?}: {result:?}");
            assert!(matches!(replies.last(), Some(FileMsg::Error { id: 1, .. })), "{bad:?}: {replies:?}");
            assert_eq!(listing(&base), ["dest"], "{bad:?} left files behind");
            fs::remove_dir_all(base).unwrap();
        }
    }

    #[tokio::test]
    async fn hostile_dir_paths_are_refused() {
        for bad in ["..", "../up", "/abs", "a/../..", "COM1"] {
            let base = temp_dir("hostile-dir");
            let dest = base.join("dest");
            let script = vec![offer(0, 0), FileMsg::FolderDir { folder: 1, path: bad.into() }, FileMsg::FolderDone { id: 1 }];
            let (result, _) = receive_scripted(&dest, script).await;
            assert!(matches!(result, Err(FileError::BadName)), "{bad:?}: {result:?}");
            assert_eq!(listing(&base), ["dest"], "{bad:?}");
            fs::remove_dir_all(base).unwrap();
        }
    }

    #[tokio::test]
    async fn bad_folder_name_is_rejected_before_consent() {
        for bad in ["..", "a/b", "/abs", "C:", "LPT1", ".config"] {
            let base = temp_dir("bad-root");
            let (tx, rx) = mpsc::channel(4);
            let (reply_tx, mut reply_rx) = mpsc::channel(4);
            tx.send(FileMsg::FolderOffer { id: 1, name: bad.into(), files: 0, total: 0 }).await.unwrap();
            let result = FolderReceiver::receive(&mut Source(rx), &mut Sink(reply_tx), &base, |_| async {
                panic!("no prompt for a bad name")
            }, |_| {}, &TransferGate::open())
            .await;
            assert!(matches!(result, Err(FileError::BadName)), "{bad:?}");
            assert_eq!(reply_rx.recv().await, Some(FileMsg::Ack { id: 1, offset: 0 }));
            assert!(matches!(reply_rx.recv().await, Some(FileMsg::Reject { id: 1, .. })));
            assert!(listing(&base).is_empty());
            fs::remove_dir_all(base).unwrap();
        }
    }

    #[tokio::test]
    async fn sender_cannot_exceed_what_was_offered() {
        let base = temp_dir("over-offer");
        let dest = base.join("dest");
        // One file offered, two sent.
        let mut script = vec![offer(1, 8)];
        script.extend(file("a.txt", b"four"));
        script.extend(file("b.txt", b"more").into_iter().map(|message| match message {
            FileMsg::FolderFile { folder, path, size, hash, .. } => FileMsg::FolderFile { folder, id: 3, path, size, hash },
            FileMsg::Chunk { offset, data, .. } => FileMsg::Chunk { id: 3, offset, data },
            _ => FileMsg::Done { id: 3 },
        }));
        let (result, _) = receive_scripted(&dest, script).await;
        assert!(matches!(result, Err(FileError::Limit(_))), "{result:?}");
        assert_eq!(listing(&base), ["dest"], "the partial folder is removed");

        // More bytes than offered.
        let mut script = vec![offer(1, 3)];
        script.extend(file("a.txt", b"four"));
        let (result, _) = receive_scripted(&dest, script).await;
        assert!(matches!(result, Err(FileError::Limit(_))), "{result:?}");

        // Too many files to even ask about.
        let (result, replies) = receive_scripted(&dest, vec![offer(MAX_FOLDER_FILES + 1, 0)]).await;
        assert!(matches!(result, Err(FileError::Limit(_))), "{result:?}");
        assert!(matches!(replies.last(), Some(FileMsg::Reject { id: 1, .. })));
        fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn receiver_refuses_to_walk_through_a_symlink() {
        use std::os::unix::fs::symlink;
        let base = temp_dir("receiver-link");
        let dest = base.join("dest");
        let outside = base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let (tx, rx) = mpsc::channel(64);
        let (reply_tx, mut reply_rx) = mpsc::channel(64);
        tx.send(offer(1, 4)).await.unwrap();
        tx.send(FileMsg::FolderDir { folder: 1, path: "sub".into() }).await.unwrap();
        let receiver = tokio::spawn(async move {
            FolderReceiver::receive(&mut Source(rx), &mut Sink(reply_tx), &dest, accept, |_| {}, &TransferGate::open()).await
        });
        // Once accepted, a local process swaps the new "sub" for a symlink.
        assert_eq!(reply_rx.recv().await, Some(FileMsg::Ack { id: 1, offset: 0 }));
        assert_eq!(reply_rx.recv().await, Some(FileMsg::Accept { id: 1 }));
        let sub = base.join("dest/evil/sub");
        for _ in 0..100 {
            if sub.is_dir() { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        fs::remove_dir(&sub).unwrap();
        symlink(&outside, &sub).unwrap();
        for message in file("sub/planted.txt", b"evil") {
            tx.send(message).await.unwrap();
        }
        let result = timeout(Duration::from_secs(5), receiver).await.unwrap().unwrap();
        assert!(matches!(result, Err(FileError::BadName)), "{result:?}");
        assert!(listing(&outside).is_empty(), "nothing may be written through the link");
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn rejected_folder_writes_nothing() {
        let base = temp_dir("reject");
        let root = make_tree(&base);
        let dest = base.join("dest");
        let plan = scan_folder(&root).unwrap();
        let (mut out, mut replies, mut incoming, mut answers) = link();
        let sender = tokio::spawn(async move {
            FolderSender::send(&plan, &mut out, &mut replies, |_| {}, |_| {}, fast(), &TransferGate::open()).await
        });
        let result = FolderReceiver::receive(&mut incoming, &mut answers, &dest, |_| async { ConsentDecision::Reject }, |_| {}, &TransferGate::open()).await;
        assert!(matches!(result, Err(FileError::Rejected)));
        assert!(matches!(sender.await.unwrap(), Err(FileError::Rejected)));
        assert!(!dest.join("project").exists());
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn an_older_peer_fails_fast_instead_of_hanging() {
        let base = temp_dir("old-peer");
        let root = make_tree(&base);
        let plan = scan_folder(&root).unwrap();
        // The old receiver drops what it cannot parse and never answers.
        let (mut out, mut replies, _incoming, _answers) = link();
        let result = timeout(
            Duration::from_secs(5),
            FolderSender::send(&plan, &mut out, &mut replies, |_| {}, |_| {}, fast(), &TransferGate::open()),
        )
        .await
        .expect("the sender must give up on its own");
        assert!(matches!(result, Err(FileError::Unsupported(_))), "{result:?}");
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn a_peer_that_rejects_folders_outright_is_a_clean_rejection() {
        let base = temp_dir("web-peer");
        let root = make_tree(&base);
        let plan = scan_folder(&root).unwrap();
        let (mut out, mut replies, mut incoming, mut answers) = link();
        tokio::spawn(async move {
            if let Some(FileMsg::FolderOffer { id, .. }) = incoming.recv().await {
                answers.send(FileMsg::Reject { id, reason: "folders are not supported".into() }).await;
            }
        });
        let result = FolderSender::send(&plan, &mut out, &mut replies, |_| {}, |_| {}, fast(), &TransferGate::open()).await;
        assert!(matches!(result, Err(FileError::Rejected)), "{result:?}");
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn cancelling_the_sender_stops_the_folder_and_cleans_up() {
        let base = temp_dir("cancel");
        let root = base.join("big");
        fs::create_dir_all(&root).unwrap();
        for index in 0..4 {
            fs::write(root.join(format!("part{index}.bin")), vec![7u8; 3 * crate::filexfer::SEND_WINDOW_BYTES as usize]).unwrap();
        }
        let dest = base.join("dest");
        let plan = scan_folder(&root).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let (mut out, mut replies, mut incoming, mut answers) = link();
        let sender = tokio::spawn(async move {
            let gate = TransferGate::open().with_cancel(flag.clone());
            FolderSender::send(&plan, &mut out, &mut replies, |_| {}, move |state| {
                if state.files_done == 1 && state.bytes_done > state.bytes_total / 3 { flag.store(true, Ordering::Relaxed); }
            }, fast(), &gate)
            .await
        });
        let result = FolderReceiver::receive(&mut incoming, &mut answers, &dest, accept, |_| {}, &TransferGate::open()).await;
        assert!(matches!(result, Err(FileError::Peer(_))), "{result:?}");
        assert!(matches!(sender.await.unwrap(), Err(FileError::Cancelled)));
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(!dest.join("big").exists(), "a cancelled folder leaves nothing behind");
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn revoking_the_receivers_gate_stops_the_folder() {
        let base = temp_dir("revoke");
        let root = base.join("big");
        fs::create_dir_all(&root).unwrap();
        for index in 0..3 {
            fs::write(root.join(format!("part{index}.bin")), vec![1u8; 3 * crate::filexfer::SEND_WINDOW_BYTES as usize]).unwrap();
        }
        let dest = base.join("dest");
        let plan = scan_folder(&root).unwrap();
        let files = Arc::new(AtomicBool::new(true));
        let (mut out, mut replies, mut incoming, mut answers) = link();
        let sender = tokio::spawn(async move {
            FolderSender::send(&plan, &mut out, &mut replies, |_| {}, |_| {}, fast(), &TransferGate::open()).await
        });
        let flag = files.clone();
        let result = FolderReceiver::receive(&mut incoming, &mut answers, &dest, accept, move |state| {
            if state.files_done == 1 { flag.store(false, Ordering::Relaxed); }
        }, &TransferGate::new(files))
        .await;
        assert!(matches!(result, Err(FileError::Revoked)), "{result:?}");
        let sent = sender.await.unwrap();
        assert!(matches!(&sent, Err(FileError::Peer(message)) if message == crate::filexfer::REVOKED_MESSAGE), "{sent:?}");
        assert!(!dest.join("big").exists());
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn a_single_file_still_works_alongside_folders() {
        // The refactored per-file sender keeps the plain Offer path intact.
        let base = temp_dir("single");
        let path = base.join("one.txt");
        fs::write(&path, b"just one").unwrap();
        let (mut out, mut replies, mut incoming, mut answers) = link();
        let sender = tokio::spawn(async move { FileSender::send(path, &mut out, &mut replies, |_, _| {}).await });
        let received = FileReceiver::receive(&mut incoming, &mut answers, &base.join("dest"), |_| ConsentDecision::Accept, |_, _| {}).await.unwrap();
        sender.await.unwrap().unwrap();
        assert_eq!(fs::read(received).unwrap(), b"just one");
        fs::remove_dir_all(base).unwrap();
    }
}
