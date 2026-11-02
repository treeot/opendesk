/** Test doubles for the wasm core and the WebSocket. */
import type { Socket } from "./connection";
import type { Core, Step, Viewer, ViewerEvent } from "./types";

type Queued = { kind: string; text?: string; bytes?: number[] };

export class FakeViewer implements Viewer {
  events: Queued[] = [];
  received: number[][] = [];
  closedCalls = 0;
  freed = false;
  /** What `receive` queues, per call. */
  onReceive: (message: Uint8Array) => Queued[] = () => [];
  /** What `tick` queues, given the time. */
  onTick: (now: number) => Queued[] = () => [];

  constructor(readonly password: string, readonly authCode: string) {}

  receive(message: Uint8Array) {
    this.received.push([...message]);
    this.events.push(...this.onReceive(message));
  }
  closed() {
    this.closedCalls += 1;
    this.events.push({ kind: "ended", text: "connection closed during session" });
  }
  tick(now: number) { this.events.push(...this.onTick(now)); }
  nextEvent(): ViewerEvent | undefined {
    const next = this.events.shift();
    if (!next) return undefined;
    return { kind: next.kind, text: next.text ?? "", takeBytes: () => new Uint8Array(next.bytes ?? []), free: () => {} };
  }
  input(event: Uint8Array) { return new Uint8Array([0x12, ...event]); }
  clipboard(text: string) { return new Uint8Array([0x13, ...new TextEncoder().encode(text)]); }
  clipboardImage(_width: number, _height: number, png: Uint8Array) { return new Uint8Array([0x14, ...png]); }
  sendFile(id: number, _name: string, _data: Uint8Array) { return new Uint8Array([0x13, 0x01, id & 0xff]); }
  acceptFile(id: number) { return new Uint8Array([0x13, 0x02, id & 0xff]); }
  rejectFile(id: number) { return new Uint8Array([0x13, 0x03, id & 0xff]); }
  requestKeyframe() { return new Uint8Array([0x01, 0xaa]); }
  selectMonitor(id: number) { return new Uint8Array([0x01, id]); }
  bye() { return new Uint8Array([0x01, 0xbb]); }
  free() { this.freed = true; }
}

export class FakeCore implements Core {
  viewers: FakeViewer[] = [];
  failures: { message: string; sessionStarted: boolean; attempt: number; outageMs: number }[] = [];
  /** Retry decision, given the failure. */
  decide: (message: string, sessionStarted: boolean) => { retry: boolean; delayMs: number; message: string } = (message) => ({ retry: false, delayMs: 0, message });
  setup: (viewer: FakeViewer) => void = () => {};

  createViewer(password: string, authCode: string) {
    const viewer = new FakeViewer(password, authCode);
    this.setup(viewer);
    this.viewers.push(viewer);
    return viewer;
  }
  connectFrame(target: string, viewerId: string) {
    if (!/^\d{9}$/.test(target)) throw new Error("target ID must be 9 digits");
    return new TextEncoder().encode(`connect ${target} ${viewerId}`);
  }
  afterFailure(message: string, sessionStarted: boolean, attempt: number, outageMs: number): Step {
    this.failures.push({ message, sessionStarted, attempt, outageMs });
    return { ...this.decide(message, sessionStarted), free: () => {} };
  }
  rejectionCode(error: string) {
    const prefix = "session rejected by host: ";
    return error.startsWith(prefix) ? error.slice(prefix.length) : undefined;
  }
}

export class FakeSocket implements Socket {
  static all: FakeSocket[] = [];
  binaryType: BinaryType = "blob";
  readyState = 0;
  sent: number[][] = [];
  closeCode: number | undefined;
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;

  constructor(readonly url: string) {
    FakeSocket.all.push(this);
  }
  send(data: Uint8Array) { this.sent.push([...data]); }
  close(code?: number) {
    this.closeCode = code;
    this.readyState = 3;
  }
  open() {
    this.readyState = 1;
    this.onopen?.(new Event("open"));
  }
  message(bytes: number[]) {
    this.onmessage?.(new MessageEvent("message", { data: new Uint8Array(bytes).buffer }));
  }
  drop() {
    this.readyState = 3;
    this.onclose?.(new Event("close") as CloseEvent);
  }
}
