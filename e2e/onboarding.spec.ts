import { test, expect, type Browser, type Page } from "@playwright/test";
import { rmSync } from "node:fs";
import { join } from "node:path";
import { connectToApp, E2E_DEFAULT_VAULT_BASE, E2E_PERSONAL_BASE } from "./helpers";

/**
 * Onboarding E2E — first-run default vault auto-open.
 *
 * This spec exercises the app's own onMount auto-flow, NOT the loadVault dev-bridge. It therefore
 * opens whatever folder the app resolves as its default home, which must be an isolated one:
 * against a real notes folder these assertions describe personal content rather than the seeded
 * vault. `npm run dev:e2e` establishes that isolation; expectIsolatedDefaultVault below fails by
 * name if the app was started some other way.
 */

const LAST_VAULT_KEY = "textree-last-vault";

/**
 * The device's list of notes folders under the launcher's isolated home. Startup falls back to it
 * when nothing was opened last, so a fresh install is simulated only once it is gone too —
 * otherwise the folder an earlier spec opened is restored instead of the default vault.
 */
const WORKSPACE_DIR = join(E2E_PERSONAL_BASE, ".textree", "workspace");

/** Fail with the cause, not with a downstream assertion, when the app is not isolated. */
async function expectIsolatedDefaultVault(page: Page): Promise<void> {
  await expect
    .poll(() => page.evaluate((key) => localStorage.getItem(key), LAST_VAULT_KEY), {
      timeout: 30_000,
      message:
        `The default vault must live under ${E2E_DEFAULT_VAULT_BASE}. ` +
        "Start the app with `npm run dev:e2e` (scripts/dev-e2e.mjs forces TEXTREE_DEFAULT_VAULT_BASE); " +
        "a plain `tauri dev` resolves the real Documents folder instead.",
    })
    .toContain(E2E_DEFAULT_VAULT_BASE);
}

let browser: Browser;
let page: Page;

test.beforeAll(async () => {
  ({ browser, page } = await connectToApp());
});

test.afterAll(async () => {
  // Restore a neutral state: clear the last-vault so subsequent spec runs that also
  // call connectToApp start from a known (empty) state, not from the default vault.
  try {
    await page.evaluate((key) => localStorage.removeItem(key), LAST_VAULT_KEY);
  } catch {
    // Best-effort; if the page is gone the test run is already over.
  }
  await browser?.close();
});

test("restore failure: offers a calm recovery prompt when the last vault cannot be reopened", async () => {
  // Plant a path that cannot possibly exist so openVault throws on the restore branch.
  const stalePath = "Z:\\nonexistent-textree-vault";
  await page.evaluate(
    ([key, path]) => localStorage.setItem(key, path),
    [LAST_VAULT_KEY, stalePath] as [string, string],
  );
  await page.reload();

  // The recovery prompt must render instead of a dead-end. It must NOT silently auto-create a
  // new default over the intended vault — the user is offered an explicit choice.
  await expect(
    page.getByRole("button", { name: /open a folder/i }),
  ).toBeVisible({ timeout: 30_000 });
  await expect(
    page.getByRole("button", { name: /start with a default vault/i }),
  ).toBeVisible({ timeout: 5_000 });

  // The calm explanation and the stale path are surfaced (data sovereignty: the user always
  // knows which vault failed), not the alarming "Could not open vault (os error 3)" dead-end.
  await expect(
    page.getByText(/Couldn't open your last vault/),
  ).toBeVisible({ timeout: 5_000 });
  await expect(page.getByText(stalePath)).toBeVisible({ timeout: 5_000 });

  // The recovery must actually recover, not just prompt: "Start with a default vault" creates and
  // opens the default vault (seeded welcome.md), landing the user on content instead of a dead-end.
  await page.getByRole("button", { name: /start with a default vault/i }).click();
  await expectIsolatedDefaultVault(page);
  await expect(
    page.getByRole("treeitem", { name: /welcome/i }),
  ).toBeVisible({ timeout: 30_000 });

  // Cleanup: remove the bad key so the next test starts clean.
  await page.evaluate((key) => localStorage.removeItem(key), LAST_VAULT_KEY);
});

test("first run: onMount auto-opens default vault and seeds welcome.md", async () => {
  // Simulate a fresh install: clear the last-vault key and the device's list of notes folders,
  // and reload. onMount will see neither → call ensureDefaultVault() automatically.
  await page.evaluate((key) => localStorage.removeItem(key), LAST_VAULT_KEY);
  rmSync(WORKSPACE_DIR, { recursive: true, force: true });
  await page.reload();
  await expectIsolatedDefaultVault(page);

  // The seeded welcome note must appear as a treeitem (unique role, extension stripped).
  // Generous timeout: ensureDefaultVault() creates the directory + seeds the file before
  // the IPC resolves and the tree re-renders.
  await expect(
    page.getByRole("treeitem", { name: /welcome/i }),
  ).toBeVisible({ timeout: 30_000 });

  // A1: the first note (the seeded welcome) is auto-selected on startup, so the app opens to
  // content — not the "Select a note." empty hint. The welcome body must be rendered.
  await expect(page.getByText(/This is your vault/)).toBeVisible({ timeout: 10_000 });
  await expect(page.getByText("Select a note.")).toHaveCount(0);

  // The startup code sets LAST_VAULT_KEY after the vault loads.
  // By the time the treeitem is visible, setItem has already run.
  const stored = await page.evaluate(
    (key) => localStorage.getItem(key),
    LAST_VAULT_KEY,
  );
  expect(stored).not.toBeNull();
  // The default vault is always inside a directory named "Textree".
  expect(stored).toContain("Textree");
});
