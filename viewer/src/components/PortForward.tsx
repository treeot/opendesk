import { useEffect, useState } from "react";
import * as api from "../lib/api";
import { friendlyError, useNotify } from "../lib/notify";

/** Viewer panel to forward local TCP ports to the host's loopback (e.g. Ollama). */
export default function PortForward({ open, onClose }: { open: boolean; onClose: () => void }) {
  const notify = useNotify();
  const [forwards, setForwards] = useState<[number, number][]>([]);
  const [local, setLocal] = useState("");
  const [remote, setRemote] = useState("");

  const refresh = () => void api.listForwards().then(setForwards).catch(() => {});
  useEffect(() => { if (open) refresh(); }, [open]);
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
      refresh();
    } catch (error) {
      notify(friendlyError(error));
    }
  };

  const stop = async (l: number) => {
    try { await api.stopForward(l); refresh(); } catch (error) { notify(friendlyError(error)); }
  };

  return (
    <section className="forward-panel" role="dialog" aria-label="Port forwarding">
      <div className="forward-panel__head">
        <strong>Port forwarding</strong>
        <button className="quiet-button" type="button" onClick={onClose} aria-label="Close port forwarding">Close</button>
      </div>
      <p className="caption">
        Reach a service running on the remote over this connection. Example: forward local 11434 to remote 11434, then use <span className="mono">localhost:11434</span> here for the remote's Ollama. The host must allow port forwarding.
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
