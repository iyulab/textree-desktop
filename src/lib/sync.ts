// Sync Coordinator — coordinates the backend `fs_changed` event with UI state (design §4).
//
// Responsibility: receive external file changes to (1) refresh the tree, and (2) when the open note
// is affected, branch into reload/removal-mark/conflict-banner per the conflict policy (design §4.2).
// The state itself is held by the page; here we only coordinate via injected handlers.

import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { listTree, readNote, type RootId, type TreeNode } from "./ipc";
import { decideChangeAction, decideRescanAction } from "./sync.helpers";
import { samePath } from "./vaultPaths.helpers";

export type FsChangeKind = "created" | "modified" | "removed";

export interface FsChange {
  kind: FsChangeKind;
  path: string;
}

/** A change as the backend says it: the folder it happened in, and where in it. */
interface FsEvent {
  rootId: RootId;
  kind: FsChangeKind;
  rel: string;
}

export interface SyncHandlers {
  /** Current vault root (null if none). */
  root: () => RootId | null;
  /** Body path of the currently open note, relative to the vault (null if none). */
  activePath: () => string | null;
  /** Current text of the open note as the user sees it (live editor doc). */
  activeDoc: () => string;
  /** Whether the editor has unsaved edits. */
  isDirty: () => boolean;
  /** The last content the editor and disk were known to agree on (last loaded or last saved). */
  synced: () => string;
  /** Replace with the refreshed tree. */
  setTree: (tree: TreeNode[]) => void;
  /** Reload a clean note with the disk content (FS is the truth). */
  reloadActive: (diskContent: string) => void;
  /** The open note was removed/moved externally. */
  activeRemoved: () => void;
  /** The open note was read from disk — it exists (again, if it had been reported removed). */
  activePresent: () => void;
  /** External change while dirty — passes the disk content for non-destructive conflict resolution. */
  conflict: (diskContent: string) => void;
}

/** Handle a single fs change (refresh tree + apply to the open note). */
async function handleChange(handlers: SyncHandlers, event: FsEvent): Promise<void> {
  const root = handlers.root();
  // A change in a folder no longer open says nothing about this one, even for a note of the same name.
  if (!root || event.rootId !== root) return;
  const payload: FsChange = { kind: event.kind, path: event.rel };

  // 1) Refresh the tree on any change (reflect create/remove/rename).
  try {
    handlers.setTree(await listTree(root));
  } catch {
    // A tree refresh failure is non-fatal — recovered on the next event.
  }

  // 2) Process the body only if the open note is affected.
  const active = handlers.activePath();
  if (!active || !samePath(payload.path, active)) return;

  if (payload.kind === "removed") {
    handlers.activeRemoved();
    return;
  }

  // created/modified: read the disk content and apply the conflict policy.
  let disk: string;
  try {
    disk = await readNote(root, active);
  } catch {
    handlers.activeRemoved(); // read failure = effectively gone
    return;
  }
  handlers.activePresent();

  const action = decideChangeAction(disk, handlers.activeDoc(), handlers.isDirty(), handlers.synced());
  if (action === "conflict") handlers.conflict(disk); // protect unsaved edits — user chooses
  else if (action === "reload") handlers.reloadActive(disk); // reload silently
}

/**
 * Handle a watcher rescan — the OS dropped events (queue overflow), so per-path state is
 * unknowable. Refresh the tree unconditionally, then reconcile the open note against disk.
 * The reconcile is content-compared (see decideRescanAction): an unaffected note is left
 * completely alone, so a rescan never disturbs typing focus or shows a false banner.
 */
async function handleRescan(handlers: SyncHandlers, rootId: RootId): Promise<void> {
  const root = handlers.root();
  // A rescan of a folder no longer open says nothing about this one.
  if (!root || root !== rootId) return;

  try {
    handlers.setTree(await listTree(root));
  } catch {
    // A tree refresh failure is non-fatal — recovered on the next event.
  }

  const active = handlers.activePath();
  if (!active) return;

  let disk: string;
  try {
    disk = await readNote(root, active);
  } catch {
    handlers.activeRemoved(); // read failure = effectively gone (same policy as handleChange)
    return;
  }
  handlers.activePresent();

  const action = decideRescanAction(disk, handlers.activeDoc(), handlers.isDirty(), handlers.synced());
  if (action === "conflict") handlers.conflict(disk);
  else if (action === "reload") handlers.reloadActive(disk);
}

/**
 * Start the `fs_changed` + `fs_rescan` subscriptions. Detach via the returned unlisten.
 * Serializes handlers into a single promise chain to prevent ordering inversions where, under
 * concurrent execution, stale `listTree`/`readNote` results overwrite the latest results.
 */
export async function startSync(handlers: SyncHandlers): Promise<UnlistenFn> {
  let chain: Promise<void> = Promise.resolve();
  const enqueue = (task: () => Promise<void>) => {
    chain = chain.then(task).catch(() => {});
  };
  const unlistenChanged = await listen<FsEvent>("fs_changed", ({ payload }) => {
    enqueue(() => handleChange(handlers, payload));
  });
  const unlistenRescan = await listen<{ rootId: RootId }>("fs_rescan", ({ payload }) => {
    enqueue(() => handleRescan(handlers, payload.rootId));
  });
  return () => {
    unlistenChanged();
    unlistenRescan();
  };
}
