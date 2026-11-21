//! Copy and paste of files through the clipboard.
//!
//! The host announces files copied in its file manager by name only. Nothing
//! moves until the viewer user pastes: then the viewer asks for each entry by
//! index, it arrives over the normal file transfer path into a folder the
//! viewer chose, and the received paths go on the viewer's clipboard so a
//! normal paste in Finder or Explorer drops them in place. The other way, the
//! viewer user uploads the files on its own clipboard to the host, which puts
//! them on its clipboard once they arrive. The host never pulls files from
//! the viewer.

use super::*;
use host_core::clipboard::SharedFiles;
use protocol::{browse::BrowseMsg, clipboard::ClipboardFile};

/// How long a requested file may take to start before the paste gives up,
/// for example because the host predates clipboard files.
const PULL_START_TIMEOUT: Duration = Duration::from_secs(20);

/// Host side: the paths behind the host's latest file-list announcement.
pub(crate) fn shared_files() -> &'static SharedFiles {
    static SHARED: std::sync::OnceLock<SharedFiles> = std::sync::OnceLock::new();
    SHARED.get_or_init(SharedFiles::default)
}

/// Viewer side: the host's latest file-list announcement.
fn remote_files() -> &'static Mutex<Option<(u64, Vec<ClipboardFile>)>> {
    static REMOTE: Mutex<Option<(u64, Vec<ClipboardFile>)>> = Mutex::new(None);
    &REMOTE
}

/// Viewer side: the one entry a paste is waiting for.
struct PendingPull {
    req: u64,
    name: String,
    dest: PathBuf,
    started: Option<oneshot::Sender<Result<(), String>>>,
    done: Option<oneshot::Sender<Result<PathBuf, String>>>,
}

fn pending_pull() -> &'static Mutex<Option<PendingPull>> {
    static PENDING: Mutex<Option<PendingPull>> = Mutex::new(None);
    &PENDING
}

/// Host side: uploads announced with PasteNext and not yet started (batch,
/// count), and the files of the current batch received so far.
#[derive(Default)]
struct PasteBatches {
    armed: Option<(u64, u32)>,
    received: Option<(u64, Vec<PathBuf>)>,
}

fn paste_batches() -> &'static Mutex<PasteBatches> {
    static BATCHES: Mutex<PasteBatches> = Mutex::new(PasteBatches { armed: None, received: None });
    &BATCHES
}

/// Forget everything when a session ends.
pub(crate) fn reset() {
    shared_files().clear();
    *lock_unpoisoned(remote_files()) = None;
    *lock_unpoisoned(pending_pull()) = None;
    *lock_unpoisoned(paste_batches()) = PasteBatches::default();
}

#[derive(Clone, Serialize)]
pub(crate) struct RemoteClipboardFile {
    name: String,
    size: u64,
    is_dir: bool,
}

#[derive(Clone, Serialize)]
pub(crate) struct RemoteClipboardFiles {
    files: Vec<RemoteClipboardFile>,
}

fn view(files: &[ClipboardFile]) -> RemoteClipboardFiles {
    RemoteClipboardFiles {
        files: files.iter().map(|file| RemoteClipboardFile { name: file.name.clone(), size: file.size, is_dir: file.is_dir }).collect(),
    }
}

/// Viewer side: the host's clipboard changed. A file list is remembered for
/// a later paste; anything else clears it.
pub(crate) fn remote_clipboard_changed(app: &AppHandle, seq: u64, files: Option<Vec<ClipboardFile>>) {
    let mut remote = lock_unpoisoned(remote_files());
    match files {
        Some(files) => {
            emit(app, "remote_clipboard_files", view(&files));
            *remote = Some((seq, files));
        }
        None => {
            if remote.take().is_some() {
                emit(app, "remote_clipboard_files", RemoteClipboardFiles { files: Vec::new() });
            }
        }
    }
}

/// The files the host has on its clipboard, for a window that opens late.
#[tauri::command]
pub(crate) fn remote_clipboard_files() -> RemoteClipboardFiles {
    lock_unpoisoned(remote_files()).as_ref().map_or(RemoteClipboardFiles { files: Vec::new() }, |(_, files)| view(files))
}

/// Viewer side: where an incoming offer should be saved if it is the entry a
/// paste is waiting for.
pub(crate) fn pull_dest() -> Option<PathBuf> {
    lock_unpoisoned(pending_pull()).as_ref().map(|pending| pending.dest.clone())
}

/// Viewer side: claim the waiting paste for an offer named `name`. On a match
/// the offer is accepted without a prompt (the user asked for it by pasting)
/// and the returned sender reports where it was saved.
pub(crate) fn claim_pull(name: &str) -> Option<oneshot::Sender<Result<PathBuf, String>>> {
    let mut pending = lock_unpoisoned(pending_pull());
    let waiting = pending.as_mut().filter(|pending| pending.name == name)?;
    let started = waiting.started.take()?;
    let _ = started.send(Ok(()));
    waiting.done.take()
}

/// Viewer side: the host refused request `req`.
pub(crate) fn pull_refused(req: u64, message: &str) -> bool {
    let mut pending = lock_unpoisoned(pending_pull());
    let Some(waiting) = pending.as_mut().filter(|pending| pending.req == req) else { return false };
    if let Some(started) = waiting.started.take() {
        let _ = started.send(Err(message.to_owned()));
    }
    true
}

/// Host side: the viewer asked for entry `index` of announcement `seq`. Sent
/// like a file manager download, gated and audited the same way, and only
/// while the file is still what the host has on its clipboard.
pub(crate) fn serve_clipboard_pull(app: &AppHandle, req: u64, seq: u64, index: u32, files_allowed: &Arc<AtomicBool>) {
    let clipboard_allowed = app.state::<AppState>().lock().viewer_permissions.clipboard.load(Ordering::Relaxed);
    let refusal = if !files_allowed.load(Ordering::Relaxed) {
        audit_hosted(app, AuditEvent::new(audit_kind::FILE_DOWNLOAD_REJECTED, None).with_detail("clipboard paste: file access off"));
        Some("The other computer turned off file access.")
    } else if !clipboard_allowed {
        audit_hosted(app, AuditEvent::new(audit_kind::FILE_DOWNLOAD_REJECTED, None).with_detail("clipboard paste: clipboard off"));
        Some("The other computer turned off clipboard sharing.")
    } else {
        None
    };
    let path = shared_files().path(seq, index);
    let refusal = refusal.or_else(|| path.is_none().then_some("That item is no longer on the other computer's clipboard."));
    if let Some(message) = refusal {
        refuse(app, req, message);
        return;
    }
    let Some(path) = path else { return };
    let path = path.display().to_string();
    emit_activity(app, "files", format!("sent \"{path}\" from the clipboard"));
    files::serve_path(app, req, path, files_allowed.clone(), "pasted from clipboard by viewer");
}

fn refuse(app: &AppHandle, req: u64, message: &str) {
    let (outgoing, sealer) = {
        let state = app.state::<AppState>();
        let state = state.lock();
        (state.file_outgoing.clone(), state.file_sealer.clone())
    };
    let (Some(outgoing), Some(sealer)) = (outgoing, sealer) else { return };
    let reply = BrowseMsg::BrowseError { req, message: message.to_owned() };
    let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Data as u8], &DataMsg::Browse(reply).to_bytes());
    tauri::async_runtime::spawn(async move {
        let _ = outgoing.send_frame(Tag::Data, &sealed).await;
    });
}

/// Host side: the next upload belongs to paste `batch`.
pub(crate) fn arm_paste(batch: u64) {
    let mut batches = lock_unpoisoned(paste_batches());
    batches.armed = match batches.armed {
        Some((armed, count)) if armed == batch => Some((batch, count.saturating_add(1))),
        _ => Some((batch, 1)),
    };
}

/// Host side: claim the paste mark for an upload that is starting.
pub(crate) fn take_paste_mark() -> Option<u64> {
    let mut batches = lock_unpoisoned(paste_batches());
    let (batch, count) = batches.armed?;
    batches.armed = (count > 1).then(|| (batch, count - 1));
    Some(batch)
}

/// Host side: a pasted upload arrived. Every file of its batch so far goes on
/// the host's clipboard, subject to the clipboard permission and preference.
pub(crate) fn pasted(app: &AppHandle, batch: u64, path: PathBuf) {
    let paths = {
        let mut batches = lock_unpoisoned(paste_batches());
        let received = match batches.received.take() {
            Some((current, mut paths)) if current == batch => {
                paths.push(path);
                paths
            }
            _ => vec![path],
        };
        batches.received = Some((batch, received.clone()));
        received
    };
    let state = app.state::<AppState>();
    let state = state.lock();
    if let Some(thread) = &state.clipboard_thread {
        let _ = thread.command_tx.send(ClipboardCommand::SetFiles(paths, false));
    }
}

async fn send_browse(state: &tauri::State<'_, AppState>, message: BrowseMsg) -> Result<(), String> {
    let (outgoing, sealer) = {
        let state = state.lock();
        (state.file_outgoing.clone(), state.file_sealer.clone())
    };
    let outgoing = outgoing.ok_or_else(|| "no active file channel".to_owned())?;
    let sealer = sealer.ok_or_else(|| "file session keys are not ready".to_owned())?;
    let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Data as u8], &DataMsg::Browse(message).to_bytes());
    outgoing.send_frame(Tag::Data, &sealed).await.map_err(|error| error.to_string())
}

/// Viewer side: paste the files the host has on its clipboard into `dir`
/// (Downloads when empty), one at a time, then put them on this computer's
/// clipboard. Returns where they were saved.
#[tauri::command]
pub(crate) async fn paste_from_remote(state: tauri::State<'_, AppState>, dir: String) -> Result<Vec<String>, String> {
    if state.lock().hosted_peer.is_some() {
        return Err("Pasting files is only available while controlling another computer.".into());
    }
    let (seq, files) = lock_unpoisoned(remote_files()).clone().ok_or_else(|| "Nothing to paste: copy files on the other computer first.".to_owned())?;
    let dest = if dir.is_empty() { files::downloads_dir() } else { PathBuf::from(dir) };
    if !dest.exists() && dest == files::downloads_dir() {
        let _ = fs::create_dir_all(&dest);
    }
    if !dest.is_dir() {
        return Err(format!("{} is not a folder.", dest.display()));
    }
    let mut saved = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let index = u32::try_from(index).map_err(|_| "too many files".to_owned())?;
        let req = host_core::filexfer::next_transfer_id();
        let (started_tx, started_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        {
            let mut pending = lock_unpoisoned(pending_pull());
            if pending.is_some() {
                return Err("A paste is already in progress.".into());
            }
            *pending = Some(PendingPull { req, name: file.name.clone(), dest: dest.clone(), started: Some(started_tx), done: Some(done_tx) });
        }
        let result = pull_one(&state, req, seq, index, &file.name, started_rx, done_rx).await;
        *lock_unpoisoned(pending_pull()) = None;
        saved.push(result?);
    }
    let paths: Vec<PathBuf> = saved.iter().map(PathBuf::from).collect();
    if let Some(thread) = &state.lock().clipboard_thread {
        let _ = thread.command_tx.send(ClipboardCommand::SetFiles(paths, true));
    }
    Ok(saved)
}

async fn pull_one(
    state: &tauri::State<'_, AppState>,
    req: u64,
    seq: u64,
    index: u32,
    name: &str,
    started: oneshot::Receiver<Result<(), String>>,
    done: oneshot::Receiver<Result<PathBuf, String>>,
) -> Result<String, String> {
    send_browse(state, BrowseMsg::PullClipboard { req, seq, index }).await?;
    match tokio::time::timeout(PULL_START_TIMEOUT, started).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(message))) => return Err(message),
        Ok(Err(_)) => return Err("The paste was cancelled.".into()),
        Err(_) => return Err(format!("{name} did not start. The other computer may need an OpenDesk update to paste files.")),
    }
    match done.await {
        Ok(Ok(path)) => Ok(path.display().to_string()),
        Ok(Err(message)) => Err(message),
        Err(_) => Err("The paste was cancelled.".into()),
    }
}

/// Viewer side: upload the files on this computer's clipboard into the
/// remote folder `dir` (the host's Downloads when empty). The host puts them
/// on its clipboard as they arrive, so a paste there drops them in place.
/// Returns how many were sent.
#[tauri::command]
pub(crate) async fn paste_to_remote(state: tauri::State<'_, AppState>, app: AppHandle, dir: String) -> Result<usize, String> {
    let (outgoing, sealer, router, hosting) = {
        let state = state.lock();
        (state.file_outgoing.clone(), state.file_sealer.clone(), state.file_router.clone(), state.hosted_peer.is_some())
    };
    if hosting {
        return Err("Pasting files is only available while controlling another computer.".into());
    }
    let outgoing = outgoing.ok_or_else(|| "no active file channel".to_owned())?;
    let sealer = sealer.ok_or_else(|| "file session keys are not ready".to_owned())?;
    let router = router.ok_or_else(|| "no active file channel".to_owned())?;
    let paths = tauri::async_runtime::spawn_blocking(host_core::clipboard::local_file_list).await.map_err(|error| error.to_string())?;
    if paths.is_empty() {
        return Err("Nothing to paste: copy files on this computer first.".into());
    }
    let batch = host_core::filexfer::next_transfer_id();
    for path in &paths {
        send_browse(&state, BrowseMsg::SetUploadDir { dir: dir.clone() }).await?;
        send_browse(&state, BrowseMsg::PasteNext { batch }).await?;
        spawn_outgoing(app.clone(), path.display().to_string(), outgoing.clone(), sealer.clone(), router.clone(), TransferGate::open(), None)
            .await
            .map_err(|_| "file transfer stopped before sending its offer".to_owned())??;
    }
    Ok(paths.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The paste state is global; keep these tests from interleaving.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn paste_marks_count_down_per_upload() {
        let _serial = lock_unpoisoned(&SERIAL);
        reset();
        arm_paste(5);
        arm_paste(5);
        assert_eq!(take_paste_mark(), Some(5));
        assert_eq!(take_paste_mark(), Some(5));
        assert_eq!(take_paste_mark(), None);
        // A new batch replaces a stale one.
        arm_paste(5);
        arm_paste(6);
        assert_eq!(take_paste_mark(), Some(6));
        assert_eq!(take_paste_mark(), None);
        reset();
    }

    #[test]
    fn a_waiting_paste_claims_only_its_own_file() {
        let _serial = lock_unpoisoned(&SERIAL);
        reset();
        let (started_tx, mut started_rx) = oneshot::channel();
        let (done_tx, _done_rx) = oneshot::channel();
        *lock_unpoisoned(pending_pull()) = Some(PendingPull { req: 9, name: "a.txt".into(), dest: PathBuf::from("/tmp"), started: Some(started_tx), done: Some(done_tx) });
        assert_eq!(pull_dest(), Some(PathBuf::from("/tmp")));
        assert!(claim_pull("other.txt").is_none());
        assert!(!pull_refused(8, "no"));
        assert!(claim_pull("a.txt").is_some());
        assert_eq!(started_rx.try_recv().unwrap(), Ok(()));
        // Claimed once only.
        assert!(claim_pull("a.txt").is_none());
        reset();
    }

    #[test]
    fn a_refused_pull_reports_the_host_message() {
        let _serial = lock_unpoisoned(&SERIAL);
        reset();
        let (started_tx, mut started_rx) = oneshot::channel();
        let (done_tx, _done_rx) = oneshot::channel();
        *lock_unpoisoned(pending_pull()) = Some(PendingPull { req: 3, name: "a".into(), dest: PathBuf::from("/tmp"), started: Some(started_tx), done: Some(done_tx) });
        assert!(pull_refused(3, "file access off"));
        assert_eq!(started_rx.try_recv().unwrap(), Err("file access off".to_owned()));
        reset();
    }
}
