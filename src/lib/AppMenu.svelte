<script lang="ts">
  /*
   * ⋮ app menu — a categorized dropdown surfacing the same commands as the Ctrl+P
   * palette (single source of truth: commands.ts). Discoverable by mouse without a
   * persistent menu bar (Chrome on demand). Grouping is delegated to the pure
   * groupCommands helper; this component only owns open/close and rendering.
   */
  import Icon from "./Icon.svelte";
  import { groupCommands } from "./appMenu.helpers";
  import { formatKeybinding } from "./keybinding.helpers";
  import { onMac } from "./platform";
  import type { Command } from "./commands";

  let {
    commands,
    onRunCommand,
    onOpenPalette,
  }: {
    commands: Command[];
    onRunCommand: (cmd: Command) => void;
    onOpenPalette: () => void;
  } = $props();

  let open = $state(false);
  let groups = $derived(groupCommands(commands));

  function run(cmd: Command): void {
    open = false;
    onRunCommand(cmd);
  }

  function searchAll(): void {
    open = false;
    onOpenPalette();
  }

  // Escape closes the menu while open (outside-click is handled by the backdrop).
  function onWindowKey(e: KeyboardEvent): void {
    if (open && e.key === "Escape") {
      e.preventDefault();
      open = false;
    }
  }
</script>

<svelte:window onkeydown={onWindowKey} />

<div class="app-menu">
  <button
    class="trigger"
    data-testid="app-menu-button"
    aria-haspopup="menu"
    aria-expanded={open}
    aria-label="Menu"
    title="Menu"
    onclick={() => (open = !open)}
  ><Icon name="more-vertical" /></button>

  {#if open}
    <div class="backdrop" role="presentation" onclick={() => (open = false)} onkeydown={() => {}}></div>
    <div class="menu" role="menu" data-testid="app-menu">
      {#each groups as g (g.category)}
        <div class="category" data-testid="app-menu-category">{g.category}</div>
        {#each g.items as cmd (cmd.id)}
          <button class="item" role="menuitem" data-testid="app-menu-item" onclick={() => run(cmd)}>
            <span class="item-title">{cmd.title}</span>
            {#if cmd.keybinding}
              <kbd class="kbd">{formatKeybinding(cmd.keybinding, onMac)}</kbd>
            {/if}
          </button>
        {/each}
      {/each}
      <div class="sep" role="separator"></div>
      <button
        class="item search-all"
        role="menuitem"
        data-testid="app-menu-search-all"
        onclick={searchAll}
      >
        <span class="item-title">Search all…</span>
        <kbd class="kbd">{formatKeybinding("mod+p", onMac)}</kbd>
      </button>
    </div>
  {/if}
</div>

<style>
  .app-menu {
    position: relative;
    flex-shrink: 0;
  }
  /* Trigger mirrors the sidebar-head .icon-btn treatment. */
  .trigger {
    width: 26px;
    height: 26px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: var(--text-muted);
    background: none;
    border: none;
    border-radius: var(--radius-s);
    cursor: pointer;
    transition:
      background var(--transition-fast),
      color var(--transition-fast);
  }
  .trigger:hover {
    background: var(--bg-hover);
    color: var(--text-normal);
  }
  .trigger:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  /* Full-viewport catcher: any outside click closes the menu. */
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
    min-width: 15rem;
    max-height: 70vh;
    overflow-y: auto;
    padding: var(--sp-1);
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: var(--radius-m);
    box-shadow: var(--shadow-m);
  }
  .category {
    padding: var(--sp-1) var(--sp-2) 2px;
    color: var(--text-muted);
    font-size: var(--font-size-smaller);
    font-weight: var(--font-weight-semibold);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .item {
    width: 100%;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--sp-2);
    padding: var(--sp-1) var(--sp-2);
    font: inherit;
    text-align: left;
    color: var(--text-normal);
    background: none;
    border: none;
    border-radius: var(--radius-s);
    cursor: pointer;
  }
  .item:hover {
    background: var(--selection-bg);
  }
  .item:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  .item-title {
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .kbd {
    flex-shrink: 0;
    padding: 0.05rem 0.35rem;
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    color: var(--text-muted);
    font-size: 0.72rem;
    font-family: inherit;
    white-space: nowrap;
  }
  .sep {
    height: 1px;
    margin: var(--sp-1) var(--sp-2);
    background: var(--border);
  }
  .search-all .item-title {
    color: var(--text-muted);
  }
</style>
