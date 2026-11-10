// Vault paths — comparison and composition of the paths the backend hands out, relative to the
// vault and `/`-separated (the vault itself is `""`), and where a structure change puts them.
// Pure: nothing here touches the disk.
//
// Names compare the way Windows does: ignoring case.

import type { Remap } from "./noteSave";

const normalize = (p: string) => p.replace(/\/+$/, "").toLowerCase();

/** One key per place: two spellings {@link samePath} equates give the same key. */
export const pathKey = normalize;

/** Is `child` equal to or below `ancestor` — and everything is below the vault itself (`""`). */
export function pathInside(child: string, ancestor: string): boolean {
  const c = normalize(child);
  const a = normalize(ancestor);
  return a === "" || c === a || c.startsWith(a + "/");
}

/** Do `a` and `b` name the same place. */
export function samePath(a: string, b: string): boolean {
  return normalize(a) === normalize(b);
}

/**
 * An absolute path as the vault at `root` names it — `""` for the vault itself — or `null` when it is
 * not inside. For reading what was kept before paths were relative.
 */
export function relativeTo(root: string, abs: string): string | null {
  const n = (p: string) => p.replace(/\\/g, "/").replace(/\/+$/, "");
  const r = n(root);
  const a = n(abs);
  if (r === "") return null;
  if (a.toLowerCase() === r.toLowerCase()) return "";
  if (a.slice(0, r.length).toLowerCase() !== r.toLowerCase() || a[r.length] !== "/") return null;
  return a.slice(r.length + 1);
}

/** The folder a path is in: `""` for one at the top of the vault. */
export function parentDir(p: string): string {
  const i = p.lastIndexOf("/");
  return i >= 0 ? p.slice(0, i) : "";
}

/** Last component of a path (file or folder name). */
export function baseName(p: string): string {
  return p.slice(p.lastIndexOf("/") + 1);
}

/** `child` inside the folder `dir` (`""` = the vault itself). */
export function joinPath(dir: string, child: string): string {
  return dir === "" ? child : `${dir}/${child}`;
}

/** A note's name: its file name without `.md`. */
export function noteStem(p: string): string {
  return baseName(p).replace(/\.md$/i, "");
}

/** The note a folder holds as its own body: `<dir>/<dir name>.md`. */
export function folderBody(dir: string): string {
  return joinPath(dir, `${baseName(dir)}.md`);
}

/** `from` (and everything under it) now lives at `to`. */
export function remapMoved(from: string, to: string): Remap {
  return (p) => (pathInside(p, from) ? to + p.slice(from.length) : p);
}

/**
 * `target` was renamed to `renamed`. A folder carries its own body note along under the new name,
 * so `<old>/<old>.md` becomes `<new>/<new>.md`; everything else under it keeps its name.
 */
export function remapRenamed(target: string, renamed: string, isFolder: boolean): Remap {
  const moved = remapMoved(target, renamed);
  if (!isFolder) return moved;
  const oldBody = folderBody(target);
  const newBody = folderBody(renamed);
  return (p) => (samePath(p, oldBody) ? newBody : moved(p));
}

/** The note `leaf` became a folder `dir`, and is now that folder's body note. */
export function remapPromoted(leaf: string, dir: string): Remap {
  const body = folderBody(dir);
  return (p) => (samePath(p, leaf) ? body : p);
}

/**
 * `src` was moved into `leaf`, which became a folder to hold it; `moved` is where `src` landed.
 * The leaf is now that folder's body note.
 */
export function remapAdopted(src: string, leaf: string, moved: string): Remap {
  const promoted = remapPromoted(leaf, parentDir(moved));
  const carried = remapMoved(src, moved);
  return (p) => (samePath(p, leaf) ? promoted(p) : carried(p));
}
