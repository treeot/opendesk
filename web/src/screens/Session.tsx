import { useEffect, useRef, useState, type ReactNode } from "react";
import { makeDecoder } from "../../../viewer/src/lib/decode";
import { makeAudioPlayer } from "../lib/audio";
import { attachInput } from "../../../viewer/src/lib/input";
import type { ConnectionStatus } from "../lib/connection";
import { downloadBlob, downloadBytes, formatSize, nextFileId } from "../lib/files";
import { formatId } from "../lib/settings";
import type { FileSignal, MonitorInfo } from "../lib/types";

export interface SessionControls {
  sendInput(event: Uint8Array): void;
  requestKeyframe(): void;
  selectMonitor(id: number): void;
  /** Where video frames go; the session screen sets it while mounted. */
  setVideoSink(sink: ((frame: Uint8Array) => void) | null): void;
  /** Where audio packets go while sound is on. */
  setAudioSink(sink: ((payload: Uint8Array, channels: number) => void) | null): void;
  /** Send local clipboard text to the host. */
  sendClipboard(text: string): void;
  sendClipboardImage(width: number, height: number, png: Uint8Array): void;
  /** Where inbound clipboard text goes while the sync toggle is on. */
  setClipboardSink(sink: ((content: { type: "text"; text: string } | { type: "image"; width: number; height: number; png: Uint8Array }) => void) | null): void;
  /** Offer a local file to the host. */
  sendFile(id: number, name: string, data: Uint8Array): void;
  /** Accept or reject an incoming file offer. */
  acceptFile(id: number): void;
  rejectFile(id: number): void;
  /** Where file-transfer updates go while the session is mounted. */
  setFileSink(sink: ((signal: FileSignal) => void) | null): void;
}

type TransferState = "pending" | "active" | "done" | "error";

interface Transfer {
  id: number;
  name: string;
  total: number;
  transferred: number;
  sending: boolean;
  state: TransferState;
  message?: string;
}

function applySignal(transfers: Transfer[], signal: FileSignal): Transfer[] {
  const index = transfers.findIndex((transfer) => transfer.id === signal.id);
  const next = [...transfers];
  const patch = (changes: Partial<Transfer>) => {
    if (index >= 0) next[index] = { ...next[index], ...changes };
  };
  switch (signal.kind) {
    case "file-offer":
      if (index < 0) {
        next.push({ id: signal.id, name: signal.name ?? "file", total: signal.size ?? 0, transferred: 0, sending: false, state: "pending" });
      }
      break;
    case "file-progress":
      patch({ transferred: signal.transferred ?? 0, total: signal.total ?? next[index]?.total ?? 0, state: "active" });
      break;
    case "file-received":
    case "file-sent":
      patch({ state: "done", transferred: next[index]?.total ?? 0 });
      break;
    case "file-error":
      patch({ state: "error", message: signal.message });
      break;
  }
  return next;
}

export interface SessionProps {
  remoteId: string;
  status: ConnectionStatus;
  monitors: MonitorInfo[];
  controls: SessionControls;
  onDisconnect(): void;
}

const overlays: Partial<Record<ConnectionStatus, { title: (id: string) => ReactNode; text: string }>> = {
  connecting: { title: (id) => <>Connecting to <span className="mono">{id}</span></>, text: "Checking the password with the other computer." },
  waiting: { title: (id) => <>Waiting for <span className="mono">{id}</span> to accept</>, text: "The person on the other device needs to allow the request." },
  reconnecting: { title: () => "Connection lost", text: "Reconnecting…" },
};

const statusDot: Record<ConnectionStatus, string> = { connecting: "connecting", waiting: "connecting", reconnecting: "connecting", streaming: "in_session" };

function formatClock(ms: number): string {
  const total = Math.floor(ms / 1000);
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

function monitorName(monitor: MonitorInfo, index: number): string {
  const device = /^\\\\\.\\DISPLAY(\d+)$/i.exec(monitor.name.trim());
  const name = device ? `Display ${device[1]}` : monitor.name.trim() || `Display ${index + 1}`;
  return `${name}${monitor.width && monitor.height ? ` · ${monitor.width}×${monitor.height}` : ""}${monitor.is_primary ? " · Primary" : ""}`;
}

export default function Session({ remoteId, status, monitors, controls, onDisconnect }: SessionProps) {
  const screenRef = useRef<HTMLElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [selected, setSelected] = useState<number>();
  const [clipboardOn, setClipboardOn] = useState(false);
  const [soundOn, setSoundOn] = useState(false);
  const [filesOpen, setFilesOpen] = useState(false);
  const [transfers, setTransfers] = useState<Transfer[]>([]);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [recording, setRecording] = useState(false);
  const [recordMs, setRecordMs] = useState(0);
  const recorderRef = useRef<{ recorder: MediaRecorder; stream: MediaStream; timer: ReturnType<typeof setInterval> } | null>(null);

  useEffect(() => {
    setSelected(monitors.find((monitor) => monitor.is_primary)?.id ?? monitors[0]?.id);
  }, [monitors]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const decoder = makeDecoder(canvas, { requestKeyframe: () => controls.requestKeyframe() });
    controls.setVideoSink(decoder.push);
    const detachInput = attachInput(canvas, async (bytes) => controls.sendInput(bytes));
    // Right clicks go to the remote computer, not the browser menu.
    const noMenu = (event: Event) => event.preventDefault();
    canvas.addEventListener("contextmenu", noMenu);
    canvas.focus();
    return () => {
      controls.setVideoSink(null);
      detachInput();
      canvas.removeEventListener("contextmenu", noMenu);
      decoder.close();
    };
  }, [controls]);

  // Clipboard sync is off by default, like the desktop viewer. While on,
  // inbound text is written to the page clipboard and local text is sent back
  // on focus. The same value is never echoed straight back to its origin.
  useEffect(() => {
    if (!clipboardOn) {
      controls.setClipboardSink(null);
      return;
    }
    const clip = navigator.clipboard;
    let last = "";
    controls.setClipboardSink((content) => {
      if (content.type === "text") {
        last = content.text;
        void clip?.writeText?.(content.text).catch(() => {});
      } else if (clip?.write && typeof ClipboardItem !== "undefined") {
        void clip.write([new ClipboardItem({ "image/png": new Blob([content.png.slice().buffer as ArrayBuffer], { type: "image/png" }) })]).catch(() => {});
      }
    });
    const pull = () => {
      void clip?.readText?.().then((text) => {
        if (text && text !== last && text.length <= 1_000_000) {
          last = text;
          controls.sendClipboard(text);
        }
      }).catch(() => {});
      if (clip?.read && typeof ClipboardItem !== "undefined") {
        void clip.read().then(async (items) => {
          const item = items.find((entry) => entry.types.includes("image/png"));
          if (!item) return;
          const blob = await item.getType("image/png");
          if (blob.size > 5_000_000) return;
          const png = new Uint8Array(await blob.arrayBuffer());
          const bitmap = await createImageBitmap(blob);
          if (bitmap.width && bitmap.height) {
            controls.sendClipboardImage(bitmap.width, bitmap.height, png);
            bitmap.close();
          }
        }).catch(() => {});
      }
    };
    window.addEventListener("focus", pull);
    pull();
    return () => {
      controls.setClipboardSink(null);
      window.removeEventListener("focus", pull);
    };
  }, [clipboardOn, controls]);

  // Sound is off by default. Toggling it on happens inside a click (a user
  // gesture), so the AudioContext is allowed to start. If WebCodecs audio is
  // missing, makeAudioPlayer returns null and we simply play nothing.
  useEffect(() => {
    if (!soundOn) {
      controls.setAudioSink(null);
      return;
    }
    const player = makeAudioPlayer();
    if (!player) {
      controls.setAudioSink(null);
      return;
    }
    controls.setAudioSink((payload, channels) => player.push(payload, channels));
    return () => {
      controls.setAudioSink(null);
      player.close();
    };
  }, [soundOn, controls]);

  // File transfers run for the whole session. Incoming offers wait for the
  // user's Accept or Reject; received files download; a pending offer opens
  // the panel so the prompt is seen.
  useEffect(() => {
    controls.setFileSink((signal) => {
      setTransfers((prev) => applySignal(prev, signal));
      if (signal.kind === "file-received" && signal.data && signal.name) {
        downloadBytes(signal.name, signal.data);
      }
      if (signal.kind === "file-offer") setFilesOpen(true);
    });
    return () => controls.setFileSink(null);
  }, [controls]);

  const acceptOffer = (id: number) => {
    controls.acceptFile(id);
    setTransfers((prev) => prev.map((transfer) => (transfer.id === id ? { ...transfer, state: "active" } : transfer)));
  };

  const rejectOffer = (id: number) => {
    controls.rejectFile(id);
    setTransfers((prev) => prev.filter((transfer) => transfer.id !== id));
  };

  const chooseFiles = async (list: FileList | null) => {
    if (!list) return;
    for (const file of Array.from(list)) {
      const id = nextFileId();
      const data = new Uint8Array(await file.arrayBuffer());
      setTransfers((prev) => [...prev, { id, name: file.name, total: file.size, transferred: 0, sending: true, state: "active" }]);
      controls.sendFile(id, file.name, data);
    }
  };

  // Recording captures what the viewer sees (the decoded remote video) from
  // the canvas to a local .webm via MediaRecorder. It is viewer-only: nothing
  // is sent to the host. If MediaRecorder or captureStream is missing, the
  // button stays disabled.
  const canRecord = typeof MediaRecorder !== "undefined"
    && typeof HTMLCanvasElement !== "undefined"
    && "captureStream" in HTMLCanvasElement.prototype;

  const stopRecording = () => {
    const current = recorderRef.current;
    if (!current) return;
    recorderRef.current = null;
    clearInterval(current.timer);
    if (current.recorder.state !== "inactive") current.recorder.stop();
    current.stream.getTracks().forEach((track) => track.stop());
    setRecording(false);
  };

  const startRecording = () => {
    const canvas = canvasRef.current;
    if (!canvas || !canRecord || recorderRef.current) return;
    const stream = (canvas as HTMLCanvasElement & { captureStream: (fps?: number) => MediaStream }).captureStream(30);
    const preferred = ["video/webm;codecs=vp9", "video/webm;codecs=vp8", "video/webm"];
    const mimeType = preferred.find((type) => MediaRecorder.isTypeSupported(type));
    let recorder: MediaRecorder;
    try {
      recorder = new MediaRecorder(stream, mimeType ? { mimeType } : undefined);
    } catch (error) {
      console.error("could not start recording:", error);
      stream.getTracks().forEach((track) => track.stop());
      return;
    }
    const chunks: Blob[] = [];
    recorder.ondataavailable = (event) => { if (event.data.size) chunks.push(event.data); };
    recorder.onstop = () => {
      const stamp = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
      downloadBlob(`opendesk-${stamp}.webm`, new Blob(chunks, { type: recorder.mimeType || "video/webm" }));
    };
    recorder.start(1000);
    const start = performance.now();
    setRecordMs(0);
    const timer = setInterval(() => setRecordMs(performance.now() - start), 500);
    recorderRef.current = { recorder, stream, timer };
    setRecording(true);
  };

  // Always stop a recording when the session screen unmounts.
  useEffect(() => () => stopRecording(), []);

  const toggleFullscreen = () => {
    if (document.fullscreenElement) void document.exitFullscreen();
    else void screenRef.current?.requestFullscreen?.().then(() => canvasRef.current?.focus());
  };

  const chooseMonitor = (id: number) => {
    setSelected(id);
    controls.selectMonitor(id);
  };

  const overlay = overlays[status];
  const pendingOffers = transfers.filter((transfer) => transfer.state === "pending").length;

  return (
    <main ref={screenRef} className="session-screen web-session">
      <canvas ref={canvasRef} className="session-canvas" tabIndex={0} aria-label="Remote screen" />
      {overlay && (
        <div className="session-host-overlay web-overlay" aria-live="polite">
          <span className="connecting-spinner" aria-hidden="true" />
          <h1>{overlay.title(formatId(remoteId))}</h1>
          <p>{overlay.text}</p>
        </div>
      )}
      <div className="session-toolbar" role="toolbar" aria-label="Session controls">
        <div className="session-toolbar__connection">
          <span className="session-toolbar__connected-dot" data-status={statusDot[status]} aria-hidden="true" />
          <span className="sr-only">{status === "streaming" ? "Connected" : "Connecting"}</span>
          <span className="session-toolbar__remote mono">{formatId(remoteId)}</span>
        </div>
        <div className="session-toolbar__controls">
          <button className="session-toolbar__icon" type="button" aria-label="Fullscreen" title="Fullscreen" onClick={toggleFullscreen}>
            <svg className="session-toolbar__svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
              <path d="M8 3H5a2 2 0 0 0-2 2v3" /><path d="M21 8V5a2 2 0 0 0-2-2h-3" /><path d="M3 16v3a2 2 0 0 0 2 2h3" /><path d="M16 21h3a2 2 0 0 0 2-2v-3" />
            </svg>
            <span className="session-toolbar__icon-text" aria-hidden="true">Fullscreen</span>
          </button>
          <button className="session-toolbar__icon" type="button" aria-pressed={clipboardOn} aria-label="Clipboard sync" title={clipboardOn ? "Clipboard sync on" : "Clipboard sync off"} onClick={() => setClipboardOn((on) => !on)}>
            <svg className="session-toolbar__svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
              <rect x="8" y="2" width="8" height="4" rx="1" /><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2" />
            </svg>
            <span className="session-toolbar__icon-text" aria-hidden="true">Clipboard</span>
          </button>
          <button className="session-toolbar__icon" type="button" aria-pressed={soundOn} aria-label="Sound" title={soundOn ? "Sound on" : "Sound off"} onClick={() => setSoundOn((on) => !on)}>
            <svg className="session-toolbar__svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
              <path d="M11 5 6 9H2v6h4l5 4V5z" />
              {soundOn ? <><path d="M15.5 8.5a5 5 0 0 1 0 7" /><path d="M18.5 5.5a9 9 0 0 1 0 13" /></> : <path d="M22 9l-6 6M16 9l6 6" />}
            </svg>
            <span className="session-toolbar__icon-text" aria-hidden="true">Sound</span>
          </button>
          <button className="session-toolbar__icon" type="button" aria-pressed={filesOpen} aria-label="Files" title="File transfer" onClick={() => setFilesOpen((open) => !open)}>
            <svg className="session-toolbar__svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
              <path d="M4 20h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13c0 1.1.9 2 2 2z" />
            </svg>
            <span className="session-toolbar__icon-text" aria-hidden="true">Files</span>
            {pendingOffers > 0 && <span className="session-toolbar__badge" aria-hidden="true">{pendingOffers}</span>}
          </button>
          {canRecord && (
            <button className="session-toolbar__icon" type="button" aria-pressed={recording} aria-label={recording ? "Stop recording" : "Record"} title={recording ? "Stop recording" : "Record the remote screen"} onClick={() => (recording ? stopRecording() : startRecording())}>
              <svg className="session-toolbar__svg" viewBox="0 0 24 24" fill={recording ? "currentColor" : "none"} stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                {recording ? <rect x="7" y="7" width="10" height="10" rx="1.5" /> : <circle cx="12" cy="12" r="6" />}
              </svg>
              <span className="session-toolbar__icon-text" aria-hidden="true">{recording ? formatClock(recordMs) : "Record"}</span>
            </button>
          )}
          {monitors.length > 1 && (
            <label className="session-toolbar__select-chip">
              <select aria-label="Monitor" value={selected ?? monitors[0].id} onChange={(event) => chooseMonitor(Number(event.target.value))}>
                {monitors.map((monitor, index) => <option key={monitor.id} value={monitor.id}>{monitorName(monitor, index)}</option>)}
              </select>
            </label>
          )}
        </div>
        <div className="session-toolbar__actions">
          <button className="session-toolbar__disconnect" type="button" onClick={onDisconnect}>Disconnect</button>
        </div>
      </div>
      {filesOpen && (
        <aside className="file-panel" aria-label="File transfer">
          <div className="file-panel__head">
            <h2>Files</h2>
            <button type="button" className="file-panel__close" aria-label="Close files" onClick={() => setFilesOpen(false)}>×</button>
          </div>
          <input
            ref={fileInputRef}
            type="file"
            multiple
            className="sr-only"
            onChange={(event) => { void chooseFiles(event.target.files); event.target.value = ""; }}
          />
          <button type="button" className="file-panel__send" onClick={() => fileInputRef.current?.click()}>Send a file</button>
          <ul className="file-panel__list">
            {transfers.length === 0 && <li className="file-panel__empty">No transfers yet.</li>}
            {transfers.map((transfer) => (
              <li key={transfer.id} className="file-panel__item" data-state={transfer.state}>
                <div className="file-panel__row">
                  <span className="file-panel__name" title={transfer.name}>{transfer.sending ? "↑" : "↓"} {transfer.name}</span>
                  <span className="file-panel__size mono">{formatSize(transfer.total)}</span>
                </div>
                {transfer.state === "pending" ? (
                  <div className="file-panel__actions">
                    <button type="button" onClick={() => acceptOffer(transfer.id)}>Accept</button>
                    <button type="button" className="file-panel__reject" onClick={() => rejectOffer(transfer.id)}>Reject</button>
                  </div>
                ) : (
                  <div className="file-panel__progress" aria-hidden={transfer.state === "done"}>
                    <div className="file-panel__bar" style={{ width: `${transfer.total ? Math.round((transfer.transferred / transfer.total) * 100) : 100}%` }} />
                  </div>
                )}
                {transfer.state === "done" && <span className="file-panel__status">Done</span>}
                {transfer.state === "error" && <span className="file-panel__status file-panel__status--error">{transfer.message ?? "Failed"}</span>}
              </li>
            ))}
          </ul>
        </aside>
      )}
      <div className="session-security-chip mono">Encrypted</div>
    </main>
  );
}
