// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

const { listForwards, startForward, stopForward } = vi.hoisted(() => ({
  listForwards: vi.fn(), startForward: vi.fn(), stopForward: vi.fn(),
}));
vi.mock("../lib/api", () => ({ listForwards, startForward, stopForward }));

import PortForward from "./PortForward";

describe("PortForward", () => {
  afterEach(() => { cleanup(); vi.clearAllMocks(); localStorage.clear(); });

  it("restores a device's saved forwards when the panel opens", async () => {
    localStorage.setItem("opendesk.forwards.905218437", JSON.stringify([[11500, 11434]]));
    listForwards.mockResolvedValueOnce([]).mockResolvedValue([[11500, 11434]]);
    startForward.mockResolvedValue(undefined);
    render(<PortForward open onClose={() => {}} remoteId="905218437" />);
    await waitFor(() => expect(startForward).toHaveBeenCalledWith(11500, 11434));
  });

  it("saves a forward per device after adding it", async () => {
    listForwards.mockResolvedValueOnce([]).mockResolvedValue([[9000, 8000]]);
    startForward.mockResolvedValue(undefined);
    render(<PortForward open onClose={() => {}} remoteId="730514826" />);
    fireEvent.change(screen.getByLabelText("Local port"), { target: { value: "9000" } });
    fireEvent.change(screen.getByLabelText("Remote port"), { target: { value: "8000" } });
    fireEvent.click(screen.getByRole("button", { name: "Add" }));
    await waitFor(() => expect(localStorage.getItem("opendesk.forwards.730514826")).toBe(JSON.stringify([[9000, 8000]])));
  });

  it("does not restore or save without a remote ID", async () => {
    localStorage.setItem("opendesk.forwards.undefined", JSON.stringify([[1, 2]]));
    listForwards.mockResolvedValue([]);
    render(<PortForward open onClose={() => {}} />);
    await waitFor(() => expect(listForwards).toHaveBeenCalled());
    expect(startForward).not.toHaveBeenCalled();
  });
});
