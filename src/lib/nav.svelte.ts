import type { RootId } from "./ipc";
/*
 * Navigation store — favorites, recent, manual order.
 * Favorites/order are kept with the app per vault; recent lives in localStorage (device-bound).
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

const RECENT_KEY = "textree-recent";

const storage = () => (typeof localStorage === "undefined" ? undefined : localStorage);

class NavStore {
  favorites = $state<string[]>([]);
  /** Notes opened lately in this vault, on this device. */
  recent = $state<string[]>([]);
  /** parentPath → child path order. */
  order = $state<Record<string, string[]>>({});
  /** Current vault (target of persist IPC). null means not loaded. */
  private root: RootId | null = null;
  /** Where that vault is on disk: this device remembers its recent notes by it. */
  private path: string | null = null;
  /** Files not to write this time: saved by a newer release, or unreadable and not set aside. */
  private kept = new Set<string>();
  // Favorites and order are separate files: a failed write of one is retried when the other is
  // written, not only when the same one is.
  private writer = new RetryingWriter(
    (rel, body) =>
      this.root === null || this.kept.has(rel) ? Promise.resolve() : writeSidecar(this.root, rel, body),
    // Non-blocking: the in-memory state stays as the person left it.
    (rel, e) => console.warn(`Sidecar write failed (${rel}):`, e),
  );

  /**
   * Called on vault open/switch — reloads favorites/order from the sidecar. `readOnly`: a newer
   * release saved them; shown as defaults, never written over. Returns the names of files that
   * could not be read and were kept aside.
   */
  async load(root: RootId, path: string, readOnly = false): Promise<string[]> {
    this.writer.reset(); // owed writes belong to the folder being left
    this.root = root;
    this.path = path;
    this.recent = readVaultList(storage(), RECENT_KEY, path);
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
    if (this.path !== null) writeVaultList(storage(), RECENT_KEY, this.path, this.recent);
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
      if (this.path !== null) writeVaultList(storage(), RECENT_KEY, this.path, recent);
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

export const nav = new NavStore();
