// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { attachInput, encodeInputEvent } from "./input";
import { DEFAULT_KEYBOARD_MODE, hidUsage, isMacPlatform, isPrintable, KeyboardState, keyAction, loadKeyboardMode, saveKeyboardMode, type KeyInput } from "./keyboard";

const key = (key: string, code: string, mods: Partial<KeyInput> = {}): KeyInput => ({
  key, code, shiftKey: false, ctrlKey: false, altKey: false, metaKey: false, ...mods,
});
const altGr = (k: string, code: string): KeyInput => key(k, code, { ctrlKey: true, altKey: true, getModifierState: (name) => name === "AltGraph" });

describe("HID usage table", () => {
  it("maps DOM codes to USB HID usages", () => {
    expect(hidUsage("KeyA")).toBe(0x070004);
    expect(hidUsage("KeyZ")).toBe(0x07001d);
    expect(hidUsage("Digit1")).toBe(0x07001e);
    expect(hidUsage("Digit0")).toBe(0x070027);
    expect(hidUsage("F1")).toBe(0x07003a);
    expect(hidUsage("F12")).toBe(0x070045);
    expect(hidUsage("F13")).toBe(0x070068);
    expect(hidUsage("F24")).toBe(0x070073);
    expect(hidUsage("Numpad1")).toBe(0x070059);
    expect(hidUsage("Numpad0")).toBe(0x070062);
    expect(hidUsage("ArrowUp")).toBe(0x070052);
    expect(hidUsage("IntlBackslash")).toBe(0x070064);
    expect(hidUsage("AltRight")).toBe(0x0700e6);
    expect(hidUsage("OSLeft")).toBe(hidUsage("MetaLeft"));
    expect(hidUsage("MediaPlayPause")).toBe(0x0c00cd);
  });

  it("knows nothing about unknown or inherited names", () => {
    expect(hidUsage("")).toBeNull();
    expect(hidUsage("Unidentified")).toBeNull();
    expect(hidUsage("toString")).toBeNull();
  });
});

describe("printable keys", () => {
  it("tells characters from named keys", () => {
    for (const k of ["a", "@", "é", " ", "ß", "\u{1F600}", "\u{1F44D}\u{1F3FD}"]) expect(isPrintable(k)).toBe(true);
    for (const k of ["", "Enter", "F5", "ArrowUp", "Dead", "\u0007"]) expect(isPrintable(k)).toBe(false);
  });
});

describe("map mode", () => {
  it("sends the physical key with no modifier bits", () => {
    // "z" on a German keyboard is the key a US keyboard calls Y.
    expect(keyAction("map", key("z", "KeyY", { shiftKey: true }), false)).toEqual({
      press: [{ kind: "scanKey", usage: 0x07001c, down: true, modifiers: 0 }],
      release: [{ kind: "scanKey", usage: 0x07001c, down: false, modifiers: 0 }],
    });
  });

  it("forwards modifier keys themselves", () => {
    expect(keyAction("map", key("Shift", "ShiftLeft", { shiftKey: true }), false)?.press).toEqual([{ kind: "scanKey", usage: 0x0700e1, down: true, modifiers: 0 }]);
  });

  it("taps Caps Lock on a Mac and skips unknown keys", () => {
    expect(keyAction("map", key("CapsLock", "CapsLock"), true)).toEqual({
      press: [{ kind: "scanKey", usage: 0x070039, down: true, modifiers: 0 }, { kind: "scanKey", usage: 0x070039, down: false, modifiers: 0 }],
      release: [],
    });
    expect(keyAction("map", key("CapsLock", "CapsLock"), false)?.release).toHaveLength(1);
    expect(keyAction("map", key("x", ""), false)).toBeNull();
  });
});

describe("translate mode", () => {
  it("types the character the viewer's layout produced", () => {
    expect(keyAction("translate", key("A", "KeyA", { shiftKey: true }), false)).toEqual({ press: [{ kind: "typeChar", ch: "A" }], release: [] });
    // AZERTY: the key in the US "Q" spot types "a".
    expect(keyAction("translate", key("a", "KeyQ"), false)?.press).toEqual([{ kind: "typeChar", ch: "a" }]);
  });

  it("types AltGr characters instead of sending Ctrl+Alt", () => {
    // German "@" is AltGr+Q; Windows reports AltGr as Ctrl+Alt.
    expect(keyAction("translate", altGr("@", "KeyQ"), false)).toEqual({ press: [{ kind: "typeChar", ch: "@" }], release: [] });
  });

  it("types Option characters on a Mac but treats Alt as a shortcut elsewhere", () => {
    expect(keyAction("translate", key("@", "KeyL", { altKey: true }), true)?.press).toEqual([{ kind: "typeChar", ch: "@" }]);
    expect(keyAction("translate", key("@", "KeyL", { altKey: true }), false)?.press).toEqual([{ kind: "scanKey", usage: 0x07000f, down: true, modifiers: 0b0100 }]);
  });

  it("waits out dead keys and IME composition", () => {
    expect(keyAction("translate", key("Dead", "BracketRight"), false)).toBeNull();
    expect(keyAction("translate", key("Process", "KeyA"), false)).toBeNull();
    expect(keyAction("translate", key("a", "KeyA", { isComposing: true }), false)).toBeNull();
    // The composed character arrives with the next key.
    expect(keyAction("translate", key("é", "KeyE"), false)?.press).toEqual([{ kind: "typeChar", ch: "é" }]);
  });

  it("does not send modifier or Caps Lock keys on their own", () => {
    for (const k of ["Shift", "Control", "Alt", "Meta", "AltGraph", "CapsLock"]) expect(keyAction("translate", key(k, "ShiftLeft"), false)).toBeNull();
  });

  it("sends letter shortcuts by character so Ctrl+Z stays undo", () => {
    // German keyboard, Ctrl+Z: the key sits where a US keyboard has Y.
    expect(keyAction("translate", key("z", "KeyY", { ctrlKey: true }), false)).toEqual({
      press: [{ kind: "char", ch: "z", down: true, modifiers: 0b0010 }],
      release: [{ kind: "char", ch: "z", down: false, modifiers: 0b0010 }],
    });
    expect(keyAction("translate", key("Z", "KeyZ", { ctrlKey: true, shiftKey: true }), false)?.press).toEqual([{ kind: "char", ch: "z", down: true, modifiers: 0b0011 }]);
    expect(keyAction("translate", key("c", "KeyC", { metaKey: true }), true)?.press).toEqual([{ kind: "char", ch: "c", down: true, modifiers: 0b1000 }]);
  });

  it("sends other shortcuts by physical key", () => {
    // Russian layout, Ctrl+C types Cyrillic "с": the host gets Ctrl+physical C.
    expect(keyAction("translate", key("с", "KeyC", { ctrlKey: true }), false)?.press).toEqual([{ kind: "scanKey", usage: 0x070006, down: true, modifiers: 0b0010 }]);
  });

  it("sends keys that type nothing as physical keys with their modifiers", () => {
    expect(keyAction("translate", key("Enter", "Enter", { shiftKey: true }), false)).toEqual({
      press: [{ kind: "scanKey", usage: 0x070028, down: true, modifiers: 1 }],
      release: [{ kind: "scanKey", usage: 0x070028, down: false, modifiers: 1 }],
    });
    expect(keyAction("translate", key("F4", "F4", { altKey: true }), false)?.press).toEqual([{ kind: "scanKey", usage: 0x07003d, down: true, modifiers: 0b0100 }]);
    expect(keyAction("translate", key("Unidentified", ""), false)).toBeNull();
  });
});

describe("keyboard state", () => {
  it("releases what was pressed even after Shift changed the key", () => {
    const state = new KeyboardState(false);
    state.down("translate", key("1", "Digit1", { ctrlKey: true }));
    expect(state.up("translate", key("!", "Digit1", { ctrlKey: true, shiftKey: true }))).toEqual([{ kind: "char", ch: "1", down: false, modifiers: 0b0010 }]);
    expect(state.up("translate", key("!", "Digit1"))).toBeNull();
  });

  it("repeats presses but remembers one release", () => {
    const state = new KeyboardState(false);
    expect(state.down("map", key("a", "KeyA"))).toHaveLength(1);
    expect(state.down("map", key("a", "KeyA"))).toHaveLength(1);
    expect(state.releaseAll()).toEqual([{ kind: "scanKey", usage: 0x070004, down: false, modifiers: 0 }]);
    expect(state.releaseAll()).toEqual([]);
  });

  it("releases everything newest first", () => {
    const state = new KeyboardState(false);
    state.down("map", key("Shift", "ShiftLeft", { shiftKey: true }));
    state.down("map", key("A", "KeyA", { shiftKey: true }));
    expect(state.releaseAll().map((ev) => ev.kind === "scanKey" && ev.usage)).toEqual([0x070004, 0x0700e1]);
  });

  it("releases keys a Mac never reports once Cmd goes up", () => {
    const state = new KeyboardState(true);
    state.down("map", key("Meta", "MetaLeft", { metaKey: true }));
    state.down("map", key("c", "KeyC", { metaKey: true }));
    // No keyup for C arrives; Cmd's release carries it.
    expect(state.up("map", key("Meta", "MetaLeft"))?.map((ev) => ev.kind === "scanKey" && ev.usage)).toEqual([0x070006, 0x0700e3]);
    // Translate mode never sent Cmd itself, but still flushes the shortcut.
    state.down("translate", key("c", "KeyC", { metaKey: true }));
    expect(state.up("translate", key("Meta", "MetaLeft"))).toEqual([{ kind: "char", ch: "c", down: false, modifiers: 0b1000 }]);
  });

  it("taps Caps Lock again when a Mac turns it off", () => {
    const state = new KeyboardState(true);
    expect(state.down("map", key("CapsLock", "CapsLock"))).toHaveLength(2);
    expect(state.up("map", key("CapsLock", "CapsLock"))).toHaveLength(2);
  });
});

describe("keyboard mode storage", () => {
  const memory = () => {
    const items = new Map<string, string>();
    return { getItem: (k: string) => items.get(k) ?? null, setItem: (k: string, v: string) => { items.set(k, v); } };
  };

  it("defaults to translate and remembers the choice per device", () => {
    const storage = memory();
    expect(DEFAULT_KEYBOARD_MODE).toBe("translate");
    expect(loadKeyboardMode("111", storage)).toBe("translate");
    saveKeyboardMode("111", "map", storage);
    expect(loadKeyboardMode("111", storage)).toBe("map");
    expect(loadKeyboardMode("222", storage)).toBe("translate");
    storage.setItem("opendesk.keyboard.333", "bogus");
    expect(loadKeyboardMode("333", storage)).toBe("translate");
  });

  it("survives storage that throws", () => {
    const broken = { getItem: () => { throw new Error("denied"); }, setItem: () => { throw new Error("denied"); } };
    expect(loadKeyboardMode("1", broken)).toBe("translate");
    expect(() => saveKeyboardMode("1", "map", broken)).not.toThrow();
  });
});

describe("wire format", () => {
  it("encodes scan keys and typed characters with their own tags", () => {
    expect(Array.from(encodeInputEvent({ kind: "scanKey", usage: 0x070004, down: true, modifiers: 3 }, 0))).toEqual([32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x07, 0, 0x04, 1, 3]);
    expect(Array.from(encodeInputEvent({ kind: "typeChar", ch: "\u{1F600}" }, 0))).toEqual([33, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0xf6, 0x00]);
    expect(() => encodeInputEvent({ kind: "typeChar", ch: "ab" }, 0)).toThrow();
  });
});

describe("platform", () => {
  it("detects a Mac viewer", () => {
    expect(isMacPlatform({ platform: "MacIntel", userAgent: "" })).toBe(true);
    expect(isMacPlatform({ platform: "Win32", userAgent: "Mozilla/5.0 (Windows NT 10.0)" })).toBe(false);
    expect(isMacPlatform(undefined)).toBe(false);
  });
});

describe("attachInput keyboard modes", () => {
  const setup = (mode: () => "map" | "translate" | null) => {
    const listeners = new Map<string, EventListener>();
    const canvas = {
      tabIndex: 0,
      addEventListener: (type: string, listener: EventListener) => listeners.set(type, listener),
      removeEventListener: () => {},
      focus: () => {},
      getBoundingClientRect: () => ({ left: 0, top: 0, width: 100, height: 100 }),
    } as unknown as HTMLCanvasElement;
    const sent: number[][] = [];
    const detach = attachInput(canvas, (bytes) => { sent.push(Array.from(bytes)); return Promise.resolve(); }, { keyboardMode: mode });
    const fire = (type: "keydown" | "keyup", event: KeyInput) => listeners.get(type)!({ preventDefault: () => {}, keyCode: 0, ...event } as unknown as Event);
    const settle = () => new Promise((resolve) => setTimeout(resolve, 0));
    return { sent, detach, fire, settle };
  };

  it("sends scan keys in map mode and releases held keys on detach", async () => {
    const { sent, detach, fire, settle } = setup(() => "map");
    fire("keydown", key("Shift", "ShiftLeft", { shiftKey: true }));
    fire("keydown", key("A", "KeyA", { shiftKey: true }));
    fire("keyup", key("A", "KeyA", { shiftKey: true }));
    detach();
    await settle();
    expect(sent.map((bytes) => [bytes[0], bytes[12], bytes[13]])).toEqual([[32, 0xe1, 1], [32, 0x04, 1], [32, 0x04, 0], [32, 0xe1, 0]]);
  });

  it("types characters in translate mode", async () => {
    const { sent, fire, settle } = setup(() => "translate");
    fire("keydown", altGr("@", "KeyQ"));
    fire("keyup", altGr("@", "KeyQ"));
    await settle();
    // One typed "@" and no release.
    expect(sent.map((bytes) => [bytes[0], ...bytes.slice(9)])).toEqual([[33, 0, 0, 0, 0x40]]);
  });

  it("keeps the original key events for hosts without keyboard modes", async () => {
    const { sent, fire, settle } = setup(() => null);
    fire("keydown", key("a", "KeyA"));
    fire("keyup", key("a", "KeyA"));
    await settle();
    expect(sent.map((bytes) => [bytes[0], bytes[13]])).toEqual([[5, 1], [5, 0]]);
  });

  it("releases a key pressed in one mode after switching modes", async () => {
    let mode: "map" | "translate" | null = "map";
    const { sent, fire, settle } = setup(() => mode);
    fire("keydown", key("a", "KeyA"));
    mode = null;
    fire("keyup", key("a", "KeyA"));
    await settle();
    expect(sent.map((bytes) => [bytes[0], bytes[13]])).toEqual([[32, 1], [32, 0]]);
  });

  it("does not send anything for keys it cannot map", async () => {
    const send = vi.fn(() => Promise.resolve());
    const listeners = new Map<string, EventListener>();
    const canvas = { tabIndex: 0, addEventListener: (type: string, listener: EventListener) => listeners.set(type, listener), removeEventListener: () => {}, focus: () => {} } as unknown as HTMLCanvasElement;
    attachInput(canvas, send, { keyboardMode: () => "map" });
    listeners.get("keydown")!({ preventDefault: () => {}, ...key("x", "") } as unknown as Event);
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(send).not.toHaveBeenCalled();
  });
});
