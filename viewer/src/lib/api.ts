import { Channel, invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { disable as disableAutostart, enable as enableAutostart, isEnabled as isAutostartEnabled } from "@tauri-apps/plugin-autostart";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

export type Status = "offline" | "online" | "connecting" | "in_session";
export interface Identity { id: string; password: string }
/** A relay refusing this device's ID: another device's ID there ("taken"), or a copy of this device online with it ("in_use"). */
export interface IdentityConflict { relay: string; kind: "taken" | "in_use"; message: string }
export interface AppConfig { relay_url: string; relay_urls: string[]; recent_ids: string[]; clipboard_sync: boolean; allow_virtual_displays: boolean }
/** `ready` means the keyring answered and connections skip approval; `problem` says why not when it is on. */
export interface UnattendedStatus { enabled: boolean; has_password: boolean; twofa_enabled: boolean; ready: boolean; problem: string | null }
export interface WindowsServiceStatus { supported: boolean; installed: boolean; running: boolean }
export interface PermissionStatus { screen_recording: boolean; accessibility: boolean }
export interface IncomingRequest { peer_id: string; fingerprint: string; known: boolean }
export interface SessionStarted { role: "host" | "viewer"; remote_id: string }
export interface SessionEnded { reason: string }
export interface ErrorEvent { message: string }
/** The host wants a 2FA code: "required" (none sent) or "invalid" (rejected). */
export interface TwofaPrompt { reason: "required" | "invalid" }
export interface StatusChanged { state: Status }
export interface AudioFrame { sample_rate: number; channels: number; samples: string } // base64 of little-endian i16 PCM
export interface FileOffer { id: number; name: string; size: number; from: string; dir: string }
export interface FileProgress { id: number; transferred: number; total: number; direction: "send" | "receive" | string }
export interface FileDone { id: number; path?: string | null }
export interface FileError { id: number; message: string }
export interface Contact { id: string; alias: string; last_connected: number | null }
export interface TotpEnrollment { uri: string; secret: string; qr_width: number; qr_modules: boolean[] }
/** `is_virtual`: a virtual display this session added, which the viewer may remove. */
export interface MonitorInfo { id: number; name: string; width: number; height: number; is_primary: boolean; is_virtual?: boolean }
/** Media path of a viewer session and its measured round trip; `rtt_ms` is null until there is a real sample. */
export interface LinkStats { path: "direct" | "relay"; rtt_ms: number | null }
/** Host-enforced capabilities for the connected viewer; all default to enabled and reset each session. */
export interface ViewerPermissions { control: boolean; files: boolean; clipboard: boolean; audio: boolean; forwarding: boolean }
export type Capability = keyof ViewerPermissions;
/** One line in the host's session activity log; `at` is Unix milliseconds. */
export interface Activity { at: number; kind: string; detail: string }

export const getIdentity = () => invoke<Identity>("get_identity");
export const rotatePassword = () => invoke<{ password: string }>("rotate_password");
/** Replaces this device's ID and registration token; resolves to the new ID. */
export const resetIdentity = () => invoke<string>("reset_identity");
export const getIdentityConflicts = () => invoke<IdentityConflict[]>("get_identity_conflicts");
export const getConfig = () => invoke<AppConfig>("get_config");
export const listContacts = () => invoke<Contact[]>("list_contacts");
export const listAllowed = () => invoke<string[]>("list_allowed");
export const addAllowed = (peer_id: string) => invoke<void>("add_allowed", { peerId: peer_id });
export const removeAllowed = (peer_id: string) => invoke<void>("remove_allowed", { peerId: peer_id });
export const addContact = (alias: string, peer_id: string) => invoke<Contact>("add_contact", { alias, peerId: peer_id });
export const removeContact = (peer_id: string) => invoke<void>("remove_contact", { peerId: peer_id });
export const touchContact = (peer_id: string) => invoke<void>("touch_contact", { peerId: peer_id });
export const setRelay = (url: string) => invoke<void>("set_relay", { url });
export const setRelays = (urls: string[]) => invoke<void>("set_relays", { urls });
export const listRelays = () => invoke<string[]>("list_relays");
export const getUnattendedStatus = () => invoke<UnattendedStatus>("get_unattended_status");
export const getWindowsServiceStatus = () => invoke<WindowsServiceStatus>("windows_service_status");
export const installWindowsService = () => invoke<WindowsServiceStatus>("install_windows_service");
export const uninstallWindowsService = () => invoke<WindowsServiceStatus>("uninstall_windows_service");
export const setUnattendedAccess = (enabled: boolean, password: string | null) => invoke<UnattendedStatus>("set_unattended_access", { enabled, password });
/** Enabling returns a pending enrollment; 2FA is active only after confirmUnattended2fa. */
export const setUnattended2fa = (enabled: boolean) => invoke<TotpEnrollment | null>("set_unattended_2fa", { enabled });
export const confirmUnattended2fa = (code: string) => invoke<UnattendedStatus>("confirm_unattended_2fa", { code });
export const permissionStatus = () => invoke<PermissionStatus>("permission_status");
export const openPermissionSettings = (which: "screen" | "accessibility") => invoke<void>("open_permission_settings", { which });
// Tauri v2 exposes Rust snake_case params to JS as camelCase (peerId/targetId).
export const respondToRequest = (peer_id: string, allow: boolean, always: boolean) => invoke<void>("respond_to_request", { peerId: peer_id, allow, always });
/** Read from the OS sign-in entry each time; there is no saved copy in our config. */
export const getStartAtSignIn = () => isAutostartEnabled();
export const setStartAtSignIn = (enabled: boolean) => enabled ? enableAutostart() : disableAutostart();
/** True when closing the window hides it to the tray instead of quitting. */
export const closeKeepsRunning = () => invoke<boolean>("close_keeps_running");
export const quitApp = () => invoke<void>("quit_app");
export const startHosting = () => invoke<void>("start_hosting");
export const stopHosting = () => invoke<void>("stop_hosting");
export const connect = (target_id: string, password: string, auth_code?: string) => invoke<void>("connect", auth_code ? { targetId: target_id, password, authCode: auth_code } : { targetId: target_id, password });
export const sendAuthCode = (code: string) => invoke<void>("send_auth_code", { code });
export const disconnect = () => invoke<void>("disconnect");
export const sendInput = (bytes: Uint8Array) => invoke<void>("send_input", { bytes: Array.from(bytes) });
export const selectMonitor = (id: number) => invoke<void>("select_monitor", { id });
/** Cap the stream bitrate at `bps`, or 0 for the host's automatic behavior. */
export const setMaxBitrate = (bps: number) => invoke<void>("set_max_bitrate", { bps });
/** Tell the host which codecs this viewer can decode (lets it offer HEVC). */
export const setDecoderCodecs = (hevc: boolean) => invoke<void>("set_decoder_codecs", { hevc });
/** Forward local `127.0.0.1:localPort` to the host's `127.0.0.1:remotePort`. */
export const startForward = (localPort: number, remotePort: number) => invoke<void>("start_forward", { localPort, remotePort });
export const stopForward = (localPort: number) => invoke<void>("stop_forward", { localPort });
/** Active forwards as `[localPort, remotePort]` pairs. */
export const listForwards = () => invoke<[number, number][]>("list_forwards");
export const requestKeyframe = () => invoke<void>("request_keyframe");
export const addVirtualDisplay = (width: number, height: number, hidpi: boolean) => invoke<void>("add_virtual_display", { width, height, hidpi });
export const removeVirtualDisplay = (id: number) => invoke<void>("remove_virtual_display", { id });
export const sendSecureAttention = () => invoke<void>("send_secure_attention");
export const setAllowVirtualDisplays = (enabled: boolean) => invoke<void>("set_allow_virtual_displays", { enabled });
/** Windows' virtual display driver (Amyuni's): `applies` only on 64-bit Windows; `license` is Amyuni's, shown before installing. */
export interface DisplayDriver { applies: boolean; installed: boolean; license: string }
/** `restart`: Windows wants a restart to finish. */
export interface DisplayDriverChange { status: DisplayDriver; restart: boolean }
export const getDisplayDriver = () => invoke<DisplayDriver>("virtual_display_driver_status");
/** Downloads the driver from amyuni.com, checks it, and installs it (Windows asks for administrator approval). */
export const installDisplayDriver = () => invoke<DisplayDriverChange>("install_virtual_display_driver");
export const uninstallDisplayDriver = () => invoke<DisplayDriverChange>("uninstall_virtual_display_driver");
/**
 * Stream video frame messages (raw bytes: header plus access unit, see
 * video.ts) to `handler` over a binary channel. Call the returned function to
 * stop; a later startVideo from a remounted screen is not affected by it.
 */
export function startVideo(handler: (message: ArrayBuffer) => void): () => void {
  const channel = new Channel<ArrayBuffer>(handler);
  const started = invoke<void>("start_video", { channel });
  let stopped = false;
  return () => {
    if (stopped) return;
    stopped = true;
    channel.onmessage = () => {};
    // After start resolves, so the backend never sees stop before start.
    void started.then(() => invoke<void>("stop_video", { id: channel.id })).catch(() => {});
  };
}
/**
 * Like {@link startVideo}, for an extra monitor window. Frames for `monitor`
 * (the host's monitor id) stream to `handler`. Call the returned function to
 * stop.
 */
export function startMonitorVideo(monitor: number, handler: (message: ArrayBuffer) => void): () => void {
  const channel = new Channel<ArrayBuffer>(handler);
  const started = invoke<void>("start_monitor_video", { monitor, channel });
  let stopped = false;
  return () => {
    if (stopped) return;
    stopped = true;
    channel.onmessage = () => {};
    void started.then(() => invoke<void>("stop_monitor_video", { monitor, id: channel.id })).catch(() => {});
  };
}
/** Ask the host to start streaming monitor `id` as its own window. */
export const openMonitorStream = (id: number) => invoke<void>("open_monitor_stream", { id });
/** Ask the host to stop the aux stream for monitor `id`. */
export const closeMonitorStream = (id: number) => invoke<void>("close_monitor_stream", { id });
/** Point the host's input at monitor `id` without switching the main view. */
export const setInputMonitor = (id: number) => invoke<void>("set_input_monitor", { id });
/**
 * Which of `ids` (saved device IDs) have a host online on the configured
 * relays right now. Returns the online subset; resolves to `[]` on any relay
 * error so the address book degrades to "unknown" rather than failing.
 */
export const getPresence = async (ids: string[]): Promise<string[]> => {
  try {
    return await invoke<string[]>("get_presence", { ids });
  } catch {
    return [];
  }
};
export const sendChat = (text: string) => invoke<void>("send_chat", { text });
export const setClipboardSync = (enabled: boolean) => invoke<void>("set_clipboard_sync", { enabled });
export const setAudioMuted = (muted: boolean) => invoke<void>("set_audio_muted", { muted });
/** Host-enforced viewer permissions for the current session. */
export const getViewerPermissions = () => invoke<ViewerPermissions>("get_viewer_permissions");
export const setViewerPermission = (capability: Capability, allowed: boolean) => invoke<void>("set_viewer_permission", { capability, allowed });
/** Ends the current viewer session but keeps hosting (re-registers for the next viewer). */
export const disconnectViewer = () => invoke<void>("disconnect_viewer");
export const sendFile = (path: string) => invoke<number>("send_file", { path });
/** Native file picker; resolves to the chosen path or null when cancelled. */
export const pickFile = async (): Promise<string | null> => {
  const selected = await openDialog({ multiple: false, directory: false, title: "Send file" });
  return typeof selected === "string" ? selected : null;
};
/** Files dropped anywhere on the window; `over` reports drag hover state. */
export const onFileDrop = (handler: (event: { type: "over" | "drop" | "leave"; paths: string[] }) => void): Promise<UnlistenFn> =>
  getCurrentWebview().onDragDropEvent(({ payload }) => {
    if (payload.type === "drop") handler({ type: "drop", paths: payload.paths });
    else if (payload.type === "leave") handler({ type: "leave", paths: [] });
    else handler({ type: "over", paths: [] });
  });
export const respondFile = (id: number, accept: boolean) => invoke<void>("respond_file", { id, accept });
// Recordings stream to disk in chunks (base64, like audio events) so long
// sessions are never held in memory.
export const startRecording = (suggestedName: string) => invoke<number>("start_recording", { suggestedName });
export const appendRecording = (id: number, chunk: ArrayBuffer) => invoke<void>("append_recording", { id, chunk: toBase64(new Uint8Array(chunk)) });
export const finishRecording = (id: number) => invoke<string>("finish_recording", { id });

/** A finished session recording on disk, newest first. */
export interface RecordingEntry { name: string; path: string; size: number; modified: number }
export const listRecordings = () => invoke<RecordingEntry[]>("list_recordings");
export const deleteRecording = (path: string) => invoke<void>("delete_recording", { path });

function toBase64(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(binary);
}

const on = <T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn> => listen<T>(event, ({ payload }) => handler(payload));

/** A native menu item was chosen; the payload is the item's id (see menu.rs). */
export const onMenuAction = (handler: (action: string) => void) => on<string>("menu", handler);
/** The host's cursor image; `rgba` is base64 of straight-alpha RGBA pixels. */
export interface CursorShape { width: number; height: number; hot_x: number; hot_y: number; rgba: string }
export const onCursorShape = (handler: (payload: CursorShape) => void) => on<CursorShape>("cursor_shape", handler);
export const onCursorHidden = (handler: () => void) => on<void>("cursor_hidden", handler);
/** Enable/disable the macOS Session menu and reflect clipboard/recording state. */
export const setSessionMenu = (active: boolean, clipboardSync: boolean, recording: boolean) => invoke<void>("set_session_menu", { active, clipboardSync, recording });
/** Hosting presence only; an outgoing session reports through onViewerStatus. */
export const onStatusChanged = (handler: (payload: StatusChanged) => void) => on("status_changed", handler);
export const onViewerStatus = (handler: (payload: StatusChanged) => void) => on("viewer_status", handler);
export const onIncomingRequest = (handler: (payload: IncomingRequest) => void) => on("incoming_request", handler);
export const onSessionStarted = (handler: (payload: SessionStarted) => void) => on("session_started", handler);
export const onSessionEnded = (handler: (payload: SessionEnded) => void) => on("session_ended", handler);
export const onError = (handler: (payload: ErrorEvent) => void) => on("error", handler);
/** The host requires a 2FA code; the UI opens a popup to collect it. */
export const onTwofaPrompt = (handler: (payload: TwofaPrompt) => void) => on("twofa_prompt", handler);
export const onMonitorList = (handler: (payload: { monitors: MonitorInfo[] }) => void) => on("monitor_list", handler);
/** The host lets this viewer add up to `max` virtual displays; hosts that don't never send it. */
export const onVirtualDisplaySupport = (handler: (payload: { max: number }) => void) => on("virtual_display_support", handler);
export const onVirtualDisplayAdded = (handler: (payload: { id: number }) => void) => on("virtual_display_added", handler);
export const onVirtualDisplayRefused = (handler: (payload: { code: string }) => void) => on("virtual_display_refused", handler);
/** The host can deliver Ctrl+Alt+Del; hosts that can't never send it, so the control stays hidden. */
export const onSecureAttentionSupport = (handler: () => void) => on("secure_attention_support", handler);
export const onMultiMonitorSupport = (handler: (payload: { max: number }) => void) => on("multi_monitor_support", handler);
export const onMonitorStreamOpened = (handler: (payload: { id: number }) => void) => on("monitor_stream_opened", handler);
export const onMonitorStreamClosed = (handler: (payload: { id: number }) => void) => on("monitor_stream_closed", handler);
export const onMonitorStreamRefused = (handler: (payload: { id: number; code: string }) => void) => on("monitor_stream_refused", handler);
/** The host can restart its computer and come back hosting. */
export const onRestartSupport = (handler: () => void) => on("restart_support", handler);
/** The host is restarting; the viewer will auto-reconnect when it returns. */
export const onHostRestarting = (handler: () => void) => on("host_restarting", handler);
/** Ask the host to restart its computer. */
export const requestRemoteRestart = () => invoke<void>("request_remote_restart");
/** The host can blank its screen and ignore its local input (privacy mode). */
export const onPrivacyModeSupport = (handler: () => void) => on("privacy_mode_support", handler);
/** Privacy mode turned on or off on the host. */
export const onPrivacyModeChanged = (handler: (payload: { on: boolean }) => void) => on("privacy_mode_changed", handler);
/** Turn the host's privacy mode on or off. */
export const setPrivacyMode = (on: boolean) => invoke<void>("set_privacy_mode", { on });

/** One entry in a remote directory listing. */
export interface DirEntry { name: string; is_dir: boolean; size: number; modified: number }
/** List a directory on this computer (empty path = home folder). */
export const browseLocal = (path: string) => invoke<{ path: string; entries: DirEntry[] }>("browse_local", { path });
/** Ask the host to list a directory (empty path = its starting folder). The
 * answer arrives via {@link onRemoteListing}. */
export const browseRemote = (path: string) => invoke<void>("browse_remote", { path });
/** A remote directory listing, or an error, for the file manager. */
export const onRemoteListing = (handler: (payload: { path: string; entries: DirEntry[]; error: string | null }) => void) => on("remote_listing", handler);
/** Download a remote file; it arrives as an incoming transfer (saved to Downloads). */
export const pullRemote = (path: string) => invoke<void>("pull_remote", { path });
/** Upload a local file into the remote folder `dir`. Returns the transfer id. */
export const uploadToRemote = (dir: string, path: string) => invoke<number>("upload_to_remote", { dir, path });
/** Something the host user should know, e.g. that the viewer added a display. */
export const onHostNotice = (handler: (payload: { message: string }) => void) => on("host_notice", handler);
export const onLinkStats = (handler: (payload: LinkStats) => void) => on("link_stats", handler);
/** An inbound chat message from the peer; your own sent messages are not echoed. */
export const onChatMessage = (handler: (payload: { text: string }) => void) => on("chat_message", handler);
export const onAudioFrame = (handler: (frame: AudioFrame) => void) => on("audio_frame", handler);
export const onFileOffer = (handler: (payload: FileOffer) => void) => on("file_offer", handler);
export const onFileProgress = (handler: (payload: FileProgress) => void) => on("file_progress", handler);
export const onFileDone = (handler: (payload: FileDone) => void) => on("file_done", handler);
export const onFileError = (handler: (payload: FileError) => void) => on("file_error", handler);
export const onPasswordRotated = (handler: (payload: { password: string }) => void) => on("password_rotated", handler);
export const onIdentityChanged = (handler: (payload: { id: string }) => void) => on("identity_changed", handler);
export const onIdentityConflicts = (handler: (payload: { conflicts: IdentityConflict[] }) => void) => on("identity_conflicts", handler);
/** Host-enforced viewer permissions changed; keeps the control panel in sync. */
export const onViewerPermissions = (handler: (payload: ViewerPermissions) => void) => on("viewer_permissions", handler);
/** A new line for the host's session activity log. */
export const onActivity = (handler: (payload: Activity) => void) => on("activity", handler);
