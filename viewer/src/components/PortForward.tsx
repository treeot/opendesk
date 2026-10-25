import { useEffect, useState } from "react";
import * as api from "../lib/api";
import { friendlyError, useNotify } from "../lib/notify";

/** Forwards saved for a device, so they return on the next connection. */
const savedKey = (remoteId?: string) => (remoteId ? `opendesk.forwards.${remoteId}` : null);
const loadSaved = (remoteId?: string): [number, number][] => {
  const key = savedKey(remoteId);
  if (!key) return [];
  try {
    const value = JSON.parse(localStorage.getItem(key) ?? "[]");
    if (!Array.isArray(value)) return [];
    return value.filter((p): p is [number, number] => Array.isArray(p) && p.length === 2 && Number.isInteger(p[0]) && Number.isInteger(p[1]));
  } catch { return []; }
};
const saveForwards = (remoteId: string | undefined, list: [number, number][]) => {
  const key = savedKey(remoteId);
  if (!key) return;
  try { localStorage.setItem(key, JSON.stringify(list)); } catch { /* storage may be blocked */ }
};

/** Viewer panel to forward local TCP ports to the host's loopback (e.g. Ollama). */
export default function PortForward({ open, onClose, remoteId }: { open: boolean; onClose: () => void; remoteId?: string }) {
  const notify = useNotify();
  const [forwards, setForwards] = useState<[number, number][]>([]);
  const [local, setLocal] = useState("");
  const [remote, setRemote] = useState("");

  const refresh = () => api.listForwards().then((list) => { setForwards(list); return list; }).catch(() => [] as [number, number][]);
  // On open, restore this device's saved forwards if none are running yet.
  useEffect(() => {
    if (!open) return;
    let active = true;
    void (async () => {
      const current = await refresh();
      if (!active || current.length > 0) return;
      const saved = loadSaved(remoteId);
      if (saved.length === 0) return;
      let failed = 0;
      for (const [l, r] of saved) {
        try { await api.startForward(l, r); } catch { failed += 1; }
      }
      if (!active) return;
      await refresh();
      if (failed > 0) notify("Some saved forwards could not start. Is forwarding allowed on the host?", "info");
    })();
    return () => { active = false; };
  }, [open, remoteId]);
  if (!open) return null;

  const port = (value: string) => {
    const n = Number(value);
    return Number.isInteger(n) && n >= 1 && n <= 65535 ? n : null;
  };

  const add = async () => {
    const l = port(local);
    const r = port(remote);
    if (l === null || r === null) { notify("Enter valid ports (1-65535)."); return; }
    try {
      await api.startForward(l, r);
      setLocal("");
      setRemote("");
      saveForwards(remoteId, await refresh());
    } catch (error) {
      notify(friendlyError(error));
    }
  };

  const stop = async (l: number) => {
    try { await api.stopForward(l); saveForwards(remoteId, await refresh()); } catch (error) { notify(friendlyError(error)); }
  };

  return (
    <section className="forward-panel" role="dialog" aria-label="Port forwarding">
      <div className="forward-panel__head">
        <strong>Port forwarding</strong>
        <button className="quiet-button" type="button" onClick={onClose} aria-label="Close port forwarding">Close</button>
      </div>
      <p className="caption">
        Reach a service on the remote. Forward a local port to a remote one, then use <span className="mono">localhost:PORT</span>. The host must allow forwarding.
      </p>
      <div className="forward-panel__add">
        <input className="mono" inputMode="numeric" placeholder="local" aria-label="Local port" value={local} onChange={(e) => setLocal(e.target.value)} />
        <span aria-hidden="true">→</span>
        <input className="mono" inputMode="numeric" placeholder="remote" aria-label="Remote port" value={remote} onChange={(e) => setRemote(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter") void add(); }} />
        <button className="primary-button" type="button" onClick={() => void add()}>Add</button>
      </div>
      {forwards.length > 0 && (
        <ul className="forward-panel__list">
          {forwards.map(([l, r]) => (
            <li key={l}>
              <span className="mono">127.0.0.1:{l} → remote:{r}</span>
              <button className="quiet-button" type="button" onClick={() => void stop(l)}>Stop</button>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
