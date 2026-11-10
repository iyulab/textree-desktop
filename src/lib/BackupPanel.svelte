<script lang="ts">
  import type { RootId } from "./ipc";
  import { backupStore } from "./remoteSync.svelte";
  import { relativeTime, repositoryLabel } from "./remoteSync.helpers";
  import { friendlyError, type FriendlyError } from "./friendlyError.helpers";

  interface Props {
    /** Absolute folder root the panel is about. */
    root: RootId;
    /** Shown as Cancel / Close when the panel sits in its own dialog. */
    onclose?: () => void;
    /** Put the cursor in the first field when there is one. */
    autofocus?: boolean;
  }
  let { root, onclose, autofocus = false }: Props = $props();

  const ADDRESS_HELP =
    "The web address of a GitHub or GitLab repository, e.g. https://github.com/you/notes.git — a private one keeps your notes to you and the people you share it with.";
  const TOKEN_HELP =
    "A token the repository site gives you, with permission to read and write that repository.";

  let address = $state("");
  let token = $state("");
  let busy = $state(false);
  let failure = $state<FriendlyError | null>(null);
  let confirmingDisconnect = $state(false);
  let addressField = $state<HTMLInputElement | null>(null);

  // What this panel shows is about the open folder only.
  const ready = $derived(backupStore.known && backupStore.root === root);
  const connection = $derived(ready ? backupStore.connection : null);
  const session = $derived(backupStore.session);

  // Keeps "Last synced 3 minutes ago" true while the panel stays open.
  let now = $state(Date.now());
  $effect(() => {
    const id = setInterval(() => (now = Date.now()), 30_000);
    return () => clearInterval(id);
  });

  $effect(() => {
    if (autofocus && ready && !connection) addressField?.focus();
  });

  async function connect(event: SubmitEvent) {
    event.preventDefault();
    if (busy || address.trim() === "" || token === "") return;
    busy = true;
    failure = null;
    try {
      await backupStore.connect(root, address.trim(), token);
      token = "";
      address = "";
    } catch (e) {
      failure = friendlyError(e);
    } finally {
      busy = false;
    }
  }

  async function syncNow() {
    now = Date.now();
    await backupStore.sync(root, "manual");
    now = Date.now();
  }

  async function disconnect() {
    busy = true;
    failure = null;
    try {
      await backupStore.disconnect(root);
      confirmingDisconnect = false;
    } catch (e) {
      failure = friendlyError(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="backup-panel" data-testid="backup-panel">
  {#if !ready}
    <p class="muted">Checking…</p>
  {:else if connection}
    <p class="where" data-testid="backup-where">
      Backed up to <strong title={connection.url}>{repositoryLabel(connection.url)}</strong>
    </p>
    <p class="muted" data-testid="backup-last" role="status">
      {#if session.running}
        Syncing…
      {:else if session.syncedAt !== null}
        Last synced {relativeTime(session.syncedAt, now)}
      {:else}
        Not synced yet this session
      {/if}
    </p>
    {#if session.outcome && !session.running}
      <p class="outcome" data-testid="backup-outcome">{session.outcome}</p>
    {/if}
    <!-- One line: what the person just did failing says more than an earlier exchange failing. -->
    {#if failure ?? session.failure}
      {@const shown = failure ?? session.failure}
      <p class="failure" data-testid="backup-error" title={shown?.raw}>{shown?.summary}</p>
    {/if}

    {#if confirmingDisconnect}
      <div class="confirm" role="group" aria-label="Stop backing up">
        <p>
          Stop backing up this folder from this computer? What the repository already holds stays
          there.
        </p>
        <div class="buttons">
          <button class="btn" onclick={() => (confirmingDisconnect = false)} disabled={busy}
          >Keep backing up</button>
          <button
            class="btn danger"
            onclick={disconnect}
            disabled={busy}
            data-testid="backup-disconnect-confirm"
          >Disconnect</button>
        </div>
      </div>
    {:else}
      <div class="buttons">
        <button
          class="btn"
          onclick={() => (confirmingDisconnect = true)}
          disabled={busy}
          data-testid="backup-disconnect"
        >Disconnect</button>
        {#if onclose}
          <button class="btn" onclick={onclose}>Close</button>
        {/if}
        <button
          class="btn primary"
          onclick={syncNow}
          disabled={busy || session.running}
          data-testid="backup-sync-now"
        >Sync now</button>
      </div>
    {/if}
  {:else}
    {#if backupStore.lookupError}
      <p class="failure" title={backupStore.lookupError.raw}>{backupStore.lookupError.summary}</p>
    {/if}
    <form class="fields" onsubmit={connect}>
      <label class="field">
        <span class="label" title={ADDRESS_HELP}>Repository address</span>
        <input
          bind:this={addressField}
          bind:value={address}
          type="text"
          inputmode="url"
          placeholder="https://github.com/you/notes.git"
          title={ADDRESS_HELP}
          spellcheck="false"
          autocomplete="off"
          disabled={busy}
          data-testid="backup-url"
        />
        <span class="help">{ADDRESS_HELP}</span>
      </label>
      <label class="field">
        <span class="label" title={TOKEN_HELP}>Access token</span>
        <input
          bind:value={token}
          type="password"
          title={TOKEN_HELP}
          autocomplete="off"
          disabled={busy}
          data-testid="backup-token"
        />
        <span class="help">{TOKEN_HELP}</span>
      </label>

      {#if failure}
        <p class="failure" data-testid="backup-error" title={failure.raw}>{failure.summary}</p>
      {/if}

      <div class="buttons">
        {#if onclose}
          <button type="button" class="btn" onclick={onclose} disabled={busy}>Cancel</button>
        {/if}
        <button
          type="submit"
          class="btn primary"
          disabled={busy || address.trim() === "" || token === ""}
          data-testid="backup-connect"
        >{busy ? "Connecting…" : "Connect"}</button>
      </div>
    </form>
  {/if}
</div>

<style>
  .backup-panel {
    display: flex;
    flex-direction: column;
    gap: var(--sp-3);
  }
  .fields {
    display: flex;
    flex-direction: column;
    gap: var(--sp-3);
    margin: 0;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: var(--sp-1);
  }
  .label {
    font-size: var(--font-size-ui);
    color: var(--text-normal);
  }
  .field input {
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
  .help,
  .muted {
    margin: 0;
    font-size: var(--font-size-smaller);
    color: var(--text-muted);
  }
  .where,
  .outcome,
  .confirm p {
    margin: 0;
    font-size: var(--font-size-ui);
    color: var(--text-normal);
    overflow-wrap: anywhere;
  }
  .failure {
    margin: 0;
    padding: var(--sp-2);
    border-radius: var(--radius-s);
    background: var(--bg-secondary-alt);
    color: var(--text-error);
    font-size: var(--font-size-smaller);
  }
  .confirm {
    display: flex;
    flex-direction: column;
    gap: var(--sp-2);
  }
  .buttons {
    display: flex;
    justify-content: flex-end;
    flex-wrap: wrap;
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
  .btn.danger {
    color: var(--text-error);
  }
</style>
