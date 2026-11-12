/*
 * What the app keeps for each open notes folder: its tree, favorites, recent and order, saved views,
 * publish choices, the backlink index and collapsed folders. Each folder's state is bound to that folder for
 * its whole life — it reads and writes only there — so nothing of one folder can land in another.
 */

import { FolderBacklinks } from "./backlinkStore.svelte";
import type { RootId, TreeNode } from "./ipc";
import { FolderNav } from "./nav.svelte";
import { FolderPublishScope } from "./publishScope.svelte";
import { RootStates } from "./rootStates";
import { FolderFolds } from "./tree.svelte";
import { FolderViews } from "./views.svelte";

export class FolderState {
  readonly nav: FolderNav;
  readonly views: FolderViews;
  readonly publish: FolderPublishScope;
  readonly backlinks = new FolderBacklinks();
  readonly folds: FolderFolds;
  /** The folder's notes and folders as last read. */
  tree = $state<TreeNode[]>([]);

  /** `path`: where the folder is on disk — what this device remembers of it is kept by it. */
  constructor(
    readonly root: RootId,
    readonly path: string,
  ) {
    this.nav = new FolderNav(root, path);
    this.views = new FolderViews(root);
    this.publish = new FolderPublishScope(root);
    this.folds = new FolderFolds(path);
  }
}

/** The open notes folders' states. */
export const folders = new RootStates<RootId, FolderState>();
