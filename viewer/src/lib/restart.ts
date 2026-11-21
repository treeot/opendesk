/**
 * Remote restart wording and timing for the viewer. The desktop backend
 * keeps retrying for the restart window (host_core::reconnect) once the host
 * says it is restarting; this is what the session screen shows meanwhile.
 */

/** host_core::reconnect::VIEWER_RESTART_WINDOW, for the countdown text. */
export const RESTART_WINDOW_MS = 300_000;

/**
 * Asked before a restart. `sessionPassword` is true when this session used
 * the host's one-time password, which a restart replaces, so the viewer
 * can't get back in on its own.
 */
export function restartConfirmText(sessionPassword: boolean): string {
  if (sessionPassword) {
    return "Restart the remote computer?\n\nYou connected with its one-time password, which changes when it restarts, so OpenDesk can't reconnect on its own. Connect again afterwards with its unattended access password.";
  }
  return `Restart the remote computer?\n\nOpenDesk waits up to ${RESTART_WINDOW_MS / 60_000} minutes for it to come back and reconnects on its own. This needs OpenDesk to start with the remote computer, with unattended access on.`;
}

/** "m:ss" for a wait of `ms` milliseconds. */
export function formatWait(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(seconds / 60)}:${(seconds % 60).toString().padStart(2, "0")}`;
}

/**
 * How long after asking for a restart a still-live session means the host
 * did not restart (it sends `Restarting` and reboots a few seconds later).
 */
export const RESTART_NO_SHOW_MS = 60_000;
