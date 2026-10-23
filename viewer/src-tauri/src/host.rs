//! Hosting: relay registration, pairing requests, and the host session loop.

use super::*;

/// The hosting slot is free only when no host thread is running and no
/// `start_hosting` registration is in flight.
pub(crate) fn can_reserve_host_slot(host_thread_running: bool, host_starting: bool) -> bool {
    !host_thread_running && !host_starting
}

/// Whether `token` still owns the in-flight hosting reservation.
pub(crate) fn owns_host_reservation(reservation: Option<&HostReservation>, token: &Arc<()>) -> bool {
    reservation.is_some_and(|reservation| Arc::ptr_eq(&reservation.token, token))
}

/// Admit `decision_tx` as the pending pairing request. Only one request may
/// wait for a user decision: if a live one is already pending, the newcomer is
/// handed back so the caller can reject it (the first keeps its UI prompt). A
/// stale sender whose requester already went away does not block new requests.
pub(crate) fn admit_pending_request(
    slot: &mut Option<oneshot::Sender<bool>>,
    decision_tx: oneshot::Sender<bool>,
) -> Result<(), oneshot::Sender<bool>> {
    if slot.as_ref().is_some_and(|existing| !existing.is_closed()) {
        return Err(decision_tx);
    }
    *slot = Some(decision_tx);
    Ok(())
}

#[tauri::command]
pub(crate) fn respond_to_request(
    state: tauri::State<'_, AppState>,
    peer_id: String,
    allow: bool,
    always: bool,
) -> Result<(), String> {
    let mut state = state.lock();
    if always && allow && peer_id != "unknown" {
        state
            .config
            .add_allow(&peer_id)
            .map_err(|e| e.to_string())?;
    }
    state
        .pending_request
        .take()
        .ok_or_else(|| "no incoming request".to_string())?
        .send(allow)
        .map_err(|_| "incoming request expired".into())
}

/// Per-session host credentials and policy. Read fresh for every pairing so a
/// rotated password or a changed unattended/2FA setting applies to the next
/// session of a long-lived hosting loop instead of the value from when
/// hosting started.
pub(crate) struct HostSessionParams {
    pub(crate) password: String,
    pub(crate) unattended_enabled: bool,
    pub(crate) twofa_enabled: bool,
    pub(crate) totp_secret: Option<String>,
    /// Why unattended access is on but can't be used for this session.
    pub(crate) unattended_problem: Option<String>,
    pub(crate) allow_virtual_displays: bool,
}

pub(crate) fn host_session_params(s: &mut RuntimeState) -> HostSessionParams {
    // A keyring failure deliberately disables unattended approval for this
    // session and uses the normal manual-accept PAKE path; the problem is
    // shown to the user when the session starts.
    #[cfg(windows)]
    let check = if crate::windows_service::is_service_host_process() {
        unattended::check_runtime_config(&s.config.config)
    } else {
        unattended::check_config(&s.config.config)
    };
    #[cfg(not(windows))]
    let check = unattended::check_config(&s.config.config);
    let unattended_enabled = check.ready();
    let twofa_enabled = unattended_enabled && s.config.config.unattended_twofa_enabled;
    let password = check.password.unwrap_or_else(|| current_password(s));
    let allow_virtual_displays = s.config.config.allow_virtual_displays;
    HostSessionParams { password, unattended_enabled, twofa_enabled, totp_secret: check.totp_secret, unattended_problem: check.problem, allow_virtual_displays }
}

/// Wakes hosting loops when the saved relay list or the device ID changes,
/// so a parked registration moves over right away instead of at its next
/// renewal.
fn relays_changed() -> &'static tokio::sync::Notify {
    static RELAYS_CHANGED: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
    RELAYS_CHANGED.get_or_init(tokio::sync::Notify::new)
}

/// Call after saving a new relay list or ID: every hosting loop re-registers.
pub(crate) fn notify_relays_changed() {
    relays_changed().notify_waiters();
}

/// The relay identity a hosting loop registers under. It is re-read from the
/// config before every attempt, so a new ID applies to the next registration.
/// Re-registering with the same token is accepted by the relay's ownership
/// lease, so a host can come back after every session.
#[derive(Clone)]
pub(crate) struct HostRegistration {
    pub(crate) id: String,
    pub(crate) token: String,
}

/// The relays to try next: the saved list minus those that refused this
/// device's ID, or the whole list again once every relay has refused it.
pub(crate) fn relays_to_try(relays: &[String], refused: &[String]) -> Vec<String> {
    let usable: Vec<String> = relays.iter().filter(|relay| !refused.contains(relay)).cloned().collect();
    if usable.is_empty() { relays.to_vec() } else { usable }
}

/// What the user is told when `relay` refuses ID `id`. `in_use` means the
/// relay saw another live connection with this device's own ID and token;
/// otherwise the ID belongs to someone else's token there.
pub(crate) fn identity_conflict(relay: &str, id: &str, in_use: bool, registered_here: bool) -> IdentityConflict {
    let message = if in_use {
        format!("Another computer is online with ID {id} on {relay}. It is probably a copy of this one, such as a cloned disk or virtual machine. Get a new ID on one of them.")
    } else if registered_here {
        format!("ID {id} now belongs to another device on {relay}, probably because this one was offline long enough for the relay to release it. Get a new ID to be reachable there again.")
    } else {
        format!("ID {id} belongs to another device on {relay}, so this device can't be reached through that relay. Remove it from your relays or get a new ID.")
    };
    IdentityConflict { relay: relay.to_owned(), kind: if in_use { "in_use" } else { "taken" }, message }
}

/// Replace `relay`'s entry in the conflict list (`None` clears it) and tell
/// the UI if anything changed.
pub(crate) fn set_identity_conflict(app: &AppHandle, relay: &str, conflict: Option<IdentityConflict>) {
    let conflicts = {
        let state = app.state::<AppState>();
        let mut state = state.lock();
        let before = state.identity_conflicts.clone();
        state.identity_conflicts.retain(|known| known.relay != relay);
        state.identity_conflicts.extend(conflict);
        if state.identity_conflicts == before { return; }
        state.identity_conflicts.clone()
    };
    emit(app, "identity_conflicts", IdentityConflictsEvent { conflicts });
}

/// Keep only conflicts for relays still in `relays` (`None`: drop all).
pub(crate) fn prune_identity_conflicts(app: &AppHandle, relays: Option<&[String]>) {
    let conflicts = {
        let state = app.state::<AppState>();
        let mut state = state.lock();
        let before = state.identity_conflicts.len();
        state.identity_conflicts.retain(|conflict| relays.is_some_and(|relays| relays.contains(&conflict.relay)));
        if state.identity_conflicts.len() == before { return; }
        state.identity_conflicts.clone()
    };
    emit(app, "identity_conflicts", IdentityConflictsEvent { conflicts });
}

/// Connect to the relay, register, and wait for a viewer to pair, retrying
/// with capped exponential backoff until a pairing arrives. Returns `None`
/// once `still_hosting` reports that this loop no longer owns the hosting
/// slot. Callers may also cancel by dropping the future (task abort or a
/// `select!` on the host thread's stop signal).
///
/// Registration uses the saved relay list in order (first reachable wins),
/// and starts over at once when [`notify_relays_changed`] reports a new list.
/// A relay that refuses the ID is skipped while others are left: a new ID
/// is picked if no relay has accepted this one yet, otherwise the ID is kept
/// and the refusal is shown as an identity conflict.
///
/// Status: "online" when a registration is parked on the relay; "connecting"
/// (plus one error event per failure streak) while registration is failing.
/// A healthy registration that the relay expires (its pairing timeout), or
/// that goes quiet after sleep or a network change, is renewed silently so
/// the UI stays "online". A relay refusal with a known code is shown in
/// plain words; rate limits wait until the relay can say yes.
pub(crate) async fn register_host_until_paired(
    app: &AppHandle,
    registration: &mut HostRegistration,
    still_hosting: impl Fn() -> bool,
) -> Option<(host_core::relay_client::RelayWebSocket, host_core::relay_client::Pairing)> {
    use host_core::relay_client::{
        await_pairing, is_id_in_use, is_id_taken, refusal_message, refusal_retry_after, register_host, settle_registration,
        REGISTRATION_SETTLE,
    };
    let mut backoff = host_core::reconnect::HostRegistrationBackoff::new();
    let mut online = false;
    // Relays that refused the ID during this loop.
    let mut refused: Vec<String> = Vec::new();
    loop {
        // Created before the attempt so a change saved while connecting is
        // not missed (a Notified future counts from creation).
        let relay_change = relays_changed().notified();
        tokio::pin!(relay_change);
        let delay = backoff.delay();
        if !delay.is_zero() {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = &mut relay_change => {
                    backoff = host_core::reconnect::HostRegistrationBackoff::new();
                    refused.clear();
                    continue;
                }
            }
        }
        if !still_hosting() {
            return None;
        }
        // Re-read per attempt so a relay or identity change applies to the
        // next registration.
        let relay_urls = {
            let state = app.state::<AppState>();
            let state = state.lock();
            registration.id.clone_from(&state.config.config.id);
            registration.token.clone_from(&state.config.config.registration_token);
            relays_to_try(&state.config.config.relay_urls, &refused)
        };
        let failure = match register_host(&relay_urls, &registration.id, &registration.token).await {
            Err(error) => {
                backoff.record_failure();
                error
            }
            Ok((relay, mut ws)) => {
                let parked_at = Instant::now();
                // The relay never acknowledges a registration; it only
                // refuses one, right away. Wait for that before trusting it.
                let settled = tokio::select! {
                    settled = settle_registration(&mut ws, REGISTRATION_SETTLE) => settled,
                    _ = &mut relay_change => {
                        backoff = host_core::reconnect::HostRegistrationBackoff::new();
                        refused.clear();
                        continue;
                    }
                };
                match settled {
                    Err(error) if is_id_taken(&error) || is_id_in_use(&error) => {
                        let in_use = is_id_in_use(&error);
                        let (replaced, registered_here) = {
                            let state = app.state::<AppState>();
                            let mut state = state.lock();
                            let registered_here = state.config.is_registered_on(&relay);
                            (state.config.replace_colliding_id(), registered_here)
                        };
                        if let Some(id) = replaced {
                            // Nobody knows this ID yet, so just pick another.
                            log::info!("device ID was refused by the relay before it was ever used; switched to a new one");
                            emit(app, "identity_changed", IdentityChangedEvent { id });
                            continue;
                        }
                        set_identity_conflict(app, &relay, Some(identity_conflict(&relay, &registration.id, in_use, registered_here)));
                        if !refused.contains(&relay) {
                            refused.push(relay);
                        }
                        backoff.record_failure();
                        error
                    }
                    Err(error) => {
                        match refusal_retry_after(&error) {
                            // Try the other relays now; come back to this
                            // one only once its limit can have cleared.
                            Some(wait) => {
                                if !refused.contains(&relay) {
                                    refused.push(relay);
                                }
                                if relay_urls.iter().any(|url| !refused.contains(url)) {
                                    backoff.record_failure();
                                } else {
                                    backoff.record_refusal(wait);
                                }
                            }
                            None => backoff.record_failure(),
                        }
                        error
                    }
                    Ok(early) => {
                        {
                            let state = app.state::<AppState>();
                            state.lock().config.mark_registered_on(&relay);
                        }
                        set_identity_conflict(app, &relay, None);
                        if !online {
                            emit_host_status(app, "online");
                            online = true;
                        }
                        let paired = match early {
                            Some(pairing) => Ok(pairing),
                            None => tokio::select! {
                                paired = await_pairing(&mut ws) => paired,
                                _ = &mut relay_change => {
                                    log::info!("relay list or device ID changed; re-registering");
                                    backoff = host_core::reconnect::HostRegistrationBackoff::new();
                                    refused.clear();
                                    continue;
                                }
                            },
                        };
                        match paired {
                            Ok(pairing) => return Some((ws, pairing)),
                            Err(error) => {
                                backoff.record_parked_then_closed(parked_at.elapsed());
                                error
                            }
                        }
                    }
                }
            }
        };
        if backoff.consecutive_failures() == 0 {
            // A long-parked registration expired normally; renew it quietly.
            continue;
        }
        if online || backoff.consecutive_failures() == 1 {
            let message = match refusal_message(&failure) {
                Some(message) => message.to_owned(),
                None => format!("{failure}; retrying host registration"),
            };
            emit(app, "error", ErrorEvent { message });
            emit_host_status(app, "connecting");
        }
        online = false;
    }
}

/// A new ID and token for this device, on the user's request: the way out
/// when a relay has given the old ID to another device or a copy of this
/// one is using it. Hosting re-registers under the new ID right away.
#[tauri::command]
pub(crate) fn reset_identity(app: AppHandle) -> String {
    let id = app.state::<AppState>().lock().config.reset_identity();
    log::info!("device ID replaced at the user's request");
    prune_identity_conflicts(&app, None);
    emit(&app, "identity_changed", IdentityChangedEvent { id: id.clone() });
    notify_relays_changed();
    id
}

#[tauri::command]
pub(crate) fn get_identity_conflicts(state: tauri::State<'_, AppState>) -> Vec<IdentityConflict> {
    state.lock().identity_conflicts.clone()
}

/// Run one paired host session. Returns `true` when the stop signal ended it
/// (stop_hosting/disconnect), `false` when the session ended on its own.
pub(crate) async fn run_paired_host_session(
    app: &AppHandle,
    ws: host_core::relay_client::RelayWebSocket,
    pairing: host_core::relay_client::Pairing,
    params: HostSessionParams,
    stop_rx: &mut oneshot::Receiver<()>,
) -> HostSessionEnd {
    let host_core::relay_client::Pairing { session: ice_session, peer_id } = pairing;
    // Set once the viewer has passed the password (and 2FA) checks, which is
    // when host-core asks for approval.
    let authenticated = Arc::new(AtomicBool::new(false));
    let authenticated_flag = authenticated.clone();
    let HostSessionParams { password, unattended_enabled, twofa_enabled, totp_secret, unattended_problem, allow_virtual_displays } = params;
    // Every session starts with all four viewer permissions enabled, and a
    // fresh per-session disconnect signal distinct from the stop-hosting one.
    let (gates, mut disconnect_rx) = {
        let state = app.state::<AppState>();
        let mut state = state.lock();
        state.viewer_permissions.reset();
        // Drop any one-shot upload destination from a previous session, so a
        // stale viewer-chosen path can never redirect the next session's file.
        *lock_unpoisoned(&state.upload_dir) = None;
        let gates = host_core::roles::SessionGates {
            control: state.viewer_permissions.control.clone(),
            audio: state.viewer_permissions.audio.clone(),
        };
        let (disconnect_tx, disconnect_rx) = oneshot::channel::<()>();
        state.disconnect_viewer = Some(disconnect_tx);
        (gates, disconnect_rx)
    };
    emit(app, "viewer_permissions", app.state::<AppState>().lock().viewer_permissions.snapshot());
    if let Some(problem) = unattended_problem {
        // This connection will need approval; say why before the prompt.
        emit(app, "error", ErrorEvent { message: problem });
    }
    // The viewer's peer ID comes from the relay's pairing message and is not
    // authenticated, so an allow-listed ID never skips approval; the prompt
    // only marks it as a known device. Unattended access (its own password,
    // plus optional TOTP) is the only path without a prompt.
    let known = app.state::<AppState>().lock().config.is_allowed(&peer_id);
    let remote_id = peer_id.clone();
    let approve_app = app.clone();
    let (data_ready_tx, data_ready_rx) = oneshot::channel();
    let approve = move || async move {
        authenticated_flag.store(true, Ordering::Relaxed);
        if unattended_enabled {
            emit_host_status(&approve_app, "in_session");
            emit(&approve_app, "session_started", SessionStarted { role: "host", remote_id: peer_id.clone() });
            emit_activity(&approve_app, "session", format!("{} connected", peer_id));
            return true;
        }
        let (decision_tx, decision_rx) = oneshot::channel();
        // Install before emitting so the UI can never answer a
        // prompt whose sender is not yet in place.
        let admitted = admit_pending_request(
            &mut approve_app.state::<AppState>().lock().pending_request,
            decision_tx,
        );
        if let Err(rejected_tx) = admitted {
            // Another request is already awaiting a decision:
            // reject this one and leave the existing prompt.
            let _ = rejected_tx.send(false);
            return false;
        }
        // A host started at sign-in may be hidden in the tray.
        startup::reveal_if_hidden(&approve_app);
        emit(&approve_app, "incoming_request", IncomingRequest {
            peer_id: peer_id.clone(), fingerprint: "pending".into(), known,
        });
        // host-core drops this wait when the viewer leaves or nobody answers
        // in time; the session_ended that follows closes the prompt.
        let allowed = decision_rx.await.unwrap_or(false);
        if allowed {
            emit_host_status(&approve_app, "in_session");
            emit(&approve_app, "session_started", SessionStarted { role: "host", remote_id: peer_id.clone() });
            emit_activity(&approve_app, "session", format!("{} connected", peer_id));
        }
        allowed
    };
    let mut displays = host_core::roles::HostDisplays::platform(allow_virtual_displays);
    let notice_app = app.clone();
    displays.notify = Some(Arc::new(move |notice| emit(&notice_app, "host_notice", HostNoticeEvent { message: display_notice(notice) })));
    let mut session = Box::pin(host_core::roles::run_default_host_session_with_displays(
        ws, &password, &ice_session, unattended_enabled, twofa_enabled, totp_secret.as_deref(), approve, data_ready_tx, displays, gates,
    ));
    let mut streamed = false;
    // The host user disconnected this viewer but wants to keep hosting; it ends
    // the session like a stop but loops back instead of leaving the loop.
    let mut disconnected = false;
    let result = tokio::select! {
        handle = data_ready_rx => {
            match handle {
                Ok(handle) => {
                    streamed = true;
                    activate_data_channel(app, handle, remote_id.clone(), true);
                    tokio::select! {
                        result = &mut session => Some(result),
                        _ = &mut *stop_rx => None,
                        _ = &mut disconnect_rx => { disconnected = true; None }
                    }
                }
                Err(_) => Some(Err("session ended before the data channel was ready".into())),
            }
        }
        result = &mut session => Some(result),
        _ = &mut *stop_rx => None,
        _ = &mut disconnect_rx => { disconnected = true; None }
    };
    // This session is over, so its disconnect sender must not linger.
    app.state::<AppState>().lock().disconnect_viewer = None;
    if result.is_none() {
        // The host user stopped or disconnected the viewer: tell it explicitly
        // so it ends cleanly instead of treating the close as a drop and
        // reconnecting. The session (and its writer) is still alive here.
        send_host_bye(app).await;
    }
    if let Some(Err(error)) = &result {
        emit(app, "error", ErrorEvent { message: error.to_string() });
    }
    emit(
        app,
        "session_ended",
        SessionEnded {
            reason: if result.is_some() { "host session ended".into() } else { "disconnected".into() },
        },
    );
    emit_activity(app, "session", if disconnected { format!("{remote_id} disconnected by host") } else { "viewer disconnected".to_owned() });
    clear_data_channel(app);
    let dropped = streamed && matches!(result, Some(Err(_)));
    // A host-initiated disconnect keeps hosting: the loop re-registers.
    HostSessionEnd { stopped: should_stop_hosting(result.is_none(), disconnected), authenticated: authenticated.load(Ordering::Relaxed), dropped }
}

/// Whether the hosting loop should leave after this session. Only a stop (the
/// `stop_rx` signal) ends hosting; a host-initiated viewer disconnect does not,
/// so the loop re-registers and awaits the next viewer.
pub(crate) fn should_stop_hosting(ended_by_signal: bool, disconnected: bool) -> bool {
    ended_by_signal && !disconnected
}

/// What the host user is told when a session changes its displays.
fn display_notice(notice: host_core::roles::DisplayNotice) -> &'static str {
    use host_core::roles::DisplayNotice;
    match notice {
        DisplayNotice::HeadlessAdded => "No screen is connected, so OpenDesk added a virtual display for this session.",
        DisplayNotice::ViewerAdded => "The viewer added a virtual display. It goes away when the session ends.",
        DisplayNotice::ViewerRemoved => "The viewer removed a virtual display.",
    }
}

/// Best-effort sealed `Control::Bye` on the active session's control channel.
async fn send_host_bye(app: &AppHandle) {
    let (sender, sealer) = {
        let state = app.state::<AppState>();
        let state = state.lock();
        (state.control_sender.clone(), state.control_sealer.clone())
    };
    if let (Some(sender), Some(sealer)) = (sender, sealer) {
        host_core::roles::send_bye(&sender, &sealer).await;
    }
}

/// Cleans up after a host thread that panicked (e.g. inside encoder
/// initialization): without it `host_thread` stays `Some` and every later
/// `start_hosting` silently does nothing. Normal exits clean up themselves.
/// Only effective where panics unwind (release builds abort on panic).
struct HostThreadPanicGuard {
    app: AppHandle,
    token: Arc<()>,
}

impl Drop for HostThreadPanicGuard {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        clear_data_channel(&self.app);
        clear_own_host_thread(&self.app, &self.token);
        emit(&self.app, "error", ErrorEvent { message: "hosting stopped unexpectedly; start hosting again".into() });
        emit(&self.app, "session_ended", SessionEnded { reason: "host session failed".into() });
        emit_host_status(&self.app, "offline");
    }
}

pub(crate) struct HostSessionEnd {
    /// The user stopped hosting; leave the hosting loop.
    pub(crate) stopped: bool,
    /// The viewer got past the password/2FA checks.
    pub(crate) authenticated: bool,
    /// A running session ended without a goodbye; the viewer will reconnect.
    pub(crate) dropped: bool,
}

/// Limits password guessing against a host. The main defence is rotation:
/// the one-time session password changes after every authenticated session
/// and after every third consecutive failed one, so a guesser gets at most
/// three tries per password (3 in 900,000). On top of that a short delay
/// (2 s, 4 s, then 5 s) slows repeated failures. The delay is capped low
/// because the host is unregistered while it waits: a long delay would let
/// anyone who knows the host ID keep it offline with a few bad logins.
#[derive(Default)]
pub(crate) struct AuthBackoff {
    consecutive_failures: u32,
}

/// What the hosting loop does after a session ends.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AfterSession {
    /// Wait this long before registering again.
    pub(crate) delay: Duration,
    /// Replace the one-time session password.
    pub(crate) rotate_password: bool,
    /// Replace it once the viewer's reconnect window has passed instead, so a
    /// dropped viewer can still get back in with the password it has.
    pub(crate) rotate_after_reconnect_window: bool,
}

impl AuthBackoff {
    const FREE_ATTEMPTS: u32 = 3;
    const ROTATE_EVERY_FAILURES: u32 = 3;
    const MAX_DELAY: Duration = Duration::from_secs(5);

    pub(crate) fn after_session(&mut self, authenticated: bool, dropped: bool) -> AfterSession {
        if authenticated {
            self.consecutive_failures = 0;
            return AfterSession { delay: Duration::ZERO, rotate_password: !dropped, rotate_after_reconnect_window: dropped };
        }
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let rotate_password = self.consecutive_failures.is_multiple_of(Self::ROTATE_EVERY_FAILURES);
        let over = self.consecutive_failures.saturating_sub(Self::FREE_ATTEMPTS);
        let delay = if over == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(1u64 << over.min(6)).min(Self::MAX_DELAY)
        };
        AfterSession { delay, rotate_password, rotate_after_reconnect_window: false }
    }
}

/// Each host session runs on this dedicated thread's current-thread runtime.
/// The thread is the hosting loop:
/// after a session ends on its own it re-registers with the relay under the
/// same ID/token and waits for the next pairing, so a dropped viewer can
/// reconnect. Only the stop signal (stop_hosting/disconnect) or losing
/// ownership of `host_thread` ends the loop.
pub(crate) fn spawn_host_thread(
    app: AppHandle,
    mut registration: HostRegistration,
    first: (host_core::relay_client::RelayWebSocket, host_core::relay_client::Pairing),
    mut stop_rx: oneshot::Receiver<()>,
    token: Arc<()>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let _panic_guard = HostThreadPanicGuard { app: app.clone(), token: token.clone() };
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                emit(&app, "error", ErrorEvent { message: error.to_string() });
                emit(&app, "session_ended", SessionEnded { reason: "runtime initialization failed".into() });
                emit_host_status(&app, "offline");
                clear_own_host_thread(&app, &token);
                return;
            }
        };
        runtime.block_on(async {
            let mut next = first;
            let mut backoff = AuthBackoff::default();
            // Deferred rotation after a dropped session; see AfterSession.
            let mut rotate_at: Option<tokio::time::Instant> = None;
            'hosting: loop {
                let params = host_session_params(&mut app.state::<AppState>().lock());
                let (ws, pairing) = next;
                let ended = run_paired_host_session(&app, ws, pairing, params, &mut stop_rx).await;
                let after = backoff.after_session(ended.authenticated, ended.dropped);
                let delay = after.delay;
                if after.rotate_password || rotate_at.is_some_and(|at| at <= tokio::time::Instant::now()) {
                    // A finished session's password must not let anyone back
                    // in, and a guesser gets a fresh target every 3 misses.
                    rotate_session_password(&app);
                    rotate_at = None;
                }
                if after.rotate_after_reconnect_window {
                    rotate_at = Some(tokio::time::Instant::now() + host_core::reconnect::VIEWER_RECONNECT_WINDOW);
                }
                if ended.stopped {
                    break;
                }
                if !delay.is_zero() {
                    log::warn!("repeated failed authentication; waiting {}s before accepting the next request", delay.as_secs());
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = &mut stop_rx => break,
                    }
                }
                // Ended on its own (viewer left, network drop, rejection):
                // go straight back to waiting rather than going offline.
                emit_host_status(&app, "connecting");
                let still_hosting = || owns_host_thread(&app, &token);
                let register = register_host_until_paired(&app, &mut registration, still_hosting);
                tokio::pin!(register);
                loop {
                    tokio::select! {
                        paired = &mut register => match paired {
                            Some(paired) => { next = paired; break; }
                            None => break 'hosting,
                        },
                        _ = &mut stop_rx => break 'hosting,
                        _ = tokio::time::sleep_until(rotate_at.unwrap_or_else(tokio::time::Instant::now)), if rotate_at.is_some() => {
                            rotate_session_password(&app);
                            rotate_at = None;
                        }
                    }
                }
            }
        });
        emit_host_status(&app, "offline");
        // Leave no stale handle preventing a later start_hosting call.
        clear_own_host_thread(&app, &token);
    })
}

#[tauri::command]
pub(crate) fn start_hosting(state: tauri::State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    if !crate::windows_service::should_run_user_host(crate::windows_service::status().running) {
        log::info!("hosted by the OpenDesk service; user host is standing down");
        emit_host_status(&app, "offline");
        return Ok(());
    }
    let reservation_token = Arc::new(());
    let mut registration = {
        let mut s = state.lock();
        // Check and reserve the hosting slot under this single lock so two
        // rapid calls (e.g. a StrictMode double mount) cannot both spawn.
        if !can_reserve_host_slot(s.host_thread.is_some(), s.host_starting.is_some()) {
            return Ok(());
        }
        s.host_starting = Some(HostReservation { token: reservation_token.clone(), task: None });
        HostRegistration { id: s.config.ensure_identity(), token: s.config.ensure_registration_token() }
    };
    emit_host_status(&app, "connecting");
    let task_token = reservation_token.clone();
    let task = tauri::async_runtime::spawn(async move {
        let still_reserved = || owns_current_host_reservation(&app, &task_token);
        let Some(first) = register_host_until_paired(&app, &mut registration, still_reserved).await else {
            return;
        };
        // Hold the state lock across spawning and installing the thread:
        // the thread's own cleanup needs this lock, so it cannot clear the
        // slot before its handle is installed.
        let app_state = app.state::<AppState>();
        let mut runtime_state = app_state.lock();
        if !owns_host_reservation(runtime_state.host_starting.as_ref(), &task_token) {
            // stop_hosting() released the reservation while we were
            // pairing; drop the socket instead of starting a session.
            return;
        }
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let join = spawn_host_thread(app.clone(), registration, first, stop_rx, task_token.clone());
        runtime_state.host_starting = None;
        runtime_state.host_thread = Some(HostThread { stop_tx, join, token: task_token.clone() });
    });
    // Keep the registration handle with the reservation so stop_hosting() can
    // abort a registration still retrying or waiting for a pairing. If the
    // reservation is already gone it was either handed off to the HostThread
    // (the task is finishing) or released by stop_hosting/disconnect before
    // the handle existed; abort so a retrying registration cannot outlive it.
    let mut runtime_state = state.lock();
    match runtime_state
        .host_starting
        .as_mut()
        .filter(|reservation| Arc::ptr_eq(&reservation.token, &reservation_token))
    {
        Some(reservation) => reservation.task = Some(task),
        None => task.abort(),
    }
    Ok(())
}

pub(crate) fn owns_current_host_reservation(app: &AppHandle, token: &Arc<()>) -> bool {
    let app_state = app.state::<AppState>();
    let runtime_state = app_state.lock();
    owns_host_reservation(runtime_state.host_starting.as_ref(), token)
}

pub(crate) fn owns_host_thread(app: &AppHandle, token: &Arc<()>) -> bool {
    let app_state = app.state::<AppState>();
    let runtime_state = app_state.lock();
    runtime_state
        .host_thread
        .as_ref()
        .is_some_and(|host_thread| Arc::ptr_eq(&host_thread.token, token))
}

/// Clear `host_thread` only if it still belongs to the thread identified by
/// `token`, so a finishing old session never clears a newer one.
pub(crate) fn clear_own_host_thread(app: &AppHandle, token: &Arc<()>) {
    let mut runtime_state = app.state::<AppState>().inner().lock();
    if runtime_state
        .host_thread
        .as_ref()
        .is_some_and(|host_thread| Arc::ptr_eq(&host_thread.token, token))
    {
        runtime_state.host_thread = None;
    }
}
#[tauri::command]
pub(crate) fn stop_hosting(state: tauri::State<'_, AppState>, app: AppHandle) {
    let (host_thread, pending_request, host_starting) = {
        let mut s = state.lock();
        (s.host_thread.take(), s.pending_request.take(), s.host_starting.take())
    };
    drop(pending_request);
    // Releasing the reservation also makes an in-flight registration task
    // bail out at hand-off if the abort lands after its last await point.
    if let Some(task) = host_starting.and_then(|reservation| reservation.task) {
        task.abort();
    }
    if let Some(host_thread) = host_thread {
        let _ = host_thread.stop_tx.send(());
        let _ = host_thread.join.join();
    } else {
        emit_host_status(&app, "offline");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(list: &[&str]) -> Vec<String> { list.iter().map(|url| (*url).to_owned()).collect() }

    #[test]
    fn a_relay_that_refused_the_id_is_skipped_while_others_remain() {
        let relays = urls(&["wss://a", "wss://b", "wss://c"]);
        assert_eq!(relays_to_try(&relays, &[]), relays);
        assert_eq!(relays_to_try(&relays, &urls(&["wss://a"])), urls(&["wss://b", "wss://c"]));
        // Every relay refused it: keep trying them all so the error stays visible.
        assert_eq!(relays_to_try(&relays, &relays), relays);
    }

    #[test]
    fn disconnect_keeps_hosting_but_stop_leaves() {
        // Viewer disconnected by the host: keep hosting.
        assert!(!should_stop_hosting(true, true));
        // Stop signal, no disconnect: leave the loop.
        assert!(should_stop_hosting(true, false));
        // Session ended on its own: not a signal, so keep hosting.
        assert!(!should_stop_hosting(false, false));
    }

    #[test]
    fn conflicts_explain_each_case() {
        let copy = identity_conflict("wss://a", "123456789", true, true);
        assert_eq!(copy.kind, "in_use");
        assert!(copy.message.contains("copy of this one"), "{}", copy.message);
        let lost = identity_conflict("wss://a", "123456789", false, true);
        assert_eq!(lost.kind, "taken");
        assert!(lost.message.contains("now belongs to another device") && lost.message.contains("Get a new ID"), "{}", lost.message);
        let other_relay = identity_conflict("wss://b", "123456789", false, false);
        assert!(other_relay.message.contains("can't be reached through that relay"), "{}", other_relay.message);
        assert!(other_relay.message.contains("wss://b") && other_relay.message.contains("123456789"));
    }
}
