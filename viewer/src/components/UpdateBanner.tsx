import { useCallback, useEffect, useRef, useState } from "react";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { friendlyError, useNotify } from "../lib/notify";

/** A manual "Check for updates" request, dispatched from the Home screen. */
export const CHECK_NOW_EVENT = "opendesk:check-for-updates";

/** How often a running app looks for a new release. */
export const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;
/** Limit for the update check request. */
export const CHECK_TIMEOUT_MS = 30 * 1000;
/** Limit for the whole download request, slow links included. */
export const DOWNLOAD_TIMEOUT_MS = 20 * 60 * 1000;
/** Give up when no data has arrived for this long. */
export const DOWNLOAD_STALL_MS = 60 * 1000;

type Phase = "available" | "downloading" | "ready" | "installing" | "failed";

export interface UpdateBannerProps {
  /** False in dev builds, where there is nothing to update. */
  enabled: boolean;
  /** Only shown on the home screen; a session is never interrupted. */
  visible: boolean;
}

/**
 * Checks for a signed release in the background and offers to install it.
 * The updater plugin verifies the signature before installing. Failures are
 * logged and otherwise ignored: an update is never worth a scary dialog.
 */
export default function UpdateBanner({ enabled, visible }: UpdateBannerProps) {
  const notify = useNotify();
  const [update, setUpdate] = useState<Update | null>(null);
  const [phase, setPhase] = useState<Phase>("available");
  const [progress, setProgress] = useState<number | null>(null);
  const updateRef = useRef<Update | null>(null);
  // Bumped by Cancel so a download that finishes later is ignored.
  const attemptRef = useRef(0);
  const dismissed = useRef<string | null>(null);
  const visibleRef = useRef(visible);
  visibleRef.current = visible;

  // A manual check (from the Home button) reports its result and ignores a
  // previous dismissal; the background check stays silent and respects it.
  const runCheck = useCallback(async (manual: boolean) => {
    if (updateRef.current) { if (manual) notify(`Update available: v${updateRef.current.version}`, "info"); return; }
    if (manual) notify("Checking for updates…", "info");
    try {
      const found = await check({ timeout: CHECK_TIMEOUT_MS });
      if (!found) { if (manual) notify("OpenDesk is up to date.", "info"); return; }
      if (!manual && found.version === dismissed.current) { void found.close().catch(() => {}); return; }
      updateRef.current = found;
      setUpdate(found);
      setPhase("available");
      if (manual) notify(`Update available: v${found.version}`, "info");
    } catch (error) {
      if (manual) notify(friendlyError(error), "error");
      else console.warn("Update check failed:", error);
    }
  }, [notify]);

  useEffect(() => {
    // The manual trigger works regardless of the background schedule.
    const onManual = () => void runCheck(true);
    window.addEventListener(CHECK_NOW_EVENT, onManual);
    if (!enabled) return () => window.removeEventListener(CHECK_NOW_EVENT, onManual);
    void runCheck(false);
    const timer = window.setInterval(() => void runCheck(false), CHECK_INTERVAL_MS);
    return () => { window.clearInterval(timer); window.removeEventListener(CHECK_NOW_EVENT, onManual); };
  }, [enabled, runCheck]);

  const restart = useCallback(async (target: Update) => {
    setPhase("installing");
    // On Windows, install() hands over to the installer and exits the app.
    await target.install();
    await relaunch();
  }, []);

  const start = useCallback(async () => {
    const target = updateRef.current;
    if (!target) return;
    const attempt = ++attemptRef.current;
    const current = () => attemptRef.current === attempt;
    let stallTimer: number | undefined;
    try {
      if (phase === "ready") { await restart(target); return; }
      setPhase("downloading");
      setProgress(null);
      let total = 0;
      let received = 0;
      let stall: (error: Error) => void = () => {};
      const stalled = new Promise<never>((_, reject) => { stall = reject; });
      const arm = () => {
        window.clearTimeout(stallTimer);
        stallTimer = window.setTimeout(() => stall(new Error("update download stalled")), DOWNLOAD_STALL_MS);
      };
      arm();
      await Promise.race([stalled, target.download((event) => {
        arm();
        if (!current()) return;
        if (event.event === "Started") total = event.data.contentLength ?? 0;
        else if (event.event === "Progress" && total > 0) {
          received += event.data.chunkLength;
          setProgress(Math.min(100, Math.round((received / total) * 100)));
        }
      }, { timeout: DOWNLOAD_TIMEOUT_MS })]);
      window.clearTimeout(stallTimer);
      if (!current()) return;
      // A session may have started while downloading; restart once it ends.
      if (!visibleRef.current) { setPhase("ready"); return; }
      await restart(target);
    } catch (error) {
      window.clearTimeout(stallTimer);
      if (!current()) return;
      console.warn("Update install failed:", error);
      setPhase("failed");
    }
  }, [phase, restart]);

  const cancel = () => {
    attemptRef.current += 1;
    setPhase("available");
    setProgress(null);
  };

  const dismiss = () => {
    const target = updateRef.current;
    if (!target) return;
    dismissed.current = target.version;
    updateRef.current = null;
    setUpdate(null);
    void target.close().catch(() => {});
  };

  if (!update || !visible) return null;
  let text = `Update available: v${update.version}`;
  if (phase === "downloading") text = progress === null ? "Downloading update…" : `Downloading update… ${progress}%`;
  else if (phase === "installing") text = "Installing update…";
  else if (phase === "ready") text = `v${update.version} is ready to install.`;
  else if (phase === "failed") text = "The update didn't install. Try again later.";
  const action = phase === "failed" ? "Try again" : phase === "ready" ? "Restart to update" : "Install and restart";

  return (
    <div className="update-banner" role="status">
      <span className="update-banner__text">{text}</span>
      {phase === "downloading" && <button className="update-banner__action" type="button" onClick={cancel}>Cancel</button>}
      {phase !== "downloading" && phase !== "installing" && <>
        <button className="update-banner__action" type="button" onClick={() => void start()}>{action}</button>
        <button className="toast__dismiss" type="button" onClick={dismiss} aria-label="Dismiss update">×</button>
      </>}
    </div>
  );
}
