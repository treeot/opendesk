// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

const { listRecordings, deleteRecording, getConfig, setRecordingsMaxBytes, setRecordIncomingSessions, openPath, revealItemInDir } = vi.hoisted(() => ({
  listRecordings: vi.fn(), deleteRecording: vi.fn(),
  setRecordIncomingSessions: vi.fn(() => Promise.resolve()),
  getConfig: vi.fn(() => Promise.resolve({ relay_url: "", relay_urls: [], recent_ids: [], clipboard_sync: false, allow_virtual_displays: true, recordings_max_bytes: 5 * 1024 * 1024 * 1024 })),
  setRecordingsMaxBytes: vi.fn(() => Promise.resolve()),
  openPath: vi.fn(() => Promise.resolve()), revealItemInDir: vi.fn(() => Promise.resolve()),
}));
vi.mock("../lib/api", () => ({ listRecordings, deleteRecording, getConfig, setRecordingsMaxBytes, setRecordIncomingSessions }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openPath, revealItemInDir }));

import Recordings from "./Recordings";

const entry = (name: string) => ({ name, path: `/Users/me/Downloads/${name}`, size: 1024 * 1024, modified: 1_700_000_000_000 });

describe("Recordings", () => {
  afterEach(() => { cleanup(); vi.clearAllMocks(); });

  it("shows an empty state when there are none", async () => {
    listRecordings.mockResolvedValue([]);
    render(<Recordings />);
    expect(await screen.findByText(/No recordings yet/)).toBeTruthy();
  });

  it("lists recordings and plays, reveals, and deletes one", async () => {
    listRecordings.mockResolvedValue([entry("opendesk-session-a.webm")]);
    deleteRecording.mockResolvedValue(undefined);
    render(<Recordings />);
    expect(await screen.findByText("opendesk-session-a.webm")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Play" }));
    expect(openPath).toHaveBeenCalledWith("/Users/me/Downloads/opendesk-session-a.webm");
    fireEvent.click(screen.getByRole("button", { name: "Reveal" }));
    expect(revealItemInDir).toHaveBeenCalledWith("/Users/me/Downloads/opendesk-session-a.webm");

    fireEvent.click(screen.getByRole("button", { name: "Delete opendesk-session-a.webm" }));
    expect(deleteRecording).toHaveBeenCalledWith("/Users/me/Downloads/opendesk-session-a.webm");
    await waitFor(() => expect(screen.queryByText("opendesk-session-a.webm")).toBeNull());
  });

  it("shows host recordings and turns recording incoming sessions on", async () => {
    listRecordings.mockImplementation((source: string) => Promise.resolve(source === "host" ? [entry("opendesk-host-2026-11-21_10-00-00-UTC-123456789.mkv")] : []));
    render(<Recordings />);
    expect(await screen.findByText(/No recordings yet/)).toBeTruthy();
    expect(screen.queryByLabelText("Record incoming sessions")).toBeNull();
    fireEvent.click(screen.getByRole("tab", { name: "Host recordings" }));
    expect(await screen.findByText("opendesk-host-2026-11-21_10-00-00-UTC-123456789.mkv")).toBeTruthy();
    expect(listRecordings).toHaveBeenLastCalledWith("host");
    const toggle = screen.getByLabelText("Record incoming sessions") as HTMLInputElement;
    await waitFor(() => expect(toggle.disabled).toBe(false));
    expect(toggle.checked).toBe(false);
    fireEvent.click(toggle);
    expect(setRecordIncomingSessions).toHaveBeenCalledWith(true);
    expect(toggle.checked).toBe(true);
  });

  it("changes the storage cap", async () => {
    listRecordings.mockResolvedValue([]);
    render(<Recordings />);
    const select = await screen.findByLabelText("Recordings storage limit");
    fireEvent.change(select, { target: { value: String(2 * 1024 * 1024 * 1024) } });
    expect(setRecordingsMaxBytes).toHaveBeenCalledWith(2 * 1024 * 1024 * 1024);
  });
});
