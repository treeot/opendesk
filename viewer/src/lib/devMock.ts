// Browser dev fallback. When the app runs in a plain browser (`npm run dev`)
// there is no Tauri bridge, so every invoke() would throw
// "window.__TAURI_INTERNALS__ is undefined". This module stands in for the
// backend with believable, interactive fake data so the UI can be worked on
// without launching the desktop app. It is never used inside the real Tauri
// window (see `isTauri` in api.ts).

const randomDigits = (n: number) => Array.from({ length: n }, () => Math.floor(Math.random() * 10)).join("");
const randomPassword = () => Array.from({ length: 8 }, () => "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"[Math.floor(Math.random() * 32)]).join("");

// Mutable session state so interactions (rotate, add contact, toggle 2FA…) stick.
const state = {
  identity: { id: "481902375", password: "K7mP4xQ2" },
  config: {
    relay_url: "wss://relay.treeot.dev/ws",
    relay_urls: ["wss://relay.treeot.dev/ws"],
    recent_ids: ["730514826", "905218437"],
    clipboard_sync: true,
    allow_virtual_displays: true,
    recordings_max_bytes: 5 * 1024 * 1024 * 1024,
  },
  unattended: { enabled: true, has_password: true, twofa_enabled: false, ready: true, problem: null as string | null },
  contacts: [
    { id: "730514826", alias: "Home PC", last_connected: Math.floor(Date.now() / 1000) - 3600, mac: "A4:83:E7:2C:19:0F" as string | null },
    { id: "905218437", alias: "Office Workstation", last_connected: Math.floor(Date.now() / 1000) - 86400 * 2 },
    { id: "118273645", alias: "Mom's Laptop", last_connected: null as number | null },
  ],
  allowed: ["730514826", "905218437"],
  savedPasswords: new Set<string>(["730514826"]),
  recordings: [
    { name: "OpenDesk 2026-10-22 14-05.webm", path: "/Users/you/Movies/OpenDesk 2026-10-22 14-05.webm", size: 48_200_000, modified: Date.now() - 7200_000 },
    { name: "OpenDesk 2026-10-19 09-31.webm", path: "/Users/you/Movies/OpenDesk 2026-10-19 09-31.webm", size: 12_750_000, modified: Date.now() - 86400_000 * 5 },
  ],
};

// A small fake QR pattern so the 2FA enrollment view renders something.
const fakeQr = (width: number) => Array.from({ length: width * width }, (_, i) => (i * 7 + ((i / width) | 0)) % 3 === 0);

const handlers: Record<string, (args: Record<string, unknown>) => unknown> = {
  get_identity: () => state.identity,
  rotate_password: () => { state.identity.password = randomPassword(); return { password: state.identity.password }; },
  reset_identity: () => { state.identity.id = randomDigits(9); return state.identity.id; },
  get_identity_conflicts: () => [],
  get_config: () => state.config,
  list_contacts: () => state.contacts.map((c) => ({ ...c, has_saved_password: state.savedPasswords.has(c.id) })),
  set_contact_password: ({ peerId, password }) => { if (password) state.savedPasswords.add(peerId as string); else state.savedPasswords.delete(peerId as string); },
  set_contact_mac: ({ peerId, mac }) => {
    const value = (mac as string).trim();
    const contact = state.contacts.find((c) => c.id === peerId);
    if (contact) contact.mac = value === "" ? null : value;
    return contact;
  },
  wake_contact: () => undefined,
  list_allowed: () => state.allowed,
  add_allowed: ({ peerId }) => { if (!state.allowed.includes(peerId as string)) state.allowed.push(peerId as string); },
  remove_allowed: ({ peerId }) => { state.allowed = state.allowed.filter((id) => id !== peerId); },
  add_contact: ({ alias, peerId }) => {
    const contact = { id: peerId as string, alias: alias as string, last_connected: null as number | null };
    state.contacts = [...state.contacts.filter((c) => c.id !== contact.id), contact];
    return contact;
  },
  remove_contact: ({ peerId }) => { state.contacts = state.contacts.filter((c) => c.id !== peerId); state.savedPasswords.delete(peerId as string); },
  touch_contact: () => undefined,
  set_relay: () => undefined,
  set_relays: ({ urls }) => { state.config.relay_urls = urls as string[]; },
  list_relays: () => state.config.relay_urls,
  get_unattended_status: () => state.unattended,
  windows_service_status: () => ({ supported: false, installed: false, running: false }),
  install_windows_service: () => ({ supported: true, installed: true, running: true }),
  uninstall_windows_service: () => ({ supported: true, installed: false, running: false }),
  set_unattended_access: ({ enabled, password }) => {
    state.unattended.enabled = enabled as boolean;
    if (password) state.unattended.has_password = true;
    state.unattended.ready = state.unattended.enabled && state.unattended.has_password;
    state.unattended.problem = null;
    return state.unattended;
  },
  set_unattended_2fa: ({ enabled }) => {
    if (!enabled) { state.unattended.twofa_enabled = false; return null; }
    return { uri: "otpauth://totp/OpenDesk:demo?secret=JBSWY3DPEHPK3PXP&issuer=OpenDesk", secret: "JBSWY3DPEHPK3PXP", qr_width: 21, qr_modules: fakeQr(21) };
  },
  confirm_unattended_2fa: () => { state.unattended.twofa_enabled = true; return state.unattended; },
  permission_status: () => ({ screen_recording: true, accessibility: true }),
  close_keeps_running: () => false,
  get_presence: ({ ids }) => (ids as string[]).filter((_, i) => i % 2 === 0),
  virtual_display_driver_status: () => ({ applies: false, installed: false, license: "" }),
  set_allow_virtual_displays: ({ enabled }) => { state.config.allow_virtual_displays = enabled as boolean; },
  list_recordings: () => state.recordings,
  delete_recording: ({ path }) => { state.recordings = state.recordings.filter((r) => r.path !== path); },
  set_recordings_max_bytes: ({ bytes }) => { state.config.recordings_max_bytes = bytes as number; },
};

export const devInvoke = <T>(command: string, args?: Record<string, unknown>): Promise<T> => {
  const handler = handlers[command];
  const value = handler ? handler(args ?? {}) : undefined;
  return Promise.resolve(value as T);
};
