import { invoke, Channel } from "@tauri-apps/api/core";
import type { ChatMessage } from './ask.helpers';
import type { DownloadSnapshot } from './modelDownload.helpers';
import type { ByoConfig } from './byoConfig';
import type { PublishChoice } from './publishScope.helpers';

export type NodeKind = "leaf" | "container";

export interface TreeNode {
  name: string;
  kind: NodeKind;
  // The node's own path, relative to the vault (leaf=.md, container=directory). The address for
  // structure edits.
  path: string;
  // Rust PathBuf serializes to a string in JSON. null for a container with no body.
  body_path: string | null;
  children: TreeNode[];
}

/** The id a notes folder is named by in every command that works in it. Only `registerRoot` makes one. */
export type RootId = string & { readonly __rootId: unique symbol };

/** Makes the folder at `path` known and returns the id to name it by. Looks at nothing on disk. */
export async function registerRoot(path: string): Promise<RootId> {
  return invoke<RootId>("register_root", { path });
}

export async function openVault(rootId: RootId): Promise<TreeNode[]> {
  return invoke<TreeNode[]>("open_vault", { rootId });
}

/** Resolved default vault: its absolute path, and whether the preferred (Documents) location was
 * unusable so the vault was created in a fallback location (home/app-local-data) instead. */
export type DefaultVault = { path: string; fellBack: boolean };

/**
 * Ensures the default vault exists (created + welcome.md seeded on first run) and returns its
 * absolute path. The backend owns OS-path resolution, folder creation, and seeding, and falls
 * back through home → app-local-data when Documents is unusable so first run never blank-screens.
 */
export async function ensureDefaultVault(): Promise<DefaultVault> {
  return invoke<DefaultVault>("ensure_default_vault");
}

export async function listTree(rootId: RootId): Promise<TreeNode[]> {
  return invoke<TreeNode[]>("list_tree", { rootId });
}

export async function readNote(rootId: RootId, path: string): Promise<string> {
  return invoke<string>("read_note", { rootId, rel: path });
}

/**
 * What became of a save. `conflict` means nothing was written: the note no longer held `expected`
 * (someone changed it since), and `disk` is what it holds now — or `null` when it is gone.
 */
export type WriteOutcome = { kind: "written" } | { kind: "conflict"; disk: string | null };

/**
 * Replaces a note's content, but only while it still holds `expected` — the content the editor
 * and disk last agreed on. A save never overwrites a change it has not seen.
 */
export async function writeNote(
  rootId: RootId,
  path: string,
  content: string,
  expected: string,
): Promise<WriteOutcome> {
  return invoke<WriteOutcome>("write_note", { rootId, rel: path, content, expected });
}

/**
 * Records the given notes as one revision of the vault. Returns the revision identifier, or
 * `null` when the notes are already in the state the history holds and nothing was written.
 *
 * Rejects rather than partially applying: an ignored path, a path outside the vault, or a
 * repository busy with another operation all leave the history untouched.
 */
export async function commitNotes(
  rootId: RootId,
  paths: string[],
  message: string,
): Promise<string | null> {
  return invoke<string | null>("commit_notes", { rootId, rels: paths.map((p) => p), message });
}

/** One recorded state of a note. `id` is opaque — hand it back to `noteVersionText`. */
export type NoteVersion = {
  id: string;
  message: string;
  /** Unix epoch seconds. */
  seconds: number;
  author: string;
};

/** A note the history holds but the folder no longer does. */
export type DeletedNote = {
  /** Vault-root-relative, `/`-separated. */
  rel: string;
  /**
   * Unix epoch seconds of when it left the folder. For a note removed outside the application
   * nothing recorded that moment, and the last time it was written to history stands in.
   */
  seconds: number;
  /** Whether it was ever recorded deliberately, as opposed to only kept when deleted. */
  recorded: boolean;
};

/** Where a restored note's contents came from. */
export type RestoredNote = {
  /** Vault-root-relative, `/`-separated path it actually landed at. */
  rel: string;
  /**
   * True when the contents are the ones it held at the moment it left the folder, false when
   * they come from the last state recorded before that.
   */
  asDeleted: boolean;
};

/** Every recorded state of one note, newest first. */
export async function noteVersions(rootId: RootId, path: string): Promise<NoteVersion[]> {
  return invoke<NoteVersion[]>("note_versions", { rootId, rel: path });
}

/** What a note held at one recorded state. The file on disk is not touched. */
export async function noteVersionText(
  rootId: RootId,
  path: string,
  id: string,
): Promise<string> {
  return invoke<string>("note_version_text", { rootId, rel: path, id });
}

/**
 * Puts a note back to one of its recorded states, overwriting what is on disk.
 *
 * What is being replaced is kept first, so unrecorded work is never the price of going back.
 */
export async function restoreVersion(
  rootId: RootId,
  path: string,
  id: string,
): Promise<void> {
  return invoke<void>("restore_version", { rootId, rel: path, id });
}

/** One open alternative of a note: a second version kept beside it until a person picks one. */
export type NoteAlternative = {
  /** Opaque handle; hand it back to the other alternative calls. */
  id: string;
  /** Vault-root-relative, `/`-separated path of the note it is an alternative of. */
  rel: string;
  /** True when it arrived from elsewhere: the note was changed both here and there. */
  arrived: boolean;
  /** Unix epoch seconds of its newest version. */
  seconds: number;
  author: string;
  /** False while it holds the same as the note's last recorded version. */
  differs: boolean;
};

/** The open alternatives of the notes in a folder. */
export async function listAlternatives(rootId: RootId): Promise<NoteAlternative[]> {
  return invoke<NoteAlternative[]>("list_alternatives", { rootId });
}

/** Starts an alternative of a note from its last recorded version. The file is not touched. */
export async function startAlternative(rootId: RootId, path: string): Promise<NoteAlternative> {
  return invoke<NoteAlternative>("start_alternative", { rootId, rel: path });
}

/** What an alternative holds for its note. */
export async function alternativeText(rootId: RootId, id: string): Promise<string> {
  return invoke<string>("alternative_text", { rootId, id });
}

/**
 * What is being typed into an alternative — kept outside the folder until it is added as a version.
 * Null when nothing is: the alternative then holds just {@link alternativeText}.
 */
export async function alternativeDraft(rootId: RootId, id: string): Promise<string | null> {
  return invoke<string | null>("alternative_draft", { rootId, id });
}

/** Keeps what is being typed into an alternative. The note's file is not touched. */
export async function writeAlternativeDraft(rootId: RootId, id: string, text: string): Promise<void> {
  return invoke<void>("write_alternative_draft", { rootId, id, text });
}

/** Adds a version to an alternative. Null when it already holds exactly this. */
export async function addAlternativeVersion(
  rootId: RootId,
  id: string,
  text: string,
  message: string,
): Promise<string | null> {
  return invoke<string | null>("add_alternative_version", { rootId, id, text, message });
}

/**
 * Makes an alternative's version the note — written to the file and recorded as one new
 * version — and ends the alternative. What was on disk is kept first, and what was still being
 * typed into the alternative is its version.
 */
export async function useAlternative(rootId: RootId, path: string, id: string): Promise<void> {
  return invoke<void>("use_alternative", { rootId, rel: path, id });
}

/**
 * Ends an alternative without using it. The note stays as it is; the alternative is kept, with
 * what was still being typed into it.
 */
export async function setAsideAlternative(rootId: RootId, id: string): Promise<void> {
  return invoke<void>("set_aside_alternative", { rootId, id });
}

/** What a folder gave up when the app stopped keeping things inside it. */
export type MoveOut = {
  /** Whether settings were carried over to where they live now. */
  settings: boolean;
  /** How many notes the set-aside copies held, now reachable as deleted notes. */
  notes: number;
  /** How many other files (attachments kept beside a deleted note) came with them. */
  files: number;
  /** What is still inside the folder's `.textree/` afterwards, relative to the folder. */
  leftBehind: string[];
};

/**
 * Moves everything the app keeps out of the notes folder, keeping every file.
 *
 * Safe to call on every open: a folder with nothing left inside it reports that nothing moved.
 */
export async function moveStateOutOfVault(rootId: RootId): Promise<MoveOut> {
  return invoke<MoveOut>("move_state_out_of_vault", { rootId });
}

/** Everything the history holds that is no longer in the folder, newest first. */
export async function deletedNotes(rootId: RootId): Promise<DeletedNote[]> {
  return invoke<DeletedNote[]>("deleted_notes", { rootId });
}

/**
 * Brings a deleted note back, in whichever of its two possible states is the newer: what it
 * held when it left the folder, or the last state recorded before that.
 *
 * Never overwrites: if the name is in use the copy is numbered alongside it.
 */
export async function restoreDeleted(rootId: RootId, rel: string): Promise<RestoredNote> {
  return invoke<RestoredNote>("restore_deleted", { rootId, rel });
}

// ── Structure edits (M4) ────────────────────────────────────────────────

export async function createNote(
  rootId: RootId,
  parent: string,
  name: string,
): Promise<string> {
  return invoke<string>("create_note", { rootId, parent, name });
}

export async function createUntitledNote(rootId: RootId, parent: string): Promise<string> {
  return invoke<string>("create_untitled_note", { rootId, parent });
}

export async function createNoteWithContent(
  rootId: RootId,
  parent: string,
  name: string,
  content: string,
): Promise<string> {
  return invoke<string>("create_note_with_content", { rootId, parent, name, content });
}

export async function createFolder(
  rootId: RootId,
  parent: string,
  name: string,
): Promise<string> {
  return invoke<string>("create_folder", { rootId, parent, name });
}

export async function renameNode(
  rootId: RootId,
  path: string,
  name: string,
): Promise<string> {
  return invoke<string>("rename_node", { rootId, rel: path, name });
}

export async function renameNoteUnique(
  rootId: RootId,
  path: string,
  name: string,
): Promise<string> {
  return invoke<string>("rename_note_unique", { rootId, rel: path, name });
}

export async function moveNode(
  rootId: RootId,
  path: string,
  dest: string,
): Promise<string> {
  return invoke<string>("move_node", { rootId, rel: path, dest });
}

/** Promote a leaf note to a container and move the path node into it. Returns the new path. */
export async function adoptNode(
  rootId: RootId,
  path: string,
  leaf: string,
): Promise<string> {
  return invoke<string>("adopt_node", { rootId, rel: path, leaf });
}

/**
 * Save an attached image to assets/ next to the note (note, .md). data is base64 bytes.
 * Returns the relative link to insert into the body (e.g. "assets/Pasted-….png").
 */
export async function saveAttachment(
  rootId: RootId,
  note: string,
  data: string,
  ext: string,
): Promise<string> {
  return invoke<string>("save_attachment", { rootId, note, data, ext });
}

export async function deleteNode(rootId: RootId, path: string): Promise<void> {
  return invoke<void>("delete_node", { rootId, rel: path });
}

export async function promoteNode(
  rootId: RootId,
  path: string,
): Promise<string> {
  return invoke<string>("promote_node", { rootId, rel: path });
}

// ── Sidecar persistence (per-vault settings kept with the app) ────────

/** Read the `<rel>` sidecar kept for this vault. null if absent. */
export async function readSidecar(
  rootId: RootId,
  rel: string,
): Promise<string | null> {
  return invoke<string | null>("read_sidecar", { rootId, rel });
}

/** Atomic write of the `<rel>` sidecar kept for this vault (parent auto-created). */
export async function writeSidecar(
  rootId: RootId,
  rel: string,
  content: string,
): Promise<void> {
  return invoke<void>("write_sidecar", { rootId, rel, content });
}

/**
 * Bring this vault's settings to the format this release keeps them in; run when the vault opens,
 * before anything reads them. `readOnly`: a newer release saved them, and nothing may be written
 * over them until the app is updated.
 */
export async function prepareSidecar(rootId: RootId): Promise<{ readOnly: boolean }> {
  return invoke<{ readOnly: boolean }>("prepare_sidecar", { rootId });
}

/** An edit to a note already left, kept because it could not be written to it yet. */
export interface StrandedEdit {
  id: string;
  path: string;
  text: string;
  base: string;
}

/** Keep an unwritten edit where it outlasts the app. Returns its id. */
export async function keepStranded(rootId: RootId, path: string, text: string, base: string): Promise<string> {
  return invoke<string>("keep_stranded", { rootId, rel: path, text, base });
}

/** Let go of a kept edit once it has landed. */
export async function forgetStranded(rootId: RootId, id: string): Promise<void> {
  return invoke<void>("forget_stranded", { rootId, id });
}

/** Edits kept for this vault's notes from an earlier run, oldest first. */
export async function listStranded(rootId: RootId): Promise<StrandedEdit[]> {
  const kept = await invoke<(Omit<StrandedEdit, "path"> & { rel: string })[]>("list_stranded", { rootId });
  return kept.map(({ rel, ...edit }) => ({ ...edit, path: rel }));
}

/** Move an unreadable settings file aside, keeping it. Returns the name it is kept under. */
export async function setAsideSidecar(rootId: RootId, rel: string): Promise<string> {
  return invoke<string>("set_aside_sidecar", { rootId, rel });
}

// ── Body full-text search (P1b) ──────────────────────────────────────────

export interface SearchHit {
  /** Vault-relative path (POSIX). The front end combines it with root to open the absolute path. */
  path: string;
  title: string;
  snippet: string;
  /** Highlight [start, end) char indices within the snippet string. */
  ranges: [number, number][];
}

/** Body full-text search. Empty array if the index is missing or the query is empty. */
export async function searchContent(
  query: string,
  limit = 50,
): Promise<SearchHit[]> {
  return invoke<SearchHit[]>("search_content", { query, limit });
}

/** Full reindex (>reindex command). */
export async function rebuildIndex(rootId: RootId): Promise<void> {
  return invoke<void>("rebuild_index", { rootId });
}

// ── Publishing (P2 — render the vault to a static site via canopy) ─────────

export interface PublishOptions {
  /** Overrides the site title (defaults to the vault folder name). */
  siteTitle?: string;
  /** Design-token CSS content injected so the published site matches the app. */
  tokensCss?: string;
}

export interface PublishResult {
  pageCount: number;
  outDir: string;
}

/**
 * Publish the vault to a static site at `outDir` (which must be outside the vault). Read-only over
 * the source: the vault `.md` is never mutated. Spawns the canopy renderer in the backend.
 */
export async function publishSite(
  rootId: RootId,
  outDir: string,
  options: PublishOptions = {},
): Promise<PublishResult> {
  return invoke<PublishResult>("publish_site", { rootId, outDir, options });
}

/** What publishing the vault would send out — shown for confirmation before anything is sent. */
export interface PublishPreview {
  /** Nothing in the folder is chosen for publishing. */
  nothingChosen: boolean;
  /** Chosen notes that go out, as their last added version. */
  notes: string[];
  /** Files those notes link to that go out with them. */
  files: string[];
  /** Of the notes going out, those whose file holds edits not added as a version. */
  unrecorded: string[];
  /** Chosen notes that have no version yet and so stay out. */
  neverRecorded: string[];
  /** Files linked from published notes that are not in the last added versions. */
  missing: string[];
  /** Notes linked from published notes that do not go out. */
  unpublishedLinks: string[];
  /** Linked files left out because their own nearest choice is "leave out". */
  withheld: string[];
  /** Linked files left out because they are too large to publish. */
  tooLarge: string[];
}

/** List what publishing the vault would send out. Reads only. */
export async function publishPreview(rootId: RootId): Promise<PublishPreview> {
  return invoke<PublishPreview>("publish_preview", { rootId });
}

/** The folder's publish choices, relative to it ("" = the whole folder). */
export async function publishChoices(rootId: RootId): Promise<PublishChoice[]> {
  return invoke<PublishChoice[]>("publish_choices", { rootId });
}

/** Include (true), leave out (false), or follow the folder above (null). Returns the new choices. */
export async function setPublishChoice(
  rootId: RootId,
  rel: string,
  choice: boolean | null,
): Promise<PublishChoice[]> {
  return invoke<PublishChoice[]>("set_publish_choice", { rootId, rel, choice });
}

// ── Remote (exchange recorded notes with a remote the person connected) ─────────────────────

/** Where this folder's remote is. The secret sent with it is never returned. */
export interface RemoteConnection {
  url: string;
  username: string;
}

/** What one exchange with the remote did. Notes are named relative to the folder. */
export interface RemoteExchange {
  /** Notes written here because they arrived. */
  received: string[];
  /** Notes removed here because the other side removed them (kept among deleted notes). */
  removed: string[];
  /** Notes both sides changed. Nothing moved for them; a person decides. */
  held: string[];
  /** Of those, the notes whose version from elsewhere now waits as an alternative. */
  alternatives: string[];
  /** Notes the other side changed that hold unrecorded edits here. Nothing moved. */
  keptBack: string[];
  /**
   * Notes of another folder in the same repository that are held or kept back, named relative to
   * the repository. They hold this folder's exchange too, and only that folder can show them.
   */
  waitingElsewhere: string[];
  /** The remote now holds everything recorded here. */
  sent: boolean;
}

export async function remoteConnection(rootId: RootId): Promise<RemoteConnection | null> {
  return invoke<RemoteConnection | null>("remote_connection", { rootId });
}

/** Reaches the remote with `secret` first and remembers it only when that worked. */
export async function connectRemote(
  rootId: RootId,
  url: string,
  username: string,
  secret: string,
): Promise<void> {
  return invoke<void>("connect_remote", { rootId, url, username, secret });
}

/** Forgets this folder's remote on this machine. Nothing on the remote changes. */
export async function disconnectRemote(rootId: RootId): Promise<void> {
  return invoke<void>("disconnect_remote", { rootId });
}

/**
 * Whether this folder's recorded notes are somewhere besides this machine: connected to a remote
 * that held everything recorded here at the last exchange. Asks nothing over the network.
 */
export async function notesBackedUp(rootId: RootId): Promise<boolean> {
  return invoke<boolean>("notes_backed_up", { rootId });
}

/** One exchange: take in what arrived where it can, send what was recorded here. */
export async function syncRemote(rootId: RootId): Promise<RemoteExchange> {
  return invoke<RemoteExchange>("sync_remote", { rootId });
}

export interface CloudPublishResult {
  url: string;
  pageCount: number;
}

/** Render the open vault locally and upload it to pub.textree.me via api /publish. The publish
 * token is read from the OS keychain in the backend (never passed from JS). Reuses the same
 * read-only canopy render as `publishSite`. */
export async function publishToCloud(
  rootId: RootId,
  options: PublishOptions = {},
): Promise<CloudPublishResult> {
  return invoke<CloudPublishResult>("publish_to_cloud", { rootId, options });
}

/** Run the in-app web-publish sign-in (browser OAuth loopback + PKCE) and store the resulting
 * token in the OS Credential Manager. Resolves once connected; rejects on cancel/timeout/error.
 * The desktop only ever receives the opaque token — never the cloud identity. */
export function connectPublish(): Promise<void> {
  return invoke<void>("connect_publish");
}

export function clearPublishToken(): Promise<void> {
  return invoke<void>("clear_publish_token");
}

export function hasPublishToken(): Promise<boolean> {
  return invoke<boolean>("has_publish_token");
}






// ── Semantic search (AI-sidecar) ──────────────────────────────────────────

export interface SemanticHit {
  /** Relative to the vault, `/`-separated. */
  path: string;
  snippet: string;
  score: number;
}

export type HostStatus = "starting" | "ready" | "unavailable";

export async function semanticSearch(
  rootId: RootId,
  query: string,
  scopePath: string | null,
  limit = 20,
): Promise<SemanticHit[]> {
  return invoke<SemanticHit[]>("semantic_search", {
    rootId,
    query,
    scope: scopePath,
    limit,
  });
}

export async function hostStatus(): Promise<{
  status: HostStatus;
  generatorReady: boolean;
  generatorError: string | null;
  embedderError: string | null;
  embedderDownload: DownloadSnapshot | null;
  generatorDownload: DownloadSnapshot | null;
  activeProvider: string;
}> {
  return invoke("host_status");
}

/** Why an answer ended short of finishing: it reached the length limit, or it began repeating itself. */
export type AnswerEnd = 'length' | 'degeneration';

export type AskEvent =
  | { kind: 'token'; text: string }
  | { kind: 'citations'; hits: SemanticHit[] }
  /** `reason` only when the answer ended short of finishing and the backend said why. */
  | { kind: 'done'; reason?: AnswerEnd }
  | { kind: 'error'; message: string };

export function ask(
  rootId: RootId,
  messages: ChatMessage[],
  citationHits: SemanticHit[],
  scopePath: string | null,
  onEvent: (e: AskEvent) => void,
): Promise<void> {
  const channel = new Channel<AskEvent>();
  channel.onmessage = onEvent;
  return invoke<void>('ask', { rootId, messages, citationHits, scope: scopePath, onEvent: channel });
}

export function prepareGeneration(): Promise<void> {
  return invoke<void>('prepare_generation');
}

/** Fire-and-forget: bump Rust ask_generation so any in-flight ask stream aborts on its next line read. */
export function cancelAsk(): Promise<void> {
  return invoke<void>('cancel_ask');
}

/** Spawn the local-AI host if not already up. Idempotent no-op if already Starting/Ready. When
 * `byo` is given, spawns pointed at that OpenAI-compatible endpoint instead of the bundled local
 * model; omitted (or undefined) spawns the bundled local model as before. */
export async function prepareAiModel(byo?: ByoConfig): Promise<void> {
  return invoke<void>("prepare_ai_model", {
    preset: byo?.preset ?? null,
    baseUrl: byo?.baseUrl ?? null,
    model: byo?.model ?? null,
  });
}

/** Force-reconfigure the host (Settings ▸Advanced Save / switch back to bundled local model):
 * unlike `prepareAiModel` (no-op if already up), this always stops the current host and spawns a
 * fresh one with the given config (or the bundled local model if `byo` is omitted). */
export async function restartAiHost(byo?: ByoConfig): Promise<void> {
  return invoke<void>("restart_ai_host", {
    preset: byo?.preset ?? null,
    baseUrl: byo?.baseUrl ?? null,
    model: byo?.model ?? null,
  });
}

export type ByoConnectionTestResult = { ok: true; modelCount: number } | { ok: false; message: string };

/** Test-connect to a candidate BYO endpoint (Settings ▸Advanced "Test connection") without
 * touching the running host: the AI helper checks it once — a real, authenticated model-list
 * request — and reports how many models the server offers. */
export async function testByoConnection(
  preset: string,
  baseUrl: string,
  apiKey?: string,
): Promise<ByoConnectionTestResult> {
  try {
    const found = await invoke<{ modelCount: number }>("test_byo_connection", {
      preset,
      baseUrl,
      apiKey: apiKey ?? null,
    });
    return { ok: true, modelCount: found.modelCount };
  } catch (e) {
    return { ok: false, message: String(e) };
  }
}

/** Store the BYO API key in the OS Credential Manager. Never persisted in localStorage. */
export function setByoApiKey(key: string): Promise<void> {
  return invoke<void>("set_byo_api_key", { key });
}

export function clearByoApiKey(): Promise<void> {
  return invoke<void>("clear_byo_api_key");
}

export function hasByoApiKey(): Promise<boolean> {
  return invoke<boolean>("has_byo_api_key");
}

/** Stop the local-AI host now (Settings → turn local AI off). No-op-safe if already down. */
export function stopHost(): Promise<void> {
  return invoke<void>("stop_host");
}

/** Open the OS app log directory in the system file explorer. */
export async function openLogDir(): Promise<void> {
  return invoke<void>("open_log_dir");
}
