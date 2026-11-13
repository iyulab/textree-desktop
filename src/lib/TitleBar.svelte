<script lang="ts">
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import Icon from "./Icon.svelte";
  import { formatKeybinding } from "./keybinding.helpers";
  import { onMac } from "./platform";

  let {
    label,
    collapsed,
    onToggleSidebar,
    onOpenPalette,
  }: {
    label: string;
    collapsed: boolean;
    onToggleSidebar: () => void;
    onOpenPalette: () => void;
  } = $props();

  // The maximize/restore glyph tracks the window's maximized state. Refreshed on mount and
  // whenever a control is used; also cheap to re-read on the resize event Tauri surfaces.
  let maximized = $state(false);
  const appWindow = getCurrentWindow();

  async function refreshMaximized() {
    try {
      maximized = await appWindow.isMaximized();
    } catch {
      /* non-fatal — glyph just stays as-is */
    }
  }

  $effect(() => {
    void refreshMaximized();
    // Keep the glyph correct when the user maximizes via drag-to-top or OS shortcuts.
    const un = appWindow.onResized(() => void refreshMaximized());
    return () => { void un.then((f) => f()); };
  });

  // Window controls are fire-and-forget: a rejected promise has no meaningful user recovery
  // (a failed minimize is a no-op), so swallow it rather than surfacing an error.
  function minimize() { void appWindow.minimize().catch(() => {}); }
  function toggleMax() { void appWindow.toggleMaximize().catch(() => {}).then(refreshMaximized); }
  function close() { void appWindow.close().catch(() => {}); }
</script>

<!-- data-tauri-drag-region="deep" makes the bar background AND its non-clickable descendants
     (the flex spacers and the brand span) window-move handles (double-click maximizes). "deep"
     is required because the flex children cover the bar; a bare attribute would only drag when
     the mousedown target is .titlebar itself. Real <button>s below still short-circuit and stay
     clickable. -->
<!-- On macOS the system draws the window controls at the top left, over this bar (`mac` leaves
     room for them), and the bar draws none of its own. -->
<div class="titlebar" class:mac={onMac} data-tauri-drag-region="deep">
  <div class="tb-left">
    <button
      class="tb-icon-btn"
      onclick={onToggleSidebar}
      title={collapsed ? "Show sidebar" : "Hide sidebar"}
      aria-label={collapsed ? "Show sidebar" : "Hide sidebar"}
    ><Icon name={collapsed ? "panel-left-open" : "panel-left-close"} /></button>
    <span class="tb-brand">
      <svg class="tb-mark" viewBox="8 8 48 48" aria-hidden="true">
        <path class="tb-mark-leaf" d="M12 52V26Q12 12 26 12H52V38Q52 52 38 52Z" />
        <path class="tb-mark-branch" d="M19 45L40 24M27 37V28M33 31H42" />
        <circle class="tb-mark-node" cx="42.5" cy="21.5" r="3" />
        <circle class="tb-mark-node" cx="27" cy="25" r="3" />
        <circle class="tb-mark-node" cx="45" cy="31" r="3" />
      </svg>
      Textree
    </span>
  </div>

  <button
    class="tb-center"
    onclick={onOpenPalette}
    title={`Search or run a command (${formatKeybinding("mod+p", onMac)})`}
  >
    <Icon name="search" />
    <span class="tb-context">{label || "Textree"}</span>
  </button>

  <div class="tb-right">
    {#if !onMac}
    <button class="tb-win-btn" onclick={minimize} title="Minimize" aria-label="Minimize"
      ><Icon name="minus" /></button>
    <button class="tb-win-btn" onclick={toggleMax}
      title={maximized ? "Restore" : "Maximize"} aria-label={maximized ? "Restore" : "Maximize"}
      ><Icon name={maximized ? "restore" : "square"} /></button>
    <button class="tb-win-btn tb-close" onclick={close} title="Close" aria-label="Close"
      ><Icon name="x" /></button>
    {/if}
  </div>
</div>

<style>
  .titlebar {
    display: flex;
    align-items: center;
    height: 36px;
    flex-shrink: 0;
    background: var(--bg-secondary);
    border-bottom: 1px solid var(--border);
    user-select: none;
    -webkit-user-select: none;
  }
  .tb-left,
  .tb-right {
    display: flex;
    align-items: center;
    gap: var(--sp-1);
    padding: 0 var(--sp-2);
  }
  .tb-left { flex: 1; }
  /* Room for the system's window controls (three buttons and their margins, ~68px) on macOS. */
  .titlebar.mac .tb-left { padding-left: 76px; }
  .tb-right { flex: 1; justify-content: flex-end; }
  .tb-brand {
    display: inline-flex;
    align-items: center;
    gap: var(--sp-1);
    font-size: var(--font-size-small);
    font-weight: 600;
    color: var(--text-normal);
  }
  /* The app-icon mark (app-icon.svg), drawn from tokens so it stays in step with the icon. */
  .tb-mark { width: 16px; height: 16px; flex-shrink: 0; }
  .tb-mark-leaf { fill: var(--brand-leaf); }
  .tb-mark-branch { fill: none; stroke: var(--brand-on-leaf); stroke-width: 3.5; stroke-linecap: round; }
  .tb-mark-node { fill: var(--brand-on-leaf); }
  .tb-icon-btn,
  .tb-win-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 28px;
    color: var(--text-muted);
    background: transparent;
    border: none;
    border-radius: var(--radius-s);
    cursor: pointer;
  }
  .tb-icon-btn:hover,
  .tb-win-btn:hover { background: var(--bg-hover); color: var(--text-normal); }
  .tb-close:hover { background: var(--text-error); color: var(--text-on-accent); }
  .tb-center {
    display: inline-flex;
    align-items: center;
    gap: var(--sp-1);
    /* A search bar reads as a stable target, so give it a generous minimum width instead of
       shrinking to fit the label. Caps at 50% and never exceeds the viewport on a narrow window. */
    min-width: min(360px, 50%);
    max-width: 50%;
    padding: var(--sp-1) var(--sp-3);
    color: var(--text-muted);
    background: var(--bg-primary);
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    font: inherit;
    font-size: var(--font-size-smaller);
    cursor: pointer;
  }
  .tb-center:hover { background: var(--bg-hover); }
  .tb-context {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
