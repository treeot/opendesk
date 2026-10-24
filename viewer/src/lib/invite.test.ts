import { describe, expect, it } from "vitest";
import { buildInviteLink, parseInvite } from "./invite";

describe("invite", () => {
  it("builds a link with and without a relay", () => {
    expect(buildInviteLink("481902375")).toBe("opendesk://connect?id=481902375");
    expect(buildInviteLink("481902375", "wss://relay.example/ws")).toBe("opendesk://connect?id=481902375&relay=wss%3A%2F%2Frelay.example%2Fws");
  });

  it("parses a bare id, with or without grouping", () => {
    expect(parseInvite("481902375")).toEqual({ id: "481902375" });
    expect(parseInvite("481 902 375")).toEqual({ id: "481902375" });
    expect(parseInvite("  481-902-375 ")).toEqual({ id: "481902375" });
  });

  it("parses an invite link and its relay", () => {
    expect(parseInvite("opendesk://connect?id=481902375")).toEqual({ id: "481902375", relay: undefined });
    expect(parseInvite("opendesk://connect?id=481902375&relay=wss%3A%2F%2Fr%2Fws")).toEqual({ id: "481902375", relay: "wss://r/ws" });
  });

  it("rejects text without a 9-digit id", () => {
    expect(parseInvite("1234")).toBeNull();
    expect(parseInvite("hello")).toBeNull();
    expect(parseInvite("opendesk://connect?id=12345")).toBeNull();
  });
});
