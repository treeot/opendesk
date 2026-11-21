/** The browser viewer's list of file transfers, driven by web-core signals. */
import type { FileSignal } from "./types";

export type TransferState = "pending" | "active" | "done" | "error";

export interface Transfer {
  id: number;
  name: string;
  total: number;
  transferred: number;
  sending: boolean;
  state: TransferState;
  message?: string;
}

export function applySignal(transfers: Transfer[], signal: FileSignal): Transfer[] {
  const index = transfers.findIndex((transfer) => transfer.id === signal.id);
  const next = [...transfers];
  const patch = (changes: Partial<Transfer>) => {
    if (index >= 0) next[index] = { ...next[index], ...changes };
  };
  switch (signal.kind) {
    case "file-offer":
      if (index < 0) {
        next.push({ id: signal.id, name: signal.name ?? "file", total: signal.size ?? 0, transferred: 0, sending: false, state: "pending" });
      }
      break;
    case "file-progress":
      patch({ transferred: signal.transferred ?? 0, total: signal.total ?? next[index]?.total ?? 0, state: "active" });
      break;
    case "file-received":
    case "file-sent":
      patch({ state: "done", transferred: next[index]?.total ?? 0 });
      break;
    case "file-error":
      patch({ state: "error", message: signal.message });
      break;
  }
  return next;
}

/** How long a requested download waits for the host's offer. */
export const PULL_WAIT_MS = 60_000;

/**
 * Files the user asked the host for. The host answers a pull with an
 * ordinary offer, which is accepted without a second prompt when its name
 * matches a recent request; any other offer still asks.
 */
export class PendingPulls {
  private pulls: { name: string; at: number }[] = [];

  constructor(private readonly now: () => number = () => Date.now()) {}

  add(name: string) {
    this.pulls.push({ name, at: this.now() });
  }

  /** Consume a request for `name`, if one is still fresh. */
  take(name: string): boolean {
    const now = this.now();
    this.pulls = this.pulls.filter((pull) => now - pull.at < PULL_WAIT_MS);
    const index = this.pulls.findIndex((pull) => pull.name === name);
    if (index < 0) return false;
    this.pulls.splice(index, 1);
    return true;
  }
}
