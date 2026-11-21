//! File transfer over the session data channel.

use super::*;

/// A FileSink backed by the session's reliable, sealed Data channel.
pub(crate) struct ReliableFileSink {
    pub(crate) outgoing: ReliableSender,
    pub(crate) sealer: Arc<Mutex<Sealer>>,
    pub(crate) offer_id: Option<oneshot::Sender<Result<u64, String>>>,
    pub(crate) transfer_id: Arc<Mutex<Option<u64>>>,
    /// For an outgoing transfer: where the peer's replies are routed once the
    /// Offer reveals the transfer id. Registered before the Offer is sent.
    pub(crate) replies: Option<(Arc<FileRouter>, mpsc::Sender<FileMsg>)>,
}

#[async_trait::async_trait]
impl FileSink for ReliableFileSink {
    async fn send(&mut self, message: FileMsg) {
        if let FileMsg::Offer { id, .. } = &message {
            *lock_unpoisoned(&self.transfer_id) = Some(*id);
            if let Some((router, replies)) = self.replies.take() {
                router.register_outgoing(*id, replies);
            }
            if let Some(sender) = self.offer_id.take() {
                let _ = sender.send(Ok(*id));
            }
        }
        let plaintext = DataMsg::FileTransfer(message).to_bytes();
        let sealed = lock_unpoisoned(&self.sealer).seal(&[Tag::Data as u8], &plaintext);
        let _ = self.outgoing.send_frame(Tag::Data, &sealed).await;
    }
}

impl ReliableFileSink {
    pub(crate) fn fail_before_offer(&mut self, error: &str) {
        if let Some(sender) = self.offer_id.take() {
            let _ = sender.send(Err(error.to_owned()));
        }
    }
}

pub(crate) struct ReliableFileSource {
    pub(crate) incoming: mpsc::Receiver<FileMsg>,
}

#[async_trait::async_trait]
impl FileSource for ReliableFileSource {
    async fn recv(&mut self) -> Option<FileMsg> {
        self.incoming.recv().await
    }
}

pub(crate) fn downloads_dir() -> PathBuf {
    if let Ok(path) = std::env::var("XDG_DOWNLOAD_DIR") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    home.map(|path| path.join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("Downloads"))
}

/// The user's videos folder: `~/Movies` on macOS, `~/Videos` elsewhere (honours
/// `XDG_VIDEOS_DIR` on Linux). Unlike Downloads, macOS does not guard this
/// folder behind a privacy prompt, so writing recordings here is silent.
pub(crate) fn videos_dir() -> PathBuf {
    if let Ok(path) = std::env::var("XDG_VIDEOS_DIR") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let folder = if cfg!(target_os = "macos") { "Movies" } else { "Videos" };
    home.map(|path| path.join(folder))
        .unwrap_or_else(|| PathBuf::from(folder))
}

pub(crate) fn route_file_consent(
    pending: &mut HashMap<u64, oneshot::Sender<bool>>,
    id: u64,
    accept: bool,
) -> Result<(), String> {
    pending
        .remove(&id)
        .ok_or_else(|| "no incoming file offer with that id".to_owned())?
        .send(accept)
        .map_err(|_| "incoming file offer expired".to_owned())
}

/// Serve every incoming file offer for the whole session. Each offer gets its
/// own task, so a prompt the user has not answered yet never blocks a later
/// offer or the session's Data channel. Ends when the session's router goes.
pub(crate) fn spawn_file_receiver(
    app: AppHandle,
    mut offers: mpsc::Receiver<IncomingTransfer>,
    from: String,
    is_host: bool,
) {
    tauri::async_runtime::spawn(async move {
        while let Some(IncomingTransfer { id, source, folder }) = offers.recv().await {
            let (outgoing, sealer) = {
                let state = app.state::<AppState>();
                let state = state.lock();
                (state.file_outgoing.clone(), state.file_sealer.clone())
            };
            let (Some(outgoing), Some(sealer)) = (outgoing, sealer) else { break };
            if folder {
                tauri::async_runtime::spawn(receive_folder_offer(app.clone(), source, id, from.clone(), outgoing, sealer, is_host));
            } else {
                tauri::async_runtime::spawn(receive_offer(app.clone(), source, id, from.clone(), outgoing, sealer, is_host));
            }
        }
    });
}

/// Ask the user about one offer and, if accepted, receive the file. Consent
/// is awaited asynchronously before FileReceiver creates a temporary file,
/// so no Tokio worker is blocked by the UI response.
async fn receive_offer(
    app: AppHandle,
    mut source: host_core::datachan::TransferSource,
    id: u64,
    from: String,
    outgoing: ReliableSender,
    sealer: Arc<Mutex<Sealer>>,
    is_host: bool,
) {
    let mut answers = ReliableFileSink {
        outgoing,
        sealer,
        offer_id: None,
        transfer_id: Arc::new(Mutex::new(Some(id))),
        replies: None,
    };
    let pending_app = app.clone();
    let progress_app = app.clone();
    // The accepted file's name and size, captured at consent time so completion
    // can name it in the activity and audit logs.
    let received_name = Arc::new(Mutex::new((String::new(), 0u64)));
    let name_slot = received_name.clone();
    // A viewer uploading into a browsed folder sets a one-shot destination;
    // consume it here, else fall back to Downloads. An invalid or non-directory
    // path also falls back, so a bad value never drops the file.
    let dest_dir = take_upload_dir(&app);
    // Shown in the accept prompt so the host sees where the file would land.
    let dest_display = dest_dir.display().to_string();
    // Checked per chunk too, so turning file access off stops an accepted
    // upload mid-way and deletes the partial file.
    let gate = TransferGate::new(app.state::<AppState>().lock().viewer_permissions.files.clone());
    // Uploads into this host are audited; files a viewer receives are not.
    let audit_dir = dest_display.clone();
    let audit_upload = move |app: &AppHandle, kind: &str, name: &str, size: Option<u64>, detail: Option<String>| {
        if !is_host { return; }
        let mut event = AuditEvent::new(kind, None).with_path(audit_dir.clone());
        if !name.is_empty() { event = event.with_file(name, size); }
        event.detail = detail;
        audit_hosted(app, event);
    };
    let consent_audit = audit_upload.clone();
    let result = FileReceiver::receive_gated(
        &mut source,
        &mut answers,
        &dest_dir,
        move |offer| async move {
            let FileMsg::Offer { id, name, size, .. } = offer else {
                return ConsentDecision::Reject;
            };
            // Host-enforced file permission: auto-reject without prompting the
            // user when the viewer's file transfer is blocked.
            if !pending_app.state::<AppState>().lock().viewer_permissions.files.load(Ordering::Relaxed) {
                emit_activity(&pending_app, "file", format!("blocked file \"{name}\""));
                consent_audit(&pending_app, audit_kind::FILE_UPLOAD_REJECTED, &name, Some(size), Some("file access off".into()));
                return ConsentDecision::Reject;
            }
            *lock_unpoisoned(&name_slot) = (name.clone(), size);
            let audit_name = name.clone();
            let (decision_tx, decision_rx) = oneshot::channel();
            pending_app
                .state::<AppState>()
                .lock()
                .pending_file_requests
                .insert(id, decision_tx);
            // A host started at sign-in may be hidden in the tray.
            startup::reveal_if_hidden(&pending_app);
            emit(
                &pending_app,
                "file_offer",
                FileOffer {
                    id,
                    name,
                    size,
                    from,
                    dir: dest_display,
                    files: None,
                },
            );

            match decision_rx.await.unwrap_or(false) {
                true => {
                    consent_audit(&pending_app, audit_kind::FILE_UPLOAD_ACCEPTED, &audit_name, Some(size), None);
                    ConsentDecision::Accept
                }
                false => {
                    consent_audit(&pending_app, audit_kind::FILE_UPLOAD_REJECTED, &audit_name, Some(size), Some("declined by host user".into()));
                    ConsentDecision::Reject
                }
            }
        },
        move |transferred, total| {
            emit(
                &progress_app,
                "file_progress",
                FileProgress {
                    id,
                    transferred,
                    total,
                    direction: "receive",
                    files_done: None,
                    files_total: None,
                },
            );
        },
        &gate,
    )
    .await;
    match result {
        Ok(path) => {
            let (name, size) = lock_unpoisoned(&received_name).clone();
            if !name.is_empty() { emit_activity(&app, "file", format!("received \"{name}\"")); }
            // The saved name can differ from the offered one when a file of
            // that name already exists.
            let saved = path.file_name().map_or(name, |saved| saved.to_string_lossy().into_owned());
            audit_upload(&app, audit_kind::FILE_UPLOAD_COMPLETED, &saved, Some(size), None);
            emit(&app, "file_done", FileDone { id, path: Some(path.display().to_string()) });
        }
        Err(error) => {
            // A refusal was already audited when it was decided.
            if !matches!(error, host_core::filexfer::FileError::Rejected) {
                let (name, size) = lock_unpoisoned(&received_name).clone();
                let size = (!name.is_empty()).then_some(size);
                audit_upload(&app, audit_kind::FILE_UPLOAD_FAILED, &name, size, Some(error.to_string()));
            }
            emit(
                &app,
                "file_error",
                FileError {
                    id,
                    message: error.to_string(),
                },
            )
        }
    }
}

#[tauri::command]
pub(crate) async fn send_file(
    state: tauri::State<'_, AppState>,
    app: AppHandle,
    path: String,
) -> Result<u64, String> {
    let (outgoing, sealer, router) = {
        let state = state.lock();
        (
        state.file_outgoing.clone(),
        state.file_sealer.clone(),
        state.file_router.clone(),
        )
    };
    let outgoing = outgoing.ok_or_else(|| "no active reliable file channel".to_owned())?;
    let sealer = sealer.ok_or_else(|| "file session keys are not ready".to_owned())?;
    let router = router.ok_or_else(|| "no active reliable file channel".to_owned())?;
    // A host pushing a file to its viewer is audited like a pull.
    let audit = state.lock().hosted_peer.is_some().then_some("sent by host user");
    let offer_rx = spawn_outgoing(app, path, outgoing, sealer, router, TransferGate::open(), audit);
    // FileSender allocates the id when it emits Offer. Returning after that
    // first sealed frame lets the UI track a transfer while it waits for the
    // receiver's answer and streams.
    offer_rx
        .await
        .map_err(|_| "file transfer stopped before sending its offer".to_owned())?
}

/// Spawn a task that sends `path` over the data channel as a file transfer.
/// Shared by the viewer's "Send file" and by answering a remote pull. Resolves
/// to the transfer id once the offer is on the wire. A remote pull passes the
/// host's file permission as `gate` so revoking it stops the transfer. With
/// `audit` (why the file left this host) the outcome goes to the host audit log.
pub(crate) fn spawn_outgoing_file(
    app: AppHandle,
    path: String,
    outgoing: ReliableSender,
    sealer: Arc<Mutex<Sealer>>,
    router: Arc<FileRouter>,
    gate: TransferGate,
    audit: Option<&'static str>,
) -> oneshot::Receiver<Result<u64, String>> {
    let (offer_tx, offer_rx) = oneshot::channel();
    let task_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let transfer_id = Arc::new(Mutex::new(None));
        let progress_id = transfer_id.clone();
        let progress_app = task_app.clone();
        let (reply_tx, reply_rx) = mpsc::channel(64);
        let mut replies = ReliableFileSource { incoming: reply_rx };
        let mut sink = ReliableFileSink {
            outgoing,
            sealer,
            offer_id: Some(offer_tx),
            transfer_id: transfer_id.clone(),
            replies: Some((Arc::clone(&router), reply_tx)),
        };
        let progress = move |transferred, total| {
            if let Some(id) = *lock_unpoisoned(&progress_id) {
                emit(
                    &progress_app,
                    "file_progress",
                    FileProgress {
                        id,
                        transferred,
                        total,
                        direction: "send",
                        files_done: None,
                        files_total: None,
                    },
                );
            }
        };
        let result = FileSender::send_gated(Path::new(&path), &mut sink, &mut replies, progress, FlowTimeouts::default(), &gate).await;
        let id = *lock_unpoisoned(&transfer_id);
        if let Some(id) = id {
            router.finish_outgoing(id);
        }
        if let Some(reason) = audit {
            let source = Path::new(&path);
            let name = source.file_name().map_or_else(|| path.clone(), |name| name.to_string_lossy().into_owned());
            let size = std::fs::metadata(source).ok().map(|meta| meta.len());
            let (kind, detail) = match &result {
                Ok(()) => (audit_kind::FILE_DOWNLOAD_COMPLETED, reason.to_owned()),
                Err(host_core::filexfer::FileError::Rejected) => (audit_kind::FILE_DOWNLOAD_REJECTED, "declined by viewer".to_owned()),
                Err(error) => (audit_kind::FILE_DOWNLOAD_FAILED, format!("{reason}: {error}")),
            };
            audit_hosted(&task_app, AuditEvent::new(kind, None).with_file(name, size).with_path(path.clone()).with_detail(detail));
        }
        if let Err(error) = result {
            sink.fail_before_offer(&error.to_string());
            emit(
                &task_app,
                "file_error",
                FileError {
                    id: id.unwrap_or(0),
                    message: error.to_string(),
                },
            );
        } else if let Some(id) = id {
            emit(&task_app, "file_done", FileDone { id, path: None });
        }
    });
    offer_rx
}

/// The one-shot upload folder a viewer chose with SetUploadDir, or Downloads.
/// An invalid or non-directory path also falls back, so a bad value never
/// drops the transfer.
fn take_upload_dir(app: &AppHandle) -> PathBuf {
    let slot = { app.state::<AppState>().lock().upload_dir.clone() };
    let chosen = lock_unpoisoned(&slot).take();
    match chosen {
        Some(dir) if Path::new(&dir).is_dir() => PathBuf::from(dir),
        _ => downloads_dir(),
    }
}

/// Cancel flags of the folder transfers in flight, by folder id.
fn folder_cancels() -> &'static Mutex<HashMap<u64, Arc<AtomicBool>>> {
    static CANCELS: std::sync::OnceLock<Mutex<HashMap<u64, Arc<AtomicBool>>>> = std::sync::OnceLock::new();
    CANCELS.get_or_init(Mutex::default)
}

/// Stop a folder transfer in either direction. The other side is told, and a
/// partially received folder is deleted.
#[tauri::command]
pub(crate) fn cancel_transfer(id: u64) -> Result<(), String> {
    let flag = lock_unpoisoned(folder_cancels()).get(&id).cloned();
    let flag = flag.ok_or_else(|| "that transfer has already finished".to_owned())?;
    flag.store(true, Ordering::Relaxed);
    Ok(())
}

/// Send `path` as a folder if it is a directory, else as a single file.
pub(crate) fn spawn_outgoing(
    app: AppHandle,
    path: String,
    outgoing: ReliableSender,
    sealer: Arc<Mutex<Sealer>>,
    router: Arc<FileRouter>,
    gate: TransferGate,
    audit: Option<&'static str>,
) -> oneshot::Receiver<Result<u64, String>> {
    if Path::new(&path).is_dir() {
        spawn_outgoing_folder(app, path, outgoing, sealer, router, gate, audit)
    } else {
        spawn_outgoing_file(app, path, outgoing, sealer, router, gate, audit)
    }
}

/// Answer a viewer's Pull on the host: send the file or folder at `path`
/// (gated on file access), and tell the viewer if it could not even start,
/// for example a folder over the size limits.
pub(crate) fn serve_pull(app: &AppHandle, req: u64, path: String, files_allowed: Arc<AtomicBool>) {
    let (outgoing, sealer, router) = {
        let state = app.state::<AppState>();
        let state = state.lock();
        (state.file_outgoing.clone(), state.file_sealer.clone(), state.file_router.clone())
    };
    let (Some(outgoing), Some(sealer), Some(router)) = (outgoing, sealer, router) else { return };
    let started = spawn_outgoing(app.clone(), path, outgoing.clone(), sealer.clone(), router, TransferGate::new(files_allowed), Some("requested by viewer"));
    tauri::async_runtime::spawn(async move {
        let Ok(Err(message)) = started.await else { return };
        let reply = protocol::browse::BrowseMsg::BrowseError { req, message };
        let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Data as u8], &DataMsg::Browse(reply).to_bytes());
        let _ = outgoing.send_frame(Tag::Data, &sealed).await;
    });
}

/// The sink of an outgoing folder: routes the peer's replies for the folder
/// and for each of its files to one channel, and stops routing a file's once
/// the next file is offered (files go one at a time).
struct FolderSink {
    inner: ReliableFileSink,
    router: Arc<FileRouter>,
    replies: mpsc::Sender<FileMsg>,
    current_file: Option<u64>,
}

#[async_trait::async_trait]
impl FileSink for FolderSink {
    async fn send(&mut self, message: FileMsg) {
        match &message {
            FileMsg::FolderOffer { id, .. } => {
                *lock_unpoisoned(&self.inner.transfer_id) = Some(*id);
                self.router.register_outgoing(*id, self.replies.clone());
            }
            FileMsg::FolderFile { id, .. } => {
                if let Some(previous) = self.current_file.replace(*id) {
                    self.router.finish_outgoing(previous);
                }
                self.router.register_outgoing(*id, self.replies.clone());
            }
            _ => {}
        }
        self.inner.send(message).await;
    }
}

impl FolderSink {
    fn finish(&mut self) {
        if let Some(id) = *lock_unpoisoned(&self.inner.transfer_id) {
            self.router.finish_outgoing(id);
        }
        if let Some(id) = self.current_file.take() {
            self.router.finish_outgoing(id);
        }
    }
}

/// Spawn a task that sends the folder at `path`, one consented folder holding
/// ordinary per-file transfers. Resolves to the folder id once its offer is
/// on the wire, or to an error if the folder cannot be sent at all.
pub(crate) fn spawn_outgoing_folder(
    app: AppHandle,
    path: String,
    outgoing: ReliableSender,
    sealer: Arc<Mutex<Sealer>>,
    router: Arc<FileRouter>,
    gate: TransferGate,
    audit: Option<&'static str>,
) -> oneshot::Receiver<Result<u64, String>> {
    use host_core::folder::{FolderProgress, FolderSender, FolderTimeouts};
    let (offer_tx, offer_rx) = oneshot::channel();
    tauri::async_runtime::spawn(async move {
        let root = PathBuf::from(&path);
        let plan = match tauri::async_runtime::spawn_blocking(move || host_core::folder::scan_folder(&root)).await {
            Ok(Ok(plan)) => plan,
            Ok(Err(error)) => {
                let _ = offer_tx.send(Err(error.to_string()));
                return;
            }
            Err(_) => {
                let _ = offer_tx.send(Err("could not read that folder".to_owned()));
                return;
            }
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let gate = gate.with_cancel(cancelled.clone());
        let folder_id = Arc::new(Mutex::new(None));
        let (reply_tx, reply_rx) = mpsc::channel(64);
        let mut replies = ReliableFileSource { incoming: reply_rx };
        let mut sink = FolderSink {
            inner: ReliableFileSink { outgoing, sealer, offer_id: None, transfer_id: folder_id.clone(), replies: None },
            router,
            replies: reply_tx,
            current_file: None,
        };
        let on_offer = move |id| {
            lock_unpoisoned(folder_cancels()).insert(id, cancelled);
            let _ = offer_tx.send(Ok(id));
        };
        let progress_app = app.clone();
        let progress_id = folder_id.clone();
        let progress = move |state: FolderProgress| {
            let Some(id) = *lock_unpoisoned(&progress_id) else { return };
            emit(&progress_app, "file_progress", FileProgress {
                id,
                transferred: state.bytes_done,
                total: state.bytes_total,
                direction: "send",
                files_done: Some(state.files_done),
                files_total: Some(state.files_total),
            });
        };
        let result = FolderSender::send(&plan, &mut sink, &mut replies, on_offer, progress, FolderTimeouts::default(), &gate).await;
        sink.finish();
        let id = *lock_unpoisoned(&folder_id);
        if let Some(id) = id {
            lock_unpoisoned(folder_cancels()).remove(&id);
        }
        if let Some(reason) = audit {
            let files = plan.files.len();
            let (kind, detail) = match &result {
                Ok(_) => (audit_kind::FILE_DOWNLOAD_COMPLETED, format!("folder, {files} files: {reason}")),
                Err(host_core::filexfer::FileError::Rejected) => (audit_kind::FILE_DOWNLOAD_REJECTED, format!("folder, {files} files: declined by viewer")),
                Err(error) => (audit_kind::FILE_DOWNLOAD_FAILED, format!("folder, {files} files: {reason}: {error}")),
            };
            audit_hosted(&app, AuditEvent::new(kind, None).with_file(plan.name.clone(), Some(plan.total)).with_path(path.clone()).with_detail(detail));
        }
        match (result, id) {
            (Ok(_), Some(id)) => emit(&app, "file_done", FileDone { id, path: None }),
            (Err(error), Some(id)) => emit(&app, "file_error", FileError { id, message: error.to_string() }),
            _ => {}
        }
    });
    offer_rx
}

/// Ask the user about one incoming folder and, if accepted, receive the whole
/// tree into a new folder. One prompt covers every file in it.
async fn receive_folder_offer(
    app: AppHandle,
    mut source: host_core::datachan::TransferSource,
    id: u64,
    from: String,
    outgoing: ReliableSender,
    sealer: Arc<Mutex<Sealer>>,
    is_host: bool,
) {
    use host_core::folder::{FolderProgress, FolderReceiver};
    let mut answers = ReliableFileSink {
        outgoing,
        sealer,
        offer_id: None,
        transfer_id: Arc::new(Mutex::new(Some(id))),
        replies: None,
    };
    let dest_dir = take_upload_dir(&app);
    let dest_display = dest_dir.display().to_string();
    let files_allowed = app.state::<AppState>().lock().viewer_permissions.files.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    lock_unpoisoned(folder_cancels()).insert(id, cancelled.clone());
    // The file permission is checked per chunk across the whole folder.
    let gate = TransferGate::new(files_allowed.clone()).with_cancel(cancelled);
    // (name, total size, file count) of the offer, for the logs.
    let offered = Arc::new(Mutex::new((String::new(), 0u64, 0u32)));
    let offer_slot = offered.clone();
    // Folder uploads into this host are audited once for the whole folder.
    let audit_dir = dest_display.clone();
    let audit = move |app: &AppHandle, kind: &str, offered: &(String, u64, u32), detail: Option<&str>| {
        if !is_host { return; }
        let (name, size, files) = offered;
        let detail = match detail {
            Some(detail) => format!("folder, {files} files: {detail}"),
            None => format!("folder, {files} files"),
        };
        let event = AuditEvent::new(kind, None).with_path(audit_dir.clone()).with_file(name.clone(), Some(*size)).with_detail(detail);
        audit_hosted(app, event);
    };
    let consent_audit = audit.clone();
    let pending_app = app.clone();
    let consent = move |offer: FileMsg| async move {
        let FileMsg::FolderOffer { id, name, files, total } = offer else {
            return ConsentDecision::Reject;
        };
        let details = (name.clone(), total, files);
        *lock_unpoisoned(&offer_slot) = details.clone();
        if !files_allowed.load(Ordering::Relaxed) {
            emit_activity(&pending_app, "file", format!("blocked folder \"{name}\""));
            consent_audit(&pending_app, audit_kind::FILE_UPLOAD_REJECTED, &details, Some("file access off"));
            return ConsentDecision::Reject;
        }
        let (decision_tx, decision_rx) = oneshot::channel();
        pending_app.state::<AppState>().lock().pending_file_requests.insert(id, decision_tx);
        startup::reveal_if_hidden(&pending_app);
        emit(&pending_app, "file_offer", FileOffer { id, name, size: total, from, dir: dest_display, files: Some(files) });
        if decision_rx.await.unwrap_or(false) {
            consent_audit(&pending_app, audit_kind::FILE_UPLOAD_ACCEPTED, &details, None);
            ConsentDecision::Accept
        } else {
            consent_audit(&pending_app, audit_kind::FILE_UPLOAD_REJECTED, &details, Some("declined by host user"));
            ConsentDecision::Reject
        }
    };
    let progress_app = app.clone();
    let progress = move |state: FolderProgress| {
        emit(&progress_app, "file_progress", FileProgress {
            id,
            transferred: state.bytes_done,
            total: state.bytes_total,
            direction: "receive",
            files_done: Some(state.files_done),
            files_total: Some(state.files_total),
        });
    };
    let result = FolderReceiver::receive(&mut source, &mut answers, &dest_dir, consent, progress, &gate).await;
    lock_unpoisoned(folder_cancels()).remove(&id);
    let details = lock_unpoisoned(&offered).clone();
    match result {
        Ok(received) => {
            emit_activity(&app, "file", format!("received folder \"{}\" ({} files)", details.0, received.files));
            // The saved name can differ from the offered one when a folder of
            // that name already exists.
            let saved = received.root.file_name().map_or(details.0.clone(), |saved| saved.to_string_lossy().into_owned());
            audit(&app, audit_kind::FILE_UPLOAD_COMPLETED, &(saved, received.bytes, received.files), None);
            emit(&app, "file_done", FileDone { id, path: Some(received.root.display().to_string()) });
        }
        Err(error) => {
            // A refusal was already audited when it was decided.
            if !matches!(error, host_core::filexfer::FileError::Rejected) {
                audit(&app, audit_kind::FILE_UPLOAD_FAILED, &details, Some(&error.to_string()));
            }
            emit(&app, "file_error", FileError { id, message: error.to_string() });
        }
    }
}

/// A directory listing returned to the UI (the local pane of the file manager).
#[derive(serde::Serialize)]
pub(crate) struct Listing {
    pub(crate) path: String,
    pub(crate) entries: Vec<protocol::browse::DirEntry>,
}

/// List a directory on THIS computer (the file manager's local pane). An empty
/// path means the user's home folder.
#[tauri::command]
pub(crate) async fn browse_local(path: String) -> Result<Listing, String> {
    let (path, entries) = tauri::async_runtime::spawn_blocking(move || host_core::browse::list_dir(&path))
        .await
        .map_err(|error| error.to_string())??;
    Ok(Listing { path, entries })
}

/// Ask the host to list a directory for the file manager. The answer arrives as
/// a `remote_listing` event. An empty path means the host's starting folder.
#[tauri::command]
pub(crate) async fn browse_remote(state: tauri::State<'_, AppState>, path: String) -> Result<(), String> {
    let (outgoing, sealer) = {
        let state = state.lock();
        (state.file_outgoing.clone(), state.file_sealer.clone())
    };
    let outgoing = outgoing.ok_or_else(|| "no active file channel".to_owned())?;
    let sealer = sealer.ok_or_else(|| "file session keys are not ready".to_owned())?;
    let req = host_core::filexfer::next_transfer_id();
    let message = protocol::datamsg::DataMsg::Browse(protocol::browse::BrowseMsg::ListDir { req, path });
    let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Data as u8], &message.to_bytes());
    outgoing.send_frame(Tag::Data, &sealed).await.map_err(|error| error.to_string())
}

/// Upload a local file into the remote folder `dir`. Tells the host the
/// destination, then sends the file; both go over the ordered data channel so
/// the destination arrives first. Returns the transfer id.
#[tauri::command]
pub(crate) async fn upload_to_remote(state: tauri::State<'_, AppState>, app: AppHandle, dir: String, path: String) -> Result<u64, String> {
    let (outgoing, sealer, router) = {
        let state = state.lock();
        (state.file_outgoing.clone(), state.file_sealer.clone(), state.file_router.clone())
    };
    let outgoing = outgoing.ok_or_else(|| "no active file channel".to_owned())?;
    let sealer = sealer.ok_or_else(|| "file session keys are not ready".to_owned())?;
    let router = router.ok_or_else(|| "no active file channel".to_owned())?;
    let message = protocol::datamsg::DataMsg::Browse(protocol::browse::BrowseMsg::SetUploadDir { dir });
    let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Data as u8], &message.to_bytes());
    outgoing.send_frame(Tag::Data, &sealed).await.map_err(|error| error.to_string())?;
    spawn_outgoing(app, path, outgoing, sealer, router, TransferGate::open(), None)

        .await
        .map_err(|_| "file transfer stopped before sending its offer".to_owned())?
}

/// Ask the host to send the file at `path`; it arrives as an incoming transfer.
#[tauri::command]
pub(crate) async fn pull_remote(state: tauri::State<'_, AppState>, path: String) -> Result<(), String> {
    let (outgoing, sealer) = {
        let state = state.lock();
        (state.file_outgoing.clone(), state.file_sealer.clone())
    };
    let outgoing = outgoing.ok_or_else(|| "no active file channel".to_owned())?;
    let sealer = sealer.ok_or_else(|| "file session keys are not ready".to_owned())?;
    let req = host_core::filexfer::next_transfer_id();
    let message = protocol::datamsg::DataMsg::Browse(protocol::browse::BrowseMsg::Pull { req, path });
    let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Data as u8], &message.to_bytes());
    outgoing.send_frame(Tag::Data, &sealed).await.map_err(|error| error.to_string())
}

async fn send_browse_action(state: &AppState, message: protocol::browse::BrowseMsg) -> Result<(), String> {
    let (outgoing, sealer) = {
        let state = state.lock();
        (state.file_outgoing.clone(), state.file_sealer.clone())
    };
    let outgoing = outgoing.ok_or_else(|| "no active file channel".to_owned())?;
    let sealer = sealer.ok_or_else(|| "file session keys are not ready".to_owned())?;
    let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Data as u8], &DataMsg::Browse(message).to_bytes());
    outgoing.send_frame(Tag::Data, &sealed).await.map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn make_remote_dir(state: tauri::State<'_, AppState>, path: String, name: String) -> Result<(), String> {
    let req = host_core::filexfer::next_transfer_id();
    send_browse_action(&state, protocol::browse::BrowseMsg::MakeDir { req, path, name }).await
}

#[tauri::command]
pub(crate) async fn delete_remote(state: tauri::State<'_, AppState>, path: String) -> Result<(), String> {
    let req = host_core::filexfer::next_transfer_id();
    send_browse_action(&state, protocol::browse::BrowseMsg::Delete { req, path }).await
}

#[tauri::command]
pub(crate) async fn rename_remote(state: tauri::State<'_, AppState>, path: String, name: String) -> Result<(), String> {
    let req = host_core::filexfer::next_transfer_id();
    send_browse_action(&state, protocol::browse::BrowseMsg::Rename { req, path, name }).await
}

#[tauri::command]
pub(crate) fn respond_file(
    state: tauri::State<'_, AppState>,
    id: u64,
    accept: bool,
) -> Result<(), String> {
    route_file_consent(
        &mut state.lock().pending_file_requests,
        id,
        accept,
    )
}
