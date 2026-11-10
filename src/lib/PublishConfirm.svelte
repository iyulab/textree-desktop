<script lang="ts">
  import type { RootId } from "./ipc";
  import { onMount } from "svelte";
  import { commitNotes, publishPreview, type PublishPreview } from "./ipc";
  import { friendlyError, type FriendlyError } from "./friendlyError.helpers";
  import { publishScope } from "./publishScope.svelte";
  import { versionName } from "./addVersion.helpers";
  import {
    canPublish,
    missingLine,
    neverRecordedLine,
    nothingChosenLine,
    publishSummary,
    tooLargeLine,
    unpublishedLinksLine,
    unrecordedLead,
    unrecordedNames,
    withheldLine,
    type PublishDestination,
  } from "./publishConfirm.helpers";

  interface Props {
    /** Absolute path of the folder to publish. */
    root: RootId;
    /** Where the site goes. */
    destination: PublishDestination;
    /** The person chose to publish. */
    onpublish: () => void;
    /** The person chose not to. Nothing is published. */
    oncancel: () => void;
    /**
     * Gets the open note's edits onto disk before a version is added from here; false when they
     * could not be (the reason is on screen).
     */
    settleOpenNote: () => Promise<boolean>;
    /** Versions were added from the dialog. */
    onadded: () => void;
  }
  let { root, destination, onpublish, oncancel, settleOpenNote, onadded }: Props = $props();

  let preview = $state<PublishPreview | null>(null);
  let failure = $state<FriendlyError | null>(null);
  let scrim = $state<HTMLDivElement | null>(null);
  let go = $state<HTMLButtonElement | null>(null);

  const ready = $derived(
    preview !== null && failure === null && canPublish(preview.notes.length, preview.files.length),
  );
  let choosing = $state(false);

  function load() {
    preview = null;
    publishPreview(root).then(
      (found) => (preview = found),
      (e) => (failure = friendlyError(e)),
    );
  }

  /**
   * Why the last action asked for from the dialog did not happen. Said beside its button, so the
   * rest of the dialog stays and the person can try again; cleared when they do.
   */
  let actionFailure = $state<FriendlyError | null>(null);

  /** The person chose to publish everything: recorded as a choice (the sidebar learns of it too), then shown again. */
  async function chooseWholeFolder() {
    choosing = true;
    actionFailure = null;
    try {
      await publishScope.choose("", true);
      load();
    } catch (e) {
      actionFailure = friendlyError(e);
    } finally {
      choosing = false;
    }
  }

  let adding = $state(false);

  /** Adds one version of the chosen notes that have none yet, then shows what goes out again. */
  async function addVersions() {
    if (!preview || adding) return;
    const paths = preview.neverRecorded;
    adding = true;
    actionFailure = null;
    try {
      // A version of what is on disk is only the right one once the open note's edits are there.
      if (!(await settleOpenNote())) {
        actionFailure = friendlyError("The open note's changes are not saved yet, so no version was added.");
        return;
      }
      await commitNotes(root, paths, versionName("", paths));
      onadded();
      load();
    } catch (e) {
      actionFailure = friendlyError(e);
    } finally {
      adding = false;
    }
  }

  onMount(() => {
    scrim?.focus();
    load();
  });

  // Publish is the answer the dialog expects, so it takes focus as soon as it can be pressed.
  $effect(() => {
    if (ready) go?.focus();
  });

  function onkeydown(event: KeyboardEvent) {
    if (event.key === "Escape") {
      event.preventDefault();
      oncancel();
    }
  }
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
  bind:this={scrim}
  class="scrim"
  role="dialog"
  aria-modal="true"
  aria-label="Publish this folder?"
  tabindex="-1"
  {onkeydown}
  data-testid="publish-confirm"
>
  <div class="dialog">
    <h2 class="title">Publish this folder?</h2>

    {#if failure}
      <p class="failure" data-testid="publish-confirm-error">{failure.summary}</p>
    {:else if preview === null}
      <p class="muted" aria-busy="true" data-testid="publish-confirm-loading">
        Checking what will be published…
      </p>
    {:else if preview.nothingChosen}
      <p class="summary" data-testid="publish-confirm-nothing-chosen">{nothingChosenLine()}</p>
      <button
        class="btn"
        onclick={chooseWholeFolder}
        disabled={choosing}
        data-testid="publish-confirm-choose-all">Publish the whole notes folder</button
      >
      {#if actionFailure}
        <p class="failure" role="alert" data-testid="publish-confirm-action-error">{actionFailure.summary}</p>
      {/if}
    {:else}
      <p class="summary" data-testid="publish-confirm-summary">
        {publishSummary(preview.notes.length, preview.files.length, destination)}
      </p>
      {#if unrecordedLead(preview.unrecorded)}
        <div class="notice" data-testid="publish-confirm-unrecorded">
          <p>{unrecordedLead(preview.unrecorded)}</p>
          <p class="names">{unrecordedNames(preview.unrecorded)}</p>
        </div>
      {/if}
      {#if neverRecordedLine(preview.neverRecorded)}
        <div class="with-action">
          <p class="muted" data-testid="publish-confirm-never-recorded">{neverRecordedLine(preview.neverRecorded)}</p>
          <button
            class="btn small"
            onclick={addVersions}
            disabled={adding}
            data-testid="publish-confirm-add-versions">Add a version of these notes</button
          >
          {#if actionFailure}
            <p class="failure" role="alert" data-testid="publish-confirm-action-error">{actionFailure.summary}</p>
          {/if}
        </div>
      {/if}
      {#each [[missingLine(preview.missing), "missing"], [tooLargeLine(preview.tooLarge), "too-large"], [unpublishedLinksLine(preview.unpublishedLinks), "unpublished-links"], [withheldLine(preview.withheld), "withheld"]] as [line, id] (id)}
        {#if line}<p class="muted" data-testid={`publish-confirm-${id}`}>{line}</p>{/if}
      {/each}
    {/if}

    <div class="buttons">
      <button class="btn" onclick={oncancel} data-testid="publish-confirm-cancel">Cancel</button>
      <button
        bind:this={go}
        class="btn primary"
        onclick={onpublish}
        disabled={!ready}
        data-testid="publish-confirm-go"
      >Publish</button>
    </div>
  </div>
</div>

<style>
  .scrim {
    position: fixed;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    /* --bg-primary: background-based translucent overlay, as the palette overlay does */
    background: color-mix(in srgb, var(--bg-primary) 60%, transparent);
    z-index: 50;
  }
  .scrim:focus {
    outline: none;
  }
  .dialog {
    width: min(30rem, 92vw);
    display: flex;
    flex-direction: column;
    gap: var(--sp-3);
    padding: var(--sp-5) var(--sp-6);
    border: 1px solid var(--border);
    border-radius: var(--radius-m);
    background: var(--bg-primary);
    box-shadow: var(--shadow-m);
  }
  .title {
    margin: 0;
    font-size: var(--font-size-ui);
    font-weight: var(--font-weight-semibold);
    color: var(--text-normal);
  }
  .summary {
    margin: 0;
    font-size: var(--font-size-ui);
    color: var(--text-normal);
    overflow-wrap: anywhere;
  }
  .notice {
    display: flex;
    flex-direction: column;
    gap: var(--sp-1);
    padding: var(--sp-2);
    border-radius: var(--radius-s);
    background: var(--bg-secondary);
    font-size: var(--font-size-smaller);
    color: var(--text-normal);
  }
  .notice p {
    margin: 0;
  }
  .names {
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
  .muted {
    margin: 0;
    font-size: var(--font-size-smaller);
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
  .with-action {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--sp-1);
  }
  .btn.small {
    padding: var(--sp-1) var(--sp-3);
    font-size: var(--font-size-smaller);
  }
  .failure {
    margin: 0;
    padding: var(--sp-2);
    border-radius: var(--radius-s);
    background: var(--bg-secondary-alt);
    color: var(--text-error);
    font-size: var(--font-size-smaller);
  }
  .buttons {
    display: flex;
    justify-content: flex-end;
    gap: var(--sp-2);
  }
  .btn {
    padding: var(--sp-2) var(--sp-4);
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    background: var(--bg-secondary);
    color: var(--text-normal);
    font-size: var(--font-size-ui);
    font-family: inherit;
    cursor: pointer;
  }
  .btn:hover:not(:disabled) {
    background: var(--bg-secondary-alt);
  }
  .btn:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--bg-primary);
  }
</style>
