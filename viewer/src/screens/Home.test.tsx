// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import Home from "./Home";
import * as api from "../lib/api";
import { NotifyContext } from "../lib/notify";

vi.mock("../lib/api", async () => {
  const actual = await vi.importActual<typeof import("../lib/api")>("../lib/api");
  return { ...actual, getIdentity: vi.fn(), getConfig: vi.fn(), getUnattendedStatus: vi.fn(), listContacts: vi.fn(), listAllowed: vi.fn(), startHosting: vi.fn(), getWindowsServiceStatus: vi.fn(), installWindowsService: vi.fn(), uninstallWindowsService: vi.fn(), closeKeepsRunning: vi.fn(), quitApp: vi.fn(), connect: vi.fn(), setUnattendedAccess: vi.fn(), setUnattended2fa: vi.fn(), confirmUnattended2fa: vi.fn(), rotatePassword: vi.fn(), addContact: vi.fn(), setContactPassword: vi.fn(), setContactMac: vi.fn(), wakeContact: vi.fn(), permissionStatus: vi.fn(), appVersion: vi.fn(), getStartAtSignIn: vi.fn(), setStartAtSignIn: vi.fn(), onIdentityChanged: vi.fn(), onContactMac: vi.fn(), resetIdentity: vi.fn(), getIdentityConflicts: vi.fn(), onIdentityConflicts: vi.fn(), setAllowVirtualDisplays: vi.fn(), getDisplayDriver: vi.fn(), installDisplayDriver: vi.fn(), uninstallDisplayDriver: vi.fn(), listRecordings: vi.fn(), deleteRecording: vi.fn() };
});
vi.mock("@tauri-apps/plugin-opener", () => ({ openPath: vi.fn(() => Promise.resolve()), revealItemInDir: vi.fn(() => Promise.resolve()) }));
describe("Home", () => {
  afterEach(cleanup);
  // Settings controls now live behind the ⚙ Settings view and its tabs.
  const openSettings = (tab?: "Access" | "Security" | "Network" | "Recordings" | "About") => {
    fireEvent.click(screen.getByRole("button", { name: /Settings/ }));
    if (tab) fireEvent.click(screen.getByRole("button", { name: tab }));
  };
  beforeEach(() => {
    vi.mocked(api.getIdentity).mockResolvedValue({ id: "123456789", password: "654321" });
    vi.mocked(api.getConfig).mockResolvedValue({ relay_url: "wss://relay", relay_urls: ["wss://relay"], recent_ids: [], clipboard_sync: false, allow_virtual_displays: true });
    vi.mocked(api.getUnattendedStatus).mockResolvedValue({ enabled: false, has_password: false, twofa_enabled: false, ready: false, problem: null });
    vi.mocked(api.listContacts).mockResolvedValue([]);
    vi.mocked(api.listAllowed).mockResolvedValue([]);
    vi.mocked(api.listRecordings).mockResolvedValue([]);
    vi.mocked(api.startHosting).mockResolvedValue(undefined); vi.mocked(api.connect).mockResolvedValue(undefined);
    vi.mocked(api.getWindowsServiceStatus).mockResolvedValue({ supported: false, installed: false, running: false });
    vi.mocked(api.getStartAtSignIn).mockResolvedValue(false); vi.mocked(api.setStartAtSignIn).mockResolvedValue(undefined);
    vi.mocked(api.onIdentityChanged).mockResolvedValue(() => {});
    vi.mocked(api.onContactMac).mockResolvedValue(() => {});
    vi.mocked(api.appVersion).mockResolvedValue("0.1.0-test");
    vi.mocked(api.closeKeepsRunning).mockResolvedValue(false); vi.mocked(api.quitApp).mockResolvedValue(undefined);
    vi.mocked(api.getIdentityConflicts).mockResolvedValue([]);
    vi.mocked(api.onIdentityConflicts).mockResolvedValue(() => {});
    vi.mocked(api.resetIdentity).mockReset();
    vi.mocked(api.getDisplayDriver).mockResolvedValue({ applies: false, installed: false, license: "" });
    vi.mocked(api.permissionStatus).mockResolvedValue({ screen_recording: true, accessibility: true });
  });
  it("warns and links to permissions when hosting access is missing", async () => {
    vi.mocked(api.permissionStatus).mockResolvedValue({ screen_recording: false, accessibility: true });
    const onOpenPermissions = vi.fn();
    render(<Home status="online" onConnect={vi.fn()} onOpenPermissions={onOpenPermissions} />);
    const banner = await screen.findByText(/Screen Recording is off/);
    fireEvent.click(banner);
    expect(onOpenPermissions).toHaveBeenCalled();
  });
  it("shows no hosting warning when permissions are granted", async () => {
    render(<Home status="online" onConnect={vi.fn()} onOpenPermissions={vi.fn()} />);
    await screen.findByText("123456789");
    expect(screen.queryByText(/Fix permissions/)).toBeNull();
  });
  it("renders the identity returned by the backend", async () => { render(<Home status="online" onConnect={vi.fn()} incomingRequest={null} onDismissRequest={vi.fn()} />); expect(await screen.findByText("123456789")).toBeTruthy(); });
  it("shows a replacement ID when the relay says the old one is taken", async () => {
    render(<Home status="online" onConnect={vi.fn()} incomingRequest={null} onDismissRequest={vi.fn()} />);
    expect(await screen.findByText("123456789")).toBeTruthy();
    const handler = vi.mocked(api.onIdentityChanged).mock.calls[0][0];
    act(() => handler({ id: "987654321" }));
    expect(await screen.findByText("987654321")).toBeTruthy();
  });
  it("saves the virtual display setting and undoes a failed save", async () => {
    vi.mocked(api.setAllowVirtualDisplays).mockResolvedValueOnce(undefined).mockRejectedValueOnce("disk full");
    render(<Home status="online" onConnect={vi.fn()} incomingRequest={null} onDismissRequest={vi.fn()} />);
    openSettings("Security");
    const toggle = () => screen.getByRole("checkbox", { name: "Let viewers add virtual displays" }) as HTMLInputElement;
    await waitFor(() => expect(toggle().checked).toBe(true));
    fireEvent.click(toggle());
    expect(api.setAllowVirtualDisplays).toHaveBeenLastCalledWith(false);
    expect(toggle().checked).toBe(false);
    fireEvent.click(toggle());
    await waitFor(() => expect(toggle().checked).toBe(false));
  });

  it("installs Windows' virtual display driver after showing its licence", async () => {
    const license = "Copyright 2014-2021 Amyuni Technologies Inc.";
    vi.mocked(api.getDisplayDriver).mockResolvedValue({ applies: true, installed: false, license });
    vi.mocked(api.installDisplayDriver).mockRejectedValueOnce("Nothing was changed: Windows' administrator prompt was declined.")
      .mockResolvedValueOnce({ status: { applies: true, installed: true, license }, restart: false });
    vi.mocked(api.uninstallDisplayDriver).mockResolvedValueOnce({ status: { applies: true, installed: false, license }, restart: true });
    render(<Home status="online" onConnect={vi.fn()} incomingRequest={null} onDismissRequest={vi.fn()} />);
    openSettings("Security");
    fireEvent.click(await screen.findByRole("button", { name: "Install virtual display driver" }));
    // Nothing happens until the licence is accepted.
    expect(screen.getByText(license)).toBeTruthy();
    expect(api.installDisplayDriver).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Accept and install" }));
    // Declined at the prompt: still offered.
    await waitFor(() => expect(api.installDisplayDriver).toHaveBeenCalledTimes(1));
    fireEvent.click(await screen.findByRole("button", { name: "Accept and install" }));
    fireEvent.click(await screen.findByRole("button", { name: "Uninstall driver" }));
    await waitFor(() => expect(api.uninstallDisplayDriver).toHaveBeenCalledTimes(1));
    expect(await screen.findByRole("button", { name: "Install virtual display driver" })).toBeTruthy();
  });

  it("offers the driver only on Windows and while virtual displays are allowed", async () => {
    vi.mocked(api.getDisplayDriver).mockResolvedValue({ applies: true, installed: false, license: "" });
    vi.mocked(api.getConfig).mockResolvedValue({ relay_url: "wss://relay", relay_urls: ["wss://relay"], recent_ids: [], clipboard_sync: false, allow_virtual_displays: false });
    render(<Home status="online" onConnect={vi.fn()} incomingRequest={null} onDismissRequest={vi.fn()} />);
    expect(await screen.findByText("123456789")).toBeTruthy();
    openSettings("Security");
    expect(screen.queryByRole("button", { name: "Install virtual display driver" })).toBeNull();
    cleanup();
    vi.mocked(api.getDisplayDriver).mockResolvedValue({ applies: false, installed: false, license: "" });
    vi.mocked(api.getConfig).mockResolvedValue({ relay_url: "wss://relay", relay_urls: ["wss://relay"], recent_ids: [], clipboard_sync: false, allow_virtual_displays: true });
    render(<Home status="online" onConnect={vi.fn()} incomingRequest={null} onDismissRequest={vi.fn()} />);
    expect(await screen.findByText("123456789")).toBeTruthy();
    openSettings("Security");
    expect(screen.queryByRole("button", { name: "Install virtual display driver" })).toBeNull();
  });

  it("offers to start hosting only while offline", async () => {
    const { rerender } = render(<Home status="offline" onConnect={vi.fn()} />);
    await screen.findByText("123456789");
    const before = vi.mocked(api.startHosting).mock.calls.length;
    fireEvent.click(screen.getByRole("button", { name: "Start hosting" }));
    expect(api.startHosting).toHaveBeenCalledTimes(before + 1);
    rerender(<Home status="online" onConnect={vi.fn()} />);
    expect(screen.queryByRole("button", { name: "Start hosting" })).toBeNull();
  });
  it("shows the app version on the About screen", async () => {
    render(<Home status="online" onConnect={vi.fn()} />);
    await screen.findByText("123456789");
    openSettings("About");
    expect(await screen.findByText("v0.1.0-test")).toBeTruthy();
  });

  it("offers Quit only when closing the window keeps OpenDesk running", async () => {
    render(<Home status="online" onConnect={vi.fn()} />);
    await screen.findByText("123456789");
    openSettings("About");
    expect(screen.queryByRole("button", { name: "Quit OpenDesk" })).toBeNull();
    cleanup();
    vi.mocked(api.closeKeepsRunning).mockResolvedValue(true);
    render(<Home status="online" onConnect={vi.fn()} />);
    openSettings("About");
    fireEvent.click(await screen.findByRole("button", { name: "Quit OpenDesk" }));
    expect(api.quitApp).toHaveBeenCalledOnce();
  });
  it("explains relays that refuse the ID", async () => {
    vi.mocked(api.getIdentityConflicts).mockResolvedValue([{ relay: "wss://a", kind: "taken", message: "ID 123456789 now belongs to another device on wss://a." }]);
    render(<Home status="connecting" onConnect={vi.fn()} />);
    expect((await screen.findByRole("alert")).textContent).toContain("now belongs to another device");
    const handler = vi.mocked(api.onIdentityConflicts).mock.calls[0][0];
    act(() => handler({ conflicts: [{ relay: "wss://b", kind: "in_use", message: "Another computer is online with ID 123456789 on wss://b." }] }));
    expect(screen.getByRole("alert").textContent).toContain("Another computer is online");
    act(() => handler({ conflicts: [] }));
    expect(screen.queryByRole("alert")).toBeNull();
  });
  it("gets a new ID only after the user confirms", async () => {
    vi.mocked(api.resetIdentity).mockResolvedValue("555666777");
    render(<Home status="online" onConnect={vi.fn()} />);
    expect(await screen.findByText("123456789")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Get a new ID" }));
    const confirm = screen.getByRole("group", { name: "Confirm new ID" });
    expect(confirm.textContent).toContain("Anyone who saved 123456789");
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("group", { name: "Confirm new ID" })).toBeNull();
    expect(api.resetIdentity).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Get a new ID" }));
    fireEvent.click(screen.getByRole("button", { name: "Get new ID" }));
    expect(await screen.findByText("555666777")).toBeTruthy();
    expect(api.resetIdentity).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("group", { name: "Confirm new ID" })).toBeNull();
  });
  it("reveals the session password on demand", async () => {
    render(<Home status="online" onConnect={vi.fn()} incomingRequest={null} onDismissRequest={vi.fn()} />);
    const field = await screen.findByLabelText("Session password") as HTMLInputElement;
    await waitFor(() => expect(field.value).toBe("654321"));
    expect(field.type).toBe("password");
    fireEvent.click(screen.getByRole("button", { name: "Show password" }));
    expect(field.type).toBe("text");
    fireEvent.click(screen.getByRole("button", { name: "Hide password" }));
    expect(field.type).toBe("password");
  });
  it("connects with the entered target ID and password through the popup", async () => {
    const onConnect = vi.fn().mockResolvedValue(undefined);
    const prompt = vi.spyOn(window, "prompt").mockReturnValue(null);
    render(<Home status="online" onConnect={onConnect} incomingRequest={null} onDismissRequest={vi.fn()} />);
    fireEvent.change(screen.getByPlaceholderText("000 000 000"), { target: { value: "987654321" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));
    const dialog = await screen.findByRole("dialog");
    fireEvent.change(within(dialog).getByLabelText("Device password"), { target: { value: "remote-secret" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Connect" }));
    await waitFor(() => expect(onConnect).toHaveBeenCalledWith("987654321", "remote-secret"));
    expect(prompt).not.toHaveBeenCalled();
  });
  it("saves the password when 'remember' is checked before connecting", async () => {
    vi.mocked(api.setContactPassword).mockResolvedValue(undefined);
    const onConnect = vi.fn().mockResolvedValue(undefined);
    render(<Home status="online" onConnect={onConnect} />);
    fireEvent.change(screen.getByPlaceholderText("000 000 000"), { target: { value: "987654321" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));
    const dialog = await screen.findByRole("dialog");
    fireEvent.change(within(dialog).getByLabelText("Device password"), { target: { value: "remote-secret" } });
    fireEvent.click(within(dialog).getByRole("checkbox", { name: "Remember password for this device" }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Connect" }));
    await waitFor(() => expect(api.setContactPassword).toHaveBeenCalledWith("987654321", "remote-secret"));
    expect(onConnect).toHaveBeenCalledWith("987654321", "remote-secret");
  });
  it("has no inline 2FA field; connecting uses ID and password only", async () => {
    const onConnect = vi.fn().mockResolvedValue(undefined);
    render(<Home status="online" onConnect={onConnect} incomingRequest={null} onDismissRequest={vi.fn()} />);
    expect(screen.queryByLabelText("2FA code (optional)")).toBeNull();
    fireEvent.change(screen.getByPlaceholderText("000 000 000"), { target: { value: "987654321" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));
    const dialog = await screen.findByRole("dialog");
    fireEvent.change(within(dialog).getByLabelText("Device password"), { target: { value: "remote-secret" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Connect" }));
    await waitFor(() => expect(onConnect).toHaveBeenCalledWith("987654321", "remote-secret"));
  });

  it("opens the password popup for a device without a saved password", async () => {
    vi.mocked(api.listContacts).mockResolvedValue([{ id: "111222333", alias: "Office", last_connected: null, has_saved_password: false }]);
    const onConnect = vi.fn();
    render(<Home status="online" onConnect={onConnect} />);
    await screen.findByText("Office");
    fireEvent.click(screen.getByRole("button", { name: "Connect to Office" }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Office")).toBeTruthy();
    expect(onConnect).not.toHaveBeenCalled();
  });

  it("connects a saved-password device in one click, without the popup", async () => {
    vi.mocked(api.listContacts).mockResolvedValue([{ id: "111222333", alias: "Office", last_connected: null, has_saved_password: true }]);
    const onConnect = vi.fn().mockResolvedValue(undefined);
    render(<Home status="online" onConnect={onConnect} />);
    await screen.findByText("Office");
    fireEvent.click(screen.getByRole("button", { name: "Connect to Office" }));
    await waitFor(() => expect(onConnect).toHaveBeenCalledWith("111222333", ""));
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("rejects contact IDs that are not 9 digits", async () => {
    const notify = vi.fn();
    render(<NotifyContext.Provider value={notify}><Home status="online" onConnect={vi.fn()} /></NotifyContext.Provider>);
    fireEvent.change(screen.getByLabelText("Contact alias"), { target: { value: "Office" } });
    fireEvent.change(screen.getByLabelText("Contact device ID"), { target: { value: "1234" } });
    fireEvent.click(screen.getAllByRole("button", { name: "Add" })[0]);
    expect(notify).toHaveBeenCalledWith("Enter the 9-digit ID.", "error");
    expect(api.addContact).not.toHaveBeenCalled();
  });

  it("lets you enable first, prompts for a password, and reverts when saving fails", async () => {
    vi.mocked(api.setUnattendedAccess).mockRejectedValue("unattended access password must be at least 12 characters");
    const notify = vi.fn();
    render(<NotifyContext.Provider value={notify}><Home status="online" onConnect={vi.fn()} /></NotifyContext.Provider>);
    openSettings("Access");
    const enabled = screen.getByRole("checkbox", { name: "Enabled" }) as HTMLInputElement;
    expect(enabled.disabled).toBe(false);
    fireEvent.click(enabled);
    expect(enabled.checked).toBe(true);
    // Saving with no password yet tells you what to do instead of calling the backend.
    fireEvent.click(screen.getByRole("button", { name: "Save settings" }));
    await waitFor(() => expect(notify).toHaveBeenCalledWith("Set a password to turn on unattended access.", "error"));
    expect(api.setUnattendedAccess).not.toHaveBeenCalled();
    fireEvent.change(screen.getByLabelText("Unattended access password"), { target: { value: "short" } });
    fireEvent.click(screen.getByRole("button", { name: "Save settings" }));
    await waitFor(() => expect(notify).toHaveBeenCalledWith("Use at least 12 characters for the unattended access password.", "error"));
    expect(enabled.checked).toBe(false);
  });

  it("enables 2FA only after a code from the authenticator is confirmed", async () => {
    vi.mocked(api.getUnattendedStatus).mockResolvedValue({ enabled: true, has_password: true, twofa_enabled: false, ready: true, problem: null });
    vi.mocked(api.setUnattended2fa).mockResolvedValue({ uri: "otpauth://totp/OpenDesk:123456789?secret=JBSWY3DPEHPK3PXP&issuer=OpenDesk", secret: "JBSWY3DPEHPK3PXP", qr_width: 2, qr_modules: [true, false, false, true] });
    vi.mocked(api.confirmUnattended2fa).mockRejectedValueOnce("invalid 2FA code").mockResolvedValueOnce({ enabled: true, has_password: true, twofa_enabled: true, ready: true, problem: null });
    const notify = vi.fn();
    render(<NotifyContext.Provider value={notify}><Home status="online" onConnect={vi.fn()} /></NotifyContext.Provider>);
    openSettings("Access");
    const toggle = screen.getByRole("checkbox", { name: "Require 2FA code" }) as HTMLInputElement;
    await waitFor(() => expect(toggle.disabled).toBe(false));
    fireEvent.click(toggle);

    expect(await screen.findByRole("img", { name: "QR code for your authenticator app" })).toBeTruthy();
    expect(screen.getByLabelText("Setup key").textContent).toBe("JBSW Y3DP EHPK 3PXP");
    expect(toggle.checked).toBe(false);

    fireEvent.change(screen.getByLabelText("Authenticator code"), { target: { value: "000000" } });
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
    await waitFor(() => expect(notify).toHaveBeenCalledWith(expect.stringMatching(/didn't match/), "error"));
    expect(toggle.checked).toBe(false);

    fireEvent.change(screen.getByLabelText("Authenticator code"), { target: { value: "123456" } });
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
    await waitFor(() => expect(toggle.checked).toBe(true));
    expect(api.confirmUnattended2fa).toHaveBeenLastCalledWith("123456");
    expect(screen.queryByLabelText("Setup key")).toBeNull();
  });

  it("discards a pending 2FA setup on cancel", async () => {
    vi.mocked(api.getUnattendedStatus).mockResolvedValue({ enabled: true, has_password: true, twofa_enabled: false, ready: true, problem: null });
    vi.mocked(api.setUnattended2fa).mockResolvedValueOnce({ uri: "otpauth://x", secret: "ABCD", qr_width: 1, qr_modules: [true] }).mockResolvedValueOnce(null);
    render(<Home status="online" onConnect={vi.fn()} />);
    openSettings("Access");
    const toggle = screen.getByRole("checkbox", { name: "Require 2FA code" }) as HTMLInputElement;
    await waitFor(() => expect(toggle.disabled).toBe(false));
    fireEvent.click(toggle);
    fireEvent.click(await screen.findByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByLabelText("Setup key")).toBeNull());
    expect(api.setUnattended2fa).toHaveBeenLastCalledWith(false);
    expect(toggle.checked).toBe(false);
  });

  it("keeps the 2FA toggle disabled until unattended access is on", async () => {
    render(<Home status="online" onConnect={vi.fn()} />);
    await screen.findByText("123456789");
    openSettings("Access");
    expect((screen.getByRole("checkbox", { name: "Require 2FA code" }) as HTMLInputElement).disabled).toBe(true);
  });

  describe("start at sign-in", () => {
    const toggle = () => screen.getByRole("checkbox", { name: "Start OpenDesk on startup" }) as HTMLInputElement;

    it("stays disabled with a hint until unattended access is saved", async () => {
      render(<Home status="online" onConnect={vi.fn()} />);
      openSettings("Access");
      expect(await screen.findByText("Turn on and save unattended access first.")).toBeTruthy();
      expect(toggle().checked).toBe(false);
      expect(toggle().disabled).toBe(true);
    });

    it("turns on through the OS once unattended access is ready", async () => {
      vi.mocked(api.getUnattendedStatus).mockResolvedValue({ enabled: true, has_password: true, twofa_enabled: false, ready: true, problem: null });
      render(<Home status="online" onConnect={vi.fn()} />);
      openSettings("Access");
      await waitFor(() => expect(toggle().disabled).toBe(false));
      fireEvent.click(toggle());
      await waitFor(() => expect(toggle().checked).toBe(true));
      expect(api.setStartAtSignIn).toHaveBeenCalledWith(true);
    });

    it("shows the state the OS reports and can always be turned off", async () => {
      vi.mocked(api.getStartAtSignIn).mockResolvedValue(true);
      render(<Home status="online" onConnect={vi.fn()} />);
      openSettings("Access");
      await waitFor(() => expect(toggle().checked).toBe(true));
      expect(screen.getByText(/will just open its window at sign-in/)).toBeTruthy();
      expect(toggle().disabled).toBe(false);
      fireEvent.click(toggle());
      await waitFor(() => expect(toggle().checked).toBe(false));
      expect(api.setStartAtSignIn).toHaveBeenCalledWith(false);
    });

    it("keeps the old state and reports an error when the OS refuses", async () => {
      vi.mocked(api.getUnattendedStatus).mockResolvedValue({ enabled: true, has_password: true, twofa_enabled: false, ready: true, problem: null });
      vi.mocked(api.setStartAtSignIn).mockRejectedValue("permission denied");
      const notify = vi.fn();
      render(<NotifyContext.Provider value={notify}><Home status="online" onConnect={vi.fn()} /></NotifyContext.Provider>);
      openSettings("Access");
      await waitFor(() => expect(toggle().disabled).toBe(false));
      fireEvent.click(toggle());
      await waitFor(() => expect(notify).toHaveBeenCalled());
      expect(toggle().checked).toBe(false);
    });

    it("is disabled when the OS state can't be read", async () => {
      vi.mocked(api.getUnattendedStatus).mockResolvedValue({ enabled: true, has_password: true, twofa_enabled: false, ready: true, problem: null });
      vi.mocked(api.getStartAtSignIn).mockRejectedValue("unsupported");
      render(<Home status="online" onConnect={vi.fn()} />);
      await screen.findByText("123456789");
      openSettings("Access");
      expect(toggle().disabled).toBe(true);
    });
  });

  describe("when the system keyring fails", () => {
    const keyringLocked = "OpenDesk couldn't read the unattended access password from the system keyring. Until it can, connections need your approval. Unlock your login keychain in Keychain Access, then try again.";
    const broken: api.UnattendedStatus = { enabled: true, has_password: true, twofa_enabled: false, ready: false, problem: keyringLocked };
    const startToggle = () => screen.getByRole("checkbox", { name: "Start OpenDesk on startup" }) as HTMLInputElement;

    it("says what's wrong and keeps start at sign-in off", async () => {
      vi.mocked(api.getUnattendedStatus).mockResolvedValue(broken);
      const notify = vi.fn();
      render(<NotifyContext.Provider value={notify}><Home status="online" onConnect={vi.fn()} /></NotifyContext.Provider>);
      openSettings("Access");
      expect((await screen.findByRole("alert")).textContent).toBe(keyringLocked);
      expect(notify).toHaveBeenCalledWith(keyringLocked, "error");
      expect(screen.getByText("Fix unattended access first.")).toBeTruthy();
      expect(startToggle().disabled).toBe(true);
    });

    it("warns that a sign-in launch will open the window", async () => {
      vi.mocked(api.getUnattendedStatus).mockResolvedValue(broken);
      vi.mocked(api.getStartAtSignIn).mockResolvedValue(true);
      render(<Home status="online" onConnect={vi.fn()} />);
      openSettings("Access");
      expect(await screen.findByText(/isn't working, so OpenDesk will just open its window at sign-in/)).toBeTruthy();
      expect(startToggle().disabled).toBe(false);
    });

    it("shows the keyring error when saving and reverts the toggle", async () => {
      const saveFailed = "OpenDesk couldn't find a system keyring to save the unattended access password. Install and unlock a keyring such as GNOME Keyring or KWallet, then try again.";
      vi.mocked(api.setUnattendedAccess).mockRejectedValue(saveFailed);
      const notify = vi.fn();
      render(<NotifyContext.Provider value={notify}><Home status="online" onConnect={vi.fn()} /></NotifyContext.Provider>);
      await screen.findByText("123456789");
      openSettings("Access");
      fireEvent.change(screen.getByLabelText("Unattended access password"), { target: { value: "long-password-1" } });
      const enabled = screen.getByRole("checkbox", { name: "Enabled" }) as HTMLInputElement;
      fireEvent.click(enabled);
      fireEvent.click(screen.getByRole("button", { name: "Save settings" }));
      await waitFor(() => expect(notify).toHaveBeenCalledWith(saveFailed, "error"));
      expect(enabled.checked).toBe(false);
      expect(screen.queryByRole("alert")).toBeNull();
    });

    it("clears the problem once the password is saved again", async () => {
      vi.mocked(api.getUnattendedStatus).mockResolvedValue({ ...broken, has_password: false, problem: "The unattended access password is missing from the system keyring, so connections need your approval. Enter the password again and save." });
      vi.mocked(api.setUnattendedAccess).mockResolvedValue({ enabled: true, has_password: true, twofa_enabled: false, ready: true, problem: null });
      render(<Home status="online" onConnect={vi.fn()} />);
      openSettings("Access");
      await screen.findByRole("alert");
      fireEvent.change(screen.getByLabelText("Unattended access password"), { target: { value: "long-password-1" } });
      fireEvent.click(screen.getByRole("button", { name: "Save settings" }));
      await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
      expect(api.setUnattendedAccess).toHaveBeenCalledWith(true, "long-password-1");
      expect(startToggle().disabled).toBe(false);
    });

    it("lets 2FA be turned off when its secret is gone", async () => {
      const lost = { enabled: true, has_password: true, twofa_enabled: true, ready: false, problem: "The 2FA secret is missing from the system keyring, so connections need your approval. Turn off \"Require 2FA code\" and set it up again." };
      vi.mocked(api.getUnattendedStatus).mockResolvedValueOnce(lost).mockResolvedValue({ ...lost, twofa_enabled: false, ready: true, problem: null });
      vi.mocked(api.setUnattended2fa).mockResolvedValue(null);
      render(<Home status="online" onConnect={vi.fn()} />);
      openSettings("Access");
      await screen.findByRole("alert");
      const twofa = screen.getByRole("checkbox", { name: "Require 2FA code" }) as HTMLInputElement;
      expect(twofa.checked).toBe(true);
      fireEvent.click(twofa);
      await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
      expect(api.setUnattended2fa).toHaveBeenCalledWith(false);
      expect(twofa.checked).toBe(false);
    });
  });

  it("confirms a rotated password", async () => {
    vi.mocked(api.rotatePassword).mockResolvedValue({ password: "999999" });
    const notify = vi.fn();
    render(<NotifyContext.Provider value={notify}><Home status="online" onConnect={vi.fn()} /></NotifyContext.Provider>);
    fireEvent.click(screen.getByRole("button", { name: "Rotate password" }));
    await waitFor(() => expect(notify).toHaveBeenCalledWith("New password generated.", "info"));
  });

  it("wakes a device with a saved MAC over Wake-on-LAN", async () => {
    try { localStorage.setItem("opendesk.addressView", "list"); } catch { /* ignore */ }
    vi.mocked(api.listContacts).mockResolvedValue([{ id: "905218437", alias: "Office", last_connected: null, mac: "01:23:45:67:89:AB" }]);
    vi.mocked(api.wakeContact).mockResolvedValue(undefined);
    render(<Home status="online" onConnect={vi.fn()} />);
    const wake = await screen.findByRole("button", { name: "Wake Office" });
    fireEvent.click(wake);
    await waitFor(() => expect(api.wakeContact).toHaveBeenCalledWith("905218437"));
  });

  it("saves a MAC address for a device", async () => {
    try { localStorage.setItem("opendesk.addressView", "list"); } catch { /* ignore */ }
    vi.mocked(api.listContacts).mockResolvedValue([{ id: "905218437", alias: "Office", last_connected: 1_700_000_000 }]);
    vi.mocked(api.setContactMac).mockResolvedValue({ id: "905218437", alias: "Office", last_connected: 1_700_000_000, mac: "01:23:45:67:89:AB" });
    render(<Home status="online" onConnect={vi.fn()} />);
    fireEvent.click(await screen.findByRole("button", { name: "Wake-on-LAN settings for Office" }));
    const input = await screen.findByLabelText("MAC address");
    fireEvent.change(input, { target: { value: "01:23:45:67:89:AB" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.setContactMac).toHaveBeenCalledWith("905218437", "01:23:45:67:89:AB"));
  });

  it("hides the Wake-on-LAN button until a device has been connected to", async () => {
    try { localStorage.setItem("opendesk.addressView", "list"); } catch { /* ignore */ }
    vi.mocked(api.listContacts).mockResolvedValue([{ id: "905218437", alias: "Office", last_connected: null }]);
    render(<Home status="online" onConnect={vi.fn()} />);
    await screen.findByRole("button", { name: "Connect to Office" });
    expect(screen.queryByRole("button", { name: "Wake-on-LAN settings for Office" })).toBeNull();
  });

  it("refuses connecting to this device's own ID", async () => {
    const notify = vi.fn();
    const onConnect = vi.fn().mockResolvedValue(undefined);
    render(<NotifyContext.Provider value={notify}><Home status="online" onConnect={onConnect} /></NotifyContext.Provider>);
    await screen.findByText("123456789");
    fireEvent.change(screen.getByLabelText("Remote device ID"), { target: { value: "123456789" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));
    expect(notify).toHaveBeenCalledWith("That's this device's own ID.", "error");
    expect(onConnect).not.toHaveBeenCalled();
  });
});
