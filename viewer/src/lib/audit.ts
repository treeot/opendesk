import type { AuditEvent } from "./api";

/** Plain labels for the host audit log's event names. */
const LABELS: Record<string, string> = {
  session_start: "Session started",
  session_end: "Session ended",
  auth_failed: "Wrong password",
  twofa_failed: "2FA check failed",
  connection_denied: "Connection request denied",
  file_upload_accepted: "Incoming file accepted",
  file_upload_rejected: "Incoming file refused",
  file_upload_completed: "File received",
  file_upload_failed: "Incoming file failed",
  file_download_rejected: "File not sent",
  file_download_completed: "File sent to viewer",
  file_download_failed: "Sending file failed",
  file_mkdir: "Folder created",
  file_delete: "File deleted",
  file_rename: "File renamed",
  secure_attention: "Ctrl+Alt+Del requested",
  restart_request: "Restart requested",
  privacy_mode: "Privacy mode",
  permission_changed: "Permission changed",
};

/** The label for an event name. Names this version does not know (written by
 * a newer host) are shown as words instead of failing. */
export function auditLabel(event: string): string {
  const known = LABELS[event];
  if (known) return known;
  const words = String(event ?? "").replace(/_/g, " ").trim();
  return words ? words.charAt(0).toUpperCase() + words.slice(1) : "Unknown event";
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

/** The secondary facts of an entry (file, place, outcome, length), in order. */
export function auditDetails(entry: AuditEvent): string[] {
  const parts: string[] = [];
  if (entry.filename) parts.push(entry.size_bytes != null ? `${entry.filename} (${formatBytes(entry.size_bytes)})` : entry.filename);
  if (entry.path) parts.push(entry.path);
  if (entry.detail) parts.push(entry.detail);
  if (entry.duration_seconds != null) parts.push(`${entry.duration_seconds}s`);
  return parts;
}
