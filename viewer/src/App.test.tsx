// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

type Listener = (payload: unknown) => void;
const { listeners, mocks } = vi.hoisted(() => {
  const listeners = new Map<string, (payload: unknown) => void>();
  const on = (name: string) => (handler: (payload: unknown) => void) => { listeners.set(name, handler); return Promise.resolve(() => {}); };
  const mocks = {
    onStatusChanged: on("status_changed"), onViewerStatus: on("viewer_status"), onIncomingRequest: on("incoming_request"), onSessionStarted: on("session_started"),
    onSessionEnded: on("session_ended"), onError: on("error"), onTwofaPrompt: on("twofa_prompt"),
    startVideo: () => () => {}, requestKeyframe: vi.fn(), onAudioFrame: on("audio_frame"), onMonitorList: on("monitor_list"), onLinkStats: on("link_stats"),
    onVirtualDisplaySupport: on("virtual_display_support"), onVirtualDisplayAdded: on("virtual_display_added"), onVirtualDisplayRefused: on("virtual_display_refused"), onHostNotice: on("host_notice"),
    onFileOffer: on("file_offer"), onFileProgress: on("file_progress"), onFileDone: on("file_done"), onFileError: on("file_error"),
    onChatMessage: on("chat_message"), sendChat: vi.fn(), onSecureAttentionSupport: on("secure_attention_support"), sendSecureAttention: vi.fn(),
    onMultiMonitorSupport: on("multi_monitor_support"), onMonitorStreamOpened: on("monitor_stream_opened"), onMonitorStreamClosed: on("monitor_stream_closed"), onMonitorStreamRefused: on("monitor_stream_refused"), setInputMonitor: vi.fn(),
    onRestartSupport: on("restart_support"), onHostRestarting: on("host_restarting"), requestRemoteRestart: vi.fn(),
    onPrivacyModeSupport: on("privacy_mode_support"), onPrivacyModeChanged: on("privacy_mode_changed"), setPrivacyMode: vi.fn(),
    browseLocal: vi.fn(() => Promise.resolve({ path: "/home", entries: [] })), browseRemote: vi.fn(() => Promise.resolve()), onRemoteListing: on("remote_listing"), pullRemote: vi.fn(() => Promise.resolve()),
    onFileDrop: on("drop"), onPasswordRotated: on("password_rotated"), onIdentityChanged: on("identity_changed"),
    onIdentityConflicts: on("identity_conflicts"), getIdentityConflicts: () => Promise.resolve([]),
    permissionStatus: vi.fn(), connect: vi.fn(), disconnect: vi.fn(),
    getIdentity: vi.fn(), getConfig: vi.fn(), getUnattendedStatus: vi.fn(), listContacts: vi.fn(), listAllowed: vi.fn(), startHosting: vi.fn(), getWindowsServiceStatus: vi.fn(), getStartAtSignIn: vi.fn(), getDisplayDriver: vi.fn(), closeKeepsRunning: vi.fn(),
    sendInput: vi.fn(), respondToRequest: vi.fn(), respondFile: vi.fn(),
    getViewerPermissions: () => Promise.resolve({ control: true, files: true, clipboard: true, audio: true }),
    setViewerPermission: vi.fn(() => Promise.resolve()), disconnectViewer: vi.fn(() => Promise.resolve()),
    onViewerPermissions: on("viewer_permissions"), onActivity: on("activity"),
    onMenuAction: on("menu"), setSessionMenu: vi.fn(() => Promise.resolve()),
    setMaxBitrate: vi.fn(() => Promise.resolve()),
    setDecoderCodecs: vi.fn(() => Promise.resolve()),
    onCursorShape: on("cursor_shape"), onCursorHidden: on("cursor_hidden"),
    listRecordings: () => Promise.resolve([]), deleteRecording: vi.fn(() => Promise.resolve()),
    isDevBrowser: false,
  };
  return { listeners, mocks };
});
vi.mock("./lib/api", () => mocks);
vi.mock("@tauri-apps/plugin-opener", () => ({ openPath: vi.fn(() => Promise.resolve()), revealItemInDir: vi.fn(() => Promise.resolve()) }));
vi.mock("./lib/decode", () => ({ makeDecoder: () => ({ push: vi.fn(), close: vi.fn() }), supportsHevcDecode: () => Promise.resolve(false) }));
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  WebviewWindow: Object.assign(function () { return { once: (_n: string, cb: () => void) => { cb(); return Promise.resolve(() => {}); }, show: () => Promise.resolve(), hide: () => Promise.resolve(), setFocus: () => Promise.resolve(), isVisible: () => Promise.resolve(false), close: () => Promise.resolve() }; }, { getByLabel: () => Promise.resolve(null) }),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: () => Promise.resolve(() => {}), emit: () => Promise.resolve() }));

import App from "./App";

const emit = (name: string, payload: unknown) => act(() => { (listeners.get(name) as Listener)(payload); });

async function startConnecting() {
  render(<App />);
  fireEvent.change(await screen.findByPlaceholderText("000 000 000"), { target: { value: "987654321" } });
  fireEvent.click(screen.getByRole("button", { name: "Connect" }));
  const dialog = await screen.findByRole("dialog");
  fireEvent.change(within(dialog).getByLabelText("Device password"), { target: { value: "secret" } });
  fireEvent.click(within(dialog).getByRole("button", { name: "Connect" }));
  await screen.findByRole("heading", { name: /Waiting for 987 654 321 to accept/ });
}

describe("App", () => {
  beforeEach(() => {
    mocks.permissionStatus.mockResolvedValue({ screen_recording: true, accessibility: true });
    mocks.connect.mockResolvedValue(undefined);
    mocks.disconnect.mockResolvedValue(undefined);
    mocks.getIdentity.mockResolvedValue({ id: "123456789", password: "654321" });
    mocks.getConfig.mockResolvedValue({ relay_url: "wss://relay", relay_urls: ["wss://relay"], recent_ids: [], clipboard_sync: false, allow_virtual_displays: true });
    mocks.getUnattendedStatus.mockResolvedValue({ enabled: false, has_password: false, twofa_enabled: false, ready: false, problem: null });
    mocks.listContacts.mockResolvedValue([]);
    mocks.listAllowed.mockResolvedValue([]);
    mocks.startHosting.mockResolvedValue(undefined);
    mocks.getWindowsServiceStatus.mockResolvedValue({ supported: false, installed: false, running: false });
    mocks.getStartAtSignIn.mockResolvedValue(false);
    mocks.getDisplayDriver.mockResolvedValue({ applies: false, installed: false, license: "" });
    mocks.closeKeepsRunning.mockResolvedValue(false);
  });
  afterEach(() => { cleanup(); vi.useRealTimers(); listeners.clear(); });

  it("waits for the host to accept before showing the session", async () => {
    await startConnecting();
    expect(screen.queryByText("Encrypted")).toBeNull();
    emit("session_started", { role: "viewer", remote_id: "987654321" });
    expect(await screen.findByRole("toolbar", { name: "Session controls" })).toBeTruthy();
  });

  it("shows the viewer session's own status, not the host's", async () => {
    await startConnecting();
    emit("viewer_status", { state: "in_session" });
    emit("session_started", { role: "viewer", remote_id: "987654321" });
    const toolbar = await screen.findByRole("toolbar", { name: "Session controls" });
    const before = toolbar.innerHTML;
    emit("status_changed", { state: "connecting" });
    expect(toolbar.innerHTML).toBe(before);
    emit("viewer_status", { state: "connecting" });
    expect(toolbar.innerHTML).not.toBe(before);
  });

  it("asks the host user about a file the viewer sends", async () => {
    mocks.respondFile.mockResolvedValue(undefined);
    render(<App />);
    await screen.findByText("123456789");
    emit("session_started", { role: "host", remote_id: "987654321" });
    await screen.findByText(/You are being controlled by 987654321/);
    emit("file_offer", { id: 4, name: "notes.txt", size: 2048, from: "987654321" });
    const dialog = await screen.findByRole("dialog", { name: "Accept this file?" });
    expect(dialog.textContent).toContain("notes.txt");
    expect(dialog.textContent).toContain("2 KB");
    fireEvent.click(screen.getByRole("button", { name: "Accept" }));
    await waitFor(() => expect(mocks.respondFile).toHaveBeenCalledWith(4, true));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Accept this file?" })).toBeNull());
    emit("file_done", { id: 4, path: "/home/me/Downloads/notes.txt" });
    expect((await screen.findByRole("status")).textContent).toContain("Saved notes.txt to /home/me/Downloads/notes.txt");
  });

  it("returns home with the error visible when the host declines", async () => {
    await startConnecting();
    emit("error", { message: "The host declined the connection request." });
    emit("session_ended", { reason: "error" });
    expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining("The host declined the connection request."));
    expect((screen.getByPlaceholderText("000 000 000") as HTMLInputElement).value).toBe("987654321");
  });

  it("prompts for a 2FA code and reconnects with it", async () => {
    await startConnecting();
    emit("twofa_prompt", { reason: "required" });
    emit("session_ended", { reason: "error" });
    const dialog = await screen.findByRole("dialog", { name: "Enter your 2FA code" });
    expect(dialog.textContent).toContain("Enter the current 6-digit code");
    fireEvent.change(screen.getByLabelText("Two-factor code"), { target: { value: "123456" } });
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    await waitFor(() => expect(mocks.connect).toHaveBeenLastCalledWith("987654321", "secret", "123456"));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Enter your 2FA code" })).toBeNull());
  });

  it("reopens the 2FA popup with an error when the code is wrong", async () => {
    await startConnecting();
    emit("twofa_prompt", { reason: "invalid" });
    emit("session_ended", { reason: "error" });
    const dialog = await screen.findByRole("dialog", { name: "Enter your 2FA code" });
    expect(dialog.textContent).toContain("That code didn't match");
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Enter your 2FA code" })).toBeNull());
    expect(mocks.connect).toHaveBeenCalledTimes(1);
  });

  it("closes an incoming request that ended before it was answered", async () => {
    render(<App />);
    await screen.findByPlaceholderText("000 000 000");
    emit("incoming_request", { peer_id: "111222333", fingerprint: "pending", known: false });
    expect(await screen.findByRole("dialog", { name: "Allow remote control?" })).toBeTruthy();
    emit("error", { message: "nobody answered the connection request" });
    emit("session_ended", { reason: "host session ended" });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("cancels a pending connection", async () => {
    await startConnecting();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(mocks.disconnect).toHaveBeenCalledOnce();
    expect(await screen.findByPlaceholderText("000 000 000")).toBeTruthy();
  });

  it("shows backend errors as a dismissible toast in plain language", async () => {
    render(<App />);
    await screen.findByText("123456789");
    emit("error", { message: "target ID must be 9 digits" });
    const toast = await screen.findByRole("alert");
    expect(toast.textContent).toContain("Enter the 9-digit ID.");
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("clears the toast once the host is online", async () => {
    render(<App />);
    await screen.findByText("123456789");
    emit("error", { message: "relay unreachable; retrying host registration" });
    expect(await screen.findByRole("alert")).toBeTruthy();
    emit("status_changed", { state: "online" });
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("auto-clears informational notices", async () => {
    Object.assign(navigator, { clipboard: { writeText: vi.fn(() => Promise.resolve()) } });
    render(<App />);
    await screen.findByText("123456789");
    vi.useFakeTimers();
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Copy device ID" })); });
    expect(screen.getByRole("status").textContent).toContain("ID copied.");
    act(() => { vi.advanceTimersByTime(4000); });
    expect(screen.queryByText("ID copied.")).toBeNull();
  });

  it("opens Permissions on launch when one is missing", async () => {
    mocks.permissionStatus.mockResolvedValue({ screen_recording: false, accessibility: true });
    render(<App />);
    expect(await screen.findByRole("heading", { name: "Permissions" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Skip for now" }));
    await waitFor(() => expect(screen.queryByRole("heading", { name: "Permissions" })).toBeNull());
    // App checks on launch and the Permissions screen re-checks; Home then checks again too.
    expect(mocks.permissionStatus.mock.calls.length).toBeGreaterThanOrEqual(2);
  });
});
