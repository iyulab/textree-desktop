import type { RootId } from "./ipc";
/*
 * Related-notes store — semantic similarity panel for the open note.
 *
 * Calls the AI sidecar's semantic_search with the note's leading text as the query and
 * excludes the current note from results. Degrades silently (empty list) when the sidecar
 * is absent or returns an error. Excluded from vitest (runes module) — the filtering logic
 * is unit-tested via the pure helper (relatedNotes.helpers.ts).
 */

import { semanticSearch } from "./ipc";
import { excludeSelf, type RelatedNote } from "./relatedNotes.helpers";

class RelatedNotesStore {
  items = $state<RelatedNote[]>([]);
  // Generation counter: incremented on each new load() call so that results from
  // a superseded (stale) search are discarded when a newer one wins the race.
  // Also used to invalidate in-flight requests when clear() is called.
  private _gen = 0;

  /**
   * Fetch semantically related notes for the open note.
   *
   * Immediately clears the panel on each call so that stale results from the
   * previously-open note are never shown for the new note — this prevents the
   * related-notes section from briefly displaying another note's relations
   * (which could include the current note, breaking the self-exclusion invariant
   * from the user's perspective even when the filter is logically correct).
   *
   * @param vaultRoot  Absolute vault path (passed to the IPC).
   * @param notePath   Vault-relative POSIX path of the open note (used to exclude self).
   * @param noteBody   Current body text; the first 800 characters serve as the semantic query.
   * @param scopePath  Folder to scope the search, relative to the vault (`""` = all of it), or null
   *                   for the whole vault.
   */
  async load(
    vaultRoot: RootId,
    notePath: string,
    noteBody: string,
    scopePath: string | null,
  ): Promise<void> {
    const gen = ++this._gen; // capture generation for this request
    this.items = []; // clear stale results immediately; panel hides until new results arrive
    try {
      const query = noteBody.slice(0, 800); // lead chunk as the semantic query
      const hits = await semanticSearch(vaultRoot, query, scopePath, 6);
      // Discard stale results: if a newer load() was started while this one was in-flight,
      // its generation will be higher — do not overwrite with our (now-stale) results.
      if (gen !== this._gen) return;
      this.items = excludeSelf(hits, notePath);
    } catch {
      if (gen !== this._gen) return;
      this.items = []; // degrade silently — sidecar absent or unavailable
    }
  }

  /** Drop all results (vault switch / editor close). */
  clear(): void {
    this._gen++; // invalidate any in-flight load()
    this.items = [];
  }
}

export const relatedNotes = new RelatedNotesStore();
