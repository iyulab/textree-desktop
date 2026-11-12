<script module lang="ts">
  // Drag-and-drop transfer key within the tree (single source). App-specific MIME distinct from OS file drops.
  export const DRAG_MIME = "application/x-textree-path";

  // Path of the node highlighted as drop target. A single state shared across all recursive instances —
  // there is always only one highlight during a drag, and it is reliably cleared by any instance's dragend.
  let dragOverPath = $state<string | null>(null);
</script>

<script lang="ts">
  import type { TreeNode } from "./ipc";
  import type { FriendlyError } from "./friendlyError.helpers";
  import { tree } from "./tree.svelte";
  import Icon from "./Icon.svelte";
  import type { FolderState } from "./folderState.svelte";
  import { mergeOrder } from "./nav.svelte";
  import Self from "./TreeView.svelte";

  let {
    folder,
    nodes,
    parentPath,
    onselect,
    onmove,
    onadopt,
    onrename,
    ondelete,
    onfavorite,
    oncommitrename,
    oncancelrename,
    editingPath = null,
    selectedPath = null,
    top = false,
  }: {
    /** The notes folder these nodes belong to: its order, favorites, publish choices, collapsed folders. */
    folder: FolderState;
    nodes: TreeNode[];
    /** Parent path of this node list (manual order key). The top level is the vault root. */
    parentPath: string;
    onselect: (node: TreeNode) => void;
    /** Move node (between folders). Moves srcPath into destDir (container directory). */
    onmove: (srcPath: string, destDir: string) => void;
    /** Drop onto a leaf: promote leafPath to a container and move srcPath as its child. */
    onadopt: (srcPath: string, leafPath: string) => void;
    /** Keyboard F2: rename the node. */
    onrename: (node: TreeNode) => void;
    /** Keyboard Delete: delete the node. */
    ondelete: (node: TreeNode) => void;
    /** Toggle the node's favorite state (star affordance in the row). */
    onfavorite: (node: TreeNode) => void;
    /** Commit an inline rename. Returns null on success, FriendlyError on failure (input stays open). */
    oncommitrename: (node: TreeNode, name: string) => Promise<FriendlyError | null>;
    /** Cancel the inline rename (Escape). */
    oncancelrename: () => void;
    /** Path of the node currently being inline-renamed (null = none). */
    editingPath?: string | null;
    /** Currently selected node path (for highlighting). Propagated to all descendants. */
    selectedPath?: string | null;
    /** Whether this is the top-level instance (role=tree vs group, fallback focus). */
    top?: boolean;
  } = $props();

  /** Derived array of sibling nodes ordered by nav.order[parentPath]. Original order if not registered in order. */
  let ordered = $derived(mergeOrder(nodes, folder.nav.order[parentPath] ?? [], (n) => n.path));

  function onDragStart(e: DragEvent, node: TreeNode) {
    if (!e.dataTransfer) return;
    e.dataTransfer.setData(DRAG_MIME, node.path);
    e.dataTransfer.effectAllowed = "move";
  }

  function onDragOver(e: DragEvent, _node: TreeNode) {
    // Container = move between folders, leaf = adopt (child after promotion). Both are drop targets.
    // preventDefault is required to allow drop. stopPropagation makes only the innermost node react.
    e.preventDefault();
    e.stopPropagation();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
    dragOverPath = _node.path;
  }

  function onDragLeave(node: TreeNode) {
    if (dragOverPath === node.path) dragOverPath = null;
  }

  function onDrop(e: DragEvent, node: TreeNode) {
    e.preventDefault();
    e.stopPropagation();
    dragOverPath = null;
    const src = e.dataTransfer?.getData(DRAG_MIME);
    if (!src) return;
    if (node.kind === "container") onmove(src, node.path); // move into container directory
    else onadopt(src, node.path); // leaf → child after promotion
  }

  // ── Keyboard navigation (WAI-ARIA tree) ──────────────────────────
  // Movement is handled by actual DOM document order (collapsed nodes are not rendered), so no flat model is needed.
  function items(from: HTMLElement): HTMLElement[] {
    const root = from.closest('[role="tree"]');
    if (!root) return [];
    return [...root.querySelectorAll<HTMLElement>('[role="treeitem"]')];
  }

  function focusItem(el: HTMLElement | null | undefined, path?: string) {
    if (!el) return;
    el.focus();
    if (path) tree.setFocused(path);
  }

  function siblingItem(el: HTMLElement, delta: number) {
    const all = items(el);
    const i = all.indexOf(el);
    return i >= 0 ? all[i + delta] : undefined;
  }

  function firstChildItem(el: HTMLElement): HTMLElement | null {
    const li = el.closest("li");
    return (
      li?.querySelector<HTMLElement>(
        ':scope > ul[role="group"] > li > .row > [role="treeitem"]',
      ) ?? null
    );
  }

  function parentItem(el: HTMLElement): HTMLElement | null {
    const ul = el.closest('ul[role="group"]');
    const parentLi = ul?.parentElement; // parent of the group ul = parent li
    return (
      parentLi?.querySelector<HTMLElement>(
        ':scope > .row > [role="treeitem"]',
      ) ?? null
    );
  }

  function onKeydown(e: KeyboardEvent, node: TreeNode) {
    const el = e.currentTarget as HTMLElement;
    const isContainer = node.kind === "container" && node.children.length > 0;
    const open = isContainer && !folder.folds.isCollapsed(node.path);

    // Keyboard move: Ctrl+X cut → Ctrl+V at target (container = move, leaf = adopt).
    // Accessibility path equivalent to DnD (WCAG 2.1.1).
    if (e.ctrlKey && (e.key === "x" || e.key === "X")) {
      e.preventDefault();
      tree.setCut(node.path);
      return;
    }
    if (e.ctrlKey && (e.key === "v" || e.key === "V")) {
      e.preventDefault();
      const src = tree.cut;
      if (!src) return;
      tree.setCut(null);
      if (node.kind === "container") onmove(src, node.path);
      else onadopt(src, node.path);
      return;
    }

    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        focusItem(siblingItem(el, 1));
        break;
      case "ArrowUp":
        e.preventDefault();
        focusItem(siblingItem(el, -1));
        break;
      case "ArrowRight":
        e.preventDefault();
        if (isContainer && !open) folder.folds.expand(node.path);
        else if (open) focusItem(firstChildItem(el));
        break;
      case "ArrowLeft":
        e.preventDefault();
        if (open) folder.folds.collapse(node.path);
        else focusItem(parentItem(el));
        break;
      case "Home":
        e.preventDefault();
        focusItem(items(el)[0]);
        break;
      case "End": {
        e.preventDefault();
        const all = items(el);
        focusItem(all[all.length - 1]);
        break;
      }
      case "Enter":
      case " ":
        e.preventDefault();
        onselect(node);
        break;
      case "F2":
        e.preventDefault();
        onrename(node);
        break;
      case "Delete":
        e.preventDefault();
        ondelete(node);
        break;
      case "Escape":
        if (tree.cut) {
          e.preventDefault();
          tree.setCut(null);
        }
        break;
    }
  }

  /** roving tabindex: only the focused node is 0; if unset, the top-level first item is 0. */
  function tabFor(path: string, index: number): number {
    if (tree.focused === path) return 0;
    if (tree.focused === null && top && index === 0) return 0;
    return -1;
  }

  // ── Inline rename (T2) ──────────────────────────────────────────
  // Only ever one node is edited globally; the single instance rendering that node owns the
  // input + error. editError clears whenever the edited node changes.
  let editError = $state<FriendlyError | null>(null);
  // Suppress the commit fired by the blur that follows an Escape cancel.
  let suppressRenameBlur = false;

  $effect(() => {
    editingPath; // re-run when the edited node changes
    editError = null;
  });

  /** Focus + select-all on the inline rename input; seed its initial value.
   * Reset the blur guard on every mount: the blur-away success path arms it without a
   * following unmount-blur to consume it (the input was already unfocused), so a fresh
   * session must start clean or it would silently swallow its first commit. */
  function initRename(el: HTMLInputElement, name: string) {
    suppressRenameBlur = false;
    el.value = name;
    el.focus();
    el.select();
  }

  async function commitRename(node: TreeNode, el: HTMLInputElement) {
    if (suppressRenameBlur) {
      suppressRenameBlur = false;
      return;
    }
    const err = await oncommitrename(node, el.value);
    // On success the parent clears editingPath → the input unmounts and fires a blur; arm the
    // guard so that unmount-blur does not re-commit (the Escape path arms it the same way).
    // On failure the input stays open, so no unmount-blur fires and the guard stays unset.
    if (err === null) suppressRenameBlur = true;
    editError = err;
  }

  function onRenameKey(e: KeyboardEvent, node: TreeNode) {
    e.stopPropagation(); // don't bubble to tree keyboard navigation
    if (e.key === "Enter") {
      e.preventDefault();
      commitRename(node, e.currentTarget as HTMLInputElement);
    } else if (e.key === "Escape") {
      e.preventDefault();
      suppressRenameBlur = true; // the unmount blur must not commit
      editError = null;
      oncancelrename();
    }
  }
</script>

<ul class="tree" role={top ? "tree" : "group"}>
  <!-- Keyed by path: an unkeyed list reuses rows by position, so a note appearing above the focused
       row would turn that focused element into a different note — and the next key (Delete, F2,
       Enter) would act on that note instead of the one the person had focused. -->
  {#each ordered as node, i (node.path)}
    {@const hasChildren = node.children.length > 0}
    {@const open = !folder.folds.isCollapsed(node.path)}
    {@const isContainer = node.kind === "container" && hasChildren}
    {@const fav = folder.nav.isFavorite(node.path)}
    {@const published = folder.publish.isIncluded(node.path)}
    <li>
      <div
        class="row"
        class:selected={node.path === selectedPath}
        class:drop-over={node.path === dragOverPath}
        class:drop-leaf={node.path === dragOverPath && node.kind === "leaf"}
        class:cut={node.path === tree.cut}
      >
        {#if isContainer}
          <button
            class="chevron"
            class:open
            onclick={(e) => {
              e.stopPropagation();
              folder.folds.toggle(node.path);
            }}
            aria-label={open ? "Collapse" : "Expand"}
            tabindex="-1"
          ><Icon name="chevron-right" size={14} /></button>
        {:else}
          <span class="chevron-spacer"></span>
        {/if}
        {#if editingPath === node.path}
          <span class="icon"><Icon name={node.kind === "container" ? "folder" : "file-text"} size={15} /></span>
          <input
            class="tree-rename-input"
            use:initRename={node.name}
            onkeydown={(e) => onRenameKey(e, node)}
            onblur={(e) => commitRename(node, e.currentTarget)}
            aria-label="New name"
          />
        {:else}
          <button
            class="node"
            class:no-body={!node.body_path}
            role="treeitem"
            aria-selected={node.path === selectedPath}
            aria-expanded={isContainer ? open : undefined}
            tabindex={tabFor(node.path, i)}
            draggable="true"
            ondragstart={(e) => onDragStart(e, node)}
            ondragend={() => (dragOverPath = null)}
            ondragover={(e) => onDragOver(e, node)}
            ondragleave={() => onDragLeave(node)}
            ondrop={(e) => onDrop(e, node)}
            onclick={() => onselect(node)}
            onfocus={() => tree.setFocused(node.path)}
            onkeydown={(e) => onKeydown(e, node)}
          >
            <span class="icon"><Icon name={node.kind === "container" ? "folder" : "file-text"} size={15} /></span>
            <span class="label">{node.name}</span>
          </button>
        {/if}
        {#if published}
          <span
            class="published"
            role="img"
            title="Included in the published site"
            aria-label="Included in the published site"
            data-testid="tree-published"
            ><Icon name="globe" size={13} /></span>
        {/if}
        <button
          class="fav"
          class:is-fav={fav}
          onclick={(e) => {
            e.stopPropagation();
            onfavorite(node);
          }}
          tabindex="-1"
          aria-label={fav ? "Remove from favorites" : "Add to favorites"}
          title={fav ? "Remove from favorites" : "Add to favorites"}
          aria-pressed={fav}
        ><Icon name="star" size={14} /></button>
      </div>
      {#if editingPath === node.path && editError}
        <p
          class="tree-rename-error"
          title={editError.raw !== editError.summary ? editError.raw : undefined}
        >⚠ {editError.summary}</p>
      {/if}
      {#if hasChildren && open}
        <Self
          {folder}
          nodes={node.children}
          parentPath={node.path}
          {onselect}
          {onmove}
          {onadopt}
          {onrename}
          {ondelete}
          {onfavorite}
          {oncommitrename}
          {oncancelrename}
          {editingPath}
          {selectedPath}
        />
      {/if}
    </li>
  {/each}
</ul>

<style>
  .tree {
    list-style: none;
    margin: 0;
    padding-left: var(--sp-3);
  }
  /* Node row: chevron + node button. Selection/drop highlight covers the whole row. */
  .row {
    display: flex;
    align-items: center;
    border-radius: var(--radius-s);
    transition: background var(--transition-fast);
  }
  .row:hover {
    background: var(--bg-hover);
  }
  .row.selected {
    background: var(--selection-bg);
  }
  /* Drop target — container (move): solid border + accent background. */
  .row.drop-over {
    outline: 2px solid var(--drop-border);
    outline-offset: -2px;
    background: var(--drop-bg);
  }
  /* Drop target — leaf (adopt, child after promotion): dashed border to visually distinguish from "move". */
  .row.drop-leaf {
    outline-style: dashed;
  }
  /* Node "cut" via keyboard: dimmed + dashed border (indicates awaiting paste). */
  .row.cut {
    opacity: 0.5;
    outline: 1px dashed var(--border-strong);
    outline-offset: -1px;
  }
  .chevron {
    flex-shrink: 0;
    width: 16px;
    height: 20px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    font-size: 10px;
    line-height: 1;
    color: var(--text-faint);
    background: none;
    border: none;
    cursor: pointer;
    transition: transform var(--transition-fast);
  }
  .chevron.open {
    transform: rotate(90deg);
  }
  .chevron-spacer {
    flex-shrink: 0;
    width: 16px;
  }
  .node {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: var(--sp-1);
    background: none;
    border: none;
    cursor: pointer;
    text-align: left;
    padding: 3px var(--sp-1);
    font: inherit;
    font-size: var(--font-size-small);
    color: var(--text-normal);
    border-radius: var(--radius-s);
  }
  /* Keyboard focus ring (focus-visible so it doesn't appear on mouse click). */
  .node:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  .icon {
    flex-shrink: 0;
    display: inline-flex;
    align-items: center;
    /* Node glyphs recede a step from the label so the name reads first. */
    color: var(--text-muted);
  }
  .label {
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  /* Inline rename input — sits in the node's place; matches the node label's type scale. */
  .tree-rename-input {
    flex: 1;
    min-width: 0;
    font: inherit;
    font-size: var(--font-size-small);
    color: var(--text-normal);
    background: var(--bg-primary);
    border: 1px solid var(--accent);
    border-radius: var(--radius-s);
    padding: 2px var(--sp-1);
  }
  .tree-rename-input:focus {
    outline: none;
  }
  /* Inline rename error — sits directly under the edited row. */
  .tree-rename-error {
    margin: var(--sp-1) 0 var(--sp-1) var(--sp-5);
    font-size: var(--font-size-small);
    color: var(--text-error);
  }
  /* Container without a body: body can't be opened, but it can still be selected for structure editing */
  .no-body .label {
    color: var(--text-muted);
  }
  .published {
    display: inline-flex;
    align-items: center;
    color: var(--text-muted);
    padding: 0 var(--sp-1);
  }
  /* Favorite star — read indicator + toggle. Hidden until row hover/focus unless favorited. */
  .fav {
    flex-shrink: 0;
    width: 22px;
    height: 22px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    font-size: 0.85em;
    line-height: 1;
    background: none;
    border: none;
    cursor: pointer;
    color: var(--text-faint);
    opacity: 0;
    transition:
      opacity var(--transition-fast),
      color var(--transition-fast);
  }
  .row:hover .fav,
  .fav:focus-visible {
    opacity: 1;
  }
  .fav.is-fav {
    opacity: 1;
    color: var(--accent);
  }
  /* Favorited: fill the star (CSS overrides the svg's fill="none" presentation attribute). */
  .fav.is-fav :global(svg) {
    fill: currentColor;
  }
  .fav:hover {
    color: var(--accent);
  }
  @media (prefers-reduced-motion: reduce) {
    .fav {
      transition: none;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .chevron {
      transition: none;
    }
  }
</style>
