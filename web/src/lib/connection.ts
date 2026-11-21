/**
 * One viewer connection: the relay WebSocket, the wasm session, its timer,
 * and the desktop viewer's reconnect loop. Decisions (crypto, retry rules,
 * error wording) come from web-core; this file only moves bytes and time.
 */
import type { BrowseSignal, Core, FileSignal, HostFeature, MonitorInfo, Viewer } from "./types";

export type ConnectionStatus =
  /** Pairing and key exchange. */
  | "connecting"
  /** Authenticated; the host may be asking its user. */
  | "waiting"
  /** Video is arriving. */
  | "streaming"
  | "reconnecting";

export interface ConnectionTarget {
  relay: string;
  target: string;
  viewerId: string;
  password: string;
  authCode: string;
}

export interface ConnectionHandlers {
  status(status: ConnectionStatus): void;
  video(frame: Uint8Array): void;
  /** One Opus packet from the host and its channel count. */
  audio(payload: Uint8Array, channels: number): void;
  monitors(monitors: MonitorInfo[]): void;
  /** Inbound clipboard text from the host. */
  clipboard(content: { type: "text"; text: string } | { type: "image"; width: number; height: number; png: Uint8Array }): void;
  /** A file-transfer update (offer, progress, result). */
  file(signal: FileSignal): void;
  /** A folder listing or browse error from the host. */
  browse?(signal: BrowseSignal): void;
  /** A chat message from the person at the host. */
  chat?(text: string): void;
  /** The latest relay round trip in milliseconds. */
  roundTrip?(ms: number): void;
  /** The host offers an optional feature. */
  supports?(feature: HostFeature): void;
  /** The connection is over: `error` is null after the user disconnected. */
  ended(error: string | null): void;
  /** The host wants a 2FA code: "required" (none sent) or "invalid" (rejected). */
  twofa?(reason: "required" | "invalid"): void;
}

/** The WebSocket surface used here, so tests can fake it. */
export interface Socket {
  binaryType: BinaryType;
  readonly readyState: number;
  onopen: ((event: Event) => void) | null;
  onmessage: ((event: MessageEvent) => void) | null;
  onclose: ((event: CloseEvent) => void) | null;
  onerror: ((event: Event) => void) | null;
  send(data: Uint8Array<ArrayBuffer>): void;
  close(code?: number, reason?: string): void;
}

export interface Environment {
  core: Core;
  openSocket?: (url: string) => Socket;
  now?: () => number;
}

const OPEN = 1;
/**
 * How often session timers (keepalive, deadlines) run. Browsers throttle
 * timers in background tabs to about once a minute, longer than the host's
 * dead-peer limit, so every incoming message also runs them.
 */
const TICK_MS = 1000;
/** How often to measure the relay round trip once the session is live. */
export const PING_MS = 2000;

export class Connection {
  private viewer: Viewer | null = null;
  private socket: Socket | null = null;
  private ticker: ReturnType<typeof setInterval> | undefined;
  private retry: ReturnType<typeof setTimeout> | undefined;
  private stopped = false;
  private sessionStarted = false;
  private mediaReceived = false;
  private attempt = 0;
  private outageStart: number | null = null;
  private lastPing = -Infinity;
  private readonly openSocket: (url: string) => Socket;
  private readonly now: () => number;

  constructor(
    private readonly target: ConnectionTarget,
    private readonly handlers: ConnectionHandlers,
    private readonly env: Environment,
  ) {
    this.openSocket = env.openSocket ?? ((url) => new WebSocket(url));
    this.now = env.now ?? (() => performance.now());
  }

  start() {
    this.connect();
  }

  private connect() {
    if (this.stopped) return;
    this.retry = undefined;
    this.mediaReceived = false;
    this.handlers.status(this.attempt > 0 ? "reconnecting" : "connecting");
    const { relay, target, viewerId, password, authCode } = this.target;
    let frame: Uint8Array<ArrayBuffer>;
    let socket: Socket;
    try {
      frame = this.env.core.connectFrame(target, viewerId) as Uint8Array<ArrayBuffer>;
      socket = this.openSocket(relay);
    } catch (error) {
      this.finish(error instanceof Error ? error.message : String(error));
      return;
    }
    const viewer = this.env.core.createViewer(password, authCode);
    this.viewer = viewer;
    this.socket = socket;
    let opened = false;
    socket.binaryType = "arraybuffer";
    socket.onopen = () => {
      opened = true;
      socket.send(frame);
    };
    socket.onmessage = (event) => {
      if (this.socket !== socket || !(event.data instanceof ArrayBuffer)) return;
      const now = this.now();
      viewer.receive(new Uint8Array(event.data), now);
      // The host sends at least a keepalive every 15 s, so this keeps ours
      // going while the interval below is throttled.
      viewer.tick(now);
      this.drain();
    };
    // An error is always followed by a close.
    socket.onerror = () => {};
    socket.onclose = () => {
      if (this.socket !== socket) return;
      if (!opened) {
        this.failed(`could not reach the relay at ${relay}`);
        return;
      }
      viewer.closed();
      this.drain();
    };
    this.lastPing = -Infinity;
    this.ticker = setInterval(() => {
      if (this.viewer !== viewer) return;
      const now = this.now();
      viewer.tick(now);
      this.drain();
      // Nothing is sent before the session is authenticated.
      if (this.viewer === viewer && now - this.lastPing >= PING_MS) {
        const ping = viewer.ping(now);
        if (ping) {
          this.lastPing = now;
          this.send(ping);
        }
      }
    }, TICK_MS);
  }

  private drain() {
    const viewer = this.viewer;
    while (viewer && this.viewer === viewer) {
      const event = viewer.nextEvent();
      if (!event) return;
      const { kind } = event;
      try {
        if (kind === "send") this.send(event.takeBytes());
        else if (kind === "authenticated") this.handlers.status("waiting");
        else if (kind === "video") {
          if (!this.mediaReceived) {
            this.mediaReceived = true;
            this.sessionStarted = true;
            this.handlers.status("streaming");
          }
          this.handlers.video(event.takeBytes());
        } else if (kind === "audio") this.handlers.audio(event.takeBytes(), Number(event.text) || 2);
        else if (kind === "monitors") this.handlers.monitors(JSON.parse(event.text) as MonitorInfo[]);
        else if (kind === "clipboard-text") this.handlers.clipboard({ type: "text", text: event.text });
        else if (kind === "clipboard-image") {
          const [width, height] = event.text.split("x").map(Number);
          this.handlers.clipboard({ type: "image", width, height, png: event.takeBytes() });
        }
        else if (kind.startsWith("file-")) {
          const record = JSON.parse(event.text) as Omit<FileSignal, "kind" | "data">;
          const signal: FileSignal = { ...record, kind: kind as FileSignal["kind"] };
          if (kind === "file-received") signal.data = event.takeBytes();
          this.handlers.file(signal);
        }
        else if (kind === "browse") this.handlers.browse?.(JSON.parse(event.text) as BrowseSignal);
        else if (kind === "chat") this.handlers.chat?.(event.text);
        else if (kind === "rtt") this.handlers.roundTrip?.(Number(event.text));
        else if (kind === "supports") this.handlers.supports?.(event.text as HostFeature);
        else if (kind === "ended") {
          const message = event.text;
          event.free();
          this.failed(message);
          return;
        }
      } finally {
        if (kind !== "ended") event.free();
      }
    }
  }

  private send(bytes: Uint8Array | undefined) {
    if (bytes && this.socket?.readyState === OPEN) this.socket.send(bytes as Uint8Array<ArrayBuffer>);
  }

  /** The current attempt failed with `message`: retry or give up, like the desktop viewer. */
  private failed(message: string) {
    this.teardown();
    if (this.stopped) return;
    // A 2FA rejection ends the attempt and asks the viewer for a code through
    // a popup, rather than showing it as an error and retrying.
    const code = this.env.core.rejectionCode(message);
    if (code === "2fa_required" || code === "2fa_invalid") {
      this.handlers.twofa?.(code === "2fa_required" ? "required" : "invalid");
      this.finish(null);
      return;
    }
    if (this.mediaReceived) {
      this.attempt = 0;
      this.outageStart = null;
    }
    const now = this.now();
    this.outageStart ??= now;
    const step = this.env.core.afterFailure(message, this.sessionStarted, this.attempt, now - this.outageStart);
    const { retry, delayMs, message: text } = step;
    step.free();
    if (!retry) {
      this.finish(text);
      return;
    }
    console.info(text);
    this.attempt += 1;
    this.handlers.status("reconnecting");
    this.retry = setTimeout(() => this.connect(), delayMs);
  }

  private teardown() {
    clearInterval(this.ticker);
    this.ticker = undefined;
    const socket = this.socket;
    this.socket = null;
    if (socket && socket.readyState <= OPEN) socket.close(1000);
    this.viewer?.free();
    this.viewer = null;
  }

  private finish(error: string | null) {
    if (this.stopped) return;
    this.stopped = true;
    clearTimeout(this.retry);
    this.teardown();
    this.handlers.ended(error);
  }

  /** Tell the host goodbye and close. */
  disconnect() {
    if (this.stopped) return;
    // Sent before close(), so the browser flushes it first.
    this.send(this.viewer?.bye());
    this.finish(null);
  }

  sendInput(event: Uint8Array) {
    this.send(this.viewer?.input(event));
  }

  requestKeyframe() {
    this.send(this.viewer?.requestKeyframe());
  }

  selectMonitor(id: number) {
    this.send(this.viewer?.selectMonitor(id));
  }

  sendClipboard(text: string) {
    this.send(this.viewer?.clipboard(text));
  }

  sendClipboardImage(width: number, height: number, png: Uint8Array) {
    this.send(this.viewer?.clipboardImage(width, height, png));
  }

  sendFile(id: number, name: string, data: Uint8Array) {
    this.send(this.viewer?.sendFile(id, name, data));
  }

  acceptFile(id: number) {
    this.send(this.viewer?.acceptFile(id));
  }

  rejectFile(id: number) {
    this.send(this.viewer?.rejectFile(id));
  }

  /**
   * Send a file into host folder `dir`. The destination goes first on the
   * same ordered channel; the host applies it to the next offer.
   */
  sendFileTo(dir: string, id: number, name: string, data: Uint8Array) {
    const viewer = this.viewer;
    if (!viewer) return;
    this.send(viewer.setUploadDir(dir));
    this.send(viewer.sendFile(id, name, data));
  }

  /** Send a chat message; false if it could not be sent. */
  sendChat(text: string): boolean {
    const sealed = this.viewer?.chat(text);
    if (!sealed || this.socket?.readyState !== OPEN) return false;
    this.send(sealed);
    return true;
  }

  listDir(path: string) {
    this.send(this.viewer?.listDir(path));
  }

  pullFile(path: string) {
    this.send(this.viewer?.pullFile(path));
  }

  makeDir(path: string, name: string) {
    this.send(this.viewer?.makeDir(path, name));
  }

  deletePath(path: string) {
    this.send(this.viewer?.deletePath(path));
  }

  renamePath(path: string, name: string) {
    this.send(this.viewer?.renamePath(path, name));
  }

  secureAttention() {
    this.send(this.viewer?.secureAttention());
  }

  setMaxBitrate(bps: number) {
    this.send(this.viewer?.setMaxBitrate(bps));
  }

  setTextReadability(enabled: boolean) {
    this.send(this.viewer?.textReadability(enabled));
  }
}
