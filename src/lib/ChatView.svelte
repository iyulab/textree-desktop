<script lang="ts">
  import type { RootId } from "$lib/ipc";
  import { onDestroy } from 'svelte';
  import { chatStore } from './chatStore.svelte';
  import { getGenerationConsent, setGenerationConsent, setAiConsent } from './aiConsent';
  import { getByoConfig } from './byoConfig';
  import { prepareAiModel } from './ipc';
  import type { TreeNode } from './ipc';
  import { formatModelDownload } from './modelDownload.helpers';
  import { friendlyError } from './friendlyError.helpers';
  import Icon from './Icon.svelte';
  import ChatMarkdown from './ChatMarkdown.svelte';
  import { parseChatMarkdown } from './chatMarkdown.helpers';
  import { answerEndNotice } from './ask.helpers';

  let {
    vault,
    tree,
    onOpenNote,
    onNewChat,
    onBack,
    onSaved,
  }: {
    vault: RootId;
    tree: TreeNode[];
    onOpenNote: (path: string) => void;
    onNewChat: () => void;
    onBack: () => void;
    onSaved: (path: string) => void;
  } = $props();

  let consented = $state(getGenerationConsent());
  let retryTimer: ReturnType<typeof setTimeout> | null = null;

  function clearRetry() {
    if (retryTimer !== null) {
      clearTimeout(retryTimer);
      retryTimer = null;
    }
  }
  onDestroy(clearRetry);

  // Enable local AI Q&A: persist consent, spawn host, then send the first message.
  function enableAndSend() {
    setGenerationConsent(true);
    setAiConsent(true);
    consented = true;
    void prepareAiModel(getByoConfig() ?? undefined);
    void sendAndPoll();
  }

  async function sendAndPoll() {
    clearRetry();
    if (!chatStore.draft.trim()) return;
    await chatStore.send(vault);
    scheduleRetryIfPreparing();
  }

  function scheduleRetryIfPreparing() {
    if (chatStore.status === 'preparing') {
      retryTimer = setTimeout(async () => {
        await chatStore.retry(vault);
        scheduleRetryIfPreparing();
      }, 2000);
    }
  }

  async function retryGeneration() {
    clearRetry();
    await chatStore.retryGeneration(vault);
    scheduleRetryIfPreparing();
  }

  function regenerateAnswer() {
    clearRetry();
    saveError = null;
    void (async () => {
      await chatStore.regenerate(vault);
      scheduleRetryIfPreparing();
    })();
  }

  let saveError = $state<string | null>(null);

  function summarize() {
    clearRetry();
    saveError = null;
    void (async () => {
      await chatStore.summarize(vault, tree);
      scheduleRetryIfPreparing();
    })();
  }

  function handleNewChat() {
    saveError = null;
    onNewChat();
  }

  function saveToNote(text: string) {
    saveError = null;
    void chatStore
      .saveSummaryToNote(vault, text)
      .then((p) => onSaved(p))
      .catch((e) => { saveError = friendlyError(e).summary; });
  }

  function handleKeydown(e: KeyboardEvent) {
    // Shift+Enter inserts a newline (default textarea behavior, left alone). Enter alone
    // sends — but not while an IME composition is being confirmed (Korean/Japanese/Chinese
    // input commonly fires a keydown Enter to confirm the composed text, not to submit).
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      if (busy) return;
      clearRetry();
      void sendAndPoll();
    }
  }

  // Stop an in-flight answer, keeping whatever streamed so far (chatStore.stop preserves the
  // partial assistant turn). Also clears the 'preparing' retry timer so it can't relaunch.
  function stopGeneration() {
    clearRetry();
    chatStore.stop();
  }

  const busy = $derived(chatStore.status === 'searching' || chatStore.status === 'generating');

  // ── Input auto-grow ────────────────────────────────────────────────────────
  // The composer textarea grows with its content up to MAX_INPUT_PX, then scrolls internally.
  // Re-measuring on every draft change (not just user keystrokes) also shrinks it back to one
  // row when the draft is cleared programmatically after a send.
  let inputEl = $state<HTMLTextAreaElement | null>(null);
  const MAX_INPUT_PX = 120;

  $effect(() => {
    void chatStore.draft;
    const el = inputEl;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = `${Math.min(el.scrollHeight, MAX_INPUT_PX)}px`;
  });

  // ── Scroll follow ──────────────────────────────────────────────────────────
  // Keep the transcript pinned to the newest tokens while streaming, but only when the reader
  // is already at the bottom — scrolling up to re-read must not be yanked back down. When not
  // pinned, a floating button offers a one-click jump to the latest.
  let turnsEl = $state<HTMLDivElement | null>(null);
  let atBottom = $state(true);
  const NEAR_BOTTOM_PX = 48; // treat "within 48px of the end" as pinned (tolerates sub-pixel rounding)

  // Only the reader's own hand unpins — a wheel or key moving up, or a drag/touch that ends away
  // from the end. Scroll events alone can't tell: they arrive after the scroll that caused them,
  // and layout changes (an answer re-rendered as markdown when it ends, the lines that land with
  // it) move the scroll position too. Measured then, our own jump to the end looked like the
  // reader leaving it, and following stopped short of the last lines.
  let handOnTranscript = false;
  const UP_KEYS = new Set(["ArrowUp", "PageUp", "Home"]);

  function updateAtBottom() {
    const el = turnsEl;
    if (!el) return;
    if (el.scrollHeight - el.scrollTop - el.clientHeight < NEAR_BOTTOM_PX) atBottom = true;
    else if (handOnTranscript) atBottom = false;
  }

  function unpinIfMovingUp(up: boolean) {
    if (up && turnsEl && turnsEl.scrollTop > 0) atBottom = false;
  }

  function scrollToBottom() {
    const el = turnsEl;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
    atBottom = true;
  }

  // Follow the content's height, not the data that feeds it: a pushed turn and each streamed
  // token grow it, and so does everything that lands when an answer ends — the bubble re-rendered
  // as markdown, the citations, the end notice, Regenerate. Watching only the text left the
  // transcript short of the end by whatever arrived last. Follow only while pinned.
  let contentEl = $state<HTMLDivElement | null>(null);
  $effect(() => {
    const content = contentEl;
    if (!content) return;
    const follow = new ResizeObserver(() => {
      if (atBottom) scrollToBottom();
    });
    follow.observe(content);
    return () => follow.disconnect();
  });
</script>

<svelte:window
  onpointerup={() => (handOnTranscript = false)}
  ontouchend={() => (handOnTranscript = false)}
  ontouchcancel={() => (handOnTranscript = false)}
/>

<section class="chat-view" aria-label="Chat about your notes">
  {#if !consented}
    <div class="chat-scopebar">
      <button class="chat-back" onclick={onBack} title="Back to note" aria-label="Back to note"><Icon name="file-text" /></button>
    </div>
    <div class="chat-consent">
      <p class="chat-consent-text">
        Ask questions answered from your notes — free, local, private.
        Requires a one-time AI model download (~5 GB).
      </p>
      <button class="chat-enable" onclick={enableAndSend}>Enable local AI Q&amp;A</button>
    </div>
  {:else}
    <div class="chat-scopebar">
      <button class="chat-back" onclick={onBack} title="Back to note" aria-label="Back to note"><Icon name="file-text" /></button>
      <span class="chat-chip" title={chatStore.scope.path ?? 'Whole vault'}>
        ◈ Scope: {chatStore.scope.label}
      </span>
      <button class="chat-summarize" onclick={summarize} disabled={busy} aria-label="Summarize this scope">Summarize</button>
      <button class="chat-newchat" onclick={handleNewChat}>New chat</button>
    </div>

    <div class="chat-scroll-wrap">
    <!-- A scrollable region takes focus so the keyboard can scroll it (WCAG 2.1.1); its key,
         wheel and pointer listeners only watch the reader scroll, they are not controls. -->
    <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
    <div
      class="chat-turns"
      role="region"
      aria-label="Conversation"
      tabindex="0"
      bind:this={turnsEl}
      onscroll={updateAtBottom}
      onwheel={(e) => unpinIfMovingUp(e.deltaY < 0)}
      onkeydown={(e) => unpinIfMovingUp(UP_KEYS.has(e.key))}
      onpointerdown={() => (handOnTranscript = true)}
      ontouchstart={() => (handOnTranscript = true)}
    >
      <div class="chat-turns-content" bind:this={contentEl}>
      {#each chatStore.turns as turn, i (i)}
        <!-- The last assistant turn is "streaming" while busy: keep it raw text inside a polite
             live region so tokens are announced naturally and partial markdown does not flicker.
             Completed assistant turns render as markdown and carry no live region (already
             announced, so swapping to rendered nodes does not re-announce). -->
        {@const streaming = turn.role === 'assistant' && i === chatStore.turns.length - 1 && busy}
        <div class="chat-turn {turn.role}">
          <span class="chat-speaker">{turn.role === 'user' ? 'You' : 'Assistant'}:</span>
          <!-- Live region scoped to the assistant bubble only: a polite region on the whole
               transcript re-announces every prior turn on each streamed token. Static turns are
               already in the DOM (not re-announced); only the streaming bubble's appended tokens
               speak. User turns carry no live region. -->
          {#if turn.role === 'assistant' && !streaming && turn.text}
            <div class="chat-bubble"><ChatMarkdown blocks={parseChatMarkdown(turn.text)} /></div>
          {:else}
            <div class="chat-bubble" aria-live={streaming ? 'polite' : undefined}>{turn.text}</div>
          {/if}
          {#if turn.role === 'assistant' && turn.text}
            <button class="chat-copy" type="button" aria-label="Copy message"
              onclick={() => navigator.clipboard.writeText(turn.text).catch(() => {})}>Copy</button>
            {#if chatStore.turns[i - 1]?.kind === 'summary'}
              <button class="chat-save" type="button" aria-label="Save summary to a new note"
                disabled={busy} onclick={() => saveToNote(turn.text)}>Save to note</button>
            {/if}
          {/if}
          {#if turn.role === 'assistant' && answerEndNotice(turn.end)}
            <p class="chat-end" data-testid="answer-end">{answerEndNotice(turn.end)}</p>
          {/if}
          {#if turn.role === 'assistant' && i === chatStore.turns.length - 1 && chatStore.status === 'done'}
            <button class="chat-regenerate" type="button" aria-label="Regenerate response"
              onclick={regenerateAnswer}>Regenerate</button>
          {/if}
          {#if turn.citations.length}
            <ul class="chat-citations">
              {#each turn.citations as c (c.path)}
                <li>
                  <button class="chat-citation" onclick={() => onOpenNote(c.path)} title={c.path}>
                    {c.path}
                  </button>
                </li>
              {/each}
            </ul>
          {/if}
        </div>
      {/each}
      </div>
    </div>
      {#if !atBottom}
        <button
          class="chat-scrollbtn"
          type="button"
          onclick={scrollToBottom}
          aria-label="Scroll to latest"
        ><Icon name="chevron-down" /></button>
      {/if}
    </div>

    {#if saveError}
      <p class="chat-error" role="alert">Save failed: {saveError}</p>
    {/if}

    {#if chatStore.status === 'preparing'}
      {#if formatModelDownload(chatStore.modelDownload) !== null}
        {@const dl = formatModelDownload(chatStore.modelDownload)!}
        <div class="chat-status chat-dl-progress" role="status">
          <span>{dl.label}</span>
          <div class="chat-dl-bar-track" aria-hidden="true">
            <div class="chat-dl-bar-fill" style="width:{dl.ratio * 100}%"></div>
          </div>
          {#if dl.detail}<span class="chat-dl-detail">{dl.detail}</span>{/if}
        </div>
      {:else}
        <p class="chat-status" role="status">Local AI is preparing… (will retry automatically)</p>
      {/if}
    {:else if chatStore.status === 'searching'}
      <p class="chat-status" role="status">Searching notes…</p>
    {:else if chatStore.status === 'generating'}
      <p class="chat-status" role="status">Generating answer…</p>
    {:else if chatStore.status === 'empty'}
      <p class="chat-status" role="status">No related notes found for this question.</p>
    {:else if chatStore.status === 'error'}
      <p class="chat-error" role="alert">{chatStore.errorMessage}</p>
      <button class="chat-retry" type="button" onclick={retryGeneration}>Retry</button>
    {/if}

    <div class="chat-composer">
      <textarea
        class="chat-input"
        bind:this={inputEl}
        bind:value={chatStore.draft}
        onkeydown={handleKeydown}
        rows="1"
        placeholder="Ask about your notes…"
        aria-label="Question"
      ></textarea>
      <!-- Ask ⇄ Stop: while an answer streams, the same button becomes a Stop control
           (chatStore.stop keeps the partial answer). Toggling in place keeps one focus target. -->
      <button
        class="chat-send"
        onclick={busy ? stopGeneration : () => { clearRetry(); void sendAndPoll(); }}
        aria-label={busy ? 'Stop generating' : 'Send question'}
      >{busy ? 'Stop' : 'Ask'}</button>
    </div>
  {/if}
</section>

<style>
  .chat-view {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    padding: var(--sp-3) var(--sp-6);
    background: var(--bg-primary);
  }
  .chat-consent { display: flex; flex-direction: column; gap: var(--sp-2); }
  .chat-consent-text { margin: 0; font-size: var(--font-size-small); color: var(--text-muted); }
  .chat-enable {
    align-self: flex-start;
    padding: var(--sp-1) var(--sp-3);
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    background: var(--accent);
    color: var(--text-on-accent);
    font: inherit;
    font-size: var(--font-size-small);
    cursor: pointer;
  }
  .chat-enable:hover { background: var(--accent-hover); }
  .chat-scopebar {
    display: flex;
    align-items: center;
    gap: var(--sp-2);
    margin-bottom: var(--sp-2);
  }
  .chat-back {
    flex-shrink: 0;
    width: 26px;
    height: 26px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: var(--text-muted);
    background: transparent;
    border: none;
    border-radius: var(--radius-s);
    cursor: pointer;
  }
  .chat-back:hover {
    background: var(--bg-hover);
    color: var(--text-normal);
  }
  .chat-chip {
    flex: 1;
    font-size: var(--font-size-smaller);
    color: var(--text-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .chat-summarize,
  .chat-newchat {
    flex-shrink: 0;
    padding: var(--sp-1) var(--sp-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    background: var(--bg-secondary);
    color: var(--text-normal);
    font: inherit;
    font-size: var(--font-size-smaller);
    cursor: pointer;
  }
  .chat-summarize:hover,
  .chat-newchat:hover { background: var(--bg-secondary-alt); }
  .chat-summarize:disabled { opacity: 0.5; cursor: default; }
  .chat-copy {
    align-self: flex-start;
    margin-top: var(--sp-1);
    padding: var(--sp-1) var(--sp-2);
    font-size: var(--font-size-smaller);
    color: var(--text-muted);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    cursor: pointer;
  }
  .chat-copy:hover { background: var(--bg-secondary-alt); }
  .chat-save {
    align-self: flex-start;
    margin-top: var(--sp-1);
    padding: var(--sp-1) var(--sp-2);
    font-size: var(--font-size-smaller);
    color: var(--text-muted);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    cursor: pointer;
  }
  .chat-save:hover { background: var(--bg-secondary-alt); }
  .chat-regenerate {
    align-self: flex-start;
    margin-top: var(--sp-1);
    padding: var(--sp-1) var(--sp-2);
    font-size: var(--font-size-smaller);
    color: var(--text-muted);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
    cursor: pointer;
  }
  .chat-regenerate:hover { background: var(--bg-secondary-alt); }
  .chat-end {
    margin: var(--sp-1) 0 0;
    font-size: var(--font-size-smaller);
    color: var(--text-muted);
  }
  /* Positioned wrapper so the scroll-to-latest button can float over the transcript's bottom
     edge without scrolling away with the content (the button is a sibling of, not inside, the
     overflow container). */
  .chat-scroll-wrap {
    position: relative;
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    margin-bottom: var(--sp-2);
  }
  .chat-turns {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }
  .chat-turns-content {
    display: flex;
    flex-direction: column;
    gap: var(--sp-3);
  }
  .chat-scrollbtn {
    position: absolute;
    bottom: var(--sp-2);
    left: 50%;
    transform: translateX(-50%);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    padding: 0;
    color: var(--text-normal);
    background: var(--bg-secondary);
    border: 1px solid var(--border-strong);
    border-radius: 50%;
    box-shadow: var(--shadow-m);
    cursor: pointer;
  }
  .chat-scrollbtn:hover { background: var(--bg-secondary-alt); }
  .chat-turns:focus-visible { outline: 2px solid var(--accent); outline-offset: -2px; }
  .chat-scrollbtn:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
  .chat-turn { display: flex; flex-direction: column; gap: var(--sp-1); }
  .chat-turn.user { align-items: flex-end; }
  /* Visually hidden, screen-reader only: announces the speaker (the bubble alignment/colour
     conveys it visually, but not to assistive tech). */
  .chat-speaker {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }
  .chat-bubble {
    max-width: 90%;
    padding: var(--sp-2) var(--sp-3);
    border-radius: var(--radius-s);
    font-size: var(--font-size-small);
    line-height: 1.6;
    white-space: pre-wrap;
  }
  .chat-turn.user .chat-bubble { background: var(--accent); color: var(--text-on-accent); }
  .chat-turn.assistant .chat-bubble { background: var(--bg-secondary); color: var(--text-normal); }
  .chat-citations { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: var(--sp-1); }
  .chat-citation {
    display: block;
    max-width: 100%;
    text-align: left;
    background: none;
    border: none;
    padding: var(--sp-1) var(--sp-2);
    border-radius: var(--radius-s);
    color: var(--accent);
    font-family: var(--font-ui);
    font-size: var(--font-size-small);
    cursor: pointer;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .chat-citation:hover { background: var(--bg-secondary-alt); }
  .chat-citation:focus-visible { outline: 2px solid var(--accent); outline-offset: -2px; }
  .chat-status { margin: 0 0 var(--sp-2); font-size: var(--font-size-small); color: var(--text-muted); font-style: italic; }
  /* Model download progress shown during the 'preparing' state. */
  .chat-dl-progress { display: flex; flex-direction: column; gap: var(--sp-1); font-style: italic; }
  .chat-dl-bar-track {
    width: 100%;
    height: var(--sp-1);
    background: var(--bg-secondary-alt);
    border-radius: var(--radius-s);
    overflow: hidden;
  }
  .chat-dl-bar-fill {
    height: 100%;
    background: var(--accent);
    border-radius: var(--radius-s);
    transition: width 0.4s ease;
  }
  .chat-dl-detail { font-size: var(--font-size-smaller); color: var(--text-muted); }
  .chat-error { margin: 0 0 var(--sp-2); font-size: var(--font-size-small); color: var(--text-error); }
  .chat-retry {
    align-self: flex-start;
    margin: 0 0 var(--sp-2);
    padding: var(--sp-1) var(--sp-3);
    font: inherit;
    font-size: var(--font-size-small);
    cursor: pointer;
    color: var(--text-normal);
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: var(--radius-s);
  }
  .chat-retry:hover { background: var(--bg-secondary-alt); }
  .chat-retry:focus-visible { outline: 2px solid var(--accent); outline-offset: -2px; }
  .chat-composer { display: flex; align-items: flex-end; gap: var(--sp-1); flex-shrink: 0; }
  .chat-input {
    flex: 1;
    min-width: 0;
    font: inherit;
    font-size: var(--font-size-small);
    padding: var(--sp-1) var(--sp-2);
    color: var(--text-normal);
    background: var(--bg-primary);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-s);
    resize: none;
    overflow-y: auto;
    max-height: 120px;
  }
  .chat-input:focus { outline: none; border-color: var(--accent); }
  .chat-send {
    flex-shrink: 0;
    padding: var(--sp-1) var(--sp-3);
    font: inherit;
    font-size: var(--font-size-small);
    cursor: pointer;
    color: var(--text-on-accent);
    background: var(--accent);
    border: none;
    border-radius: var(--radius-s);
  }
  .chat-send:hover { background: var(--accent-hover); }
</style>
