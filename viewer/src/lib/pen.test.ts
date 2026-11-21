// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { attachInput, encodeInputEvent } from "./input";
import { PEN_BARREL, PEN_ERASER, PHASE, pointerContact } from "./pen";

const pointerEvent = (fields: Partial<PointerEvent>) => ({ pointerId: 1, pointerType: "pen", buttons: 0, pressure: 0.5, tiltX: 0, tiltY: 0, clientX: 0, clientY: 0, preventDefault: () => {}, ...fields }) as PointerEvent;

describe("pointerContact", () => {
  it("scales pressure, clamps tilt and maps pen buttons", () => {
    const contact = pointerContact(pointerEvent({ pressure: 1, tiltX: -120, tiltY: 30.4, buttons: 2 | 32 }), PHASE.move, { x: 1, y: 2 });
    expect(contact).toEqual({ kind: "pointer", pointerKind: "pen", phase: PHASE.move, id: 1, x: 1, y: 2, pressure: 65535, tiltX: -90, tiltY: 30, buttons: PEN_BARREL | PEN_ERASER });
    expect(pointerContact(pointerEvent({ pointerType: "touch", pressure: 0.5 }), PHASE.up, { x: 0, y: 0 }).pressure).toBe(0);
  });

  it("encodes the protocol's 28-byte pointer event", () => {
    const encoded = encodeInputEvent({ kind: "pointer", pointerKind: "pen", phase: PHASE.move, id: 7, x: 100, y: 65535, pressure: 40000, tiltX: -45, tiltY: 30, buttons: PEN_BARREL }, 5);
    expect(Array.from(encoded)).toEqual([17, 0, 0, 0, 0, 0, 0, 0, 5, 1, 2, 0, 0, 0, 7, 0, 0, 0, 100, 0, 0, 0xff, 0xff, 0x9c, 0x40, 0xd3, 30, 1]);
  });
});

describe("attachInput pen and touch", () => {
  const setup = (enabled: boolean) => {
    const listeners = new Map<string, EventListener>();
    const canvas = {
      tabIndex: 0,
      width: 100,
      height: 100,
      addEventListener: (type: string, listener: EventListener) => listeners.set(type, listener),
      removeEventListener: () => {},
      focus: () => {},
      getBoundingClientRect: () => ({ left: 0, top: 0, width: 100, height: 100 }),
    } as unknown as HTMLCanvasElement;
    const sent: string[] = [];
    const send = (bytes: Uint8Array) => {
      sent.push(bytes[0] === 17 ? `pointer ${bytes[9]}/${bytes[10]}` : bytes[0] === 1 ? "move" : bytes[0] === 2 ? `button ${bytes[10] ? "down" : "up"}` : `other ${bytes[0]}`);
      return Promise.resolve();
    };
    attachInput(canvas, send, { penTouch: () => enabled });
    const fire = (type: string, fields: Partial<PointerEvent> = {}) => listeners.get(type)!(pointerEvent({ clientX: 50, clientY: 50, ...fields }));
    const settle = () => new Promise((resolve) => setTimeout(resolve, 0));
    return { fire, sent, settle, listeners };
  };

  it("sends a touch stroke as contacts and drops the compatibility mouse events", async () => {
    const { fire, sent, settle } = setup(true);
    fire("pointerdown", { pointerType: "touch", pointerId: 3 });
    fire("mousedown", { button: 0 });
    fire("pointermove", { pointerType: "touch", pointerId: 3, clientX: 500 });
    fire("mousemove");
    fire("pointerup", { pointerType: "touch", pointerId: 3 });
    fire("mouseup", { button: 0 });
    await settle();
    expect(sent).toEqual(["pointer 2/1", "pointer 2/2", "pointer 2/3"]);
  });

  it("hovers a pen and cancels held contacts when the window loses focus", async () => {
    const { fire, sent, settle } = setup(true);
    fire("pointermove", { pointerId: 9 });
    fire("pointerdown", { pointerId: 9 });
    window.dispatchEvent(new Event("blur"));
    // A touch move that never went down is not ours.
    fire("pointermove", { pointerType: "touch", pointerId: 4 });
    await settle();
    expect(sent).toEqual(["pointer 1/0", "pointer 1/1", "pointer 1/4"]);
  });

  it("leaves pen and touch to the mouse path when the host lacks support", async () => {
    const { fire, sent, settle } = setup(false);
    fire("pointerdown", { pointerType: "touch" });
    fire("mousedown", { button: 0 });
    await settle();
    expect(sent).toEqual(["button down"]);
  });
});
