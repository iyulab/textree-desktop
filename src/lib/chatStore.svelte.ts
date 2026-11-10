import type { RootId } from "./ipc";
// Runes orchestration store for the multi-turn local AI Chat (workspace shell).
// NOT imported by tests (runes module — test only pure logic in ask.helpers.ts).
import { ask, cancelAsk, hostStatus, prepareGeneration, readNote, semanticSearch } from './ipc';
import type { SemanticHit, TreeNode } from './ipc';
import { aiHost } from './aiHost.svelte';
import { getByoConfig } from './byoConfig';
import type { DownloadSnapshot } from './modelDownload.helpers';
import {
  buildChatMessages,
  DEFAULT_FILE_MAX_CHARS,
  dropTrailingAssistantTurn,
  extractCitations,
  fileToContext,
  generatorFailureMessage,
  hasUsableContext,
  pruneOrphanedAssistantTurn,
  resolveGenerationGate,
  selectContext,
  type ChatTurn,
} from './ask.helpers';
import { collectScopeNotes, budgetConcat, buildSummaryMessages } from './summary.helpers';
import { buildSummaryNote, summaryNoteParent, summaryNoteBaseName } from './summaryNote.helpers';
import { createNoteWithContent } from './ipc';

export type ChatScopeKind = 'file' | 'folder' | 'vault';
export interface ChatScope {
  kind: ChatScopeKind;
  path: string | null; // file/folder path relative to the vault; null for whole-vault
  label: string;
}
export type ChatState =
  | 'idle' | 'searching' | 'generating' | 'done' | 'error' | 'empty' | 'preparing';

const CONTEXT_LIMIT = 5;

class ChatStore {
  scope = $state<ChatScope>({ kind: 'vault', path: null, label: 'Whole vault' });
  turns = $state<ChatTurn[]>([]);
  status = $state<ChatState>('idle');
  errorMessage = $state('');
  draft = $state('');
  started = $state(false);
  /** Active download snapshot while status === 'preparing'; null otherwise. */
  modelDownload = $state<DownloadSnapshot | null>(null);

  // Monotonic guard — each new run increments it; stale stream callbacks check
  // their captured seq against the current and discard if behind (askStore pattern).
  private seq = 0;
  // Tree snapshot captured at summarize() time, reused by run()'s summary branch on retry
  // (so retryGeneration need not re-thread the tree).
  private summaryTree: TreeNode[] = [];
  private summaryResult: import('./summary.helpers').BudgetResult | null = null;

  /** Begin a fresh conversation pinned to `scope` (New chat / re-scope / first entry). */
  startSession(scope: ChatScope) {
    this.cancel();
    this.scope = scope;
    this.turns = [];
    this.status = 'idle';
    this.errorMessage = '';
    this.draft = '';
    this.started = true;
  }

  /** Commit the draft as a user turn and run the pipeline. */
  async send(vault: RootId) {
    const q = this.draft.trim();
    if (!q) return;
    // A prior generation error can leave an orphaned (empty/partial) assistant turn at the tail.
    // The composer stays enabled in the 'error' state, so a user typing a new question instead of
    // clicking Retry would otherwise ship that orphan as model history. Symmetric with retryGeneration.
    this.turns = pruneOrphanedAssistantTurn(this.turns, this.status);
    this.turns = [...this.turns, { role: 'user', text: q, citations: [] }];
    this.draft = '';
    await this.run(vault);
  }

  /** Begin a fresh summary conversation pinned to the current scope; seed a synthetic summary turn. */
  async summarize(vault: RootId, tree: TreeNode[]) {
    this.summaryTree = tree;
    const label = this.scope.label;
    this.startSession(this.scope); // clears turns + cancels any in-flight stream
    this.turns = [{ role: 'user', text: `Summarize "${label}"`, citations: [], kind: 'summary' }];
    await this.run(vault);
  }

  /** Re-attempt the latest user turn (used by the 'preparing' retry timer). */
  async retry(vault: RootId) {
    await this.run(vault);
  }

  /** Save the current summary as a new note in the scope's folder; returns the new note path.
   *  `summaryText` is the summary turn's rendered text (passed by the UI so a follow-up Q&A
   *  turn cannot be mistaken for the summary). Throws if there is no summary to save. */
  async saveSummaryToNote(vault: RootId, summaryText: string): Promise<string> {
    if (!this.summaryResult) throw new Error('No summary to save');
    const parent = summaryNoteParent(this.scope.kind, this.scope.path);
    const name = summaryNoteBaseName(this.scope.label);
    const content = buildSummaryNote(
      this.scope.label,
      summaryText,
      this.summaryResult.hits.map((h) => h.path),
      {
        includedCount: this.summaryResult.includedCount,
        totalCount: this.summaryResult.totalCount,
        bodyTruncated: this.summaryResult.bodyTruncated,
      },
    );
    return createNoteWithContent(vault, parent, name, content);
  }

  private async run(vault: RootId) {
    const seq = ++this.seq;
    this.errorMessage = '';
    const lastUser = [...this.turns].reverse().find((t) => t.role === 'user');
    const q = lastUser?.text ?? '';
    if (!q) return;

    // Gate 1 + 2: host ready + generation model loaded (askStore 3-gate pattern).
    let st: Awaited<ReturnType<typeof hostStatus>>;
    try {
      st = await hostStatus();
    } catch {
      if (seq !== this.seq) return;
      this.status = 'error';
      this.modelDownload = null;
      this.errorMessage = 'Could not reach the local AI host.';
      return;
    }
    if (seq !== this.seq) return;
    const gate = resolveGenerationGate(st);
    if (gate === 'error') {
      this.status = 'error';
      this.modelDownload = null;
      this.errorMessage = generatorFailureMessage(st.generatorError ?? '', getByoConfig() !== null);
      return;
    }
    if (gate === 'preparing') {
      this.status = 'preparing';
      // Surface download progress (generator-first; fall back to embedder if generator not yet started).
      this.modelDownload = st.generatorDownload ?? st.embedderDownload;
      aiHost.startPolling(); // durable app-wide indicator, survives leaving the chat view
      // Only kick a prepare once the host itself is up (matches prior behavior; the host
      // command no-ops when not Ready anyway). The panel's retry timer re-polls.
      if (st.status === 'ready' && !st.generatorReady) {
        try {
          await prepareGeneration();
        } catch {
          // non-fatal: panel retries and re-polls
        }
      }
      return;
    }
    // gate === 'ready' — clear any stale download snapshot.
    this.modelDownload = null;
    // gate === 'ready' → fall through to retrieval.

    // Retrieval — branch on the latest user turn's intent (summary vs Q&A).
    this.status = 'searching';
    const isSummary = lastUser?.kind === 'summary';
    let ctx: SemanticHit[];
    try {
      if (isSummary) {
        ctx = await this.retrieveSummaryContext(vault);
      } else if (this.scope.kind === 'file' && this.scope.path) {
        const body = await readNote(vault, this.scope.path);
        ctx = selectContext(fileToContext(this.scope.path, body));
      } else {
        ctx = selectContext(await semanticSearch(vault, q, this.scope.path, CONTEXT_LIMIT));
      }
    } catch (e) {
      if (seq !== this.seq) return;
      this.status = 'error';
      this.errorMessage = String(e);
      return;
    }
    if (seq !== this.seq) return;

    if (!hasUsableContext(ctx)) {
      this.status = 'empty';
      return;
    }

    // Generation — append tokens to a freshly-pushed assistant turn.
    this.status = 'generating';
    const idx = this.turns.lastIndexOf(lastUser!);
    const priorTurns = this.turns.slice(0, idx);
    const messages = isSummary
      ? buildSummaryMessages(this.scope.label, this.summaryResult!)
      : buildChatMessages(priorTurns, q, ctx);
    const assistantIdx = this.turns.length;
    this.turns = [...this.turns, { role: 'assistant', text: '', citations: [] }];
    try {
      await ask(vault, messages, ctx, this.scope.path, (e) => {
        if (seq !== this.seq) return; // stale stream — discard
        // Mutate THROUGH the reactive proxy (this.turns[idx]); a captured local
        // ref to the pushed object is the un-proxied original and is NOT reactive.
        if (e.kind === 'token') this.turns[assistantIdx].text += e.text;
        else if (e.kind === 'citations') this.turns[assistantIdx].citations = extractCitations(e.hits);
        else if (e.kind === 'done') {
          if (e.reason) this.turns[assistantIdx].end = e.reason;
          this.status = 'done';
        }
        else if (e.kind === 'error') {
          this.status = 'error';
          this.errorMessage = e.message;
        }
      });
    } catch (e) {
      if (seq !== this.seq) return;
      this.status = 'error';
      this.errorMessage = String(e);
    }
  }

  /** Build summary context for the pinned scope: whole body for a file, budget-concat for folder/vault. */
  private async retrieveSummaryContext(vault: RootId): Promise<SemanticHit[]> {
    if (this.scope.kind === 'file' && this.scope.path) {
      const body = await readNote(vault, this.scope.path);
      const hits = fileToContext(this.scope.path, body);
      this.summaryResult = { hits, includedCount: hits.length, totalCount: 1, bodyTruncated: body.length > DEFAULT_FILE_MAX_CHARS };
      return hits;
    }
    const paths = collectScopeNotes(this.summaryTree, this.scope.path);
    const notes = await Promise.all(
      paths.map(async (path) => ({ path, body: await readNote(vault, path).catch(() => '') })),
    );
    this.summaryResult = budgetConcat(notes);
    return this.summaryResult.hits;
  }

  /** User-triggered retry after a generation failure (Retry button). Awaiting prepareGeneration
   *  lets the host clear its last error before we re-poll, so the gate does not immediately
   *  re-resolve to 'error' on a stale value. */
  async retryGeneration(vault: RootId) {
    // A mid-stream generation failure already pushed a partial assistant turn; drop it so the
    // retry doesn't stack a second one. (Generator-load failures return at the gate before any
    // assistant turn is pushed, so this is a no-op there.) Reached only from the 'error' state.
    this.turns = pruneOrphanedAssistantTurn(this.turns, this.status);
    this.errorMessage = '';
    this.status = 'preparing';
    try {
      await prepareGeneration();
    } catch {
      // non-fatal: re-poll below will reflect the host state.
    }
    await this.run(vault);
  }

  /** User-triggered "try again" on the last assistant turn (Regenerate button). Unlike
   *  retryGeneration() (error-state recovery via prepareGeneration + orphan pruning), this
   *  always drops the trailing assistant turn unconditionally and re-runs — it applies to
   *  'done' answers (complete or Stop-preserved), not generation failures. */
  async regenerate(vault: RootId) {
    this.turns = dropTrailingAssistantTurn(this.turns);
    this.errorMessage = '';
    await this.run(vault);
  }

  /** User-initiated Stop of an in-flight answer (Stop button). Unlike cancel(), this PRESERVES
   *  the partially streamed assistant turn — the user asked to keep what arrived so far — and
   *  settles into 'done' rather than discarding to 'idle'. Bumping seq makes any late stream
   *  callbacks no-op; cancelAsk() tells the host to stop generating (free CPU). No-op unless a
   *  run is actually in flight, so a stray call in an idle state cannot fake a 'done'. */
  stop() {
    if (this.status !== 'searching' && this.status !== 'generating') return;
    this.seq++;
    void cancelAsk(); // fire-and-forget: bump Rust ask_generation → host stops
    this.status = 'done';
  }

  /** Cancel any in-flight stream and stop the host generating (mode switch, app close). */
  cancel() {
    this.seq++;
    void cancelAsk(); // fire-and-forget: bump Rust ask_generation → host stops
    // Drop the half-streamed assistant turn before it freezes in the session: leaving it would
    // resurface a truncated answer (with clickable partial citations) on the next Chat entry.
    // Pass the pre-cancel status so the helper prunes only when a stream was actually in flight.
    this.turns = pruneOrphanedAssistantTurn(this.turns, this.status);
    this.status = 'idle';
  }
}

export const chatStore = new ChatStore();
