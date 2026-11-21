// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

const { onFileOffer, onFileProgress, onFileDone, onFileError, onFileDrop, respondFile, sendFile, pickFile, pickFolder, cancelTransfer, browseLocal, browseRemote, onRemoteListing, pullRemote } = vi.hoisted(() => ({
  onFileOffer: vi.fn(), onFileProgress: vi.fn(), onFileDone: vi.fn(), onFileError: vi.fn(), onFileDrop: vi.fn(),
  respondFile: vi.fn(() => Promise.resolve()), sendFile: vi.fn(() => Promise.resolve(1)), pickFile: vi.fn(), pickFolder: vi.fn(), cancelTransfer: vi.fn(() => Promise.resolve()),
  browseLocal: vi.fn(() => Promise.resolve({ path: "/home", entries: [] })), browseRemote: vi.fn(() => Promise.resolve()), onRemoteListing: vi.fn(() => Promise.resolve(() => {})), pullRemote: vi.fn(() => Promise.resolve()),
}));

vi.mock("../lib/api", () => ({ onFileOffer, onFileProgress, onFileDone, onFileError, onFileDrop, respondFile, sendFile, pickFile, pickFolder, cancelTransfer, browseLocal, browseRemote, onRemoteListing, pullRemote }));

import FileTransfer from "./FileTransfer";

type Handler<T> = (payload: T) => void;
function capture<T>(mock: ReturnType<typeof vi.fn>) {
  const ref: { current?: Handler<T> } = {};
  mock.mockImplementation((handler: Handler<T>) => { ref.current = handler; return Promise.resolve(vi.fn()); });
  return ref;
}

describe("FileTransfer", () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("opens for an incoming offer, accepts it, and shows where it was saved", async () => {
    const offer = capture<{ id: number; name: string; size: number; from: string }>(onFileOffer);
    const progress = capture<{ id: number; transferred: number; total: number; direction: string }>(onFileProgress);
    const done = capture<{ id: number; path?: string }>(onFileDone);
    capture(onFileError);
    capture(onFileDrop);
    const onOpenChange = vi.fn();
    const view = render(<FileTransfer open={false} onOpenChange={onOpenChange} />);
    expect(view.getByLabelText("File transfer").hidden).toBe(true);

    await waitFor(() => expect(offer.current).toBeTypeOf("function"));
    offer.current!({ id: 42, name: "report.pdf", size: 2048, from: "peer-7" });
    expect(onOpenChange).toHaveBeenCalledWith(true);
    view.rerender(<FileTransfer open onOpenChange={onOpenChange} />);
    expect(await view.findByRole("dialog", { name: "Incoming file report.pdf" })).toBeTruthy();
    fireEvent.click(view.getByRole("button", { name: "Accept" }));
    await waitFor(() => expect(respondFile).toHaveBeenCalledWith(42, true));

    progress.current!({ id: 42, transferred: 1024, total: 2048, direction: "receive" });
    const bar = view.getByRole("progressbar", { name: "report.pdf progress" }) as HTMLProgressElement;
    await waitFor(() => expect(bar.value).toBe(1024));
    expect(onOpenChange).toHaveBeenCalledTimes(1);

    done.current!({ id: 42, path: "/Users/me/Downloads/report.pdf" });
    expect(await view.findByText("Saved to /Users/me/Downloads/report.pdf")).toBeTruthy();
  });

  it("sends a picked file and shows backend errors as text", async () => {
    [onFileOffer, onFileProgress, onFileDone, onFileError, onFileDrop].forEach((mock) => capture(mock));
    pickFile.mockResolvedValueOnce("/tmp/a.txt").mockResolvedValueOnce("/tmp/b.txt");
    sendFile.mockResolvedValueOnce(5).mockRejectedValueOnce("no active viewer session");
    const view = render(<FileTransfer open onOpenChange={vi.fn()} />);

    fireEvent.click(view.getByRole("button", { name: "Send file…" }));
    await waitFor(() => expect(sendFile).toHaveBeenCalledWith("/tmp/a.txt"));
    expect(await view.findByText("a.txt")).toBeTruthy();

    fireEvent.click(view.getByRole("button", { name: "Send file…" }));
    expect((await view.findByRole("alert")).textContent).toBe("no active viewer session");
  });

  it("sends files dropped on the window and opens the panel", async () => {
    [onFileOffer, onFileProgress, onFileDone, onFileError].forEach((mock) => capture(mock));
    const drop = capture<{ type: "over" | "drop" | "leave"; paths: string[] }>(onFileDrop);
    const onOpenChange = vi.fn();
    render(<FileTransfer open={false} onOpenChange={onOpenChange} />);
    await waitFor(() => expect(drop.current).toBeTypeOf("function"));
    drop.current!({ type: "drop", paths: ["/tmp/photo.png"] });
    expect(onOpenChange).toHaveBeenCalledWith(true);
    await waitFor(() => expect(sendFile).toHaveBeenCalledWith("/tmp/photo.png"));
  });

  it("sends a picked folder and shows file counts with a cancel button", async () => {
    [onFileOffer, onFileDone, onFileError, onFileDrop].forEach((mock) => capture(mock));
    const progress = capture<{ id: number; transferred: number; total: number; direction: string; files_done?: number; files_total?: number }>(onFileProgress);
    pickFolder.mockResolvedValueOnce("/tmp/photos");
    sendFile.mockResolvedValueOnce(9);
    const view = render(<FileTransfer open onOpenChange={vi.fn()} />);

    fireEvent.click(view.getByRole("button", { name: "Send folder…" }));
    await waitFor(() => expect(sendFile).toHaveBeenCalledWith("/tmp/photos"));
    expect(await view.findByText("photos")).toBeTruthy();
    progress.current!({ id: 9, transferred: 1024, total: 4096, direction: "send", files_done: 1, files_total: 3 });
    expect(await view.findByText("1 of 3 files · 1 KB / 4 KB")).toBeTruthy();
    fireEvent.click(view.getByRole("button", { name: "Cancel photos" }));
    await waitFor(() => expect(cancelTransfer).toHaveBeenCalledWith(9));
  });

  it("shows an incoming folder with its file count", async () => {
    const offer = capture<{ id: number; name: string; size: number; from: string; files?: number }>(onFileOffer);
    [onFileProgress, onFileDone, onFileError, onFileDrop].forEach((mock) => capture(mock));
    const view = render(<FileTransfer open onOpenChange={vi.fn()} />);
    await waitFor(() => expect(offer.current).toBeTypeOf("function"));
    offer.current!({ id: 5, name: "music", size: 2048, from: "peer-7", files: 2 });
    const dialog = await view.findByRole("dialog", { name: "Incoming folder music" });
    expect(dialog.textContent).toContain("INCOMING FOLDER");
    expect(dialog.textContent).toContain("2 files · 2 KB · from peer-7");
    expect(view.queryByRole("button", { name: "Cancel music" })).toBeNull();
  });

  it("explains when a folder download never starts", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      [onFileOffer, onFileProgress, onFileDone, onFileError, onFileDrop].forEach((mock) => capture(mock));
      const listing = capture<{ path: string; entries: { name: string; is_dir: boolean; size: number; modified: number }[]; error: string | null }>(onRemoteListing);
      const view = render(<FileTransfer open onOpenChange={vi.fn()} />);
      await waitFor(() => expect(listing.current).toBeTypeOf("function"));
      act(() => listing.current!({ path: "/srv", entries: [{ name: "docs", is_dir: true, size: 0, modified: 0 }], error: null }));
      fireEvent.click(await view.findByTitle("Download docs"));
      expect(pullRemote).toHaveBeenCalledWith("/srv/docs");
      act(() => { vi.advanceTimersByTime(20_000); });
      expect(await view.findByText(/docs did not start/)).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });
});
