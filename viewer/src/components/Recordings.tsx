import { useCallback, useEffect, useState } from "react";
import { openPath, revealItemInDir } from "@tauri-apps/plugin-opener";
import * as api from "../lib/api";
import { friendlyError, useNotify } from "../lib/notify";
import { formatFileSize } from "./FileTransfer";

const formatWhen = (ms: number) => ms ? new Date(ms).toLocaleString([], { dateStyle: "medium", timeStyle: "short" }) : "";

/** Lists finished session recordings, newest first, with play,
 * reveal-in-folder, and delete: the ones this app made while viewing, or
 * (Host recordings) the ones it made of incoming sessions. */
const GB = 1024 * 1024 * 1024;
const CAP_OPTIONS = [
  { label: "No limit", bytes: 0 },
  { label: "1 GB", bytes: 1 * GB },
  { label: "2 GB", bytes: 2 * GB },
  { label: "5 GB", bytes: 5 * GB },
  { label: "10 GB", bytes: 10 * GB },
  { label: "20 GB", bytes: 20 * GB },
];

const SOURCES: { id: api.RecordingSource; label: string }[] = [
  { id: "viewer", label: "Viewer recordings" },
  { id: "host", label: "Host recordings" },
];

export default function Recordings() {
  const notify = useNotify();
  const [source, setSource] = useState<api.RecordingSource>("viewer");
  const [recordings, setRecordings] = useState<api.RecordingEntry[]>([]);
  const [capBytes, setCapBytes] = useState<number>(0);
  const [recordIncoming, setRecordIncoming] = useState<boolean | null>(null);

  const refresh = useCallback(() => {
    void api.listRecordings(source).then(setRecordings).catch((error: unknown) => notify(friendlyError(error)));
  }, [notify, source]);

  useEffect(() => { refresh(); }, [refresh]);
  useEffect(() => {
    void api.getConfig().then((config) => {
      setCapBytes(config.recordings_max_bytes ?? 0);
      setRecordIncoming(config.record_incoming_sessions ?? false);
    }).catch(() => undefined);
  }, []);

  const used = recordings.reduce((sum, entry) => sum + entry.size, 0);
  const changeCap = (bytes: number) => {
    const previous = capBytes;
    setCapBytes(bytes);
    // Lowering the cap prunes oldest recordings, so refresh the list after.
    void api.setRecordingsMaxBytes(bytes).then(refresh).catch((error: unknown) => { setCapBytes(previous); notify(friendlyError(error)); });
  };
  const toggleRecordIncoming = (enabled: boolean) => {
    setRecordIncoming(enabled);
    void api.setRecordIncomingSessions(enabled).catch((error: unknown) => { setRecordIncoming(!enabled); notify(friendlyError(error)); });
  };

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
      <div className="recordings-tabs" role="tablist" aria-label="Recording source">
        {SOURCES.map((tab) => (
          <button key={tab.id} type="button" role="tab" aria-selected={source === tab.id} className={`recordings-tab${source === tab.id ? " recordings-tab--active" : ""}`} onClick={() => setSource(tab.id)}>{tab.label}</button>
        ))}
      </div>
      {source === "host" && (
        <div className="recordings-host-setting">
          <label className="toggle-label"><input type="checkbox" checked={recordIncoming ?? false} disabled={recordIncoming === null} onChange={(event) => toggleRecordIncoming(event.target.checked)} /><span>Record incoming sessions</span></label>
          <p className="caption">Saves every session on this computer as a video file, for compliance. The viewer is shown that the session is being recorded, and each file is noted in the session log. Applies from the next session.</p>
        </div>
      )}
      {recordings.length === 0
        ? <p className="caption">{source === "host" ? "No host recordings yet." : "No recordings yet. Use Record in a session to save one."}</p>
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
      <div className="recordings-cap">
        <span className="caption">Using {formatFileSize(used)}{capBytes ? ` of ${formatFileSize(capBytes)}` : ""}</span>
        <label className="recordings-cap__select">Keep up to&nbsp;
          <select value={capBytes} onChange={(event) => changeCap(Number(event.target.value))} aria-label="Recordings storage limit">
            {CAP_OPTIONS.map((option) => <option key={option.bytes} value={option.bytes}>{option.label}</option>)}
          </select>
        </label>
      </div>
    </section>
  );
}
