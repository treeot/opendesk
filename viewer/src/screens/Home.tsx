import { useEffect, useState, type FormEvent } from "react";
import * as api from "../lib/api";
import { friendlyError, useNotify } from "../lib/notify";
import QrCode from "../components/QrCode";
import { CHECK_NOW_EVENT } from "../components/UpdateBanner";
import Recordings from "../components/Recordings";
import { buildInviteLink, parseInvite } from "../lib/invite";

interface HomeProps {
  status: api.Status;
  /** Kept optional for callers from the pre-integration shell. */
  incomingRequest?: api.IncomingRequest | null;
  onDismissRequest?: () => void;
  onOpenPermissions?: () => void;
  /** Pre-fills the connect form, e.g. after a rejected attempt. */
  initialTargetId?: string;
  onConnect: (targetId: string, password: string, authCode?: string) => Promise<void>;
}
const statusLabels: Record<api.Status, string> = { offline: "Offline", online: "Online", connecting: "Connecting", reconnecting: "Reconnecting", in_session: "In session" };

/** Base32 secret in groups of four for manual entry. */
const groupSecret = (secret: string) => secret.replace(/(.{4})(?=.)/g, "$1 ");

export default function Home({ status, onConnect, onOpenPermissions, initialTargetId = "" }: HomeProps) {
  const notify = useNotify();
  const setMessage = (text: string | null, tone: "error" | "info" = "error") => { if (text) notify(text, tone); };
  const [identity, setIdentity] = useState<api.Identity | null>(null);
  const [showPassword, setShowPassword] = useState(false);
  const [recentIds, setRecentIds] = useState<string[]>([]);
  const [contacts, setContacts] = useState<api.Contact[]>([]);
  const [allowedDevices, setAllowedDevices] = useState<string[]>([]);
  const [allowedId, setAllowedId] = useState("");
  const [targetId, setTargetId] = useState(initialTargetId);
  // Prefill when a deep link arrives after mount (App updates this prop).
  useEffect(() => { if (initialTargetId) setTargetId(initialTargetId); }, [initialTargetId]);
  const [alias, setAlias] = useState("");
  const [contactId, setContactId] = useState("");
  const [busy, setBusy] = useState(false);
  const [unattended, setUnattended] = useState<api.UnattendedStatus>({ enabled: false, has_password: false, twofa_enabled: false, ready: false, problem: null });
  const [unattendedEnabled, setUnattendedEnabled] = useState(false);
  const [unattendedPassword, setUnattendedPassword] = useState("");
  const [enrollment, setEnrollment] = useState<api.TotpEnrollment | null>(null);
  const [enrollmentCode, setEnrollmentCode] = useState("");
  const [twofaBusy, setTwofaBusy] = useState(false);
  // null until the OS answers (or if it can't).
  const [startAtSignIn, setStartAtSignIn] = useState<boolean | null>(null);
  const [startAtSignInBusy, setStartAtSignInBusy] = useState(false);
  // null until the config loads.
  const [allowVirtualDisplays, setAllowVirtualDisplays] = useState<boolean | null>(null);
  // Windows' virtual display driver; null until known (and elsewhere).
  const [displayDriver, setDisplayDriver] = useState<api.DisplayDriver | null>(null);
  const [confirmingDriver, setConfirmingDriver] = useState(false);
  const [driverBusy, setDriverBusy] = useState(false);
  const [relays, setRelays] = useState<string[]>([]);
  const [closeKeepsRunning, setCloseKeepsRunning] = useState(false);
  const [relayText, setRelayText] = useState("");
  const [conflicts, setConflicts] = useState<api.IdentityConflict[]>([]);
  const [confirmingNewId, setConfirmingNewId] = useState(false);
  const [resettingId, setResettingId] = useState(false);
  const [windowsService, setWindowsService] = useState<api.WindowsServiceStatus | null>(null);
  const [windowsServiceBusy, setWindowsServiceBusy] = useState(false);
  // Saved-device IDs the relay reports online right now (address-book dots).
  const [onlineIds, setOnlineIds] = useState<Set<string>>(new Set());
  // Connect screen vs. the tabbed settings screen (AnyDesk-style split).
  const [view, setView] = useState<"connect" | "settings">("connect");
  const [settingsTab, setSettingsTab] = useState<"access" | "security" | "network" | "recordings" | "about">("access");
  const [version, setVersion] = useState("");
  useEffect(() => { void api.appVersion().then(setVersion).catch(() => undefined); }, []);
  // Address book layout, remembered per device.
  const [addressView, setAddressView] = useState<"grid" | "list">(() => { try { return localStorage.getItem("opendesk.addressView") === "list" ? "list" : "grid"; } catch { return "grid"; } });
  const chooseAddressView = (next: "grid" | "list") => { setAddressView(next); try { localStorage.setItem("opendesk.addressView", next); } catch { /* private mode */ } };
  // The password popup shown when a device has no saved password.
  const [pendingConnect, setPendingConnect] = useState<{ id: string; alias?: string } | null>(null);
  const [popupPassword, setPopupPassword] = useState("");
  const [popupRemember, setPopupRemember] = useState(false);
  const [popupShowPassword, setPopupShowPassword] = useState(false);
  // Wake-on-LAN: the contact whose MAC address is being edited, and the draft.
  const [macEdit, setMacEdit] = useState<{ id: string; alias: string } | null>(null);
  const [macDraft, setMacDraft] = useState("");
  // macOS hosting permissions; null until known. Re-checked when the window
  // regains focus, so granting them in System Settings clears the warning.
  const [perms, setPerms] = useState<api.PermissionStatus | null>(null);

  useEffect(() => {
    let mounted = true;
    void Promise.all([api.getIdentity(), api.getConfig(), api.getUnattendedStatus(), api.listContacts(), api.listAllowed()]).then(([loadedIdentity, config, unattendedStatus, loadedContacts, loadedAllowed]) => {
      if (mounted) { const configuredRelays = config.relay_urls?.length ? config.relay_urls : [config.relay_url]; setIdentity(loadedIdentity); setRecentIds(config.recent_ids); setAllowVirtualDisplays(config.allow_virtual_displays ?? true); setRelays(configuredRelays); setRelayText(configuredRelays.join("\n")); setUnattended(unattendedStatus); setUnattendedEnabled(unattendedStatus.enabled); setContacts(loadedContacts); setAllowedDevices(loadedAllowed); setMessage(unattendedStatus.problem); }
    }).catch((error: unknown) => mounted && setMessage(friendlyError(error)));
    void api.getStartAtSignIn().then((enabled) => { if (mounted) setStartAtSignIn(enabled); }).catch(() => undefined);
    void Promise.resolve().then(api.getDisplayDriver).then((driver) => { if (mounted) setDisplayDriver(driver ?? null); }).catch(() => undefined);
    if (api.getWindowsServiceStatus) {
      void api.getWindowsServiceStatus().then((service) => { if (mounted) { setWindowsService(service); if (!service.running) void api.startHosting().catch((error: unknown) => setMessage(friendlyError(error))); } }).catch(() => { void api.startHosting().catch((error: unknown) => mounted && setMessage(friendlyError(error))); });
    } else {
      void api.startHosting().catch((error: unknown) => mounted && setMessage(friendlyError(error)));
    }
    // The backend rotates the session password after each session and after
    // repeated failed logins.
    const passwordListener = api.onPasswordRotated(({ password }) => { if (mounted) setIdentity((current) => current ? { ...current, password } : current); }).catch(() => undefined);
    // A new device's random ID can collide with another device's; the backend picks a new one.
    const idListener = api.onIdentityChanged(({ id }) => { if (mounted) setIdentity((current) => current ? { ...current, id } : current); }).catch(() => undefined);
    // Relays that refuse a known ID; the user decides whether to get a new one.
    void api.getIdentityConflicts().then((loaded) => { if (mounted) setConflicts(loaded); }).catch(() => undefined);
    const conflictListener = api.onIdentityConflicts(({ conflicts: current }) => { if (mounted) setConflicts(current); }).catch(() => undefined);
    // A connected host reported its MAC; reflect it so the tile shows Wake.
    const macListener = api.onContactMac(({ id, mac }) => { if (mounted) setContacts((current) => current.map((contact) => contact.id === id ? { ...contact, mac } : contact)); }).catch(() => undefined);
    return () => { mounted = false; for (const listener of [passwordListener, idListener, conflictListener, macListener]) void listener.then((unlisten) => unlisten?.()); };
  }, []);

  // Watch macOS hosting permissions and re-check on focus (granting them
  // happens in System Settings, outside the app).
  useEffect(() => {
    let active = true;
    const check = () => { void api.permissionStatus().then((status) => { if (active) setPerms(status); }).catch(() => undefined); };
    check();
    window.addEventListener("focus", check);
    return () => { active = false; window.removeEventListener("focus", check); };
  }, []);

  // Closing the window may only hide it, so offer a way to quit from here.
  useEffect(() => {
    let mounted = true;
    void api.closeKeepsRunning().then((keeps) => { if (mounted) setCloseKeepsRunning(keeps); }).catch(() => undefined);
    return () => { mounted = false; };
  }, [startAtSignIn, unattended.enabled, unattended.has_password]);

  const rotate = async () => {
    try { const result = await api.rotatePassword(); setIdentity((current) => current ? { ...current, password: result.password } : current); setMessage("New password generated.", "info"); }
    catch (error) { setMessage(friendlyError(error)); }
  };
  const getNewId = async () => {
    setResettingId(true);
    try {
      const id = await api.resetIdentity();
      setIdentity((current) => current ? { ...current, id } : current);
      setConflicts([]);
      setConfirmingNewId(false);
      setMessage("This device has a new ID.", "info");
    } catch (error) { setMessage(friendlyError(error)); } finally { setResettingId(false); }
  };
  const saveUnattended = async () => {
    if (unattendedEnabled && !unattended.has_password && unattendedPassword.length === 0) {
      setMessage("Set a password to turn on unattended access.", "error");
      return;
    }
    try {
      const saved = await api.setUnattendedAccess(unattendedEnabled, unattendedPassword || null);
      setUnattended(saved);
      setUnattendedPassword("");
      setMessage(unattendedEnabled ? "Unattended access is on." : "Unattended access is off.", "info");
    } catch (error) {
      setUnattendedEnabled(unattended.enabled);
      setMessage(friendlyError(error));
    }
  };
  const saveRelays = async () => {
    const urls = relayText.split(/\r?\n|,/).map((url) => url.trim()).filter(Boolean);
    if (!urls.length) { setMessage("Enter at least one relay URL."); return; }
    try { await api.setRelays(urls); setRelays(urls); setRelayText(urls.join("\n")); setMessage("Relay regions updated.", "info"); } catch (error) { setMessage(friendlyError(error)); }
  };
  // Turning 2FA on only starts an enrollment; it becomes active once a code
  // from the authenticator is confirmed.
  const toggleTwofa = async (enabled: boolean) => {
    if (twofaBusy) return;
    setTwofaBusy(true);
    try {
      if (enabled) {
        setEnrollment(await api.setUnattended2fa(true));
        setEnrollmentCode("");
      } else {
        await api.setUnattended2fa(false);
        setEnrollment(null);
        setUnattended((current) => ({ ...current, twofa_enabled: false }));
        // Turning off 2FA can clear a "2FA secret is missing" problem.
        void api.getUnattendedStatus().then(setUnattended).catch(() => undefined);
        setMessage("Two-factor is off.", "info");
      }
    } catch (error) { setMessage(friendlyError(error)); } finally { setTwofaBusy(false); }
  };
  const confirmTwofa = async (event: FormEvent) => {
    event.preventDefault();
    if (!/^\d{6}$/.test(enrollmentCode)) { setMessage("Enter the 6-digit code from your authenticator app."); return; }
    setTwofaBusy(true);
    try {
      const saved = await api.confirmUnattended2fa(enrollmentCode);
      setEnrollment(null);
      setEnrollmentCode("");
      setUnattended(saved);
      setMessage("Two-factor is on.", "info");
    } catch (error) { setMessage(friendlyError(error)); } finally { setTwofaBusy(false); }
  };
  const cancelTwofa = async () => {
    setTwofaBusy(true);
    try { await api.setUnattended2fa(false); setEnrollment(null); setEnrollmentCode(""); }
    catch (error) { setMessage(friendlyError(error)); } finally { setTwofaBusy(false); }
  };
  const toggleVirtualDisplays = (enabled: boolean) => {
    setAllowVirtualDisplays(enabled);
    void api.setAllowVirtualDisplays(enabled).catch((error: unknown) => { setAllowVirtualDisplays(!enabled); setMessage(friendlyError(error)); });
  };

  const changeDisplayDriver = async (install: boolean) => {
    setDriverBusy(true);
    try {
      const { status, restart } = install ? await api.installDisplayDriver() : await api.uninstallDisplayDriver();
      setDisplayDriver(status);
      setConfirmingDriver(false);
      const done = install ? "The virtual display driver is installed." : "The virtual display driver is removed.";
      setMessage(restart ? `${done} Restart Windows to finish.` : done, "info");
    } catch (error) { setMessage(friendlyError(error)); } finally { setDriverBusy(false); }
  };

  const toggleStartAtSignIn = async (enabled: boolean) => {
    setStartAtSignInBusy(true);
    try { await api.setStartAtSignIn(enabled); setStartAtSignIn(enabled); }
    catch (error) { setMessage(friendlyError(error)); } finally { setStartAtSignInBusy(false); }
  };
  const toggleWindowsService = async (enabled: boolean) => {
    setWindowsServiceBusy(true);
    try {
      const next = enabled ? await api.installWindowsService() : await api.uninstallWindowsService();
      setWindowsService(next);
      setMessage(enabled ? "The sign-in-screen service is enabled." : "The sign-in-screen service is disabled.", "info");
    } catch (error) { setMessage(friendlyError(error)); } finally { setWindowsServiceBusy(false); }
  };
  // Connect with a known password. An empty string tells the backend to use
  // the device's saved password (it never passes through the UI).
  const runConnect = async (id: string, password: string) => {
    setBusy(true);
    try { await onConnect(id, password); } catch (error) { setMessage(friendlyError(error)); } finally { setBusy(false); }
  };
  const openConnectPopup = (id: string, alias?: string) => { setPendingConnect({ id, alias }); setPopupPassword(""); setPopupRemember(false); setPopupShowPassword(false); };
  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (!/^\d{9}$/.test(targetId)) { setMessage("Enter the 9-digit ID."); return; }
    if (targetId === identity?.id) { setMessage("That's this device's own ID."); return; }
    openConnectPopup(targetId);
  };
  const submitPopup = async (event: FormEvent) => {
    event.preventDefault();
    if (!pendingConnect) return;
    if (!popupPassword) { setMessage("Enter the device password."); return; }
    const { id } = pendingConnect;
    if (popupRemember) {
      try { await api.setContactPassword(id, popupPassword); setContacts((current) => current.map((contact) => contact.id === id ? { ...contact, has_saved_password: true } : contact)); }
      catch (error) { setMessage(friendlyError(error)); }
    }
    setPendingConnect(null);
    await runConnect(id, popupPassword);
  };
  // Fill the form rather than reusing whatever password happens to be typed:
  // Poll the relay for which saved devices are online, now and every 20s.
  useEffect(() => {
    if (contacts.length === 0) { setOnlineIds(new Set()); return; }
    let active = true;
    const ids = contacts.map((contact) => contact.id);
    const poll = async () => {
      const online = await api.getPresence(ids);
      if (active) setOnlineIds(new Set(online));
    };
    void poll();
    const timer = setInterval(() => void poll(), 20_000);
    return () => { active = false; clearInterval(timer); };
  }, [contacts]);

  // Saved-password devices connect in one click; the rest open the popup.
  const startConnect = (contact: api.Contact) => {
    if (contact.id === identity?.id) { setMessage("That's this device's own ID."); return; }
    if (contact.has_saved_password) void runConnect(contact.id, "");
    else openConnectPopup(contact.id, contact.alias);
  };
  const add = async (event: FormEvent) => {
    event.preventDefault();
    if (!alias.trim()) { setMessage("Enter a name for the device."); return; }
    if (!/^\d{9}$/.test(contactId)) { setMessage("Enter the 9-digit ID."); return; }
    try { const contact = await api.addContact(alias.trim(), contactId); setContacts((current) => { const without = current.filter((item) => item.id !== contact.id); return [...without, contact]; }); setAlias(""); setContactId(""); } catch (error) { setMessage(friendlyError(error)); }
  };
  const remove = async (id: string) => {
    try { await api.removeContact(id); setContacts((current) => current.filter((contact) => contact.id !== id)); } catch (error) { setMessage(friendlyError(error)); }
  };
  const openMacEdit = (contact: api.Contact) => { setMacEdit({ id: contact.id, alias: contact.alias }); setMacDraft(contact.mac ?? ""); };
  const saveMac = async () => {
    if (!macEdit) return;
    try { const updated = await api.setContactMac(macEdit.id, macDraft); setContacts((current) => current.map((contact) => contact.id === macEdit.id ? { ...contact, mac: updated.mac } : contact)); setMacEdit(null); }
    catch (error) { setMessage(friendlyError(error)); }
  };
  const wake = async (id: string, alias: string) => {
    try { await api.wakeContact(id); setMessage(`Wake signal sent to ${alias}.`); }
    catch (error) { setMessage(friendlyError(error)); }
  };
  const addAllowedDevice = async (event: FormEvent) => {
    event.preventDefault();
    if (!/^\d{9}$/.test(allowedId)) { setMessage("Enter the 9-digit ID."); return; }
    try { await api.addAllowed(allowedId); setAllowedDevices((current) => current.includes(allowedId) ? current : [...current, allowedId]); setAllowedId(""); } catch (error) { setMessage(friendlyError(error)); }
  };
  const removeAllowedDevice = async (id: string) => {
    try { await api.removeAllowed(id); setAllowedDevices((current) => current.filter((device) => device !== id)); } catch (error) { setMessage(friendlyError(error)); }
  };
  const unattendedDirty = unattendedEnabled !== unattended.enabled || unattendedPassword.length > 0;
  // 2FA protects unattended access, so it can only be turned on once that is
  // saved as enabled; turning it off is always allowed.
  const twofaToggleEnabled = unattended.twofa_enabled || unattended.enabled;
  // Starting at sign-in is only useful when someone can connect without
  // approval, so it waits for saved unattended access whose password the
  // keyring can actually return. Turning it off is always allowed.
  const unattendedReady = unattended.ready;
  const startAtSignInHint = startAtSignIn === null ? null
    : !unattendedReady && unattended.enabled ? (startAtSignIn ? "Unattended access isn't working, so OpenDesk will just open its window at sign-in." : "Fix unattended access first.")
    : !unattendedReady ? (startAtSignIn ? "Unattended access is off, so OpenDesk will just open its window at sign-in." : "Turn on and save unattended access first.")
    : null;
  const relativeLastConnected = (timestamp: number | null) => {
    if (timestamp === null) return "Never connected";
    const seconds = Math.max(0, Math.floor(Date.now() / 1000) - timestamp);
    if (seconds < 60) return "Just now";
    if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
    if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
    return `${Math.floor(seconds / 86400)}d ago`;
  };
  const startHosting = () => { void api.startHosting().catch((error: unknown) => setMessage(friendlyError(error))); };
  const copyId = async () => {
    if (!identity) return;
    try { await navigator.clipboard.writeText(identity.id); setMessage("ID copied.", "info"); } catch { setMessage("Could not copy ID."); }
  };
  const copyPassword = async () => {
    if (!identity) return;
    try { await navigator.clipboard.writeText(identity.password); setMessage("Password copied.", "info"); } catch { setMessage("Could not copy password."); }
  };
  const copyInvite = async () => {
    if (!identity) return;
    try { await navigator.clipboard.writeText(buildInviteLink(identity.id, relays[0])); setMessage("Invite link copied.", "info"); } catch { setMessage("Could not copy invite link."); }
  };

  const hostingProblem = perms && (!perms.screen_recording || !perms.accessibility)
    ? !perms.screen_recording && !perms.accessibility
      ? "Hosting is off: others can't see or control this computer."
      : !perms.screen_recording
        ? "Others will see a black screen: Screen Recording is off."
        : "Others can see but not control this computer: Accessibility is off."
    : null;
  const eyeIcon = <svg viewBox="0 0 24 24" width="17" height="17" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d="M1 12s4-7 11-7 11 7 11 7-4 7-11 7-11-7-11-7z" /><circle cx="12" cy="12" r="3" /></svg>;
  const eyeOffIcon = <svg viewBox="0 0 24 24" width="17" height="17" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19M1 1l22 22M9.9 9.9a3 3 0 1 0 4.2 4.2" /></svg>;
  const rotateIcon = <svg viewBox="0 0 24 24" width="17" height="17" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d="M23 4v6h-6" /><path d="M1 20v-6h6" /><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15" /></svg>;
  const idCard = <section className="card identity-card"><div className="card-head"><span className="eyebrow">YOUR ID</span><div className="card-head__actions"><button className="quiet-button" type="button" onClick={() => void copyInvite()}>Copy invite</button></div></div><div className="id-row"><strong className="mono identity-value">{identity?.id ?? "—"}</strong><div className="id-actions"><button className="icon-button" type="button" onClick={() => void copyId()} aria-label="Copy device ID">⧉</button><button className="icon-button" type="button" disabled={confirmingNewId} onClick={() => setConfirmingNewId(true)} aria-label="Get a new ID">{rotateIcon}</button></div></div>{conflicts.length > 0 && <div className="identity-conflict" role="alert">{conflicts.map((conflict) => <p key={conflict.relay}>{conflict.message}</p>)}</div>}{confirmingNewId && <div className="modal-scrim" role="presentation" onClick={() => !resettingId && setConfirmingNewId(false)}><div className="new-id-modal" role="group" aria-label="Confirm new ID" onClick={(event) => event.stopPropagation()}><p>Get a new ID for this device? Anyone who saved {identity?.id ?? "the current ID"} on another device will need the new one to reach it.</p><div className="new-id-modal__actions"><button className="ghost-button" type="button" disabled={resettingId} onClick={() => setConfirmingNewId(false)}>Cancel</button><button className="primary-button" type="button" disabled={resettingId} onClick={() => void getNewId()}>Get new ID</button></div></div></div>}<div className="password-row"><label htmlFor="device-password">SESSION PASSWORD</label><div className="password-control"><div className="password-field"><input id="device-password" className="mono password-value" type={showPassword ? "text" : "password"} value={identity?.password ?? ""} readOnly aria-label="Session password" /><button className="eye-toggle" type="button" onClick={() => setShowPassword((shown) => !shown)} aria-label={showPassword ? "Hide password" : "Show password"}>{showPassword ? eyeOffIcon : eyeIcon}</button></div><button className="icon-button" type="button" onClick={() => void copyPassword()} aria-label="Copy password">⧉</button><button className="icon-button" type="button" onClick={() => void rotate()} aria-label="Rotate password">{rotateIcon}</button></div></div><p className="caption">{unattendedReady ? "Connect with this session password or your unattended access password; either works." : "Share your ID and password to let someone control this device."}</p></section>;

  const connectCard = <section className="card connect-card"><span className="eyebrow">CONTROL A REMOTE DEVICE</span><form onSubmit={submit}><div className="connect-fields"><input className="mono target-input" inputMode="numeric" maxLength={9} placeholder="000 000 000" value={targetId} onChange={(event) => setTargetId(event.target.value.replace(/\D/g, ""))} onPaste={(event) => { const invite = parseInvite(event.clipboardData.getData("text")); if (invite) { event.preventDefault(); setTargetId(invite.id); } }} aria-label="Remote device ID" /><button className="primary-button" type="submit" disabled={busy}>{busy ? "Connecting…" : "Connect"}</button></div></form>{recentIds.length > 0 && <div className="recent"><span className="recent-label">RECENT</span>{recentIds.map((id) => <button className="chip mono" type="button" key={id} onClick={() => setTargetId(id)}>{id}</button>)}</div>}</section>;

  const macModal = macEdit && <div className="modal-scrim" role="presentation" onClick={() => setMacEdit(null)}><form className="new-id-modal" role="dialog" aria-modal="true" aria-label="Wake-on-LAN" onClick={(event) => event.stopPropagation()} onSubmit={(event) => { event.preventDefault(); void saveMac(); }}><div><span className="eyebrow">WAKE-ON-LAN</span><strong className="connect-modal__target">{macEdit.alias}</strong></div><p className="caption" style={{ margin: 0 }}>Enter this device's MAC address to wake it over your local network. It must be on the same network and have Wake-on-LAN enabled in its BIOS and OS.</p><input className="mono" autoFocus placeholder="01:23:45:67:89:AB" value={macDraft} onChange={(event) => setMacDraft(event.target.value)} aria-label="MAC address" /><div className="new-id-modal__actions"><button className="ghost-button" type="button" onClick={() => setMacEdit(null)}>Cancel</button><button className="primary-button" type="submit">Save</button></div></form></div>;
  const connectModal = pendingConnect && <div className="modal-scrim" role="presentation" onClick={() => !busy && setPendingConnect(null)}><form className="new-id-modal connect-modal" role="dialog" aria-modal="true" aria-label="Enter device password" onClick={(event) => event.stopPropagation()} onSubmit={(event) => void submitPopup(event)}><div><span className="eyebrow">CONNECT TO</span><strong className="mono connect-modal__target">{pendingConnect.alias ?? pendingConnect.id}</strong></div><div className="password-control"><input className="password-value" type={popupShowPassword ? "text" : "password"} autoFocus placeholder="Device password" value={popupPassword} onChange={(event) => setPopupPassword(event.target.value)} aria-label="Device password" /><button className="icon-button" type="button" onClick={() => setPopupShowPassword((shown) => !shown)} aria-label={popupShowPassword ? "Hide password" : "Show password"}>{popupShowPassword ? eyeOffIcon : eyeIcon}</button></div><label className="toggle-label"><input type="checkbox" checked={popupRemember} onChange={(event) => setPopupRemember(event.target.checked)} /><span>Remember password for this device</span></label><p className="caption" style={{ margin: 0 }}>Saved passwords only keep working if the other device has unattended access on.</p><div className="new-id-modal__actions"><button className="ghost-button" type="button" disabled={busy} onClick={() => setPendingConnect(null)}>Cancel</button><button className="primary-button" type="submit" disabled={busy}>{busy ? "Connecting…" : "Connect"}</button></div></form></div>;

  const sortedContacts = [...contacts].sort((a, b) => Number(onlineIds.has(b.id)) - Number(onlineIds.has(a.id)));
  const deviceGlyph = <svg viewBox="0 0 48 36" width="46" height="34" aria-hidden="true"><rect x="3" y="3" width="42" height="27" rx="2.5" fill="#0b0e12" stroke="currentColor" strokeWidth="1.6" /><rect x="7" y="7" width="34" height="19" rx="1" fill="currentColor" opacity="0.14" /><path d="M18 33h12M24 30v3" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" /></svg>;
  const addressBody = contacts.length === 0
    ? <p className="caption">Save a device to reconnect without retyping its ID.</p>
    : addressView === "grid"
      ? <div className="device-grid">{sortedContacts.map((contact) => { const online = onlineIds.has(contact.id); return <div className={`device-tile${online ? " device-tile--online" : ""}`} key={contact.id}><button className="device-tile__main" type="button" disabled={busy} onClick={() => startConnect(contact)} aria-label={`Connect to ${contact.alias}`}><span className="device-tile__preview">{deviceGlyph}<span className={`presence-dot${online ? " presence-dot--online" : ""}`} title={online ? "Online" : "Offline or unknown"} /></span><span className="device-tile__meta"><strong>{contact.alias}</strong><span className="mono">{contact.id}</span><small>{online ? "Online" : relativeLastConnected(contact.last_connected)}</small></span></button><button className="device-tile__remove" type="button" onClick={() => void remove(contact.id)} aria-label={`Remove ${contact.alias}`}>×</button>{contact.mac && !online && <button className="device-tile__wake" type="button" onClick={() => void wake(contact.id, contact.alias)} aria-label={`Wake ${contact.alias}`} title="Wake this device">⏾ Wake</button>}</div>; })}</div>
      : <div className="contact-list">{sortedContacts.map((contact) => { const online = onlineIds.has(contact.id); return <div className="contact-row" key={contact.id}><div><strong><span className={`presence-dot${online ? " presence-dot--online" : ""}`} title={online ? "Online" : "Offline or unknown"} aria-hidden="true" />{contact.alias}</strong><span className="mono">{contact.id}</span><small>{relativeLastConnected(contact.last_connected)}</small></div><div className="contact-actions"><button className="quiet-button" type="button" disabled={busy} onClick={() => startConnect(contact)} aria-label={`Connect to ${contact.alias}`}>Connect</button>{contact.mac && <button className="quiet-button" type="button" onClick={() => void wake(contact.id, contact.alias)} aria-label={`Wake ${contact.alias}`}>Wake</button>}{contact.last_connected && <button className="quiet-button" type="button" onClick={() => openMacEdit(contact)} aria-label={`Wake-on-LAN settings for ${contact.alias}`}>{contact.mac ? "Edit MAC" : "Wake-on-LAN"}</button>}<button className="icon-button" type="button" onClick={() => void remove(contact.id)} aria-label={`Remove ${contact.alias}`}>×</button></div></div>; })}</div>;
  const addressBook = <section className="card address-book"><div className="address-book-heading"><span className="eyebrow">ADDRESS BOOK</span><div className="address-book-views" role="group" aria-label="Address book layout"><button type="button" className={`view-toggle${addressView === "grid" ? " view-toggle--active" : ""}`} aria-pressed={addressView === "grid"} aria-label="Grid view" title="Grid view" onClick={() => chooseAddressView("grid")}><svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true"><rect x="3" y="3" width="7" height="7" rx="1" /><rect x="14" y="3" width="7" height="7" rx="1" /><rect x="3" y="14" width="7" height="7" rx="1" /><rect x="14" y="14" width="7" height="7" rx="1" /></svg></button><button type="button" className={`view-toggle${addressView === "list" ? " view-toggle--active" : ""}`} aria-pressed={addressView === "list"} aria-label="List view" title="List view" onClick={() => chooseAddressView("list")}><svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" aria-hidden="true"><path d="M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01" /></svg></button></div></div>{addressBody}<form className="contact-form" onSubmit={(event) => void add(event)}><input value={alias} onChange={(event) => setAlias(event.target.value)} placeholder="Alias" aria-label="Contact alias" /><input className="mono" inputMode="numeric" maxLength={9} value={contactId} onChange={(event) => setContactId(event.target.value.replace(/\D/g, ""))} placeholder="Device ID" aria-label="Contact device ID" /><button className="primary-button" type="submit">Add</button></form></section>;

  const accessCard = <section className="card unattended-card"><div className="unattended-heading"><div><span className="eyebrow">UNATTENDED ACCESS</span><h2>Connect without approval</h2></div><label className="toggle-label"><input type="checkbox" checked={unattendedEnabled} onChange={(event) => setUnattendedEnabled(event.target.checked)} /><span>Enabled</span></label></div><input className="password-value" type="password" placeholder={unattended.has_password ? "Enter a new password to replace it" : "Set a password (12+ characters)"} value={unattendedPassword} onChange={(event) => setUnattendedPassword(event.target.value)} aria-label="Unattended access password" />{unattended.problem && <p className="unattended-problem" role="alert">{unattended.problem}</p>}<label className="toggle-label" title={twofaToggleEnabled ? undefined : "Turn on and save unattended access first"}><input type="checkbox" checked={unattended.twofa_enabled} disabled={!twofaToggleEnabled || twofaBusy || enrollment !== null} onChange={(event) => void toggleTwofa(event.target.checked)} /><span>Require 2FA code</span></label>{enrollment && <form className="twofa-enroll" onSubmit={(event) => void confirmTwofa(event)} aria-label="Set up two-factor"><QrCode width={enrollment.qr_width} modules={enrollment.qr_modules} label="QR code for your authenticator app" /><div className="twofa-enroll__details"><p className="caption">Scan with an authenticator app, or enter this key manually:</p><code className="mono twofa-secret" aria-label="Setup key">{groupSecret(enrollment.secret)}</code><input className="password-value mono" inputMode="numeric" autoComplete="one-time-code" maxLength={6} placeholder="6-digit code" aria-label="Authenticator code" value={enrollmentCode} onChange={(event) => setEnrollmentCode(event.target.value.replace(/\D/g, ""))} /><div className="twofa-enroll__actions"><button className="ghost-button" type="button" disabled={twofaBusy} onClick={() => void cancelTwofa()}>Cancel</button><button className="primary-button" type="submit" disabled={twofaBusy}>Confirm</button></div></div></form>}<label className="toggle-label"><input type="checkbox" checked={startAtSignIn ?? false} disabled={startAtSignIn === null || startAtSignInBusy || (!startAtSignIn && !unattendedReady)} onChange={(event) => void toggleStartAtSignIn(event.target.checked)} /><span>Start OpenDesk on startup</span></label>{startAtSignInHint && <p className="caption start-at-sign-in-hint">{startAtSignInHint}</p>}{windowsService?.supported && <label className="toggle-label"><input type="checkbox" checked={windowsService.installed} disabled={windowsServiceBusy} onChange={(event) => void toggleWindowsService(event.target.checked)} /><span>Run OpenDesk at the Windows sign-in screen</span></label>}{windowsService?.running && <p className="caption">Hosted by the OpenDesk service.</p>}<p className="caption">Anyone with this password can connect without approval. Protect it carefully.</p><button className="quiet-button unattended-save" type="button" disabled={!unattendedDirty} onClick={() => void saveUnattended()}>Save settings</button>{unattendedDirty && <p className="caption unattended-pending">Changes apply after you save.</p>}</section>;

  const displaysCard = <section className="card display-card"><span className="eyebrow">VIRTUAL DISPLAYS</span><h2>Extra screens on demand</h2><p className="caption" style={{ marginTop: 0 }}>Let a viewer add a software-only screen during a session, for example a second monitor, or a display for a computer with no monitor attached. It is removed when the session ends. Windows needs a free driver; Linux is not supported yet.</p><label className="toggle-label"><input type="checkbox" checked={allowVirtualDisplays ?? false} disabled={allowVirtualDisplays === null} onChange={(event) => toggleVirtualDisplays(event.target.checked)} /><span>Let viewers add virtual displays</span></label>{displayDriver?.applies && (allowVirtualDisplays || displayDriver.installed) && (displayDriver.installed
    ? <div className="driver-row"><span className="caption">Virtual display driver installed.</span><button className="quiet-button" type="button" disabled={driverBusy} onClick={() => void changeDisplayDriver(false)}>{driverBusy ? "Removing…" : "Uninstall driver"}</button></div>
    : confirmingDriver
      ? <div className="new-id-confirm driver-confirm" role="group" aria-label="Install virtual display driver"><p>OpenDesk downloads Amyuni's free virtual display driver from amyuni.com, checks it is the exact known file, and installs it. Windows asks for administrator approval, and any user on this computer can then add and remove its displays. Amyuni's licence:</p><pre className="driver-license">{displayDriver.license}</pre><div><button className="ghost-button" type="button" disabled={driverBusy} onClick={() => setConfirmingDriver(false)}>Cancel</button><button className="primary-button" type="button" disabled={driverBusy} onClick={() => void changeDisplayDriver(true)}>{driverBusy ? "Installing…" : "Accept and install"}</button></div></div>
      : <div className="driver-row"><span className="caption">Windows needs a driver for virtual displays.</span><button className="quiet-button" type="button" onClick={() => setConfirmingDriver(true)}>Install virtual display driver</button></div>)}</section>;

  const allowListCard = <section className="card allow-list-card"><span className="eyebrow">ALLOWED DEVICES</span><h2>Known devices</h2><p className="caption">Known devices are marked in the approval prompt. They still need your approval and the session password.</p>{allowedDevices.length > 0 ? <div className="contact-list">{allowedDevices.map((id) => <div className="contact-row" key={id}><span className="mono">{id}</span><button className="icon-button" type="button" onClick={() => void removeAllowedDevice(id)} aria-label={`Remove allowed device ${id}`}>×</button></div>)}</div> : <p className="caption">No devices are allowed yet.</p>}<form className="contact-form" onSubmit={(event) => void addAllowedDevice(event)}><input className="mono" inputMode="numeric" maxLength={9} value={allowedId} onChange={(event) => setAllowedId(event.target.value.replace(/\D/g, ""))} placeholder="Device ID" aria-label="Allowed device ID" /><button className="primary-button" type="submit">Add</button></form></section>;

  const networkCard = <section className="card relay-card"><span className="eyebrow">RELAY REGIONS</span><h2>Relay endpoints</h2><textarea className="relay-editor mono" value={relayText} onChange={(event) => setRelayText(event.target.value)} aria-label="Relay URLs" rows={Math.max(2, relays.length)} /><button className="quiet-button" type="button" onClick={() => void saveRelays()}>Save relay list</button><p className="caption">One WebSocket URL per line. OpenDesk contacts them all at once and uses whichever answers fastest, so add relays near the people you connect with.</p></section>;

  const aboutCard = <section className="card about-card"><span className="eyebrow">ABOUT</span><div className="about-head"><h2>OpenDesk</h2>{version && <span className="about-badge mono">v{version}</span>}</div><p className="caption">Open-source remote desktop. End-to-end encrypted sessions over your own relay.</p><div className="about-actions"><button className="quiet-button" type="button" onClick={() => window.dispatchEvent(new Event(CHECK_NOW_EVENT))}>Check for updates</button><button className="quiet-button" type="button" onClick={onOpenPermissions}>Permissions</button>{closeKeepsRunning && <button className="quiet-button" type="button" onClick={() => void api.quitApp().catch(() => undefined)}>Quit OpenDesk</button>}</div></section>;

  const settingsTabs: { id: typeof settingsTab; label: string }[] = [
    { id: "access", label: "Access" },
    { id: "security", label: "Security" },
    { id: "network", label: "Network" },
    { id: "recordings", label: "Recordings" },
    { id: "about", label: "About" },
  ];

  return <main className="app-shell">
    <header className="topbar"><div className="brand"><svg className="brand-mark" viewBox="0 0 48 48" aria-hidden="true"><g fill="none" strokeWidth="6"><circle cx="19" cy="29" r="13" stroke="#22d3ee" /><circle cx="29" cy="19" r="13" stroke="#f5b921" /></g><circle cx="29" cy="19" r="5" fill="#22d3ee" /></svg> OpenDesk</div><div className="topbar-actions"><div className="status" data-status={status}><span className="status-dot" aria-hidden="true" /><span>{statusLabels[status]}</span></div>{status === "offline" && <button className="quiet-button" type="button" onClick={startHosting}>Start hosting</button>}{view === "connect"
      ? <button className="icon-button settings-toggle" type="button" onClick={() => setView("settings")} aria-label="Settings" title="Settings"><svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="3" /><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" /></svg></button>
      : <button className="icon-button settings-toggle" type="button" onClick={() => setView("connect")} aria-label="Home" title="Home"><svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d="M3 11.5 12 4l9 7.5" /><path d="M5 10v9h5v-6h4v6h5v-9" /></svg></button>}</div></header>
    {hostingProblem && <button className="perm-banner" type="button" onClick={onOpenPermissions}><span>⚠ {hostingProblem}</span><span className="perm-banner__cta">Fix permissions →</span></button>}
    <div className="view-swap" key={view}>
    {view === "connect" ? <>
      <section className="hero"><span className="eyebrow">PERSONAL DEVICE ACCESS</span><h1>Remote access, end to end.</h1><p>Share your ID to let someone help you, or enter another device's ID to control it.</p></section>
      <div className="home-grid">
        <div className="home-col">{idCard}</div>
        <div className="home-col">{connectCard}</div>
      </div>
      {addressBook}
      {connectModal}
      {macModal}
    </> : <div className="settings-layout">
      <nav className="settings-nav" aria-label="Settings sections">{settingsTabs.map((tab) => <button key={tab.id} type="button" className={`settings-nav__item${settingsTab === tab.id ? " settings-nav__item--active" : ""}`} aria-current={settingsTab === tab.id} onClick={() => setSettingsTab(tab.id)}>{tab.label}</button>)}</nav>
      <div className="settings-panel">
        {settingsTab === "access" && accessCard}
        {settingsTab === "security" && <>{displaysCard}{allowListCard}</>}
        {settingsTab === "network" && networkCard}
        {settingsTab === "recordings" && <Recordings />}
        {settingsTab === "about" && aboutCard}
      </div>
    </div>}
    </div>
  </main>;
}
