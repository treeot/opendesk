import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import * as api from "../lib/api";
import { friendlyError, useNotify } from "../lib/notify";
import { formatFileSize } from "./FileTransfer";

/**
 * Asks the host user about files the viewer sends. Mounted for the whole
 * host session; the viewer side has its own panel (FileTransfer). Offers
 * queue up and are shown one at a time.
 */
export default function IncomingFiles() {
  const [offers, setOffers] = useState<api.FileOffer[]>([]);
  const [busy, setBusy] = useState(false);
  const accepted = useRef(new Map<number, string>());
  const declineRef = useRef<HTMLButtonElement>(null);
  const notify = useNotify();
  const notifyRef = useRef(notify);
  notifyRef.current = notify;

  useEffect(() => {
    let active = true;
    const stops: (() => void)[] = [];
    const keep = (listener: Promise<() => void>) => void listener.then((stop) => { if (active) stops.push(stop); else stop(); }).catch(() => {});
    keep(api.onFileOffer((offer) => setOffers((current) => [...current.filter(({ id }) => id !== offer.id), offer])));
    keep(api.onFileDone(({ id, path }) => {
      const name = accepted.current.get(id);
      if (name === undefined) return;
      accepted.current.delete(id);
      notifyRef.current(path ? `Saved ${name} to ${path}` : `Received ${name}.`, "info");
    }));
    keep(api.onFileError(({ id, message }) => {
      const name = accepted.current.get(id);
      setOffers((current) => current.filter((offer) => offer.id !== id));
      if (name === undefined) return;
      accepted.current.delete(id);
      notifyRef.current(`Could not receive ${name}: ${friendlyError(message)}`);
    }));
    return () => { active = false; stops.forEach((stop) => stop()); };
  }, []);

  const offer = offers[0];
  // Decline is the safe default, as for connection requests.
  useEffect(() => { declineRef.current?.focus(); }, [offer?.id]);
  if (!offer) return null;

  const answer = async (accept: boolean) => {
    if (busy) return;
    setBusy(true);
    try {
      if (accept) accepted.current.set(offer.id, offer.name);
      await api.respondFile(offer.id, accept);
    } catch (error) {
      accepted.current.delete(offer.id);
      notify(friendlyError(error));
    } finally {
      setOffers((current) => current.filter(({ id }) => id !== offer.id));
      setBusy(false);
    }
  };
  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key !== "Escape") return;
    event.preventDefault();
    void answer(false);
  };

  return (
    <div className="incoming-backdrop" role="presentation">
      <section className="incoming-modal" role="dialog" aria-modal="true" aria-labelledby="incoming-file-title" onKeyDown={onKeyDown}>
        <span className="eyebrow">INCOMING FILE</span>
        <h1 id="incoming-file-title">Accept this file?</h1>
        <p className="incoming-lead">{offer.from} wants to send you a file. Check where it will be saved before you accept.</p>
        <div className="request-details">
          <div><span className="request-label">FILE</span><strong className="request-peer">{offer.name}</strong><span>{formatFileSize(offer.size)}</span></div>
          <div><span className="request-label">SAVE TO</span><span className="request-dir" title={offer.dir}>{offer.dir}</span></div>
        </div>
        <div className="incoming-actions">
          <button ref={declineRef} className="ghost-button" type="button" disabled={busy} onClick={() => void answer(false)}>Decline</button>
          <button className="primary-button incoming-allow" type="button" disabled={busy} onClick={() => void answer(true)}>Accept</button>
        </div>
      </section>
    </div>
  );
}
