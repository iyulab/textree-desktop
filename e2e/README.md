# Textree E2E (Playwright + WebView2 CDP)

Connects via CDP to the **real WebView2** of the running Tauri app. This is not mock IPC —
it goes through the actual filesystem backend, so it verifies the core value that
"the filesystem is the source of truth" exactly as it is. (CDP is **Windows/WebView2 only**.)

## Running

**One command** — `npm run e2e:run` does everything below: it stops a dev build or dev server an
earlier session left running (only processes whose executable is inside this repository), starts
the app through the launcher, waits until the app page answers over CDP, runs the suite, and stops
the app again. Arguments are passed to `playwright test`; `--soak N` repeats the suite N times
against one app instance. It points `TEXTREE_CANOPY_CLI` at the assembled renderer
(`src-tauri/resources/canopy/node_modules/@iyulab/canopy/dist/cli.js`) when that exists and the variable is unset. The app's output
goes to `e2e-results/app.log`.

`npm run e2e:run -- --embedded` runs the same suite against a debug build with the frontend
embedded (`tauri build --debug --no-bundle`) instead of the vite dev server. That is the only way
the pages run under the content security policy a shipped app has — `tauri dev` uses the looser
`devCsp`. It is still a debug build, so the test overrides are honoured; the test bridge is bundled
because the launcher sets `VITE_TEXTREE_E2E`, which a shipped build never has. The first run
compiles the app, which takes minutes.

A leftover app is worth the trouble: the specs cannot tell an app built from older code from the
current one, so a stale instance answering on the CDP port makes them pass or fail against the
wrong build.

**By hand** — two terminals.

**Terminal 1 — launch the app with a remote debugging port:**

```powershell
$env:TEXTREE_CANOPY_CLI=(Resolve-Path "..\canopy\dist\cli.js").Path   # only needed for publish.spec (build canopy first)
npm run dev:e2e
```

`TEXTREE_CANOPY_CLI` must be an **absolute** path (hence `Resolve-Path`): the backend passes it
to `node` as-is, and under `tauri dev` the app process's working directory is `src-tauri/`, so a
path relative to this folder silently fails to spawn and publish reports
"The publishing tool couldn't finish".

`dev:e2e` runs `scripts/dev-e2e.mjs`, which starts `tauri dev` with the overlay
`src-tauri/tauri.e2e.conf.json`: the overlay injects `--remote-debugging-port` via the window's
`additionalBrowserArgs` (the programmatic `CoreWebView2EnvironmentOptions` path).

The launcher owns the port. It uses 9222 when free; when something else already holds 9222 (a
browser or an Electron app with remote debugging on), it picks a free port instead and says so.
Set `TEXTREE_E2E_CDP_PORT` to demand a specific port — the launcher then fails immediately, naming
the occupant, if that port is taken. The chosen endpoint is written to `textree-e2e-cdp.json` in the
temp directory, and `e2e/helpers.ts` reads it, so the specs need no configuration of their own.
`TEXTREE_E2E_CDP` still overrides the endpoint the specs connect to.

The launcher also points `TEXTREE_DEFAULT_VAULT_BASE` at a temp directory it recreates on every
start. Specs that exercise the first-run flow open whatever folder the app resolves as its default
home; without that isolation it is the developer's real Documents folder, and those specs end up
asserting against personal notes. Start the app any other way and they fail by name (see
`expectIsolatedDefaultVault` in `onboarding.spec.ts`) rather than as a puzzling mismatch.

It also sets `TEXTREE_E2E_STALL_FLAG` to a file in the temp directory. While that file exists, the
development build holds every write just before it lands — a folder that has stopped answering.
Specs create and remove it with `stallWrites()` / `releaseWrites()`; release builds do not have
the hold at all.

`TEXTREE_E2E_OPEN_STALL_FLAG` does the same for opening a folder: while the file holds a folder's
path, opening that folder waits before reading it. Specs use `stallOpening(path)` /
`releaseOpening()`.

Dialogs never open: `dev:e2e` builds the app with the `e2e` feature and sets
`TAURI_KIT_DIALOG_ANSWERS`, so each folder dialog takes the next answer from that file instead of
showing anything. A spec queues one with `answerFolderDialog(path)` (or `null` for closing it) before
the click that opens the dialog, and `dialogsAsked()` lists the titles asked so far. A dialog with no
answer queued fails the call rather than waiting for someone at the screen.

> ⚠️ The old `$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` route no longer works — WebView2
> Runtime 150+ ignores the loader environment variable, so the CDP port silently never opens.
> Setting `additionalBrowserArgs` replaces wry's default arguments, which is why the overlay
> restates `--disable-features=...` and `--autoplay-policy=...` alongside the debug port.
> The overlay must also restate the full window object: `--config` merges via JSON Merge
> Patch (RFC 7396), which replaces the `app.windows` array wholesale — keep it in sync with
> `tauri.conf.json` if the main window config changes.

**Terminal 2 — run the E2E suite:**

```bash
npm run test:e2e
```

## Layout

- `helpers.ts` — CDP connection / app page lookup, a dev bridge (`window.__textreeTest`:
  `loadVault` / `publishTo`) to bypass the native folder dialogs, temporary vault fixtures, and
  native DnD dispatch.

**Core (filesystem ↔ tree ↔ editor):**

- `smoke.spec.ts` — open vault, select note (depends on `sample-vault`).
- `editing.spec.ts` — editing, debounced autosave, flush on switch.
- `sync.spec.ts` — external sync (reload/create/delete, conflict banner & resolution).
- `structure.spec.ts` — structural edits (create/rename/delete/DnD/＋child/adopt, vault-switch isolation).
- `attachment.spec.ts` — image paste → save into assets/ + link.
- `tree.spec.ts` — tree expand/collapse, keyboard navigation, breadcrumb.
- `pagedetail.spec.ts` — inline title (file name) editing.
- `search.spec.ts` — full-text content search.

**UI / shell:**

- `palette.spec.ts` — unified palette (file search + command mode, depends on `sample-vault`).
- `layout.spec.ts` — sidebar collapse/expand + resize, persistence.
- `theme.spec.ts` — theme toggle + token-driven color change, persistence.
- `sidecar-ux.spec.ts` — manual ordering + favorites (star affordance and palette command) persist to the sidecar.

**Editor rendering (pretty-by-default):**

- `livepreview.spec.ts` — inline heading/emphasis/code render + marker hide/reveal.
- `frontmatter.spec.ts` — frontmatter page header (title/icon) + editor folding pill.
- `reading.spec.ts` — reading-view toggle (markers hidden, editor read-only).
- `wikilink.spec.ts` — wikilink render/click navigation, backlinks panel, `[[` autocomplete.

**Publishing:**

- `publish.spec.ts` — publish the vault to a static site via canopy (auto-theming tokens, source
  `.md` byte-unchanged, self-host banner), the confirmation shown first (what goes out, notes
  without a version, hidden files; Cancel and Escape publish nothing), and dot-files kept out of the
  site. The bridge's `publishTo` settles only once the confirmation is answered — click
  `publish-confirm-go` or `publish-confirm-cancel`. Requires the app to be launched with
  `TEXTREE_CANOPY_CLI` set to canopy's CLI (e.g. `../canopy/dist/cli.js`) so the backend can spawn it.

  > The dev E2E drives publish via `TEXTREE_CANOPY_CLI` (the dev resolution branch). The **production**
  > path — the bundled `node + cli.js` sidecar under the resource dir — is guarded by the Rust
  > integration test `run_publish_via_assembled_sidecar` (run after assembling the payload; CI runs it
  > in the release build). See `scripts/assemble-canopy-sidecar.ps1`.

**Backup:**

- `backup-status.spec.ts` — the "Not backed up" indicator stays in view on an open note.
- `backup.spec.ts` — connect a folder to a repository from the indicator (a wrong token is refused
  and nothing is kept), a second folder receives the notes, a new version reaches the repository
  in the background, and Settings shows and stops the same backup. The repository is served by
  `git-remote.ts`: a bare repository behind `git http-backend` run as CGI from a small Node HTTP
  server with Basic authentication on 127.0.0.1. Skipped when `git` is not on PATH. The spec
  disconnects both folders at the end so no access token is left in the system credential store.

## Notes

- `smoke.spec.ts` and `palette.spec.ts` depend on a local `sample-vault/` (gitignored). The rest
  create and clean up isolated fixture vaults under the OS temp directory, so they are self-contained.
- Specs share one running app instance, so app-level state (theme, reading mode) can leak between
  files; tests that care normalize it at their start.
- The dev bridge is guarded by `import.meta.env.DEV`, so it is tree-shaken out of production bundles.
- Every run appends a block to `e2e-results/history.log` (gitignored): the time, the totals, and
  each failed test with its location and first error line. `test-results/` is wiped by the next
  run and the console scrolls away, so a failure that does not come back on a re-run is only named
  there — read it before re-running, and across runs to see which tests fail now and then.
