import { useEffect, useRef, useState, type DragEvent, type ReactNode } from "react";
import { makeDecoder } from "../../../viewer/src/lib/decode";
import { makeAudioPlayer } from "../lib/audio";
import { attachInput } from "../../../viewer/src/lib/input";
import { createPointerLock, RELEASE_HINT, type PointerLockControl } from "../../../viewer/src/lib/pointerLock";
import ChatPanel, { type ChatEntry } from "../components/ChatPanel";
import FilesPanel, { EMPTY_FOLDER, type FilesTab, type RemoteFolder } from "../components/FilesPanel";
import ToolbarMenu from "../components/ToolbarMenu";
import type { ConnectionStatus } from "../lib/connection";
import { downloadBlob, downloadBytes, nextFileId } from "../lib/files";
import { SPECIAL_KEYS, specialKeyEvents } from "../lib/keys";
import { formatId } from "../lib/settings";
import { applySignal, PendingPulls, type Transfer } from "../lib/transfers";
import type { BrowseSignal, FileSignal, HostFeature, MonitorInfo } from "../lib/types";

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
  /** Offer a local file to the host, into its folder `dir` if given. */
  sendFile(id: number, name: string, data: Uint8Array, dir?: string): void;
  /** Accept or reject an incoming file offer. */
  acceptFile(id: number): void;
  rejectFile(id: number): void;
  /** Where file-transfer updates go while the session is mounted. */
  setFileSink(sink: ((signal: FileSignal) => void) | null): void;
  /** Remote file manager requests; answers arrive at the browse sink. */
  listDir(path: string): void;
  pullFile(path: string): void;
  makeDir(path: string, name: string): void;
  deletePath(path: string): void;
  renamePath(path: string, name: string): void;
  setBrowseSink(sink: ((signal: BrowseSignal) => void) | null): void;
  /** Send a chat message; false when it could not be sent. */
  sendChat(text: string): boolean;
  setChatSink(sink: ((text: string) => void) | null): void;
  secureAttention(): void;
  setMaxBitrate(bps: number): void;
  setTextReadability(enabled: boolean): void;
}

export interface SessionProps {
  remoteId: string;
  status: ConnectionStatus;
  monitors: MonitorInfo[];
  controls: SessionControls;
  /** The latest relay round trip, or null before the first answer. */
  roundTrip?: number | null;
  /** Optional features the host offers. */
  features?: HostFeature[];
  onDisconnect(): void;
}

/** Stream quality tiers, as in the desktop viewer: a bitrate cap, 0 for automatic. */
export const QUALITY_TIERS: { label: string; bps: number }[] = [
  { label: "Auto", bps: 0 },
  { label: "High", bps: 20_000_000 },
  { label: "Balanced", bps: 8_000_000 },
  { label: "Data-saver", bps: 3_000_000 },
];

/** Round trips above this get the warning colour, as in the desktop viewer. */
export const SLOW_RTT_MS = 100;

/**
 * A browser holds a whole file in memory (and copies it into web-core) to
 * send it, so one pick or drop is capped at this many bytes.
 */
export const MAX_SEND_BYTES = 512 * 1024 ** 2;

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

function Svg({ children, filled }: { children: ReactNode; filled?: boolean }) {
  return (
    <svg className="session-toolbar__svg" viewBox="0 0 24 24" fill={filled ? "currentColor" : "none"} stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      {children}
    </svg>
  );
}

const icons = {
  keyboard: <Svg><rect width="20" height="16" x="2" y="4" rx="2" /><path d="M6 8h.01M10 8h.01M14 8h.01M18 8h.01M6 12h.01M18 12h.01M8 16h8" /></Svg>,
  more: <Svg><circle cx="12" cy="12" r="1" /><circle cx="19" cy="12" r="1" /><circle cx="5" cy="12" r="1" /></Svg>,
  chat: <Svg><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></Svg>,
  files: <Svg><path d="M4 20h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13c0 1.1.9 2 2 2z" /></Svg>,
  fullscreen: <Svg><path d="M8 3H5a2 2 0 0 0-2 2v3" /><path d="M21 8V5a2 2 0 0 0-2-2h-3" /><path d="M3 16v3a2 2 0 0 0 2 2h3" /><path d="M16 21h3a2 2 0 0 0 2-2v-3" /></Svg>,
  gauge: <Svg><path d="m12 14 4-4" /><path d="M3.34 19a10 10 0 1 1 17.32 0" /></Svg>,
};

export function LinkBadge({ roundTrip }: { roundTrip: number | null }) {
  const measured = roundTrip === null ? "Round trip not measured yet." : `Round trip ${roundTrip} ms.`;
  return (
    <span
      className="session-toolbar__link mono"
      data-path="relay"
      data-slow={roundTrip !== null && roundTrip > SLOW_RTT_MS}
      title={`Relayed: a browser always connects through the relay server. Traffic stays end-to-end encrypted. ${measured}`}
    >
      <span className="session-toolbar__link-dot" aria-hidden="true" />
      <span className="session-toolbar__link-label">Relayed</span>
      <span className="session-toolbar__link-sep" aria-hidden="true">·</span>
      <span className="session-toolbar__link-rtt">{roundTrip === null ? "–" : `${roundTrip} ms`}</span>
    </span>
  );
}

export default function Session({ remoteId, status, monitors, controls, roundTrip = null, features = [], onDisconnect }: SessionProps) {
  const screenRef = useRef<HTMLElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [selected, setSelected] = useState<number>();
  const [clipboardOn, setClipboardOn] = useState(false);
  const [soundOn, setSoundOn] = useState(false);
  const [quality, setQuality] = useState(0);
  const [textReadability, setTextReadability] = useState(false);
  const [viewOnly, setViewOnly] = useState(false);
  const viewOnlyRef = useRef(false);
  const [filesOpen, setFilesOpen] = useState(false);
  const [filesTab, setFilesTab] = useState<FilesTab>("transfers");
  const [transfers, setTransfers] = useState<Transfer[]>([]);
  const [folder, setFolder] = useState<RemoteFolder>(EMPTY_FOLDER);
  const folderRef = useRef(folder);
  const pullsRef = useRef(new PendingPulls());
  const [chatOpen, setChatOpen] = useState(false);
  const chatOpenRef = useRef(false);
  const [chat, setChat] = useState<ChatEntry[]>([]);
  const [unread, setUnread] = useState(0);
  const [dragging, setDragging] = useState(false);
  const dragDepth = useRef(0);
  const [notice, setNotice] = useState<string>();
  const [recording, setRecording] = useState(false);
  const [recordMs, setRecordMs] = useState(0);
  const recorderRef = useRef<{ recorder: MediaRecorder; stream: MediaStream; timer: ReturnType<typeof setInterval> } | null>(null);
  // Relative mouse mode: on while the canvas holds the pointer lock.
  const [relativeMouse, setRelativeMouse] = useState(false);
  const relativeMouseRef = useRef(false);
  const pointerLockRef = useRef<PointerLockControl | null>(null);

  useEffect(() => { viewOnlyRef.current = viewOnly; }, [viewOnly]);
  useEffect(() => { folderRef.current = folder; }, [folder]);
  useEffect(() => { chatOpenRef.current = chatOpen; }, [chatOpen]);

  useEffect(() => {
    setSelected(monitors.find((monitor) => monitor.is_primary)?.id ?? monitors[0]?.id);
  }, [monitors]);

  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(undefined), 6000);
    return () => clearTimeout(timer);
  }, [notice]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const decoder = makeDecoder(canvas, { requestKeyframe: () => controls.requestKeyframe() });
    controls.setVideoSink(decoder.push);
    // View only keeps watching but sends no input, like the desktop viewer.
    const detachInput = attachInput(canvas, async (bytes) => { if (!viewOnlyRef.current) controls.sendInput(bytes); }, { relative: () => relativeMouseRef.current });
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

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const lock = createPointerLock(canvas, (locked) => {
      relativeMouseRef.current = locked;
      setRelativeMouse(locked);
      if (locked) setNotice(RELEASE_HINT);
    });
    pointerLockRef.current = lock;
    return () => { lock.dispose(); pointerLockRef.current = null; relativeMouseRef.current = false; };
  }, []);
  useEffect(() => { if (status !== "streaming" || viewOnly) pointerLockRef.current?.release(); }, [status, viewOnly]);

  // Runs inside the menu click, which pointer lock needs as its user gesture.
  const toggleRelativeMouse = () => {
    const lock = pointerLockRef.current;
    if (!lock) return;
    if (relativeMouseRef.current) { lock.release(); return; }
    if (viewOnlyRef.current) { setNotice("View only is on. Turn it off to capture the mouse."); return; }
    void lock.request().catch(() => setNotice("This browser couldn't capture the mouse."));
  };

  // The host forgets these on a reconnect, so send them again whenever the
  // stream (re)starts. Defaults are left alone for older hosts.
  useEffect(() => {
    if (status === "streaming" && quality !== 0) controls.setMaxBitrate(quality);
  }, [status, quality, controls]);
  const textReadabilitySent = useRef(false);
  useEffect(() => {
    if (status !== "streaming" || (!textReadability && !textReadabilitySent.current)) return;
    textReadabilitySent.current = true;
    controls.setTextReadability(textReadability);
  }, [status, textReadability, controls]);

  const chooseQuality = (bps: number) => {
    setQuality(bps);
    // Going back to Auto must reach the host too; the effect skips 0.
    if (bps === 0) controls.setMaxBitrate(0);
  };

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
  // user's Accept or Reject, except a file the user just asked the host for;
  // received files download; a pending offer opens the panel so it is seen.
  useEffect(() => {
    controls.setFileSink((signal) => {
      if (signal.kind === "file-offer" && signal.name && pullsRef.current.take(signal.name)) {
        setTransfers((prev) => applySignal(prev, signal).map((transfer) => (transfer.id === signal.id ? { ...transfer, state: "active" } : transfer)));
        controls.acceptFile(signal.id);
        return;
      }
      setTransfers((prev) => applySignal(prev, signal));
      if (signal.kind === "file-received" && signal.data && signal.name) {
        downloadBytes(signal.name, signal.data);
      }
      if (signal.kind === "file-offer") {
        setFilesTab("transfers");
        setFilesOpen(true);
      }
    });
    return () => controls.setFileSink(null);
  }, [controls]);

  useEffect(() => {
    controls.setBrowseSink((signal) => {
      if (signal.t === "dir_listing") setFolder({ path: signal.path, entries: signal.entries, error: null, loading: false });
      else setFolder((current) => ({ ...current, error: signal.message, loading: false }));
    });
    return () => controls.setBrowseSink(null);
  }, [controls]);

  useEffect(() => {
    controls.setChatSink((text) => {
      setChat((prev) => [...prev, { from: "peer", text, at: Date.now() }]);
      if (!chatOpenRef.current) setUnread((count) => count + 1);
    });
    return () => controls.setChatSink(null);
  }, [controls]);

  const browse = (path: string) => {
    setFolder((current) => ({ ...current, error: null, loading: true }));
    controls.listDir(path);
  };

  // The host answers a change only when it fails, so list the folder again
  // shortly after to show the result.
  const refreshSoon = () => setTimeout(() => browse(folderRef.current.path), 400);

  const openFilesTab = (tab: FilesTab) => {
    setFilesTab(tab);
    if (tab === "remote" && !folder.path && !folder.loading) browse("");
  };

  const acceptOffer = (id: number) => {
    controls.acceptFile(id);
    setTransfers((prev) => prev.map((transfer) => (transfer.id === id ? { ...transfer, state: "active" } : transfer)));
  };

  const rejectOffer = (id: number) => {
    controls.rejectFile(id);
    setTransfers((prev) => prev.filter((transfer) => transfer.id !== id));
  };

  const sendFiles = async (files: File[], dir?: string) => {
    const total = files.reduce((sum, file) => sum + file.size, 0);
    if (total > MAX_SEND_BYTES) {
      setNotice("Those files are too big to send from a browser. Send less than 512 MB at a time.");
      return;
    }
    setFilesTab("transfers");
    setFilesOpen(true);
    for (const file of files) {
      const id = nextFileId();
      let data: Uint8Array;
      try {
        data = new Uint8Array(await file.arrayBuffer());
      } catch {
        // A folder or an unreadable file.
        setNotice(`Couldn't read "${file.name}". Folders can't be sent from a browser yet.`);
        continue;
      }
      setTransfers((prev) => [...prev, { id, name: file.name, total: file.size, transferred: 0, sending: true, state: "active" }]);
      controls.sendFile(id, file.name, data, dir);
    }
  };

  const pull = (path: string, name: string) => {
    pullsRef.current.add(name);
    controls.pullFile(path);
    setNotice(`Asked the remote computer for "${name}".`);
  };

  // Files dropped anywhere on the session go to the host. With the remote
  // file manager open on a folder, they go into that folder.
  const dropDir = filesOpen && filesTab === "remote" && folder.path ? folder.path : undefined;
  const hasFiles = (event: DragEvent) => Array.from(event.dataTransfer?.types ?? []).includes("Files");
  const onDragEnter = (event: DragEvent) => {
    if (!hasFiles(event)) return;
    event.preventDefault();
    dragDepth.current += 1;
    setDragging(true);
  };
  const onDragOver = (event: DragEvent) => {
    if (!hasFiles(event)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = status === "streaming" ? "copy" : "none";
  };
  const onDragLeave = (event: DragEvent) => {
    if (!hasFiles(event)) return;
    dragDepth.current = Math.max(0, dragDepth.current - 1);
    if (dragDepth.current === 0) setDragging(false);
  };
  const onDrop = (event: DragEvent) => {
    if (!hasFiles(event)) return;
    event.preventDefault();
    dragDepth.current = 0;
    setDragging(false);
    if (status !== "streaming") return;
    const files = Array.from(event.dataTransfer.files);
    if (files.length) void sendFiles(files, dropDir);
  };

  const sendChat = (text: string) => {
    if (!controls.sendChat(text)) return false;
    setChat((prev) => [...prev, { from: "me", text, at: Date.now() }]);
    return true;
  };

  const toggleChat = () => {
    setChatOpen((open) => !open);
    setUnread(0);
  };

  const sendSpecialKey = (id: string) => {
    const key = SPECIAL_KEYS.find((entry) => entry.id === id);
    if (!key) return;
    // A Windows host can only take Ctrl+Alt+Del through its secure attention
    // path; other hosts get the key presses.
    if (id === "ctrl-alt-del" && features.includes("secure-attention")) controls.secureAttention();
    else specialKeyEvents(key).forEach((bytes) => controls.sendInput(bytes));
  };

  // Recording captures what the viewer sees (the decoded remote video) from
  // the canvas to a local .webm via MediaRecorder. It is viewer-only: nothing
  // is sent to the host. If MediaRecorder or captureStream is missing, the
  // option is hidden.
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

  const focusScreen = () => canvasRef.current?.focus();

  const overlay = overlays[status];
  const pendingOffers = transfers.filter((transfer) => transfer.state === "pending").length;
  const moreItems = [
    { id: "clipboard", label: "Clipboard sync", on: clipboardOn, onSelect: () => setClipboardOn((on) => !on) },
    { id: "text", label: "Text readability", on: textReadability, onSelect: () => setTextReadability((on) => !on) },
    ...(features.includes("relative-mouse") ? [{ id: "relative-mouse", label: "Relative mouse (games)", on: relativeMouse, onSelect: toggleRelativeMouse }] : []),
    { id: "view-only", label: "View only", on: viewOnly, onSelect: () => setViewOnly((on) => !on) },
    ...(canRecord ? [{ id: "record", label: recording ? "Stop recording" : "Record the screen", danger: true, onSelect: () => (recording ? stopRecording() : startRecording()) }] : []),
  ];

  return (
    <main
      ref={screenRef}
      className="session-screen web-session"
      onDragEnter={onDragEnter}
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
    >
      <canvas ref={canvasRef} className="session-canvas" tabIndex={0} aria-label="Remote screen" />
      {features.includes("recorded") && <div className="session-recorded" role="status"><span className="session-recorded__dot" aria-hidden="true" />This session is being recorded by the remote computer</div>}
      {overlay && (
        <div className="session-host-overlay web-overlay" aria-live="polite">
          <span className="connecting-spinner" aria-hidden="true" />
          <h1>{overlay.title(formatId(remoteId))}</h1>
          <p>{overlay.text}</p>
        </div>
      )}
      {dragging && (
        <div className="web-drop" aria-hidden="true">
          <div className="web-drop__card">
            {status === "streaming"
              ? <>Drop to send to <span className="mono">{formatId(remoteId)}</span>{dropDir ? <span className="web-drop__dir mono">{dropDir}</span> : <span className="web-drop__dir">Saved in its Downloads folder</span>}</>
              : "Wait for the connection before sending files."}
          </div>
        </div>
      )}
      <div className="session-toolbar" role="toolbar" aria-label="Session controls">
        <div className="session-toolbar__connection">
          <span className="session-toolbar__connected-dot" data-status={statusDot[status]} aria-hidden="true" />
          <span className="sr-only">{status === "streaming" ? "Connected" : "Connecting"}</span>
          <span className="session-toolbar__remote mono">{formatId(remoteId)}</span>
          {status === "streaming" && <LinkBadge roundTrip={roundTrip} />}
        </div>
        <div className="session-toolbar__controls">
          <button className="session-toolbar__icon" type="button" aria-label="Fullscreen" title="Fullscreen" onClick={toggleFullscreen}>
            {icons.fullscreen}
            <span className="session-toolbar__icon-text" aria-hidden="true">Fullscreen</span>
          </button>
          {monitors.length > 1 && (
            <label className="session-toolbar__select-chip">
              <select aria-label="Monitor" value={selected ?? monitors[0].id} onChange={(event) => chooseMonitor(Number(event.target.value))}>
                {monitors.map((monitor, index) => <option key={monitor.id} value={monitor.id}>{monitorName(monitor, index)}</option>)}
              </select>
            </label>
          )}
          <label className="session-toolbar__select-chip" title="Cap the stream quality. Auto follows your connection.">
            {icons.gauge}
            <select aria-label="Quality" value={quality} onChange={(event) => chooseQuality(Number(event.target.value))}>
              {QUALITY_TIERS.map((tier) => <option key={tier.bps} value={tier.bps}>{tier.label}</option>)}
            </select>
          </label>
          <button className="session-toolbar__icon" type="button" aria-pressed={soundOn} aria-label="Sound" title={soundOn ? "Sound on" : "Sound off"} onClick={() => setSoundOn((on) => !on)}>
            <Svg>
              <path d="M11 5 6 9H2v6h4l5 4V5z" />
              {soundOn ? <><path d="M15.5 8.5a5 5 0 0 1 0 7" /><path d="M18.5 5.5a9 9 0 0 1 0 13" /></> : <path d="M22 9l-6 6M16 9l6 6" />}
            </Svg>
            <span className="session-toolbar__icon-text" aria-hidden="true">Sound</span>
          </button>
          <button className="session-toolbar__icon" type="button" aria-pressed={filesOpen} aria-expanded={filesOpen} aria-label="Files" title="Send, receive and browse files" onClick={() => setFilesOpen((open) => !open)}>
            {icons.files}
            <span className="session-toolbar__icon-text" aria-hidden="true">Files</span>
            {pendingOffers > 0 && <span className="session-toolbar__badge" aria-hidden="true">{pendingOffers}</span>}
          </button>
          <button className="session-toolbar__icon" type="button" aria-pressed={chatOpen} aria-expanded={chatOpen} aria-label={unread > 0 ? `Chat, ${unread} unread` : "Chat"} title={chatOpen ? "Hide chat" : "Open chat"} onClick={toggleChat}>
            {icons.chat}
            <span className="session-toolbar__icon-text" aria-hidden="true">Chat</span>
            {unread > 0 && <span className="session-toolbar__badge" aria-hidden="true">{unread > 99 ? "99+" : unread}</span>}
          </button>
          <ToolbarMenu
            label="Special keys"
            text="Keys"
            icon={icons.keyboard}
            title="Send keys your browser keeps for itself"
            items={SPECIAL_KEYS.map((key) => ({ id: key.id, label: key.label, onSelect: () => sendSpecialKey(key.id) }))}
            onChosen={focusScreen}
          />
          {recording && (
            <button className="session-toolbar__icon" type="button" aria-label="Stop recording" title="Stop recording" onClick={stopRecording}>
              <Svg filled><rect x="7" y="7" width="10" height="10" rx="1.5" /></Svg>
              <span className="session-toolbar__icon-text" aria-hidden="true">{formatClock(recordMs)}</span>
            </button>
          )}
          <ToolbarMenu label="More" text="More" icon={icons.more} title="More controls" items={moreItems} onChosen={focusScreen} />
        </div>
        <div className="session-toolbar__actions">
          <button className="session-toolbar__disconnect" type="button" onClick={onDisconnect}>Disconnect</button>
        </div>
      </div>
      {filesOpen && (
        <FilesPanel
          tab={filesTab}
          onTab={openFilesTab}
          transfers={transfers}
          onAccept={acceptOffer}
          onReject={rejectOffer}
          onSend={(files, dir) => void sendFiles(files, dir)}
          folder={folder}
          onBrowse={browse}
          onPull={pull}
          onMakeDir={(path, name) => { controls.makeDir(path, name); refreshSoon(); }}
          onDelete={(path) => { controls.deletePath(path); refreshSoon(); }}
          onRename={(path, name) => { controls.renamePath(path, name); refreshSoon(); }}
          onClose={() => { setFilesOpen(false); focusScreen(); }}
        />
      )}
      <ChatPanel open={chatOpen} messages={chat} onSend={sendChat} onClose={() => { setChatOpen(false); focusScreen(); }} />
      {notice && <div className="web-notice" role="status">{notice}</div>}
      <div className="session-security-chip mono">{viewOnly ? "View only · Encrypted" : "Encrypted"}</div>
    </main>
  );
}
