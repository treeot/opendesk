/**
 * Rolling stream statistics for the session stats panel, measured from the
 * video frames as they arrive (before decoding). Pure, so it is unit-testable.
 */

/** How far back the frame rate and bitrate look. */
export const STATS_WINDOW_MS = 2_000;

export interface StreamStats {
  /** Frames received per second over the window. */
  fps: number;
  /** Video bits received per second over the window. */
  bitrateBps: number;
  /** Codec byte from the frame header (1 = H.264, 2 = HEVC), if any frame arrived. */
  codec?: number;
  width?: number;
  height?: number;
}

interface Sample { at: number; bytes: number }

export class StreamMeter {
  private samples: Sample[] = [];
  private last: { codec: number; width: number; height: number } | undefined;

  /** Record one received frame of `bytes` bytes at `now` (ms). */
  onFrame(now: number, bytes: number, frame: { codec: number; width: number; height: number }) {
    this.samples.push({ at: now, bytes });
    this.last = { codec: frame.codec, width: frame.width, height: frame.height };
    this.trim(now);
  }

  snapshot(now: number): StreamStats {
    this.trim(now);
    // Rate over the window once it is full; over the time since the first
    // frame while it is still filling, so the first second isn't understated.
    const first = this.samples[0];
    const span = first === undefined ? STATS_WINDOW_MS : Math.min(STATS_WINDOW_MS, Math.max(now - first.at, 250));
    const bytes = this.samples.reduce((sum, sample) => sum + sample.bytes, 0);
    return {
      fps: Math.round((this.samples.length * 1000) / span),
      bitrateBps: Math.round((bytes * 8 * 1000) / span),
      ...this.last,
    };
  }

  private trim(now: number) {
    const cutoff = now - STATS_WINDOW_MS;
    let drop = 0;
    while (drop < this.samples.length && this.samples[drop].at <= cutoff) drop++;
    if (drop > 0) this.samples.splice(0, drop);
  }
}

export function codecName(codec: number | undefined): string {
  switch (codec) {
    case 1: return "H.264";
    case 2: return "HEVC";
    case undefined: return "None yet";
    default: return `Unknown (${codec})`;
  }
}

export function formatBitrate(bps: number): string {
  if (bps >= 1_000_000) return `${(bps / 1_000_000).toFixed(1)} Mbps`;
  if (bps >= 1_000) return `${Math.round(bps / 1_000)} kbps`;
  return `${bps} bps`;
}

/** Packet loss as shown: the relay runs over TCP, so it never loses packets. */
export function formatLoss(path: "direct" | "relay", lossPct: number | null | undefined): string {
  if (path === "relay") return "None (relay uses TCP)";
  if (lossPct === null || lossPct === undefined) return "Not measured";
  return `${lossPct.toFixed(1)}%`;
}
