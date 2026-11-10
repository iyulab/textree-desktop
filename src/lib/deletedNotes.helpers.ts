/**
 * Pure text and ordering for the notes the folder no longer has.
 *
 * One list, two origins: notes that were recorded and later removed, and notes that were only
 * ever kept on the way out. The difference does not matter to someone looking for what they
 * deleted, so it is not made into two screens — but it is not hidden either, because it changes
 * what "restore" will bring back.
 */
import type { DeletedNote, RestoredNote } from "./ipc";
import { SCREEN_LOCALE } from "./screenLocale";

/** Last component of a path, `/`-separated as the backend reports it. */
export function displayName(rel: string): string {
  const cut = rel.lastIndexOf("/");
  const segment = cut >= 0 ? rel.slice(cut + 1) : rel;
  return segment.toLowerCase().endsWith(".md") ? segment.slice(0, -3) : segment;
}

/** The folder a note was in, or null when it sat at the top of the vault. */
export function containingFolder(rel: string): string | null {
  const cut = rel.lastIndexOf("/");
  return cut > 0 ? rel.slice(0, cut) : null;
}

/** What the screen says when nothing has been deleted. */
export function emptyMessage(): string {
  return "Nothing has been deleted.";
}

/**
 * The standing note under the list.
 *
 * Says what the list covers and, just as importantly, what it does not do — there is no way to
 * remove anything from here for good, and letting that be discovered by looking for the button
 * would be worse than saying it.
 */
export function standingNote(): string {
  return "Notes you delete are kept here even if they never had a version. Nothing is removed from here.";
}

/** When a note left the folder, as a person reads it. */
export function formatDeletedAt(seconds: number, now: Date = new Date()): string {
  if (seconds <= 0) return "date unknown";
  const at = new Date(seconds * 1000);
  const sameYear = at.getFullYear() === now.getFullYear();
  const date = at.toLocaleDateString(SCREEN_LOCALE, { month: "short", day: "numeric" });
  return sameYear ? `deleted ${date}` : `deleted ${date}, ${at.getFullYear()}`;
}

/**
 * What to say once a note is back.
 *
 * Two things can surprise the person here and both are said. The name may not be the one they
 * asked for, because something already had it and overwriting is never the answer. And the
 * contents may be the last recorded state rather than the one the note was in when it went,
 * which happens exactly when it was edited without being recorded again.
 */
export function restoredMessage(asked: string, landed: RestoredNote): string {
  const renamed = landed.rel !== asked;
  const where = renamed
    ? `Restored as ${displayName(landed.rel)} — a note called ${displayName(asked)} is already there.`
    : `Restored ${displayName(landed.rel)}.`;
  const which = landed.asDeleted
    ? "It came back as it was when you deleted it."
    : "It came back from its last version.";
  return `${where} ${which}`;
}

/** Newest first, then by name, so the order does not shuffle between visits. */
export function sortDeleted(notes: DeletedNote[]): DeletedNote[] {
  return [...notes].sort((a, b) => b.seconds - a.seconds || a.rel.localeCompare(b.rel));
}
