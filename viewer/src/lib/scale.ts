/**
 * How the remote screen is drawn in the session window, remembered per
 * device. Pure helpers; Session applies them.
 */

export type ScaleMode = "fit" | "actual" | "stretch";

export const SCALE_MODES: { mode: ScaleMode; label: string }[] = [
  { mode: "fit", label: "Fit to window" },
  { mode: "actual", label: "Actual size (100%)" },
  { mode: "stretch", label: "Stretch" },
];

type Storage = Pick<globalThis.Storage, "getItem" | "setItem">;

const storageKey = (remoteId: string) => `opendesk.scale.${remoteId}`;

function defaultStorage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

/** The mode last chosen for `remoteId`, or fit to window. */
export function loadScaleMode(remoteId: string, storage: Storage | undefined = defaultStorage()): ScaleMode {
  let saved: string | null = null;
  try { saved = storage?.getItem(storageKey(remoteId)) ?? null; } catch { /* unavailable */ }
  return SCALE_MODES.some(({ mode }) => mode === saved) ? (saved as ScaleMode) : "fit";
}

export function saveScaleMode(remoteId: string, mode: ScaleMode, storage: Storage | undefined = defaultStorage()) {
  try { storage?.setItem(storageKey(remoteId), mode); } catch { /* not remembered */ }
}

/**
 * CSS size that shows a `width` x `height` frame one remote pixel per screen
 * pixel at device pixel ratio `ratio`.
 */
export function actualSize(width: number, height: number, ratio: number): { width: number; height: number } {
  const scale = Number.isFinite(ratio) && ratio > 0 ? ratio : 1;
  return { width: width / scale, height: height / scale };
}
