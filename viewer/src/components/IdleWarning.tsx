import { useEffect, useRef, useState } from "react";
import * as api from "../lib/api";

/** "1:05" for 65 seconds. */
export const formatCountdown = (seconds: number) => `${Math.floor(seconds / 60)}:${(seconds % 60).toString().padStart(2, "0")}`;

/**
 * The host's warning that it will end this unused session soon. Input sent
 * to the host resets its timer, so the banner also goes away on a key press
 * or click on the remote screen; "Stay connected" tells the host without one.
 */
export default function IdleWarning() {
  const [endsAt, setEndsAt] = useState<number | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const banner = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let stop: (() => void) | undefined;
    const listener = api.onIdleWarning(({ seconds }) => { setNow(Date.now()); setEndsAt(Date.now() + seconds * 1000); });
    void listener.then((unlisten) => { stop = unlisten; });
    return () => { if (stop) stop(); else void listener.then((unlisten) => unlisten()); };
  }, []);

  useEffect(() => {
    if (endsAt === null) return;
    const tick = window.setInterval(() => setNow(Date.now()), 1000);
    // Input on the remote screen reaches the host and resets its timer.
    // Clicks on the banner or the toolbar do not.
    const dismiss = (event: Event) => {
      const target = event.target;
      if (target instanceof Node && banner.current?.contains(target)) return;
      if (target instanceof HTMLCanvasElement) setEndsAt(null);
    };
    window.addEventListener("keydown", dismiss, true);
    window.addEventListener("pointerdown", dismiss, true);
    return () => {
      window.clearInterval(tick);
      window.removeEventListener("keydown", dismiss, true);
      window.removeEventListener("pointerdown", dismiss, true);
    };
  }, [endsAt]);

  if (endsAt === null) return null;
  const left = Math.max(0, Math.ceil((endsAt - now) / 1000));
  const stay = () => { setEndsAt(null); void api.stayConnected().catch(() => undefined); };
  return (
    <div className="idle-banner" role="alert" ref={banner}>
      <span>No activity for a while. The host will end this session in <strong className="mono">{formatCountdown(left)}</strong>.</span>
      <button className="primary-button" type="button" onClick={stay}>Stay connected</button>
    </div>
  );
}
