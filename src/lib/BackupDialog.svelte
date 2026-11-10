<script lang="ts">
  import type { RootId } from "./ipc";
  import BackupPanel from "./BackupPanel.svelte";

  interface Props {
    /** Absolute folder root to back up. */
    root: RootId;
    onclose: () => void;
  }
  let { root, onclose }: Props = $props();

  let dialog = $state<HTMLDivElement | null>(null);

  $effect(() => {
    // The panel moves focus into its first field when it has one; otherwise the dialog holds it,
    // so Escape still reaches it.
    if (dialog && !dialog.contains(document.activeElement)) dialog.focus();
  });

  // On the window, not the dialog: connecting replaces the form the focus was in, and Escape
  // still has to close the dialog afterwards.
  function onkeydown(event: KeyboardEvent) {
    if (event.key === "Escape" && !event.defaultPrevented) {
      event.preventDefault();
      onclose();
    }
  }
</script>

<svelte:window {onkeydown} />

<div
  class="scrim"
  role="dialog"
  aria-modal="true"
  aria-label="Back up your notes"
  tabindex="-1"
  bind:this={dialog}
  data-testid="backup-dialog"
>
  <div class="dialog">
    <h2 class="title">Back up your notes</h2>
    <BackupPanel {root} {onclose} autofocus />
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
    width: min(30rem, 92vw);
    max-height: 86vh;
    overflow-y: auto;
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
</style>
