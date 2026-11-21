// Note Save — the open note's autosave and what stands between the editor and leaving it.
//
// Responsibility: hold the edit not yet on disk, write it one save at a time (each only replacing
// the note while it still holds what the edit started from), and say whether the note can be left.
// It knows nothing of the DOM: the page draws banners and moves focus; here we only decide.
//
// The observable part (`NoteSaveState`) is handed in by the page as reactive state, so the banners
// follow it without a mirror; tests hand in a plain object. Everything else is private bookkeeping.

import { friendlyError, type FriendlyError } from "./friendlyError.helpers";
import type { WriteOutcome } from "./ipc";
import { noteStem, pathInside, pathKey, samePath } from "./vaultPaths.helpers";

/** An edit not yet on disk. `base` is what the note held when the edit started from it. */
export interface PendingEdit {
  path: string;
  text: string;
  base: string;
}

/** What the page shows about the open note's saving. */
export interface NoteSaveState {
  /** The open note has edits that are not on disk yet. Follows the pending edit — never set apart. */
  dirty: boolean;
  /** The last failure to report — a save, but also anything else the page reports through it. */
  saveError: FriendlyError | null;
  /** The last save of the open note's edits failed and they are still unsaved. */
  saveFailure: FriendlyError | null;
  /** The open note was moved or deleted outside the app. */
  removed: boolean;
  /** The copy on disk, while the open note's edits conflict with a change made elsewhere. */
  conflictDisk: string | null;
  /**
   * A save of the open note has been running long enough that the folder is taken as not answering
   * (a stalled sync or network drive). The save is still on its way — a write handed to the disk
   * cannot be called back — so nothing is dropped; the person is told why it has not landed.
   */
  stalled: boolean;
}

export function initialNoteSaveState(): NoteSaveState {
  return {
    dirty: false,
    saveError: null,
    saveFailure: null,
    removed: false,
    conflictDisk: null,
    stalled: false,
  };
}

/**
 * What saving before leaving came to. `asking`: a question about the unsaved edits is still open —
 * which copy wins, or whether to put back a note deleted elsewhere. `failed`: the edits could not be
 * saved. Neither may be left: leaving would drop the edits. `busy`: edits are still arriving —
 * opening another note may go ahead (they are written to this one), but moving, renaming or
 * deleting this note must not, since a write landing afterwards would bring the old path back.
 */
export type LeaveOutcome = "saved" | "asking" | "failed" | "busy";

/** Where a structure change put a path; a path it did not move comes back unchanged. */
export type Remap = (path: string) => string;

/** Which banner a refusal to leave points at. */
export type LeaveQuestion = "conflict" | "removed" | "failure";

export interface NoteSaveDeps {
  root: () => string | null;
  /** Body path of the open note (null if none). */
  activePath: () => string | null;
  /** Replace a note's content only while it still holds `expected`. */
  write: (root: string, path: string, text: string, expected: string) => Promise<WriteOutcome>;
  /** A save landed in `root` — the folder the edit was typed in, which may no longer be open. */
  saved?: (root: string, path: string, text: string) => void;
  /** An edit was made (anything said about the note as it was no longer holds). */
  edited?: () => void;
  /** The open note was found gone. */
  removed?: () => void;
  /** A structure change moved paths (see `move`); the open note may be among them. */
  moved?: (remap: Remap) => void;
  /**
   * Keep `text` as a new note beside `path` without overwriting anything, and say where it went.
   * Used for edits to a note already left that can no longer be written to it.
   */
  keepCopy?: (root: string, path: string, text: string) => Promise<string>;
  /**
   * Keep an edit to a note already left, not written yet, somewhere that outlasts the app — so
   * closing before its folder takes it does not lose it. Returns an id to let go of it by.
   */
  keepStranded?: (root: string, edit: PendingEdit) => Promise<string>;
  /** A kept edit landed (or was kept as a copy): let go of it. */
  forgetStranded?: (root: string, id: string) => Promise<void>;
  /** Kept edits landed on the note that is open again (see `#stranded`); show `text`, now on disk. */
  landedOnOpen?: (text: string) => void;
  /** Leaving was refused; the named banner should be brought to the person's attention. */
  ask?: (question: LeaveQuestion) => void;
  debounceMs?: number;
  /** How long closing waits for a save before treating the folder as not answering. Default 5 s. */
  closeWaitMs?: number;
  /** How long a save of the open note runs before it is said to be stalled. Default 10 s. */
  stallMs?: number;
}

const DEFAULT_DEBOUNCE_MS = 500;
/** Extra saves before leaving for edits typed while the last one ran. Bounded, so someone who never
 *  stops typing is not held. */
const MAX_TRAILING_SAVES = 5;
/** A save still running after this long when the window closes is taken as a folder that is not
 *  answering (a stalled sync or network drive). The wait is bounded so it cannot hold the app open. */
const DEFAULT_CLOSE_WAIT_MS = 5000;
/** A save still running after this long is said to be stalled. Long enough that a slow disk or a
 *  large note does not trip it; short enough that someone waiting on it learns why. */
const DEFAULT_STALL_MS = 10_000;

/** An edit to a note already left that could not be written yet. */
interface Stranded {
  root: string;
  edit: PendingEdit;
  /** Where it is kept outside the app, once it is (see `keepStranded`). */
  id: string | null;
  /** Landed or let go of while it was still being kept: forget it as soon as it is. */
  gone: boolean;
  keeping: Promise<void> | null;
}

export class NoteSave {
  #synced = "";

  #pending: PendingEdit | null = null;
  // Edits to a note already left whose write failed. Nothing on screen holds them any more, so they
  // are kept here — with the vault they belong to — and tried after every save until they land.
  // Oldest first, and never merged: two kept edits to one note are two things the person typed.
  #stranded: Stranded[] = [];
  #warnedOnClose = false;
  #warnedStalledOnClose = false;
  // Saves to one note run one at a time, in order: two in flight would each be based on the same
  // state, and the second would mistake the first for a change made elsewhere. Saves to different
  // notes do not wait for each other — a folder that stops answering for one note holds up only it.
  // Keyed by vault and path: a note of the same name in another vault is another note.
  #chains = new Map<string, { root: string | null; path: string; tail: Promise<void> }>();
  // Structure changes under way, the vault each runs in and the paths it moves. A save to a path
  // being moved waits for the move and then goes where the note went; saves elsewhere carry on.
  #moves: { root: string | null; from: string[] | null; done: Promise<void> }[] = [];
  // Edits to a note already left, waiting their turn to be written. Moves carry them along.
  #queued = new Set<PendingEdit>();
  // The vault each edit was typed in. Another vault can open before its turn comes, and then the
  // vault open at the time it is written is not the one it belongs to — and a note there may well
  // have the same name.
  #typedIn = new WeakMap<PendingEdit, string>();
  // Edits to a note already left, as opposed to the open note's own.
  #left = new WeakSet<PendingEdit>();
  // What an edit was typed on top of: the edit to the same note before it, not on disk yet when this
  // one began, so both start from the same base. Once that one lands, this one starts from it —
  // otherwise it would take its own earlier edit for a change made elsewhere.
  #prev = new WeakMap<PendingEdit, PendingEdit>();
  // Edits on disk. An edit whose later edit landed first is done too: the later one holds it.
  #landed = new WeakSet<PendingEdit>();
  #done = new WeakSet<PendingEdit>();
  #running = new Map<PendingEdit, Promise<void>>();
  // Saves running longer than the stall threshold, and who is waiting to hear of one.
  #slow = new Set<PendingEdit>();
  #onStall: (() => void)[] = [];
  #timer: ReturnType<typeof setTimeout> | null = null;
  readonly #debounceMs: number;
  readonly #closeWaitMs: number;
  readonly #stallMs: number;

  constructor(
    readonly state: NoteSaveState,
    private readonly deps: NoteSaveDeps,
  ) {
    this.#debounceMs = deps.debounceMs ?? DEFAULT_DEBOUNCE_MS;
    this.#closeWaitMs = deps.closeWaitMs ?? DEFAULT_CLOSE_WAIT_MS;
    this.#stallMs = deps.stallMs ?? DEFAULT_STALL_MS;
  }

  /**
   * The last text the editor and disk were known to agree on. Set by every load and moved forward by
   * every successful save of the open note; the external-change reconciliation compares disk against
   * it to tell an edit made elsewhere from the person's own unsaved typing.
   */
  get synced(): string {
    return this.#synced;
  }

  /** The edit not yet on disk, if any. */
  get pending(): Readonly<PendingEdit> | null {
    return this.#pending;
  }

  /** The editor changed `path` to `text`. Saved after a pause in typing. */
  schedule(path: string, text: string): void {
    if (this.#pending && !(this.#here(this.#pending) && samePath(this.#pending.path, path))) {
      // Edits to the note being left, typed while the next one was loading (the editor stays on the
      // old note until then). They are written, not replaced by the first edit to the new one.
      this.#writeLeft(this.#pending);
      this.#pending = null;
    }
    const prev = this.#pending;
    const edit = { path, text, base: prev ? prev.base : this.#synced };
    const root = this.deps.root();
    if (root) this.#typedIn.set(edit, root);
    if (prev) this.#prev.set(edit, prev);
    this.#pending = edit;
    this.#syncDirty();
    this.deps.edited?.();
    this.#cancelTimer();
    this.#timer = setTimeout(() => {
      this.#timer = null;
      // While the note is in conflict with a change on disk, the person has been asked which copy
      // wins. Saving on a timer would answer for them — and overwrite the copy on disk. The edit
      // stays pending until they choose (keepMine saves it; takeDisk drops it).
      if (this.state.conflictDisk !== null) return;
      void this.flush();
    }, this.#debounceMs);
  }

  /**
   * Write the pending edit now. Never throws: a failure is reported through the state and the edit
   * stays pending, so the next flush retries. While the open note waits on an answer (a conflict, or
   * whether to put it back) it writes nothing — saving would answer for the person.
   */
  flush(): Promise<void> {
    const { own, kept } = this.#flushParts();
    return Promise.all([own, kept]).then(() => {});
  }

  /**
   * The open note's own edits, and the kept edits for notes already left, each in its own note's
   * turn — a kept edit that never lands does not hold up the open note.
   */
  #flushParts(): { own: Promise<void>; kept: Promise<void> } {
    this.#cancelTimer();
    const own = this.#pending
      ? this.#onPath(this.#rootOf(this.#pending), this.#pending.path, async () => {
          // Read when its turn comes: a move meanwhile may have carried the edit elsewhere.
          if (this.#pending) await this.#write(this.#pending);
        })
      : Promise.resolve();
    const kept = Promise.all(
      [...this.#stranded].map(({ root, edit }) => this.#onPath(root, edit.path, () => this.#write(edit, root))),
    ).then(() => {});
    return { own, kept };
  }

  /** Waits for the open note's own save — or for it to be found stalled, whichever comes first. */
  async #flushOwn(): Promise<"done" | "stalled"> {
    const { own } = this.#flushParts();
    if (this.state.stalled) return "stalled";
    let stop = () => {};
    const stalled = new Promise<"stalled">((resolve) => {
      const hear = () => resolve("stalled");
      this.#onStall.push(hear);
      stop = () => (this.#onStall = this.#onStall.filter((f) => f !== hear));
    });
    try {
      return await Promise.race([own.then(() => "done" as const), stalled]);
    } finally {
      stop();
    }
  }

  /** Saves still on their way to disk, for any note. */
  get writing(): number {
    return new Set([...this.#running.keys(), ...this.#queued]).size;
  }

  /** Edits to notes already left that are not on disk yet (see `#stranded`). */
  get stranded(): number {
    return this.#stranded.length;
  }

  #strand(root: string, edit: PendingEdit): void {
    const entry: Stranded = { root, edit, id: null, gone: false, keeping: null };
    this.#typedIn.set(edit, root);
    this.#stranded.push(entry);
    this.#warnedOnClose = false; // new edits waiting: the next close warns about them again
    this.#keepOutlasting(entry);
  }

  /** Writes `entry` where it outlasts the app (see `keepStranded`). */
  #keepOutlasting(entry: Stranded): Promise<void> {
    const keep = this.deps.keepStranded;
    if (!keep) return Promise.resolve();
    entry.keeping = keep(entry.root, { ...entry.edit })
      .then((id) => {
        if (entry.gone) void this.#forget(entry.root, id);
        else entry.id = id;
      })
      .catch((e) => console.warn("Could not keep edits outside the app:", e))
      .finally(() => (entry.keeping = null));
    return entry.keeping;
  }

  #forget(root: string, id: string): Promise<void> {
    return (this.deps.forgetStranded?.(root, id) ?? Promise.resolve()).catch((e) =>
      console.warn("Could not let go of kept edits:", e),
    );
  }

  #unstrand(edit: PendingEdit): void {
    for (const entry of this.#stranded.filter((k) => k.edit === edit)) {
      entry.gone = true;
      if (entry.id !== null) void this.#forget(entry.root, entry.id);
    }
    this.#stranded = this.#stranded.filter((k) => k.edit !== edit);
  }

  /**
   * Edits kept from an earlier run for `root`'s notes, not written then. They join the ones kept in
   * this run and are tried with the next save.
   */
  adopt(root: string, kept: { id: string; path: string; text: string; base: string }[]): void {
    for (const { id, path, text, base } of kept) {
      if (this.#stranded.some((k) => k.id === id)) continue;
      const edit = { path, text, base };
      this.#typedIn.set(edit, root);
      this.#stranded.push({ root, edit, id, gone: false, keeping: null });
    }
  }

  /** Save before leaving the open note — switching, changing vault, restructuring, or closing. */
  async beforeLeaving(): Promise<LeaveOutcome> {
    // Only the open note's own save is waited for: kept edits for notes left earlier are tried too,
    // but are not this note's to wait on. A save found stalled is not waited out — see `busy`.
    if ((await this.#flushOwn()) === "stalled") return "busy";
    // Edits typed while that save ran are pending again without anything having gone wrong.
    const s = this.state;
    for (let i = 0; i < MAX_TRAILING_SAVES && this.#pending && !s.saveFailure && s.conflictDisk === null && !s.removed; i++) {
      if ((await this.#flushOwn()) === "stalled") return "busy";
    }
    if (!this.#pending) return "saved";
    if (s.conflictDisk !== null) {
      this.deps.ask?.("conflict");
      return "asking";
    }
    if (s.removed) {
      this.deps.ask?.("removed");
      return "asking";
    }
    // The save failed: the edits exist only in the editor. The banner offers another try or letting
    // them go.
    if (s.saveFailure) {
      this.deps.ask?.("failure");
      return "failed";
    }
    // Nothing went wrong — the typing simply has not stopped, or the folder is slow to take it. What
    // is still pending is saved to this note like any other edit, even after another note opens (see
    // schedule); if the folder never takes it, it is kept like any edit to a note already left.
    return "busy";
  }

  /**
   * The app is about to close. `stay`: something is unsaved and the person has been shown why.
   * Edits kept for a note already left live only in memory, so the first close that finds them
   * still unwritten stays open and warns; the next one closes without them — a disk that never
   * takes them must not keep the app from closing.
   *
   * The same holds for a save that never comes back at all: a write cannot be taken back once it
   * is handed to the disk, so the close does not cancel it — it stops waiting. The first close that
   * finds a save still running after the wait stays open and says so; the next one closes. Nothing
   * is written after it, so a write that lands late cannot overwrite anything newer.
   */
  async beforeClosing(): Promise<"close" | "stay"> {
    if (!this.#pending && this.#stranded.length === 0 && this.writing === 0) return "close";
    let timer: ReturnType<typeof setTimeout> | undefined;
    const stalled = new Promise<"stalled">((done) => {
      timer = setTimeout(() => done("stalled"), this.#closeWaitMs);
    });
    // Every save still on its way counts — edits to a note already left too, not only the open one.
    const everything = (async () => {
      const left = await this.beforeLeaving();
      await Promise.all([...this.#running.values()].map((r) => r.catch(() => {})));
      return left === "busy" && this.state.stalled ? ("stalled" as const) : left;
    })();
    const left = await Promise.race([everything, stalled]);
    clearTimeout(timer);
    if (left === "stalled") {
      // Keep what is still on its way where it outlasts the app, and nothing is lost by closing: it
      // is tried again when the folder next opens (and found already there if it landed meanwhile).
      if (await this.#keepEverythingUnwritten()) return "close";
      if (this.#warnedStalledOnClose) return "close";
      this.#warnedStalledOnClose = true;
      this.state.saveError = friendlyError(
        "Some edits are still being written — the folder isn't answering. Close again to quit without waiting for them.",
      );
      return "stay";
    }
    this.#warnedStalledOnClose = false;
    if (left !== "saved") return "stay";
    await Promise.all(this.#stranded.map((k) => k.keeping));
    if (this.#stranded.some((k) => k.id === null) && !this.#warnedOnClose) {
      this.#warnedOnClose = true;
      this.state.saveError = friendlyError(
        "Some edits to a note you left are still not saved. Close again to quit without them.",
      );
      return "stay";
    }
    return "close";
  }

  /**
   * A note holding `text` on disk was opened (or none, with ""). An edit still pending for the one
   * left is written to it — to its own path — as an edit to a note left.
   */
  opened(text: string): void {
    // An edit still pending for a note that is no longer open is an edit to a note left, whether or
    // not anything is typed into the next one: it goes on its way now. Kept as the open note's, it
    // would be what leaving the next note waits for — and a folder that stopped answering would hold
    // every note opened after it.
    if (this.#pending && !this.#isOpen(this.#pending)) {
      this.#writeLeft(this.#pending);
      this.#pending = null;
    }
    this.#synced = text;
    this.state.removed = false;
    this.state.conflictDisk = null;
    this.#syncDirty(); // an edit still pending for this very note keeps it unsaved
  }

  /** The editor was closed and its unsaved edits with it. */
  closed(): void {
    this.#cancelTimer();
    this.#pending = null;
    this.#synced = "";
    this.#syncDirty();
    // The edit context is gone, so the errors about it are too — except the warning about edits kept
    // for a note left earlier, which are still waiting to land.
    if (this.#stranded.length === 0) this.state.saveError = null;
    this.state.saveFailure = null;
    this.state.removed = false;
    this.state.conflictDisk = null;
  }

  /** The editor was reloaded with `text` from disk; anything unsaved is gone. */
  reloaded(text: string): void {
    this.#synced = text;
    this.#pending = null;
    this.#syncDirty();
    if (this.#stranded.length === 0) this.state.saveError = null;
    this.state.saveFailure = null;
    this.state.removed = false; // back to normal if it was re-created after being deleted
  }

  /** The open note was moved or deleted outside the app. */
  markRemoved(): void {
    this.state.removed = true;
    this.state.saveFailure = null; // the note being gone is the question now, not the failed save
    this.state.conflictDisk = null; // deletion takes priority over a conflict
    // Unsaved edits stay in the editor. The file is gone, but what they typed since the last save
    // exists nowhere else — they decide whether to put the note back with it or let it go.
    this.#syncDirty();
    this.deps.removed?.();
  }

  /** The open note is on disk again (put back elsewhere, or by a sync tool). */
  markPresent(): boolean {
    if (!this.state.removed) return false;
    this.state.removed = false;
    if (this.#pending) void this.flush(); // saves resume against what the note last saved held
    return true;
  }

  /** The open note's edits conflict with `disk`, a change made elsewhere. */
  markConflict(disk: string): void {
    this.state.removed = false;
    this.state.conflictDisk = disk;
  }

  /** Conflict: keep my edits — they replace the copy the banner showed (and only that copy). */
  keepMine(): void {
    if (this.state.conflictDisk !== null && this.#pending) this.#rebase(this.#pending, this.state.conflictDisk);
    this.state.conflictDisk = null;
    void this.flush();
  }

  /** Conflict: take the copy on disk. Returns it for the editor to reload with, if there was one. */
  takeDisk(): string | null {
    this.#cancelTimer();
    const disk = this.state.conflictDisk;
    this.state.conflictDisk = null;
    return disk;
  }

  /** Save failure: let the unsaved edits go. Returns what the note last saved, to reload with. */
  discardUnsaved(): string {
    this.#cancelTimer();
    return this.#synced;
  }

  /** Removed note: let the unsaved edits go. */
  discardRemoved(): void {
    this.#cancelTimer();
    this.#pending = null;
    this.#syncDirty();
  }

  /**
   * The removed note was put back holding `text`, at `movedTo` when it could not return to its own
   * path. Edits typed while that ran stay pending, now on top of what was put back.
   */
  putBack(text: string, movedTo: string | null): void {
    this.#synced = text;
    if (this.#pending?.text === text) {
      this.#pending = null;
    } else if (this.#pending) {
      this.#rebase(this.#pending, text);
    }
    if (movedTo !== null && this.#pending) this.#pending.path = movedTo;
    this.#syncDirty();
    this.state.removed = false;
    this.state.saveError = null;
  }

  /**
   * Move notes on disk — the open one, or a folder holding it. `relocate` does the move and says
   * where paths went. It runs between saves, never alongside one: a save landing on the old path
   * mid-move would bring the note back there, or report it gone. An edit made while it ran is
   * carried to where its note went.
   */
  move(relocate: () => Promise<Remap>, from?: string | string[]): Promise<void> {
    // `from`: the notes or folders being moved. Saves to anything else neither wait for the move nor
    // hold it up. Without it, the move waits for every save and every save waits for it.
    const moving = from === undefined ? null : [from].flat();
    const root = this.deps.root();
    const covers = (path: string) => moving === null || moving.some((m) => pathInside(path, m));
    const before = [
      ...[...this.#chains.values()].filter((c) => c.root === root && covers(c.path)).map((c) => c.tail),
      ...this.#moves.filter((m) => m.root === root).map((m) => m.done),
    ];
    let finished!: () => void;
    const entry = { root, from: moving, done: new Promise<void>((r) => (finished = r)) };
    this.#moves.push(entry);
    const run = Promise.all(before).then(async () => {
      const remap = await relocate();
      // A move is in the vault open now: edits typed in another one stay where they are.
      if (this.#pending && this.#here(this.#pending)) this.#pending.path = remap(this.#pending.path);
      for (const edit of this.#queued) if (this.#here(edit)) edit.path = remap(edit.path);
      for (const entry of this.#stranded) {
        if (!this.#here(entry.edit)) continue;
        const to = remap(entry.edit.path);
        if (to === entry.edit.path) continue;
        entry.edit.path = to;
        // What was kept names the old place: keep it again under the new one, then let the old go.
        const old = entry.id;
        entry.id = null;
        void this.#keepOutlasting(entry).then(() => {
          if (old !== null) void this.#forget(entry.root, old);
        });
      }
      this.deps.moved?.(remap);
      this.#syncDirty();
    });
    const end = () => {
      this.#moves = this.#moves.filter((m) => m !== entry);
      finished();
    };
    run.then(end, end);
    return run;
  }

  async #keepAsCopy(job: PendingEdit, root: string): Promise<void> {
    const s = this.state;
    const name = noteStem(job.path);
    try {
      if (!this.deps.keepCopy) throw new Error("no place to keep them");
      const copy = await this.deps.keepCopy(root, job.path, job.text);
      // Said even over another warning — the copy is news — and the other warning is carried along.
      const others = this.#stranded.length > 0 ? " Other edits are still waiting to be saved." : "";
      s.saveError = friendlyError(
        `"${name}" also changed elsewhere before your earlier edits to it could be saved, so they were kept as "${noteStem(copy)}".${others}`,
      );
    } catch (e) {
      this.#strand(root, job);
      s.saveError = friendlyError(
        `Your last edits to "${name}" are not saved yet — it also changed elsewhere, and keeping them as a copy failed (${friendlyError(e).summary}). They are kept and tried again with every save.`,
      );
    }
  }

  /**
   * Keeps every edit not on disk yet — on its way, waiting its turn, or failed — where it outlasts the
   * app. True when all of them are kept (or there are none to keep).
   */
  async #keepEverythingUnwritten(): Promise<boolean> {
    if (!this.deps.keepStranded) return false;
    const root = this.deps.root();
    const waiting = new Set<PendingEdit>([...this.#running.keys(), ...this.#queued]);
    if (this.#pending) waiting.add(this.#pending);
    for (const edit of waiting) {
      // An edit to a note left keeps the vault it was typed in, which may no longer be the open one.
      const vault = this.#typedIn.get(edit) ?? root;
      if (this.#done.has(edit) || this.#stranded.some((k) => k.edit === edit) || !vault) continue;
      this.#strand(vault, edit);
    }
    await Promise.all(this.#stranded.map((k) => k.keeping));
    return this.#stranded.every((k) => k.id !== null);
  }

  #syncDirty(): void {
    this.state.dirty = this.#pending !== null && this.#isOpen(this.#pending);
    this.#syncStalled();
  }

  /** Whether `edit` was typed in the vault open now (an edit from before any vault counts as here). */
  #here(edit: PendingEdit): boolean {
    const typedIn = this.#typedIn.get(edit);
    return typedIn === undefined || typedIn === this.deps.root();
  }

  /** Whether `edit` is to the note open now — in this vault, not one of the same name elsewhere. */
  #isOpen(edit: PendingEdit): boolean {
    const open = this.deps.activePath();
    return open !== null && this.#here(edit) && samePath(edit.path, open);
  }

  #syncStalled(): void {
    const stalled = [...this.#slow].some((j) => this.#isOpen(j));
    this.state.stalled = stalled;
    if (stalled) for (const hear of [...this.#onStall]) hear();
  }

  /** Runs `job` in `path`'s turn: after the saves to it before this one, and after any move of it. */
  #onPath(root: string | null, path: string, job: () => Promise<void>): Promise<void> {
    const key = `${root ?? ""}\n${pathKey(path)}`;
    const before = [
      this.#chains.get(key)?.tail ?? Promise.resolve(),
      ...this.#moves
        .filter((m) => m.root === root && (m.from === null || m.from.some((f) => pathInside(path, f))))
        .map((m) => m.done),
    ];
    const run = Promise.all(before).then(job);
    const tail = run.catch(() => {});
    this.#chains.set(key, { root, path, tail });
    void tail.then(() => {
      if (this.#chains.get(key)?.tail === tail) this.#chains.delete(key);
    });
    return run;
  }

  /** The vault `edit` was typed in. */
  #rootOf(edit: PendingEdit): string | null {
    return this.#typedIn.get(edit) ?? this.deps.root();
  }

  /** Queues the edits to a note being left, in that note's turn. */
  #writeLeft(edit: PendingEdit): void {
    this.#left.add(edit);
    this.#queued.add(edit);
    void this.#onPath(this.#rootOf(edit), edit.path, () => this.#write(edit))
      .catch(() => {})
      .finally(() => this.#queued.delete(edit));
  }

  /** `edit` now starts from `base`, set on purpose — not from whatever the edit before it lands as. */
  #rebase(edit: PendingEdit, base: string): void {
    edit.base = base;
    this.#prev.delete(edit);
  }

  /** The nearest edit before `job` that is on disk, if any: `job` was typed on top of it. */
  #landedBefore(job: PendingEdit): PendingEdit | null {
    for (let p = this.#prev.get(job); p; p = this.#prev.get(p)) {
      if (this.#landed.has(p)) return p;
    }
    return null;
  }

  /** `job` landed: every edit before it is on disk inside it. */
  #landedNow(job: PendingEdit): void {
    this.#landed.add(job);
    this.#done.add(job);
    this.#unstrand(job); // kept outside the app at a close that did not wait for it: not needed now
    for (let p = this.#prev.get(job); p; p = this.#prev.get(p)) {
      this.#done.add(p);
      this.#queued.delete(p);
      this.#unstrand(p);
    }
  }

  #cancelTimer(): void {
    if (this.#timer) {
      clearTimeout(this.#timer);
      this.#timer = null;
    }
  }

  #write(job: PendingEdit, rootOf?: string): Promise<void> {
    // Already on disk (or held by a later edit that is): nothing to write. Already being written:
    // the same write, not a second one.
    if (this.#done.has(job)) return Promise.resolve();
    const running = this.#running.get(job);
    if (running) return running;
    const run = this.#writeOnce(job, rootOf).finally(() => this.#running.delete(job));
    this.#running.set(job, run);
    return run;
  }

  async #writeOnce(job: PendingEdit, rootOf?: string): Promise<void> {
    const root = rootOf ?? this.#typedIn.get(job) ?? this.deps.root();
    if (!root) return;
    const retry = rootOf !== undefined;
    const s = this.state;
    // The note open now, when it is in the vault this edit belongs to: one of the same name in
    // another vault is not this edit's note.
    const openHere = () => (root === this.deps.root() ? this.deps.activePath() : null);
    if (job.path === openHere() && (s.conflictDisk !== null || s.removed)) return;
    try {
      const landedBefore = this.#landedBefore(job);
      if (landedBefore) this.#rebase(job, landedBefore.text);
      const slow = setTimeout(() => {
        this.#slow.add(job);
        this.#syncStalled();
      }, this.#stallMs);
      let outcome: WriteOutcome;
      try {
        outcome = await this.deps.write(root, job.path, job.text, job.base);
      } finally {
        clearTimeout(slow);
        if (this.#slow.delete(job)) this.#syncStalled();
      }
      // The note already holds exactly these edits: an earlier try that seemed not to finish landed
      // after all (a write that outlasted the app that sent it). Nothing to keep a copy of.
      if (outcome.kind === "conflict" && outcome.disk === job.text) outcome = { kind: "written" };
      if (retry) this.#unstrand(job);
      if (outcome.kind === "written") this.#landedNow(job);
      const active = retry ? null : openHere();
      if (outcome.kind === "conflict") {
        if (job.path !== active) {
          // Edits to a note already left that changed (or went) on disk meanwhile: there is no banner
          // to ask on, and writing would overwrite the other change. Keep them as a copy beside it.
          await this.#keepAsCopy(job, root);
          return;
        }
        s.saveError = null;
        s.saveFailure = null;
        // Answered while this save was in flight (e.g. they took the copy on disk): nothing to ask.
        if (!this.#pending) return;
        if (outcome.disk === null) this.markRemoved();
        else s.conflictDisk = outcome.disk;
        return;
      }
      if (retry) {
        // Kept edits landed. Whatever was said about them no longer holds, unless others remain.
        if (this.#stranded.length === 0 && !this.state.saveFailure) this.state.saveError = null;
        this.deps.saved?.(root, job.path, job.text);
        if (root !== this.deps.root()) return;
        // The note was opened again meanwhile and shows what disk held before: disk now holds the
        // kept edits, so the screen and the save baseline follow — unless the person is typing in it,
        // whose edits then meet this change as any other change on disk.
        const ownPending = this.#pending !== null && this.#pending.path === job.path;
        if (job.path === this.deps.activePath() && !ownPending) {
          this.#synced = job.text;
          this.deps.landedOnOpen?.(job.text);
        }
        return;
      }
      this.deps.saved?.(root, job.path, job.text);
      // Written into a vault no longer open: nothing on screen is about it.
      if (root !== this.deps.root()) return;
      if (this.#left.has(job) && job.path === active) {
        // Edits to a note left, landing after it was opened again — from a read that may have come
        // before them. Nothing typed since: the screen follows the disk, if it is behind. Something
        // typed since was typed on what the screen showed, not on these edits; it keeps that base, so
        // its save meets these edits as a change on disk instead of writing over them.
        const ownPending = this.#pending !== null && samePath(this.#pending.path, job.path);
        if (!ownPending && this.#synced !== job.text) {
          this.#synced = job.text;
          this.deps.landedOnOpen?.(job.text);
        }
        return;
      }
      if (job.path === active) this.#synced = job.text;
      if (this.#pending === job) {
        this.#pending = null;
        this.#syncDirty();
      } else if (this.#pending && samePath(this.#pending.path, job.path)) {
        this.#rebase(this.#pending, job.text); // the newer edit now starts from what this save put on disk
      }
      // Only the open note's own save speaks for it: a note already left saving fine says nothing
      // about whether the open one's edits are saved.
      if (job.path === active) {
        s.saveFailure = null;
        // A note left behind with edits still not on disk keeps its warning up.
        if (this.#stranded.length === 0) s.saveError = null;
      }
    } catch (e) {
      const active = openHere();
      if (!retry && this.#pending !== job && job.path !== active) {
        // Edits to a note already left: nothing on screen holds them, so keep them to try again.
        this.#strand(root, job);
      }
      if (retry || this.#stranded.some((k) => k.edit === job)) {
        const why = friendlyError(e);
        s.saveError = friendlyError(
          `Your last edits to "${noteStem(job.path)}" are not saved yet (${why.summary}). They are kept and tried again with every save.`,
        );
        return;
      }
      // The edit stays pending, so it can be tried again.
      s.saveError = friendlyError(e);
      if (job.path === active) s.saveFailure = s.saveError;
    }
  }
}
