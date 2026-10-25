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
) {
    tauri::async_runtime::spawn(async move {
        while let Some(IncomingTransfer { id, source }) = offers.recv().await {
            let (outgoing, sealer) = {
                let state = app.state::<AppState>();
                let state = state.lock();
                (state.file_outgoing.clone(), state.file_sealer.clone())
            };
            let (Some(outgoing), Some(sealer)) = (outgoing, sealer) else { break };
            tauri::async_runtime::spawn(receive_offer(app.clone(), source, id, from.clone(), outgoing, sealer));
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
    // The accepted file's name, captured at consent time so completion can name
    // it in the activity log.
    let received_name = Arc::new(Mutex::new(String::new()));
    let name_slot = received_name.clone();
    // A viewer uploading into a browsed folder sets a one-shot destination;
    // consume it here, else fall back to Downloads. An invalid or non-directory
    // path also falls back, so a bad value never drops the file.
    let dest_dir = {
        let slot = { app.state::<AppState>().lock().upload_dir.clone() };
        let chosen = lock_unpoisoned(&slot).take();
        match chosen {
            Some(dir) if std::path::Path::new(&dir).is_dir() => std::path::PathBuf::from(dir),
            _ => downloads_dir(),
        }
    };
    // Shown in the accept prompt so the host sees where the file would land.
    let dest_display = dest_dir.display().to_string();
    let result = FileReceiver::receive_with_async_consent(
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
                return ConsentDecision::Reject;
            }
            *lock_unpoisoned(&name_slot) = name.clone();
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
                },
            );

            match decision_rx.await.unwrap_or(false) {
                true => ConsentDecision::Accept,
                false => ConsentDecision::Reject,
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
                },
            );
        },
    )
    .await;
    match result {
        Ok(path) => {
            let name = lock_unpoisoned(&received_name).clone();
            if !name.is_empty() { emit_activity(&app, "file", format!("received \"{name}\"")); }
            emit(&app, "file_done", FileDone { id, path: Some(path.display().to_string()) });
        }
        Err(error) => emit(
            &app,
            "file_error",
            FileError {
                id,
                message: error.to_string(),
            },
        ),
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
    let offer_rx = spawn_outgoing_file(app, path, outgoing, sealer, router);
    // FileSender allocates the id when it emits Offer. Returning after that
    // first sealed frame lets the UI track a transfer while it waits for the
    // receiver's answer and streams.
    offer_rx
        .await
        .map_err(|_| "file transfer stopped before sending its offer".to_owned())?
}

/// Spawn a task that sends `path` over the data channel as a file transfer.
/// Shared by the viewer's "Send file" and by answering a remote pull. Resolves
/// to the transfer id once the offer is on the wire.
pub(crate) fn spawn_outgoing_file(
    app: AppHandle,
    path: String,
    outgoing: ReliableSender,
    sealer: Arc<Mutex<Sealer>>,
    router: Arc<FileRouter>,
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
        let result = FileSender::send(Path::new(&path), &mut sink, &mut replies, move |transferred, total| {
            if let Some(id) = *lock_unpoisoned(&progress_id) {
                emit(
                    &progress_app,
                    "file_progress",
                    FileProgress {
                        id,
                        transferred,
                        total,
                        direction: "send",
                    },
                );
            }
        })
        .await;
        let id = *lock_unpoisoned(&transfer_id);
        if let Some(id) = id {
            router.finish_outgoing(id);
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
    spawn_outgoing_file(app, path, outgoing, sealer, router)
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
