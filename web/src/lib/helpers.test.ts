import { describe, expect, it } from "vitest";
import { SPECIAL_KEYS, specialKeyEvents } from "./keys";
import { baseName, joinPath, parentPath, validName } from "./remotePath";
import { applySignal, PendingPulls, PULL_WAIT_MS, type Transfer } from "./transfers";

describe("remote paths", () => {
  it("joins with the path's own separator", () => {
    expect(joinPath("/home/me", "docs")).toBe("/home/me/docs");
    expect(joinPath("/", "etc")).toBe("/etc");
    expect(joinPath("C:\\Users", "me")).toBe("C:\\Users\\me");
    expect(joinPath("C:\\", "Users")).toBe("C:\\Users");
    expect(joinPath("", "C:\\")).toBe("C:\\");
  });

  it("goes up to the root, then to the starting places", () => {
    expect(parentPath("/home/me/docs")).toBe("/home/me");
    expect(parentPath("/home")).toBe("/");
    expect(parentPath("/")).toBe("");
    expect(parentPath("C:\\Users\\me")).toBe("C:\\Users");
    expect(parentPath("C:\\Users")).toBe("C:\\");
    expect(parentPath("C:\\")).toBe("");
    expect(parentPath("")).toBeNull();
  });

  it("names the last component and checks new names", () => {
    expect(baseName("/home/me/a.txt")).toBe("a.txt");
    expect(baseName("C:\\x\\y.doc")).toBe("y.doc");
    expect(validName("Reports 2026")).toBe(true);
    for (const bad of ["", "  ", ".", "..", "a/b", "a\\b", "c:d"]) expect(validName(bad)).toBe(false);
  });
});

describe("special keys", () => {
  it("encodes a press and a release with the combo's modifiers", () => {
    const altTab = SPECIAL_KEYS.find((key) => key.id === "alt-tab")!;
    const [down, up] = specialKeyEvents(altTab, 5);
    // Kind 4 (key), timestamp, code 9, down flag, modifiers (Alt = 4).
    expect([...down]).toEqual([4, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 9, 1, 4]);
    expect([...up]).toEqual([4, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 9, 0, 4]);
    const meta = SPECIAL_KEYS.find((key) => key.id === "meta")!;
    expect(specialKeyEvents(meta, 0)[0][12]).toBe(91);
  });
});

describe("transfers", () => {
  it("tracks an offer through progress to done", () => {
    let list: Transfer[] = [];
    list = applySignal(list, { kind: "file-offer", id: 1, name: "a.bin", size: 10 });
    expect(list[0]).toMatchObject({ state: "pending", sending: false, total: 10 });
    list = applySignal(list, { kind: "file-progress", id: 1, transferred: 4, total: 10 });
    expect(list[0]).toMatchObject({ state: "active", transferred: 4 });
    list = applySignal(list, { kind: "file-received", id: 1, name: "a.bin" });
    expect(list[0]).toMatchObject({ state: "done", transferred: 10 });
    list = applySignal(list, { kind: "file-error", id: 2, message: "unknown" });
    expect(list).toHaveLength(1);
  });

  it("accepts a requested file once and only while fresh", () => {
    let now = 0;
    const pulls = new PendingPulls(() => now);
    pulls.add("a.txt");
    expect(pulls.take("b.txt")).toBe(false);
    expect(pulls.take("a.txt")).toBe(true);
    expect(pulls.take("a.txt")).toBe(false);
    pulls.add("c.txt");
    now += PULL_WAIT_MS;
    expect(pulls.take("c.txt")).toBe(false);
  });
});
