// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { BrowseSignal, FileSignal } from "../lib/types";
import Session, { type SessionControls } from "./Session";

vi.mock("../../../viewer/src/lib/decode", () => ({ makeDecoder: () => ({ push: vi.fn(), close: vi.fn() }), supportsHevcDecode: () => Promise.resolve(false) }));

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function fakeControls() {
  const sinks: { file?: (signal: FileSignal) => void; browse?: (signal: BrowseSignal) => void; chat?: (text: string) => void } = {};
  const controls = {
    sendInput: vi.fn(),
    requestKeyframe: vi.fn(),
    selectMonitor: vi.fn(),
    setVideoSink: vi.fn(),
    setAudioSink: vi.fn(),
    sendClipboard: vi.fn(),
    sendClipboardImage: vi.fn(),
    setClipboardSink: vi.fn(),
    sendFile: vi.fn(),
    acceptFile: vi.fn(),
    rejectFile: vi.fn(),
    setFileSink: vi.fn((sink) => { sinks.file = sink ?? undefined; }),
    listDir: vi.fn(),
    pullFile: vi.fn(),
    makeDir: vi.fn(),
    deletePath: vi.fn(),
    renamePath: vi.fn(),
    setBrowseSink: vi.fn((sink) => { sinks.browse = sink ?? undefined; }),
    sendChat: vi.fn(() => true),
    setChatSink: vi.fn((sink) => { sinks.chat = sink ?? undefined; }),
    secureAttention: vi.fn(),
    setMaxBitrate: vi.fn(),
    setTextReadability: vi.fn(),
  } satisfies SessionControls;
  return { controls, sinks };
}

function renderSession(extra: Partial<Parameters<typeof Session>[0]> = {}) {
  const { controls, sinks } = fakeControls();
  render(<Session remoteId="123456789" status="streaming" monitors={[]} controls={controls} onDisconnect={() => {}} {...extra} />);
  return { controls, sinks };
}

describe("Session", () => {
  it("shows the relayed link and its round trip", () => {
    renderSession({ roundTrip: 140 });
    const badge = screen.getByTitle(/Relayed/);
    expect(badge.textContent).toContain("140 ms");
    expect(badge.getAttribute("data-slow")).toBe("true");
  });

  it("chats both ways and counts unread messages", () => {
    const { controls, sinks } = renderSession();
    act(() => sinks.chat?.("hello from the host"));
    expect(screen.getByRole("button", { name: "Chat, 1 unread" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Chat, 1 unread" }));
    const panel = screen.getByRole("region", { name: "Chat" });
    expect(panel.textContent).toContain("hello from the host");
    fireEvent.change(within(panel).getByLabelText("Message"), { target: { value: "on my way" } });
    fireEvent.click(within(panel).getByRole("button", { name: "Send" }));
    expect(controls.sendChat).toHaveBeenCalledWith("on my way");
    expect(panel.textContent).toContain("on my way");
    expect(screen.getByRole("button", { name: "Chat" })).toBeTruthy();
  });

  it("says so when the host records the session", () => {
    renderSession({ features: ["recorded"] });
    expect(screen.getByText("This session is being recorded by the remote computer")).toBeTruthy();
  });

  it("sends special keys, using secure attention when the host offers it", () => {
    const { controls } = renderSession({ features: ["secure-attention"] });
    fireEvent.click(screen.getByRole("button", { name: "Special keys" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Alt+Tab" }));
    expect(controls.sendInput).toHaveBeenCalledTimes(2);
    const [down] = controls.sendInput.mock.calls[0] as [Uint8Array];
    expect([down[0], down[12], down[13], down[14]]).toEqual([4, 9, 1, 4]);
    expect(screen.queryByRole("menu")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Special keys" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Ctrl+Alt+Del" }));
    expect(controls.secureAttention).toHaveBeenCalledTimes(1);
    expect(controls.sendInput).toHaveBeenCalledTimes(2);
  });

  it("closes a menu with Escape", () => {
    renderSession();
    fireEvent.click(screen.getByRole("button", { name: "More" }));
    const menu = screen.getByRole("menu", { name: "More" });
    fireEvent.keyDown(menu, { key: "Escape" });
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("view only stops input and quality reaches the host", () => {
    const { controls } = renderSession();
    fireEvent.click(screen.getByRole("button", { name: "More" }));
    fireEvent.click(screen.getByRole("menuitemcheckbox", { name: /View only/ }));
    expect(screen.getByText("View only · Encrypted")).toBeTruthy();
    fireEvent.change(screen.getByLabelText("Quality"), { target: { value: "3000000" } });
    expect(controls.setMaxBitrate).toHaveBeenLastCalledWith(3_000_000);
    fireEvent.change(screen.getByLabelText("Quality"), { target: { value: "0" } });
    expect(controls.setMaxBitrate).toHaveBeenLastCalledWith(0);
    fireEvent.click(screen.getByRole("button", { name: "More" }));
    fireEvent.click(screen.getByRole("menuitemcheckbox", { name: /Text readability/ }));
    expect(controls.setTextReadability).toHaveBeenLastCalledWith(true);
  });

  it("browses host folders, downloads without a second prompt and manages entries", () => {
    vi.useFakeTimers();
    try {
      const { controls, sinks } = renderSession();
      fireEvent.click(screen.getByRole("button", { name: "Files" }));
      fireEvent.click(screen.getByRole("tab", { name: "Remote files" }));
      expect(controls.listDir).toHaveBeenCalledWith("");
      act(() => sinks.browse?.({ t: "dir_listing", req: 1, path: "/home/me", entries: [
        { name: "notes.txt", is_dir: false, size: 12, modified: 0 },
        { name: "Docs", is_dir: true, size: 0, modified: 0 },
      ] }));
      const list = screen.getByRole("list", { name: "Contents of /home/me" });
      expect(within(list).getAllByRole("listitem")[0].textContent).toContain("Docs");
      fireEvent.click(within(list).getByRole("button", { name: "Docs" }));
      expect(controls.listDir).toHaveBeenLastCalledWith("/home/me/Docs");

      fireEvent.click(screen.getByRole("button", { name: "Download notes.txt" }));
      expect(controls.pullFile).toHaveBeenCalledWith("/home/me/notes.txt");
      act(() => sinks.file?.({ kind: "file-offer", id: 5, name: "notes.txt", size: 12 }));
      expect(controls.acceptFile).toHaveBeenCalledWith(5);
      // An offer nobody asked for still waits for the user.
      act(() => sinks.file?.({ kind: "file-offer", id: 6, name: "other.txt", size: 1 }));
      expect(controls.acceptFile).toHaveBeenCalledTimes(1);
      expect(screen.getByRole("button", { name: "Accept" })).toBeTruthy();

      fireEvent.click(screen.getByRole("tab", { name: "Remote files" }));
      vi.spyOn(window, "prompt").mockReturnValue("Archive");
      fireEvent.click(screen.getByRole("button", { name: "New folder" }));
      expect(controls.makeDir).toHaveBeenCalledWith("/home/me", "Archive");
      vi.spyOn(window, "confirm").mockReturnValue(true);
      fireEvent.click(screen.getByRole("button", { name: "Delete notes.txt" }));
      expect(controls.deletePath).toHaveBeenCalledWith("/home/me/notes.txt");
      vi.spyOn(window, "prompt").mockReturnValue("a/b");
      vi.spyOn(window, "alert").mockImplementation(() => {});
      fireEvent.click(screen.getByRole("button", { name: "Rename notes.txt" }));
      expect(controls.renamePath).not.toHaveBeenCalled();
      act(() => { vi.advanceTimersByTime(500); });
      expect(controls.listDir).toHaveBeenLastCalledWith("/home/me");
    } finally {
      vi.useRealTimers();
    }
  });

  it("sends dropped files into the open host folder", async () => {
    const { controls, sinks } = renderSession();
    fireEvent.click(screen.getByRole("button", { name: "Files" }));
    fireEvent.click(screen.getByRole("tab", { name: "Remote files" }));
    act(() => sinks.browse?.({ t: "dir_listing", req: 1, path: "/srv/drop", entries: [] }));
    const file = new File(["hello"], "hello.txt", { type: "text/plain" });
    const main = screen.getByRole("main");
    fireEvent.dragEnter(main, { dataTransfer: { types: ["Files"], files: [file] } });
    expect(document.querySelector(".web-drop")?.textContent).toContain("/srv/drop");
    await act(async () => { fireEvent.drop(main, { dataTransfer: { types: ["Files"], files: [file] } }); });
    expect(controls.sendFile).toHaveBeenCalledTimes(1);
    const [, name, data, dir] = controls.sendFile.mock.calls[0] as unknown as [number, string, Uint8Array, string];
    expect([name, new TextDecoder().decode(data), dir]).toEqual(["hello.txt", "hello", "/srv/drop"]);
    expect(screen.getByRole("tab", { name: "Transfers" }).getAttribute("aria-selected")).toBe("true");
  });
});
