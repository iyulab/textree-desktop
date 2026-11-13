/**
 * Pure text and ordering for the list of a note's recorded states.
 *
 * The list is read a page at a time. Reading a note's history costs more the further back it
 * goes, and a list nobody scrolls to the end of is the cheapest place to stop paying for that —
 * so the newest page is asked for, and older ones only when the person asks.
 */
import type { NoteVersion } from "./ipc";
import { SCREEN_LOCALE } from "./screenLocale";

/** How many states are asked for at a time. */
export const PAGE = 25;

/** What the list says when there is nothing in it yet. */
export function emptyMessage(): string {
  return "No versions yet. Adding one keeps this note's current state so you can come back to it.";
}

/**
 * When a state was recorded, as a person reads it.
 *
 * Same-day states show only the time, since the date is the one they are already looking at.
 */
export function formatRecordedAt(seconds: number, now: Date = new Date()): string {
  const at = new Date(seconds * 1000);
  const sameDay =
    at.getFullYear() === now.getFullYear() &&
    at.getMonth() === now.getMonth() &&
    at.getDate() === now.getDate();
  const time = at.toLocaleTimeString(SCREEN_LOCALE, { hour: "numeric", minute: "2-digit" });
  if (sameDay) return time;
  const date = at.toLocaleDateString(SCREEN_LOCALE, { month: "short", day: "numeric" });
  const year = at.getFullYear() === now.getFullYear() ? "" : `, ${at.getFullYear()}`;
  return `${date}${year}, ${time}`;
}

/**
 * Whether what is on disk is the newest recorded state.
 *
 * Going back to a state the note is already in writes nothing anyone wanted and keeps a copy of
 * what it replaced, so the option is not offered.
 */
export function isCurrent(version: NoteVersion, versions: NoteVersion[], dirty: boolean): boolean {
  return !dirty && versions.length > 0 && versions[0].id === version.id;
}
