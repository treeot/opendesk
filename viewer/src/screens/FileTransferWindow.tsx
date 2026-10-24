import { useCallback, useEffect } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { emit } from "@tauri-apps/api/event";
import FileTransfer from "../components/FileTransfer";

/**
 * The file-transfer pop-out window (AnyDesk-style). It owns the dual-pane
 * browser and all transfer state, so it never misses an offer or progress
 * event. Closing it only hides the window: the session keeps it alive so the
 * next incoming file can reveal it again.
 */
export default function FileTransferWindow() {
  useEffect(() => {
    document.title = "File transfer · OpenDesk";
    const self = getCurrentWindow();
    // X hides instead of destroying, so file events are never missed.
    const closing = self.onCloseRequested((event) => {
      event.preventDefault();
      void self.hide();
      void emit("file-window-visibility", false);
    });
    return () => { void closing.then((off) => off()); };
  }, []);

  // FileTransfer asks to open on an incoming offer, a transfer, or a drop;
  // reflect that by showing or hiding the real window.
  const onOpenChange = useCallback((open: boolean) => {
    const self = getCurrentWindow();
    if (open) { void self.show(); void self.setFocus(); }
    else void self.hide();
    void emit("file-window-visibility", open);
  }, []);

  return (
    <main className="file-window">
      <FileTransfer variant="window" open onOpenChange={onOpenChange} />
    </main>
  );
}
