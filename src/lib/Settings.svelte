<script lang="ts">
  import type { RootId } from "./ipc";
  import { theme, type ThemeMode } from "$lib/theme.svelte";
  import {
    getAiConsent, setAiConsent, getGenerationConsent, setGenerationConsent,
  } from "$lib/aiConsent";
  import {
    prepareAiModel, prepareGeneration, stopHost, hostStatus, restartAiHost,
    testByoConnection, setByoApiKey, clearByoApiKey, hasByoApiKey, type HostStatus,
    type ByoConnectionTestResult,
    connectPublish, clearPublishToken, hasPublishToken,
  } from "$lib/ipc";
  import {
    computeAiSectionState, themeButtons, planEmbeddingToggle, byoTestLabel,
    presetDefaults, isValidByoUrlForPreset, isValidByoModel, byoProviderBadge, byoKeyRequired,
    byoBaseUrlOptional,
  } from "$lib/settings.helpers";
  import {
    getByoConfig, setByoConfig, clearByoConfig, BYO_PRESETS, BYO_PRESET_IDS, type ByoConfig, type ByoPreset,
  } from "$lib/byoConfig";
  import BackupPanel from "$lib/BackupPanel.svelte";

  interface Props {
    root: RootId | null;
    /** Where that folder is on disk — what the person is shown. */
    rootPath: string | null;
    onOpenVault: () => void;
    onclose: () => void;
  }
  let { root, rootPath, onOpenVault, onclose }: Props = $props();

  let aiConsent = $state(getAiConsent());
  let genConsent = $state(getGenerationConsent());
  let host = $state<HostStatus | null>(null);
  let activeProvider = $state("");
  let panelEl = $state<HTMLDivElement | undefined>();

  const ai = $derived(computeAiSectionState(aiConsent, genConsent, host));
  const themes = $derived(themeButtons(theme.mode));

  // BYO (bring-your-own OpenAI-compatible endpoint) — Settings ▸Advanced.
  // `byoStored` is re-read from its source of truth (localStorage, via getByoConfig()) after
  // every mutation rather than derived/patched in place, so the "switch back to bundled local
  // model" affordance never lingers stale against what's actually persisted.
  const initialByo = getByoConfig();
  let byoStored = $state(initialByo);
  let byoEnabled = $state(initialByo !== null);
  let byoPreset = $state<ByoPreset>(initialByo?.preset ?? "ollama");
  let byoBaseUrl = $state(initialByo?.baseUrl ?? presetDefaults("ollama").baseUrl);
  let byoModel = $state(initialByo?.model ?? "");
  let byoApiKeyInput = $state("");
  let byoHasStoredKey = $state(false);
  let byoTestResult = $state<ByoConnectionTestResult | "checking" | null>(null);
  let byoSaving = $state(false);
  // Sibling to byoTestResult: surfaces Save/Switch-to-local failures inline (near the
  // triggering button) rather than letting a rejected restartAiHost/setByoApiKey promise
  // become an unhandled rejection visible only in devtools.
  let byoActionError = $state<string | null>(null);

  $effect(() => {
    void hasByoApiKey().then((v) => { byoHasStoredKey = v; });
  });

  // Web publishing connection (Settings ▸Advanced). The token is obtained via in-app sign-in
  // (browser OAuth) — never pasted — and lives in the OS keychain. The UI only ever knows whether
  // a token is stored; it never sees the token itself.
  let publishHasToken = $state(false);
  let publishSaving = $state(false);
  let publishError = $state<string | null>(null);

  $effect(() => {
    void hasPublishToken().then((v) => { publishHasToken = v; });
  });

  async function onConnectPublish(): Promise<void> {
    publishSaving = true;
    publishError = null;
    try {
      await connectPublish(); // opens the browser; resolves once the token is stored
      publishHasToken = true;
    } catch (err) {
      publishError = err instanceof Error ? err.message : String(err);
    }
    publishSaving = false;
  }

  async function onClearPublishToken(): Promise<void> {
    publishSaving = true;
    publishError = null;
    try {
      await clearPublishToken();
      publishHasToken = false;
    } catch (err) {
      publishError = err instanceof Error ? err.message : String(err);
    }
    publishSaving = false;
  }

  function onPresetChange(preset: ByoPreset): void {
    byoPreset = preset;
    byoBaseUrl = presetDefaults(preset).baseUrl;
  }

  async function onTestConnection(): Promise<void> {
    byoTestResult = "checking";
    const key = byoApiKeyInput || undefined;
    byoTestResult = await testByoConnection(byoPreset, byoBaseUrl, key);
  }

  async function onSaveByo(): Promise<void> {
    byoSaving = true;
    byoActionError = null;
    const config: ByoConfig = { preset: byoPreset, baseUrl: byoBaseUrl, model: byoModel };
    try {
      if (byoApiKeyInput) {
        await setByoApiKey(byoApiKeyInput);
        byoHasStoredKey = true;
        byoApiKeyInput = "";
      }
      await restartAiHost(config);
      // Only persist to localStorage once the host confirms the restart succeeded —
      // otherwise a failed restart would leave localStorage claiming BYO is active
      // while the host is still on (or stuck on) the old provider.
      setByoConfig(config);
      byoStored = getByoConfig();
    } catch (err) {
      byoActionError = err instanceof Error ? err.message : String(err);
    }
    // Unconditional, mirroring toggleEmbedding: the badge is the honesty signal
    // regardless of whether the restart above succeeded or failed.
    await refreshHost();
    byoSaving = false;
  }

  async function onSwitchToLocal(): Promise<void> {
    byoSaving = true;
    byoActionError = null;
    byoEnabled = false;
    try {
      await restartAiHost();
      // Only clear the stored BYO config once the switch back actually succeeds —
      // same reasoning as onSaveByo, in reverse.
      clearByoConfig();
      byoStored = getByoConfig();
    } catch (err) {
      byoActionError = err instanceof Error ? err.message : String(err);
    }
    await refreshHost();
    byoSaving = false;
  }

  async function onClearApiKey(): Promise<void> {
    await clearByoApiKey();
    byoHasStoredKey = false;
  }

  async function refreshHost(): Promise<void> {
    try {
      const status = await hostStatus();
      host = status.status;
      activeProvider = status.activeProvider;
    } catch {
      host = "unavailable";
    }
  }
  $effect(() => {
    void refreshHost();
  });

  // While the host is spawning, keep polling so the badge flips preparing → ready
  // without the user reopening the modal (spec §4.3: the badge is the honesty signal).
  $effect(() => {
    if (host !== "starting") return;
    const id = setInterval(() => {
      void refreshHost();
    }, 1000);
    return () => clearInterval(id);
  });

  $effect(() => {
    panelEl?.focus();
  });

  async function toggleEmbedding(next: boolean): Promise<void> {
    const plan = planEmbeddingToggle(next, genConsent);
    setAiConsent(plan.nextAiConsent);
    setGenerationConsent(plan.nextGenConsent);
    aiConsent = plan.nextAiConsent;
    genConsent = plan.nextGenConsent;
    try {
      if (plan.host === "spawn") await prepareAiModel();
      else await stopHost();
    } catch {
      /* failures surface via the host-status badge on the next refresh */
    }
    await refreshHost();
  }

  async function toggleGeneration(next: boolean): Promise<void> {
    setGenerationConsent(next);
    genConsent = next;
    if (next) {
      try {
        await prepareGeneration();
      } catch {
        /* badge reflects host state */
      }
    }
  }

  function onKey(e: KeyboardEvent): void {
    if (e.key === "Escape") {
      e.preventDefault();
      onclose();
    }
  }
</script>

<div class="overlay" role="presentation" onclick={onclose} onkeydown={() => {}}>
  <div
    bind:this={panelEl}
    class="panel"
    role="dialog"
    aria-modal="true"
    aria-label="Settings"
    tabindex="-1"
    onclick={(e) => e.stopPropagation()}
    onkeydown={onKey}
  >
    <header class="head">
      <h2>Settings</h2>
      <button type="button" class="close" aria-label="Close settings" onclick={onclose}>×</button>
    </header>

    <section aria-label="Appearance">
      <h3>Appearance</h3>
      <!-- Segmented single-choice control: mirrors the Note│Chat / reading toggles
           (role=group + aria-pressed), the project convention. A radiogroup would
           demand roving tabindex + arrow-key nav; native toggle buttons stay fully
           keyboard-reachable without it. -->
      <div class="seg-group" role="group" aria-label="Theme">
        {#each themes as t (t.mode)}
          <button
            type="button"
            class="seg"
            class:active={t.active}
            aria-pressed={t.active}
            onclick={() => theme.set(t.mode as ThemeMode)}
          >{t.label}</button>
        {/each}
      </div>
    </section>

    <section aria-label="Vault">
      <h3>Vault</h3>
      <p class="vault-path" title={rootPath ?? ""}>{rootPath ?? "No vault open"}</p>
      <button type="button" class="action" onclick={onOpenVault}>Open / switch vault</button>
    </section>

    <section aria-label="Backup" data-testid="settings-backup">
      <h3>Backup</h3>
      {#if root}
        <BackupPanel {root} />
      {:else}
        <p class="badge">Open a folder to back it up.</p>
      {/if}
    </section>

    <section aria-label="Local AI">
      <h3>Local AI</h3>
      <label class="toggle">
        <input
          type="checkbox"
          checked={ai.embeddingChecked}
          onchange={(e) => toggleEmbedding(e.currentTarget.checked)}
        />
        <span>Embeddings &amp; search</span>
      </label>
      <label class="toggle" class:disabled={ai.generationDisabled}>
        <input
          type="checkbox"
          checked={ai.generationChecked}
          disabled={ai.generationDisabled}
          onchange={(e) => toggleGeneration(e.currentTarget.checked)}
        />
        <span>Q&amp;A &amp; chat</span>
      </label>
      <p class="badge" role="status">{ai.badge}</p>

      <details class="byo-advanced">
        <summary>▸ Advanced: custom AI server &amp; web publish</summary>

        <p class="badge">
          Connect your account to publish your vault to the web with one click.
        </p>
        {#if publishHasToken}
          <p class="badge" role="status">✓ Connected to web publishing</p>
          <button type="button" class="action" disabled={publishSaving} onclick={onClearPublishToken}>
            {publishSaving ? "…" : "Disconnect"}
          </button>
        {:else}
          <button type="button" class="action" disabled={publishSaving} onclick={onConnectPublish}>
            {publishSaving ? "Connecting…" : "Connect"}
          </button>
        {/if}
        {#if publishError}
          <p class="badge" role="status">✗ Failed: {publishError}</p>
        {/if}

        <label class="toggle">
          <input
            type="checkbox"
            checked={byoEnabled}
            onchange={(e) => (byoEnabled = e.currentTarget.checked)}
          />
          <span>Use a custom AI server</span>
        </label>
        {#if byoEnabled}
          <div class="seg-group" role="group" aria-label="Preset">
            {#each BYO_PRESET_IDS as p (p)}
              <button
                type="button"
                class="seg"
                class:active={byoPreset === p}
                aria-pressed={byoPreset === p}
                onclick={() => onPresetChange(p)}
              >{BYO_PRESETS[p].label}</button>
            {/each}
          </div>
          <label>
            Base URL{#if byoBaseUrlOptional(byoPreset)} (optional){/if}
            <input type="text" bind:value={byoBaseUrl}
              placeholder={byoBaseUrlOptional(byoPreset) ? BYO_PRESETS[byoPreset].defaultBaseUrl : "https://your-server/v1"} />
          </label>
          <label>
            Model (required)
            <input type="text" bind:value={byoModel} placeholder="e.g. llama3.1 (no provider default exists)" />
          </label>
          <label>
            API Key {byoKeyRequired(byoPreset) ? "(required)" : "(optional)"}
            <input
              type="password"
              bind:value={byoApiKeyInput}
              placeholder={byoHasStoredKey ? "•••• saved" : "Leave blank if none"}
            />
          </label>
          {#if byoHasStoredKey}
            <button type="button" class="action" onclick={onClearApiKey}>Clear API key</button>
          {/if}
          <!-- Not gated on isValidByoModel: this button's job is reachability of the base URL
               (does the server answer at all), not correctness of the model name — a user
               should be able to verify the server is up before typing a model. Save is where
               the model becomes required, since that's what actually reaches /chat. -->
          <button
            type="button"
            class="action"
            disabled={!isValidByoUrlForPreset(byoPreset, byoBaseUrl) || byoTestResult === "checking"}
            onclick={onTestConnection}
          >
            Test connection
          </button>
          {#if byoTestResult}
            <p class="badge" role="status">
              {byoTestLabel(byoTestResult)}
            </p>
          {/if}
          <button
            type="button"
            class="action"
            disabled={byoSaving || !isValidByoUrlForPreset(byoPreset, byoBaseUrl) || !isValidByoModel(byoModel)
              || (byoKeyRequired(byoPreset) && !byoApiKeyInput.trim() && !byoHasStoredKey)}
            onclick={onSaveByo}
          >
            {byoSaving ? "Applying…" : "Save"}
          </button>
        {:else if byoStored !== null}
          <button type="button" class="action" disabled={byoSaving} onclick={onSwitchToLocal}>
            {byoSaving ? "Applying…" : "Switch back to bundled local model"}
          </button>
        {/if}
        {#if byoActionError}
          <p class="badge" role="status">✗ Failed: {byoActionError}</p>
        {/if}
        <p class="badge" role="status">Current: {byoProviderBadge(activeProvider)}</p>
      </details>
    </section>
  </div>
</div>

<style>
  /* Overlay + panel mirror Palette.svelte's proven modal pattern. */
  .overlay {
    position: fixed;
    inset: 0;
    z-index: 50;
    background: color-mix(in srgb, var(--bg-primary) 60%, transparent);
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .panel {
    width: min(28rem, 92vw);
    max-height: 86vh;
    overflow-y: auto;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: var(--radius-m);
    box-shadow: var(--shadow-m);
    padding: var(--sp-4);
  }
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: var(--sp-3);
  }
  h2 {
    /* --fs-l not in tokens.css; substituted with --font-size-editor (16px) for heading prominence */
    font-size: var(--font-size-editor);
    margin: 0;
    color: var(--text-normal);
  }
  .close {
    background: none;
    border: none;
    color: var(--text-muted);
    /* --fs-l not in tokens.css; substituted with --font-size-editor (16px) */
    font-size: var(--font-size-editor);
    cursor: pointer;
  }
  section {
    padding: var(--sp-3) 0;
    border-top: 1px solid var(--border);
  }
  h3 {
    /* --fs-s not in tokens.css; substituted with --font-size-small (13px) */
    font-size: var(--font-size-small);
    text-transform: uppercase;
    color: var(--text-muted);
    margin: 0 0 var(--sp-2);
  }
  .seg-group {
    display: inline-flex;
    flex-wrap: wrap;
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    overflow: hidden;
  }
  .seg {
    background: var(--bg-secondary);
    border: none;
    color: var(--text-normal);
    padding: var(--sp-1) var(--sp-3);
    min-height: var(--sp-6);
    cursor: pointer;
  }
  .seg:hover {
    background: var(--bg-hover);
  }
  .seg.active {
    background: var(--accent);
    color: var(--text-on-accent);
  }
  .vault-path {
    color: var(--text-muted);
    /* --fs-s not in tokens.css; substituted with --font-size-small (13px) */
    font-size: var(--font-size-small);
    word-break: break-all;
    margin: 0 0 var(--sp-2);
  }
  .action {
    background: var(--bg-secondary-alt);
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    color: var(--text-normal);
    padding: var(--sp-1) var(--sp-3);
    min-height: var(--sp-6);
    cursor: pointer;
  }
  .action:hover {
    background: var(--bg-hover);
  }
  .toggle {
    display: flex;
    align-items: center;
    gap: var(--sp-2);
    padding: var(--sp-1) 0;
    color: var(--text-normal);
    cursor: pointer;
  }
  .toggle.disabled {
    color: var(--text-muted);
    cursor: not-allowed;
  }
  .badge {
    margin: var(--sp-2) 0 0;
    /* --fs-s not in tokens.css; substituted with --font-size-small (13px) */
    font-size: var(--font-size-small);
    color: var(--text-muted);
  }
  .byo-advanced {
    margin-top: var(--sp-3);
  }
  .byo-advanced summary {
    cursor: pointer;
    color: var(--text-muted);
    font-size: var(--font-size-small);
  }
  .byo-advanced > :not(summary) {
    margin-top: var(--sp-2);
  }
  .byo-advanced label:not(.toggle) {
    display: flex;
    flex-direction: column;
    gap: var(--sp-1);
    color: var(--text-normal);
    font-size: var(--font-size-small);
  }
  .byo-advanced input[type="text"],
  .byo-advanced input[type="password"] {
    background: var(--bg-secondary-alt);
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    color: var(--text-normal);
    padding: var(--sp-1) var(--sp-2);
    min-height: var(--sp-6);
  }
  .byo-advanced .seg-group {
    margin: var(--sp-2) 0;
  }
</style>
