<script lang="ts">
  /*
   * ⋮ menu of one notes folder in the sidebar: name it, move it up or down, show it in the
   * system's file manager, take it out of the sidebar. The folder and its notes are never touched by any of these.
   */
  import Icon from "./Icon.svelte";
  import { fileManagerAction } from "./workspace.helpers";

  let {
    name,
    canMoveUp,
    canMoveDown,
    onrename,
    onmove,
    onreveal,
    onremove,
  }: {
    /** The folder's name in the sidebar, for the menu's label. */
    name: string;
    canMoveUp: boolean;
    canMoveDown: boolean;
    onrename: () => void;
    /** −1 up, +1 down. */
    onmove: (delta: number) => void;
    /** Show the folder in the system's file manager. */
    onreveal: () => void;
    onremove: () => void;
  } = $props();

  let open = $state(false);
  const revealLabel = fileManagerAction(typeof navigator === "undefined" ? "" : navigator.platform);

  function choose(action: () => void): void {
    open = false;
    action();
  }

  function onWindowKey(e: KeyboardEvent): void {
    if (open && e.key === "Escape") {
      e.preventDefault();
      open = false;
    }
  }
</script>

<svelte:window onkeydown={onWindowKey} />

<div class="root-menu">
  <button
    class="trigger"
    aria-haspopup="menu"
    aria-expanded={open}
    aria-label={`Notes folder menu — ${name}`}
    title="Notes folder menu"
    onclick={() => (open = !open)}
  ><Icon name="more-vertical" size={14} /></button>

  {#if open}
    <div class="backdrop" role="presentation" onclick={() => (open = false)} onkeydown={() => {}}></div>
    <div class="menu" role="menu" aria-label={name}>
      <button class="item" role="menuitem" onclick={() => choose(onrename)}>Rename in sidebar…</button>
      <button class="item" role="menuitem" disabled={!canMoveUp} onclick={() => choose(() => onmove(-1))}
        >Move up</button>
      <button class="item" role="menuitem" disabled={!canMoveDown} onclick={() => choose(() => onmove(1))}
        >Move down</button>
      <button class="item" role="menuitem" onclick={() => choose(onreveal)}>{revealLabel}</button>
      <div class="sep" role="separator"></div>
      <button
        class="item"
        role="menuitem"
        title="The folder and its notes stay where they are."
        onclick={() => choose(onremove)}>Remove from sidebar</button>
    </div>
  {/if}
</div>

<style>
  .root-menu {
    position: relative;
    flex-shrink: 0;
  }
  .trigger {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: var(--sp-1);
    color: var(--text-muted);
    background: none;
    border: none;
    border-radius: var(--radius-s);
    cursor: pointer;
  }
  .trigger:hover {
    background: var(--bg-hover);
    color: var(--text-normal);
  }
  .trigger:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 40;
  }
  .menu {
    position: absolute;
    top: calc(100% + var(--sp-1));
    right: 0;
    z-index: 50;
    min-width: 12rem;
    padding: var(--sp-1);
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: var(--radius-m);
    box-shadow: var(--shadow-m);
  }
  .item {
    width: 100%;
    display: block;
    padding: var(--sp-1) var(--sp-2);
    font: inherit;
    text-align: left;
    color: var(--text-normal);
    background: none;
    border: none;
    border-radius: var(--radius-s);
    cursor: pointer;
  }
  .item:hover:not(:disabled) {
    background: var(--selection-bg);
  }
  .item:disabled {
    color: var(--text-muted);
    cursor: default;
  }
  .item:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  .sep {
    height: 1px;
    margin: var(--sp-1) var(--sp-2);
    background: var(--border);
  }
</style>
