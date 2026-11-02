/**
 * The parts of the web-core wasm module the app uses, as interfaces so tests
 * can stand in for it. See crates/web-core/src/wasm.rs.
 */

export interface ViewerEvent {
  /**
   * "send", "authenticated", "video", "audio", "monitors", "clipboard",
   * "file-offer", "file-progress", "file-received", "file-sent", "file-error"
   * or "ended".
   */
  readonly kind: string;
  /**
   * Monitors JSON, clipboard text, the audio channel count, a file-transfer
   * JSON record or the end message.
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
