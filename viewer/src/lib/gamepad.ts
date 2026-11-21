/**
 * Gamepad forwarding (protocol::input::GamepadState). The Gamepad API is
 * polled once per animation frame; a pad's state goes out only when it
 * changed, and at most once per MIN_INTERVAL_MS per pad.
 */

/** protocol::input::MAX_GAMEPADS. */
export const MAX_GAMEPADS = 4;
/** Fastest a pad's state is resent (about 120 updates a second). */
export const MIN_INTERVAL_MS = 8;

export interface GamepadState {
  kind: "gamepad";
  index: number;
  connected: boolean;
  /** Bit i is standard-mapping button i (0..16). */
  buttons: number;
  /** Left X, left Y, right X, right Y in -32768..32767; positive is right and down. */
  axes: [number, number, number, number];
  /** Left and right trigger, 0..255. */
  triggers: [number, number];
}

const STANDARD_BUTTONS = 17;

function axis(value: number | undefined): number {
  const v = Number.isFinite(value) ? Math.min(1, Math.max(-1, value!)) : 0;
  return Math.round(v * 32767);
}

function trigger(button: GamepadButton | undefined): number {
  if (!button) return 0;
  const value = Number.isFinite(button.value) ? button.value : button.pressed ? 1 : 0;
  return Math.round(Math.min(1, Math.max(0, value)) * 255);
}

/** The protocol state of `pad`, assuming the standard mapping. */
export function readGamepad(pad: Gamepad, index: number): GamepadState {
  let buttons = 0;
  for (let i = 0; i < Math.min(STANDARD_BUTTONS, pad.buttons.length); i++) {
    if (pad.buttons[i].pressed) buttons |= 1 << i;
  }
  return {
    kind: "gamepad",
    index,
    connected: true,
    buttons: buttons >>> 0,
    axes: [axis(pad.axes[0]), axis(pad.axes[1]), axis(pad.axes[2]), axis(pad.axes[3])],
    triggers: [trigger(pad.buttons[6]), trigger(pad.buttons[7])],
  };
}

/** Everything released, sticks centred. */
export function neutralGamepad(index: number, connected = true): GamepadState {
  return { kind: "gamepad", index, connected, buttons: 0, axes: [0, 0, 0, 0], triggers: [0, 0] };
}

export function sameGamepad(a: GamepadState, b: GamepadState): boolean {
  return a.connected === b.connected && a.buttons === b.buttons && a.triggers[0] === b.triggers[0] && a.triggers[1] === b.triggers[1] && a.axes.every((value, i) => value === b.axes[i]);
}

/** Write the state after the 9-byte event header (tag 18, 25 bytes in all). */
export function writeGamepad(view: DataView, state: GamepadState): void {
  view.setUint8(9, state.index);
  view.setUint8(10, state.connected ? 1 : 0);
  view.setUint32(11, state.buttons >>> 0, false);
  state.axes.forEach((value, i) => view.setInt16(15 + i * 2, value, false));
  view.setUint8(23, state.triggers[0]);
  view.setUint8(24, state.triggers[1]);
}

/** Why the host can't take a gamepad, from InputSupport's gamepad_unavailable code. */
export function gamepadUnavailableText(code: string | undefined): string {
  switch (code) {
    case "vigembus_missing": return "The remote computer needs the ViGEmBus driver installed to accept a gamepad.";
    case "uinput_missing": return "The remote computer has no /dev/uinput, which virtual gamepads need on Linux.";
    case "no_permission": return "The remote computer's user can't write /dev/uinput, which virtual gamepads need on Linux.";
    case "unsupported_os": return "Gamepads aren't supported when the remote computer is a Mac.";
    case undefined: return "The remote computer doesn't accept gamepads.";
    default: return `The remote computer can't accept a gamepad (${code}).`;
  }
}

export interface GamepadOptions {
  /** Forward input now (window focused, not view only). While false, pads held centred and released. */
  active: () => boolean;
  getGamepads?: () => ArrayLike<Gamepad | null>;
  now?: () => number;
  schedule?: (callback: () => void) => number;
  cancel?: (handle: number) => void;
}

/**
 * Forward the local gamepads until the returned function is called, which
 * reports every forwarded pad as disconnected so the host unplugs it.
 */
export function attachGamepads(send: (state: GamepadState) => void, options: GamepadOptions): () => void {
  const getGamepads = options.getGamepads ?? (() => (typeof navigator.getGamepads === "function" ? navigator.getGamepads() : []));
  const now = options.now ?? (() => performance.now());
  const schedule = options.schedule ?? ((callback) => window.requestAnimationFrame(callback));
  const cancel = options.cancel ?? ((handle) => window.cancelAnimationFrame(handle));
  const sent = new Map<number, GamepadState>();
  const sentAt = new Map<number, number>();
  let handle = 0;

  const emit = (state: GamepadState, at: number) => {
    send(state);
    if (state.connected) sent.set(state.index, state);
    else sent.delete(state.index);
    sentAt.set(state.index, at);
  };

  const poll = () => {
    const at = now();
    const active = options.active();
    const pads = active ? getGamepads() : [];
    for (let index = 0; index < MAX_GAMEPADS; index++) {
      const previous = sent.get(index);
      const pad = pads[index];
      let next: GamepadState | undefined;
      if (pad && pad.connected) next = readGamepad(pad, index);
      // Not forwarding right now: let go of everything but keep the pad
      // plugged in, so a game doesn't see its controller vanish.
      else if (!active && previous) next = neutralGamepad(index);
      else if (previous) next = neutralGamepad(index, false);
      if (!next || (previous && sameGamepad(previous, next))) continue;
      // Unplugs always go out; other changes wait out the interval.
      if (next.connected && previous && at - (sentAt.get(index) ?? -Infinity) < MIN_INTERVAL_MS) continue;
      emit(next, at);
    }
    handle = schedule(poll);
  };
  handle = schedule(poll);

  return () => {
    cancel(handle);
    for (const index of [...sent.keys()]) emit(neutralGamepad(index, false), now());
  };
}
