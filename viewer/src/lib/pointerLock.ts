/**
 * Relative mouse mode for games and 3D apps: the canvas takes the pointer
 * (Pointer Lock) and the viewer sends raw movement instead of positions.
 */

/** Shown while the mouse is captured. */
export const RELEASE_HINT = "Mouse captured. Press Ctrl+Alt+Shift or Esc to release it.";

/** Ctrl+Alt+Shift, completed by pressing whichever of the three comes last. */
export function isReleaseChord(event: KeyboardEvent): boolean {
  return event.ctrlKey && event.altKey && event.shiftKey && (event.key === "Control" || event.key === "Alt" || event.key === "Shift");
}

/**
 * Whole-unit deltas from pointer movement. Browsers may report fractional
 * movementX/Y; the remainder carries over so slow motion isn't lost.
 */
export class DeltaAccumulator {
  private x = 0;
  private y = 0;

  take(movementX: number, movementY: number): { dx: number; dy: number } | null {
    if (Number.isFinite(movementX)) this.x += movementX;
    if (Number.isFinite(movementY)) this.y += movementY;
    const dx = Math.trunc(this.x) || 0;
    const dy = Math.trunc(this.y) || 0;
    if (dx === 0 && dy === 0) return null;
    this.x -= dx;
    this.y -= dy;
    return { dx, dy };
  }

  reset(): void {
    this.x = 0;
    this.y = 0;
  }
}

export interface PointerLockControl {
  /** Capture the pointer. Must run from a user gesture (a click). */
  request(): Promise<void>;
  release(): void;
  locked(): boolean;
  /** Release and stop listening. */
  dispose(): void;
}

type LockOptions = { unadjustedMovement?: boolean };
type Lockable = HTMLElement & { requestPointerLock(options?: LockOptions): Promise<void> | void };

/**
 * Pointer lock on `element`. `onChange` reports every lock and release,
 * including the browser's own (Esc, the window losing focus). Ctrl+Alt+Shift
 * releases too, since Esc may be wanted by the remote app.
 */
export function createPointerLock(element: HTMLElement, onChange: (locked: boolean) => void, doc: Document = document): PointerLockControl {
  const locked = () => doc.pointerLockElement === element;
  const release = () => { if (locked()) doc.exitPointerLock(); };
  const onLockChange = () => onChange(locked());
  const onLockError = () => onChange(false);
  const onKeyDown = (event: KeyboardEvent) => {
    if (locked() && isReleaseChord(event)) { event.preventDefault(); release(); }
  };
  const onBlur = () => release();
  doc.addEventListener("pointerlockchange", onLockChange);
  doc.addEventListener("pointerlockerror", onLockError);
  window.addEventListener("keydown", onKeyDown, true);
  window.addEventListener("blur", onBlur);

  const request = async () => {
    const target = element as Lockable;
    if (typeof target.requestPointerLock !== "function") throw new Error("This viewer can't capture the mouse.");
    element.focus();
    try {
      // Raw, unaccelerated deltas where supported (Chromium, WebView2).
      await target.requestPointerLock({ unadjustedMovement: true });
    } catch {
      await target.requestPointerLock();
    }
  };

  return {
    request,
    release,
    locked,
    dispose: () => {
      release();
      doc.removeEventListener("pointerlockchange", onLockChange);
      doc.removeEventListener("pointerlockerror", onLockError);
      window.removeEventListener("keydown", onKeyDown, true);
      window.removeEventListener("blur", onBlur);
    },
  };
}
