<script lang="ts">
  import type { RootId } from "./ipc";
  import { commitNotes } from "./ipc";
  import { friendlyError, type FriendlyError } from "./friendlyError.helpers";
  import { outcomeMessage, scopeSummary, versionName } from "./addVersion.helpers";

  interface Props {
    /** Absolute vault root path. */
    root: RootId;
    /** Absolute paths of the notes this version covers. */
    paths: string[];
    /** The note goes out when the folder is published. */
    published: boolean;
    /** Close the dialog. */
    onclose: () => void;
    /** Called after the attempt finished without error, so the caller can refresh. */
    ondone: (message: string) => void;
  }
  let { root, paths, published, onclose, ondone }: Props = $props();

  let typed = $state("");
  let busy = $state(false);
  let failure = $state<FriendlyError | null>(null);
  let field = $state<HTMLInputElement | null>(null);

  $effect(() => {
    field?.focus();
  });

  async function add() {
    if (busy || paths.length === 0) return;
    busy = true;
    failure = null;
    try {
      // Null means the notes already hold the recorded state and nothing was written. It is not
      // an error and it is not the same as having added one, so it gets its own sentence.
      const revision = await commitNotes(root, paths, versionName(typed, paths));
      ondone(outcomeMessage(revision, paths));
      onclose();
    } catch (e) {
      failure = friendlyError(e);
    } finally {
      busy = false;
    }
  }

  function onkeydown(event: KeyboardEvent) {
    if (event.key === "Escape") {
      event.preventDefault();
      onclose();
    } else if (event.key === "Enter") {
      event.preventDefault();
      void add();
    }
  }
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
  class="scrim"
  role="dialog"
  aria-modal="true"
  aria-label="Name this version"
  tabindex="-1"
  {onkeydown}
  data-testid="add-version"
>
  <div class="dialog">
    <h2 class="title">Name this version</h2>
    <input
      bind:this={field}
      bind:value={typed}
      class="name-field"
      type="text"
      placeholder="Optional"
      disabled={busy}
      data-testid="add-version-name"
    />
    <p class="scope" data-testid="add-version-scope">{scopeSummary(paths)}</p>
    {#if published}
      <p class="published-line" data-testid="add-version-published">Included in the published site: this version goes out the next time you publish.</p>
    {/if}

    {#if failure}
      <p class="failure" data-testid="add-version-error">{failure.summary}</p>
    {/if}

    <div class="buttons">
      <button class="btn" onclick={onclose} disabled={busy}>Cancel</button>
      <button
        class="btn primary"
        onclick={add}
        disabled={busy || paths.length === 0}
        data-testid="add-version-confirm"
      >Add</button>
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
  .dialog {
    width: min(28rem, 92vw);
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
  .name-field {
    width: 100%;
    box-sizing: border-box;
    padding: var(--sp-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    background: var(--bg-secondary);
    color: var(--text-normal);
    font-size: var(--font-size-ui);
    font-family: inherit;
  }
  .scope,
  .published-line {
    margin: 0;
    font-size: var(--font-size-smaller);
    color: var(--text-muted);
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
