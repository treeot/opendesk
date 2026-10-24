mod config;
mod contacts;
mod permissions;
mod unattended;
mod events;
mod recording;
mod files;
mod host;
mod machine;
mod menu;
mod tunnel;
mod startup;
mod vdriver;
mod windows_service;
#[cfg(windows)]
pub use windows_service::run_windows_service;
mod viewer;

use config::ConfigStore;
use contacts::{Contact, ContactStore};
use futures_util::StreamExt;
use host_core::{clipboard::{ArboardBackend, ClipboardSync}, datachan::{FileRouter, IncomingTransfer}, filexfer::{ConsentDecision, FileReceiver, FileSink, FileSource, FileSender}, roles::DataHandle, transport::reliable::ReliableSender};
use protocol::{chat::ChatMsg, control::{Control, MonitorInfo}, crypto::{Opener, Sealer}, datamsg::DataMsg, filexfer::FileMsg, frame, Tag};
use serde::Serialize;
use std::{collections::HashMap, fs, fs::OpenOptions, io, path::{Path, PathBuf}, sync::{atomic::{AtomicBool, Ordering}, mpsc as std_mpsc, Arc, Mutex}, time::{Duration, Instant}};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, oneshot};
pub(crate) use events::*;
pub(crate) use recording::*;
pub(crate) use files::*;
pub(crate) use host::*;
pub(crate) use viewer::*;

struct RuntimeState {
    config: ConfigStore,
    contacts: ContactStore,
    password: Option<String>,
    host_thread: Option<HostThread>,
    /// Set while a `start_hosting` registration task is connecting/waiting for
    /// a pairing, before its `HostThread` exists. Together with `host_thread`
    /// this makes the hosting slot reservation atomic under one lock.
    host_starting: Option<HostReservation>,
    viewer_task: Option<tauri::async_runtime::JoinHandle<()>>,
    viewer_stop: Option<Arc<AtomicBool>>,
    viewer_input: Option<mpsc::Sender<Vec<u8>>>,
    pending_request: Option<oneshot::Sender<bool>>,
    pending_file_requests: HashMap<u64, oneshot::Sender<bool>>,
    file_outgoing: Option<ReliableSender>,
    file_sealer: Option<Arc<Mutex<Sealer>>>,
    control_sender: Option<ReliableSender>,
    control_sealer: Option<Arc<Mutex<Sealer>>>,
    /// Routes the session's inbound file messages to their transfers.
    file_router: Option<Arc<FileRouter>>,
    /// Destination for the next uploaded file (the folder the viewer is
    /// browsing), consumed once; `None` means save to Downloads.
    upload_dir: Arc<Mutex<Option<String>>>,
    clipboard_enabled: Arc<AtomicBool>,
    audio_muted: Arc<AtomicBool>,
    clipboard_thread: Option<ClipboardThread>,
    /// Relays currently refusing this device's ID; see `host::identity_conflict`.
    identity_conflicts: Vec<IdentityConflict>,
    /// Host-enforced permissions for the currently connected viewer. Shared with
    /// the session paths so flipping one takes effect live. Reset to all-on when
    /// a session starts.
    viewer_permissions: ViewerPermissions,
    /// Ends the current host session (without stopping hosting) when fired; see
    /// `disconnect_viewer`. Present only while a session is active.
    disconnect_viewer: Option<oneshot::Sender<()>>,
}

/// The four host-enforced capabilities a connected viewer may use. Each is a
/// cheaply shared atomic so the session paths (input, audio, clipboard, files)
/// can read it without locking `AppState`. All default to enabled.
#[derive(Clone)]
pub(crate) struct ViewerPermissions {
    pub(crate) control: Arc<AtomicBool>,
    pub(crate) files: Arc<AtomicBool>,
    pub(crate) clipboard: Arc<AtomicBool>,
    pub(crate) audio: Arc<AtomicBool>,
    /// Let the viewer forward a local TCP port to a loopback port on this host
    /// (e.g. reach Ollama). Off by default and each session: it opens a network
    /// path into the host, so the host user turns it on deliberately.
    pub(crate) forwarding: Arc<AtomicBool>,
}

impl ViewerPermissions {
    fn new() -> Self {
        Self {
            control: Arc::new(AtomicBool::new(true)),
            files: Arc::new(AtomicBool::new(true)),
            clipboard: Arc::new(AtomicBool::new(true)),
            audio: Arc::new(AtomicBool::new(true)),
            forwarding: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Back to defaults for a new session: everything on except forwarding.
    pub(crate) fn reset(&self) {
        for flag in [&self.control, &self.files, &self.clipboard, &self.audio] {
            flag.store(true, Ordering::Relaxed);
        }
        self.forwarding.store(false, Ordering::Relaxed);
    }

    /// The atomic for one capability name, or `None` if the name is unknown.
    pub(crate) fn flag(&self, capability: &str) -> Option<&Arc<AtomicBool>> {
        match capability {
            "control" => Some(&self.control),
            "files" => Some(&self.files),
            "clipboard" => Some(&self.clipboard),
            "audio" => Some(&self.audio),
            "forwarding" => Some(&self.forwarding),
            _ => None,
        }
    }

    pub(crate) fn snapshot(&self) -> ViewerPermissionsEvent {
        ViewerPermissionsEvent {
            control: self.control.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
            clipboard: self.clipboard.load(Ordering::Relaxed),
            audio: self.audio.load(Ordering::Relaxed),
            forwarding: self.forwarding.load(Ordering::Relaxed),
        }
    }
}

/// Each host session runs with its own current-thread Tokio runtime on a
/// dedicated OS thread (input injection has its own thread inside host-core;
/// see `roles.rs`). The stop
/// signal is separate so Tauri commands can shut it down without aborting a
/// task that may be running non-`Send` code. The thread is the hosting loop
/// (see `spawn_host_thread`): it re-registers after each session until the
/// stop signal fires or its sender is dropped.
struct HostThread {
    stop_tx: oneshot::Sender<()>,
    join: std::thread::JoinHandle<()>,
    /// Identifies which `start_hosting` call owns this thread so a finishing
    /// thread only clears its own handle, never a newer one.
    token: Arc<()>,
}

/// Marker for an in-flight `start_hosting` call. `token` is compared with
/// `Arc::ptr_eq` (mirroring the viewer slot's `viewer_stop` marker) so a stale
/// registration task cannot install or release a newer reservation.
struct HostReservation {
    token: Arc<()>,
    task: Option<tauri::async_runtime::JoinHandle<()>>,
}
struct ClipboardThread {
    command_tx: std_mpsc::Sender<ClipboardCommand>,
    join: std::thread::JoinHandle<()>,
}
enum ClipboardCommand {
    Apply(String),
    Stop,
}
struct AppState(Mutex<RuntimeState>);

impl AppState {
    fn lock(&self) -> std::sync::MutexGuard<'_, RuntimeState> {
        lock_unpoisoned(&self.0)
    }
}

/// Poison-tolerant lock: a panic while another holder had the mutex must not
/// cascade into every later command or background task.
fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

trait AbortableTask {
    fn abort(&self);
}

impl<T> AbortableTask for tauri::async_runtime::JoinHandle<T> {
    fn abort(&self) {
        self.abort();
    }
}

struct AbortOnDrop<T: AbortableTask>(Option<T>);

impl<T: AbortableTask> AbortOnDrop<T> {
    fn new(task: T) -> Self {
        Self(Some(task))
    }
}

impl<T: AbortableTask> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

impl<T> AbortOnDrop<tauri::async_runtime::JoinHandle<T>> {
    async fn join(&mut self) -> Result<T, tauri::Error> {
        self.0.as_mut().expect("task already joined").await
    }
}

fn spawn_clipboard_thread(
    outgoing: ReliableSender,
    sealer: Arc<Mutex<Sealer>>,
    enabled: Arc<AtomicBool>,
    allowed: Arc<AtomicBool>,
) -> Option<(std_mpsc::Sender<ClipboardCommand>, std::thread::JoinHandle<()>)> {
    let backend = ArboardBackend::new()?;
    let (command_tx, command_rx) = std_mpsc::channel();
    // Clipboard moves only when the user's sync preference is on AND the host
    // hasn't blocked the viewer's clipboard permission, in both directions.
    let active = move || enabled.load(Ordering::Relaxed) && allowed.load(Ordering::Relaxed);
    let join = std::thread::spawn(move || {
        let mut sync = ClipboardSync::new(backend);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
        let Ok(runtime) = runtime else { return };
        loop {
            match command_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(ClipboardCommand::Apply(text)) => {
                    if active() { sync.apply(&text); }
                }
                Ok(ClipboardCommand::Stop) | Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    if !active() { continue; }
                    let Some(message) = sync.poll_data() else { continue };
                    let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Data as u8], &message.to_bytes());
                    if runtime.block_on(outgoing.send_frame(Tag::Data, &sealed)).is_err() { break; }
                }
            }
        }
    });
    Some((command_tx, join))
}

/// Dispatch the session's inbound Data channel for the whole session. It
/// never stops on an unexpected message: ending it would drop the reliable
/// reader's Data queue and with it the session.
#[allow(clippy::too_many_arguments)]
fn dispatch_data(
    app: AppHandle,
    incoming: mpsc::Receiver<Vec<u8>>,
    opener: Arc<Mutex<Opener>>,
    clipboard_tx: Option<std_mpsc::Sender<ClipboardCommand>>,
    data_sender: ReliableSender,
    data_sealer: Arc<Mutex<Sealer>>,
    files_allowed: Arc<std::sync::atomic::AtomicBool>,
    is_host: bool,
) -> (Arc<FileRouter>, mpsc::Receiver<IncomingTransfer>) {
    let (router, offers) = FileRouter::new();
    let clipboard_app = app.clone();
    let chat_app = app.clone();
    let browse_app = app.clone();
    let tunnel_manager = app.state::<Arc<tunnel::TunnelManager>>().inner().clone();
    let forwarding = app.state::<AppState>().lock().viewer_permissions.forwarding.clone();
    tauri::async_runtime::spawn(host_core::datachan::dispatch_data(
        incoming,
        move |sealed: &[u8]| lock_unpoisoned(&opener).open(&[Tag::Data as u8], sealed),
        Arc::clone(&router),
        move |message| {
            emit_activity(&clipboard_app, "clipboard", "clipboard received");
            if let Some(sender) = &clipboard_tx { let _ = sender.send(ClipboardCommand::Apply(message.text)); }
        },
        move |message| emit(&chat_app, "chat_message", ChatMessage { text: message.text }),
        move |message| handle_browse(&browse_app, &data_sender, &data_sealer, &files_allowed, is_host, message),
        move |message| tunnel::handle(&tunnel_manager, is_host, &forwarding, message),
    ));
    (router, offers)
}

/// Handle one browse message. On the host we list the directory and reply (if
/// file access is on); on the viewer we surface the listing to the UI. The two
/// message kinds never overlap between roles, so one handler serves both.
fn handle_browse(
    app: &AppHandle,
    sender: &ReliableSender,
    sealer: &Arc<Mutex<Sealer>>,
    files_allowed: &Arc<std::sync::atomic::AtomicBool>,
    is_host: bool,
    message: protocol::browse::BrowseMsg,
) {
    use protocol::browse::BrowseMsg;
    use protocol::datamsg::DataMsg;
    // Request kinds (ListDir/Pull/SetUploadDir) only act on the host; reply kinds
    // (DirListing/BrowseError) only on the viewer. Enforce it so a malicious peer
    // cannot drive the wrong half, e.g. a host making a viewer list or send its
    // own files.
    let for_host = matches!(message, BrowseMsg::ListDir { .. } | BrowseMsg::Pull { .. } | BrowseMsg::SetUploadDir { .. });
    if for_host != is_host { return; }
    match message {
        BrowseMsg::ListDir { req, path } => {
            // Logged so the host can see which folders the viewer browsed.
            emit_activity(app, "files", format!("browsed \"{path}\""));
            let allowed = files_allowed.load(std::sync::atomic::Ordering::Relaxed);
            let sender = sender.clone();
            let sealer = sealer.clone();
            tauri::async_runtime::spawn(async move {
                let reply = if !allowed {
                    BrowseMsg::BrowseError { req, message: "The other computer turned off file access.".into() }
                } else {
                    match tauri::async_runtime::spawn_blocking(move || host_core::browse::list_dir(&path)).await {
                        Ok(Ok((shown, entries))) => BrowseMsg::DirListing { req, path: shown, entries },
                        Ok(Err(error)) => BrowseMsg::BrowseError { req, message: error },
                        Err(_) => BrowseMsg::BrowseError { req, message: "could not read that folder".into() },
                    }
                };
                let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Data as u8], &DataMsg::Browse(reply).to_bytes());
                let _ = sender.send_frame(Tag::Data, &sealed).await;
            });
        }
        BrowseMsg::DirListing { path, entries, .. } => emit(app, "remote_listing", RemoteListingEvent { path, entries, error: None }),
        BrowseMsg::BrowseError { message, .. } => emit(app, "remote_listing", RemoteListingEvent { path: String::new(), entries: Vec::new(), error: Some(message) }),
        BrowseMsg::Pull { path, .. } => {
            // Host side: the viewer asked for this file; send it if file access
            // is on. It reaches the viewer as a normal incoming transfer.
            if !files_allowed.load(std::sync::atomic::Ordering::Relaxed) { return; }
            // Logged so the host can see which files the viewer pulled.
            emit_activity(app, "files", format!("sent \"{path}\""));
            let (outgoing, sealer, router) = {
                let state = app.state::<AppState>();
                let state = state.lock();
                (state.file_outgoing.clone(), state.file_sealer.clone(), state.file_router.clone())
            };
            if let (Some(outgoing), Some(sealer), Some(router)) = (outgoing, sealer, router) {
                // The send runs in its own task; we don't need the offer-id future.
                drop(files::spawn_outgoing_file(app.clone(), path, outgoing, sealer, router));
            }
        }
        BrowseMsg::SetUploadDir { dir } => {
            // Host side: the next incoming file goes here instead of Downloads.
            // Gated on file access, and the chosen directory is shown to the host
            // in the accept prompt (see the FileOffer event), so a viewer cannot
            // silently redirect a file into an autostart or persistence location.
            if !files_allowed.load(std::sync::atomic::Ordering::Relaxed) { return; }
            let slot = { app.state::<AppState>().lock().upload_dir.clone() };
            *lock_unpoisoned(&slot) = if dir.is_empty() { None } else { Some(dir) };
        }
    }
}

fn activate_data_channel(app: &AppHandle, handle: DataHandle, from: String, is_host: bool) {
    let DataHandle { sender, receiver, sealer, opener, control_sender, control_sealer, .. } = handle;
    let (enabled, clipboard_allowed) = {
        let state = app.state::<AppState>();
        let state = state.lock();
        (state.clipboard_enabled.clone(), state.viewer_permissions.clipboard.clone())
    };
    let files_allowed = { app.state::<AppState>().lock().viewer_permissions.files.clone() };
    let sealer = Arc::new(Mutex::new(sealer));
    // Port forwarding shares the sealed data channel; bind it for this session.
    app.state::<Arc<tunnel::TunnelManager>>().activate(sender.clone(), sealer.clone());
    let clipboard_thread = spawn_clipboard_thread(sender.clone(), sealer.clone(), enabled, clipboard_allowed);
    let clipboard_tx = clipboard_thread.as_ref().map(|(tx, _)| tx.clone());
    let (file_router, offers) = dispatch_data(app.clone(), receiver, Arc::new(Mutex::new(opener)), clipboard_tx, sender.clone(), sealer.clone(), files_allowed, is_host);
    let state = app.state::<AppState>();
    let mut state = state.lock();
    state.file_outgoing = Some(sender);
    state.file_sealer = Some(sealer);
    state.control_sender = Some(control_sender);
    state.control_sealer = Some(control_sealer);
    state.file_router = Some(file_router);
    state.clipboard_thread = clipboard_thread.map(|(command_tx, join)| ClipboardThread { command_tx, join });
    drop(state);
    spawn_file_receiver(app.clone(), offers, from);
}

fn clear_data_channel(app: &AppHandle) {
    // Stop any port forwards and tear down tunnel streams for the ended session.
    app.state::<Arc<tunnel::TunnelManager>>().deactivate();
    let state = app.state::<AppState>();
    // Take the thread handle out under the lock, then release the global state
    // lock BEFORE joining. The clipboard thread may be mid `block_on(send_frame)`
    // when it observes Stop; joining while holding the AppState mutex would
    // freeze every other Tauri command behind a network-dependent join.
    let thread = {
        let mut state = state.lock();
        let thread = state.clipboard_thread.take();
        state.file_outgoing = None;
        state.file_sealer = None;
        state.control_sender = None;
        state.control_sealer = None;
        state.file_router = None;
        // Back to the user's saved preference for the next session.
        let saved = state.config.config.clipboard_sync;
        state.clipboard_enabled.store(saved, Ordering::Relaxed);
        state.pending_file_requests.clear();
        thread
    };
    if let Some(thread) = thread {
        let _ = thread.command_tx.send(ClipboardCommand::Stop);
        let _ = thread.join.join();
    }
}
fn fresh_password() -> String {
    // 8 digits (no leading zero): ~90M values, so online guessing against the
    // host's rate limit and backoff is ~100x harder than a 6-digit PIN while
    // staying easy to read aloud for attended support.
    rand::random_range(10_000_000..=99_999_999).to_string()
}
fn current_password(state: &mut RuntimeState) -> String {
    state.password.get_or_insert_with(fresh_password).clone()
}

#[tauri::command]
fn get_identity(state: tauri::State<'_, AppState>) -> Identity {
    let mut state = state.lock();
    let id = state.config.ensure_identity();
    Identity {
        id,
        password: current_password(&mut state),
    }
}
#[tauri::command]
fn rotate_password(app: AppHandle) -> PasswordResponse {
    PasswordResponse { password: rotate_session_password(&app) }
}
/// Replace the one-time session password and tell the UI through the
/// `password_rotated` event (payload `{ password }`), so automatic rotation
/// after a session or failed logins shows up without a refresh.
fn rotate_session_password(app: &AppHandle) -> String {
    let password = fresh_password();
    app.state::<AppState>().lock().password = Some(password.clone());
    emit(app, "password_rotated", PasswordResponse { password: password.clone() });
    password
}
#[tauri::command]
fn get_config(state: tauri::State<'_, AppState>) -> AppConfig {
    let state = state.lock();
    AppConfig {
        relay_url: state.config.config.relay_url.clone(),
        relay_urls: state.config.config.relay_urls.clone(),
        recent_ids: state.config.config.recent_ids.clone(),
        clipboard_sync: state.config.config.clipboard_sync,
        allow_virtual_displays: state.config.config.allow_virtual_displays,
    }
}
#[tauri::command]
fn list_contacts(state: tauri::State<'_, AppState>) -> Vec<Contact> {
    state.lock().contacts.list()
}
#[tauri::command]
fn list_allowed(state: tauri::State<'_, AppState>) -> Vec<String> {
    state.lock().config.config.allow_list.clone()
}
#[tauri::command]
fn add_allowed(state: tauri::State<'_, AppState>, peer_id: String) -> Result<(), String> {
    if peer_id.len() != 9 || !peer_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("peer ID must be 9 digits".into());
    }
    state.lock().config.add_allow(&peer_id).map_err(|error| error.to_string())
}
#[tauri::command]
fn remove_allowed(state: tauri::State<'_, AppState>, peer_id: String) -> Result<(), String> {
    state.lock().config.remove_allow(&peer_id).map_err(|error| error.to_string())
}
#[tauri::command]
fn add_contact(
    state: tauri::State<'_, AppState>,
    alias: String,
    peer_id: String,
) -> Result<Contact, String> {
    state.lock().contacts.add(alias, peer_id).map_err(|e| e.to_string())
}
#[tauri::command]
fn remove_contact(state: tauri::State<'_, AppState>, peer_id: String) -> Result<(), String> {
    state.lock().contacts.remove(&peer_id).map_err(|e| e.to_string())
}
#[tauri::command]
fn touch_contact(state: tauri::State<'_, AppState>, peer_id: String) -> Result<(), String> {
    state.lock().contacts.touch(&peer_id).map_err(|e| e.to_string())
}
#[tauri::command]
fn set_relay(state: tauri::State<'_, AppState>, app: AppHandle, url: String) -> Result<(), String> {
    let url = url.trim().to_owned();
    if url.is_empty() {
        return Err("relay URL cannot be empty".into());
    }
    state.lock().config.set_relays(vec![url.clone()])?;
    prune_identity_conflicts(&app, Some(&[url]));
    notify_relays_changed();
    Ok(())
}
#[tauri::command]
fn set_relays(state: tauri::State<'_, AppState>, app: AppHandle, urls: Vec<String>) -> Result<(), String> {
    let urls: Vec<String> = urls.into_iter().map(|url| url.trim().to_owned()).filter(|url| !url.is_empty()).collect();
    state.lock().config.set_relays(urls.clone())?;
    prune_identity_conflicts(&app, Some(&urls));
    notify_relays_changed();
    Ok(())
}
#[tauri::command]
fn list_relays(state: tauri::State<'_, AppState>) -> Vec<String> {
    state.lock().config.config.relay_urls.clone()
}
#[tauri::command]
fn get_unattended_status(state: tauri::State<'_, AppState>) -> UnattendedStatus {
    unattended_status(&state.lock().config.config)
}

#[tauri::command]
fn windows_service_status() -> windows_service::ServiceStatus { windows_service::status() }

#[tauri::command]
fn install_windows_service(app: AppHandle) -> Result<windows_service::ServiceStatus, String> {
    let data_dir = app.path().app_data_dir().map_err(|error| error.to_string())?;
    #[cfg(windows)]
    unattended::stage_machine_store(&data_dir)?;
    windows_service::install(&data_dir)?;
    windows_service::start()
}

#[tauri::command]
fn uninstall_windows_service() -> Result<windows_service::ServiceStatus, String> {
    let status = windows_service::uninstall()?;
    #[cfg(windows)]
    unattended::clear_machine_store();
    Ok(status)
}

fn unattended_status(config: &config::Config) -> UnattendedStatus {
    let check = unattended::check_config(config);
    UnattendedStatus {
        enabled: config.unattended_enabled,
        has_password: check.has_password,
        twofa_enabled: config.unattended_twofa_enabled,
        ready: check.ready(),
        problem: check.problem,
    }
}

fn random_base32_secret() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bytes = [0u8; 20];
    rand::fill(&mut bytes);
    let mut output = String::with_capacity(32);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            output.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
            buffer &= (1 << bits) - 1;
        }
    }
    if bits > 0 { output.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char); }
    output
}

/// Enabling starts an enrollment: the secret stays in memory until
/// `confirm_unattended_2fa` checks a code from the authenticator. Asking again
/// while one is pending returns the same secret instead of a new one.
#[tauri::command]
fn set_unattended_2fa(
    state: tauri::State<'_, AppState>,
    pending: tauri::State<'_, unattended::PendingTotp>,
    enabled: bool,
) -> Result<Option<TotpEnrollment>, String> {
    let mut state = state.lock();
    let mut pending = pending.lock();
    if enabled {
        if !state.config.config.unattended_enabled {
            return Err("turn on unattended access before adding two-factor".into());
        }
        if unattended::twofa_active(&unattended::Keyring, &state.config.config) {
            return Err("two-factor is already enabled".into());
        }
        let secret = pending.get_or_insert_with(random_base32_secret).clone();
        let uri = unattended::otpauth_uri(&state.config.ensure_identity(), &secret);
        let (qr_width, qr_modules) = unattended::qr_modules(&uri)?;
        Ok(Some(TotpEnrollment { uri, secret, qr_width, qr_modules }))
    } else {
        *pending = None;
        unattended::disable_twofa(&unattended::Keyring, &mut state.config.config)?;
        state.config.save().map_err(|error| error.to_string())?;
        Ok(None)
    }
}
#[tauri::command]
fn confirm_unattended_2fa(
    state: tauri::State<'_, AppState>,
    pending: tauri::State<'_, unattended::PendingTotp>,
    code: String,
) -> Result<UnattendedStatus, String> {
    let mut state = state.lock();
    let mut pending = pending.lock();
    let secret = pending.clone().ok_or("no two-factor setup is in progress")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs();
    if !host_core::roles::totp_is_valid(&secret, code.trim(), now) {
        return Err("invalid 2FA code".into());
    }
    unattended::enable_twofa(&unattended::Keyring, &mut state.config.config, &secret)?;
    state.config.save().map_err(|error| error.to_string())?;
    *pending = None;
    Ok(unattended_status(&state.config.config))
}
/// Unattended access skips the approval prompt, so its password is the only
/// barrier against online guessing.
const MIN_UNATTENDED_PASSWORD_LEN: usize = 12;

/// Very common passwords (lowercase) long enough to pass the length rule but
/// among the first guesses of any dictionary attack.
const COMMON_PASSWORDS: &[&str] = &[
    "123456789012", "1234567890123", "12345678901234", "123456789abc", "1234567890qwerty",
    "qwertyuiop12", "qwertyuiop123", "qwertyuiopasdf", "qwertyuiopasdfgh", "1q2w3e4r5t6y",
    "1q2w3e4r5t6y7u", "q1w2e3r4t5y6", "1qaz2wsx3edc", "1qaz2wsx3edc4rfv", "zaq12wsxcde3",
    "qazwsxedcrfv", "qazwsxedcrfvtgb", "asdfghjkl123", "zxcvbnm12345", "zxcvbnm123456",
    "abcdefghijkl", "abcdefghijklm", "abcdefghijklmnop", "abc123abc123", "abcd1234abcd",
    "abcd12345678", "password1234", "password12345", "password123456", "password1234!",
    "password123!", "passwordpassword", "mypassword123", "mypassword1234", "p@ssw0rd1234",
    "p@ssword1234", "passw0rd1234", "letmein12345", "letmein123456", "welcome12345",
    "welcome123456", "welcome2024!", "welcome2025!", "welcome2026!", "iloveyou1234",
    "iloveyou12345", "iloveyouiloveyou", "trustno1trustno1", "changeme1234", "changeme12345",
    "administrator", "administrator1", "admin1234567", "admin12345678", "adminadmin123",
    "rootroot1234", "superman1234", "batman123456", "dragon123456", "football1234",
    "baseball1234", "basketball12", "basketball123", "monkey123456", "sunshine1234",
    "princess1234", "starwars1234", "pokemon12345", "master123456", "shadow123456",
    "michael12345", "jennifer1234", "computer1234", "internet1234", "opendesk1234",
    "opendesk12345", "remotedesktop", "remoteaccess1", "remoteaccess123", "unattended123",
    "qwerty123456", "qwerty1234567", "qwertyqwerty", "qwertyqwerty1", "1234qwerasdf",
    "1234qwerasdfzxcv", "asdfasdfasdf", "asdf1234asdf", "11223344556677", "112233445566",
    "123123123123", "123321123321", "121212121212", "147258369147", "159753159753",
    "789456123789", "987654321987", "0987654321ab", "aaaaaa111111", "abc123456789",
    "football12345", "letmeinletmein", "hello1234567", "secret123456",
];

/// Whether every character is the same (`aaaaaaaaaaaa`).
fn is_single_repeated_char(password: &str) -> bool {
    let mut chars = password.chars();
    chars.next().is_some_and(|first| chars.all(|c| c == first))
}

/// Whether the password is a run of digits counting up or down by one
/// (`123456789012`, `987654321098`), wrapping between 9 and 0.
fn is_sequential_digits(password: &str) -> bool {
    let digits: Vec<u8> = password.bytes().collect();
    if digits.len() < 2 || !digits.iter().all(u8::is_ascii_digit) {
        return false;
    }
    let step = |delta: u8| digits.windows(2).all(|pair| (pair[0] - b'0' + delta) % 10 == pair[1] - b'0');
    step(1) || step(9)
}

fn validate_unattended_password(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_UNATTENDED_PASSWORD_LEN {
        return Err(format!("unattended access password must be at least {MIN_UNATTENDED_PASSWORD_LEN} characters"));
    }
    let lower = password.to_lowercase();
    if is_single_repeated_char(password) || is_sequential_digits(password) || COMMON_PASSWORDS.contains(&lower.as_str()) {
        return Err("this unattended access password is too common or predictable; choose a less guessable one".into());
    }
    Ok(())
}

#[tauri::command]
fn set_unattended_access(
    state: tauri::State<'_, AppState>,
    enabled: bool,
    password: Option<String>,
) -> Result<UnattendedStatus, String> {
    let mut state = state.lock();
    // A blank field keeps the stored password (the UI says "enter a new
    // password to replace it"); disabling is how access is turned off.
    let password = password.filter(|value| !value.is_empty());
    if let Some(password) = &password {
        validate_unattended_password(password)?;
    }
    unattended::set_access(&unattended::Keyring, &mut state.config.config, enabled, password.as_deref())?;
    state.config.save().map_err(|error| error.to_string())?;
    Ok(unattended_status(&state.config.config))
}
#[tauri::command]
fn permission_status() -> PermissionStatus {
    PermissionStatus {
        screen_recording: permissions::screen_recording(),
        accessibility: permissions::accessibility(),
    }
}
#[tauri::command]
fn open_permission_settings(which: String) -> Result<(), String> {
    permissions::open_settings(&which)
}
#[tauri::command]
fn set_clipboard_sync(state: tauri::State<'_, AppState>, enabled: bool) {
    let mut state = state.lock();
    state.clipboard_enabled.store(enabled, Ordering::Relaxed);
    // The live toggle applies even if saving the preference fails.
    if let Err(error) = state.config.set_clipboard_sync(enabled) {
        log::warn!("could not save clipboard sync preference: {error}");
    }
}
#[tauri::command]
async fn send_chat(state: tauri::State<'_, AppState>, text: String) -> Result<(), String> {
    let (outgoing, sealer) = {
        let state = state.lock();
        (state.file_outgoing.clone(), state.file_sealer.clone())
    };
    let outgoing = outgoing.ok_or_else(|| "no active session".to_owned())?;
    let sealer = sealer.ok_or_else(|| "no active session".to_owned())?;
    let plaintext = DataMsg::Chat(ChatMsg { text }).to_bytes();
    let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Data as u8], &plaintext);
    outgoing.send_frame(Tag::Data, &sealed).await.map_err(|error| error.to_string())
}

/// Applies from the next session on.
#[tauri::command]
fn set_allow_virtual_displays(state: tauri::State<'_, AppState>, enabled: bool) -> Result<(), String> {
    state.lock().config.set_allow_virtual_displays(enabled).map_err(|error| error.to_string())
}

#[tauri::command]
fn set_audio_muted(state: tauri::State<'_, AppState>, muted: bool) {
    state.lock().audio_muted.store(muted, Ordering::Relaxed);
}

/// The current host-enforced viewer permissions.
#[tauri::command]
fn get_viewer_permissions(state: tauri::State<'_, AppState>) -> ViewerPermissionsEvent {
    state.lock().viewer_permissions.snapshot()
}

/// Flip one host-enforced viewer permission. Takes effect live on the running
/// session, emits `viewer_permissions` so the UI stays in sync, and logs the
/// change to the session activity log.
#[tauri::command]
fn set_viewer_permission(
    state: tauri::State<'_, AppState>,
    app: AppHandle,
    capability: String,
    allowed: bool,
) -> Result<(), String> {
    let snapshot = {
        let state = state.lock();
        let flag = state
            .viewer_permissions
            .flag(&capability)
            .ok_or_else(|| format!("unknown capability: {capability}"))?;
        flag.store(allowed, Ordering::Relaxed);
        state.viewer_permissions.snapshot()
    };
    emit(&app, "viewer_permissions", snapshot);
    emit_activity(&app, "permission", format!("{capability} {}", if allowed { "allowed" } else { "blocked" }));
    Ok(())
}

/// End the CURRENT host session without stopping hosting: the host thread loops
/// back, re-registers, and awaits a new viewer. A no-op if no session is active.
#[tauri::command]
fn disconnect_viewer(state: tauri::State<'_, AppState>) {
    if let Some(tx) = state.lock().disconnect_viewer.take() {
        let _ = tx.send(());
    }
}

#[tauri::command]
fn disconnect(state: tauri::State<'_, AppState>, app: AppHandle) {
    let (host_thread, viewer_task, host_starting) = {
        let mut s = state.lock();
        s.viewer_input = None;
        if let Some(stop) = &s.viewer_stop { stop.store(true, Ordering::Relaxed); }
        s.viewer_stop = None;
        (s.host_thread.take(), s.viewer_task.take(), s.host_starting.take())
    };
    if let Some(viewer_task) = viewer_task {
        // Best-effort Control::Bye so the host ends its session cleanly, then
        // abort. Done off the command thread so disconnect never stalls the UI;
        // bounded to ~200 ms so a wedged writer can't keep the task alive.
        let (sender, sealer) = { let s = state.lock(); (s.control_sender.clone(), s.control_sealer.clone()) };
        tauri::async_runtime::spawn(async move {
            if let (Some(sender), Some(sealer)) = (sender, sealer) {
                let bye = Control::Bye { session: String::new() }.to_bytes();
                let sealed = sealer.lock().unwrap_or_else(|error| error.into_inner()).seal(&[Tag::Control as u8], &bye);
                let sent = tokio::time::timeout(Duration::from_millis(150), sender.send_frame(Tag::Control, &sealed)).await;
                if matches!(sent, Ok(Ok(_))) {
                    // Let the WebSocket writer flush before the abort tears it down.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            viewer_task.abort();
        });
    }
    // Cancel an in-flight registration too, or the reservation would block
    // every later start_hosting call while the task waits on the relay.
    if let Some(task) = host_starting.and_then(|reservation| reservation.task) {
        task.abort();
    }
    if let Some(host_thread) = host_thread {
        let _ = host_thread.stop_tx.send(());
        let _ = host_thread.join.join();
    }
    clear_data_channel(&app);
    emit(
        &app,
        "session_ended",
        SessionEnded {
            reason: "disconnected".into(),
        },
    );
    emit_host_status(&app, "offline");
}

/// The web UI drives the Session menu: enabled while a session is live, with
/// the clipboard-sync check and the Record label kept in step.
#[tauri::command]
fn set_session_menu(app: tauri::AppHandle, active: bool, clipboard_sync: bool, recording: bool) {
    menu::set_session(&app, active, clipboard_sync, recording);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(windows)]
    if windows_service::is_service_host_process() {
        std::env::set_var("OPENDESK_SERVICE_HOST", "1");
        if let Some(data_dir) = windows_service::data_dir() {
            std::env::set_var("OPENDESK_DATA_DIR", data_dir);
        }
    }
    // The elevated copy that installs the virtual display driver (Windows)
    // does only that, before anything else starts.
    if let Some(code) = vdriver::run_helper(&std::env::args_os().skip(1).collect::<Vec<_>>()) {
        std::process::exit(code);
    }
    let background = startup::launched_in_background(std::env::args_os().skip(1));
    let app = tauri::Builder::default()
        // Must be registered first. A second launch would register the same
        // device ID and fight the running host, so it just brings the
        // existing window forward and exits.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| startup::show_main_window(app)))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // Checks the manifest in `plugins.updater` (tauri.conf.json); the
        // frontend decides when to install. See docs/RELEASE.md.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        // The sign-in entry (a LaunchAgent on macOS, the Run key on Windows,
        // an XDG autostart file on Linux) launches with BACKGROUND_ARG.
        .plugin(tauri_plugin_autostart::Builder::new().arg(startup::BACKGROUND_ARG).build())
        // Stdout plus a rotating file in the OS log dir (macOS:
        // ~/Library/Logs/com.opendesk.app/) for bug reports.
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                // ICE logs every candidate check at info; keep only problems.
                .level_for("webrtc_ice", log::LevelFilter::Warn)
                .level_for("webrtc_util", log::LevelFilter::Warn)
                .max_file_size(5 * 1024 * 1024)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepOne)
                .build(),
        )
        .manage(Recordings::default())
        .manage(VideoSink::default())
        .manage(AuxVideoSinks::default())
        .manage(unattended::PendingTotp::default())
        .manage(std::sync::Arc::new(tunnel::TunnelManager::default()))
        .setup(move |app| {
            let app_data_dir = app
                .path()
                .app_data_dir()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            // Debug builds only: a second instance with its own identity, for
            // host+viewer testing on one machine (see docs/TESTING.md).
            #[cfg(debug_assertions)]
            let app_data_dir = std::env::var_os("OPENDESK_DATA_DIR").map(PathBuf::from).unwrap_or(app_data_dir);
            let config = ConfigStore::load(&app_data_dir)?;
            let contacts = ContactStore::load(&app_data_dir)?;
            let clipboard_sync = config.config.clipboard_sync;
            app.manage(AppState(Mutex::new(RuntimeState {
                config,
                contacts,
                password: None,
                host_thread: None,
                host_starting: None,
                viewer_task: None,
                viewer_stop: None,
                viewer_input: None,
                pending_request: None,
                pending_file_requests: HashMap::new(),
                file_outgoing: None,
                file_sealer: None,
                control_sender: None,
                control_sealer: None,
                file_router: None,
                upload_dir: Arc::new(Mutex::new(None)),
                clipboard_enabled: Arc::new(AtomicBool::new(clipboard_sync)),
                audio_muted: Arc::new(AtomicBool::new(false)),
                clipboard_thread: None,
                identity_conflicts: Vec::new(),
                viewer_permissions: ViewerPermissions::new(),
                disconnect_viewer: None,
            })));
            #[cfg(windows)]
            if windows_service::is_service_host_process() {
                if let Err(error) = host::start_hosting(app.state(), app.handle().clone()) {
                    log::error!("could not start the service host: {error}");
                }
            } else {
                startup::start(app.handle(), background);
            }
            #[cfg(not(windows))]
            startup::start(app.handle(), background);
            #[cfg(target_os = "macos")]
            if let Err(error) = menu::install(app.handle()) {
                log::error!("could not install the application menu: {error}");
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if startup::hide_instead_of_close(window) {
                    api.prevent_close();
                }
            }
        })
        .on_menu_event(menu::on_event)
        .invoke_handler(tauri::generate_handler![
            get_identity,
            rotate_password,
            get_config,
            list_contacts,
            list_allowed,
            add_allowed,
            remove_allowed,
            add_contact,
            remove_contact,
            touch_contact,
            set_relay,
            get_unattended_status,
            windows_service_status,
            install_windows_service,
            uninstall_windows_service,
            set_unattended_access,
            set_unattended_2fa,
            confirm_unattended_2fa,
            permission_status,
            open_permission_settings,
            respond_to_request,
            start_hosting,
            stop_hosting,
            connect,
            disconnect,
            send_input,
            select_monitor,
            send_auth_code,
            start_video,
            stop_video,
            start_monitor_video,
            stop_monitor_video,
            open_monitor_stream,
            close_monitor_stream,
            set_input_monitor,
            get_presence,
            request_remote_restart,
            set_privacy_mode,
            set_max_bitrate,
            set_decoder_codecs,
            start_forward,
            stop_forward,
            list_forwards,
            browse_remote,
            browse_local,
            pull_remote,
            upload_to_remote,
            request_keyframe,
            set_clipboard_sync,
            set_allow_virtual_displays,
            add_virtual_display,
            remove_virtual_display,
            send_secure_attention,
            vdriver::virtual_display_driver_status,
            vdriver::install_virtual_display_driver,
            vdriver::uninstall_virtual_display_driver,
            set_audio_muted,
            get_viewer_permissions,
            set_viewer_permission,
            disconnect_viewer,
            send_file,
            respond_file,
            send_chat,
            start_recording,
            append_recording,
            finish_recording,
            list_recordings,
            delete_recording,
            set_relays,
            list_relays,
            startup::close_keeps_running,
            startup::quit_app,
            reset_identity,
            get_identity_conflicts,
            set_session_menu
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");
    app.run(|_app, event| {
        // Windows keeps virtual displays after their process is gone, so
        // take ours away on the way out (macOS does it by itself).
        if let tauri::RunEvent::Exit = event {
            host_core::vdisplay::remove_owned();
        }
        // Clicking the Dock icon brings back a window hidden to the tray.
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { has_visible_windows: false, .. } = event {
            startup::show_main_window(_app);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_permissions_default_to_all_enabled() {
        let perms = ViewerPermissions::new();
        let snapshot = perms.snapshot();
        assert!(snapshot.control && snapshot.files && snapshot.clipboard && snapshot.audio);
    }

    #[test]
    fn viewer_permission_flag_toggles_and_snapshot_tracks_it() {
        let perms = ViewerPermissions::new();
        assert!(perms.flag("bogus").is_none());
        for name in ["control", "files", "clipboard", "audio"] {
            perms.flag(name).unwrap().store(false, Ordering::Relaxed);
        }
        let off = perms.snapshot();
        assert_eq!((off.control, off.files, off.clipboard, off.audio), (false, false, false, false));
        // Reset puts every capability back on for a new session.
        perms.reset();
        let on = perms.snapshot();
        assert!(on.control && on.files && on.clipboard && on.audio);
    }

    #[test]
    fn auth_backoff_stays_short_and_resets() {
        let mut backoff = AuthBackoff::default();
        let delays: Vec<u64> = (0..8).map(|_| backoff.after_session(false, false).delay.as_secs()).collect();
        assert_eq!(delays, [0, 0, 0, 2, 4, 5, 5, 5]);
        assert_eq!(backoff.after_session(true, false).delay, Duration::ZERO);
        assert_eq!(backoff.after_session(false, false).delay, Duration::ZERO);
    }

    #[test]
    fn dropped_session_keeps_password_for_the_reconnect_window() {
        let mut backoff = AuthBackoff::default();
        let after = backoff.after_session(true, true);
        assert!(!after.rotate_password);
        assert!(after.rotate_after_reconnect_window);
        assert_eq!(after.delay, Duration::ZERO);
        let clean = backoff.after_session(true, false);
        assert!(clean.rotate_password);
        assert!(!clean.rotate_after_reconnect_window);
    }

    #[test]
    fn session_password_rotates_after_success_and_every_third_failure() {
        let mut backoff = AuthBackoff::default();
        let rotations: Vec<bool> = (0..7).map(|_| backoff.after_session(false, false).rotate_password).collect();
        assert_eq!(rotations, [false, false, true, false, false, true, false]);
        assert!(backoff.after_session(true, false).rotate_password);
        assert!(!backoff.after_session(false, false).rotate_password);
    }

    #[test]
    fn unattended_password_needs_minimum_length() {
        assert!(validate_unattended_password("short").is_err());
        let error = validate_unattended_password("long enough").unwrap_err();
        assert!(error.contains("12 characters"), "{error}");
        assert!(validate_unattended_password("tulip-orbit-canyon").is_ok());
    }

    #[test]
    fn unattended_password_rejects_common_and_predictable() {
        for weak in ["Password1234", "QWERTY123456", "aaaaaaaaaaaa", "123456789012", "987654321098", "000000000000", "Administrator"] {
            assert!(validate_unattended_password(weak).is_err(), "{weak} should be rejected");
        }
        assert!(validate_unattended_password("135792468024").is_ok());
        assert!(is_sequential_digits("890123"));
        assert!(!is_sequential_digits("12a4"));
    }

    #[test]
    fn send_input_enqueues_bytes() {
        let (sender, mut receiver) = mpsc::channel(1);
        enqueue_input(Some(sender), vec![1, 2, 3]).unwrap();
        assert_eq!(receiver.try_recv().unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn send_input_requires_active_session() {
        assert_eq!(
            enqueue_input(None, vec![]),
            Err("no active viewer session".into())
        );
    }

    #[test]
    fn file_consent_routes_by_transfer_id() {
        let (sender, mut receiver) = oneshot::channel();
        let mut pending = HashMap::new();
        pending.insert(42, sender);

        route_file_consent(&mut pending, 42, true).unwrap();

        assert!(receiver.try_recv().unwrap());
        assert!(route_file_consent(&mut pending, 42, false).is_err());
    }

    #[test]
    fn reliable_file_source_opens_sealed_data_bodies() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let message = FileMsg::Done { id: 7 };
            let (file_tx, file_rx) = mpsc::channel(1);
            file_tx.send(message.clone()).await.unwrap();
            let mut source = ReliableFileSource {
                incoming: file_rx,
            };
            assert_eq!(source.recv().await, Some(message));
        });
    }

    #[test]
    fn recording_name_is_sanitized_and_has_webm_extension() {
        assert_eq!(sanitize_recording_name("../remote:session?.webm"), "remote_session_.webm");
        assert_eq!(sanitize_recording_name("nested/name"), "name.webm");
        assert_eq!(sanitize_recording_name("..."), "recording.webm");
    }

    #[test]
    fn recording_path_stays_in_downloads_directory() {
        let path = recording_path("../../capture");
        assert_eq!(path.parent(), Some(downloads_dir().as_path()));
        assert_eq!(path.file_name().and_then(|name| name.to_str()), Some("capture.webm"));
    }

    #[test]
    fn recording_filename_avoids_existing_file() {
        let directory = std::env::temp_dir().join(format!(
            "opendesk-viewer-recording-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("capture.webm"), b"existing").unwrap();

        let path = next_recording_path(&directory, "capture").unwrap();

        assert_eq!(path.file_name().and_then(|name| name.to_str()), Some("capture (2).webm"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn recording_size_cap_rejects_oversized_input() {
        assert!(validate_recording_size_len(MAX_RECORDING_SIZE).is_ok());
        let error = validate_recording_size_len(MAX_RECORDING_SIZE + 1).unwrap_err();
        assert!(error.contains("maximum size"));
    }

    #[test]
    fn recording_sink_streams_chunks_to_disk() {
        let directory = std::env::temp_dir().join(format!("opendesk-recording-sink-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();

        let mut sink = RecordingSink::create(&directory, "session").unwrap();
        sink.append(b"abc").unwrap();
        sink.append(b"def").unwrap();
        let path = sink.finish().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"abcdef");
        assert_eq!(path.file_name().and_then(|name| name.to_str()), Some("session.webm"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn recording_sink_enforces_size_cap_across_chunks() {
        let directory = std::env::temp_dir().join(format!("opendesk-recording-cap-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();

        let mut sink = RecordingSink::create(&directory, "capped").unwrap();
        sink.written = MAX_RECORDING_SIZE - 2;
        assert!(sink.append(b"ab").is_ok());
        assert!(sink.append(b"c").unwrap_err().contains("maximum size"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn viewer_slot_reservation_fails_when_already_reserved() {
        assert!(can_reserve_viewer_slot(false, false));
        assert!(!can_reserve_viewer_slot(true, false));
        assert!(!can_reserve_viewer_slot(false, true));
    }

    #[test]
    fn host_slot_reservation_fails_while_starting_or_running() {
        assert!(can_reserve_host_slot(false, false));
        assert!(!can_reserve_host_slot(true, false));
        assert!(!can_reserve_host_slot(false, true));
        assert!(!can_reserve_host_slot(true, true));
    }

    #[test]
    fn host_reservation_is_owned_only_by_its_token() {
        let token = Arc::new(());
        let other = Arc::new(());
        let reservation = HostReservation { token: token.clone(), task: None };
        assert!(owns_host_reservation(Some(&reservation), &token));
        assert!(!owns_host_reservation(Some(&reservation), &other));
        assert!(!owns_host_reservation(None, &token));
    }

    #[test]
    fn second_pending_request_is_rejected_and_first_kept() {
        let mut slot = None;
        let (first_tx, first_rx) = oneshot::channel();
        assert!(admit_pending_request(&mut slot, first_tx).is_ok());

        let (second_tx, mut second_rx) = oneshot::channel();
        let rejected = admit_pending_request(&mut slot, second_tx).unwrap_err();
        rejected.send(false).unwrap();
        assert_eq!(second_rx.try_recv(), Ok(false));

        // The first requester still receives the user's decision.
        slot.take().unwrap().send(true).unwrap();
        assert_eq!(first_rx.blocking_recv(), Ok(true));
    }

    #[test]
    fn stale_pending_request_does_not_block_new_one() {
        let mut slot = None;
        let (stale_tx, stale_rx) = oneshot::channel::<bool>();
        assert!(admit_pending_request(&mut slot, stale_tx).is_ok());
        drop(stale_rx);

        let (fresh_tx, fresh_rx) = oneshot::channel();
        assert!(admit_pending_request(&mut slot, fresh_tx).is_ok());
        slot.take().unwrap().send(true).unwrap();
        assert_eq!(fresh_rx.blocking_recv(), Ok(true));
    }

    #[test]
    fn poisoned_inner_mutex_remains_usable() {
        let mutex = Mutex::new(Some(7_u64));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().unwrap();
            panic!("poison the test mutex");
        }));
        assert!(mutex.is_poisoned());
        assert_eq!(*lock_unpoisoned(&mutex), Some(7));
    }

    #[test]
    fn poisoned_app_state_lock_remains_usable() {
        let directory = std::env::temp_dir().join(format!(
            "opendesk-viewer-lock-test-{}",
            std::process::id()
        ));
        let app_state = AppState(Mutex::new(RuntimeState {
            config: ConfigStore::load(&directory).unwrap(),
            contacts: ContactStore::load(&directory).unwrap(),
            password: None,
            host_thread: None,
            host_starting: None,
            viewer_task: None,
            viewer_stop: None,
            viewer_input: None,
            pending_request: None,
            pending_file_requests: HashMap::new(),
            file_outgoing: None,
            file_sealer: None,
            control_sender: None,
            control_sealer: None,
            file_router: None,
                upload_dir: Arc::new(Mutex::new(None)),
            clipboard_enabled: Arc::new(AtomicBool::new(true)),
            audio_muted: Arc::new(AtomicBool::new(false)),
            clipboard_thread: None,
            identity_conflicts: Vec::new(),
            viewer_permissions: ViewerPermissions::new(),
            disconnect_viewer: None,
        }));

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = app_state.lock();
            panic!("poison the test mutex");
        }));

        let guard = app_state.lock();
        assert!(guard.viewer_task.is_none());
        drop(guard);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn abort_on_drop_aborts_child_task() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (started_tx, started_rx) = oneshot::channel();
            let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let task_ticks = ticks.clone();
            let task = tauri::async_runtime::spawn(async move {
                started_tx.send(()).unwrap();
                loop {
                    task_ticks.fetch_add(1, Ordering::Relaxed);
                    tokio::task::yield_now().await;
                }
            });
            let guard = AbortOnDrop::new(task);
            started_rx.await.unwrap();
            drop(guard);
            // The task runs on Tauri's multi-threaded runtime, so it may tick
            // once more while the abort lands. Let it settle, then it must
            // stay stopped.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let ticks_after_abort = ticks.load(Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(ticks.load(Ordering::Relaxed), ticks_after_abort);
        });
    }
}
