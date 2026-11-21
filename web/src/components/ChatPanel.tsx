import { useEffect, useRef, useState } from "react";

export interface ChatEntry {
  from: "me" | "peer";
  text: string;
  at: number;
}

/** UTF-8 bytes a chat message may use (protocol::chat::MAX_CHAT). */
export const MAX_CHAT_BYTES = 8192;

export interface ChatPanelProps {
  open: boolean;
  messages: ChatEntry[];
  /** Returns false when the message could not be sent. */
  onSend(text: string): boolean;
  onClose(): void;
}

/**
 * The chat with the person at the host, matching the desktop viewer's panel.
 * It stays mounted for the whole session and is only hidden while closed.
 */
export default function ChatPanel({ open, messages, onSend, onClose }: ChatPanelProps) {
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<string>();
  const listRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (open && listRef.current) listRef.current.scrollTop = listRef.current.scrollHeight;
  }, [open, messages]);

  useEffect(() => {
    if (open) inputRef.current?.focus();
  }, [open]);

  const submit = () => {
    const text = draft.trim();
    if (!text) return;
    if (new TextEncoder().encode(text).length > MAX_CHAT_BYTES) {
      setError("That message is too long.");
      return;
    }
    if (!onSend(text)) {
      setError("Not connected. Try again in a moment.");
      return;
    }
    setError(undefined);
    setDraft("");
  };

  return (
    <section className="chat-panel web-chat" aria-label="Chat" hidden={!open} onKeyDown={(event) => { if (event.key === "Escape") onClose(); }}>
      <div className="chat-panel__header">
        <span className="eyebrow">CHAT</span>
        <button className="icon-button chat-panel__close" type="button" onClick={onClose} aria-label="Close chat">×</button>
      </div>
      <div className="chat-panel__list" ref={listRef} role="log" aria-live="polite">
        {messages.length === 0 && <p className="chat-panel__empty">No messages yet.</p>}
        {messages.map((message, index) => (
          <div className={`chat-panel__message chat-panel__message--${message.from}`} key={`${message.at}-${index}`}>
            <span className="sr-only">{message.from === "me" ? "You:" : "Remote:"}</span>
            <span className="chat-panel__bubble">{message.text}</span>
          </div>
        ))}
      </div>
      {error && <p className="web-chat__error" role="alert">{error}</p>}
      <form className="chat-panel__compose" onSubmit={(event) => { event.preventDefault(); submit(); }}>
        <input
          ref={inputRef}
          className="chat-panel__input"
          aria-label="Message"
          placeholder="Type a message"
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
        />
        <button className="primary-button" type="submit">Send</button>
      </form>
    </section>
  );
}
