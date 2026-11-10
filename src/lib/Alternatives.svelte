<script lang="ts">
  import type { RootId } from "./ipc";
  import {
    alternativeDraft,
    alternativeText,
    setAsideAlternative,
    useAlternative,
    type NoteAlternative,
  } from "./ipc";
  import { friendlyError, type FriendlyError } from "./friendlyError.helpers";
  import { noteName } from "./addVersion.helpers";
  import { formatRecordedAt } from "./versionHistory.helpers";
  import { sides, wordDiff } from "./alternatives.helpers";

  interface Props {
    /** Absolute vault root path. */
    root: RootId;
    /** Absolute path of the note. */
    path: string;
    alternative: NoteAlternative;
    /** What the note holds now, as the editor shows it. */
    current: string;
    /**
     * What the alternative holds on screen, when it is open in the editor. Otherwise it is read:
     * what is being typed into it if anything is, else its last version.
     */
    shown: string | null;
    /** Whether the note has edits that are not on disk yet. */
    dirty: boolean;
    /** Called before the alternative ends; false when what was typed into it is not kept yet. */
    beforeEnding: () => Promise<boolean>;
    /** Opens the alternative in the editor in place of the note. Absent when it already is. */
    onopen?: () => void;
    onclose: () => void;
    /**
     * Called after something changed: `noteChanged` when the note's file was written (the caller
     * reloads it); the list of alternatives is worth reading again either way.
     */
    onchanged: (noteChanged: boolean) => void;
  }
  let { root, path, alternative, current, shown, dirty, beforeEnding, onopen, onclose, onchanged }: Props =
    $props();

  let read = $state<string | null>(null);
  let busy = $state(false);
  let failure = $state<FriendlyError | null>(null);

  let text = $derived(shown ?? read);
  let compared = $derived(text === null ? null : sides(wordDiff(current, text)));

  $effect(() => {
    if (shown === null) void load(root, alternative.id);
  });

  async function load(atRoot: RootId, id: string) {
    failure = null;
    try {
      const [recorded, draft] = await Promise.all([alternativeText(atRoot, id), alternativeDraft(atRoot, id)]);
      read = draft ?? recorded;
    } catch (e) {
      failure = friendlyError(e);
    }
  }

  async function ending(work: () => Promise<void>) {
    failure = null;
    busy = true;
    try {
      if (!(await beforeEnding())) return;
      await work();
    } catch (e) {
      failure = friendlyError(e);
    } finally {
      busy = false;
    }
  }

  const useThisNote = () =>
    ending(async () => {
      await setAsideAlternative(root, alternative.id);
      onchanged(false);
      onclose();
    });

  const useTheAlternative = () =>
    ending(async () => {
      await useAlternative(root, path, alternative.id);
      onchanged(true);
      onclose();
    });
</script>

<section class="panel" aria-label="Compare with the alternative" data-testid="alternatives">
  <header class="head">
    <h2 class="heading">Compare — {noteName(path)}</h2>
    <button class="close" onclick={onclose} aria-label="Close comparison">×</button>
  </header>

  {#if alternative.arrived}
    <p class="origin">The alternative is from {alternative.author || "elsewhere"}, {formatRecordedAt(alternative.seconds)}.</p>
  {/if}
  <p class="hint">An alternative holds this note's text only. Images and other files are shared.</p>

  {#if failure}
    <p class="failure" title={failure.raw !== failure.summary ? failure.raw : undefined}>{failure.summary}</p>
  {/if}

  {#if compared}
    <div class="sides">
      <div class="side">
        <h3>This note</h3>
        <pre class="text" data-testid="alternative-current">{#each compared.before as s, i (i)}{#if s.kind === "removed"}<del>{s.text}</del>{:else}{s.text}{/if}{/each}</pre>
        <button onclick={() => void useThisNote()} disabled={busy} data-testid="alternative-use-current">Use this one</button>
      </div>
      <div class="side">
        <h3>Alternative</h3>
        <pre class="text" data-testid="alternative-text">{#each compared.after as s, i (i)}{#if s.kind === "added"}<ins>{s.text}</ins>{:else}{s.text}{/if}{/each}</pre>
        <span class="row">
          <button
            onclick={() => void useTheAlternative()}
            disabled={busy || dirty}
            title={dirty ? "Waiting for this note's edits to be saved" : undefined}
            data-testid="alternative-use-alternative"
          >Use this one</button>
          {#if onopen}
            <button onclick={onopen} disabled={busy} data-testid="alternative-edit">Open it</button>
          {/if}
        </span>
      </div>
    </div>
    <footer class="foot">
      <button class="quiet" onclick={() => void useThisNote()} disabled={busy} data-testid="alternative-set-aside">Set aside</button>
    </footer>
  {/if}
</section>

<style>
  .panel {
    flex-shrink: 0;
    max-height: 55vh;
    overflow: auto;
    border-top: 1px solid var(--border);
    padding: var(--sp-3) var(--sp-6);
    background: var(--bg-primary);
  }
  .head {
    display: flex;
    align-items: center;
    gap: var(--sp-2);
    margin-bottom: var(--sp-2);
  }
  .heading {
    flex: 1;
    margin: 0;
    font-size: var(--font-size-smaller);
    font-weight: var(--font-weight-semibold);
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .close {
    background: none;
    border: none;
    color: var(--text-muted);
    font-size: var(--font-size-ui);
    cursor: pointer;
    padding: 0 var(--sp-1);
    border-radius: var(--radius-s);
    line-height: 1;
  }
  .close:hover {
    background: var(--bg-secondary-alt);
    color: var(--text-normal);
  }
  .origin,
  .hint,
  .failure {
    margin: 0 0 var(--sp-2);
    font-size: var(--font-size-small);
    color: var(--text-muted);
  }
  .failure {
    color: var(--text-error);
  }
  .sides {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--sp-4);
  }
  .side {
    display: flex;
    flex-direction: column;
    gap: var(--sp-2);
    min-width: 0;
  }
  .side h3 {
    margin: 0;
    font-size: var(--font-size-small);
    font-weight: var(--font-weight-semibold);
    color: var(--text-normal);
  }
  .text {
    margin: 0;
    min-height: 8em;
    padding: var(--sp-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    background: var(--bg-secondary);
    color: var(--text-normal);
    font-family: inherit;
    font-size: var(--font-size-small);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  /* Marked by more than colour: struck through on one side, underlined on the other. */
  del {
    color: var(--text-error);
    text-decoration: line-through;
  }
  ins {
    color: var(--accent);
    text-decoration: underline;
  }
  .row {
    display: flex;
    gap: var(--sp-2);
  }
  .foot {
    margin-top: var(--sp-3);
  }
  .quiet {
    background: none;
    border: none;
    color: var(--text-muted);
    cursor: pointer;
    padding: 0;
    text-decoration: underline;
  }
</style>
