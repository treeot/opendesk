// Invite links let someone start a connection from a shared link or code
// instead of typing a 9-digit ID. The link carries the device ID and,
// optionally, the relay to reach it on. It never carries a password.

export interface Invite { id: string; relay?: string }

/** Build an `opendesk://connect?...` invite for this device. */
export function buildInviteLink(id: string, relay?: string): string {
  const base = `opendesk://connect?id=${id}`;
  return relay ? `${base}&relay=${encodeURIComponent(relay)}` : base;
}

/** Parse a pasted invite link, URL, or bare 9-digit ID. Returns null when the
 * text holds no usable 9-digit ID. */
export function parseInvite(text: string): Invite | null {
  const trimmed = text.trim();
  // A bare ID, with or without the grouping spaces.
  const bare = trimmed.replace(/[\s-]/g, "");
  if (/^\d{9}$/.test(bare)) return { id: bare };
  // Any link/URL carrying an id= query parameter.
  const idMatch = trimmed.match(/[?&]id=(\d{9})(?!\d)/);
  if (idMatch) {
    const relayMatch = trimmed.match(/[?&]relay=([^&\s]+)/);
    let relay: string | undefined;
    if (relayMatch) { try { relay = decodeURIComponent(relayMatch[1]); } catch { relay = relayMatch[1]; } }
    return { id: idMatch[1], relay };
  }
  return null;
}
