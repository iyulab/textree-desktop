<script lang="ts">
  import { type FolderTable, type SortDir } from "./folderTable.helpers";
  import type { FolderViews } from "./views.svelte";
  import {
    applyView,
    VIEW_VERSION,
    type FilterCondition,
    type FilterOp,
    type ViewDefinition,
  } from "./view.helpers";

  let {
    table,
    folder,
    views,
    onOpen,
  }: {
    /** The folder's notes as a table model (columns = frontmatter keys, rows = notes). */
    table: FolderTable;
    /** The folder's path — the key under which named views are saved (views.json). */
    folder: string;
    /** The saved views of the notes folder this folder is in. */
    views: FolderViews;
    /** Open a row's note. */
    onOpen: (path: string) => void;
  } = $props();

  // Ephemeral view state. The component is keyed by folder path in the parent, so this resets when
  // switching folders. Applying a saved view loads its state here; editing diverges silently (the
  // chip highlight just marks the last-applied preset — "Save view" overwrites it).
  let filters = $state<FilterCondition[]>([]);
  // sortKey: `null` = the Name column, a string = a frontmatter field, `undefined` = unsorted.
  let sortKey = $state<string | null | undefined>(undefined);
  let sortDir = $state<SortDir>("asc");

  // Single render path: the ephemeral state becomes a ViewDefinition fed through applyView — the same
  // lens that a saved view will use, so ad-hoc and saved views never diverge into two code paths.
  const view = $derived<ViewDefinition>({
    version: VIEW_VERSION,
    name: "",
    folder: "",
    columns: null, // ad-hoc view keeps the dynamic column union
    sort: sortKey === undefined ? null : { key: sortKey, dir: sortDir },
    filters,
  });
  let shown = $derived(applyView(table, view));

  function toggleSort(key: string | null): void {
    if (sortKey === key) sortDir = sortDir === "asc" ? "desc" : "asc";
    else {
      sortKey = key;
      sortDir = "asc";
    }
  }

  const ariaSort = (key: string | null): "ascending" | "descending" | "none" =>
    sortKey === key ? (sortDir === "asc" ? "ascending" : "descending") : "none";
  const arrow = (key: string | null): string =>
    sortKey === key ? (sortDir === "asc" ? " ▲" : " ▼") : "";

  const OPS: { value: FilterOp; label: string }[] = [
    { value: "contains", label: "contains" },
    { value: "equals", label: "is" },
    { value: "exists", label: "exists" },
    { value: "missing", label: "missing" },
  ];
  const needsValue = (op: FilterOp): boolean => op === "contains" || op === "equals";

  function addFilter(): void {
    filters = [...filters, { field: table.columns[0] ?? "", op: "contains", value: "" }];
  }
  function removeFilter(i: number): void {
    filters = filters.filter((_, idx) => idx !== i);
  }
  function patchFilter(i: number, patch: Partial<FilterCondition>): void {
    filters = filters.map((f, idx) => (idx === i ? { ...f, ...patch } : f));
  }

  // ── Saved views (views.json, folder-keyed) ────────────────────────────────
  let savedViews = $derived(views.forFolder(folder));
  let activeView = $state<string | null>(null);
  let naming = $state(false);
  let draftName = $state("");

  function selectView(name: string): void {
    const def = savedViews.find((v) => v.name.trim() === name.trim());
    if (!def) return;
    filters = def.filters.map((f) => ({ ...f }));
    if (def.sort) {
      sortKey = def.sort.key;
      sortDir = def.sort.dir;
    } else {
      sortKey = undefined;
      sortDir = "asc";
    }
    activeView = def.name;
  }

  function clearView(): void {
    filters = [];
    sortKey = undefined;
    sortDir = "asc";
    activeView = null;
  }

  function startNaming(): void {
    draftName = activeView ?? "";
    naming = true;
  }
  function cancelNaming(): void {
    naming = false;
    draftName = "";
  }
  function confirmSave(): void {
    const name = draftName.trim();
    if (!name) return;
    const def: ViewDefinition = {
      version: VIEW_VERSION,
      name,
      folder,
      columns: null, // no column-projection UI yet; views capture filters + sort
      sort: sortKey === undefined ? null : { key: sortKey, dir: sortDir },
      filters: filters.map((f) => ({ ...f })),
    };
    void views.save(def);
    activeView = name;
    cancelNaming();
  }
  function deleteView(name: string): void {
    void views.remove(folder, name);
    if (activeView === name) activeView = null;
  }
</script>

{#if table.rows.length}
  <section class="folder-table" aria-label="Folder table">
    {#if table.columns.length}
      <div class="views-bar" aria-label="Saved views">
        {#if savedViews.length}
          <button
            class="view-chip"
            class:active={activeView === null}
            onclick={clearView}>All</button
          >
          {#each savedViews as v (v.name)}
            <span class="view-chip-group" class:active={activeView === v.name}>
              <button class="view-chip" onclick={() => selectView(v.name)}>{v.name}</button>
              <button
                class="view-del"
                aria-label={`Delete view ${v.name}`}
                onclick={() => deleteView(v.name)}>×</button
              >
            </span>
          {/each}
        {/if}
        {#if naming}
          <!-- svelte-ignore a11y_autofocus -->
          <input
            class="view-name"
            aria-label="View name"
            type="text"
            autofocus
            value={draftName}
            oninput={(e) => (draftName = e.currentTarget.value)}
            onkeydown={(e) => {
              if (e.key === "Enter") confirmSave();
              else if (e.key === "Escape") cancelNaming();
            }}
          />
          <button class="view-save" onclick={confirmSave}>Save</button>
          <button class="view-cancel" onclick={cancelNaming}>Cancel</button>
        {:else}
          <button class="view-add" onclick={startNaming}>Save view</button>
        {/if}
      </div>
      <div class="filter-bar">
        {#each filters as f, i (i)}
          <div class="filter">
            <select
              aria-label="Filter field"
              value={f.field}
              onchange={(e) => patchFilter(i, { field: e.currentTarget.value })}
            >
              {#each table.columns as col (col)}
                <option value={col}>{col}</option>
              {/each}
            </select>
            <select
              aria-label="Filter operator"
              value={f.op}
              onchange={(e) => patchFilter(i, { op: e.currentTarget.value as FilterOp })}
            >
              {#each OPS as op (op.value)}
                <option value={op.value}>{op.label}</option>
              {/each}
            </select>
            {#if needsValue(f.op)}
              <input
                aria-label="Filter value"
                type="text"
                value={f.value}
                oninput={(e) => patchFilter(i, { value: e.currentTarget.value })}
              />
            {/if}
            <button class="filter-remove" aria-label="Remove filter" onclick={() => removeFilter(i)}>
              ×
            </button>
          </div>
        {/each}
        <button class="filter-add" onclick={addFilter}>+ Filter</button>
        {#if filters.length}
          <span class="filter-count">{shown.rows.length} / {table.rows.length}</span>
        {/if}
      </div>
    {/if}
    <table>
      <thead>
        <tr>
          <th scope="col" aria-sort={ariaSort(null)}>
            <button class="sort" onclick={() => toggleSort(null)}>Name{arrow(null)}</button>
          </th>
          {#each shown.columns as col (col)}
            <th scope="col" aria-sort={ariaSort(col)}>
              <button class="sort" onclick={() => toggleSort(col)}>{col}{arrow(col)}</button>
            </th>
          {/each}
        </tr>
      </thead>
      <tbody>
        {#each shown.rows as row (row.path)}
          <tr>
            <td>
              <button class="row-open" onclick={() => onOpen(row.path)} title={row.path}>
                {row.name}
              </button>
            </td>
            {#each shown.columns as col (col)}
              <td>{row.fields[col] ?? ""}</td>
            {/each}
          </tr>
        {/each}
      </tbody>
    </table>
    {#if !shown.rows.length}
      <p class="empty">No rows match the filter.</p>
    {/if}
  </section>
{/if}

<style>
  .folder-table {
    overflow: auto;
    padding: var(--sp-4) var(--sp-6);
  }
  .views-bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--sp-2);
    margin-bottom: var(--sp-2);
  }
  .view-chip-group {
    display: inline-flex;
    align-items: center;
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
  }
  .view-chip-group.active {
    border-color: var(--accent);
  }
  .view-chip {
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    background: none;
    color: var(--text-muted);
    cursor: pointer;
    font: inherit;
    font-size: var(--font-size-small);
    padding: var(--sp-1) var(--sp-2);
  }
  .view-chip:hover {
    color: var(--text-normal);
  }
  .view-chip.active {
    color: var(--accent);
    border-color: var(--accent);
  }
  .view-chip-group .view-chip {
    border: none;
  }
  .view-chip-group.active .view-chip {
    color: var(--accent);
  }
  .view-del {
    border: none;
    background: none;
    color: var(--text-muted);
    cursor: pointer;
    font: inherit;
    line-height: 1;
    padding: 0 var(--sp-1);
  }
  .view-del:hover {
    color: var(--text-normal);
  }
  .view-add,
  .view-save,
  .view-cancel {
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    background: none;
    color: var(--text-muted);
    cursor: pointer;
    font: inherit;
    font-size: var(--font-size-small);
    padding: var(--sp-1) var(--sp-2);
  }
  .view-add:hover,
  .view-save:hover,
  .view-cancel:hover {
    color: var(--text-normal);
    border-color: var(--text-muted);
  }
  .view-name {
    border: none;
    border-bottom: 1px solid var(--border);
    background: none;
    color: var(--text-normal);
    font: inherit;
    font-size: var(--font-size-small);
    min-width: 8rem;
  }
  .filter-bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--sp-2);
    margin-bottom: var(--sp-3);
  }
  .filter {
    display: flex;
    align-items: center;
    gap: var(--sp-1);
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    padding: var(--sp-1) var(--sp-2);
  }
  .filter select,
  .filter input {
    border: none;
    background: none;
    color: var(--text-normal);
    font: inherit;
    font-size: var(--font-size-small);
  }
  .filter input {
    border-bottom: 1px solid var(--border);
    min-width: 6rem;
  }
  .filter-remove {
    border: none;
    background: none;
    color: var(--text-muted);
    cursor: pointer;
    font: inherit;
    line-height: 1;
    padding: 0 var(--sp-1);
  }
  .filter-remove:hover {
    color: var(--text-normal);
  }
  .filter-add {
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    background: none;
    color: var(--text-muted);
    cursor: pointer;
    font: inherit;
    font-size: var(--font-size-small);
    padding: var(--sp-1) var(--sp-2);
  }
  .filter-add:hover {
    color: var(--text-normal);
    border-color: var(--text-muted);
  }
  .filter-count {
    color: var(--text-muted);
    font-size: var(--font-size-small);
  }
  table {
    border-collapse: collapse;
    width: 100%;
    font-size: var(--font-size-small);
  }
  th,
  td {
    text-align: left;
    padding: var(--sp-2) var(--sp-3);
    border-bottom: 1px solid var(--border);
    vertical-align: top;
    white-space: nowrap;
  }
  td {
    color: var(--text-normal);
  }
  .sort {
    border: none;
    background: none;
    color: var(--text-muted);
    font: inherit;
    font-weight: var(--font-weight-bold);
    cursor: pointer;
    padding: 0;
  }
  .sort:hover {
    color: var(--text-normal);
  }
  .row-open {
    border: none;
    background: none;
    color: var(--accent);
    cursor: pointer;
    font: inherit;
    padding: 0;
  }
  .row-open:hover {
    text-decoration: underline;
  }
  .empty {
    color: var(--text-muted);
    font-size: var(--font-size-small);
    padding: var(--sp-2) var(--sp-3);
  }
</style>
