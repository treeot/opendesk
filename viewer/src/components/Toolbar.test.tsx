// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import Toolbar, { monitorName, virtualDisplaySize } from "./Toolbar";

describe("monitorName", () => {
  it("turns Windows device paths into Display N", () => {
    expect(monitorName("\\\\.\\DISPLAY2", 1)).toBe("Display 2");
  });

  it("keeps readable names and falls back to the position", () => {
    expect(monitorName("Built-in Retina Display", 0)).toBe("Built-in Retina Display");
    expect(monitorName("  ", 2)).toBe("Display 3");
  });
});

describe("virtualDisplaySize", () => {
  it("uses device pixels, caps at 3840x2160 and keeps sizes even", () => {
    expect(virtualDisplaySize(1440, 900, 2)).toEqual({ width: 2880, height: 1800, hidpi: true });
    expect(virtualDisplaySize(1281, 721, 1)).toEqual({ width: 1280, height: 720, hidpi: false });
    expect(virtualDisplaySize(2560, 1440, 2)).toEqual({ width: 3840, height: 2160, hidpi: true });
    expect(virtualDisplaySize(3440, 1440, 1)).toEqual({ width: 3440, height: 1440, hidpi: false });
    // Ultra-wide at 2x: limited by width.
    expect(virtualDisplaySize(3440, 1440, 2)).toEqual({ width: 3840, height: 1606, hidpi: true });
    expect(virtualDisplaySize(800, 600, Number.NaN)).toEqual({ width: 800, height: 600, hidpi: false });
  });
});

describe("monitor picker", () => {
  afterEach(cleanup);
  const monitors = [
    { id: 1, name: "Built-in", width: 0, height: 0, is_primary: true },
    { id: 5, name: "OpenDesk Virtual Display", width: 0, height: 0, is_primary: false, is_virtual: true },
  ];

  it("offers Add display only when the host supports it", () => {
    const onAddDisplay = vi.fn();
    const onMonitorChange = vi.fn();
    const { rerender } = render(<Toolbar remoteId="1" onDisconnect={vi.fn()} monitors={monitors.slice(0, 1)} />);
    expect(screen.queryByRole("option", { name: "Add display…" })).toBeNull();
    rerender(<Toolbar remoteId="1" onDisconnect={vi.fn()} monitors={monitors.slice(0, 1)} canAddDisplay onAddDisplay={onAddDisplay} onMonitorChange={onMonitorChange} />);
    fireEvent.change(screen.getByRole("combobox", { name: "Monitor" }), { target: { value: "add-display" } });
    expect(onAddDisplay).toHaveBeenCalledOnce();
    expect(onMonitorChange).not.toHaveBeenCalled();
  });

  it("offers Remove only while a virtual display is shown", () => {
    const onRemoveDisplay = vi.fn();
    const { rerender } = render(<Toolbar remoteId="1" onDisconnect={vi.fn()} monitors={monitors} selectedMonitor={1} onRemoveDisplay={onRemoveDisplay} />);
    fireEvent.click(screen.getByRole("button", { name: "More" }));
    expect(screen.queryByRole("menuitem", { name: "Remove this screen" })).toBeNull();
    expect(screen.getByRole("option", { name: /Virtual/ })).toBeTruthy();
    rerender(<Toolbar remoteId="1" onDisconnect={vi.fn()} monitors={monitors} selectedMonitor={5} onRemoveDisplay={onRemoveDisplay} />);
    fireEvent.click(screen.getByRole("menuitem", { name: "Remove this screen" }));
    expect(onRemoveDisplay).toHaveBeenCalledWith(5);
  });

  it("is hidden from the host side", () => {
    render(<Toolbar remoteId="1" role="host" onDisconnect={vi.fn()} monitors={monitors} selectedMonitor={5} canAddDisplay />);
    expect(screen.queryByRole("combobox", { name: "Monitor" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Remove display" })).toBeNull();
  });
});

describe("Toolbar", () => {
  afterEach(cleanup);

  it("shows a visible text label on each primary viewer control", () => {
    render(<Toolbar remoteId="123456789" onDisconnect={vi.fn()} />);
    for (const label of ["Fullscreen", "Sound", "Files", "Chat", "More"]) {
      expect(screen.getByText(label)).toBeTruthy();
    }
  });

  it("calls onDisconnect when Disconnect is clicked", () => {
    const onDisconnect = vi.fn();
    render(<Toolbar remoteId="123456789" onDisconnect={onDisconnect} />);
    fireEvent.click(screen.getByRole("button", { name: "Disconnect" }));
    expect(onDisconnect).toHaveBeenCalledOnce();
  });

  it("shows the remote ID and an MM:SS timer", () => {
    render(<Toolbar remoteId="987654321" onDisconnect={vi.fn()} />);
    expect(screen.getByText("987654321")).toBeTruthy();
    expect(screen.getByLabelText("Session timer").textContent).toMatch(/^\d{2}:\d{2}$/);
  });

  it("calls the recording handler for start and stop", () => {
    const onRecordingToggle = vi.fn();
    const { rerender } = render(<Toolbar remoteId="123" onDisconnect={vi.fn()} onRecordingToggle={onRecordingToggle} />);
    fireEvent.click(screen.getByRole("button", { name: "More" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Record" }));
    rerender(<Toolbar remoteId="123" onDisconnect={vi.fn()} recordingActive onRecordingToggle={onRecordingToggle} />);
    fireEvent.click(screen.getByRole("button", { name: "More" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Stop recording" }));
    expect(onRecordingToggle).toHaveBeenCalledTimes(2);
  });

  it("exposes toggle state: audio in the bar, clipboard in the overflow menu", () => {
    const { rerender } = render(<Toolbar remoteId="123" onDisconnect={vi.fn()} clipboardSync audioMuted={false} />);
    const audio = screen.getByRole("button", { name: "Mute audio" });
    expect(audio.getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(screen.getByRole("button", { name: "More" }));
    expect(screen.getByRole("menuitem", { name: /Clipboard sync/ }).getAttribute("aria-pressed")).toBe("true");
    rerender(<Toolbar remoteId="123" onDisconnect={vi.fn()} clipboardSync={false} audioMuted />);
    expect(audio.getAttribute("aria-pressed")).toBe("true");
    expect(audio.title).toMatch(/muted/);
    expect(screen.getByRole("menuitem", { name: /Clipboard sync/ }).getAttribute("aria-pressed")).toBe("false");
  });

  it("hides controls that do nothing yet", () => {
    render(<Toolbar remoteId="123" onDisconnect={vi.fn()} />);
    expect(screen.queryByRole("button", { name: "Fit / Scale" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Keyboard" })).toBeNull();
    // Quality is always available: older hosts simply ignore the cap.
    expect(screen.getByLabelText("Quality")).toBeTruthy();
  });

  it("colours the connection dot by status", () => {
    const { container, rerender } = render(<Toolbar remoteId="123" status="in_session" onDisconnect={vi.fn()} />);
    const dot = container.querySelector(".session-toolbar__connected-dot")!;
    expect(dot.getAttribute("data-status")).toBe("in_session");
    rerender(<Toolbar remoteId="123" status="reconnecting" onDisconnect={vi.fn()} />);
    expect(dot.getAttribute("data-status")).toBe("reconnecting");
    expect(screen.getByText("Reconnecting")).toBeTruthy();
  });
});

describe("connection badge", () => {
  afterEach(cleanup);

  const badge = (container: HTMLElement) => container.querySelector<HTMLElement>(".session-toolbar__link");

  it("is absent until the backend reports a path, and never invents a latency", () => {
    const { container } = render(<Toolbar remoteId="123" onDisconnect={vi.fn()} />);
    expect(badge(container)).toBeNull();
    expect(container.textContent).not.toMatch(/\d+ ms/);
  });

  it("shows a direct path with its round trip", () => {
    const { container } = render(<Toolbar remoteId="123" link={{ path: "direct", rtt_ms: 14 }} onDisconnect={vi.fn()} />);
    const element = badge(container)!;
    expect(element.textContent).toBe("Direct·14 ms");
    expect(element.dataset.path).toBe("direct");
    expect(element.dataset.slow).toBe("false");
    expect(element.title).toMatch(/straight between the two computers.*Round trip 14 ms/);
  });

  it("shows a dash while the latency is unknown and explains Relayed", () => {
    const { container } = render(<Toolbar remoteId="123" link={{ path: "relay", rtt_ms: null }} onDisconnect={vi.fn()} />);
    const element = badge(container)!;
    expect(element.textContent).toBe("Relayed·—");
    expect(element.dataset.path).toBe("relay");
    expect(element.title).toMatch(/relay server/);
    expect(element.title).toMatch(/not measured/);
  });

  it("warns above 100 ms", () => {
    const { container, rerender } = render(<Toolbar remoteId="123" link={{ path: "relay", rtt_ms: 100 }} onDisconnect={vi.fn()} />);
    expect(badge(container)!.dataset.slow).toBe("false");
    rerender(<Toolbar remoteId="123" link={{ path: "relay", rtt_ms: 140 }} onDisconnect={vi.fn()} />);
    expect(badge(container)!.dataset.slow).toBe("true");
    expect(badge(container)!.textContent).toContain("140 ms");
  });

  it("is not shown to the host", () => {
    const { container } = render(<Toolbar remoteId="123" role="host" link={{ path: "direct", rtt_ms: 5 }} onDisconnect={vi.fn()} />);
    expect(badge(container)).toBeNull();
  });
});
