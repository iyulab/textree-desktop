import type { RootId } from "./ipc";
/*
 * Saved-views store — folder → named views, persisted to views.json (kept with the app, per vault).
 *
 * Persistence shape mirrors order.json (nav.svelte.ts): a single fixed sidecar file keyed by folder
 * path. This sidesteps enumeration (one read, filter by folder in memory), makes delete clean (drop
 * from the array — no orphan files), and uses folder paths as JSON keys (not filenames), so paths
 * like `a/b` and `a-b` never collide. New IPC 0 (reuses write_sidecar/read_sidecar).
 *
 * NOTE — deviation from the ratified `views/<slug>.json` per-view-file record: the ratified
 * load-bearing decisions (6-field schema, sidecar file, versioned JSON, file-is-truth, NEW IPC
 * 0, interop trade-off) are all preserved; only the container changes (per-view file → folder-keyed
 * map), because honoring per-view files literally would need a list/delete IPC and break "new IPC 0".
 * The ViewDefinition schema (view.helpers.ts) is untouched and stored as the map's array values.
 *
 * Pure logic (upsertView/removeView) lives in view.helpers.ts (vitest); this runes module is a thin
 * persistence wrapper and is not imported by tests, keeping pure helpers separate from reactive state.
 */

import { writeSidecar } from "./ipc";
import { readSettings } from "./settingsFile";
import { isViewsMap } from "./settingsFile.helpers";
import {
  findForeignViewFolders,
  remapViews,
  removeView,
  upsertView,
  type ViewDefinition,
} from "./view.helpers";

const VIEWS_FILE = "views.json";

class ViewsStore {
  /** folderPath → that folder's saved views. */
  private all = $state<Record<string, ViewDefinition[]>>({});
  /** Current vault root (target of persist IPC). null means not loaded. */
  private root: RootId | null = null;
  /** Not to be written this time: saved by a newer release, or unreadable and not set aside. */
  private kept = false;
  /**
   * Stored folder keys that don't belong to the current vault root (vault moved or opened on
   * another device). Surfaced as a non-destructive notice so the views don't appear to vanish
   * silently. The saved data is untouched — this is detection only.
   */
  foreignFolders = $state<string[]>([]);

  /**
   * Called on vault open/switch — reloads saved views from the sidecar. `readOnly`: a newer release
   * saved them; shown as none, never written over. Returns the name the file was kept under if it
   * could not be read.
   */
  async load(root: RootId, readOnly = false): Promise<string | null> {
    this.root = root;
    const read = readOnly ? null : await readSettings(root, VIEWS_FILE, isViewsMap);
    this.kept = !read || !read.writable;
    this.all = read?.value ?? {};
    this.foreignFolders = findForeignViewFolders(Object.keys(this.all));
    return read?.setAside ?? null;
  }

  /** Saved views for a folder, in saved order (empty if none). */
  forFolder(folder: string): ViewDefinition[] {
    return this.all[folder] ?? [];
  }

  /** Save (insert or replace by name) a view in its folder. The view's `folder` field is the key. */
  async save(view: ViewDefinition): Promise<void> {
    const next = upsertView(this.all[view.folder] ?? [], view);
    this.all = { ...this.all, [view.folder]: next };
    await this.persist();
  }

  /** Remove a named view from a folder. Drops the folder key entirely when it empties. */
  async remove(folder: string, name: string): Promise<void> {
    const next = removeView(this.all[folder] ?? [], name);
    const all = { ...this.all };
    if (next.length) all[folder] = next;
    else delete all[folder];
    this.all = all;
    await this.persist();
  }

  /** A folder moved or was renamed: its views, and those of the folders under it, go with it. */
  async follow(remap: (p: string) => string): Promise<void> {
    const next = remapViews(this.all, remap);
    if (next === this.all) return;
    this.all = next;
    await this.persist();
  }

  private async persist(): Promise<void> {
    if (this.root === null || this.kept) return;
    try {
      await writeSidecar(this.root, VIEWS_FILE, JSON.stringify(this.all));
    } catch (e) {
      // Non-blocking: keep the in-memory state. Retry recovery on the next write.
      console.warn(`Sidecar write failed (${VIEWS_FILE}):`, e);
    }
  }
}

export const views = new ViewsStore();
