import { describe, expect, it } from "vitest";
import { actualSize, loadScaleMode, saveScaleMode } from "./scale";

function memoryStorage() {
  const items = new Map<string, string>();
  return { getItem: (key: string) => items.get(key) ?? null, setItem: (key: string, value: string) => { items.set(key, value); } };
}

describe("scaling modes", () => {
  it("remembers the mode per device and defaults to fit", () => {
    const storage = memoryStorage();
    expect(loadScaleMode("111111111", storage)).toBe("fit");
    saveScaleMode("111111111", "actual", storage);
    saveScaleMode("222222222", "stretch", storage);
    expect(loadScaleMode("111111111", storage)).toBe("actual");
    expect(loadScaleMode("222222222", storage)).toBe("stretch");
    expect(loadScaleMode("333333333", storage)).toBe("fit");
  });

  it("ignores unknown saved values and missing storage", () => {
    const storage = memoryStorage();
    storage.setItem("opendesk.scale.111111111", "zoom");
    expect(loadScaleMode("111111111", storage)).toBe("fit");
    expect(loadScaleMode("111111111", undefined)).toBe("fit");
    const broken = { getItem: () => { throw new Error("denied"); }, setItem: () => { throw new Error("denied"); } };
    expect(loadScaleMode("111111111", broken)).toBe("fit");
    expect(() => saveScaleMode("111111111", "actual", broken)).not.toThrow();
  });

  it("sizes actual mode to one remote pixel per screen pixel", () => {
    expect(actualSize(2560, 1440, 2)).toEqual({ width: 1280, height: 720 });
    expect(actualSize(1920, 1080, 1)).toEqual({ width: 1920, height: 1080 });
    expect(actualSize(1920, 1080, 0)).toEqual({ width: 1920, height: 1080 });
  });
});
