<script lang="ts">
  import type { RootId } from "./ipc";
  import { palette } from "./paletteStore.svelte";
  import type { FolderNav } from "./nav.svelte";
  import { fuzzyMatch, type Match } from "./fuzzy";
  import { formatKeybinding } from "./keybinding.helpers";
  import { paletteListState, paletteStatusText, type ContentScope } from "./palette.helpers";
  import type { Command } from "./commands";
  import { semanticSearch, hostStatus, prepareAiModel, type FolderHit, type SemanticHit, type HostStatus } from "./ipc";
  import { resolveSemanticAiUi } from "./semanticAiUi.helpers";
  import { getAiConsent, setAiConsent } from "./aiConsent";
  import { getByoConfig } from "./byoConfig";
  import { aiHost } from "./aiHost.svelte";
  import { formatModelDownload } from "./modelDownload.helpers";
  import type { DownloadSnapshot } from "./modelDownload.helpers";

  /** A content hit; `folder` names its notes folder when the sidebar shows several. */
  type ContentHit = FolderHit & { folder?: string };

  interface FileEntry {
    name: string;
    path: string;
    kind: "leaf" | "container";
    /** The notes folder it is in; `folder` names it when the sidebar shows several. */
    root?: string;
    folder?: string;
  }

  let {
    files,
    commands,
    onOpenFile,
    onRunCommand,
    onSearchContent,
    vaultRoot = null,
    scopePath = null,
    nav = null,
    folderName = null,
  }: {
    files: FileEntry[];
    commands: Command[];
    /** `root`: the notes folder of a note listed from several (else the open folder). */
    onOpenFile: (path: string, root?: RootId) => void;
    onRunCommand: (cmd: Command) => void;
    onSearchContent: (query: string, scope: ContentScope) => Promise<ContentHit[]>;
    vaultRoot?: RootId | null;
    scopePath?: string | null;
    /** Favorites and recent notes of the open notes folder (null before one opens). */
    nav?: FolderNav | null;
    /** The open notes folder's name when the sidebar shows several — where semantic search looks, and content search when told to. */
    folderName?: string | null;
  } = $props();

  let fileMatches = $derived<Match<FileEntry>[]>(
    palette.mode !== "file"
      ? []
      : palette.term === ""
        ? // Empty query: surface favorites first, then recent (excluding ones already favorited).
          [
            ...(nav?.favorites ?? []),
            ...(nav?.recent ?? []).filter((p) => !nav?.favorites.includes(p)),
          ]
            .map((p) => files.find((f) => f.path === p && (f.root === undefined || f.root === vaultRoot)))
            .filter((f): f is FileEntry => f !== undefined)
            .map((item) => ({ item, score: 0, ranges: [] as [number, number][] }))
        : fuzzyMatch(palette.term, files, (f) => f.name),
  );
  let cmdMatches = $derived<Match<Command>[]>(
    palette.mode === "command" ? fuzzyMatch(palette.term, commands, (c) => c.title) : [],
  );
  // Content search uses the backend IPC (async) — debounce + apply only the last response (race prevention).
  let contentHits = $state<ContentHit[]>([]);
  let searchSeq = 0;
  // True while a content query is debouncing/in flight — lets the list distinguish "searching"
  // from a settled "no matches" (both would otherwise render zero rows).
  let searching = $state(false);

  $effect(() => {
    if (palette.mode !== "content") {
      contentHits = [];
      searching = false;
      return;
    }
    const term = palette.term;
    const scope = palette.contentScope;
    if (term === "") {
      contentHits = [];
      searching = false;
      return;
    }
    searching = true;
    const seq = ++searchSeq;
    const t = setTimeout(() => {
      void onSearchContent(term, scope)
        .then((hits) => {
          if (seq === searchSeq) {
            contentHits = hits; // apply only the latest request
            searching = false;
          }
        })
        .catch(() => {
          // On a search failure, clear the in-flight flag so the row doesn't stick on "Searching…".
          if (seq === searchSeq) searching = false;
        });
    }, 120);
    return () => clearTimeout(t);
  });

  // Semantic search — mirrors content search pattern; guarded by hostState and vaultRoot availability.
  let semanticHits = $state<SemanticHit[]>([]);
  let hostState = $state<HostStatus | null>(null);  // derived from the .status field of hostStatus() result
  let modelDownload = $state<DownloadSnapshot | null>(null); // embedder-first download snapshot during preparing
  let consent = $state(getAiConsent());

  // Live-poll hostStatus() while the AI badge is in the "preparing" state so the download
  // progress bar advances in real time (not frozen at first-sampled value).
  // Deps: palette.mode, consent — restarts when either changes, not on every keystroke.
  //
  // Grace-window: on a fresh Enable, Rust's resolve_host (filesystem work) runs before
  // try_begin_spawn sets host status to "Starting". The first poll(s) may therefore see
  // "unavailable" even though the host is mid-spawn. INITIAL_GRACE_POLLS lets the poll
  // keep rescheduling for the first ~3 cycles (~6 s) regardless of status so it rides out
  // that window. After the grace period, an always-unavailable host stops the poll (no
  // infinite loop: at most INITIAL_GRACE_POLLS extra cycles are wasted, then it stops).
  const INITIAL_GRACE_POLLS = 3;

  $effect(() => {
    if (palette.mode !== "semantic") {
      hostState = null;
      modelDownload = null;
      return;
    }
    // Read consent in the effect body so the poll auto-restarts when enableAi() flips it.
    const _consent = consent;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    // Per-run counter — resets on every effect re-run (fresh consent/mode change gets its
    // own grace window). Declared here, NOT inside poll(), so it accumulates across cycles.
    let attempts = 0;

    function poll() {
      void hostStatus()
        .then((s) => {
          if (cancelled) return;
          // Increment unconditionally so the grace bound is exact (3 polls max).
          attempts += 1;
          hostState = s.status;
          modelDownload = s.embedderDownload ?? s.generatorDownload ?? null;
          // Reschedule while preparing OR still within the initial grace window.
          if (resolveSemanticAiUi(_consent, s.status) === "preparing" || attempts < INITIAL_GRACE_POLLS) {
            timer = setTimeout(poll, 2000);
          }
        })
        .catch(() => {
          if (cancelled) return;
          hostState = "unavailable";
          modelDownload = null;
        });
    }

    poll();

    return () => {
      cancelled = true;
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
      }
    };
  });

  // Why semantic search answered with nothing (a folder kept from AI says so), or null.
  let semanticRefusal = $state<string | null>(null);

  $effect(() => {
    semanticRefusal = null;
    if (palette.mode !== "semantic") {
      semanticHits = [];
      searching = false;
      return;
    }
    const term = palette.term;
    if (term === "") {
      semanticHits = [];
      searching = false;
      return;
    }
    if (!vaultRoot) {
      searching = false;
      return;
    }
    searching = true;
    const seq = ++searchSeq;
    const vault = vaultRoot;
    const scope = scopePath;
    const t = setTimeout(() => {
      void semanticSearch(vault, term, scope, 20)
        .then((hits) => {
          if (seq === searchSeq) {
            semanticHits = hits;
            searching = false;
          }
        })
        .catch((e) => {
          if (seq === searchSeq) {
            semanticHits = [];
            searching = false;
            semanticRefusal = typeof e === "string" ? e : null;
          }
        });
    }, 120);
    return () => clearTimeout(t);
  });

  let count = $derived(
    palette.mode === "file"
      ? fileMatches.length
      : palette.mode === "command"
        ? cmdMatches.length
        : palette.mode === "semantic"
          ? semanticHits.length
          : contentHits.length,
  );
  let aiUi = $derived(resolveSemanticAiUi(consent, hostState));
  // Suppress the generic "Searching…" / "No matches found" rows while the AI row takes over.
  let aiNotReady = $derived(palette.mode === "semantic" && aiUi !== "ready");
  let listState = $derived(
    paletteListState({ mode: palette.mode, term: palette.term, count, searching }),
  );
  let statusText = $derived(
    semanticRefusal ?? paletteStatusText(listState, palette.mode, folderName, palette.contentScope),
  );
  // With several notes folders, content search can look in all of them or only the open one.
  let scopeSwitch = $derived(palette.mode === "content" && folderName !== null);

  function highlight(text: string, ranges: [number, number][]): { t: string; on: boolean }[] {
    const out: { t: string; on: boolean }[] = [];
    let i = 0;
    for (const [s, e] of ranges) {
      if (s > i) out.push({ t: text.slice(i, s), on: false });
      out.push({ t: text.slice(s, e), on: true });
      i = e;
    }
    if (i < text.length) out.push({ t: text.slice(i), on: false });
    return out;
  }

  function enableAi() {
    setAiConsent(true);
    consent = true;
    void prepareAiModel(getByoConfig() ?? undefined);
    aiHost.startPolling(); // durable cold-download indicator, survives leaving the palette
    // Re-poll so the row flips prompt → preparing right away.
    void hostStatus()
      .then((s) => { hostState = s.status; modelDownload = s.embedderDownload ?? s.generatorDownload; })
      .catch(() => { hostState = "unavailable"; modelDownload = null; });
  }

  function commit(): void {
    if (count === 0) return;
    if (palette.mode === "file") {
      const { path, root } = fileMatches[palette.selected].item;
      onOpenFile(path, root as RootId | undefined);
    }
    else if (palette.mode === "command") onRunCommand(cmdMatches[palette.selected].item);
    else if (palette.mode === "semantic") onOpenFile(semanticHits[palette.selected].path);
    else {
      const h = contentHits[palette.selected];
      onOpenFile(h.path, h.rootId);
    }
    palette.hide();
  }

  // The HTML `autofocus` attribute is unreliable for elements inserted dynamically (the palette
  // mounts via {#if palette.open}): in WebView2 it fails to pull focus off an already-focused
  // element such as the CodeMirror editor, leaving focus lost so the user can't type. Focus the
  // input explicitly when it mounts — the {#if} recreates it on every open, so this runs each time.
  function focusOnMount(node: HTMLInputElement) {
    node.focus();
  }

  function onKey(e: KeyboardEvent): void {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      palette.move(1, count);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      palette.move(-1, count);
    } else if (e.key === "Enter") {
      e.preventDefault();
      commit();
    } else if (e.key === "Escape") {
      e.preventDefault();
      palette.hide();
    } else if (e.key === "Tab" && scopeSwitch) {
      // The palette is modal: Tab switches where content search looks instead of leaving it.
      e.preventDefault();
      palette.setContentScope(palette.contentScope === "all" ? "here" : "all");
    }
  }
</script>

{#if palette.open}
  <div
    class="overlay"
    data-testid="palette-overlay"
    role="presentation"
    onclick={() => palette.hide()}
    onkeydown={() => {}}
  >
    <div
      class="panel"
      role="dialog"
      aria-modal="true"
      tabindex="-1"
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => e.stopPropagation()}
    >
      <input
        class="input"
        data-testid="palette-input"
        use:focusOnMount
        placeholder="Search files…  ('>' = commands, '/' = content, '?' = semantic)"
        value={palette.query}
        oninput={(e) => palette.setQuery(e.currentTarget.value)}
        onkeydown={onKey}
      />
      {#if scopeSwitch}
        <div class="scope" role="radiogroup" aria-label="Search in" data-testid="palette-scope">
          {#each [["all", "All notes folders"], ["here", "This notes folder"]] as [value, label] (value)}
            <button
              type="button"
              role="radio"
              aria-checked={palette.contentScope === value}
              class:on={palette.contentScope === value}
              tabindex="-1"
              title={value === "here" ? `Only ${folderName}` : undefined}
              onmousedown={(e) => {
                e.preventDefault(); // keep typing in the input
                palette.setContentScope(value as ContentScope);
              }}
            >{label}</button>
          {/each}
          <kbd class="kbd" aria-hidden="true">Tab</kbd>
        </div>
      {/if}
      <ul class="results" data-testid="palette-results">
        {#if palette.mode === "file"}
          {#each fileMatches as m, i (`${m.item.root ?? ""}:${m.item.path}`)}
            <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
            <li
              class="row"
              class:sel={i === palette.selected}
              data-testid="palette-item"
              onmousedown={() => {
                palette.selected = i;
                commit();
              }}
            >
              <span class="title">
                {#if (m.item.root === undefined || m.item.root === vaultRoot) && nav?.isFavorite(m.item.path)}<span class="fav-mark" aria-hidden="true">★</span
                  >{/if}{#each highlight(m.item.name, m.ranges) as seg}<span class:hl={seg.on}
                    >{seg.t}</span
                  >{/each}
              </span>
              <span class="sub">{#if m.item.folder}<span class="folder">{m.item.folder}</span> · {/if}{m.item.path}</span>
            </li>
          {/each}
        {:else if palette.mode === "command"}
          {#each cmdMatches as m, i (m.item.id)}
            <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
            <li
              class="row cmd"
              class:sel={i === palette.selected}
              data-testid="palette-item"
              onmousedown={() => {
                palette.selected = i;
                commit();
              }}
            >
              <span class="title">
                {#each highlight(m.item.title, m.ranges) as seg}<span class:hl={seg.on}>{seg.t}</span
                  >{/each}
              </span>
              {#if m.item.keybinding}
                <kbd class="kbd">{formatKeybinding(m.item.keybinding)}</kbd>
              {/if}
            </li>
          {/each}
        {:else if palette.mode === "content"}
          {#each contentHits as h, i (`${h.rootId}:${h.path}`)}
            <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
            <li
              class="row"
              class:sel={i === palette.selected}
              data-testid="palette-item"
              onmousedown={() => {
                palette.selected = i;
                commit();
              }}
            >
              <span class="title">{h.title}</span>
              <span class="snippet">
                {#each highlight(h.snippet, h.ranges) as seg}<span class:hl={seg.on}>{seg.t}</span
                  >{/each}
              </span>
              <span class="sub">{#if h.folder}<span class="folder">{h.folder}</span> · {/if}{h.path}</span>
            </li>
          {/each}
        {:else if palette.mode === "semantic"}
          {#if aiUi !== "ready"}
            {#if aiUi === "prompt"}
              <li class="status-row ai-unavailable ai-prompt" role="status">
                <span>Enable free local AI — downloads a model (~470MB) once.</span>
                <button type="button" class="ai-enable" data-testid="ai-enable" onclick={enableAi}>
                  Enable
                </button>
              </li>
            {:else if aiUi === "preparing"}
              <li class="status-row ai-unavailable" role="status">
                {#if formatModelDownload(modelDownload) !== null}
                  {@const dl = formatModelDownload(modelDownload)!}
                  <span>{dl.label}</span>
                  <!-- The enclosing row is role="status"; its text ({dl.label}, e.g. "42% · 1.2/2.9 GB")
                       carries the announcement. The bar is decorative — aria-hidden to avoid a
                       progressbar role double-announcing alongside the live region (mirrors ChatView). -->
                  <div class="dl-bar-track" aria-hidden="true">
                    <div class="dl-bar-fill" style="width:{dl.ratio * 100}%"></div>
                  </div>
                  {#if dl.detail}<span class="dl-detail">{dl.detail}</span>{/if}
                {:else}
                  Local AI is preparing… (first run downloads the model)
                {/if}
              </li>
            {:else}
              <li class="status-row ai-unavailable" role="status">Local AI is unavailable</li>
            {/if}
          {:else}
            {#each semanticHits as h, i (h.path)}
              <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
              <li
                class="row"
                class:sel={i === palette.selected}
                data-testid="palette-item"
                onmousedown={() => {
                  palette.selected = i;
                  commit();
                }}
              >
                <span class="title">{h.path}</span>
                <span class="snippet">{h.snippet}</span>
                <span class="sub">score: {h.score.toFixed(3)}</span>
              </li>
            {/each}
          {/if}
        {/if}
        {#if !aiNotReady && statusText}
          <li
            class="status-row"
            role="status"
            data-testid={listState === "no-results" ? "palette-empty" : "palette-status"}
          >{statusText}</li>
        {/if}
      </ul>
    </div>
  </div>
{/if}

<style>
  .overlay {
    position: fixed;
    inset: 0;
    /* --bg-primary: background-based translucent overlay */
    background: color-mix(in srgb, var(--bg-primary) 60%, transparent);
    display: flex;
    justify-content: center;
    align-items: flex-start;
    padding-top: 12vh;
    z-index: 50;
  }
  .panel {
    width: min(40rem, 92vw);
    max-height: 60vh;
    display: flex;
    flex-direction: column;
    /* --bg-secondary: panel surface */
    background: var(--bg-secondary);
    /* --border: border line */
    border: 1px solid var(--border);
    /* --radius-m: panel radius */
    border-radius: var(--radius-m);
    /* --shadow-m: strongest shadow token */
    box-shadow: var(--shadow-m);
    overflow: hidden;
  }
  .input {
    padding: 0.75rem 1rem;
    border: 0;
    border-bottom: 1px solid var(--border);
    background: transparent;
    /* --text-normal: default text */
    color: var(--text-normal);
    font-size: 1rem;
    outline: none;
  }
  /* Where content search looks, with several notes folders: two choices, the chosen one marked. */
  .scope {
    display: flex;
    align-items: center;
    gap: var(--sp-1);
    padding: var(--sp-1) var(--sp-2);
    border-bottom: 1px solid var(--border);
  }
  .scope button {
    padding: 0.1rem 0.5rem;
    border: 1px solid transparent;
    border-radius: var(--radius-s);
    background: transparent;
    color: var(--text-muted);
    font: inherit;
    font-size: var(--font-size-small);
    cursor: pointer;
  }
  .scope button:hover { background: var(--bg-hover); }
  .scope button.on {
    border-color: var(--border);
    background: var(--bg-active);
    color: var(--text-normal);
  }
  .scope .kbd { margin-left: auto; }
  .results {
    margin: 0;
    padding: 0.25rem;
    list-style: none;
    overflow-y: auto;
  }
  .row {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    padding: 0.4rem 0.6rem;
    /* --radius-s: row radius */
    border-radius: var(--radius-s);
    cursor: pointer;
  }
  .row.sel {
    /* --selection-bg: derived from accent alpha (selection highlight) */
    background: var(--selection-bg);
  }
  /* Command rows lay the title and shortcut hint out horizontally (no .sub second line). */
  .row.cmd { flex-direction: row; align-items: center; justify-content: space-between; }
  .title { color: var(--text-normal); }
  /* Keyboard shortcut hint shown next to a command (discoverability). */
  .kbd {
    flex-shrink: 0;
    margin-left: 0.6rem;
    padding: 0.05rem 0.35rem;
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    color: var(--text-muted);
    font-size: 0.72rem;
    font-family: inherit;
    white-space: nowrap;
  }
  /* Favorite marker in the empty-query file list. */
  .fav-mark { color: var(--accent); margin-right: 0.35em; }
  /* --accent: match highlight */
  .hl { color: var(--accent); font-weight: 600; }
  /* --text-muted: secondary path text */
  .sub { color: var(--text-muted); font-size: 0.8rem; }
  .sub .folder { color: var(--text-normal); }
  .snippet { color: var(--text-normal); font-size: 0.85rem; }
  /* Non-interactive status row (searching / no matches) — muted, not selectable. */
  .status-row {
    padding: 0.4rem 0.6rem;
    color: var(--text-muted);
    font-size: 0.85rem;
    cursor: default;
  }
  /* AI unavailable / indexing — same muted treatment, distinct from error. */
  .ai-unavailable {
    font-style: italic;
  }
  /* Consent prompt row — spread text and button across the row. */
  .ai-prompt {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
  }
  /* Enable button — brand accent, inherits UI font. */
  .ai-enable {
    flex: none;
    padding: 0.2rem 0.6rem;
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    background: var(--accent);
    color: var(--text-on-accent);
    font: inherit;
    cursor: pointer;
  }
  .ai-enable:hover {
    background: var(--accent-hover);
  }
  /* Model download progress bar shown during the "preparing" AI state. */
  .dl-bar-track {
    width: 100%;
    height: var(--sp-1);
    background: var(--bg-secondary-alt);
    border-radius: var(--radius-s);
    overflow: hidden;
    margin-top: var(--sp-1);
  }
  .dl-bar-fill {
    height: 100%;
    background: var(--accent);
    border-radius: var(--radius-s);
    transition: width 0.4s ease;
  }
  /* Byte transfer detail (e.g. "1.2 / 2.9 GB"). */
  .dl-detail {
    display: block;
    font-size: var(--font-size-smaller);
    color: var(--text-muted);
    margin-top: var(--sp-1);
  }
</style>
