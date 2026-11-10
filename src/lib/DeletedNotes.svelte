<script lang="ts">
  import type { RootId } from "./ipc";
  import { deletedNotes, restoreDeleted, type DeletedNote, type TreeNode } from "./ipc";
  import { friendlyError, type FriendlyError } from "./friendlyError.helpers";
  import {
    containingFolder,
    displayName,
    emptyMessage,
    formatDeletedAt,
    restoredMessage,
    sortDeleted,
    standingNote,
  } from "./deletedNotes.helpers";

  interface Props {
    /** Absolute vault root path. */
    root: RootId;
    /**
     * The vault's tree as the sidebar shows it. The list is read again whenever it changes, so a
     * note deleted while the panel is open shows up in it.
     */
    tree: TreeNode[];
    /** Close the panel. */
    onclose: () => void;
    /** Called after a note is back, so the caller can refresh the tree. */
    onrestored: () => void;
  }
  let { root, tree, onclose, onrestored }: Props = $props();

  let notes = $state<DeletedNote[]>([]);
  let failure = $state<FriendlyError | null>(null);
  let outcome = $state<string | null>(null);

  $effect(() => {
    void tree; // re-read when the folder changes
    void load(root);
  });

  async function load(atRoot: RootId) {
    failure = null;
    try {
      notes = sortDeleted(await deletedNotes(atRoot));
    } catch (e) {
      failure = friendlyError(e);
    }
  }

  async function bringBack(rel: string) {
    failure = null;
    outcome = null;
    try {
      // Which of the two states comes back is decided by which is newer, and the answer says
      // which one it was — the difference is the person's unrecorded work.
      outcome = restoredMessage(rel, await restoreDeleted(root, rel));
      onrestored();
      await load(root);
    } catch (e) {
      failure = friendlyError(e);
    }
  }
</script>

<section class="panel" aria-label="Deleted notes" data-testid="deleted-notes">
  <header class="head">
    <h2 class="heading">Deleted notes</h2>
    <button class="close" onclick={onclose} aria-label="Close deleted notes">×</button>
  </header>

  {#if failure}
    <p class="failure" title={failure.raw !== failure.summary ? failure.raw : undefined}>
      {failure.summary}
    </p>
  {/if}
  {#if outcome}
    <p class="outcome" data-testid="deleted-outcome">{outcome}</p>
  {/if}

  {#if notes.length === 0 && !failure}
    <p class="empty">{emptyMessage()}</p>
  {:else}
    <ul class="list">
      {#each notes as note (note.rel)}
        <li class="entry">
          <span class="name">
            {displayName(note.rel)}
            {#if containingFolder(note.rel)}
              <span class="folder">in {containingFolder(note.rel)}</span>
            {/if}
          </span>
          <span class="when">{formatDeletedAt(note.seconds)}</span>
          <button
            class="restore"
            onclick={() => bringBack(note.rel)}
            data-testid="deleted-restore"
          >Restore</button>
        </li>
      {/each}
    </ul>
  {/if}

  <p class="standing">{standingNote()}</p>
</section>

<style>
  .panel {
    flex-shrink: 0;
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
  .empty,
  .standing {
    margin: 0;
    padding: var(--sp-2) 0;
    font-size: var(--font-size-smaller);
    color: var(--text-muted);
  }
  .failure,
  .outcome {
    margin: 0 0 var(--sp-2);
    padding: var(--sp-1) var(--sp-2);
    border-radius: var(--radius-s);
    background: var(--bg-secondary-alt);
    font-size: var(--font-size-smaller);
  }
  .failure {
    color: var(--text-error);
  }
  .outcome {
    color: var(--text-normal);
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--sp-1);
  }
  .entry {
    display: flex;
    align-items: center;
    gap: var(--sp-2);
    padding: var(--sp-1) 0;
    font-size: var(--font-size-small);
    color: var(--text-normal);
  }
  .name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .folder {
    color: var(--text-muted);
    font-size: var(--font-size-smaller);
    margin-left: var(--sp-1);
  }
  .when {
    color: var(--text-muted);
    font-size: var(--font-size-smaller);
  }
  .restore {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    color: var(--text-normal);
    font-family: inherit;
    font-size: var(--font-size-smaller);
    padding: var(--sp-1) var(--sp-2);
    cursor: pointer;
  }
  .restore:hover {
    background: var(--bg-secondary-alt);
  }
</style>
