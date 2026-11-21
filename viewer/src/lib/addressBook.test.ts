import { describe, expect, it } from "vitest";
import type { Contact } from "./api";
import { allTags, buildSections, describeImport, matchesQuery, parseTags, RECENT_LIMIT, sortContacts, tagMatches } from "./addressBook";

const device = (id: string, alias: string, extra: Partial<Contact> = {}): Contact => ({ id, alias, last_connected: null, ...extra });
const none = new Set<string>();
const titles = (sections: { title: string }[]) => sections.map((section) => section.title);
const ids = (contacts: Contact[]) => contacts.map((contact) => contact.id);

describe("parseTags", () => {
  it("splits on commas, trims, nests with slashes and drops duplicates", () => {
    expect(parseTags(" Home, Clients / Acme ,, home, clients/acme, /Work/ ")).toEqual(["Home", "Clients/Acme", "Work"]);
    expect(parseTags("")).toEqual([]);
  });
});

describe("tags", () => {
  it("matches a tag and anything nested under it, case-insensitively", () => {
    expect(tagMatches("Clients/Acme", "clients")).toBe(true);
    expect(tagMatches("Clients", "Clients")).toBe(true);
    expect(tagMatches("ClientsX", "Clients")).toBe(false);
    expect(tagMatches("Clients", "Clients/Acme")).toBe(false);
  });
  it("lists every tag and the parents of nested ones", () => {
    const contacts = [device("111111111", "A", { tags: ["Clients/Acme", "Home"] }), device("222222222", "B", { tags: ["clients/Beta"] })];
    expect(allTags(contacts)).toEqual(["Clients", "Clients/Acme", "clients/Beta", "Home"]);
    expect(allTags([device("333333333", "C")])).toEqual([]);
  });
});

describe("matchesQuery", () => {
  const office = device("123456789", "Office Mac", { notes: "Rack 2, ask Sam", tags: ["Clients/Acme"] });
  it("searches name, ID, notes and tags", () => {
    expect(matchesQuery(office, "office")).toBe(true);
    expect(matchesQuery(office, "4567")).toBe(true);
    expect(matchesQuery(office, "123 456 789")).toBe(true);
    expect(matchesQuery(office, "sam")).toBe(true);
    expect(matchesQuery(office, "acme")).toBe(true);
    expect(matchesQuery(office, "  ")).toBe(true);
  });
  it("needs every word to match", () => {
    expect(matchesQuery(office, "mac acme")).toBe(true);
    expect(matchesQuery(office, "mac linux")).toBe(false);
  });
  it("copes with old contacts that have no notes or tags", () => {
    expect(matchesQuery(device("987654321", "Laptop"), "lap")).toBe(true);
  });
});

describe("sortContacts", () => {
  it("puts favorites first, then online devices, then sorts by name", () => {
    const contacts = [device("1", "zeta"), device("2", "Alpha"), device("3", "Mid", { favorite: true }), device("4", "beta")];
    expect(ids(sortContacts(contacts, new Set(["1"])))).toEqual(["3", "1", "2", "4"]);
  });
});

describe("buildSections", () => {
  const now = 1_700_000_000;
  const contacts = [
    device("100000001", "Fav", { favorite: true, last_connected: now }),
    device("100000002", "Newest", { last_connected: now - 10, tags: ["Home"] }),
    device("100000003", "Older", { last_connected: now - 100, tags: ["Clients/Acme"] }),
    device("100000004", "Never", { tags: ["Clients/Acme", "Home"] }),
  ];
  const filter = { query: "", tag: null, groupByTag: false };

  it("shows favorites, recent and the rest, each device once", () => {
    const sections = buildSections(contacts, filter, none);
    expect(titles(sections)).toEqual(["Favorites", "Recent", "Other devices"]);
    expect(ids(sections[0].contacts)).toEqual(["100000001"]);
    expect(ids(sections[1].contacts)).toEqual(["100000002", "100000003"]);
    expect(ids(sections[2].contacts)).toEqual(["100000004"]);
  });

  it("caps the recent section", () => {
    const many = Array.from({ length: RECENT_LIMIT + 2 }, (_, i) => device(`2000000${String(i).padStart(2, "0")}`, `D${i}`, { last_connected: now - i }));
    const sections = buildSections(many, filter, none);
    expect(sections[0].contacts).toHaveLength(RECENT_LIMIT);
    expect(sections[0].contacts[0].alias).toBe("D0");
    expect(sections[1].contacts).toHaveLength(2);
  });

  it("calls everything All devices when nothing is pinned or recent", () => {
    expect(titles(buildSections([device("100000004", "Never")], filter, none))).toEqual(["All devices"]);
    expect(buildSections([], filter, none)).toEqual([]);
  });

  it("flattens to one result list while searching or filtering by tag", () => {
    expect(titles(buildSections(contacts, { ...filter, query: "e" }, none))).toEqual(["Results"]);
    const byTag = buildSections(contacts, { ...filter, tag: "Clients" }, none);
    expect(ids(byTag[0].contacts)).toEqual(["100000004", "100000003"]);
    expect(buildSections(contacts, { ...filter, query: "nothing like this" }, none)).toEqual([]);
  });

  it("groups by tag with untagged devices last", () => {
    const sections = buildSections(contacts, { ...filter, groupByTag: true }, none);
    expect(titles(sections)).toEqual(["Clients/Acme", "Home", "Untagged"]);
    expect(ids(sections[0].contacts)).toEqual(["100000004", "100000003"]);
    expect(ids(sections[1].contacts)).toContain("100000004");
    expect(ids(sections[2].contacts)).toEqual(["100000001"]);
  });

  it("combines grouping with a search", () => {
    const sections = buildSections(contacts, { query: "never", tag: null, groupByTag: true }, none);
    expect(titles(sections)).toEqual(["Clients/Acme", "Home"]);
  });
});

describe("describeImport", () => {
  it("summarizes devices and passwords", () => {
    expect(describeImport({ added: 2, updated: 1, unchanged: 0, passwords: { saved: 1, kept_existing: 0, failed: 0 } })).toBe("Imported: 2 devices added, 1 updated. 1 password saved to the keyring.");
    expect(describeImport({ added: 0, updated: 0, unchanged: 3, passwords: { saved: 0, kept_existing: 2, failed: 1 } })).toBe("Imported: 3 already up to date. 2 saved passwords kept instead of the file's. 1 password could not be saved.");
    expect(describeImport({ added: 0, updated: 0, unchanged: 0, passwords: { saved: 0, kept_existing: 0, failed: 0 } })).toBe("The file had no devices.");
  });
});
