/*
 * Backlink index of one notes folder — "what links here" for its notes. One instance per open
 * folder (`folderState.svelte.ts`).
 *
 * The index is built by reading every note's body once per vault load / structure change. Edits to
 * the open note update just that source in-memory (no re-read). Pure construction lives in
 * wikilink.helpers (buildBacklinkIndex); this runes store holds the latest index + the body cache so
 * recomputes after an edit are cheap (parse only, no IPC). Excluded from vitest (runes module) — the
 * index logic is unit-tested via the pure helper.
 */

import { buildBacklinkIndex, type Backlink, type BacklinkIndex } from "./wikilink.helpers";

const EMPTY: BacklinkIndex = { to: () => [] };

export class FolderBacklinks {
  index = $state<BacklinkIndex>(EMPTY);
  /** path → body cache, so an edit recomputes from memory without re-reading the vault. */
  #bodies = new Map<string, string>();
  #resolve: (target: string) => string | undefined = () => undefined;

  /** Replace the whole index from the full note set (path + body) and a resolver over those paths. */
  rebuild(
    notes: { path: string; body: string }[],
    resolve: (target: string) => string | undefined,
  ): void {
    this.#bodies = new Map(notes.map((n) => [n.path, n.body]));
    this.#resolve = resolve;
    this.#recompute();
  }

  /**
   * Update one note's body (e.g. the open note after a save) and recompute from the cache — cheap,
   * no IPC. A note absent from the cache (created since the last full build) is ignored; the next
   * structure rebuild will include it.
   */
  updateSource(path: string, body: string): void {
    if (!this.#bodies.has(path)) return;
    this.#bodies.set(path, body);
    this.#recompute();
  }

  #recompute(): void {
    const notes = [...this.#bodies].map(([path, body]) => ({ path, body }));
    this.index = buildBacklinkIndex(notes, this.#resolve);
  }

  /** Incoming links for a note path (empty when none or no open note). */
  for(path: string | null): Backlink[] {
    return path ? this.index.to(path) : [];
  }
}
