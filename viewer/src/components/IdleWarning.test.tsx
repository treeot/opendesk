// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

const { onIdleWarning, stayConnected } = vi.hoisted(() => ({
  onIdleWarning: vi.fn<(handler: (payload: { seconds: number }) => void) => Promise<() => void>>(() => Promise.resolve(vi.fn())),
  stayConnected: vi.fn(() => Promise.resolve()),
}));
vi.mock("../lib/api", () => ({ onIdleWarning, stayConnected }));

import IdleWarning, { formatCountdown } from "./IdleWarning";

const warn = async (seconds: number) => {
  const handler = onIdleWarning.mock.calls[0][0];
  await act(async () => handler({ seconds }));
};

describe("IdleWarning", () => {
  afterEach(() => { cleanup(); vi.clearAllMocks(); vi.useRealTimers(); });

  it("formats the countdown", () => {
    expect(formatCountdown(60)).toBe("1:00");
    expect(formatCountdown(5)).toBe("0:05");
  });

  it("shows nothing until the host warns", () => {
    render(<IdleWarning />);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("counts down and answers the host when asked to stay", async () => {
    vi.useFakeTimers({ toFake: ["setInterval", "clearInterval", "Date"] });
    render(<IdleWarning />);
    await warn(60);
    expect(screen.getByRole("alert").textContent).toContain("1:00");
    await act(async () => { vi.advanceTimersByTime(5000); });
    expect(screen.getByRole("alert").textContent).toContain("0:55");
    fireEvent.pointerDown(screen.getByRole("button", { name: "Stay connected" }));
    fireEvent.click(screen.getByRole("button", { name: "Stay connected" }));
    expect(stayConnected).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("goes away on input to the remote screen but not on other clicks", async () => {
    render(<div><canvas aria-label="Remote screen" /><button type="button">Toolbar</button><IdleWarning /></div>);
    await warn(60);
    fireEvent.pointerDown(screen.getByRole("button", { name: "Toolbar" }));
    expect(screen.getByRole("alert")).toBeTruthy();
    fireEvent.keyDown(screen.getByLabelText("Remote screen"), { key: "a" });
    expect(screen.queryByRole("alert")).toBeNull();
    expect(stayConnected).not.toHaveBeenCalled();
  });
});
