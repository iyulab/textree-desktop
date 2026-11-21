<script lang="ts">
  /*
   * ⋮ menu of one notes folder in the sidebar: name it, move it up or down, show it in the
   * system's file manager, use it with AI or not, take it out of the sidebar. The folder and its notes are never
   * touched by any of these. With one folder the sidebar shows no names or order and the window always works in a
   * folder, so naming, moving and removing are not given — their items are left out.
   */
  import Icon from "./Icon.svelte";
  import { fileManagerAction } from "./workspace.helpers";

  let {
    name,
    canMoveUp = false,
    canMoveDown = false,
    onrename,
    onmove,
    onreveal,
    onremove,
    usedWithAi,
    onsetai,
  }: {
    /** The folder's name in the sidebar, for the menu's label. */
    name: string;
    canMoveUp?: boolean;
    canMoveDown?: boolean;
    /** Left out with one folder in the sidebar: rename, move and remove have no meaning then. */
    onrename?: () => void;
    /** −1 up, +1 down. */
    onmove?: (delta: number) => void;
    /** Show the folder in the system's file manager. */
    onreveal: () => void;
    onremove?: () => void;
    /** Whether the folder is used with AI (indexed for it, searched by meaning, asked about). */
    usedWithAi: boolean;
    onsetai: (ai: boolean) => void;
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
      {#if onrename}
        {@const rename = onrename}
        <button class="item" role="menuitem" onclick={() => choose(rename)}>Rename in sidebar…</button>
      {/if}
      {#if onmove}
        {@const move = onmove}
        <button class="item" role="menuitem" disabled={!canMoveUp} onclick={() => choose(() => move(-1))}
          >Move up</button>
        <button class="item" role="menuitem" disabled={!canMoveDown} onclick={() => choose(() => move(1))}
          >Move down</button>
      {/if}
      <button class="item" role="menuitem" onclick={() => choose(onreveal)}>{revealLabel}</button>
      <div class="sep" role="separator"></div>
      <button
        class="item"
        role="menuitemcheckbox"
        aria-checked={usedWithAi}
        data-testid="root-use-with-ai"
        title={usedWithAi
          ? "Its notes are indexed for AI, searched by meaning and can be asked about."
          : "Its notes are kept from AI: not indexed for it, not searched by meaning, not asked about."}
        onclick={() => choose(() => onsetai(!usedWithAi))}
      ><span class="check" aria-hidden="true">{usedWithAi ? "✓" : ""}</span>Use with AI</button>
      {#if onremove}
        {@const remove = onremove}
        <div class="sep" role="separator"></div>
        <button
          class="item"
          role="menuitem"
          title="The folder and its notes stay where they are."
          onclick={() => choose(remove)}>Remove from sidebar</button>
      {/if}
    </div>
  {/if}
</div>

<style>
  /* The tick of a menu item that is on or off (Use with AI), in a fixed column. */
  .check {
    display: inline-block;
    width: 1.1em;
    color: var(--text-normal);
  }
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
