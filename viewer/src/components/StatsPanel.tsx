import type { LinkStats } from "../lib/api";
import { codecName, formatBitrate, formatLoss, type StreamStats } from "../lib/stats";

interface StatsPanelProps {
  stream: StreamStats;
  /** Path, round trip and loss; absent until the first link report. */
  link?: LinkStats;
  onClose: () => void;
}

/** The toggleable session stats overlay: what the stream and link are doing right now. */
export default function StatsPanel({ stream, link, onClose }: StatsPanelProps) {
  const rows: [string, string][] = [
    ["Frame rate", `${stream.fps} fps`],
    ["Bitrate", formatBitrate(stream.bitrateBps)],
    ["Round trip", link?.rtt_ms === null || link?.rtt_ms === undefined ? "Not measured" : `${link.rtt_ms} ms`],
    ["Packet loss", link ? formatLoss(link.path, link.loss_pct) : "Not measured"],
    ["Codec", codecName(stream.codec)],
    ["Resolution", stream.width && stream.height ? `${stream.width}×${stream.height}` : "None yet"],
    ["Connection", link ? (link.path === "direct" ? "Direct" : "Relayed") : "Not known yet"],
  ];
  return (
    <section className="session-stats" aria-label="Session stats">
      <div className="session-stats__head">
        <span className="session-stats__title">Session stats</span>
        <button type="button" className="session-stats__close" aria-label="Hide session stats" onClick={onClose}>×</button>
      </div>
      <dl className="session-stats__rows">
        {rows.map(([label, value]) => (
          <div key={label} className="session-stats__row">
            <dt>{label}</dt>
            <dd className="mono">{value}</dd>
          </div>
        ))}
      </dl>
    </section>
  );
}
