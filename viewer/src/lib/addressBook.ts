import type { Contact } from "./api";

/** One block of devices in the address book, e.g. "Favorites" or a tag. */
export interface ContactSection { key: string; title: string; contacts: Contact[] }

export interface AddressBookFilter {
  /** Free text matched against name, ID, notes and tags. */
  query: string;
  /** Only show devices with this tag (or a tag nested under it); null for all. */
  tag: string | null;
  /** Show one section per tag instead of Favorites / Recent / All. */
  groupByTag: boolean;
}

/** How many recently connected devices the "Recent" section shows. */
export const RECENT_LIMIT = 4;

const fold = (text: string) => text.toLowerCase();

/** Split a comma-separated tag field into clean tags ("a / b" becomes "a/b"). */
export const parseTags = (text: string): string[] => {
  const tags: string[] = [];
  for (const raw of text.split(",")) {
    const tag = raw.split("/").map((part) => part.trim()).filter(Boolean).join("/");
    if (tag && !tags.some((existing) => fold(existing) === fold(tag))) tags.push(tag);
  }
  return tags;
};

/** True when `tag` is `filter` itself or nested under it ("Clients/Acme" is under "Clients"). */
export const tagMatches = (tag: string, filter: string) => {
  const a = fold(tag);
  const b = fold(filter);
  return a === b || a.startsWith(`${b}/`);
};

/** Every tag in use plus the parents of nested tags, sorted. */
export const allTags = (contacts: Contact[]): string[] => {
  const seen = new Map<string, string>();
  for (const contact of contacts) {
    for (const tag of contact.tags ?? []) {
      const parts = tag.split("/");
      for (let depth = 1; depth <= parts.length; depth += 1) {
        const prefix = parts.slice(0, depth).join("/");
        if (!seen.has(fold(prefix))) seen.set(fold(prefix), prefix);
      }
    }
  }
  return [...seen.values()].sort((a, b) => a.localeCompare(b, undefined, { sensitivity: "base" }));
};

/**
 * Every whitespace-separated word of `query` must appear somewhere in the
 * device's name, ID, notes or tags. IDs also match with their spaces left in
 * ("123 456").
 */
export const matchesQuery = (contact: Contact, query: string) => {
  const words = fold(query).split(/\s+/).filter(Boolean);
  if (words.length === 0) return true;
  const digits = words.join("");
  if (/^\d+$/.test(digits) && contact.id.includes(digits)) return true;
  const haystack = fold([contact.alias, contact.id, contact.notes ?? "", ...(contact.tags ?? [])].join("\n"));
  return words.every((word) => haystack.includes(word));
};

const matchesFilter = (contact: Contact, filter: AddressBookFilter) =>
  matchesQuery(contact, filter.query)
  && (filter.tag === null || (contact.tags ?? []).some((tag) => tagMatches(tag, filter.tag as string)));

/** Favorites first, then online devices, then by name. */
export const sortContacts = (contacts: Contact[], online: ReadonlySet<string>) =>
  [...contacts].sort((a, b) =>
    Number(Boolean(b.favorite)) - Number(Boolean(a.favorite))
    || Number(online.has(b.id)) - Number(online.has(a.id))
    || a.alias.localeCompare(b.alias, undefined, { sensitivity: "base" })
    || a.id.localeCompare(b.id));

/**
 * Lay out the address book. With a search or tag filter it is one flat list
 * of matches. Grouped by tag, a device appears under each of its tags (its
 * full tag, not the parents) and untagged ones come last. Otherwise it is
 * Favorites, then the most recently connected devices, then everything else,
 * with each device shown once. Empty sections are left out.
 */
export const buildSections = (contacts: Contact[], filter: AddressBookFilter, online: ReadonlySet<string>): ContactSection[] => {
  const matches = sortContacts(contacts.filter((contact) => matchesFilter(contact, filter)), online);
  const nonEmpty = (sections: ContactSection[]) => sections.filter((section) => section.contacts.length > 0);
  if (filter.groupByTag) {
    const tags = [...new Set(matches.flatMap((contact) => contact.tags ?? []))].sort((a, b) => a.localeCompare(b, undefined, { sensitivity: "base" }));
    return nonEmpty([
      ...tags.map((tag) => ({ key: `tag:${tag}`, title: tag, contacts: matches.filter((contact) => (contact.tags ?? []).includes(tag)) })),
      { key: "untagged", title: "Untagged", contacts: matches.filter((contact) => (contact.tags ?? []).length === 0) },
    ]);
  }
  if (filter.query.trim() || filter.tag !== null) return nonEmpty([{ key: "results", title: "Results", contacts: matches }]);
  const favorites = matches.filter((contact) => contact.favorite);
  const recent = matches
    .filter((contact) => !contact.favorite && contact.last_connected != null)
    .sort((a, b) => (b.last_connected ?? 0) - (a.last_connected ?? 0))
    .slice(0, RECENT_LIMIT);
  const shown = new Set([...favorites, ...recent].map((contact) => contact.id));
  const rest = matches.filter((contact) => !shown.has(contact.id));
  return nonEmpty([
    { key: "favorites", title: "Favorites", contacts: favorites },
    { key: "recent", title: "Recent", contacts: recent },
    { key: "all", title: favorites.length + recent.length > 0 ? "Other devices" : "All devices", contacts: rest },
  ]);
};

/** A short sentence describing an import, for a notification. */
export const describeImport = (summary: { added: number; updated: number; unchanged: number; passwords: { saved: number; kept_existing: number; failed: number } }) => {
  const plural = (count: number, word: string) => `${count} ${word}${count === 1 ? "" : "s"}`;
  const parts: string[] = [];
  if (summary.added) parts.push(`${plural(summary.added, "device")} added`);
  if (summary.updated) parts.push(`${summary.updated} updated`);
  if (summary.unchanged) parts.push(`${summary.unchanged} already up to date`);
  let text = parts.length ? `Imported: ${parts.join(", ")}.` : "The file had no devices.";
  const { saved, kept_existing: kept, failed } = summary.passwords;
  if (saved) text += ` ${plural(saved, "password")} saved to the keyring.`;
  if (kept) text += ` ${plural(kept, "saved password")} kept instead of the file's.`;
  if (failed) text += ` ${plural(failed, "password")} could not be saved.`;
  return text;
};
