import { useEffect, useId, useRef, useState, type CSSProperties, type KeyboardEvent, type ReactNode } from "react";
import { createPortal } from "react-dom";

export interface MenuItem {
  id: string;
  label: string;
  /** Set for on/off items: shows "On" or "Off" and aria-checked. */
  on?: boolean;
  danger?: boolean;
  onSelect(): void;
}

export interface ToolbarMenuProps {
  /** Accessible name of the button, e.g. "Special keys". */
  label: string;
  /** Short visible text under the icon. */
  text: string;
  icon: ReactNode;
  title?: string;
  items: MenuItem[];
  /** Called after an item is chosen, so focus can go back to the remote screen. */
  onChosen?(): void;
}

/**
 * A toolbar button that opens a menu, styled like the desktop viewer's
 * "More" menu. Arrow keys move between items, Escape closes, and a click
 * outside closes. The menu is placed outside the toolbar, which scrolls
 * sideways on narrow screens and would clip it.
 */
export default function ToolbarMenu({ label, text, icon, title, items, onChosen }: ToolbarMenuProps) {
  const [position, setPosition] = useState<CSSProperties | null>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const menuId = useId();
  const open = position !== null;

  const toggle = () => {
    if (open) {
      setPosition(null);
      return;
    }
    const rect = buttonRef.current?.getBoundingClientRect();
    if (!rect) return;
    setPosition({ position: "fixed", top: rect.bottom + 10, right: Math.max(8, window.innerWidth - rect.right) });
  };

  useEffect(() => {
    if (!open) return;
    menuRef.current?.querySelector<HTMLButtonElement>("button")?.focus();
    const outside = (event: PointerEvent) => {
      const target = event.target as Node;
      if (!menuRef.current?.contains(target) && !buttonRef.current?.contains(target)) setPosition(null);
    };
    const away = () => setPosition(null);
    document.addEventListener("pointerdown", outside);
    window.addEventListener("resize", away);
    return () => {
      document.removeEventListener("pointerdown", outside);
      window.removeEventListener("resize", away);
    };
  }, [open]);

  const onMenuKey = (event: KeyboardEvent) => {
    const entries = Array.from(menuRef.current?.querySelectorAll<HTMLButtonElement>("button") ?? []);
    const index = entries.indexOf(document.activeElement as HTMLButtonElement);
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      setPosition(null);
      buttonRef.current?.focus();
    } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const step = event.key === "ArrowDown" ? 1 : -1;
      entries[(index + step + entries.length) % entries.length]?.focus();
    } else if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      entries[event.key === "Home" ? 0 : entries.length - 1]?.focus();
    } else if (event.key === "Tab") {
      event.preventDefault();
      setPosition(null);
      buttonRef.current?.focus();
    }
  };

  // Inside the session screen, so the menu still shows in fullscreen.
  const host = buttonRef.current?.closest(".session-screen") ?? document.body;

  return (
    <>
      <button
        ref={buttonRef}
        className="session-toolbar__icon"
        type="button"
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        title={title ?? label}
        onClick={toggle}
      >
        {icon}
        <span className="session-toolbar__icon-text" aria-hidden="true">{text}</span>
      </button>
      {position && createPortal(
        <div ref={menuRef} className="session-toolbar__more web-menu" id={menuId} role="menu" aria-label={label} style={position} onKeyDown={onMenuKey}>
          {items.map((item) => (
            <button
              key={item.id}
              className="session-toolbar__more-item"
              type="button"
              role={item.on === undefined ? "menuitem" : "menuitemcheckbox"}
              aria-checked={item.on}
              data-tone={item.danger ? "danger" : undefined}
              onClick={() => { setPosition(null); item.onSelect(); onChosen?.(); }}
            >
              <span className="session-toolbar__more-label">{item.label}</span>
              {item.on !== undefined && <span className="session-toolbar__more-state" aria-hidden="true">{item.on ? "On" : "Off"}</span>}
            </button>
          ))}
        </div>,
        host,
      )}
    </>
  );
}
