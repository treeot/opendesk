import { useCallback, useEffect, useState } from "react";
import * as api from "../lib/api";
import { formatFileSize } from "./FileTransfer";

interface PaneState { path: string; entries: api.DirEntry[]; error: string | null }
const EMPTY: PaneState = { path: "", entries: [], error: null };

/** The separator a path uses: backslash on Windows-looking paths, else slash. */
function sep(path: string): string {
  return path.includes("\\") ? "\\" : "/";
}

function joinPath(base: string, name: string): string {
  if (!base) return name;
  const s = sep(base);
  return base.endsWith(s) ? base + name : base + s + name;
}

function parentPath(path: string): string | null {
  const s = sep(path);
  const trimmed = path.replace(/[/\\]+$/, "");
  const cut = trimmed.lastIndexOf(s);
  if (cut < 0) return null;
  if (cut === 0) return s; // the root itself
  // Keep the trailing slash on a Windows drive root like "C:\".
  return trimmed.slice(0, trimmed[cut - 1] === ":" ? cut + 1 : cut);
}

function Pane({ title, state, onOpen, onUp, onRefresh, onDownload, onUpload, onNewFolder, onDelete, onRename }: {
  title: string;
  state: PaneState;
  onOpen: (entry: api.DirEntry) => void;
  onUp: () => void;
  onRefresh: () => void;
  onDownload?: (entry: api.DirEntry) => void;
  onUpload?: () => void;
  onNewFolder?: () => void;
  onDelete?: (entry: api.DirEntry) => void;
  onRename?: (entry: api.DirEntry) => void;
}) {
  return (
    <div className="fb-pane">
      <div className="fb-pane__head">
        <strong>{title}</strong>
        <div className="fb-pane__actions">
          {onUpload && <button className="quiet-button" type="button" onClick={onUpload} disabled={!state.path}>Upload…</button>}
          {onNewFolder && <button className="quiet-button" type="button" onClick={onNewFolder} disabled={!state.path}>New folder</button>}
          <button className="quiet-button" type="button" onClick={onUp} disabled={parentPath(state.path) === null}>Up</button>
          <button className="quiet-button" type="button" onClick={onRefresh}>Refresh</button>
        </div>
      </div>
      <div className="fb-pane__path mono" title={state.path}>{state.path || "…"}</div>
      {state.error && <p className="message" role="alert">{state.error}</p>}
      <div className="fb-pane__list">
        {state.entries.length === 0 && !state.error && <p className="caption">Empty folder.</p>}
        {state.entries.map((entry) => (
          <button
            key={entry.name}
            className={`fb-entry${entry.is_dir ? " fb-entry--dir" : ""}`}
            type="button"
            onDoubleClick={() => entry.is_dir && onOpen(entry)}
            onClick={() => entry.is_dir && onOpen(entry)}
          >
            <span className="fb-entry__name">{entry.is_dir ? "📁" : "📄"} {entry.name}</span>
            {!entry.is_dir && <span className="fb-entry__size">{formatFileSize(entry.size)}</span>}
            {!entry.is_dir && onDownload && <span className="fb-entry__dl" role="button" tabIndex={0} title={`Download ${entry.name}`} onClick={(event) => { event.stopPropagation(); onDownload(entry); }} onKeyDown={(event) => { if (event.key === "Enter") { event.stopPropagation(); onDownload(entry); } }}>↓</span>}
            {onRename && <span className="fb-entry__dl" role="button" tabIndex={0} title={`Rename ${entry.name}`} onClick={(event) => { event.stopPropagation(); onRename(entry); }}>Rename</span>}
            {onDelete && <span className="fb-entry__dl" role="button" tabIndex={0} title={`Delete ${entry.name}`} onClick={(event) => { event.stopPropagation(); onDelete(entry); }}>×</span>}
          </button>
        ))}
      </div>
    </div>
  );
}

/**
 * Two-pane file browser: this computer on the left, the remote computer on the
 * right. The local pane reads over a command; the remote pane asks the host and
 * updates from the `remote_listing` event.
 */
export default function FileBrowser() {
  const [local, setLocal] = useState<PaneState>(EMPTY);
  const [remote, setRemote] = useState<PaneState>(EMPTY);

  const loadLocal = useCallback(async (path: string) => {
    try {
      const listing = await api.browseLocal(path);
      setLocal({ path: listing.path, entries: listing.entries, error: null });
    } catch (error) {
      setLocal((current) => ({ ...current, error: String(error) }));
    }
  }, []);

  useEffect(() => {
    void loadLocal("");
    void api.browseRemote("");
  }, [loadLocal]);

  useEffect(() => {
    let stop: (() => void) | undefined;
    const listener = api.onRemoteListing((payload) => setRemote({ path: payload.path, entries: payload.entries, error: payload.error }));
    void listener.then((unlisten) => { stop = unlisten; });
    return () => { if (stop) stop(); else void listener.then((unlisten) => unlisten()); };
  }, []);

  return (
    <div className="fb">
      <Pane
        title="This computer"
        state={local}
        onOpen={(entry) => void loadLocal(joinPath(local.path, entry.name))}
        onUp={() => { const up = parentPath(local.path); if (up !== null) void loadLocal(up); }}
        onRefresh={() => void loadLocal(local.path)}
      />
      <Pane
        title="Remote computer"
        state={remote}
        onOpen={(entry) => void api.browseRemote(joinPath(remote.path, entry.name))}
        onUp={() => { const up = parentPath(remote.path); if (up !== null) void api.browseRemote(up); }}
        onRefresh={() => void api.browseRemote(remote.path)}
        onDownload={(entry) => void api.pullRemote(joinPath(remote.path, entry.name))}
        onUpload={() => void (async () => { const file = await api.pickFile(); if (file) await api.uploadToRemote(remote.path, file); })().catch(() => {})}
        onNewFolder={() => { const name = window.prompt("Folder name"); if (name) void api.makeRemoteDir(remote.path, name).then(() => api.browseRemote(remote.path)); }}
        onRename={(entry) => { const name = window.prompt("New name", entry.name); if (name && name !== entry.name) void api.renameRemote(joinPath(remote.path, entry.name), name).then(() => api.browseRemote(remote.path)); }}
        onDelete={(entry) => { if (window.confirm(`Delete ${entry.name}?`)) void api.deleteRemote(joinPath(remote.path, entry.name)).then(() => api.browseRemote(remote.path)); }}
      />
    </div>
  );
}
