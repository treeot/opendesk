// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AccessRules as Rules } from "../lib/api";

const { getAccessRules, setAccessRules, getIdleTimeout, setIdleTimeout } = vi.hoisted(() => ({
  getAccessRules: vi.fn<() => Promise<Rules>>(),
  setAccessRules: vi.fn<(rules: Rules) => Promise<Rules>>((rules) => Promise.resolve(rules)),
  getIdleTimeout: vi.fn(() => Promise.resolve(0)),
  setIdleTimeout: vi.fn((_minutes: number) => Promise.resolve()),
}));
vi.mock("../lib/api", () => ({ getAccessRules, setAccessRules, getIdleTimeout, setIdleTimeout }));

import AccessRules, { IdleTimeout, describeRule, fromClock, toClock } from "./AccessRules";

const empty: Rules = { mode: "mark_known", allowed: [], hours: { enabled: false, rules: [] } };

describe("AccessRules", () => {
  afterEach(() => { cleanup(); vi.clearAllMocks(); });

  it("converts and describes times", () => {
    expect(toClock(9 * 60 + 5)).toBe("09:05");
    expect(fromClock("23:59")).toBe(23 * 60 + 59);
    expect(fromClock("24:00")).toBeNull();
    expect(fromClock("")).toBeNull();
    expect(describeRule({ days: [4], start: 22 * 60, end: 6 * 60 })).toBe("Fri, 22:00 to 06:00 (next day)");
    expect(describeRule({ days: [5, 6], start: 0, end: 0 })).toBe("Sat, Sun, all day");
  });

  it("adds a labelled ID, picks a mode and saves", async () => {
    getAccessRules.mockResolvedValue(empty);
    render(<AccessRules />);
    fireEvent.change(await screen.findByLabelText("Allowed device ID"), { target: { value: "123 456 789" } });
    fireEvent.change(screen.getByLabelText("Allowed device label"), { target: { value: "Office" } });
    fireEvent.click(screen.getByRole("button", { name: "Add" }));
    expect(screen.getByText("123456789")).toBeTruthy();
    fireEvent.click(screen.getByLabelText(/Only allow listed devices/));
    fireEvent.click(screen.getByRole("button", { name: "Save rules" }));
    await waitFor(() => expect(setAccessRules).toHaveBeenCalledWith({ mode: "only_listed", allowed: [{ id: "123456789", label: "Office" }], hours: { enabled: false, rules: [] } }));
    expect(await screen.findByText(/apply from the next connection/)).toBeTruthy();
  });

  it("refuses to save a list-only mode with no IDs", async () => {
    getAccessRules.mockResolvedValue(empty);
    render(<AccessRules />);
    fireEvent.click(await screen.findByLabelText(/Only allow listed devices/));
    fireEvent.click(screen.getByRole("button", { name: "Save rules" }));
    expect(await screen.findByText(/Add at least one ID/)).toBeTruthy();
    expect(setAccessRules).not.toHaveBeenCalled();
  });

  it("turns on allowed hours with a weekday default and edits the days", async () => {
    getAccessRules.mockResolvedValue(empty);
    render(<AccessRules />);
    fireEvent.click(await screen.findByLabelText("Only accept connections during set hours"));
    expect(screen.getByRole("group", { name: "Mon, Tue, Wed, Thu, Fri, 09:00 to 17:00" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Sat" }));
    fireEvent.change(screen.getByLabelText("Until"), { target: { value: "02:00" } });
    fireEvent.click(screen.getByRole("button", { name: "Save rules" }));
    await waitFor(() => expect(setAccessRules).toHaveBeenCalled());
    expect(setAccessRules.mock.calls[0][0].hours).toEqual({ enabled: true, rules: [{ days: [0, 1, 2, 3, 4, 5], start: 540, end: 120 }] });
  });

  it("shows the backend's reason when saving fails", async () => {
    getAccessRules.mockResolvedValue(empty);
    setAccessRules.mockRejectedValueOnce("Labels can be at most 64 characters.");
    render(<AccessRules />);
    fireEvent.click(await screen.findByLabelText(/Accept listed devices/));
    fireEvent.click(screen.getByRole("button", { name: "Save rules" }));
    expect(await screen.findByText("Labels can be at most 64 characters.")).toBeTruthy();
  });

  it("saves the idle timeout as soon as it changes", async () => {
    render(<IdleTimeout />);
    const select = await screen.findByLabelText("Disconnect after minutes without input");
    await waitFor(() => expect((select as HTMLSelectElement).disabled).toBe(false));
    fireEvent.change(select, { target: { value: "15" } });
    expect(setIdleTimeout).toHaveBeenCalledWith(15);
  });
});
