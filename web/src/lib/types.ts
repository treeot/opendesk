/**
 * The parts of the web-core wasm module the app uses, as interfaces so tests
 * can stand in for it. See crates/web-core/src/wasm.rs.
 */

export interface ViewerEvent {
  /**
   * "send", "authenticated", "video", "audio", "monitors", "clipboard",
   * "file-offer", "file-progress", "file-received", "file-sent", "file-error",
   * "browse", "chat", "rtt", "supports", "idle-warning" or "ended".
   */
  readonly kind: string;
  /**
   * Monitors JSON, clipboard text, the audio channel count, a file-transfer
   * or browse JSON record, chat text, a round trip in ms, a feature name,
   * the seconds left before an idle end, or the end message.
   */
  readonly text: string;
  takeBytes(): Uint8Array;
  free(): void;
}

export interface Viewer {
  receive(message: Uint8Array, nowMs: number): void;
  closed(): void;
  tick(nowMs: number): void;
  nextEvent(): ViewerEvent | undefined;
  input(event: Uint8Array): Uint8Array | undefined;
  clipboard(text: string): Uint8Array | undefined;
  clipboardImage(width: number, height: number, png: Uint8Array): Uint8Array | undefined;
  sendFile(id: number, name: string, data: Uint8Array): Uint8Array | undefined;
  acceptFile(id: number): Uint8Array | undefined;
  rejectFile(id: number): Uint8Array | undefined;
  chat(text: string): Uint8Array | undefined;
  listDir(path: string): Uint8Array | undefined;
  pullFile(path: string): Uint8Array | undefined;
  setUploadDir(dir: string): Uint8Array | undefined;
  makeDir(path: string, name: string): Uint8Array | undefined;
  deletePath(path: string): Uint8Array | undefined;
  renamePath(path: string, name: string): Uint8Array | undefined;
  ping(nowMs: number): Uint8Array | undefined;
  secureAttention(): Uint8Array | undefined;
  stillHere(): Uint8Array | undefined;
  setMaxBitrate(bps: number): Uint8Array | undefined;
  textReadability(enabled: boolean): Uint8Array | undefined;
  requestKeyframe(): Uint8Array | undefined;
  selectMonitor(id: number): Uint8Array | undefined;
  bye(): Uint8Array | undefined;
  free(): void;
}

export interface Step {
  readonly retry: boolean;
  readonly delayMs: number;
  readonly message: string;
  free(): void;
}

export interface Core {
  createViewer(password: string, authCode: string): Viewer;
  connectFrame(target: string, viewerId: string): Uint8Array;
  afterFailure(message: string, sessionStarted: boolean, attempt: number, outageMs: number): Step;
  /** The host's reject code inside a rejection error, e.g. "2fa_required". */
  rejectionCode(error: string): string | undefined;
}

/** A file-transfer update from web-core, decoded from a ViewerEvent. */
export interface FileSignal {
  kind: "file-offer" | "file-progress" | "file-received" | "file-sent" | "file-error";
  id: number;
  name?: string;
  size?: number;
  transferred?: number;
  total?: number;
  sending?: boolean;
  message?: string;
  /** The received file's bytes, set only for "file-received". */
  data?: Uint8Array;
}

export interface MonitorInfo {
  id: number;
  name: string;
  width: number;
  height: number;
  is_primary: boolean;
}

/** One entry of a host folder listing (protocol::browse::DirEntry). */
export interface DirEntry {
  name: string;
  is_dir: boolean;
  size: number;
  /** Unix seconds; 0 when unknown. */
  modified: number;
}

/** A remote file-manager answer from the host (protocol::browse::BrowseMsg). */
export type BrowseSignal =
  | { t: "dir_listing"; req: number; path: string; entries: DirEntry[] }
  | { t: "browse_error"; req: number; message: string };

/** Optional host features the session learns about. */
export type HostFeature = "secure-attention" | "recorded" | "relative-mouse" | "pen-touch" | "gamepad" | "keyboard-modes";
