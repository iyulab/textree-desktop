import { describe, it, expect } from "vitest";
import { emptyMessage, formatRecordedAt, isCurrent } from "./versionHistory.helpers";
import type { NoteVersion } from "./ipc";

function version(id: string, seconds = 0): NoteVersion {
  return { id, message: id, seconds, author: "Someone" };
}

describe("emptyMessage", () => {
  it("says what adding one would do rather than only that there is nothing", () => {
    expect(emptyMessage()).toContain("come back to it");
  });
});

describe("formatRecordedAt", () => {
  const now = new Date(2026, 7, 7, 15, 0, 0);

  it("drops the date for states recorded today", () => {
    const today = new Date(2026, 7, 7, 9, 5, 0).getTime() / 1000;
    const shown = formatRecordedAt(today, now);
    expect(shown).not.toMatch(/Aug|\d{4}/);
    expect(shown).toMatch(/9/);
  });

  it("keeps the date for earlier days and the year for earlier years", () => {
    const earlier = new Date(2026, 7, 3, 9, 5, 0).getTime() / 1000;
    expect(formatRecordedAt(earlier, now)).toMatch(/Aug/);
    expect(formatRecordedAt(earlier, now)).not.toMatch(/2026/);

    const lastYear = new Date(2025, 11, 24, 9, 5, 0).getTime() / 1000;
    expect(formatRecordedAt(lastYear, now)).toMatch(/2025/);
  });
});

describe("isCurrent", () => {
  const all = [version("newest"), version("older")];

  it("marks the newest state when the note is not being edited", () => {
    expect(isCurrent(all[0], all, false)).toBe(true);
    expect(isCurrent(all[1], all, false)).toBe(false);
  });

  it("marks nothing while there are unsaved edits", () => {
    // The note has moved on from every recorded state, so going back to any of them —
    // including the newest — would replace something.
    expect(isCurrent(all[0], all, true)).toBe(false);
  });

  it("marks nothing when there is no history", () => {
    expect(isCurrent(version("x"), [], false)).toBe(false);
  });
});
