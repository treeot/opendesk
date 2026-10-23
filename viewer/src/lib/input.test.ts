// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { attachInput, containedRect, encodeInputEvent, mapPointer } from "./input";

describe("encodeInputEvent", () => {
  it("matches the protocol mouse-move wire bytes exactly", () => {
    const encoded = encodeInputEvent({ kind: "mouseMove", x: 30000, y: 45000 }, 123);
    expect(Array.from(encoded)).toEqual([
      0x01,
      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7b,
      0x00, 0x00, 0x75, 0x30,
      0x00, 0x00, 0xaf, 0xc8,
    ]);
  });
});

describe("pointer mapping with object-fit: contain", () => {
  // A 1000x500 element showing a 1920x1080 frame: scale 500/1080, so the
  // frame is ~888.9px wide and centred with ~55.6px pillar bars each side.
  const element = { left: 100, top: 50, width: 1000, height: 500 };

  it("computes the drawn rect inside pillarbox bars", () => {
    const drawn = containedRect(element, 1920, 1080);
    expect(drawn.height).toBe(500);
    expect(drawn.width).toBeCloseTo(888.89, 1);
    expect(drawn.left).toBeCloseTo(155.56, 1);
    expect(drawn.top).toBe(50);
  });

  it("computes the drawn rect inside letterbox bars", () => {
    const drawn = containedRect({ left: 0, top: 0, width: 400, height: 400 }, 800, 400);
    expect(drawn).toEqual({ left: 0, top: 100, width: 400, height: 200 });
  });

  it("maps the drawn image corners and centre to the remote range", () => {
    const drawn = containedRect(element, 1920, 1080);
    expect(mapPointer(drawn.left, drawn.top, element, 1920, 1080)).toEqual({ x: 0, y: 0 });
    expect(mapPointer(drawn.left + drawn.width, drawn.top + drawn.height, element, 1920, 1080)).toEqual({ x: 65535, y: 65535 });
    expect(mapPointer(600, 300, element, 1920, 1080)).toEqual({ x: 32768, y: 32768 });
  });

  it("ignores positions over the bars", () => {
    expect(mapPointer(120, 300, element, 1920, 1080)).toBeNull();
    expect(mapPointer(1080, 300, element, 1920, 1080)).toBeNull();
    expect(mapPointer(150, 50, { left: 0, top: 0, width: 400, height: 400 }, 800, 400)).toBeNull();
  });

  it("uses the whole element before the first frame arrives", () => {
    expect(mapPointer(50, 25, { left: 0, top: 0, width: 100, height: 50 }, 0, 0)).toEqual({ x: 32768, y: 32768 });
  });

  it("forwards a right-click as down+up and suppresses the native menu", async () => {
    const listeners = new Map<string, EventListener>();
    const canvas = {
      tabIndex: 0, width: 800, height: 400,
      addEventListener: (type: string, listener: EventListener) => listeners.set(type, listener),
      removeEventListener: () => {},
      focus: () => {},
      getBoundingClientRect: () => ({ left: 0, top: 0, width: 800, height: 400 }),
    } as unknown as HTMLCanvasElement;
    const sent: number[][] = [];
    attachInput(canvas, (bytes) => { sent.push(Array.from(bytes)); return Promise.resolve(); });
    listeners.get("mousedown")!({ button: 2, clientX: 400, clientY: 200 } as MouseEvent);
    let defaulted = true;
    listeners.get("contextmenu")!({ preventDefault: () => { defaulted = false; } } as unknown as Event);
    listeners.get("mouseup")!({ button: 2, clientX: 400, clientY: 200 } as MouseEvent);
    await new Promise((resolve) => setTimeout(resolve, 0));
    // Both are mouseButton (kind 2), right button (2), down then up.
    expect(sent.map((bytes) => [bytes[0], bytes[9], bytes[10]])).toEqual([[2, 2, 1], [2, 2, 0]]);
    expect(defaulted).toBe(false);
  });

  it("releases held buttons and keys when the window loses focus", async () => {
    const listeners = new Map<string, EventListener>();
    const windowListeners = new Map<string, EventListener>();
    const canvas = {
      tabIndex: 0, width: 800, height: 400,
      addEventListener: (type: string, listener: EventListener) => listeners.set(type, listener),
      removeEventListener: () => {},
      focus: () => {},
      getBoundingClientRect: () => ({ left: 0, top: 0, width: 800, height: 400 }),
    } as unknown as HTMLCanvasElement;
    const addWin = vi.spyOn(window, "addEventListener").mockImplementation((type, listener) => windowListeners.set(type, listener as EventListener));
    const sent: number[][] = [];
    attachInput(canvas, (bytes) => { sent.push(Array.from(bytes)); return Promise.resolve(); });
    // Hold left button and the "a" key down, then blur.
    listeners.get("mousedown")!({ button: 0, clientX: 400, clientY: 200 } as MouseEvent);
    listeners.get("keydown")!({ preventDefault: () => {}, key: "a", shiftKey: false, ctrlKey: false, altKey: false, metaKey: false } as unknown as KeyboardEvent);
    windowListeners.get("blur")!(new Event("blur"));
    await new Promise((resolve) => setTimeout(resolve, 0));
    // Two presses, then two releases: mouseButton-up (kind 2) and char-up (kind 5).
    const kinds = sent.map((bytes) => [bytes[0], bytes[bytes[0] === 2 ? 10 : 13]]);
    expect(kinds).toEqual([[2, 1], [5, 1], [2, 0], [5, 0]]);
    addWin.mockRestore();
  });

  it("releases a held button when the release happens outside the canvas", async () => {
    const listeners = new Map<string, EventListener>();
    const windowListeners = new Map<string, EventListener>();
    const canvas = {
      tabIndex: 0, width: 800, height: 400,
      addEventListener: (type: string, listener: EventListener) => listeners.set(type, listener),
      removeEventListener: () => {},
      focus: () => {},
      getBoundingClientRect: () => ({ left: 0, top: 0, width: 800, height: 400 }),
    } as unknown as HTMLCanvasElement;
    const addWin = vi.spyOn(window, "addEventListener").mockImplementation((type, listener) => windowListeners.set(type, listener as EventListener));
    const sent: number[][] = [];
    attachInput(canvas, (bytes) => { sent.push(Array.from(bytes)); return Promise.resolve(); });
    listeners.get("mousedown")!({ button: 0, clientX: 400, clientY: 200 } as MouseEvent);
    // Release lands on the window, not the canvas (dragged over the toolbar).
    windowListeners.get("mouseup")!({ button: 0, clientX: 10, clientY: 10 } as unknown as Event);
    // A second window mouseup for the same button must not double-send.
    windowListeners.get("mouseup")!({ button: 0, clientX: 10, clientY: 10 } as unknown as Event);
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(sent.map((bytes) => [bytes[0], bytes[10]])).toEqual([[2, 1], [2, 0]]);
    addWin.mockRestore();
  });

  it("drops canvas clicks in the bars but always sends releases", async () => {
    const listeners = new Map<string, EventListener>();
    const canvas = {
      tabIndex: 0, width: 800, height: 400,
      addEventListener: (type: string, listener: EventListener) => listeners.set(type, listener),
      removeEventListener: () => {},
      focus: () => {},
      getBoundingClientRect: () => ({ left: 0, top: 0, width: 400, height: 400 }),
    } as unknown as HTMLCanvasElement;
    const sent: number[][] = [];
    attachInput(canvas, (bytes) => { sent.push(Array.from(bytes)); return Promise.resolve(); });
    listeners.get("mousedown")!({ button: 0, clientX: 200, clientY: 20 } as MouseEvent);
    listeners.get("mousemove")!({ clientX: 200, clientY: 380 } as MouseEvent);
    listeners.get("mouseup")!({ button: 0, clientX: 200, clientY: 20 } as MouseEvent);
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(sent.map((bytes) => bytes[0])).toEqual([2]);
    expect(sent[0][10]).toBe(0);
  });
});

describe("attachInput", () => {
  it("sends events in capture order when sends resolve out of order", async () => {
    const listeners = new Map<string, EventListener>();
    const canvas = {
      tabIndex: -1,
      addEventListener: (type: string, listener: EventListener) => listeners.set(type, listener),
      removeEventListener: () => {},
      focus: () => {},
      getBoundingClientRect: () => ({ left: 0, top: 0, width: 100, height: 100 }),
    } as unknown as HTMLCanvasElement;
    const sent: number[] = [];
    let resolveFirst!: () => void;
    const firstSend = new Promise<void>((resolve) => {
      resolveFirst = resolve;
    });
    let sendCalls = 0;
    const send = () => {
      sendCalls += 1;
      sent.push(sendCalls);
      return sendCalls === 1 ? firstSend : Promise.resolve();
    };

    attachInput(canvas, send);
    const keydown = listeners.get("keydown")!;
    const keyup = listeners.get("keyup")!;
    keydown({ preventDefault: () => {}, key: "a", keyCode: 65, shiftKey: false, ctrlKey: false, altKey: false, metaKey: false } as unknown as KeyboardEvent);
    keyup({ preventDefault: () => {}, key: "a", keyCode: 65, shiftKey: false, ctrlKey: false, altKey: false, metaKey: false } as unknown as KeyboardEvent);
    keydown({ preventDefault: () => {}, key: "b", keyCode: 66, shiftKey: false, ctrlKey: false, altKey: false, metaKey: false } as unknown as KeyboardEvent);

    await Promise.resolve();
    expect(sendCalls).toBe(1);
    expect(sent).toEqual([1]);
    resolveFirst();
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(sent).toEqual([1, 2, 3]);
  });

  it("encodes the actual printable character and modifier state", async () => {
    const listeners = new Map<string, EventListener>();
    const canvas = {
      tabIndex: 0,
      addEventListener: (type: string, listener: EventListener) => listeners.set(type, listener),
      removeEventListener: () => {},
      focus: () => {},
      getBoundingClientRect: () => ({ left: 0, top: 0, width: 100, height: 100 }),
    } as unknown as HTMLCanvasElement;
    const send = vi.fn(() => Promise.resolve());
    attachInput(canvas, send);
    listeners.get("keydown")!({ preventDefault: () => {}, key: "!", keyCode: 49, shiftKey: true, ctrlKey: true, altKey: false, metaKey: false } as unknown as KeyboardEvent);
    await Promise.resolve();
    const bytes = (send.mock.calls[0] as unknown[])[0] as Uint8Array;
    expect(bytes[0]).toBe(5);
    expect(new DataView(bytes.buffer).getUint32(9)).toBe("!".codePointAt(0));
    expect(bytes[14]).toBe(0b0011);
  });
  describe("mouse-move coalescing", () => {
    const setup = () => {
      const listeners = new Map<string, EventListener>();
      const canvas = {
        tabIndex: 0,
        addEventListener: (type: string, listener: EventListener) => listeners.set(type, listener),
        removeEventListener: () => {},
        focus: () => {},
        getBoundingClientRect: () => ({ left: 0, top: 0, width: 65535, height: 65535 }),
      } as unknown as HTMLCanvasElement;
      const sent: string[] = [];
      let release!: () => void;
      const blocked = new Promise<void>((resolve) => { release = resolve; });
      const send = (bytes: Uint8Array) => {
        const view = new DataView(bytes.buffer);
        sent.push(bytes[0] === 1 ? `move ${view.getInt32(9)}` : bytes[0] === 2 ? `button ${bytes[10] ? "down" : "up"}` : `other ${bytes[0]}`);
        return sent.length === 1 ? blocked : Promise.resolve();
      };
      attachInput(canvas, send);
      const move = (x: number) => listeners.get("mousemove")!({ clientX: x, clientY: 0 } as MouseEvent);
      const down = () => listeners.get("mousedown")!({ button: 0, clientX: 0, clientY: 0 } as MouseEvent);
      const flush = async () => { release(); await new Promise((resolve) => setTimeout(resolve, 0)); };
      return { sent, move, down, flush };
    };

    it("sends only the latest move while the backend is busy", async () => {
      const { sent, move, flush } = setup();
      move(1);
      await Promise.resolve();
      for (let x = 2; x <= 50; x += 1) move(x);
      await flush();
      expect(sent).toEqual(["move 1", "move 50"]);
    });

    it("never moves a later position ahead of a click", async () => {
      const { sent, move, down, flush } = setup();
      move(1);
      await Promise.resolve();
      move(2);
      move(3);
      down();
      move(4);
      move(5);
      await flush();
      expect(sent).toEqual(["move 1", "move 3", "button down", "move 5"]);
    });
  });
});
