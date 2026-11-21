/**
 * Key combinations a browser cannot pass through (the OS or the browser
 * keeps them), sent to the host as synthetic key presses instead.
 */
import { encodeInputEvent } from "../../../viewer/src/lib/input";

const SHIFT = 1;
const CTRL = 1 << 1;
const ALT = 1 << 2;
const META = 1 << 3;

export interface SpecialKey {
  id: string;
  label: string;
  /** Named key code, as in viewer/src/lib/input.ts and host-core's map_key. */
  code: number;
  modifiers: number;
}

export const SPECIAL_KEYS: SpecialKey[] = [
  { id: "ctrl-alt-del", label: "Ctrl+Alt+Del", code: 46, modifiers: CTRL | ALT },
  { id: "meta", label: "Windows / Command key", code: 91, modifiers: 0 },
  { id: "alt-tab", label: "Alt+Tab", code: 9, modifiers: ALT },
  { id: "alt-f4", label: "Alt+F4", code: 115, modifiers: ALT },
  { id: "ctrl-esc", label: "Ctrl+Esc", code: 27, modifiers: CTRL },
  { id: "ctrl-shift-esc", label: "Ctrl+Shift+Esc", code: 27, modifiers: CTRL | SHIFT },
  { id: "cmd-tab", label: "Command+Tab", code: 9, modifiers: META },
  { id: "cmd-space", label: "Command+Space", code: 32, modifiers: META },
  { id: "esc", label: "Esc", code: 27, modifiers: 0 },
];

/** The press and release of `key`, encoded for the host. */
export function specialKeyEvents(key: SpecialKey, tsUs = Math.floor(Date.now() * 1000)): Uint8Array[] {
  return [true, false].map((down) => encodeInputEvent({ kind: "key", code: key.code, down, modifiers: key.modifiers }, tsUs));
}
