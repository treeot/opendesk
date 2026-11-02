// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Connection, type ConnectionStatus } from "./connection";
import { FakeCore, FakeSocket } from "./fakes";
import type { MonitorInfo } from "./types";

const target = { relay: "wss://relay.example/ws", target: "123456789", viewerId: "987654321", password: "pw", authCode: "" };

function setup(core = new FakeCore(), overrides: Partial<typeof target> = {}) {
  const statuses: ConnectionStatus[] = [];
  const frames: number[][] = [];
  const monitors: MonitorInfo[][] = [];
  const clipboard: ({ type: "text"; text: string } | { type: "image"; width: number; height: number; png: Uint8Array })[] = [];
  const ended: (string | null)[] = [];
  let clock = 0;
  const connection = new Connection(
    { ...target, ...overrides },
    { status: (s) => statuses.push(s), video: (f) => frames.push([...f]), audio: () => {}, monitors: (m) => monitors.push(m), clipboard: (t) => clipboard.push(t), file: () => {}, ended: (e) => ended.push(e) },
    { core, openSocket: (url) => new FakeSocket(url), now: () => clock },
  );
  return { core, connection, statuses, frames, monitors, clipboard, ended, advance: (ms: number) => { clock += ms; } };
}

beforeEach(() => {
  FakeSocket.all = [];
  vi.useFakeTimers();
});
afterEach(() => vi.useRealTimers());

describe("Connection", () => {
  it("sends the connect frame, relays events and reports video", () => {
    const { core, connection, statuses, frames, monitors } = setup();
    core.setup = (viewer) => {
      viewer.onReceive = ([first]) => first === 1
        ? [{ kind: "send", bytes: [2, 1] }, { kind: "authenticated" }]
        : [{ kind: "monitors", text: '[{"id":1,"name":"A","width":10,"height":10,"is_primary":true}]' }, { kind: "video", bytes: [9, 9] }];
    };
    connection.start();
    const socket = FakeSocket.all[0];
    expect(socket.url).toBe("wss://relay.example/ws");
    socket.open();
    expect(socket.binaryType).toBe("arraybuffer");
    expect(new TextDecoder().decode(new Uint8Array(socket.sent[0]))).toBe("connect 123456789 987654321");
    socket.message([1]);
    expect(socket.sent[1]).toEqual([2, 1]);
    socket.message([2]);
    expect(statuses).toEqual(["connecting", "waiting", "streaming"]);
    expect(monitors[0][0].name).toBe("A");
    expect(frames).toEqual([[9, 9]]);
    connection.sendInput(new Uint8Array([7]));
    expect(socket.sent.at(-1)).toEqual([0x12, 7]);
  });

  it("stops with the core's message when it says not to retry", () => {
    const { core, connection, ended } = setup();
    core.setup = (viewer) => { viewer.onReceive = () => [{ kind: "ended", text: "key confirmation failed" }]; };
    core.decide = () => ({ retry: false, delayMs: 0, message: "wrong password message" });
    connection.start();
    const socket = FakeSocket.all[0];
    socket.open();
    socket.message([1]);
    expect(ended).toEqual(["wrong password message"]);
    expect(socket.closeCode).toBe(1000);
    expect(core.viewers[0].freed).toBe(true);
    vi.advanceTimersByTime(60_000);
    expect(FakeSocket.all).toHaveLength(1);
  });

  it("retries a dropped session after the core's delay and counts the outage", () => {
    const { core, connection, statuses, ended, advance } = setup();
    core.setup = (viewer) => { viewer.onReceive = () => [{ kind: "video", bytes: [1] }]; };
    core.decide = (message) => ({ retry: true, delayMs: 500, message: `dropped (${message})` });
    connection.start();
    FakeSocket.all[0].open();
    FakeSocket.all[0].message([0]);
    FakeSocket.all[0].drop();
    expect(core.failures[0]).toEqual({ message: "connection closed during session", sessionStarted: true, attempt: 0, outageMs: 0 });
    expect(statuses.at(-1)).toBe("reconnecting");
    vi.advanceTimersByTime(499);
    expect(FakeSocket.all).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(FakeSocket.all).toHaveLength(2);
    // The relay is unreachable this time.
    advance(1000);
    FakeSocket.all[1].drop();
    expect(core.failures[1]).toMatchObject({ message: "could not reach the relay at wss://relay.example/ws", attempt: 1, outageMs: 1000 });
    expect(ended).toEqual([]);
  });

  it("keeps sending keepalives when background timers are throttled", () => {
    const { core, connection, advance } = setup();
    let due = 15_000;
    core.setup = (viewer) => {
      viewer.onTick = (now) => {
        if (now < due) return [];
        due = now + 15_000;
        return [{ kind: "send", bytes: [0x01, 0x4b] }];
      };
    };
    connection.start();
    const socket = FakeSocket.all[0];
    socket.open();
    // No timer runs (a hidden tab), but the host's own keepalives still arrive.
    for (let i = 0; i < 4; i++) {
      advance(15_000);
      socket.message([0]);
    }
    expect(socket.sent.filter((bytes) => bytes[1] === 0x4b)).toHaveLength(4);
  });

  it("says goodbye to the host on disconnect", () => {
    const { connection, ended } = setup();
    connection.start();
    const socket = FakeSocket.all[0];
    socket.open();
    connection.disconnect();
    expect(socket.sent.at(-1)).toEqual([0x01, 0xbb]);
    expect(socket.closeCode).toBe(1000);
    expect(ended).toEqual([null]);
    connection.disconnect();
    expect(ended).toEqual([null]);
  });

  it("rejects a bad ID before opening a socket", () => {
    const { connection, ended } = setup(new FakeCore(), { target: "12" });
    connection.start();
    expect(FakeSocket.all).toHaveLength(0);
    expect(ended).toEqual(["target ID must be 9 digits"]);
  });
});
