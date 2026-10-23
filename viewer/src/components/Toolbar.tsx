import { useEffect, useState, type ReactNode } from "react";
import type { LinkStats, MonitorInfo, Status } from "../lib/api";

interface ToolbarProps {
  remoteId: string;
  /** "host" shows only the timer, clipboard toggle and Disconnect. */
  role?: "host" | "viewer";
  status?: Status;
  /** Path and round trip of the media; no badge until the first report. */
  link?: LinkStats;
  clipboardSync?: boolean;
  onClipboardSyncChange?: (enabled: boolean) => void;
  onDisconnect: () => void;
  monitors?: MonitorInfo[];
  selectedMonitor?: number;
  onMonitorChange?: (id: number) => void;
  /** The host offers virtual displays and this session may add another. */
  canAddDisplay?: boolean;
  onAddDisplay?: () => void;
  onRemoveDisplay?: (id: number) => void;
  canOpenMonitorWindow?: boolean;
  onOpenMonitorWindow?: (id: number) => void;
  canRestart?: boolean;
  onRestart?: () => void;
  canPrivacy?: boolean;
  privacyOn?: boolean;
  onPrivacyChange?: (on: boolean) => void;
  audioMuted?: boolean;
  onAudioMutedChange?: (muted: boolean) => void;
  recordingActive?: boolean;
  recordingElapsed?: number;
  onRecordingToggle?: () => void;
  filesOpen?: boolean;
  onFilesToggle?: () => void;
  chatOpen?: boolean;
  onChatToggle?: () => void;
  chatUnread?: number;
  forwardOpen?: boolean;
  onForwardToggle?: () => void;
  viewOnly?: boolean;
  onViewOnlyChange?: (viewOnly: boolean) => void;
  canSecureAttention?: boolean;
  onSecureAttention?: () => void;
  /** Selected bitrate cap in bps; 0 is automatic. */
  quality?: number;
  onQualityChange?: (bps: number) => void;
  /** Slide the bar out of view until the pointer reaches the top edge. */
  hidden?: boolean;
  onBarEnter?: () => void;
  onBarLeave?: () => void;
}

interface IconButtonProps { label: string; text: string; icon: ReactNode; onClick?: () => void; title?: string; pressed?: boolean; expanded?: boolean; tone?: "danger"; badge?: number }

function IconButton({ label, text, icon, onClick, title, pressed, expanded, tone, badge }: IconButtonProps) {
  return (
    <button className="session-toolbar__icon" type="button" aria-label={label} aria-pressed={pressed} aria-expanded={expanded} data-tone={tone} onClick={onClick} title={title ?? label}>
      {icon}
      {badge !== undefined && badge > 0 && <span className="session-toolbar__badge" aria-label={`${badge} unread`}>{badge > 99 ? "99+" : badge}</span>}
      <span className="session-toolbar__icon-text" aria-hidden="true">{text}</span>
    </button>
  );
}

// Icon paths from Lucide (https://lucide.dev, ISC License).
function Icon({ children }: { children: ReactNode }) {
  return <svg className="session-toolbar__svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">{children}</svg>;
}

const speaker = <path d="M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z" />;
const icons = {
  fullscreen: <Icon><path d="M8 3H5a2 2 0 0 0-2 2v3" /><path d="M21 8V5a2 2 0 0 0-2-2h-3" /><path d="M3 16v3a2 2 0 0 0 2 2h3" /><path d="M16 21h3a2 2 0 0 0 2-2v-3" /></Icon>,
  monitor: <Icon><rect width="20" height="14" x="2" y="3" rx="2" /><line x1="8" x2="16" y1="21" y2="21" /><line x1="12" x2="12" y1="17" y2="21" /></Icon>,
  gauge: <Icon><path d="m12 14 4-4" /><path d="M3.34 19a10 10 0 1 1 17.32 0" /></Icon>,
  newWindow: <Icon><rect width="18" height="14" x="3" y="5" rx="2" /><path d="M3 9h18" /><path d="M14 13h4v4" /><path d="M18 13l-5 5" /></Icon>,
  restart: <Icon><path d="M21 12a9 9 0 1 1-2.64-6.36" /><path d="M21 3v6h-6" /></Icon>,
  privacy: <Icon><path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12Z" /><circle cx="12" cy="12" r="3" /><path d="M3 3l18 18" /></Icon>,
  clipboard: <Icon><rect width="8" height="4" x="8" y="2" rx="1" /><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2" /></Icon>,
  sound: <Icon>{speaker}<path d="M16 9a5 5 0 0 1 0 6" /><path d="M19.364 18.364a9 9 0 0 0 0-12.728" /></Icon>,
  muted: <Icon>{speaker}<line x1="22" x2="16" y1="9" y2="15" /><line x1="16" x2="22" y1="9" y2="15" /></Icon>,
  files: <Icon><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" /></Icon>,
  remove: <Icon><rect width="20" height="14" x="2" y="3" rx="2" /><line x1="9" x2="15" y1="10" y2="10" /><line x1="12" x2="12" y1="17" y2="21" /></Icon>,
  addMonitor: <Icon><rect width="20" height="14" x="2" y="3" rx="2" /><line x1="12" x2="12" y1="7" y2="13" /><line x1="9" x2="15" y1="10" y2="10" /><line x1="8" x2="16" y1="21" y2="21" /></Icon>,
  record: <Icon><circle cx="12" cy="12" r="7" fill="currentColor" stroke="none" /></Icon>,
  stop: <Icon><rect x="6" y="6" width="12" height="12" rx="2" fill="currentColor" stroke="none" /></Icon>,
  chat: <Icon><path d="M7.9 20A9 9 0 1 0 4 16.1L2 22Z" /></Icon>,
  eye: <Icon><path d="M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0" /><circle cx="12" cy="12" r="3" /></Icon>,
  keyboard: <Icon><rect width="20" height="16" x="2" y="4" rx="2" /><path d="M6 8h.01M10 8h.01M14 8h.01M18 8h.01M6 12h.01M18 12h.01M8 16h8" /></Icon>,
  forward: <Icon><path d="M4 17V9a2 2 0 0 1 2-2h12" /><path d="m15 4 3 3-3 3" /><path d="M20 7v8a2 2 0 0 1-2 2H6" /><path d="m9 20-3-3 3-3" /></Icon>,
};

/** Readable monitor name: Windows reports device paths like `\\.\DISPLAY2`. */
export function monitorName(name: string, index: number): string {
  const device = /^\\\\\.\\DISPLAY(\d+)$/i.exec(name.trim());
  if (device) return `Display ${device[1]}`;
  return name.trim() || `Display ${index + 1}`;
}

/** Largest virtual display a viewer asks for. */
export const MAX_DISPLAY = { width: 3840, height: 2160 } as const;

/**
 * Size of a virtual display that fills a `width` x `height` (CSS pixel)
 * window at `ratio` device pixels per CSS pixel: shrunk to fit 3840x2160
 * with its shape kept, and even on both sides. On a high-density screen
 * the host makes it HiDPI, so text keeps its usual size.
 */
export function virtualDisplaySize(width: number, height: number, ratio: number): { width: number; height: number; hidpi: boolean } {
  const scale = Number.isFinite(ratio) && ratio > 0 ? ratio : 1;
  let pixelsW = Math.max(1, Math.round(width * scale));
  let pixelsH = Math.max(1, Math.round(height * scale));
  const shrink = Math.min(1, MAX_DISPLAY.width / pixelsW, MAX_DISPLAY.height / pixelsH);
  pixelsW = Math.floor(pixelsW * shrink);
  pixelsH = Math.floor(pixelsH * shrink);
  return { width: pixelsW - (pixelsW % 2), height: pixelsH - (pixelsH % 2), hidpi: scale >= 1.5 };
}

const ADD_DISPLAY = "add-display";

/** Stream quality tiers: the bitrate cap in bps, 0 meaning automatic. */
export const QUALITY_TIERS: { label: string; bps: number }[] = [
  { label: "Auto", bps: 0 },
  { label: "High", bps: 20_000_000 },
  { label: "Balanced", bps: 8_000_000 },
  { label: "Data-saver", bps: 3_000_000 },
];

function formatDuration(seconds: number): string {
  const minutes = Math.floor(seconds / 60).toString().padStart(2, "0");
  return `${minutes}:${(seconds % 60).toString().padStart(2, "0")}`;
}

/** Round trips above this get the warning colour. */
export const SLOW_RTT_MS = 100;

const linkLabels = { direct: "Direct", relay: "Relayed" } as const;
const linkHelp = {
  direct: "Direct: video and input go straight between the two computers.",
  relay: "Relayed: a direct connection couldn't be made, so traffic passes through the relay server. It is still end-to-end encrypted, but usually slower.",
} as const;

export function LinkBadge({ link }: { link: LinkStats }) {
  const rtt = link.rtt_ms === null ? "—" : `${link.rtt_ms} ms`;
  const measured = link.rtt_ms === null ? "Round trip not measured yet (older versions don't report it)." : `Round trip ${link.rtt_ms} ms.`;
  return (
    <span className="session-toolbar__link mono" data-path={link.path} data-slow={link.rtt_ms !== null && link.rtt_ms > SLOW_RTT_MS} title={`${linkHelp[link.path]} ${measured}`}>
      <span className="session-toolbar__link-dot" aria-hidden="true" />
      <span className="session-toolbar__link-label">{linkLabels[link.path]}</span>
      <span className="session-toolbar__link-sep" aria-hidden="true">·</span>
      <span className="session-toolbar__link-rtt">{rtt}</span>
    </span>
  );
}

const connectionLabels: Record<Status, string> = { in_session: "Connected", connecting: "Reconnecting", online: "Connected", offline: "Disconnected" };

export default function Toolbar({ remoteId, role = "viewer", status = "in_session", link, clipboardSync = true, onClipboardSyncChange = () => {}, onDisconnect, monitors = [], selectedMonitor, onMonitorChange = () => {}, canAddDisplay = false, onAddDisplay = () => {}, onRemoveDisplay = () => {}, canOpenMonitorWindow = false, onOpenMonitorWindow = () => {}, audioMuted = false, onAudioMutedChange = () => {}, recordingActive = false, recordingElapsed = 0, onRecordingToggle = () => {}, filesOpen = false, onFilesToggle = () => {}, chatOpen = false, onChatToggle = () => {}, chatUnread = 0, forwardOpen = false, onForwardToggle = () => {}, viewOnly = false, onViewOnlyChange = () => {}, canSecureAttention = false, onSecureAttention = () => {}, canRestart = false, onRestart = () => {}, canPrivacy = false, privacyOn = false, onPrivacyChange = () => {}, quality = 0, onQualityChange = () => {}, hidden = false, onBarEnter = () => {}, onBarLeave = () => {} }: ToolbarProps) {
  const [elapsed, setElapsed] = useState(0);

  useEffect(() => {
    const startedAt = Date.now();
    const timer = window.setInterval(() => setElapsed(Math.floor((Date.now() - startedAt) / 1000)), 1000);
    return () => window.clearInterval(timer);
  }, []);

  const toggleFullscreen = () => {
    if (document.fullscreenElement) void document.exitFullscreen();
    else void document.documentElement.requestFullscreen?.();
  };

  const viewer = role === "viewer";
  const selectedVirtual = monitors.find((monitor) => monitor.id === (selectedMonitor ?? monitors[0]?.id) && monitor.is_virtual);
  const clipboard = <IconButton label="Sync clipboard" pressed={clipboardSync} title={clipboardSync ? "Clipboard sync is on. Click to turn it off." : "Clipboard sync is off. Click to turn it on."} onClick={() => onClipboardSyncChange(!clipboardSync)}text="Clipboard" icon={icons.clipboard} />;

  return (
    <>
      <div className={`session-toolbar${hidden ? " session-toolbar--hidden" : ""}`} role="toolbar" aria-label="Session controls" onMouseEnter={onBarEnter} onMouseLeave={onBarLeave}>
        {viewer && <div className="session-toolbar__connection">
          <span className="session-toolbar__connected-dot" data-status={status} title={connectionLabels[status]} aria-hidden="true" />
          <span className="sr-only">{connectionLabels[status]}</span>
          <span className="session-toolbar__remote mono">{remoteId}</span>
          {link && <LinkBadge link={link} />}
        </div>}
        <div className="session-toolbar__controls">
          {viewer && <IconButton label="Fullscreen" text="Fullscreen" icon={icons.fullscreen} onClick={toggleFullscreen} />}
          {viewer && monitors.length > 0 && <label className="session-toolbar__select-chip">
            <span className="sr-only">Monitor</span>
            {icons.monitor}
            <select aria-label="Monitor" value={selectedMonitor ?? monitors[0].id} onChange={(event) => { if (event.target.value === ADD_DISPLAY) onAddDisplay(); else onMonitorChange(Number(event.target.value)); }}>
              {monitors.map((monitor, index) => <option key={monitor.id} value={monitor.id}>{monitorName(monitor.name, index)}{monitor.width && monitor.height ? ` · ${monitor.width}×${monitor.height}` : ""}{monitor.is_primary ? " · Primary" : ""}{monitor.is_virtual ? " · Virtual" : ""}</option>)}
              {canAddDisplay && <option value={ADD_DISPLAY}>Add display…</option>}
            </select>
          </label>}
          {viewer && canAddDisplay && <IconButton label="Add screen" text="Add screen" title="Add a virtual screen to the other computer" icon={icons.addMonitor} onClick={onAddDisplay} />}
          {viewer && <label className="session-toolbar__select-chip" title="Cap the stream quality. Auto follows your connection.">
            <span className="sr-only">Quality</span>
            {icons.gauge}
            <select aria-label="Quality" value={quality} onChange={(event) => onQualityChange(Number(event.target.value))}>
              {QUALITY_TIERS.map((tier) => <option key={tier.bps} value={tier.bps}>{tier.label}</option>)}
            </select>
          </label>}
          {viewer && canOpenMonitorWindow && <IconButton label="Open monitor in a window" text="New window" title="Open the selected monitor in its own window" icon={icons.newWindow} onClick={() => onOpenMonitorWindow(selectedMonitor ?? monitors[0]?.id ?? 0)} />}
          {viewer && selectedVirtual && <IconButton label="Remove display" text="Remove" title="Remove this virtual display from the other computer" icon={icons.remove} onClick={() => onRemoveDisplay(selectedVirtual.id)} />}
          {clipboard}
          {viewer && <>
            <IconButton label="Mute audio" pressed={audioMuted} title={audioMuted ? "Audio is muted. Click to unmute." : "Audio is on. Click to mute."} onClick={() => onAudioMutedChange(!audioMuted)} text={audioMuted ? "Muted" : "Sound"} icon={audioMuted ? icons.muted : icons.sound} />
            <IconButton label="Files" pressed={filesOpen} expanded={filesOpen} title={filesOpen ? "Hide file transfer" : "Send or receive files"} onClick={onFilesToggle} text="Files" icon={icons.files} />
            <IconButton label="Chat" pressed={chatOpen} expanded={chatOpen} badge={chatUnread} title={chatOpen ? "Hide chat" : "Open chat"} onClick={onChatToggle} text="Chat" icon={icons.chat} />
            <IconButton label="Port forwarding" pressed={forwardOpen} expanded={forwardOpen} title={forwardOpen ? "Hide port forwarding" : "Forward a port (reach a service on the remote)"} onClick={onForwardToggle} text="Forward" icon={icons.forward} />
            <IconButton label="View only" pressed={viewOnly} title={viewOnly ? "View only is on. Click to send input again." : "View only is off. Click to stop sending input."} onClick={() => onViewOnlyChange(!viewOnly)} text="View only" icon={icons.eye} />
            {canSecureAttention && <IconButton label="Send Ctrl+Alt+Del" text="Ctrl+Alt+Del" title="Send Ctrl+Alt+Del to the other computer" icon={icons.keyboard} onClick={onSecureAttention} />}
            {canRestart && <IconButton label="Restart remote" text="Restart" title="Restart the other computer and reconnect when it comes back" icon={icons.restart} onClick={onRestart} />}
            {canPrivacy && <IconButton label="Privacy mode" pressed={privacyOn} title={privacyOn ? "Privacy mode is on: the other screen is blank and its local input is blocked. Click to turn off." : "Blank the other screen and block its local keyboard and mouse"} icon={icons.privacy} text="Privacy" onClick={() => onPrivacyChange(!privacyOn)} />}
            {recordingActive && <span className="session-toolbar__recording-status" aria-label="Recording elapsed"><span className="session-toolbar__recording-dot" aria-hidden="true" />{formatDuration(recordingElapsed)}</span>}
            <IconButton label={recordingActive ? "Stop recording" : "Record"} text={recordingActive ? "Stop" : "Record"} icon={recordingActive ? icons.stop : icons.record} tone="danger" onClick={onRecordingToggle} />
          </>}
        </div>
        <div className="session-toolbar__actions">
          <span className="session-toolbar__timer mono" aria-label="Session timer">{formatDuration(elapsed)}</span>
          <button className="session-toolbar__disconnect" type="button" onClick={onDisconnect}>Disconnect</button>
        </div>
      </div>
      <div className="session-security-chip mono">Encrypted</div>
    </>
  );
}

export type { ToolbarProps };
