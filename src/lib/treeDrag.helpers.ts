/*
 * A note or folder picked up in the sidebar — by dragging it or cutting it (Ctrl+X) — named with the
 * notes folder it is in, so it is never put down in another notes folder as if it were there.
 * Pure (vitest).
 */

/** What is picked up: the notes folder's id and the path inside it. */
export interface PickedNode {
  root: string;
  path: string;
}

/** The drag data for a node of the notes folder `root`. */
export function encodePicked(picked: PickedNode): string {
  return JSON.stringify({ root: picked.root, path: picked.path });
}

/** The node a drag carries, or null when it carries something else (a file from the system). */
export function decodePicked(data: string | undefined | null): PickedNode | null {
  if (!data) return null;
  try {
    const value: unknown = JSON.parse(data);
    if (
      typeof value === "object" &&
      value !== null &&
      typeof (value as PickedNode).root === "string" &&
      typeof (value as PickedNode).path === "string"
    ) {
      return { root: (value as PickedNode).root, path: (value as PickedNode).path };
    }
  } catch {
    // not ours
  }
  return null;
}

/** Whether `a` and `b` are the same node of the same notes folder. */
export function samePicked(a: PickedNode | null, b: PickedNode | null): boolean {
  return a !== null && b !== null && a.root === b.root && a.path === b.path;
}

/** What the sidebar says when a node is put down in another notes folder. */
export const NOT_ACROSS_FOLDERS =
  "Notes can't be moved to another notes folder. Links stay inside one notes folder.";
