// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { createPointerLock, DeltaAccumulator, isReleaseChord } from "./pointerLock";

describe("DeltaAccumulator", () => {
  it("carries fractional movement until it adds up to a whole unit", () => {
    const deltas = new DeltaAccumulator();
    expect(deltas.take(0.4, -0.4)).toBeNull();
    expect(deltas.take(0.4, -0.4)).toBeNull();
    expect(deltas.take(0.4, -0.4)).toEqual({ dx: 1, dy: -1 });
    expect(deltas.take(3, Number.NaN)).toEqual({ dx: 3, dy: 0 });
  });
});

describe("isReleaseChord", () => {
  const key = (key: string, mods: Partial<KeyboardEvent>) => ({ key, ctrlKey: false, altKey: false, shiftKey: false, ...mods }) as KeyboardEvent;

  it("fires when the last of Ctrl, Alt and Shift goes down", () => {
    expect(isReleaseChord(key("Shift", { ctrlKey: true, altKey: true, shiftKey: true }))).toBe(true);
    expect(isReleaseChord(key("Control", { ctrlKey: true, altKey: true, shiftKey: true }))).toBe(true);
  });

  it("ignores partial chords and ordinary keys", () => {
    expect(isReleaseChord(key("Shift", { ctrlKey: true, shiftKey: true }))).toBe(false);
    expect(isReleaseChord(key("A", { ctrlKey: true, altKey: true, shiftKey: true }))).toBe(false);
  });
});

describe("createPointerLock", () => {
  const setup = () => {
    const element = document.createElement("canvas");
    document.body.appendChild(element);
    let lockedElement: Element | null = null;
    Object.defineProperty(document, "pointerLockElement", { configurable: true, get: () => lockedElement });
    const exitPointerLock = vi.fn(() => { lockedElement = null; document.dispatchEvent(new Event("pointerlockchange")); });
    Object.defineProperty(document, "exitPointerLock", { configurable: true, value: exitPointerLock });
    const requestPointerLock = vi.fn((options?: { unadjustedMovement?: boolean }) => {
      if (options?.unadjustedMovement) return Promise.reject(new Error("NotSupportedError"));
      lockedElement = element;
      document.dispatchEvent(new Event("pointerlockchange"));
      return Promise.resolve();
    });
    Object.defineProperty(element, "requestPointerLock", { configurable: true, value: requestPointerLock });
    const changes: boolean[] = [];
    const lock = createPointerLock(element, (locked) => changes.push(locked));
    return { lock, changes, requestPointerLock, exitPointerLock };
  };

  it("falls back to plain pointer lock and releases on the chord", async () => {
    const { lock, changes, requestPointerLock } = setup();
    await lock.request();
    expect(requestPointerLock).toHaveBeenCalledTimes(2);
    expect(lock.locked()).toBe(true);
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Alt", ctrlKey: true, altKey: true, shiftKey: true }));
    expect(lock.locked()).toBe(false);
    expect(changes).toEqual([true, false]);
    lock.dispose();
  });

  it("releases when the window loses focus and on dispose", async () => {
    const { lock, changes, exitPointerLock } = setup();
    await lock.request();
    window.dispatchEvent(new Event("blur"));
    expect(changes).toEqual([true, false]);
    await lock.request();
    lock.dispose();
    expect(exitPointerLock).toHaveBeenCalledTimes(2);
    expect(lock.locked()).toBe(false);
  });
});
