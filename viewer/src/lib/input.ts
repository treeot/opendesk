export type InputEvent =
  | { kind: "mouseMove"; x: number; y: number }
  | { kind: "mouseButton"; button: number; down: boolean }
  | { kind: "scroll"; dx: number; dy: number }
  | { kind: "key"; code: number; down: boolean; modifiers: number }
  | { kind: "char"; ch: string; down: boolean; modifiers: number };

/** Encode protocol::input::InputEvent using its big-endian wire layout. */
export function encodeInputEvent(ev: InputEvent, tsUs: number): Uint8Array {
  let length: number;
  switch (ev.kind) {
    case "mouseMove":
      length = 17;
      break;
    case "mouseButton":
      length = 11;
      break;
    case "scroll":
      length = 17;
      break;
    case "key":
    case "char":
      length = 15;
      break;
  }

  const bytes = new Uint8Array(length);
  const view = new DataView(bytes.buffer);
  view.setBigUint64(1, BigInt(tsUs), false);

  switch (ev.kind) {
    case "mouseMove":
      view.setUint8(0, 1);
      view.setInt32(9, ev.x, false);
      view.setInt32(13, ev.y, false);
      break;
    case "mouseButton":
      view.setUint8(0, 2);
      view.setUint8(9, ev.button);
      view.setUint8(10, ev.down ? 1 : 0);
      break;
    case "scroll":
      view.setUint8(0, 3);
      view.setInt32(9, ev.dx, false);
      view.setInt32(13, ev.dy, false);
      break;
    case "key":
      view.setUint8(0, 4);
      view.setUint32(9, ev.code, false);
      view.setUint8(13, ev.down ? 1 : 0);
      view.setUint8(14, ev.modifiers);
      break;
    case "char":
      if ([...ev.ch].length !== 1) throw new Error("character input must be one code point");
      view.setUint8(0, 5);
      view.setUint32(9, ev.ch.codePointAt(0)!, false);
      view.setUint8(13, ev.down ? 1 : 0);
      view.setUint8(14, ev.modifiers);
      break;
  }
  return bytes;
}

function timestampUs(): number {
  return Math.floor(Date.now() * 1000);
}

export interface ClientRect { left: number; top: number; width: number; height: number }

/**
 * The rectangle the frame actually occupies inside an element styled with
 * `object-fit: contain`: the frame is scaled to fit and centred, leaving bars
 * on two sides when the aspect ratios differ. Without a frame size yet, the
 * whole element is used.
 */
export function containedRect(element: ClientRect, contentWidth: number, contentHeight: number): ClientRect {
  if (!(contentWidth > 0 && contentHeight > 0) || element.width <= 0 || element.height <= 0) return element;
  const scale = Math.min(element.width / contentWidth, element.height / contentHeight);
  const width = contentWidth * scale;
  const height = contentHeight * scale;
  return {
    left: element.left + (element.width - width) / 2,
    top: element.top + (element.height - height) / 2,
    width,
    height,
  };
}

/**
 * Map a client-space pointer position to the protocol's 0..65535 remote
 * coordinates. Returns null when the pointer is over a letterbox bar.
 */
export function mapPointer(clientX: number, clientY: number, element: ClientRect, contentWidth: number, contentHeight: number): { x: number; y: number } | null {
  const drawn = containedRect(element, contentWidth, contentHeight);
  if (drawn.width <= 0 || drawn.height <= 0) return null;
  const fx = (clientX - drawn.left) / drawn.width;
  const fy = (clientY - drawn.top) / drawn.height;
  if (fx < 0 || fx > 1 || fy < 0 || fy > 1) return null;
  return { x: Math.round(fx * 65535), y: Math.round(fy * 65535) };
}

function modifiers(event: KeyboardEvent): number {
  return (
    (event.shiftKey ? 1 : 0) |
    (event.ctrlKey ? 1 << 1 : 0) |
    (event.altKey ? 1 << 2 : 0) |
    (event.metaKey ? 1 << 3 : 0)
  );
}

function button(button: number): number | null {
  if (button === 0) return 1; // Left
  if (button === 2) return 2; // Right
  if (button === 1) return 3; // Middle
  return null;
}

function namedKeyCode(key: string): number | null {
  const codes: Record<string, number> = {
    Backspace: 8,
    Tab: 9,
    Enter: 13,
    CapsLock: 20,
    Escape: 27,
    PageUp: 33,
    PageDown: 34,
    End: 35,
    Home: 36,
    ArrowLeft: 37,
    ArrowUp: 38,
    ArrowRight: 39,
    ArrowDown: 40,
    Delete: 46,
  };
  if (key in codes) return codes[key];
  const functionKey = /^F([1-9]|1[0-2])$/.exec(key);
  return functionKey ? 111 + Number(functionKey[1]) : null;
}

function keyCode(event: KeyboardEvent): number | null {
  const key = event.key || "";
  // A single printable code point preserves punctuation, shifted symbols, and
  // non-ASCII input without relying on a keyboard-layout-specific key table.
  if ([...key].length === 1 && !/^\p{Cc}|\p{Cf}$/u.test(key)) {
    return key.codePointAt(0) ?? null;
  }
  return namedKeyCode(key) ?? (event.keyCode >>> 0);
}

function isModifierKey(key: string): boolean {
  return key === "Shift" || key === "Control" || key === "Alt" || key === "Meta";
}

/**
 * Capture canvas input and send raw InputEvent bytes in FIFO order. Mouse
 * moves that queue up while the backend is busy collapse into the latest
 * position, but never past a click, scroll, or key event.
 */
export function attachInput(
  canvas: HTMLCanvasElement,
  send: (bytes: Uint8Array) => Promise<void>,
): () => void {
  if (canvas.tabIndex < 0) canvas.tabIndex = 0;

  let sendChain = Promise.resolve();
  // The queued, not-yet-sent move; later moves overwrite it in place.
  let pendingMove: { event: InputEvent; tsUs: number } | null = null;
  const chain = (next: () => Promise<void>): void => {
    sendChain = sendChain
      .then(next)
      .catch((error: unknown) => {
        console.error("Failed to send input event", error);
      });
  };
  const enqueue = (event: InputEvent): void => {
    pendingMove = null;
    const bytes = encodeInputEvent(event, timestampUs());
    chain(() => send(bytes));
  };

  const enqueueMove = (event: InputEvent): void => {
    if (pendingMove) {
      pendingMove.event = event;
      pendingMove.tsUs = timestampUs();
      return;
    }
    const slot = { event, tsUs: timestampUs() };
    pendingMove = slot;
    chain(() => {
      if (pendingMove === slot) pendingMove = null;
      return send(encodeInputEvent(slot.event, slot.tsUs));
    });
  };

  const pointer = (event: MouseEvent) => mapPointer(event.clientX, event.clientY, canvas.getBoundingClientRect(), canvas.width, canvas.height);

  // Buttons and keys held right now, so focus loss can release them: without
  // this, a button or key still down when the window loses focus (Cmd-Tab, a
  // click elsewhere) never gets its release and stays stuck down on the host.
  const buttonsDown = new Set<number>();
  const keysDown = new Map<string, InputEvent>();

  const onMouseMove = (event: MouseEvent): void => {
    const position = pointer(event);
    if (position) enqueueMove({ kind: "mouseMove", ...position });
  };

  // Presses over the letterbox bars are ignored; releases always go through
  // so a drag that ends over a bar never leaves a button held remotely.
  const onMouseDown = (event: MouseEvent): void => {
    canvas.focus();
    if (!pointer(event)) return;
    const mapped = button(event.button);
    if (mapped !== null) { buttonsDown.add(mapped); enqueue({ kind: "mouseButton", button: mapped, down: true }); }
  };

  const onMouseUp = (event: MouseEvent): void => {
    const mapped = button(event.button);
    if (mapped !== null) { buttonsDown.delete(mapped); enqueue({ kind: "mouseButton", button: mapped, down: false }); }
  };

  const onWheel = (event: WheelEvent): void => {
    // Keep the wheel on the remote; don't let it scroll or zoom the viewer page.
    event.preventDefault();
    if (!pointer(event)) return;
    enqueue({ kind: "scroll", dx: Math.trunc(event.deltaX), dy: Math.trunc(event.deltaY) });
  };

  // Suppress the webview's own context menu so a right-click's mousedown and
  // mouseup both reach the canvas and forward to the host; otherwise the native
  // menu swallows the release and the remote right-click never completes.
  const onContextMenu = (event: MouseEvent): void => {
    event.preventDefault();
  };

  // The event to send for a key press/release, and a stable id to track it by.
  const keyEvent = (event: KeyboardEvent, down: boolean): { id: string; ev: InputEvent } | null => {
    if (isModifierKey(event.key)) return null;
    const code = keyCode(event);
    if (code === null) return null;
    const key = event.key || "";
    return key.length > 0 && [...key].length === 1
      ? { id: `c:${key}`, ev: { kind: "char", ch: key, down, modifiers: modifiers(event) } }
      : { id: `k:${code}`, ev: { kind: "key", code, down, modifiers: modifiers(event) } };
  };

  const onKeyDown = (event: KeyboardEvent): void => {
    event.preventDefault();
    const built = keyEvent(event, true);
    if (!built) return;
    const release = keyEvent(event, false);
    if (release) keysDown.set(built.id, release.ev);
    enqueue(built.ev);
  };

  const onKeyUp = (event: KeyboardEvent): void => {
    event.preventDefault();
    const built = keyEvent(event, false);
    if (!built) return;
    keysDown.delete(built.id);
    enqueue(built.ev);
  };

  // A release outside the canvas (over the toolbar, or past the window edge)
  // never reaches the canvas handler, so catch it on the window. The guard both
  // dedupes the canvas release that bubbles here and ignores presses we never
  // started (e.g. a press that began over a letterbox bar).
  const onWindowMouseUp = (event: MouseEvent): void => {
    const mapped = button(event.button);
    if (mapped !== null && buttonsDown.has(mapped)) { buttonsDown.delete(mapped); enqueue({ kind: "mouseButton", button: mapped, down: false }); }
  };

  // Release everything still held when the window loses focus.
  const onBlur = (): void => {
    for (const mapped of buttonsDown) enqueue({ kind: "mouseButton", button: mapped, down: false });
    buttonsDown.clear();
    for (const release of keysDown.values()) enqueue(release);
    keysDown.clear();
  };

  canvas.addEventListener("mousemove", onMouseMove);
  canvas.addEventListener("mousedown", onMouseDown);
  canvas.addEventListener("mouseup", onMouseUp);
  canvas.addEventListener("wheel", onWheel, { passive: false });
  canvas.addEventListener("contextmenu", onContextMenu);
  canvas.addEventListener("keydown", onKeyDown);
  canvas.addEventListener("keyup", onKeyUp);
  window.addEventListener("mouseup", onWindowMouseUp);
  window.addEventListener("blur", onBlur);

  return () => {
    canvas.removeEventListener("mousemove", onMouseMove);
    canvas.removeEventListener("mousedown", onMouseDown);
    canvas.removeEventListener("mouseup", onMouseUp);
    canvas.removeEventListener("wheel", onWheel);
    canvas.removeEventListener("contextmenu", onContextMenu);
    canvas.removeEventListener("keydown", onKeyDown);
    canvas.removeEventListener("keyup", onKeyUp);
    window.removeEventListener("mouseup", onWindowMouseUp);
    window.removeEventListener("blur", onBlur);
  };
}
