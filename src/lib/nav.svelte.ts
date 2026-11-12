import type { RootId } from "./ipc";
/*
 * Navigation state of one notes folder — favorites, recent, manual order.
 * Favorites/order are kept with the app per folder; recent lives in localStorage (device-bound).
 * One instance per open folder (`folderState.svelte.ts`), bound to it for its whole life.
 *
 * Pure logic (mergeOrder/dedupePushFront/RECENT_MAX) lives in nav.helpers.ts —
 * split into a runes-free module so it can be tested directly under vitest (node environment).
 */

import { writeSidecar } from "./ipc";
import { dedupePushFront, RECENT_MAX, remapOrder, remapPaths } from "./nav.helpers";
import { readSettings } from "./settingsFile";
import { isStringList, isStringListMap } from "./settingsFile.helpers";
import { RetryingWriter } from "./sidecarWriter.helpers";
import { readVaultList, writeVaultList } from "./vaultLocal.helpers";

// Re-export so components can import the order helper alongside from `$lib/nav.svelte`.
export { mergeOrder } from "./nav.helpers";

export const RECENT_KEY = "textree-recent";

const storage = () => (typeof localStorage === "undefined" ? undefined : localStorage);

export class FolderNav {
  favorites = $state<string[]>([]);
  /** Notes opened lately in this vault, on this device. */
  recent = $state<string[]>([]);
  /** parentPath → child path order. */
  order = $state<Record<string, string[]>>({});
  /** Files not to write this time: saved by a newer release, or unreadable and not set aside. */
  private kept = new Set<string>();
  // Favorites and order are separate files: a failed write of one is retried when the other is
  // written, not only when the same one is.
  private writer = new RetryingWriter(
    (rel, body) =>
      this.kept.has(rel) ? Promise.resolve() : writeSidecar(this.root, rel, body),
    // Non-blocking: the in-memory state stays as the person left it.
    (rel, e) => console.warn(`Sidecar write failed (${rel}):`, e),
  );

  /** `path`: where the folder is on disk — this device remembers its recent notes by it. */
  constructor(
    private readonly root: RootId,
    private readonly path: string,
  ) {}

  /**
   * Called when the folder opens — reads favorites/order from the sidecar. `readOnly`: a newer
   * release saved them; shown as defaults, never written over. Returns the names of files that
   * could not be read and were kept aside.
   */
  async load(readOnly = false): Promise<string[]> {
    const root = this.root;
    this.recent = readVaultList(storage(), RECENT_KEY, this.path);
    this.kept = new Set();
    const setAside: string[] = [];
    const favorites = readOnly ? null : await readSettings(root, "favorites.json", isStringList);
    const order = readOnly ? null : await readSettings(root, "order.json", isStringListMap);
    this.favorites = favorites?.value ?? [];
    this.order = order?.value ?? {};
    for (const [rel, read] of [
      ["favorites.json", favorites],
      ["order.json", order],
    ] as const) {
      if (!read || !read.writable) this.kept.add(rel);
      if (read?.setAside) setAside.push(read.setAside);
    }
    return setAside;
  }

  isFavorite(path: string): boolean {
    return this.favorites.includes(path);
  }

  async toggleFavorite(path: string): Promise<void> {
    this.favorites = this.isFavorite(path)
      ? this.favorites.filter((p) => p !== path)
      : [...this.favorites, path];
    await this.writer.save("favorites.json", () => JSON.stringify(this.favorites));
  }

  pushRecent(path: string): void {
    this.recent = dedupePushFront(this.recent, path, RECENT_MAX);
    writeVaultList(storage(), RECENT_KEY, this.path, this.recent);
  }

  async setOrder(parent: string, paths: string[]): Promise<void> {
    this.order = { ...this.order, [parent]: paths };
    await this.writer.save("order.json", () => JSON.stringify(this.order));
  }

  /** A note or folder moved or was renamed: favorites, recent and manual order go with it. */
  async follow(remap: (p: string) => string): Promise<void> {
    const favorites = remapPaths(this.favorites, remap);
    const order = remapOrder(this.order, remap);
    const recent = remapPaths(this.recent, remap);
    const changed = (a: unknown, b: unknown) => JSON.stringify(a) !== JSON.stringify(b);
    if (changed(recent, this.recent)) {
      this.recent = recent;
      writeVaultList(storage(), RECENT_KEY, this.path, recent);
    }
    const writes: Promise<void>[] = [];
    if (changed(favorites, this.favorites)) {
      this.favorites = favorites;
      writes.push(this.writer.save("favorites.json", () => JSON.stringify(this.favorites)));
    }
    if (changed(order, this.order)) {
      this.order = order;
      writes.push(this.writer.save("order.json", () => JSON.stringify(this.order)));
    }
    await Promise.all(writes);
  }
}
