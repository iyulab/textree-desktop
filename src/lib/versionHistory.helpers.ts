/**
 * Pure text and ordering for the list of a note's recorded states.
 *
 * The list is finite by design. Reading a note's history costs more the longer the folder has
 * been in use, and a list nobody scrolls to the end of is the cheapest place to stop paying for
 * that — so a page is asked for, and the fact that there is more is said rather than hidden.
 */
import type { NoteVersion } from "./ipc";
import { SCREEN_LOCALE } from "./screenLocale";

/** How many states are shown before the list says there are older ones. */
export const PAGE = 25;

/** The states to show, and whether the list stops short of the beginning. */
export function page(
  versions: NoteVersion[],
  shown: number = PAGE,
): { visible: NoteVersion[]; older: number } {
  const visible = versions.slice(0, shown);
  return { visible, older: Math.max(0, versions.length - visible.length) };
}

/** What the list says when there is nothing in it yet. */
export function emptyMessage(): string {
  return "No versions yet. Adding one keeps this note's current state so you can come back to it.";
}

/** What the footer says about states beyond the page, or nothing when there are none. */
export function olderMessage(older: number): string | null {
  if (older <= 0) return null;
  return older === 1 ? "1 older version" : `${older} older versions`;
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
