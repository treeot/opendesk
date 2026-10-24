import { useCallback, useEffect, useState } from "react";
import { openPath, revealItemInDir } from "@tauri-apps/plugin-opener";
import * as api from "../lib/api";
import { friendlyError, useNotify } from "../lib/notify";
import { formatFileSize } from "./FileTransfer";

const formatWhen = (ms: number) => ms ? new Date(ms).toLocaleString([], { dateStyle: "medium", timeStyle: "short" }) : "";

/** Lists finished session recordings from the Downloads folder, newest first,
 * with play, reveal-in-folder, and delete. */
export default function Recordings() {
  const notify = useNotify();
  const [recordings, setRecordings] = useState<api.RecordingEntry[]>([]);

  const refresh = useCallback(() => {
    void api.listRecordings().then(setRecordings).catch((error: unknown) => notify(friendlyError(error)));
  }, [notify]);

  useEffect(() => { refresh(); }, [refresh]);

  const play = (entry: api.RecordingEntry) => void openPath(entry.path).catch((error: unknown) => notify(friendlyError(error)));
  const reveal = (entry: api.RecordingEntry) => void revealItemInDir(entry.path).catch((error: unknown) => notify(friendlyError(error)));
  const remove = (entry: api.RecordingEntry) => {
    void api.deleteRecording(entry.path)
      .then(() => setRecordings((current) => current.filter((item) => item.path !== entry.path)))
      .catch((error: unknown) => notify(friendlyError(error)));
  };

  return (
    <section className="card recordings-card" aria-label="Session recordings">
      <div className="recordings-head">
        <span className="eyebrow">SESSION RECORDINGS</span>
        <button className="quiet-button" type="button" onClick={refresh}>Refresh</button>
      </div>
      {recordings.length === 0
        ? <p className="caption">No recordings yet. Use Record in a session to save one.</p>
        : <div className="recordings-list">
            {recordings.map((entry) => (
              <div className="recordings-row" key={entry.path}>
                <div className="recordings-meta">
                  <strong title={entry.name}>{entry.name}</strong>
                  <span className="caption">{formatWhen(entry.modified)} · {formatFileSize(entry.size)}</span>
                </div>
                <div className="recordings-actions">
                  <button className="quiet-button" type="button" onClick={() => play(entry)}>Play</button>
                  <button className="quiet-button" type="button" onClick={() => reveal(entry)}>Reveal</button>
                  <button className="icon-button" type="button" onClick={() => remove(entry)} aria-label={`Delete ${entry.name}`}>×</button>
                </div>
              </div>
            ))}
          </div>}
    </section>
  );
}
