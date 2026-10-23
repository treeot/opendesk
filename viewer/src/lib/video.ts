/**
 * Pure video-stream logic for the viewer: parsing the binary frame messages
 * the backend streams over a Tauri channel, deciding which frames to decode,
 * and rate-limiting keyframe requests. No WebCodecs here, so it is unit-testable.
 */

/** Bytes before the H.264 access unit (seq, timestamp, codec, flags, width, height). */
export const FRAME_HEADER_LEN = 18;

export interface VideoFrame {
  seq: number;
  timestampUs: number;
  codec: number;
  keyframe: boolean;
  width: number;
  height: number;
  /** Annex B access unit; a view into the received buffer, not a copy. */
  data: Uint8Array;
}

/**
 * Parse one frame message: an 18-byte big-endian header (seq u64,
 * timestamp_us u32, codec u8, flags u8 with bit 0 = keyframe, width u16,
 * height u16) followed by the encoded frame. Returns null when too short.
 */
export function parseFrame(message: ArrayBuffer | Uint8Array): VideoFrame | null {
  const bytes = message instanceof Uint8Array ? message : new Uint8Array(message);
  if (bytes.byteLength < FRAME_HEADER_LEN) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return {
    // seq stays well below 2^53 (one per frame), so a Number is exact.
    seq: view.getUint32(0) * 2 ** 32 + view.getUint32(4),
    timestampUs: view.getUint32(8),
    codec: view.getUint8(12),
    keyframe: (view.getUint8(13) & 1) !== 0,
    width: view.getUint16(14),
    height: view.getUint16(16),
    data: bytes.subarray(FRAME_HEADER_LEN),
  };
}

/** Decoder queue depth above which delta frames are dropped to shed latency. */
export const MAX_DECODE_QUEUE = 2;

export type FrameAction =
  /** Decode the frame. `flush` discards the decoder's backlog first. */
  | { decode: true; flush: boolean; requestKeyframe: false }
  /** Skip the frame; ask the host for a keyframe when `requestKeyframe`. */
  | { decode: false; requestKeyframe: boolean };

/**
 * Decides which frames reach the decoder. A delta frame is only useful when
 * every frame since the last keyframe was decoded, so after a seq gap, a
 * backed-up decoder or a decoder reset we skip deltas until the next keyframe
 * and ask the host for one. Starts out waiting for a keyframe.
 */
export class FramePolicy {
  private lastSeq: number | undefined;
  private waitingForKeyframe = true;

  /** Skip deltas until the next keyframe (e.g. after the decoder was recreated). */
  waitForKeyframe() {
    this.waitingForKeyframe = true;
  }

  get waiting() {
    return this.waitingForKeyframe;
  }

  next(frame: Pick<VideoFrame, "seq" | "keyframe">, decodeQueueSize: number): FrameAction {
    const gap = this.lastSeq !== undefined && frame.seq !== this.lastSeq + 1;
    this.lastSeq = frame.seq;
    if (frame.keyframe) {
      this.waitingForKeyframe = false;
      return { decode: true, flush: decodeQueueSize > MAX_DECODE_QUEUE, requestKeyframe: false };
    }
    if (this.waitingForKeyframe || gap || decodeQueueSize > MAX_DECODE_QUEUE) {
      this.waitingForKeyframe = true;
      return { decode: false, requestKeyframe: true };
    }
    return { decode: true, flush: false, requestKeyframe: false };
  }
}

/** Minimum spacing between keyframe requests, so a burst of loss sends one. */
export const KEYFRAME_REQUEST_INTERVAL_MS = 500;

/** Lets a request through at most once per `intervalMs`. */
export class RateLimiter {
  private last = -Infinity;

  constructor(private readonly intervalMs = KEYFRAME_REQUEST_INTERVAL_MS) {}

  tryAcquire(now: number): boolean {
    if (now - this.last < this.intervalMs) return false;
    this.last = now;
    return true;
  }
}

/** Baseline profile, level 5.2: covers streams up to 4096 wide. */
export const DEFAULT_H264_CODEC = "avc1.42E034";

/** The frame-header codec byte for HEVC (H.264 is 1; see host `Codec`). */
export const CODEC_HEVC = 2;

/** HEVC Main profile, level 5.1: covers 4K. Used when a frame is marked HEVC. */
export const DEFAULT_HEVC_CODEC = "hev1.1.6.L153.B0";

/**
 * The WebCodecs codec string for a frame: the exact `avc1.*` from the H.264 SPS
 * when present, else a sensible default for the frame's codec byte.
 */
export function codecForFrame(frame: Pick<VideoFrame, "codec" | "keyframe" | "data">): string {
  if (frame.codec === CODEC_HEVC) return DEFAULT_HEVC_CODEC;
  return (frame.keyframe && codecFromSps(frame.data)) || DEFAULT_H264_CODEC;
}

const hex = (byte: number) => byte.toString(16).padStart(2, "0").toUpperCase();

/**
 * The `avc1.PPCCLL` codec string from the SPS in an Annex B access unit
 * (profile, constraint flags, level), or null when there is no SPS.
 */
export function codecFromSps(data: Uint8Array): string | null {
  for (let i = 0; i + 3 < data.length; i++) {
    if (data[i] !== 0 || data[i + 1] !== 0) continue;
    let start: number;
    if (data[i + 2] === 1) start = i + 3;
    else if (data[i + 2] === 0 && data[i + 3] === 1) start = i + 4;
    else continue;
    if (start + 3 < data.length && (data[start] & 0x1f) === 7) {
      return `avc1.${hex(data[start + 1])}${hex(data[start + 2])}${hex(data[start + 3])}`;
    }
    i = start - 1;
  }
  return null;
}
