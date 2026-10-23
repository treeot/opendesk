// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { FakeCore, FakeSocket } from "./lib/fakes";

// jsdom has no canvas or WebCodecs; the session screen's decoder is not under test.
vi.mock("../../viewer/src/lib/decode", () => ({ makeDecoder: () => ({ push: vi.fn(), close: vi.fn() }), supportsHevcDecode: () => Promise.resolve(false) }));

function fill(id: string, password: string) {
  fireEvent.change(screen.getByLabelText("Device ID"), { target: { value: id } });
  fireEvent.change(screen.getByLabelText("Password"), { target: { value: password } });
  fireEvent.click(screen.getByRole("button", { name: "Connect" }));
}

beforeEach(() => {
  FakeSocket.all = [];
  localStorage.clear();
});
afterEach(cleanup);

describe("App", () => {
  it("checks the form before connecting", () => {
    const loadCore = vi.fn();
    render(<App loadCore={loadCore} unsupported={null} />);
    fill("123", "pw");
    expect(screen.getByRole("alert").textContent).toBe("Enter the 9-digit ID.");
    expect(screen.queryByLabelText("Two-factor code")).toBeNull();
    expect(loadCore).not.toHaveBeenCalled();
  });

  it("runs a session and shows why it ended", async () => {
    const core = new FakeCore();
    core.setup = (viewer) => {
      viewer.onReceive = ([first]) => first === 1 ? [{ kind: "authenticated" }] : [{ kind: "ended", text: "session rejected by host: denied" }];
    };
    core.decide = () => ({ retry: false, delayMs: 0, message: "The host declined the connection request." });
    render(<App loadCore={async () => core} openSocket={(url) => new FakeSocket(url)} unsupported={null} />);
    await act(async () => fill("123-456-789", "secret"));
    expect(core.viewers[0].password).toBe("secret");
    expect(core.viewers[0].authCode).toBe("");
    const socket = FakeSocket.all[0];
    act(() => socket.open());
    act(() => socket.message([1]));
    expect(screen.getByText(/to accept/).textContent).toContain("123 456 789");
    act(() => socket.message([2]));
    expect(screen.getByRole("alert").textContent).toBe("The host declined the connection request.");
  });

  it("prompts for a 2FA code and reconnects with it", async () => {
    const core = new FakeCore();
    core.setup = (viewer) => {
      viewer.onReceive = () => [{ kind: "ended", text: "session rejected by host: 2fa_required" }];
    };
    render(<App loadCore={async () => core} openSocket={(url) => new FakeSocket(url)} unsupported={null} />);
    await act(async () => fill("123456789", "secret"));
    act(() => FakeSocket.all[0].open());
    act(() => FakeSocket.all[0].message([1]));
    const dialog = await screen.findByRole("dialog", { name: "Enter your 2FA code" });
    expect(dialog.textContent).toContain("Enter the current 6-digit code");
    fireEvent.change(screen.getByLabelText("Two-factor code"), { target: { value: "123456" } });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Submit" })));
    expect(core.viewers[1].authCode).toBe("123456");
    expect(screen.queryByRole("dialog", { name: "Enter your 2FA code" })).toBeNull();
  });

  it("disconnects from the toolbar", async () => {
    const core = new FakeCore();
    render(<App loadCore={async () => core} openSocket={(url) => new FakeSocket(url)} unsupported={null} />);
    await act(async () => fill("123456789", "pw"));
    act(() => FakeSocket.all[0].open());
    fireEvent.click(screen.getByRole("button", { name: "Disconnect" }));
    expect(FakeSocket.all[0].sent.at(-1)).toEqual([0x01, 0xbb]);
    expect(screen.getByRole("button", { name: "Connect" })).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("blocks browsers without WebCodecs", () => {
    render(<App loadCore={vi.fn()} unsupported="no WebCodecs here" />);
    expect(screen.getByRole("alert").textContent).toBe("no WebCodecs here");
    expect((screen.getByRole("button", { name: "Connect" }) as HTMLButtonElement).disabled).toBe(true);
  });
});
