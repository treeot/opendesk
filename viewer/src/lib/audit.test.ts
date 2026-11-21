import { describe, expect, it } from "vitest";
import { auditDetails, auditLabel, formatBytes } from "./audit";
import type { AuditEvent } from "./api";

const entry = (fields: Partial<AuditEvent>): AuditEvent => ({
  at: 0, event: "session_start", peer_id: null, address: null, detail: null,
  duration_seconds: null, filename: null, size_bytes: null, path: null, ...fields,
});

describe("auditLabel", () => {
  it("names known events in plain words", () => {
    expect(auditLabel("auth_failed")).toBe("Wrong password");
    expect(auditLabel("file_upload_completed")).toBe("File received");
    expect(auditLabel("secure_attention")).toBe("Ctrl+Alt+Del requested");
  });

  it("falls back to readable words for unknown events", () => {
    expect(auditLabel("some_future_event")).toBe("Some future event");
    expect(auditLabel("")).toBe("Unknown event");
  });
});

describe("auditDetails", () => {
  it("lists file, place and outcome", () => {
    expect(auditDetails(entry({ event: "file_upload_completed", filename: "a.pdf", size_bytes: 2048, path: "/tmp", detail: "done" })))
      .toEqual(["a.pdf (2.0 KB)", "/tmp", "done"]);
  });

  it("tolerates entries written before path existed", () => {
    const old = { ...entry({ duration_seconds: 5 }) } as Partial<AuditEvent>;
    delete old.path;
    expect(auditDetails(old as AuditEvent)).toEqual(["5s"]);
  });
});

describe("formatBytes", () => {
  it("scales units", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(5 * 1024 * 1024)).toBe("5.0 MB");
    expect(formatBytes(300 * 1024)).toBe("300 KB");
  });
});
