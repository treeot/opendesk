import { useEffect } from "react";
import type { Notice } from "../lib/notify";

/** How long an informational notice stays up. Errors stay until dismissed. */
export const INFO_TIMEOUT_MS = 4000;

export interface ToastProps { notice: Notice | null; onDismiss: () => void }

const toastIcon = {
  info: <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="9" /><path d="M12 11v5" /><path d="M12 8h.01" /></svg>,
  error: <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d="M10.3 3.3 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.3a2 2 0 0 0-3.4 0z" /><path d="M12 9v4" /><path d="M12 17h.01" /></svg>,
};

export default function Toast({ notice, onDismiss }: ToastProps) {
  useEffect(() => {
    if (!notice || notice.tone !== "info") return;
    const timer = window.setTimeout(onDismiss, INFO_TIMEOUT_MS);
    return () => window.clearTimeout(timer);
  }, [notice, onDismiss]);

  if (!notice) return null;
  return (
    <div className={`toast toast--${notice.tone}`} role={notice.tone === "error" ? "alert" : "status"}>
      <span className="toast__icon">{toastIcon[notice.tone]}</span>
      <span className="toast__text">{notice.text}</span>
      <button className="toast__dismiss" type="button" onClick={onDismiss} aria-label="Dismiss">×</button>
    </div>
  );
}
