// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { check, relaunch } = vi.hoisted(() => ({ check: vi.fn(), relaunch: vi.fn() }));
vi.mock("@tauri-apps/plugin-updater", () => ({ check }));
vi.mock("@tauri-apps/plugin-process", () => ({ relaunch }));

import UpdateBanner, { CHECK_INTERVAL_MS, CHECK_NOW_EVENT, CHECK_TIMEOUT_MS, DOWNLOAD_STALL_MS, DOWNLOAD_TIMEOUT_MS } from "./UpdateBanner";
import { NotifyContext } from "../lib/notify";

type DownloadEvent = { event: "Started"; data: { contentLength?: number } } | { event: "Progress"; data: { chunkLength: number } } | { event: "Finished" };

function fakeUpdate(version = "0.1.0-beta.5") {
  return {
    version,
    download: vi.fn(async (onEvent?: (event: DownloadEvent) => void) => {
      onEvent?.({ event: "Started", data: { contentLength: 100 } });
      onEvent?.({ event: "Progress", data: { chunkLength: 100 } });
      onEvent?.({ event: "Finished" });
    }),
    install: vi.fn().mockResolvedValue(undefined),
    close: vi.fn().mockResolvedValue(undefined),
  };
}

describe("UpdateBanner", () => {
  beforeEach(() => {
    check.mockReset();
    relaunch.mockReset().mockResolvedValue(undefined);
    vi.spyOn(console, "warn").mockImplementation(() => {});
  });
  afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); });

  it("shows nothing when there is no update", async () => {
    check.mockResolvedValue(null);
    render(<UpdateBanner enabled visible />);
    await waitFor(() => expect(check).toHaveBeenCalledTimes(1));
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("does not check when disabled", () => {
    render(<UpdateBanner enabled={false} visible />);
    expect(check).not.toHaveBeenCalled();
  });

  it("stays quiet when the check fails", async () => {
    check.mockRejectedValue(new Error("offline"));
    render(<UpdateBanner enabled visible />);
    await waitFor(() => expect(console.warn).toHaveBeenCalled());
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("offers an available update and installs it", async () => {
    const update = fakeUpdate();
    check.mockResolvedValue(update);
    render(<UpdateBanner enabled visible />);
    expect(await screen.findByText("Update available: v0.1.0-beta.5")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Install and restart" }));
    await waitFor(() => expect(relaunch).toHaveBeenCalledTimes(1));
    expect(update.download).toHaveBeenCalledTimes(1);
    expect(update.install).toHaveBeenCalledTimes(1);
  });

  it("hides the banner outside the home screen", async () => {
    check.mockResolvedValue(fakeUpdate());
    const { rerender } = render(<UpdateBanner enabled visible={false} />);
    await waitFor(() => expect(check).toHaveBeenCalled());
    expect(screen.queryByText(/Update available/)).toBeNull();
    rerender(<UpdateBanner enabled visible />);
    expect(await screen.findByText(/Update available/)).toBeTruthy();
  });

  it("waits to restart if a session starts during the download", async () => {
    const update = fakeUpdate();
    let finish: () => void = () => {};
    update.download.mockImplementation(() => new Promise<void>((resolve) => { finish = resolve; }));
    check.mockResolvedValue(update);
    const { rerender } = render(<UpdateBanner enabled visible />);
    fireEvent.click(await screen.findByRole("button", { name: "Install and restart" }));
    expect(await screen.findByText(/Downloading/)).toBeTruthy();
    rerender(<UpdateBanner enabled visible={false} />);
    await act(async () => { finish(); });
    expect(update.install).not.toHaveBeenCalled();
    expect(relaunch).not.toHaveBeenCalled();
    rerender(<UpdateBanner enabled visible />);
    fireEvent.click(await screen.findByRole("button", { name: "Restart to update" }));
    await waitFor(() => expect(relaunch).toHaveBeenCalledTimes(1));
    expect(update.install).toHaveBeenCalledTimes(1);
  });

  it("reports a failed install without a dialog and allows a retry", async () => {
    const update = fakeUpdate();
    update.download.mockRejectedValueOnce(new Error("signature mismatch"));
    check.mockResolvedValue(update);
    render(<UpdateBanner enabled visible />);
    fireEvent.click(await screen.findByRole("button", { name: "Install and restart" }));
    expect(await screen.findByText("The update didn't install. Try again later.")).toBeTruthy();
    expect(console.warn).toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await waitFor(() => expect(relaunch).toHaveBeenCalledTimes(1));
  });

  it("puts time limits on the check and the download", async () => {
    const update = fakeUpdate();
    check.mockResolvedValue(update);
    render(<UpdateBanner enabled visible />);
    fireEvent.click(await screen.findByRole("button", { name: "Install and restart" }));
    await waitFor(() => expect(relaunch).toHaveBeenCalledTimes(1));
    expect(check).toHaveBeenCalledWith({ timeout: CHECK_TIMEOUT_MS });
    expect(update.download).toHaveBeenCalledWith(expect.any(Function), { timeout: DOWNLOAD_TIMEOUT_MS });
  });

  it("can cancel a download and ignores it if it finishes later", async () => {
    const update = fakeUpdate();
    let finish: () => void = () => {};
    update.download.mockImplementation(() => new Promise<void>((resolve) => { finish = resolve; }));
    check.mockResolvedValue(update);
    render(<UpdateBanner enabled visible />);
    fireEvent.click(await screen.findByRole("button", { name: "Install and restart" }));
    expect(await screen.findByText(/Downloading/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.getByText("Update available: v0.1.0-beta.5")).toBeTruthy();
    await act(async () => { finish(); });
    expect(update.install).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Install and restart" })).toBeTruthy();
  });

  it("gives up on a download that stops making progress", async () => {
    vi.useFakeTimers();
    const update = fakeUpdate();
    update.download.mockImplementation(async (onEvent?: (event: DownloadEvent) => void) => {
      onEvent?.({ event: "Started", data: { contentLength: 100 } });
      await new Promise(() => {});
    });
    check.mockResolvedValue(update);
    render(<UpdateBanner enabled visible />);
    await act(async () => { await Promise.resolve(); });
    fireEvent.click(screen.getByRole("button", { name: "Install and restart" }));
    await act(async () => { await vi.advanceTimersByTimeAsync(DOWNLOAD_STALL_MS - 1); });
    expect(screen.getByText(/Downloading/)).toBeTruthy();
    await act(async () => { await vi.advanceTimersByTimeAsync(1); });
    expect(screen.getByText("The update didn't install. Try again later.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Try again" })).toBeTruthy();
  });

  it("checks on demand and reports when up to date, even when disabled", async () => {
    check.mockResolvedValue(null);
    const notify = vi.fn();
    render(<NotifyContext.Provider value={notify}><UpdateBanner enabled={false} visible /></NotifyContext.Provider>);
    expect(check).not.toHaveBeenCalled();
    act(() => { window.dispatchEvent(new Event(CHECK_NOW_EVENT)); });
    await waitFor(() => expect(check).toHaveBeenCalledTimes(1));
    expect(notify).toHaveBeenCalledWith("OpenDesk is up to date.", "info");
  });

  it("shows a banner found by an on-demand check", async () => {
    check.mockResolvedValue(fakeUpdate("0.1.0-beta.7"));
    render(<UpdateBanner enabled visible />);
    await waitFor(() => expect(check).toHaveBeenCalled());
    check.mockResolvedValue(fakeUpdate("0.1.0-beta.7"));
    act(() => { window.dispatchEvent(new Event(CHECK_NOW_EVENT)); });
    expect(await screen.findByText("Update available: v0.1.0-beta.7")).toBeTruthy();
  });

  it("remembers a dismissed version and checks again later", async () => {
    vi.useFakeTimers();
    const first = fakeUpdate();
    check.mockResolvedValue(first);
    render(<UpdateBanner enabled visible />);
    await act(async () => { await Promise.resolve(); });
    fireEvent.click(screen.getByRole("button", { name: "Dismiss update" }));
    expect(screen.queryByText(/Update available/)).toBeNull();
    expect(first.close).toHaveBeenCalled();

    const same = fakeUpdate();
    check.mockResolvedValue(same);
    await act(async () => { await vi.advanceTimersByTimeAsync(CHECK_INTERVAL_MS); });
    expect(screen.queryByText(/Update available/)).toBeNull();
    expect(same.close).toHaveBeenCalled();

    check.mockResolvedValue(fakeUpdate("0.1.0-beta.6"));
    await act(async () => { await vi.advanceTimersByTimeAsync(CHECK_INTERVAL_MS); });
    expect(screen.getByText("Update available: v0.1.0-beta.6")).toBeTruthy();
  });
});
