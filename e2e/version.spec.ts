import { test, expect, type Browser, type Page } from "@playwright/test";
import {
  connectToApp,
  loadVault,
  createTempVault,
  removeTempVault,
  readVaultFile,
  writeVaultFile,
  recordNotes,
} from "./helpers";

/**
 * Add version → Version history → look at an earlier state → go back to it.
 *
 * The round trip the version features exist for, driven through the screens a user touches:
 * the Add version dialog (Ctrl+Shift+S), the history panel, its preview, and its restore.
 *
 * An isolated temp vault: adding versions writes history, and restoring rewrites the note.
 */

let browser: Browser;
let page: Page;

test.beforeAll(async () => {
  ({ browser, page } = await connectToApp());
});

test.afterAll(async () => {
  await browser?.close();
});

/** Run a palette command, waiting until the list is filtered down to it before pressing Enter. */
async function runCommand(p: Page, title: string): Promise<void> {
  await p.keyboard.press("Control+p");
  await expect(p.getByTestId("palette-input")).toBeVisible();
  await p.getByTestId("palette-input").fill(`>${title}`);
  await expect(p.getByTestId("palette-item").first()).toContainText(title);
  await p.keyboard.press("Enter");
  await expect(p.getByTestId("palette-overlay")).toHaveCount(0);
}

/** Append a line to the open note and wait until the autosave has put it on disk. */
async function appendAndSave(p: Page, vault: string, rel: string, line: string): Promise<void> {
  await p.locator(".cm-content").click();
  await p.keyboard.press("Control+End");
  await p.keyboard.type(`\n${line}`);
  await expect.poll(() => readVaultFile(vault, rel), { timeout: 5000 }).toContain(line);
}

/** Add a version of the open note under the given name. */
async function addVersion(p: Page, name: string): Promise<void> {
  await p.keyboard.press("Control+Shift+S");
  await expect(p.getByTestId("add-version")).toBeVisible();
  await expect(p.getByTestId("add-version-scope")).toHaveText("This note only");
  await p.getByTestId("add-version-name").fill(name);
  await p.getByTestId("add-version-confirm").click();
  await expect(p.getByTestId("add-version")).toHaveCount(0);
  await expect(p.getByTestId("add-version-error")).toHaveCount(0);
}

test("versions: add two → both listed, newest current → preview the first → restore it", async () => {
  const vault = createTempVault({ "memo.md": "# memo\n\nfirst line\n" });

  try {
    await loadVault(page, vault);
    await page.getByRole("treeitem", { name: /memo/ }).click();
    await expect(page.locator(".cm-content")).toBeVisible();

    await appendAndSave(page, vault, "memo.md", "draft one");
    await addVersion(page, "draft one");
    await expect(page.getByTestId("version-notice")).toContainText("Added a version of memo");
    const firstState = readVaultFile(vault, "memo.md");

    await appendAndSave(page, vault, "memo.md", "draft two");
    // Adding the first version creates the repository, which the watcher sees as a change to
    // the checked-out branch and answers with a full refresh. Typing through that refresh is the
    // user's own edit — it must not be presented as a change made on disk.
    await expect(page.locator(".banner")).toHaveCount(0);
    await addVersion(page, "draft two");

    await runCommand(page, "Version history");
    const panel = page.getByTestId("version-history");
    await expect(panel).toBeVisible();
    const entries = page.getByTestId("version-entry");
    await expect(entries).toHaveCount(2);
    // Newest first, and the note on disk is that newest version.
    await expect(entries.nth(0)).toContainText("draft two");
    await expect(entries.nth(1)).toContainText("draft one");
    await expect(panel.getByText("Current")).toHaveCount(1);
    await expect(page.getByTestId("version-restore")).toHaveCount(1);

    // Looking at the earlier state shows it, and does not touch the note.
    await entries.nth(1).click();
    const preview = page.getByTestId("version-preview");
    await expect(preview).toContainText("draft one");
    await expect(preview).not.toContainText("draft two");
    expect(readVaultFile(vault, "memo.md")).toContain("draft two");

    // Going back rewrites the note to exactly that state, and the editor follows.
    await page.getByTestId("version-restore").click();
    await expect.poll(() => readVaultFile(vault, "memo.md"), { timeout: 5000 }).toBe(firstState);
    await expect(page.locator(".cm-content")).not.toContainText("draft two");
    await expect(page.locator(".cm-content")).toContainText("draft one");
  } finally {
    removeTempVault(vault);
  }
});

test("versions: a note never given a version says so instead of listing nothing", async () => {
  const vault = createTempVault({ "fresh.md": "# fresh\n" });

  try {
    await loadVault(page, vault);
    await page.getByRole("treeitem", { name: /fresh/ }).click();
    await expect(page.locator(".cm-content")).toBeVisible();

    await runCommand(page, "Version history");
    const panel = page.getByTestId("version-history");
    await expect(panel).toBeVisible();
    await expect(page.getByTestId("version-entry")).toHaveCount(0);
    await expect(panel.locator(".empty")).not.toBeEmpty();
  } finally {
    removeTempVault(vault);
  }
});

test("versions: a version added while the history is open shows up in it", async () => {
  const vault = createTempVault({ "open.md": "# open\n" });

  try {
    await loadVault(page, vault);
    await page.getByRole("treeitem", { name: /open/ }).click();
    await expect(page.locator(".cm-content")).toBeVisible();
    await runCommand(page, "Version history");
    await expect(page.getByTestId("version-history")).toBeVisible();
    await expect(page.getByTestId("version-entry")).toHaveCount(0);

    await appendAndSave(page, vault, "open.md", "kept");
    await addVersion(page, "kept");
    await expect(page.getByTestId("version-entry")).toHaveCount(1);
    await expect(page.getByTestId("version-entry").first()).toContainText("kept");
  } finally {
    removeTempVault(vault);
  }
});

test("versions: a long history lists the newest first and the older ones on asking", async () => {
  const vault = createTempVault({ "long.md": "# long\n\nstate 0\n" });

  try {
    await loadVault(page, vault);
    // 30 states: more than one page (25) of the list.
    for (let n = 0; n < 30; n++) {
      writeVaultFile(vault, "long.md", `# long\n\nstate ${n}\n`);
      await recordNotes(page, vault, ["long.md"]);
    }
    await page.getByRole("treeitem", { name: /long/ }).click();
    await expect(page.locator(".cm-content")).toBeVisible();
    await runCommand(page, "Version history");

    const entries = page.getByTestId("version-entry");
    await expect(entries).toHaveCount(25);
    const older = page.getByTestId("version-older");
    await expect(older).toHaveText("Show older versions");
    await older.click();
    await expect(entries).toHaveCount(30);
    await expect(older).toHaveCount(0); // the list reaches the first one

    // The oldest is the first state, and it can still be looked at.
    await entries.last().click();
    await expect(page.getByTestId("version-preview")).toContainText("state 0");
  } finally {
    removeTempVault(vault);
  }
});
