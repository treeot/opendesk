/**
 * Pen and touch contacts (protocol::input::PointerContact), sent instead of
 * mouse events when the host accepts them. Windows hosts inject real pen and
 * touch input; other hosts move their mouse with the primary contact.
 */

/** protocol::input::PointerPhase. */
export const PHASE = { hover: 0, down: 1, move: 2, up: 3, cancel: 4, leave: 5 } as const;
export type PointerPhase = (typeof PHASE)[keyof typeof PHASE];

/** protocol::input::PEN_BARREL and PEN_ERASER. */
export const PEN_BARREL = 1;
export const PEN_ERASER = 1 << 1;

export interface PointerContact {
  kind: "pointer";
  pointerKind: "pen" | "touch";
  phase: PointerPhase;
  id: number;
  x: number;
  y: number;
  /** 0..65535. */
  pressure: number;
  /** Degrees, -90..90. */
  tiltX: number;
  tiltY: number;
  buttons: number;
}

export function isPenOrTouch(event: PointerEvent): boolean {
  return event.pointerType === "pen" || event.pointerType === "touch";
}

const clamp = (value: number, min: number, max: number) => Math.min(max, Math.max(min, Number.isFinite(value) ? value : 0));

/** Build the contact for `event` at protocol position (x, y). */
export function pointerContact(event: PointerEvent, phase: PointerPhase, position: { x: number; y: number }): PointerContact {
  // DOM buttons: 2 is the pen barrel (secondary) button, 32 the eraser.
  const buttons = (event.buttons & 2 ? PEN_BARREL : 0) | (event.buttons & 32 ? PEN_ERASER : 0);
  // A lifted contact reports zero pressure; one with no sensor reports 0.5.
  const pressure = phase === PHASE.up || phase === PHASE.cancel ? 0 : event.pressure;
  return {
    kind: "pointer",
    pointerKind: event.pointerType === "pen" ? "pen" : "touch",
    phase,
    id: event.pointerId >>> 0,
    x: position.x,
    y: position.y,
    pressure: Math.round(clamp(pressure, 0, 1) * 65535),
    tiltX: Math.round(clamp(event.tiltX ?? 0, -90, 90)),
    tiltY: Math.round(clamp(event.tiltY ?? 0, -90, 90)),
    buttons,
  };
}

/** Write the contact after the 9-byte event header (tag 17, 28 bytes in all). */
export function writeContact(view: DataView, contact: PointerContact): void {
  view.setUint8(9, contact.pointerKind === "pen" ? 1 : 2);
  view.setUint8(10, contact.phase);
  view.setUint32(11, contact.id >>> 0, false);
  view.setInt32(15, contact.x, false);
  view.setInt32(19, contact.y, false);
  view.setUint16(23, contact.pressure, false);
  view.setInt8(25, contact.tiltX);
  view.setInt8(26, contact.tiltY);
  view.setUint8(27, contact.buttons);
}
