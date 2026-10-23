/**
 * Wires the backend's binary frame messages into a WebCodecs VideoDecoder,
 * rendering decoded frames to `canvas`. The host encodes H.264 and the frames
 * arrive already opened (AEAD) by the Rust backend; see `video.ts` for the
 * message layout and the drop policy.
 *
 * The caller MUST invoke `close` on teardown so the VideoDecoder is released
 * rather than leaked across session unmount/remount.
 */
import { codecForFrame, DEFAULT_H264_CODEC, DEFAULT_HEVC_CODEC, FramePolicy, parseFrame, RateLimiter } from "./video";

/** Whether this browser/webview can decode HEVC, probed via WebCodecs. */
export async function supportsHevcDecode(): Promise<boolean> {
  try {
    if (typeof VideoDecoder === "undefined" || !VideoDecoder.isConfigSupported) return false;
    const support = await VideoDecoder.isConfigSupported({ codec: DEFAULT_HEVC_CODEC });
    return support.supported === true;
  } catch {
    return false;
  }
}

export interface Decoder {
  /** Feed one frame message (header plus access unit) from the backend. */
  push: (message: ArrayBuffer | Uint8Array) => void;
  close: () => void;
}

export interface DecoderOptions {
  /** Ask the host for a keyframe; called at most once per 500 ms. */
  requestKeyframe: () => void;
  /** Clock for rate limiting, in ms. Defaults to `performance.now`. */
  now?: () => number;
}

/** Assigning a canvas size clears it, so only touch it when the size changes. */
export function sizeCanvas(canvas: HTMLCanvasElement, width: number, height: number) {
  if (canvas.width !== width) canvas.width = width;
  if (canvas.height !== height) canvas.height = height;
}

export function decoderConfig(codec: string, hardware: boolean): VideoDecoderConfig {
  return hardware
    ? { codec, optimizeForLatency: true, hardwareAcceleration: "prefer-hardware" }
    : { codec, optimizeForLatency: true };
}

export function makeDecoder(canvas: HTMLCanvasElement, options: DecoderOptions): Decoder {
  // Opaque and desynchronized: no alpha blending with the page, and frames
  // can reach the screen without waiting on the compositor.
  const ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
  const now = options.now ?? (() => performance.now());
  const policy = new FramePolicy();
  const limiter = new RateLimiter();
  let codec = DEFAULT_H264_CODEC;
  // Dropped if the decoder fails before producing a frame with it.
  let hardware = true;
  let closed = false;
  let produced = false;

  const requestKeyframe = () => {
    if (limiter.tryAcquire(now())) options.requestKeyframe();
  };

  const configure = (target: VideoDecoder) => {
    try {
      target.configure(decoderConfig(codec, hardware));
    } catch (error) {
      if (!hardware) throw error;
      hardware = false;
      target.configure(decoderConfig(codec, false));
    }
  };

  const create = (): VideoDecoder => {
    produced = false;
    const created: VideoDecoder = new VideoDecoder({
      output: (frame) => {
        if (closed || created !== decoder) { frame.close(); return; }
        produced = true;
        sizeCanvas(canvas, frame.displayWidth, frame.displayHeight);
        ctx?.drawImage(frame, 0, 0);
        frame.close();
      },
      error: (error) => {
        if (closed || created !== decoder) return;
        recover(error);
      },
    });
    try {
      configure(created);
    } catch (error) {
      console.error("video decoder configure failed:", error);
    }
    return created;
  };

  // A decode error closes the WebCodecs decoder for good. Replace it and
  // restart from the next keyframe instead of freezing on the last image.
  const recover = (error: unknown) => {
    console.error("video decode error:", error);
    if (hardware && !produced) hardware = false;
    const failed = decoder;
    decoder = create();
    if (failed.state !== "closed") {
      try { failed.close(); } catch { /* already closing */ }
    }
    policy.waitForKeyframe();
    requestKeyframe();
  };

  let decoder = create();

  return {
    push: (message) => {
      if (closed) return;
      const frame = parseFrame(message);
      if (!frame) return;
      sizeCanvas(canvas, frame.width, frame.height);
      const queued = decoder.state === "configured" ? decoder.decodeQueueSize : 0;
      const action = policy.next(frame, queued);
      if (!action.decode) {
        if (action.requestKeyframe) requestKeyframe();
        return;
      }
      try {
        // Re-read the codec on keyframes, which carry the parameter sets: the
        // exact H.264 profile from the SPS, or HEVC when the frame says so.
        const want = frame.keyframe ? codecForFrame(frame) : codec;
        const codecChanged = want !== codec;
        codec = want;
        if (action.flush) decoder.reset();
        if (action.flush || codecChanged || decoder.state !== "configured") configure(decoder);
        decoder.decode(
          new EncodedVideoChunk({
            type: frame.keyframe ? "key" : "delta",
            timestamp: frame.timestampUs,
            data: frame.data,
          }),
        );
      } catch (error) {
        recover(error);
      }
    },
    close: () => {
      closed = true;
      if (decoder.state !== "closed") {
        try {
          decoder.close();
        } catch {
          // already closing/closed — ignore
        }
      }
    },
  };
}
