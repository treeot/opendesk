/**
 * Path helpers for the host's folders. The host may run any OS, so a path
 * that contains a backslash is treated as Windows-style, as in the desktop
 * viewer's file browser.
 */

function separator(path: string): string {
  return path.includes("\\") ? "\\" : "/";
}

/** `name` inside folder `base`. */
export function joinPath(base: string, name: string): string {
  if (!base) return name;
  const sep = separator(base);
  return base.endsWith(sep) ? base + name : base + sep + name;
}

/**
 * The folder above `path`, or null when there is nothing above. Going up
 * from a root such as "/" or "C:\" returns "", so the host lists its
 * starting places (home and drives) again.
 */
export function parentPath(path: string): string | null {
  if (!path) return null;
  const sep = separator(path);
  const trimmed = path.replace(/[/\\]+$/, "");
  if (!trimmed || /^[A-Za-z]:$/.test(trimmed)) return "";
  const cut = trimmed.lastIndexOf(sep);
  if (cut < 0) return "";
  if (cut === 0) return sep;
  // Keep the trailing separator on a Windows drive root like "C:\".
  return trimmed.slice(0, trimmed[cut - 1] === ":" ? cut + 1 : cut);
}

/** The last component of `path`. */
export function baseName(path: string): string {
  const trimmed = path.replace(/[/\\]+$/, "");
  return trimmed.slice(Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\")) + 1);
}

/**
 * Whether `name` is one plain path component, fit for a new folder or a
 * rename. The host checks again; this only catches typos early.
 */
export function validName(name: string): boolean {
  const trimmed = name.trim();
  return trimmed.length > 0 && trimmed.length <= 255 && trimmed !== "." && trimmed !== ".." && !/[/\\:\0]/.test(trimmed);
}
