import { useEffect, useRef, useState } from "react";
import * as api from "../lib/api";
import { friendlyError, useNotify } from "../lib/notify";

/** How many activity lines to keep for the current session. */
const MAX_ACTIVITY = 50;

const CAPABILITIES: { key: api.Capability; label: string; hint: string }[] = [
  { key: "control", label: "Keyboard & mouse", hint: "Let the viewer control this device" },
  { key: "files", label: "File transfer", hint: "Accept files the viewer sends" },
  { key: "clipboard", label: "Clipboard", hint: "Share clipboard with the viewer" },
  { key: "audio", label: "Audio", hint: "Send this device's sound to the viewer" },
  { key: "forwarding", label: "Port forwarding", hint: "Let the viewer reach this device's local ports (e.g. Ollama)" },
];

const formatTime = (at: number) => new Date(at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
const formatDuration = (seconds: number) => `${Math.floor(seconds / 60).toString().padStart(2, "0")}:${(seconds % 60).toString().padStart(2, "0")}`;

interface HostControlPanelProps {
  remoteId: string;
  /** Ends the current session but keeps hosting. */
  onDisconnect: () => void;
  chatOpen: boolean;
  onChatToggle: () => void;
  chatUnread: number;
}

/** The host's in-session control panel: who is connected, what they may do
 * (host-enforced), chat, a disconnect button, and the session activity log. */
export default function HostControlPanel({ remoteId, onDisconnect, chatOpen, onChatToggle, chatUnread }: HostControlPanelProps) {
  const notify = useNotify();
  const [elapsed, setElapsed] = useState(0);
  useEffect(() => {
    const startedAt = Date.now();
    const timer = window.setInterval(() => setElapsed(Math.floor((Date.now() - startedAt) / 1000)), 1000);
    return () => window.clearInterval(timer);
  }, []);
  const [permissions, setPermissions] = useState<api.ViewerPermissions>({ control: true, files: true, clipboard: true, audio: true, forwarding: false });
  const [activity, setActivity] = useState<api.Activity[]>([]);
  const logRef = useRef<HTMLUListElement>(null);

  useEffect(() => {
    let mounted = true;
    void api.getViewerPermissions().then((loaded) => { if (mounted) setPermissions(loaded); }).catch(() => undefined);
    const permissionsListener = api.onViewerPermissions((loaded) => { if (mounted) setPermissions(loaded); });
    const activityListener = api.onActivity((entry) => {
      if (mounted) setActivity((current) => [...current, entry].slice(-MAX_ACTIVITY));
    });
    return () => {
      mounted = false;
      for (const listener of [permissionsListener, activityListener]) void listener.then((unlisten) => unlisten?.());
    };
  }, []);

  // Keep the newest activity in view.
  useEffect(() => { const log = logRef.current; if (log) log.scrollTop = log.scrollHeight; }, [activity]);

  const toggle = (capability: api.Capability, allowed: boolean) => {
    // Reflect the intent immediately; the backend confirms via the event.
    setPermissions((current) => ({ ...current, [capability]: allowed }));
    void api.setViewerPermission(capability, allowed).catch((error: unknown) => {
      setPermissions((current) => ({ ...current, [capability]: !allowed }));
      notify(friendlyError(error));
    });
  };

  return (
    <aside className="host-panel" aria-label="Viewer controls">
      <div className="host-panel__head">
        <span className="eyebrow">CONNECTED VIEWER</span>
        <strong className="mono host-panel__peer">{remoteId}</strong>
        <span className="mono host-panel__timer" aria-label="Session timer">{formatDuration(elapsed)}</span>
      </div>
      <fieldset className="host-panel__permissions">
        <legend className="eyebrow">ALLOWED ACTIONS</legend>
        {CAPABILITIES.map(({ key, label, hint }) => (
          <label className="toggle-label" key={key} title={hint}>
            <input type="checkbox" checked={permissions[key]} onChange={(event) => toggle(key, event.target.checked)} />
            <span>{label}</span>
          </label>
        ))}
      </fieldset>
      <div className="host-panel__actions">
        <button className="ghost-button" type="button" onClick={onChatToggle} aria-pressed={chatOpen}>
          {chatOpen ? "Hide chat" : "Chat"}{chatUnread > 0 ? ` (${chatUnread > 99 ? "99+" : chatUnread})` : ""}
        </button>
        <button className="ghost-button host-panel__disconnect" type="button" onClick={onDisconnect}>Disconnect viewer</button>
      </div>
      <div className="host-panel__activity">
        <span className="eyebrow">ACTIVITY</span>
        {activity.length > 0
          ? <ul className="host-panel__log" ref={logRef}>
              {activity.map((entry, index) => (
                <li key={`${entry.at}-${index}`}><time dateTime={new Date(entry.at).toISOString()}>{formatTime(entry.at)}</time> {entry.detail}</li>
              ))}
            </ul>
          : <p className="caption">Nothing yet this session.</p>}
      </div>
    </aside>
  );
}
