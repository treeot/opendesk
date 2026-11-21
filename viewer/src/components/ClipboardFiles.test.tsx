// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { NotifyContext } from "../lib/notify";

const { handlers, onRemoteClipboardFiles, remoteClipboardFiles, pasteFromRemote, pickFolder } = vi.hoisted(() => {
  const handlers = new Map<string, (payload: unknown) => void>();
  return {
    handlers,
    onRemoteClipboardFiles: vi.fn((handler: (payload: unknown) => void) => { handlers.set("files", handler); return Promise.resolve(() => {}); }),
    remoteClipboardFiles: vi.fn(() => Promise.resolve({ files: [] as { name: string; size: number; is_dir: boolean }[] })),
    pasteFromRemote: vi.fn((_dir: string) => Promise.resolve(["/Users/me/Downloads/a.txt", "/Users/me/Downloads/pics"])),
    pickFolder: vi.fn(() => Promise.resolve("/Users/me/Desktop")),
  };
});
vi.mock("../lib/api", () => ({ onRemoteClipboardFiles, remoteClipboardFiles, pasteFromRemote, pickFolder }));

import ClipboardFiles, { describeFiles, isPasteFilesShortcut } from "./ClipboardFiles";

const files = [{ name: "a.txt", size: 2048, is_dir: false }, { name: "pics", size: 0, is_dir: true }];
const announce = (payload: unknown) => act(() => { handlers.get("files")?.(payload); });

describe("ClipboardFiles", () => {
  afterEach(() => { cleanup(); vi.clearAllMocks(); handlers.clear(); });

  it("stays hidden until the remote copies files, then pastes to Downloads", async () => {
    const notify = vi.fn();
    render(<NotifyContext.Provider value={notify}><ClipboardFiles /></NotifyContext.Provider>);
    await waitFor(() => expect(handlers.has("files")).toBe(true));
    expect(screen.queryByRole("region")).toBeNull();
    announce({ files });
    expect(screen.getByText("2 items")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Paste to Downloads" }));
    await waitFor(() => expect(notify).toHaveBeenCalledWith("Pasted 2 items into Downloads. They are on your clipboard too.", "info"));
    expect(pasteFromRemote).toHaveBeenCalledWith("");
    announce({ files: [] });
    expect(screen.queryByText("2 items")).toBeNull();
  });

  it("pastes into a chosen folder and reports failures", async () => {
    const notify = vi.fn();
    pasteFromRemote.mockRejectedValueOnce("That item is no longer on the other computer's clipboard.");
    render(<NotifyContext.Provider value={notify}><ClipboardFiles /></NotifyContext.Provider>);
    await waitFor(() => expect(handlers.has("files")).toBe(true));
    announce({ files: [files[0]] });
    expect(screen.getByText("a.txt")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Paste to…" }));
    await waitFor(() => expect(notify).toHaveBeenCalledWith("That item is no longer on the other computer's clipboard."));
    expect(pasteFromRemote).toHaveBeenCalledWith("/Users/me/Desktop");
  });

  it("pastes on the shortcut without passing it on", async () => {
    render(<ClipboardFiles />);
    await waitFor(() => expect(handlers.has("files")).toBe(true));
    announce({ files });
    const passedOn = vi.fn();
    window.addEventListener("keydown", passedOn);
    fireEvent.keyDown(window, { key: "V", code: "KeyV", ctrlKey: true, altKey: true, shiftKey: true });
    window.removeEventListener("keydown", passedOn);
    await waitFor(() => expect(pasteFromRemote).toHaveBeenCalledWith(""));
    expect(passedOn).not.toHaveBeenCalled();
  });

  it("recognises only the full chord", () => {
    const key = { key: "v", code: "KeyV", ctrlKey: false, metaKey: true, altKey: true, shiftKey: true };
    expect(isPasteFilesShortcut(key)).toBe(true);
    expect(isPasteFilesShortcut({ ...key, altKey: false })).toBe(false);
    expect(isPasteFilesShortcut({ ...key, metaKey: false })).toBe(false);
    expect(describeFiles(files)).toBe("2 items");
  });
});
