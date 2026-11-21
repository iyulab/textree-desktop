/*
 * The sidebar's notes folders — pure rules (vitest). The runes state that holds them lives in
 * `+page.svelte` until the sidebar shows several.
 */

import type { RootId, WorkspaceRoot } from "./ipc";

/** A folder's own name: the last piece of its path. */
export function folderName(path: string): string {
  return path.replace(/[/\\]+$/, "").split(/[/\\]/).pop() || path;
}

/** What the sidebar calls a notes folder: the name the person gave it, else the folder's own. */
export function sidebarName(root: Pick<WorkspaceRoot, "path" | "label">): string {
  const label = root.label?.trim();
  return label ? label : folderName(root.path);
}

/**
 * The sidebar's folders after moving `id` by `delta` places (−1 up, +1 down), as the ids the
 * order is kept by. Unchanged (the same array) at either end or for an id not listed.
 */
export function movedRoot(ids: readonly RootId[], id: RootId, delta: number): readonly RootId[] {
  const from = ids.indexOf(id);
  const to = from + delta;
  if (from === -1 || to < 0 || to >= ids.length || delta === 0) return ids;
  const next = [...ids];
  next.splice(from, 1);
  next.splice(to, 0, id);
  return next;
}

/** What showing a folder in the system's file manager is called where the app runs. */
export function fileManagerAction(platform: string): string {
  return /mac/i.test(platform) ? "Show in Finder" : "Show in File Explorer";
}

/**
 * Whether adding a folder to the sidebar was refused because it is inside a folder the sidebar
 * shows, or holds one (the backend's wording, which the person reads as it is).
 */
export function isNestingRefusal(message: string): boolean {
  return (
    message.startsWith("This folder is already in the sidebar inside ") ||
    message.endsWith(" is inside this folder.")
  );
}

/** What opening or locating a notes folder says when there is no folder at its path (the backend's
 *  `NOT_FOUND`, read as it is). */
export const FOLDER_NOT_FOUND = "Notes folder not found";

/** Whether opening a folder failed because there is no folder at its path — it was moved, renamed
 *  or deleted, or its drive is not connected — rather than for another reason. */
export function isFolderNotFound(error: unknown): boolean {
  return String(error).includes(FOLDER_NOT_FOUND);
}

/**
 * `items` in the order of the device's list `order` (ids), those it does not name after, as they
 * came. The sidebar's open and not-found folders are shown together in this order.
 */
export function inListOrder<T>(items: readonly T[], idOf: (item: T) => string, order: readonly string[]): T[] {
  const rank = (item: T) => {
    const i = order.indexOf(idOf(item));
    return i === -1 ? Number.MAX_SAFE_INTEGER : i;
  };
  return [...items].sort((x, y) => rank(x) - rank(y));
}
