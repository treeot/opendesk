import { describe, expect, it } from "vitest";
import { codecName, formatBitrate, formatLoss, StreamMeter, STATS_WINDOW_MS } from "./stats";

const frame = { codec: 1, width: 1920, height: 1080 };

describe("stream stats", () => {
  it("measures frame rate and bitrate over a rolling window", () => {
    const meter = new StreamMeter();
    // 30 fps of 10 kB frames for three seconds.
    for (let i = 0; i < 90; i++) meter.onFrame(i * (1000 / 30), 10_000, frame);
    const stats = meter.snapshot(3000);
    expect(stats.fps).toBe(30);
    expect(Math.abs(stats.bitrateBps - 2_400_000)).toBeLessThan(24_000);
    expect(stats).toMatchObject({ codec: 1, width: 1920, height: 1080 });
  });

  it("does not understate the rate while the window fills, and drops to zero when frames stop", () => {
    const meter = new StreamMeter();
    for (let i = 0; i < 10; i++) meter.onFrame(i * 50, 1_000, frame);
    expect(meter.snapshot(500).fps).toBe(20);
    expect(meter.snapshot(450 + STATS_WINDOW_MS + 1).fps).toBe(0);
    expect(new StreamMeter().snapshot(0)).toEqual({ fps: 0, bitrateBps: 0 });
  });

  it("keeps the latest codec and resolution", () => {
    const meter = new StreamMeter();
    meter.onFrame(0, 100, frame);
    meter.onFrame(10, 100, { codec: 2, width: 2560, height: 1440 });
    expect(meter.snapshot(20)).toMatchObject({ codec: 2, width: 2560, height: 1440 });
  });

  it("formats values for the panel", () => {
    expect(codecName(1)).toBe("H.264");
    expect(codecName(2)).toBe("HEVC");
    expect(codecName(undefined)).toBe("None yet");
    expect(formatBitrate(2_450_000)).toBe("2.5 Mbps");
    expect(formatBitrate(640_000)).toBe("640 kbps");
    expect(formatBitrate(0)).toBe("0 bps");
    expect(formatLoss("relay", null)).toContain("TCP");
    expect(formatLoss("direct", null)).toBe("Not measured");
    expect(formatLoss("direct", 1.25)).toBe("1.3%");
  });
});
