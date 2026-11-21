import { describe, expect, it } from "vitest";
import { formatWait, restartConfirmText } from "./restart";

describe("remote restart", () => {
  it("explains when the viewer can't get back in on its own", () => {
    expect(restartConfirmText(false)).toContain("waits up to 5 minutes");
    expect(restartConfirmText(false)).toContain("unattended access on");
    expect(restartConfirmText(true)).toContain("one-time password");
    expect(restartConfirmText(true)).toContain("can't reconnect on its own");
  });

  it("formats the wait as minutes and seconds", () => {
    expect(formatWait(0)).toBe("0:00");
    expect(formatWait(9_999)).toBe("0:09");
    expect(formatWait(61_000)).toBe("1:01");
    expect(formatWait(-5)).toBe("0:00");
  });
});
