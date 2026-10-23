import { describe, expect, it } from "vitest";
import { codecForFrame, codecFromSps, DEFAULT_H264_CODEC, DEFAULT_HEVC_CODEC, FramePolicy, parseFrame, RateLimiter } from "./video";

function message(seq: number, flags: number, payload: number[]): Uint8Array {
  const bytes = new Uint8Array(18 + payload.length);
  const view = new DataView(bytes.buffer);
  view.setUint32(0, Math.floor(seq / 2 ** 32));
  view.setUint32(4, seq % 2 ** 32);
  view.setUint32(8, 123456);
  view.setUint8(12, 1);
  view.setUint8(13, flags);
  view.setUint16(14, 2560);
  view.setUint16(16, 1440);
  bytes.set(payload, 18);
  return bytes;
}

describe("parseFrame", () => {
  it("reads the big-endian header and keeps the payload as a view", () => {
    const bytes = message(2 ** 32 + 5, 1, [0, 0, 1, 0x65]);
    const frame = parseFrame(bytes.buffer as ArrayBuffer)!;
    expect(frame).toMatchObject({ seq: 2 ** 32 + 5, timestampUs: 123456, codec: 1, keyframe: true, width: 2560, height: 1440 });
    expect(Array.from(frame.data)).toEqual([0, 0, 1, 0x65]);
    expect(frame.data.buffer).toBe(bytes.buffer);
  });

  it("reads delta frames and ignores other flag bits", () => {
    expect(parseFrame(message(9, 0b10, [1]))!.keyframe).toBe(false);
  });

  it("accepts a Uint8Array with an offset", () => {
    const inner = message(3, 1, [7]);
    const outer = new Uint8Array(inner.length + 4);
    outer.set(inner, 4);
    expect(parseFrame(outer.subarray(4))).toMatchObject({ seq: 3, keyframe: true });
  });

  it("rejects messages shorter than the header", () => {
    expect(parseFrame(new Uint8Array(17))).toBeNull();
  });
});

describe("FramePolicy", () => {
  const key = (seq: number) => ({ seq, keyframe: true });
  const delta = (seq: number) => ({ seq, keyframe: false });

  it("waits for the first keyframe", () => {
    const policy = new FramePolicy();
    expect(policy.next(delta(1), 0)).toEqual({ decode: false, requestKeyframe: true });
    expect(policy.next(key(2), 0)).toEqual({ decode: true, flush: false, requestKeyframe: false });
    expect(policy.next(delta(3), 0)).toEqual({ decode: true, flush: false, requestKeyframe: false });
  });

  it("skips deltas after a seq gap until the next keyframe", () => {
    const policy = new FramePolicy();
    policy.next(key(1), 0);
    policy.next(delta(2), 0);
    expect(policy.next(delta(4), 0).decode).toBe(false);
    expect(policy.next(delta(5), 0)).toEqual({ decode: false, requestKeyframe: true });
    expect(policy.next(key(6), 0).decode).toBe(true);
    expect(policy.next(delta(7), 0).decode).toBe(true);
  });

  it("decodes a keyframe even after a gap", () => {
    const policy = new FramePolicy();
    policy.next(key(1), 0);
    expect(policy.next(key(10), 0).decode).toBe(true);
  });

  it("drops deltas once the decoder backs up", () => {
    const policy = new FramePolicy();
    policy.next(key(1), 0);
    expect(policy.next(delta(2), 2).decode).toBe(true);
    expect(policy.next(delta(3), 3)).toEqual({ decode: false, requestKeyframe: true });
    // Still skipping once the queue drains: the reference chain is broken.
    expect(policy.next(delta(4), 0).decode).toBe(false);
  });

  it("flushes the backlog before a keyframe when the decoder is behind", () => {
    const policy = new FramePolicy();
    expect(policy.next(key(1), 5)).toEqual({ decode: true, flush: true, requestKeyframe: false });
  });

  it("waits again after waitForKeyframe", () => {
    const policy = new FramePolicy();
    policy.next(key(1), 0);
    policy.waitForKeyframe();
    expect(policy.waiting).toBe(true);
    expect(policy.next(delta(2), 0).decode).toBe(false);
  });

  it("treats a restarted sequence as a gap", () => {
    const policy = new FramePolicy();
    policy.next(key(50), 0);
    expect(policy.next(delta(0), 0).decode).toBe(false);
  });
});

describe("RateLimiter", () => {
  it("allows one request per interval", () => {
    const limiter = new RateLimiter(500);
    expect(limiter.tryAcquire(1000)).toBe(true);
    expect(limiter.tryAcquire(1200)).toBe(false);
    expect(limiter.tryAcquire(1499)).toBe(false);
    expect(limiter.tryAcquire(1500)).toBe(true);
  });

  it("allows the first request at time zero", () => {
    expect(new RateLimiter().tryAcquire(0)).toBe(true);
  });
});

describe("codecFromSps", () => {
  it("builds the codec string from an SPS after a 4-byte start code", () => {
    const au = new Uint8Array([0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x28, 0xaa, 0, 0, 0, 1, 0x68, 0xce]);
    expect(codecFromSps(au)).toBe("avc1.42C028");
  });

  it("finds an SPS after other NAL units and 3-byte start codes", () => {
    const au = new Uint8Array([0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x67, 0x64, 0x00, 0x33, 0x11]);
    expect(codecFromSps(au)).toBe("avc1.640033");
  });

  it("returns null without an SPS", () => {
    expect(codecFromSps(new Uint8Array([0, 0, 0, 1, 0x41, 0x9a]))).toBeNull();
    expect(codecFromSps(new Uint8Array([0, 0, 1, 0x67, 0x42]))).toBeNull();
  });

  it("has a default covering 4K-wide baseline streams", () => {
    expect(DEFAULT_H264_CODEC).toBe("avc1.42E034");
  });
});

describe("codecForFrame", () => {
  const sps = new Uint8Array([0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x28, 0xaa, 0, 0, 0, 1, 0x68, 0xce]);

  it("uses the exact H.264 codec from a keyframe's SPS", () => {
    expect(codecForFrame({ codec: 1, keyframe: true, data: sps })).toBe("avc1.42C028");
  });

  it("falls back to the H.264 default without an SPS", () => {
    expect(codecForFrame({ codec: 1, keyframe: true, data: new Uint8Array([0, 0, 0, 1, 0x41]) })).toBe(DEFAULT_H264_CODEC);
    expect(codecForFrame({ codec: 1, keyframe: false, data: new Uint8Array() })).toBe(DEFAULT_H264_CODEC);
  });

  it("uses the HEVC codec when the frame is marked HEVC", () => {
    expect(codecForFrame({ codec: 2, keyframe: true, data: new Uint8Array([0, 0, 0, 1, 0x26]) })).toBe(DEFAULT_HEVC_CODEC);
  });
});
