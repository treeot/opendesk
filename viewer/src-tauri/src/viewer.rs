//! Viewer: connecting to a host, input, monitor selection, and 2FA codes.

use super::*;
use tauri::ipc::{Channel, InvokeResponseBody};

pub(crate) fn can_reserve_viewer_slot(viewer_task_reserved: bool, viewer_stop_reserved: bool) -> bool {
    !viewer_task_reserved && !viewer_stop_reserved
}

pub(crate) fn enqueue_input(sender: Option<mpsc::Sender<Vec<u8>>>, bytes: Vec<u8>) -> Result<(), String> {
    sender
        .ok_or_else(|| "no active viewer session".to_string())?
        .try_send(bytes)
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => "viewer input queue is full".into(),
            mpsc::error::TrySendError::Closed(_) => "viewer session is closed".into(),
        })
}

/// Video plaintext header: seq u64, timestamp_us u32, codec u8, flags u8,
/// width u16, height u16 (big-endian). The UI parses it (see video.ts).
pub(crate) const VIDEO_HEADER_LEN: usize = 18;

/// Frames waiting for the UI. Kept short: a stale frame is worse than a
/// dropped one, and the UI recovers from drops by requesting a keyframe.
pub(crate) const VIDEO_QUEUE_DEPTH: usize = 4;

/// The session screen's video channel. Frames go to the webview as raw bytes
/// (header plus access unit, exactly as received) with no JSON or base64.
#[derive(Default)]
pub(crate) struct VideoSink(Mutex<Option<Channel<InvokeResponseBody>>>);

impl VideoSink {
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Channel<InvokeResponseBody>>> {
        lock_unpoisoned(&self.0)
    }

    /// Route frames to `channel`, replacing any earlier one.
    pub(crate) fn attach(&self, channel: Channel<InvokeResponseBody>) {
        *self.slot() = Some(channel);
    }

    /// Stop sending to channel `id`. A stale id (an unmounted screen whose
    /// successor already attached) leaves the current channel alone.
    pub(crate) fn detach(&self, id: u32) {
        let mut slot = self.slot();
        if slot.as_ref().is_some_and(|channel| channel.id() == id) {
            *slot = None;
        }
    }

    pub(crate) fn clear(&self) {
        *self.slot() = None;
    }

    /// Forward one frame; dropped when no screen is listening.
    pub(crate) fn send(&self, frame: Vec<u8>) {
        let channel = self.slot().clone();
        if let Some(channel) = channel {
            let _ = channel.send(InvokeResponseBody::Raw(frame));
        }
    }
}

/// Per-monitor aux video channels, keyed by monitor id. Each extra monitor
/// window registers its own channel; frames are routed by the id the host
/// stamped on the aux stream.
#[derive(Default)]
pub(crate) struct AuxVideoSinks(Mutex<std::collections::HashMap<u32, Channel<InvokeResponseBody>>>);

impl AuxVideoSinks {
    fn map(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<u32, Channel<InvokeResponseBody>>> {
        lock_unpoisoned(&self.0)
    }

    /// Route monitor `id`'s frames to `channel`, replacing any earlier one.
    pub(crate) fn attach(&self, id: u32, channel: Channel<InvokeResponseBody>) {
        self.map().insert(id, channel);
    }

    /// Stop sending to monitor `id`'s window, if its channel still matches
    /// `channel_id` (a stale detach from a replaced window is ignored).
    pub(crate) fn detach(&self, id: u32, channel_id: u32) {
        let mut map = self.map();
        if map.get(&id).is_some_and(|channel| channel.id() == channel_id) {
            map.remove(&id);
        }
    }

    pub(crate) fn clear(&self) {
        self.map().clear();
    }

    /// Forward one frame to monitor `id`'s window; dropped when none listens.
    pub(crate) fn send(&self, id: u32, frame: Vec<u8>) {
        let channel = self.map().get(&id).cloned();
        if let Some(channel) = channel {
            let _ = channel.send(InvokeResponseBody::Raw(frame));
        }
    }
}

/// How often the session toolbar's connection badge is refreshed.
const LINK_STATS_INTERVAL: Duration = Duration::from_secs(1);

/// How long to wait for the relay's pairing reply before treating the socket as
/// dead. The relay answers a connect request at once (pairing or an error), so
/// silence means a half-open socket (e.g. a VPN re-keying). Without this bound
/// a reconnect attempt blocks here forever, past the give-up window, and the
/// viewer freezes on the last frame instead of retrying or ending the session.
const PAIRING_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Sends the session's path and RTT to the UI until aborted.
fn spawn_link_stats(app: AppHandle, link: host_core::transport::LinkStatus) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut ticks = tokio::time::interval(LINK_STATS_INTERVAL);
        loop {
            ticks.tick().await;
            emit(&app, "link_stats", LinkStatsEvent::new(&link, Instant::now()));
        }
    })
}

#[tauri::command]
pub(crate) fn start_video(sink: tauri::State<'_, VideoSink>, channel: Channel<InvokeResponseBody>) {
    sink.attach(channel);
}

#[tauri::command]
pub(crate) fn stop_video(sink: tauri::State<'_, VideoSink>, id: u32) {
    sink.detach(id);
}

/// A monitor window registers its video channel for monitor `monitor`.
#[tauri::command]
pub(crate) fn start_monitor_video(sinks: tauri::State<'_, AuxVideoSinks>, monitor: u32, channel: Channel<InvokeResponseBody>) {
    sinks.attach(monitor, channel);
}

/// A monitor window stops receiving frames (it closed or reloaded).
#[tauri::command]
pub(crate) fn stop_monitor_video(sinks: tauri::State<'_, AuxVideoSinks>, monitor: u32, id: u32) {
    sinks.detach(monitor, id);
}

/// Ask the host to start a per-monitor aux stream for `id`. The answer arrives
/// as `monitor_stream_opened` or `monitor_stream_refused`.
#[tauri::command]
pub(crate) async fn open_monitor_stream(state: tauri::State<'_, AppState>, id: u32) -> Result<(), String> {
    send_control(&state, Control::OpenMonitorStream { id }).await
}

/// Ask the host to stop the aux stream for `id`.
#[tauri::command]
pub(crate) async fn close_monitor_stream(state: tauri::State<'_, AppState>, id: u32) -> Result<(), String> {
    send_control(&state, Control::CloseMonitorStream { id }).await
}

/// Point the host's input at monitor `id` without switching the main view.
#[tauri::command]
pub(crate) async fn set_input_monitor(state: tauri::State<'_, AppState>, id: u32) -> Result<(), String> {
    send_control(&state, Control::SetInputMonitor { id }).await
}

/// Ask the host to restart its computer; the viewer then auto-reconnects when
/// it comes back. Only offered when the host sent `RestartSupport`.
#[tauri::command]
pub(crate) async fn request_remote_restart(state: tauri::State<'_, AppState>) -> Result<(), String> {
    send_control(&state, Control::RequestRestart).await
}

/// Turn the host's privacy mode on or off (blank its screen, ignore its local
/// input). Only offered when the host sent `PrivacyModeSupport`.
#[tauri::command]
pub(crate) async fn set_privacy_mode(state: tauri::State<'_, AppState>, on: bool) -> Result<(), String> {
    send_control(&state, Control::SetPrivacyMode { on }).await
}

/// Cap the stream bitrate at `bps`, or `0` for the host's automatic behavior.
#[tauri::command]
pub(crate) async fn set_max_bitrate(state: tauri::State<'_, AppState>, bps: u32) -> Result<(), String> {
    send_control(&state, Control::SetMaxBitrate { bps }).await
}

/// Tell the host which codecs this viewer can decode, so it may switch the
/// stream to HEVC where supported (it stays on H.264 otherwise).
#[tauri::command]
pub(crate) async fn set_decoder_codecs(state: tauri::State<'_, AppState>, hevc: bool) -> Result<(), String> {
    send_control(&state, Control::DecoderCodecs { hevc }).await
}

/// Viewer: forward local `127.0.0.1:local_port` to the host's `127.0.0.1:remote_port`.
#[tauri::command]
pub(crate) async fn start_forward(app: AppHandle, local_port: u16, remote_port: u16) -> Result<(), String> {
    let manager = app.state::<std::sync::Arc<crate::tunnel::TunnelManager>>().inner().clone();
    crate::tunnel::start_forward(manager, local_port, remote_port).await
}

/// Viewer: stop a forward by its local port.
#[tauri::command]
pub(crate) fn stop_forward(app: AppHandle, local_port: u16) {
    crate::tunnel::stop_forward(app.state::<std::sync::Arc<crate::tunnel::TunnelManager>>().inner(), local_port);
}

/// Viewer: the active forwards as `(local, remote)` port pairs.
#[tauri::command]
pub(crate) fn list_forwards(app: AppHandle) -> Vec<(u16, u16)> {
    crate::tunnel::list_forwards(app.state::<std::sync::Arc<crate::tunnel::TunnelManager>>().inner())
}

/// Which of `ids` (saved devices) have a host online on the configured relays
/// right now. Used by the address book's status dots.
#[tauri::command]
pub(crate) async fn get_presence(state: tauri::State<'_, AppState>, ids: Vec<String>) -> Result<Vec<String>, String> {
    let relays = { state.lock().config.config.relay_urls.clone() };
    host_core::relay_client::query_presence(&relays, &ids).await
}

#[tauri::command]
pub(crate) fn connect(
    state: tauri::State<'_, AppState>,
    app: AppHandle,
    target_id: String,
    password: String,
    auth_code: Option<String>,
) -> Result<(), String> {
    if target_id.len() != 9 || !target_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("target ID must be 9 digits".into());
    }
    // An empty password means "use the one saved for this device", so the
    // saved secret never has to pass through the UI.
    let password = if password.is_empty() {
        match crate::unattended::read_contact_password(&crate::unattended::Keyring, &target_id) {
            crate::unattended::Lookup::Found(saved) => saved,
            crate::unattended::Lookup::Missing => return Err("Enter the device password.".into()),
            crate::unattended::Lookup::Unavailable(message) => return Err(message),
        }
    } else {
        password
    };
    state
        .lock()
        .config
        .add_recent(&target_id)
        .map_err(|e| e.to_string())?;
    // A contact is only touched when it already exists; the regular recent-ID
    // history above continues to cover connections made without saving one.
    let touch_result = state.lock().contacts.touch(&target_id);
    if let Err(error) = touch_result {
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error.to_string());
        }
    }
    emit(&app, VIEWER_STATUS, StatusEvent { state: "connecting" });
    let (relay_urls, viewer_id) = {
        let mut state = state.lock();
        (state.config.config.relay_urls.clone(), state.config.ensure_identity())
    };
    let target = target_id.clone();
    let task_app = app.clone();
    let viewer_stop = Arc::new(AtomicBool::new(false));
    {
        let mut runtime_state = state.lock();
        if !can_reserve_viewer_slot(
            runtime_state.viewer_task.is_some(),
            runtime_state.viewer_stop.is_some(),
        ) {
            return Err("viewer session is already active".into());
        }
        // Reserve the slot before spawning. `viewer_stop` is the reservation
        // marker while the JoinHandle is being created and installed.
        runtime_state.viewer_stop = Some(viewer_stop.clone());
    }
    let task_viewer_stop = viewer_stop.clone();
    let mut task = Some(tauri::async_runtime::spawn(async move {
        let result = async {
            // Retry state for the current outage: attempts so far and when the
            // outage began. Reset once an attempt actually streams media, so
            // every drop of a working session gets a full reconnect window.
            let mut reconnect_attempt = 0;
            let mut outage_started: Option<Instant> = None;
            let mut session_started = false;
            // Set when the host says it is restarting, so the reconnect loop
            // waits the long restart window instead of the short transient one.
            let restart_expected = Arc::new(AtomicBool::new(false));
            loop {
                if task_viewer_stop.load(Ordering::Relaxed) { break Ok(()); }
                emit(&task_app, VIEWER_STATUS, StatusEvent { state: "connecting" });
                // Set once this attempt streams video; only used to restart the
                // reconnect window, never to infer an auth failure.
                let media_received = Arc::new(AtomicBool::new(false));
                // Clone per attempt: each iteration moves these into the session
                // task, so the originals must survive for the next reconnect.
                let password = password.clone();
                let auth_code = auth_code.clone();
                let attempt_result = async {
            let (_, mut ws) = host_core::relay_client::connect_best(&relay_urls).await?;
            host_core::relay_client::connect_to(&mut ws, &target, &viewer_id)
                .await
                .map_err(|error| error.to_string())?;

            let paired = tokio::time::timeout(PAIRING_REPLY_TIMEOUT, ws.next())
                .await
                .map_err(|_| "relay did not answer the connection request".to_string())?;
            let Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(message))) = paired
            else {
                return Err::<(), String>("relay closed while connecting".into());
            };
            let Some((Tag::Control, body)) = frame::unwrap(&message) else {
                return Err("invalid pairing message".into());
            };
            let session = match protocol::control::Control::from_bytes(body) {
                Some(protocol::control::Control::Paired { session, .. }) => session,
                Some(protocol::control::Control::Error { code }) => {
                    let refusal = host_core::reconnect::connect_refusal_message(&code);
                    return Err(refusal.unwrap_or("invalid pairing message").into());
                }
                _ => return Err("invalid pairing message".into()),
            };
            let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(VIDEO_QUEUE_DEPTH);
            let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>(256);
            let (monitor_tx, mut monitor_rx) = mpsc::channel::<Vec<u8>>(16);
            let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(16);
            let (aux_tx, mut aux_rx) = mpsc::channel::<(u32, Vec<u8>)>(16);
            task_app.state::<AppState>().lock().viewer_input = Some(input_tx);
            // Fresh session: drop any monitor-window channels from a prior one.
            task_app.state::<AuxVideoSinks>().clear();

            let frame_app = task_app.clone();
            let frame_media_received = media_received.clone();
            let _frame_task = AbortOnDrop::new(tauri::async_runtime::spawn(async move {
                while let Some(plaintext) = frame_rx.recv().await {
                    if plaintext.len() < VIDEO_HEADER_LEN {
                        continue;
                    }
                    frame_media_received.store(true, Ordering::Relaxed);
                    frame_app.state::<VideoSink>().send(plaintext);
                }
            }));

            let audio_app = task_app.clone();
            let audio_muted = task_app.state::<AppState>().lock().audio_muted.clone();
            let _audio_task = AbortOnDrop::new(tauri::async_runtime::spawn(async move {
                let mut decoder: Option<(u32, u8, opus::Decoder)> = None;
                while let Some(body) = audio_rx.recv().await {
                    if audio_muted.load(Ordering::Relaxed) { continue; }
                    if let Some(frame) = protocol::audio::AudioFrame::from_bytes(&body) {
                        let needs_new_decoder = decoder.as_ref().is_none_or(|(rate, channels, _)| *rate != frame.sample_rate || *channels != frame.channels);
                        if needs_new_decoder {
                            let channels = if frame.channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
                            decoder = opus::Decoder::new(frame.sample_rate, channels).ok().map(|decoder| (frame.sample_rate, frame.channels, decoder));
                        }
                        let Some((_, channels, decoder_ref)) = decoder.as_mut() else { continue };
                        let frame_capacity = usize::from(frame.frame_duration_ms) * frame.sample_rate as usize / 1_000 * usize::from(*channels);
                        let mut samples = vec![0i16; frame_capacity];
                        let Ok(sample_count) = decoder_ref.decode(&frame.payload, &mut samples, false) else { continue };
                        samples.truncate(sample_count * usize::from(*channels));
                        let mut pcm = Vec::with_capacity(samples.len() * 2);
                        for sample in &samples { pcm.extend_from_slice(&sample.to_le_bytes()); }
                        emit(&audio_app, "audio_frame", AudioFrameEvent { sample_rate: frame.sample_rate, channels: *channels, samples: b64(&pcm) });
                    }
                }
            }));

            let aux_app = task_app.clone();
            let _aux_task = AbortOnDrop::new(tauri::async_runtime::spawn(async move {
                while let Some((monitor, plaintext)) = aux_rx.recv().await {
                    if plaintext.len() < VIDEO_HEADER_LEN { continue; }
                    aux_app.state::<AuxVideoSinks>().send(monitor, plaintext);
                }
            }));

            let monitor_app = task_app.clone();
            let monitor_restart = Arc::clone(&restart_expected);
            let monitor_target = target.clone();
            let _monitor_task = AbortOnDrop::new(tauri::async_runtime::spawn(async move {
                while let Some(body) = monitor_rx.recv().await {
                    match Control::from_bytes(&body) {
                        Some(Control::MonitorList { monitors }) => emit(&monitor_app, "monitor_list", MonitorListEvent { monitors }),
                        Some(Control::VirtualDisplaySupport { max }) => emit(&monitor_app, "virtual_display_support", VirtualDisplaySupportEvent { max }),
                        Some(Control::VirtualDisplayAdded { id }) => emit(&monitor_app, "virtual_display_added", VirtualDisplayAddedEvent { id }),
                        Some(Control::VirtualDisplayRefused { code }) => emit(&monitor_app, "virtual_display_refused", VirtualDisplayRefusedEvent { code }),
                        Some(Control::SecureAttentionSupport) => emit(&monitor_app, "secure_attention_support", ()),
                        Some(Control::MultiMonitorSupport { max }) => emit(&monitor_app, "multi_monitor_support", MultiMonitorSupportEvent { max }),
                        Some(Control::MonitorStreamOpened { id }) => emit(&monitor_app, "monitor_stream_opened", MonitorStreamEvent { id }),
                        Some(Control::MonitorStreamClosed { id }) => emit(&monitor_app, "monitor_stream_closed", MonitorStreamEvent { id }),
                        Some(Control::MonitorStreamRefused { id, code }) => emit(&monitor_app, "monitor_stream_refused", MonitorStreamRefusedEvent { id, code }),
                        Some(Control::RestartSupport) => emit(&monitor_app, "restart_support", ()),
                        Some(Control::Restarting) => {
                            monitor_restart.store(true, Ordering::Relaxed);
                            emit(&monitor_app, "host_restarting", ());
                        }
                        Some(Control::PrivacyModeSupport) => emit(&monitor_app, "privacy_mode_support", ()),
                        Some(Control::PrivacyModeChanged { on }) => emit(&monitor_app, "privacy_mode_changed", PrivacyModeEvent { on }),
                        Some(Control::CursorShape { width, height, hot_x, hot_y, rgba }) => emit(&monitor_app, "cursor_shape", CursorShapeEvent { width, height, hot_x, hot_y, rgba: b64(&rgba) }),
                        Some(Control::CursorHidden) => emit(&monitor_app, "cursor_hidden", ()),
                        Some(Control::HostMac { mac }) => {
                            // Save the host's MAC to a saved contact for Wake-on-LAN, but
                            // only to fill an empty slot: a user-set MAC is left alone, and
                            // devices that aren't saved contacts (set_mac errors) are skipped.
                            let state = monitor_app.state::<AppState>();
                            let saved = { let mut guard = state.lock(); if guard.contacts.mac(&monitor_target).is_none() { guard.contacts.set_mac(&monitor_target, &mac).ok() } else { None } };
                            if let Some(contact) = saved {
                                emit(&monitor_app, "contact_mac", ContactMacEvent { id: monitor_target.clone(), mac: contact.mac.clone().unwrap_or_default() });
                            }
                        }
                        _ => {}
                    }
                }
            }));

            let (data_ready_tx, data_ready_rx) = oneshot::channel();
            let mut session_task = AbortOnDrop::new(tauri::async_runtime::spawn(async move {
                host_core::roles::run_viewer_session_with_aux(
                    ws,
                    &password,
                    &session,
                    frame_tx,
                    input_rx,
                    Some(data_ready_tx),
                    Some(monitor_tx),
                    Some(audio_tx),
                    Some(aux_tx),
                    auth_code.clone(),
                )
                .await
            }));
            match data_ready_rx.await {
                Ok(handle) => {
                    // Ends with this attempt; a reconnect reports its own path.
                    let _link_stats = AbortOnDrop::new(spawn_link_stats(task_app.clone(), handle.link.clone()));
                    activate_data_channel(&task_app, handle, target.clone(), false);
                    emit(&task_app, VIEWER_STATUS, StatusEvent { state: "in_session" });
                    if !session_started {
                        emit(
                            &task_app,
                            "session_started",
                            SessionStarted {
                                role: "viewer",
                                remote_id: target.clone(),
                            },
                        );
                        session_started = true;
                    }
                    match session_task.join().await {
                        Ok(result) => result.map_err(|error| error.to_string()),
                        Err(error) => Err(format!("viewer session task failed: {error}")),
                    }
                }
                Err(_) => match session_task.join().await {
                    Ok(result) => result.map_err(|error| error.to_string()),
                    Err(error) => Err(format!("viewer session task failed: {error}")),
                },
            }
                }
                .await;
                match attempt_result {
                    Ok(()) => break Ok(()),
                    Err(message) => {
                        clear_data_channel(&task_app);
                        task_app.state::<AppState>().lock().viewer_input = None;
                        let media_received = media_received.load(Ordering::Relaxed);
                        let reason = host_core::reconnect::classify_viewer_error(&message);
                        // Auth failures are only explicit signals (failed PAKE
                        // or the host's Control::Error rejection) and never
                        // retry: the password/2FA code will not get better (a
                        // TOTP code is single-use), and a host user already
                        // declined.
                        if !host_core::reconnect::should_retry_viewer(reason, session_started) {
                            // Keep the raw rejection for a 2FA code so the
                            // final handler shows the popup, not an error toast.
                            if matches!(
                                host_core::reconnect::rejection_code(&message),
                                Some(host_core::reconnect::reject_code::TWOFA_REQUIRED | host_core::reconnect::reject_code::TWOFA_INVALID)
                            ) {
                                break Err(message);
                            }
                            break Err(host_core::reconnect::stop_message(&message, reason, session_started));
                        }
                        if media_received {
                            reconnect_attempt = 0;
                            outage_started = None;
                            // Reconnected and streaming again: the restart (if any) is over.
                            restart_expected.store(false, Ordering::Relaxed);
                        }
                        // A restart means a long gap is expected; wait the longer window.
                        let window = if restart_expected.load(Ordering::Relaxed) {
                            host_core::reconnect::VIEWER_RESTART_WINDOW
                        } else {
                            host_core::reconnect::VIEWER_RECONNECT_WINDOW
                        };
                        let elapsed = outage_started.get_or_insert_with(Instant::now).elapsed();
                        let Some(delay) = host_core::reconnect::viewer_retry_delay_within(reconnect_attempt, elapsed, window) else {
                            break Err(format!("{message}; gave up reconnecting after {}s", window.as_secs()));
                        };
                        reconnect_attempt += 1;
                        emit(&task_app, "error", ErrorEvent { message: format!("viewer session dropped ({message}); reconnecting (attempt {reconnect_attempt})") });
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }.await;

        if let Err(message) = &result {
            let message = message.clone();
            match host_core::reconnect::rejection_code(&message) {
                Some(host_core::reconnect::reject_code::TWOFA_REQUIRED) => {
                    emit(&task_app, "twofa_prompt", TwofaPromptEvent { reason: "required" });
                }
                Some(host_core::reconnect::reject_code::TWOFA_INVALID) => {
                    emit(&task_app, "twofa_prompt", TwofaPromptEvent { reason: "invalid" });
                }
                _ => emit(&task_app, "error", ErrorEvent { message }),
            }
        }
        task_app.state::<VideoSink>().clear();
        let app_state = task_app.state::<AppState>();
        let mut state = app_state.lock();
        state.viewer_input = None;
        state.viewer_task = None;
        state.viewer_stop = None;
        drop(state);
        clear_data_channel(&task_app);
        emit(
            &task_app,
            "session_ended",
            SessionEnded {
                reason: if result.is_ok() {
                    "ended".into()
                } else {
                    "error".into()
                },
            },
        );
    }));
    let installed = {
        let mut runtime_state = state.lock();
        if runtime_state
            .viewer_stop
            .as_ref()
            .is_some_and(|stop| Arc::ptr_eq(stop, &viewer_stop))
        {
            runtime_state.viewer_task = task.take();
            true
        } else {
            false
        }
    };
    if !installed {
        // disconnect() or an early task completion rolled back the
        // reservation before the handle could be installed.
        if let Some(task) = task {
            task.abort();
        }
        return Err("viewer session was stopped before it started".into());
    }
    Ok(())
}
#[tauri::command]
pub(crate) fn send_input(state: tauri::State<'_, AppState>, bytes: Vec<u8>) -> Result<(), String> {
    let sender = state.lock().viewer_input.clone();
    enqueue_input(sender, bytes)
}

#[tauri::command]
pub(crate) async fn select_monitor(state: tauri::State<'_, AppState>, id: u32) -> Result<(), String> {
    let (sender, sealer) = {
        let state = state.lock();
        (state.control_sender.clone(), state.control_sealer.clone())
    };
    let sender = sender.ok_or_else(|| "no active viewer session".to_owned())?;
    let sealer = sealer.ok_or_else(|| "control channel is not ready".to_owned())?;
    let body = Control::SelectMonitor { id }.to_bytes();
    let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Control as u8], &body);
    sender.send_frame(Tag::Control, &sealed).await.map_err(|error| error.to_string())
}

/// Asks the host for a virtual display of `width` x `height` pixels. The
/// answer arrives as `virtual_display_added` or `virtual_display_refused`.
#[tauri::command]
pub(crate) async fn add_virtual_display(state: tauri::State<'_, AppState>, width: u32, height: u32, hidpi: bool) -> Result<(), String> {
    send_control(&state, Control::AddVirtualDisplay { width, height, hidpi }).await
}

#[tauri::command]
pub(crate) async fn remove_virtual_display(state: tauri::State<'_, AppState>, id: u32) -> Result<(), String> {
    send_control(&state, Control::RemoveVirtualDisplay { id }).await
}

/// Ask the host to deliver Ctrl+Alt+Del on its side, so the viewer can reach
/// the Windows security screen. Hosts that can't honor it ignore it.
#[tauri::command]
pub(crate) async fn send_secure_attention(state: tauri::State<'_, AppState>) -> Result<(), String> {
    send_control(&state, Control::RequestSecureAttention).await
}

async fn send_control(state: &tauri::State<'_, AppState>, control: Control) -> Result<(), String> {
    let (sender, sealer) = { let state = state.lock(); (state.control_sender.clone(), state.control_sealer.clone()) };
    let sender = sender.ok_or_else(|| "no active viewer session".to_owned())?;
    let sealer = sealer.ok_or_else(|| "control channel is not ready".to_owned())?;
    let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Control as u8], &control.to_bytes());
    sender.send_frame(Tag::Control, &sealed).await.map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn send_auth_code(state: tauri::State<'_, AppState>, code: String) -> Result<(), String> {
    let (sender, sealer) = { let state = state.lock(); (state.control_sender.clone(), state.control_sealer.clone()) };
    let sender = sender.ok_or_else(|| "no active viewer session".to_owned())?;
    let sealer = sealer.ok_or_else(|| "control channel is not ready".to_owned())?;
    let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Control as u8], &Control::AuthCode { code }.to_bytes());
    sender.send_frame(Tag::Control, &sealed).await.map_err(|error| error.to_string())
}

/// Ask the host for a fresh keyframe after the UI dropped frames or its
/// decoder failed. The UI rate-limits these.
#[tauri::command]
pub(crate) async fn request_keyframe(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let (sender, sealer) = { let state = state.lock(); (state.control_sender.clone(), state.control_sealer.clone()) };
    let sender = sender.ok_or_else(|| "no active viewer session".to_owned())?;
    let sealer = sealer.ok_or_else(|| "control channel is not ready".to_owned())?;
    let sealed = lock_unpoisoned(&sealer).seal(&[Tag::Control as u8], &Control::RequestKeyframe.to_bytes());
    sender.send_frame(Tag::Control, &sealed).await.map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Received = Arc<Mutex<Vec<Vec<u8>>>>;

    fn recording_channel() -> (Channel<InvokeResponseBody>, Received) {
        let received = Received::default();
        let log = received.clone();
        let channel = Channel::new(move |body| {
            if let InvokeResponseBody::Raw(bytes) = body {
                log.lock().unwrap().push(bytes);
            }
            Ok(())
        });
        (channel, received)
    }

    #[test]
    fn video_sink_forwards_raw_frames_to_the_attached_channel() {
        let sink = VideoSink::default();
        sink.send(vec![1]);
        let (channel, received) = recording_channel();
        sink.attach(channel);
        sink.send(vec![0; VIDEO_HEADER_LEN + 2]);
        assert_eq!(*received.lock().unwrap(), vec![vec![0; VIDEO_HEADER_LEN + 2]]);
    }

    #[test]
    fn video_sink_ignores_a_stale_detach() {
        let sink = VideoSink::default();
        let (old, _) = recording_channel();
        let old_id = old.id();
        let (new, received) = recording_channel();
        let new_id = new.id();
        sink.attach(old);
        sink.attach(new);
        sink.detach(old_id);
        sink.send(vec![7]);
        assert_eq!(received.lock().unwrap().len(), 1);
        sink.detach(new_id);
        sink.send(vec![8]);
        assert_eq!(received.lock().unwrap().len(), 1);
    }

    #[test]
    fn video_sink_clear_drops_the_channel() {
        let sink = VideoSink::default();
        let (channel, received) = recording_channel();
        sink.attach(channel);
        sink.clear();
        sink.send(vec![1]);
        assert!(received.lock().unwrap().is_empty());
    }
}
