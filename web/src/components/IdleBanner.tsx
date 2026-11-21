import { useEffect, useState } from "react";

export interface IdleBannerProps {
  /** When the host will end the session, in `Date.now()` milliseconds. */
  endsAt: number;
  onStay(): void;
}

/** The host's warning that it will end this unused session soon. */
export default function IdleBanner({ endsAt, onStay }: IdleBannerProps) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const tick = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(tick);
  }, [endsAt]);
  const left = Math.max(0, Math.ceil((endsAt - now) / 1000));
  const clock = `${Math.floor(left / 60)}:${(left % 60).toString().padStart(2, "0")}`;
  return (
    <div className="idle-banner idle-banner--web" role="alert">
      <span>No activity for a while. The host will end this session in <strong className="mono">{clock}</strong>.</span>
      <button className="primary-button" type="button" onClick={onStay}>Stay connected</button>
    </div>
  );
}
