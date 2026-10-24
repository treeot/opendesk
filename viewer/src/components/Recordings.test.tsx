// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

const { listRecordings, deleteRecording, getConfig, setRecordingsMaxBytes, openPath, revealItemInDir } = vi.hoisted(() => ({
  listRecordings: vi.fn(), deleteRecording: vi.fn(),
  getConfig: vi.fn(() => Promise.resolve({ relay_url: "", relay_urls: [], recent_ids: [], clipboard_sync: false, allow_virtual_displays: true, recordings_max_bytes: 5 * 1024 * 1024 * 1024 })),
  setRecordingsMaxBytes: vi.fn(() => Promise.resolve()),
  openPath: vi.fn(() => Promise.resolve()), revealItemInDir: vi.fn(() => Promise.resolve()),
}));
vi.mock("../lib/api", () => ({ listRecordings, deleteRecording, getConfig, setRecordingsMaxBytes }));
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

  it("changes the storage cap", async () => {
    listRecordings.mockResolvedValue([]);
    render(<Recordings />);
    const select = await screen.findByLabelText("Recordings storage limit");
    fireEvent.change(select, { target: { value: String(2 * 1024 * 1024 * 1024) } });
    expect(setRecordingsMaxBytes).toHaveBeenCalledWith(2 * 1024 * 1024 * 1024);
  });
});
