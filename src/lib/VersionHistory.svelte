<script lang="ts">
  import type { RootId } from "./ipc";
  import { noteVersions, noteVersionText, restoreVersion, type NoteVersion } from "./ipc";
  import { friendlyError, type FriendlyError } from "./friendlyError.helpers";
  import { noteName } from "./addVersion.helpers";
  import {
    emptyMessage,
    formatRecordedAt,
    isCurrent,
    olderMessage,
    page,
    PAGE,
  } from "./versionHistory.helpers";

  interface Props {
    /** Absolute vault root path. */
    root: RootId;
    /** Path of the note whose states are listed, relative to the vault. */
    path: string;
    /** Moves on when the folder's history may have changed: the list is read again. */
    revision: number;
    /** Whether the note has edits that are not on disk yet. */
    dirty: boolean;
    /** Close the panel. */
    onclose: () => void;
    /** Called after the note on disk changed, so the caller can reload it. */
    onrestored: () => void;
  }
  let { root, path, revision, dirty, onclose, onrestored }: Props = $props();

  let versions = $state<NoteVersion[]>([]);
  let shown = $state(PAGE);
  let selected = $state<NoteVersion | null>(null);
  let preview = $state<string | null>(null);
  let failure = $state<FriendlyError | null>(null);

  let listing = $derived(page(versions, shown));

  $effect(() => {
    // Re-reads whenever the note being looked at changes, or its history may have.
    void revision;
    void load(root, path);
  });

  let reads = 0;
  async function load(atRoot: RootId, ofNote: string) {
    const mine = ++reads;
    failure = null;
    selected = null;
    preview = null;
    try {
      const found = await noteVersions(atRoot, ofNote);
      // A read begun earlier that answers later says less than the latest one.
      if (mine === reads) versions = found;
    } catch (e) {
      if (mine === reads) failure = friendlyError(e);
    }
  }

  async function show(version: NoteVersion) {
    failure = null;
    selected = version;
    preview = null;
    try {
      // Reads objects only; the note on disk is not touched by looking.
      preview = await noteVersionText(root, path, version.id);
    } catch (e) {
      failure = friendlyError(e);
    }
  }

  async function goBackTo(version: NoteVersion) {
    failure = null;
    try {
      await restoreVersion(root, path, version.id);
      onrestored();
      await load(root, path);
    } catch (e) {
      failure = friendlyError(e);
    }
  }
</script>

<section class="panel" aria-label="Version history" data-testid="version-history">
  <header class="head">
    <h2 class="heading">Version history — {noteName(path)}</h2>
    <button class="close" onclick={onclose} aria-label="Close version history">×</button>
  </header>

  {#if failure}
    <p class="failure" title={failure.raw !== failure.summary ? failure.raw : undefined}>
      {failure.summary}
    </p>
  {/if}

  {#if versions.length === 0 && !failure}
    <p class="empty">{emptyMessage()}</p>
  {:else}
    <ul class="list">
      {#each listing.visible as version (version.id)}
        {@const current = isCurrent(version, versions, dirty)}
        <li class="entry" class:selected={selected?.id === version.id}>
          <button class="pick" onclick={() => show(version)} data-testid="version-entry">
            <span class="dot" class:filled={current}></span>
            <span class="name">{version.message}</span>
            <span class="when">{formatRecordedAt(version.seconds)}</span>
          </button>
          {#if current}
            <span class="current">Current</span>
          {:else}
            <button
              class="restore"
              onclick={() => goBackTo(version)}
              data-testid="version-restore"
            >Restore this version</button>
          {/if}
        </li>
      {/each}
    </ul>

    {#if olderMessage(listing.older)}
      <footer class="more">
        <span>{olderMessage(listing.older)}</span>
        <button class="show-more" onclick={() => (shown += PAGE)}>Show more</button>
      </footer>
    {/if}
  {/if}

  {#if preview !== null}
    <pre class="preview" data-testid="version-preview">{preview}</pre>
  {/if}
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
  .failure {
    margin: 0;
    padding: var(--sp-2) 0;
    font-size: var(--font-size-small);
    color: var(--text-muted);
  }
  .failure {
    color: var(--text-error);
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
  }
  .entry.selected {
    background: var(--bg-secondary-alt);
    border-radius: var(--radius-s);
  }
  .pick {
    flex: 1;
    display: flex;
    align-items: center;
    gap: var(--sp-2);
    background: none;
    border: none;
    padding: var(--sp-1) var(--sp-2);
    text-align: left;
    cursor: pointer;
    color: var(--text-normal);
    font-family: inherit;
    font-size: var(--font-size-small);
  }
  .dot {
    width: 0.5rem;
    height: 0.5rem;
    flex: none;
    border-radius: 50%;
    border: 1px solid var(--text-muted);
  }
  .dot.filled {
    background: var(--text-muted);
  }
  .name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .when {
    color: var(--text-muted);
    font-size: var(--font-size-smaller);
  }
  .current {
    color: var(--text-muted);
    font-size: var(--font-size-smaller);
    padding: 0 var(--sp-2);
  }
  .restore,
  .show-more {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    color: var(--text-normal);
    font-family: inherit;
    font-size: var(--font-size-smaller);
    padding: var(--sp-1) var(--sp-2);
    cursor: pointer;
  }
  .restore:hover,
  .show-more:hover {
    background: var(--bg-secondary-alt);
  }
  .more {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--sp-2);
    padding-top: var(--sp-2);
    color: var(--text-muted);
    font-size: var(--font-size-smaller);
  }
  .preview {
    margin: var(--sp-3) 0 0;
    padding: var(--sp-2);
    max-height: 14rem;
    overflow: auto;
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    background: var(--bg-secondary);
    color: var(--text-normal);
    font-size: var(--font-size-smaller);
    white-space: pre-wrap;
  }
</style>
