import { useCallback, useEffect, useRef, useState } from "react";
import * as api from "../lib/api";
import { friendlyError, useNotify } from "../lib/notify";
import { formatFileSize } from "./FileTransfer";

/** "3 items" or "report.pdf". */
export function describeFiles(files: api.ClipboardFile[]): string {
  if (files.length === 1) return files[0].name;
  return `${files.length} items`;
}

/** Ctrl+Alt+Shift+V (Cmd+Option+Shift+V on a Mac) pastes the remote files here. */
export function isPasteFilesShortcut(event: Pick<KeyboardEvent, "key" | "code" | "ctrlKey" | "metaKey" | "altKey" | "shiftKey">): boolean {
  const mod = event.ctrlKey || event.metaKey;
  return mod && event.altKey && event.shiftKey && (event.code === "KeyV" || event.key.toLowerCase() === "v");
}

/** Keeps track of the files the remote computer has on its clipboard. */
export function useRemoteClipboardFiles(): api.ClipboardFile[] {
  const [files, setFiles] = useState<api.ClipboardFile[]>([]);
  useEffect(() => {
    let active = true;
    let stop: (() => void) | undefined;
    void api.onRemoteClipboardFiles((payload) => { if (active) setFiles(payload.files); })
      .then((unlisten) => { if (active) stop = unlisten; else unlisten(); }).catch(() => {});
    void api.remoteClipboardFiles().then((payload) => { if (active) setFiles(payload.files); }).catch(() => {});
    return () => { active = false; stop?.(); };
  }, []);
  return files;
}

/**
 * Shown in the viewer's session while the remote computer has files on its
 * clipboard. Nothing is copied until the user pastes: then the files come
 * over into Downloads (or a folder they pick) and land on this computer's
 * clipboard, so a normal paste in Finder or Explorer puts them anywhere.
 */
export default function ClipboardFiles() {
  const files = useRemoteClipboardFiles();
  const [busy, setBusy] = useState(false);
  const [dismissed, setDismissed] = useState<api.ClipboardFile[] | null>(null);
  const notify = useNotify();
  const busyRef = useRef(false);

  const paste = useCallback(async (choose: boolean) => {
    if (busyRef.current) return;
    let dir = "";
    if (choose) {
      const picked = await api.pickFolder().catch(() => null);
      if (!picked) return;
      dir = picked;
    }
    busyRef.current = true;
    setBusy(true);
    try {
      const saved = await api.pasteFromRemote(dir);
      const where = dir || "Downloads";
      notify(`Pasted ${saved.length === 1 ? "1 item" : `${saved.length} items`} into ${where}. ${saved.length === 1 ? "It is" : "They are"} on your clipboard too.`, "info");
    } catch (error) {
      notify(friendlyError(error));
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  }, [notify]);

  const visible = files.length > 0 && dismissed !== files;
  useEffect(() => {
    if (files.length === 0) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (!isPasteFilesShortcut(event)) return;
      // Keep the chord from reaching the remote computer.
      event.preventDefault();
      event.stopImmediatePropagation();
      void paste(false);
    };
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, [files, paste]);

  if (!visible) return null;
  const total = files.reduce((sum, file) => sum + (file.is_dir ? 0 : file.size), 0);
  return (
    <section className="clipboard-files" aria-label="Files copied on the remote computer">
      <div className="clipboard-files__text">
        <span className="eyebrow">COPIED ON REMOTE</span>
        <strong title={files.map((file) => file.name).join("\n")}>{describeFiles(files)}</strong>
        {total > 0 && <span className="caption">{formatFileSize(total)}{files.some((file) => file.is_dir) ? " plus folders" : ""}</span>}
      </div>
      <div className="clipboard-files__actions">
        <button className="primary-button" type="button" disabled={busy} onClick={() => void paste(false)} title="Shortcut: Ctrl or Cmd + Alt + Shift + V">{busy ? "Pasting…" : "Paste to Downloads"}</button>
        <button className="ghost-button" type="button" disabled={busy} onClick={() => void paste(true)}>Paste to…</button>
        <button className="icon-button" type="button" aria-label="Hide" disabled={busy} onClick={() => setDismissed(files)}>×</button>
      </div>
    </section>
  );
}
