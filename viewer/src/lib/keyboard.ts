/**
 * Keyboard modes for viewers and hosts with different keyboard layouts.
 *
 * - map: send the physical key (DOM `code`) and let the host's layout decide
 *   the character. Right for games and shortcuts that live in a fixed place.
 * - translate: send the character the viewer's own layout produced (DOM
 *   `key`, after dead keys and AltGr), which the host types as text. Keys
 *   that type nothing (arrows, Enter, F-keys) and shortcuts still go as keys.
 *
 * Translate is the default: most people expect the key they press to type
 * what its keycap says, and a German or French keyboard on a US host would
 * otherwise type the wrong letters and symbols.
 *
 * Hosts only understand these events once they announce support; until then
 * `attachInput` keeps the original key events (the "legacy" mode here).
 */
import type { InputEvent } from "./input";

export type KeyboardMode = "map" | "translate";

export const DEFAULT_KEYBOARD_MODE: KeyboardMode = "translate";

export const KEYBOARD_MODES: { mode: KeyboardMode; label: string; title: string }[] = [
  { mode: "translate", label: "Type characters", title: "Type what your keyboard shows, whatever the remote layout. Best for text." },
  { mode: "map", label: "Physical keys", title: "Press the same key position on the remote keyboard. Best for games and shortcuts." },
];

type Storage = Pick<globalThis.Storage, "getItem" | "setItem">;

const storageKey = (remoteId: string) => `opendesk.keyboard.${remoteId}`;

function defaultStorage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

/** The mode last chosen for `remoteId`, or translate. */
export function loadKeyboardMode(remoteId: string, storage: Storage | undefined = defaultStorage()): KeyboardMode {
  let saved: string | null = null;
  try { saved = storage?.getItem(storageKey(remoteId)) ?? null; } catch { /* unavailable */ }
  return KEYBOARD_MODES.some(({ mode }) => mode === saved) ? (saved as KeyboardMode) : DEFAULT_KEYBOARD_MODE;
}

export function saveKeyboardMode(remoteId: string, mode: KeyboardMode, storage: Storage | undefined = defaultStorage()) {
  try { storage?.setItem(storageKey(remoteId), mode); } catch { /* not remembered */ }
}

/**
 * DOM `code` to USB HID usage (page << 16 | id). Matches the table in
 * crates/host-core/src/keyboard.rs.
 */
const HID_USAGE: Record<string, number> = (() => {
  const table: Record<string, number> = {};
  for (let i = 0; i < 26; i++) table[`Key${String.fromCharCode(65 + i)}`] = 0x070004 + i;
  for (let i = 1; i <= 9; i++) table[`Digit${i}`] = 0x07001d + i;
  table.Digit0 = 0x070027;
  for (let i = 1; i <= 12; i++) table[`F${i}`] = 0x070039 + i;
  for (let i = 13; i <= 24; i++) table[`F${i}`] = 0x07005b + i;
  for (let i = 1; i <= 9; i++) table[`Numpad${i}`] = 0x070058 + i;
  table.Numpad0 = 0x070062;
  const named: [string, number][] = [
    ["Enter", 0x070028], ["Escape", 0x070029], ["Backspace", 0x07002a], ["Tab", 0x07002b], ["Space", 0x07002c],
    ["Minus", 0x07002d], ["Equal", 0x07002e], ["BracketLeft", 0x07002f], ["BracketRight", 0x070030], ["Backslash", 0x070031],
    ["Semicolon", 0x070033], ["Quote", 0x070034], ["Backquote", 0x070035], ["Comma", 0x070036], ["Period", 0x070037],
    ["Slash", 0x070038], ["CapsLock", 0x070039], ["PrintScreen", 0x070046], ["ScrollLock", 0x070047], ["Pause", 0x070048],
    ["Insert", 0x070049], ["Home", 0x07004a], ["PageUp", 0x07004b], ["Delete", 0x07004c], ["End", 0x07004d],
    ["PageDown", 0x07004e], ["ArrowRight", 0x07004f], ["ArrowLeft", 0x070050], ["ArrowDown", 0x070051], ["ArrowUp", 0x070052],
    ["NumLock", 0x070053], ["NumpadDivide", 0x070054], ["NumpadMultiply", 0x070055], ["NumpadSubtract", 0x070056],
    ["NumpadAdd", 0x070057], ["NumpadEnter", 0x070058], ["NumpadDecimal", 0x070063], ["IntlBackslash", 0x070064],
    ["ContextMenu", 0x070065], ["NumpadEqual", 0x070067], ["AudioVolumeMute", 0x07007f], ["AudioVolumeUp", 0x070080],
    ["AudioVolumeDown", 0x070081], ["IntlRo", 0x070087], ["KanaMode", 0x070088], ["IntlYen", 0x070089], ["Convert", 0x07008a],
    ["NonConvert", 0x07008b], ["Lang1", 0x070090], ["Lang2", 0x070091],
    ["ControlLeft", 0x0700e0], ["ShiftLeft", 0x0700e1], ["AltLeft", 0x0700e2], ["MetaLeft", 0x0700e3],
    ["ControlRight", 0x0700e4], ["ShiftRight", 0x0700e5], ["AltRight", 0x0700e6], ["MetaRight", 0x0700e7],
    ["MediaTrackNext", 0x0c00b5], ["MediaTrackPrevious", 0x0c00b6], ["MediaStop", 0x0c00b7], ["MediaPlayPause", 0x0c00cd],
  ];
  for (const [code, usage] of named) table[code] = usage;
  // Older names some browsers still report.
  table.OSLeft = table.MetaLeft;
  table.OSRight = table.MetaRight;
  table.VolumeMute = table.AudioVolumeMute;
  table.VolumeUp = table.AudioVolumeUp;
  table.VolumeDown = table.AudioVolumeDown;
  return table;
})();

/** The HID usage of DOM `code`, or null for keys the host can't press. */
export function hidUsage(code: string): number | null {
  return Object.prototype.hasOwnProperty.call(HID_USAGE, code) ? HID_USAGE[code] : null;
}

/** The parts of a KeyboardEvent the mapping reads. */
export interface KeyInput {
  key: string;
  code: string;
  shiftKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
  isComposing?: boolean;
  getModifierState?(key: string): boolean;
}

/** What one key press sends, and what its release sends later. */
export interface KeyAction { press: InputEvent[]; release: InputEvent[] }

const MODIFIER_KEYS = new Set(["Shift", "Control", "Alt", "Meta", "AltGraph", "OS", "Hyper", "Super", "Fn", "FnLock"]);

function modifierBits(event: KeyInput, altGraph: boolean): number {
  return (
    (event.shiftKey ? 1 : 0) |
    (event.ctrlKey && !altGraph ? 1 << 1 : 0) |
    (event.altKey && !altGraph ? 1 << 2 : 0) |
    (event.metaKey ? 1 << 3 : 0)
  );
}

/** A `key` value that types something, as opposed to a named key like "Enter". */
export function isPrintable(key: string): boolean {
  const points = [...key];
  if (points.length === 0) return false;
  if (points.length === 1) return !/^[\p{Cc}\p{Cf}]$/u.test(key);
  // Named keys are ASCII words ("Enter", "F5", "ArrowUp"); anything else of
  // several code points is composed text such as an emoji sequence.
  return !/^[A-Za-z][A-Za-z0-9]*$/.test(key);
}

function scan(usage: number, modifiers: number): KeyAction {
  return {
    press: [{ kind: "scanKey", usage, down: true, modifiers }],
    release: [{ kind: "scanKey", usage, down: false, modifiers }],
  };
}

/**
 * The events for pressing `event` in `mode`. `mac` is true when the viewer
 * runs on a Mac, where Option types characters rather than acting as a
 * shortcut modifier. Null when the press sends nothing.
 */
export function keyAction(mode: KeyboardMode, event: KeyInput, mac: boolean): KeyAction | null {
  const usage = hidUsage(event.code);
  if (mode === "map") {
    if (usage === null) return null;
    // A Mac reports Caps Lock once when it turns on and once when it turns
    // off, so each report is a full press.
    if (mac && event.code === "CapsLock") {
      const tap = scan(usage, 0);
      return { press: [...tap.press, ...tap.release], release: [] };
    }
    return scan(usage, 0);
  }

  const key = event.key || "";
  // A dead key types nothing yet: the composed character arrives with the
  // next key. IME composition is left to the IME.
  if (event.isComposing || key === "Dead" || key === "Process") return null;
  // Modifiers ride along on the keys they change. Caps Lock is already in
  // the case of the characters typed.
  if (MODIFIER_KEYS.has(key) || key === "CapsLock") return null;

  const altGraph = event.getModifierState?.("AltGraph") ?? false;
  const modifiers = modifierBits(event, altGraph);
  const shortcut = (event.ctrlKey && !altGraph) || event.metaKey || (event.altKey && !altGraph && !mac);

  if (isPrintable(key)) {
    if (!shortcut) {
      return { press: [...key].map((ch) => ({ kind: "typeChar", ch })), release: [] };
    }
    // A letter or digit shortcut goes by the character, so Ctrl+Z is undo
    // whichever key types Z on either side. The host finds that key in its
    // own layout.
    if (/^[A-Za-z0-9]$/.test(key)) {
      const ch = key.toLowerCase();
      return {
        press: [{ kind: "char", ch, down: true, modifiers }],
        release: [{ kind: "char", ch, down: false, modifiers }],
      };
    }
  }
  // Keys that type nothing, and shortcuts on other characters (Cyrillic
  // letters, punctuation that moves between layouts): the physical key.
  if (usage === null) return null;
  return scan(usage, modifiers);
}

/**
 * Keys held down in map or translate mode, so each release matches its
 * press (even if Shift changed `key` in between) and focus loss or teardown
 * can release everything.
 */
export class KeyboardState {
  private held = new Map<string, InputEvent[]>();

  constructor(private readonly mac = isMacPlatform()) {}

  private static id(event: KeyInput): string {
    return event.code ? `c:${event.code}` : `k:${event.key}`;
  }

  /** The events for a keydown, auto-repeats included. */
  down(mode: KeyboardMode, event: KeyInput): InputEvent[] {
    const action = keyAction(mode, event, this.mac);
    if (!action) return [];
    const id = KeyboardState.id(event);
    if (!this.held.has(id) && action.release.length > 0) this.held.set(id, action.release);
    return action.press;
  }

  /**
   * The events for a keyup, or null when this tracker never saw the press
   * (it went out in the original key format, or before focus).
   */
  up(mode: KeyboardMode, event: KeyInput): InputEvent[] | null {
    // Caps Lock turning off on a Mac: a full press, as when it turned on.
    if (mode === "map" && this.mac && event.code === "CapsLock") return keyAction(mode, event, this.mac)?.press ?? [];
    const id = KeyboardState.id(event);
    const release = this.held.get(id);
    this.held.delete(id);
    // A Mac drops the keyup of keys released while Cmd is down, so letting
    // go of Cmd releases whatever is still held.
    if (this.mac && (event.code === "MetaLeft" || event.code === "MetaRight")) return [...this.releaseAll(), ...(release ?? [])];
    return release ?? null;
  }

  /** Releases for every key still held, newest first. */
  releaseAll(): InputEvent[] {
    const releases = [...this.held.values()].reverse().flat();
    this.held.clear();
    return releases;
  }
}

/** The viewer runs on a Mac. */
export function isMacPlatform(nav: Pick<Navigator, "platform" | "userAgent"> | undefined = typeof navigator === "undefined" ? undefined : navigator): boolean {
  if (!nav) return false;
  return /Mac|iPhone|iPad/.test(nav.platform || "") || /Mac OS X/.test(nav.userAgent || "");
}
