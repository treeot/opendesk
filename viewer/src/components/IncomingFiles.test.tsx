// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { NotifyContext } from "../lib/notify";

const { handlers, on, respondFile } = vi.hoisted(() => {
  const handlers = new Map<string, (payload: unknown) => void>();
  const on = (name: string) => (handler: (payload: unknown) => void) => { handlers.set(name, handler); return Promise.resolve(() => {}); };
  return { handlers, on, respondFile: vi.fn((_id: number, _accept: boolean) => Promise.resolve()) };
});
vi.mock("../lib/api", () => ({ onFileOffer: on("offer"), onFileDone: on("done"), onFileError: on("error"), respondFile }));

import IncomingFiles from "./IncomingFiles";

const emit = (name: string, payload: unknown) => act(() => { handlers.get(name)?.(payload); });
const offer = (id: number, name: string) => ({ id, name, size: 10, from: "987654321" });

describe("IncomingFiles", () => {
  afterEach(() => { cleanup(); vi.clearAllMocks(); handlers.clear(); });

  it("shows offers one at a time and declines with Escape", async () => {
    render(<IncomingFiles />);
    await waitFor(() => expect(handlers.has("offer")).toBe(true));
    emit("offer", offer(1, "a.txt"));
    emit("offer", offer(2, "b.txt"));
    expect(screen.getByRole("dialog").textContent).toContain("a.txt");
    expect(document.activeElement?.textContent).toBe("Decline");
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
    await waitFor(() => expect(screen.getByRole("dialog").textContent).toContain("b.txt"));
    expect(respondFile).toHaveBeenCalledWith(1, false);
    fireEvent.click(screen.getByRole("button", { name: "Decline" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(respondFile).toHaveBeenCalledWith(2, false);
  });

  it("drops a prompt the sender gave up on and reports failed transfers", async () => {
    const notify = vi.fn();
    render(<NotifyContext.Provider value={notify}><IncomingFiles /></NotifyContext.Provider>);
    await waitFor(() => expect(handlers.has("offer")).toBe(true));
    emit("offer", offer(1, "a.txt"));
    emit("error", { id: 1, message: "transfer cancelled" });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(notify).not.toHaveBeenCalled();
    emit("offer", offer(2, "b.txt"));
    fireEvent.click(screen.getByRole("button", { name: "Accept" }));
    await waitFor(() => expect(respondFile).toHaveBeenCalledWith(2, true));
    emit("error", { id: 2, message: "disk full" });
    expect(notify).toHaveBeenCalledWith(expect.stringContaining("Could not receive b.txt"));
  });

  it("asks once for a whole folder and shows its file count", async () => {
    render(<IncomingFiles />);
    await waitFor(() => expect(handlers.has("offer")).toBe(true));
    emit("offer", { ...offer(3, "photos"), files: 12, size: 4096 });
    const dialog = screen.getByRole("dialog");
    expect(dialog.textContent).toContain("Accept this folder?");
    expect(dialog.textContent).toContain("12 files");
    fireEvent.click(screen.getByRole("button", { name: "Accept" }));
    await waitFor(() => expect(respondFile).toHaveBeenCalledWith(3, true));
  });
});
