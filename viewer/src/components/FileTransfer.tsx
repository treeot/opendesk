import { useEffect, useRef, useState } from "react";
import * as api from "../lib/api";
import FileBrowser from "./FileBrowser";

interface Transfer {
  id: number;
  name: string;
  size: number;
  direction: "send" | "receive";
  transferred: number;
  total: number;
  status: "transferring" | "done" | "error";
  error?: string;
  savedTo?: string;
}

interface PendingOffer extends api.FileOffer { responding?: boolean }

export function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = -1;
  do { value /= 1024; unit += 1; } while (value >= 1024 && unit < units.length - 1);
  return `${value.toFixed(value >= 10 || unit === 0 ? 0 : 1)} ${units[unit]}`;
}

function subscription<T>(subscribe: (handler: (payload: T) => void) => Promise<() => void>, handler: (payload: T) => void, active: () => boolean, add: (stop: () => void) => void) {
  void subscribe(handler).then((stop) => { if (active()) add(stop); else stop(); }).catch(() => {});
}

const baseName = (path: string) => path.split(/[\\/]/).pop() || path;

function transferStatus(transfer: Transfer): string {
  if (transfer.status === "error") return transfer.error ?? "Failed";
  if (transfer.status === "done") return transfer.savedTo ? `Saved to ${transfer.savedTo}` : "Sent";
  return `${formatFileSize(transfer.transferred)} / ${formatFileSize(transfer.total)}`;
}

export interface FileTransferProps {
  open: boolean;
  /** Called with true when an incoming offer or a transfer needs attention. */
  onOpenChange: (open: boolean) => void;
  /** "panel" overlays the session; "window" fills its own pop-out window. */
  variant?: "panel" | "window";
}

/**
 * Stays mounted for the whole session so offers and progress are never
 * missed; only the panel is hidden while closed. In "window" variant it fills
 * its own pop-out window and is always visible; the window chrome hides it.
 */
export default function FileTransfer({ open, onOpenChange, variant = "panel" }: FileTransferProps) {
  const isWindow = variant === "window";
  const visible = isWindow || open;
  const [message, setMessage] = useState("");
  const [offers, setOffers] = useState<PendingOffer[]>([]);
  const [transfers, setTransfers] = useState<Transfer[]>([]);
  const [dragging, setDragging] = useState(false);
  // Read through a ref so a new callback identity never resubscribes.
  const onOpenChangeRef = useRef(onOpenChange);
  onOpenChangeRef.current = onOpenChange;
  const seen = useRef(new Set<number>());
  /** Open the panel the first time a transfer appears; later progress never reopens it. */
  const announce = (id: number) => {
    if (seen.current.has(id)) return;
    seen.current.add(id);
    onOpenChangeRef.current(true);
  };

  useEffect(() => {
    let active = true;
    const stops: (() => void)[] = [];
    const add = (stop: () => void) => stops.push(stop);

    subscription(api.onFileOffer, (offer) => {
      seen.current.delete(offer.id);
      announce(offer.id);
      setOffers((current) => [...current.filter(({ id }) => id !== offer.id), offer]);
      setTransfers((current) => current.some(({ id }) => id === offer.id) ? current : [...current, {
        id: offer.id, name: offer.name, size: offer.size, direction: "receive", transferred: 0,
        total: offer.size, status: "transferring",
      }]);
    }, () => active, add);
    subscription(api.onFileProgress, (progress) => {
      announce(progress.id);
      setTransfers((current) => {
        const direction = progress.direction === "receive" ? "receive" : "send";
        if (!current.some(({ id }) => id === progress.id)) {
          return [...current, { id: progress.id, name: "File transfer", size: progress.total, direction, transferred: progress.transferred, total: progress.total, status: "transferring" }];
        }
        return current.map((transfer) => transfer.id === progress.id ? { ...transfer, transferred: progress.transferred, total: progress.total, size: progress.total, direction } : transfer);
      });
    }, () => active, add);
    subscription(api.onFileDone, ({ id, path }) => setTransfers((current) => current.map((transfer) => transfer.id === id ? { ...transfer, transferred: transfer.total, status: "done", savedTo: path ?? undefined } : transfer)), () => active, add);
    subscription(api.onFileError, ({ id, message: error }) => setTransfers((current) => current.map((transfer) => transfer.id === id ? { ...transfer, status: "error", error } : transfer)), () => active, add);
    return () => { active = false; stops.forEach((stop) => stop()); };
  }, []);

  const sendPath = async (path: string) => {
    setMessage("");
    try {
      const id = await api.sendFile(path);
      seen.current.add(id);
      setTransfers((current) => current.some((transfer) => transfer.id === id) ? current : [...current, { id, name: baseName(path), size: 0, direction: "send", transferred: 0, total: 0, status: "transferring" }]);
    } catch (error) { setMessage(String(error)); }
  };

  // Tauri delivers OS file drops for the whole window with real paths.
  useEffect(() => {
    let active = true;
    let stop: (() => void) | undefined;
    void api.onFileDrop(({ type, paths }) => {
      if (!active) return;
      setDragging(type === "over");
      if (type === "drop" && paths.length > 0) {
        onOpenChangeRef.current(true);
        for (const path of paths) void sendPath(path);
      }
    }).then((unlisten) => { if (active) stop = unlisten; else unlisten(); }).catch(() => {});
    return () => { active = false; stop?.(); };
  }, []);

  const choose = async () => {
    try {
      const path = await api.pickFile();
      if (path) await sendPath(path);
    } catch (error) { setMessage(String(error)); }
  };

  const answer = async (offer: PendingOffer, accept: boolean) => {
    setOffers((current) => current.map((item) => item.id === offer.id ? { ...item, responding: true } : item));
    try {
      await api.respondFile(offer.id, accept);
      setOffers((current) => current.filter(({ id }) => id !== offer.id));
      if (!accept) setTransfers((current) => current.filter(({ id }) => id !== offer.id));
    } catch (error) {
      setMessage(String(error));
      setOffers((current) => current.map((item) => item.id === offer.id ? { ...item, responding: false } : item));
    }
  };

  return <>
    {dragging && !visible && <div className="file-drop-hint" aria-hidden="true">Drop to send</div>}
    <section className={`file-transfer${isWindow ? " file-transfer--window" : ""}${dragging ? " file-transfer--dragging" : ""}`} aria-label="File transfer" hidden={!visible}>
      <div className="file-transfer__send">
        <div className="file-transfer__header"><span className="eyebrow">FILE TRANSFER</span>{!isWindow && <button className="icon-button file-transfer__close" type="button" onClick={() => onOpenChange(false)} aria-label="Close file transfer">×</button>}</div>
        <div className="file-transfer__control"><button className="primary-button" type="button" onClick={() => void choose()}>Send file…</button><small>or drop files anywhere on this window</small></div>
        {message && <p className="message" role="alert">{message}</p>}
      </div>
      {visible && <FileBrowser />}
      {offers.map((offer) => <div className="file-transfer__offer" role="dialog" aria-label={`Incoming file ${offer.name}`} key={offer.id}>
        <span className="eyebrow">INCOMING FILE</span><strong>{offer.name}</strong><span>{formatFileSize(offer.size)} · from {offer.from}</span>
        <div className="incoming-actions"><button className="ghost-button" type="button" disabled={offer.responding} onClick={() => void answer(offer, false)}>Reject</button><button className="primary-button" type="button" disabled={offer.responding} onClick={() => void answer(offer, true)}>Accept</button></div>
      </div>)}
      {transfers.length > 0 && <div className="file-transfer__list"><span className="eyebrow">TRANSFERS</span>{transfers.map((transfer) => <div className="file-transfer__row" key={transfer.id}>
        <div className="file-transfer__meta"><strong>{transfer.name}</strong><span>{transfer.direction === "receive" ? "Receiving" : "Sending"}</span></div>
        <span className="file-transfer__status" title={transferStatus(transfer)}>{transferStatus(transfer)}</span>
        <progress aria-label={`${transfer.name} progress`} value={transfer.total ? transfer.transferred : undefined} max={transfer.total || 1} />
      </div>)}</div>}
    </section>
  </>;
}
