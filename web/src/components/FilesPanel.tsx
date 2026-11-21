import { useRef } from "react";
import { formatSize } from "../lib/files";
import { joinPath, parentPath, validName } from "../lib/remotePath";
import type { Transfer } from "../lib/transfers";
import type { DirEntry } from "../lib/types";

export type FilesTab = "transfers" | "remote";

/** What the remote file manager shows. `path` is "" before the first listing. */
export interface RemoteFolder {
  path: string;
  entries: DirEntry[];
  error: string | null;
  loading: boolean;
}

export const EMPTY_FOLDER: RemoteFolder = { path: "", entries: [], error: null, loading: false };

export interface FilesPanelProps {
  tab: FilesTab;
  onTab(tab: FilesTab): void;
  transfers: Transfer[];
  onAccept(id: number): void;
  onReject(id: number): void;
  /** Send local files; `dir` is a host folder, or undefined for its Downloads. */
  onSend(files: File[], dir?: string): void;
  folder: RemoteFolder;
  onBrowse(path: string): void;
  onPull(path: string, name: string): void;
  onMakeDir(path: string, name: string): void;
  onDelete(path: string): void;
  onRename(path: string, name: string): void;
  onClose(): void;
}

/** Folders first, then by name, as a file manager lists them. */
export function sortEntries(entries: DirEntry[]): DirEntry[] {
  return [...entries].sort((a, b) => Number(b.is_dir) - Number(a.is_dir) || a.name.localeCompare(b.name, undefined, { sensitivity: "base", numeric: true }));
}

const folderIcon = (
  <svg className="web-files__icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
    <path d="M4 20h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13c0 1.1.9 2 2 2z" />
  </svg>
);
const fileIcon = (
  <svg className="web-files__icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
    <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" /><path d="M14 2v6h6" />
  </svg>
);

function TransferList({ transfers, onAccept, onReject }: Pick<FilesPanelProps, "transfers" | "onAccept" | "onReject">) {
  return (
    <ul className="file-panel__list" aria-label="Transfers">
      {transfers.length === 0 && <li className="file-panel__empty">No transfers yet. Drop files on the screen to send them.</li>}
      {transfers.map((transfer) => {
        const percent = transfer.total ? Math.round((transfer.transferred / transfer.total) * 100) : 100;
        return (
          <li key={transfer.id} className="file-panel__item" data-state={transfer.state}>
            <div className="file-panel__row">
              <span className="file-panel__name" title={transfer.name}>
                <span aria-hidden="true">{transfer.sending ? "↑" : "↓"} </span>
                <span className="sr-only">{transfer.sending ? "Sending" : "Receiving"} </span>
                {transfer.name}
              </span>
              <span className="file-panel__size mono">{formatSize(transfer.total)}</span>
            </div>
            {transfer.state === "pending" ? (
              <div className="file-panel__actions">
                <button type="button" onClick={() => onAccept(transfer.id)}>Accept</button>
                <button type="button" className="file-panel__reject" onClick={() => onReject(transfer.id)}>Reject</button>
              </div>
            ) : transfer.state === "active" && (
              <div className="file-panel__progress" role="progressbar" aria-label={transfer.name} aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent}>
                <div className="file-panel__bar" style={{ width: `${percent}%` }} />
              </div>
            )}
            {transfer.state === "done" && <span className="file-panel__status">Done</span>}
            {transfer.state === "error" && <span className="file-panel__status file-panel__status--error">{transfer.message ?? "Failed"}</span>}
          </li>
        );
      })}
    </ul>
  );
}

function RemoteFiles({ folder, onBrowse, onPull, onMakeDir, onDelete, onRename, onUpload }: Pick<FilesPanelProps, "folder" | "onBrowse" | "onPull" | "onMakeDir" | "onDelete" | "onRename"> & { onUpload(): void }) {
  const up = parentPath(folder.path);
  const newFolder = () => {
    const name = window.prompt("New folder name");
    if (name === null) return;
    if (!validName(name)) {
      window.alert("Use a name without slashes or colons.");
      return;
    }
    onMakeDir(folder.path, name.trim());
  };
  const rename = (entry: DirEntry) => {
    const name = window.prompt(`Rename "${entry.name}" to`, entry.name);
    if (name === null || name.trim() === entry.name) return;
    if (!validName(name)) {
      window.alert("Use a name without slashes or colons.");
      return;
    }
    onRename(joinPath(folder.path, entry.name), name.trim());
  };
  const remove = (entry: DirEntry) => {
    if (window.confirm(`Delete "${entry.name}" on the remote computer? This can't be undone.`)) onDelete(joinPath(folder.path, entry.name));
  };

  return (
    <div className="web-files">
      <div className="web-files__bar">
        <button type="button" className="web-files__button" onClick={() => up !== null && onBrowse(up)} disabled={up === null} aria-label="Up one folder">Up</button>
        <button type="button" className="web-files__button" onClick={() => onBrowse(folder.path)}>Refresh</button>
        <button type="button" className="web-files__button" onClick={newFolder} disabled={!folder.path}>New folder</button>
        <button type="button" className="web-files__button" onClick={onUpload} disabled={!folder.path}>Upload here</button>
      </div>
      <div className="web-files__path mono" title={folder.path}>{folder.path || "Home and drives"}</div>
      {folder.error && <p className="file-panel__status file-panel__status--error" role="alert">{folder.error}</p>}
      <ul className="web-files__list" aria-label={`Contents of ${folder.path || "the remote computer"}`} aria-busy={folder.loading}>
        {folder.loading && folder.entries.length === 0 && <li className="file-panel__empty">Loading…</li>}
        {!folder.loading && !folder.error && folder.entries.length === 0 && <li className="file-panel__empty">This folder is empty.</li>}
        {sortEntries(folder.entries).map((entry) => (
          <li key={entry.name} className="web-files__entry">
            {entry.is_dir ? (
              <button type="button" className="web-files__open" onClick={() => onBrowse(joinPath(folder.path, entry.name))} title={entry.name}>
                {folderIcon}<span className="web-files__name">{entry.name}</span>
              </button>
            ) : (
              <span className="web-files__open web-files__open--file" title={entry.name}>
                {fileIcon}<span className="web-files__name">{entry.name}</span>
                <span className="file-panel__size mono">{formatSize(entry.size)}</span>
              </span>
            )}
            <span className="web-files__actions">
              {!entry.is_dir && <button type="button" className="web-files__action" aria-label={`Download ${entry.name}`} title="Download" onClick={() => onPull(joinPath(folder.path, entry.name), entry.name)}>↓</button>}
              {folder.path && <button type="button" className="web-files__action" aria-label={`Rename ${entry.name}`} title="Rename" onClick={() => rename(entry)}>✎</button>}
              {folder.path && <button type="button" className="web-files__action web-files__action--danger" aria-label={`Delete ${entry.name}`} title="Delete" onClick={() => remove(entry)}>×</button>}
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * The browser viewer's file panel: transfers in both directions with the
 * consent prompt for incoming files, and a manager for the host's folders.
 * The host still decides: its file permission covers both tabs.
 */
export default function FilesPanel(props: FilesPanelProps) {
  const { tab, onTab, onSend, folder, onClose } = props;
  const inputRef = useRef<HTMLInputElement>(null);
  const targetDir = useRef<string | undefined>(undefined);

  const pick = (dir?: string) => {
    targetDir.current = dir;
    inputRef.current?.click();
  };

  return (
    <aside className={`file-panel${tab === "remote" ? " file-panel--wide" : ""}`} aria-label="Files" onKeyDown={(event) => { if (event.key === "Escape") onClose(); }}>
      <div className="file-panel__head">
        <h2>Files</h2>
        <button type="button" className="file-panel__close" aria-label="Close files" onClick={onClose}>×</button>
      </div>
      <div className="web-tabs" role="tablist" aria-label="File views">
        <button type="button" role="tab" id="files-tab-transfers" aria-controls="files-view" aria-selected={tab === "transfers"} className="web-tabs__tab" onClick={() => onTab("transfers")}>Transfers</button>
        <button type="button" role="tab" id="files-tab-remote" aria-controls="files-view" aria-selected={tab === "remote"} className="web-tabs__tab" onClick={() => onTab("remote")}>Remote files</button>
      </div>
      <input
        ref={inputRef}
        type="file"
        multiple
        className="sr-only"
        tabIndex={-1}
        aria-hidden="true"
        onChange={(event) => {
          const files = Array.from(event.target.files ?? []);
          event.target.value = "";
          if (files.length) onSend(files, targetDir.current);
        }}
      />
      <div id="files-view" role="tabpanel" aria-labelledby={tab === "transfers" ? "files-tab-transfers" : "files-tab-remote"} className="web-tabs__panel">
        {tab === "transfers" ? (
          <>
            <button type="button" className="file-panel__send" onClick={() => pick()}>Send files</button>
            <TransferList {...props} />
          </>
        ) : (
          <RemoteFiles {...props} onUpload={() => pick(folder.path)} />
        )}
      </div>
    </aside>
  );
}
