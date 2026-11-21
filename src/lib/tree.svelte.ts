/*
 * Tree UI state.
 *
 * `FolderFolds` — which folders of one notes folder are collapsed. A presentation-layer preference,
 * so it persists in localStorage, one list per notes folder (paths are relative to it). One instance
 * per open folder (`folderState.svelte.ts`).
 * `tree` — keyboard focus and the "cut" node: one per sidebar, session state, not persisted.
 */

import type { PickedNode } from "./treeDrag.helpers";
import { readVaultList, writeVaultList } from "./vaultLocal.helpers";

export const COLLAPSE_KEY = "textree-tree-collapsed";

const storage = () => (typeof localStorage === "undefined" ? undefined : localStorage);

export class FolderFolds {
  collapsed = $state<Set<string>>(new Set());

  /** `path`: where the folder is on disk — this device remembers its collapsed folders by it. */
  constructor(private readonly path: string) {
    this.collapsed = new Set(readVaultList(storage(), COLLAPSE_KEY, path));
  }

  isCollapsed(path: string): boolean {
    return this.collapsed.has(path);
  }

  private persist(): void {
    writeVaultList(storage(), COLLAPSE_KEY, this.path, [...this.collapsed]);
  }

  /** A folder moved or was renamed: it stays collapsed, and so do the folders under it. */
  follow(remap: (p: string) => string): void {
    const next = new Set([...this.collapsed].map(remap));
    if (next.size === this.collapsed.size && [...next].every((p) => this.collapsed.has(p))) return;
    this.collapsed = next;
    this.persist();
  }

  collapse(path: string): void {
    if (this.collapsed.has(path)) return;
    const next = new Set(this.collapsed);
    next.add(path);
    this.collapsed = next;
    this.persist();
  }

  expand(path: string): void {
    if (!this.collapsed.has(path)) return;
    const next = new Set(this.collapsed);
    next.delete(path);
    this.collapsed = next;
    this.persist();
  }

  toggle(path: string): void {
    if (this.collapsed.has(path)) this.expand(path);
    else this.collapse(path);
  }
}

class TreeFocus {
  /** The node under keyboard focus, with its notes folder (roving tabindex basis). null = unset (each
   *  folder's first item takes Tab). */
  focused = $state<PickedNode | null>(null);
  /** The node "cut" for keyboard move (Ctrl+X), with its notes folder. Moved/adopted onto the paste (Ctrl+V) target. */
  cut = $state<PickedNode | null>(null);

  /** A notes folder opened in place of another: what was focused or cut belonged to it. */
  reset(): void {
    this.focused = null;
    this.cut = null;
  }

  setFocused(picked: PickedNode | null): void {
    this.focused = picked;
  }

  setCut(picked: PickedNode | null): void {
    this.cut = picked;
  }
}

export const tree = new TreeFocus();
