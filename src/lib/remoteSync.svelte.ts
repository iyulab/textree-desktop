import type { RootId } from "./ipc";
/*
 * Backup store — which repository the open folder backs up to, and the exchanges with it.
 *
 * One place owns the rules every surface shares (the backup dialog, the Backup section in
 * Settings, and the page that exchanges in the background):
 *
 * - two exchanges never run at once for the same folder: one asked for while another runs waits
 *   for it and then runs once, however many were asked for meanwhile;
 * - what an exchange did is handed to the page together with the folder it was for, so the page
 *   can ignore anything about a folder it has since left;
 * - when the folder last exchanged is kept in memory only — nothing new is saved for it.
 *
 * The wording lives in remoteSync.helpers.ts (pure, tested); this module is the thin reactive
 * wrapper around the IPC calls and is not imported by tests.
 */

import {
  connectRemote,
  disconnectRemote,
  remoteConnection,
  syncRemote,
  type RemoteConnection,
  type RemoteExchange,
} from "./ipc";
import { friendlyError, type FriendlyError } from "./friendlyError.helpers";
import { manualOutcome } from "./remoteSync.helpers";

/** Asked for by the person ("manual") or started by the app ("background"). */
export type SyncReason = "manual" | "background";

/** How the page hears about what happened to a folder. */
export interface BackupListener {
  exchanged: (root: RootId, exchange: RemoteExchange, reason: SyncReason) => void;
  failed: (root: RootId, error: FriendlyError, reason: SyncReason) => void;
  /** The folder was connected or disconnected. */
  connectionChanged: (root: RootId) => void;
}

/** What this session knows about one folder's exchanges. */
interface Session {
  running: boolean;
  /** When the last exchange finished without error (ms since epoch). */
  syncedAt: number | null;
  /** What the last exchange the person asked for came to. */
  outcome: string | null;
  failure: FriendlyError | null;
}

const IDLE: Session = { running: false, syncedAt: null, outcome: null, failure: null };

class BackupStore {
  /** The folder the connection below belongs to. */
  root = $state<RootId | null>(null);
  /** Its repository, or null when it backs up nowhere. Meaningful once `known`. */
  connection = $state<RemoteConnection | null>(null);
  /** Whether the connection of `root` has been looked up. */
  known = $state(false);
  /** Why the connection could not be looked up, if it could not. */
  lookupError = $state<FriendlyError | null>(null);

  private sessions = $state<Record<string, Session>>({});
  private listener: BackupListener | null = null;
  private inFlight = new Map<string, Promise<void>>();
  private queued = new Map<string, { promise: Promise<void>; reason: SyncReason }>();
  private lookups = 0;

  /** This session's exchanges for the open folder. */
  get session(): Session {
    return (this.root && this.sessions[this.root]) || IDLE;
  }

  listen(listener: BackupListener): void {
    this.listener = listener;
  }

  /** The open folder changed: look up where it backs up to. */
  async select(root: RootId | null): Promise<void> {
    this.root = root;
    this.connection = null;
    this.known = false;
    this.lookupError = null;
    if (root) await this.lookup(root);
  }

  private async lookup(root: RootId): Promise<RemoteConnection | null> {
    const mine = ++this.lookups;
    try {
      const found = await remoteConnection(root);
      if (mine === this.lookups && root === this.root) {
        this.connection = found;
        this.lookupError = null;
        this.known = true;
      }
      return found;
    } catch (e) {
      if (mine === this.lookups && root === this.root) {
        this.connection = null;
        this.lookupError = friendlyError(e);
        this.known = true;
      }
      return null;
    }
  }

  /**
   * Connects `root` to the repository at `url` and exchanges with it once. Throws when the
   * repository could not be reached with `token` — then nothing was saved.
   */
  async connect(root: RootId, url: string, token: string): Promise<void> {
    await connectRemote(root, url, "", token);
    this.patch(root, { outcome: null, failure: null });
    await this.lookup(root);
    this.listener?.connectionChanged(root);
    await this.sync(root, "manual");
  }

  /** Stops backing `root` up on this machine. What the repository holds stays there. */
  async disconnect(root: RootId): Promise<void> {
    await disconnectRemote(root);
    this.patch(root, { syncedAt: null, outcome: null, failure: null });
    await this.lookup(root);
    this.listener?.connectionChanged(root);
  }

  /** Exchanges with `root`'s repository if it has one. Never throws. */
  async syncIfConnected(root: RootId, reason: SyncReason = "background"): Promise<void> {
    let connected: RemoteConnection | null;
    try {
      connected = await remoteConnection(root);
    } catch {
      return; // the lookup failing is said where the connection is shown, not here
    }
    if (connected) await this.sync(root, reason);
  }

  /**
   * One exchange for `root`. While another runs for it, this waits for that one and runs once
   * afterwards — joined with any others asked for meanwhile. Never throws: the listener hears.
   */
  sync(root: RootId, reason: SyncReason): Promise<void> {
    const waiting = this.queued.get(root);
    if (waiting) {
      if (reason === "manual") waiting.reason = "manual";
      return waiting.promise;
    }
    const running = this.inFlight.get(root);
    if (!running) return this.start(root, reason);
    const entry = { reason, promise: Promise.resolve() };
    entry.promise = running.then(() => {
      this.queued.delete(root);
      return this.start(root, entry.reason);
    });
    this.queued.set(root, entry);
    return entry.promise;
  }

  private start(root: RootId, reason: SyncReason): Promise<void> {
    this.patch(root, { running: true });
    const run = this.run(root, reason).finally(() => {
      this.inFlight.delete(root);
      this.patch(root, { running: this.queued.has(root) });
    });
    this.inFlight.set(root, run);
    return run;
  }

  private async run(root: RootId, reason: SyncReason): Promise<void> {
    try {
      const exchange = await syncRemote(root);
      this.patch(root, {
        syncedAt: Date.now(),
        failure: null,
        outcome: reason === "manual" ? manualOutcome(exchange) : this.sessionOf(root).outcome,
      });
      this.listener?.exchanged(root, exchange, reason);
    } catch (e) {
      const failure = friendlyError(e);
      this.patch(root, { failure, outcome: null });
      this.listener?.failed(root, failure, reason);
    }
  }

  private sessionOf(root: RootId): Session {
    return this.sessions[root] ?? IDLE;
  }

  private patch(root: RootId, change: Partial<Session>): void {
    this.sessions[root] = { ...this.sessionOf(root), ...change };
  }
}

export const backupStore = new BackupStore();
