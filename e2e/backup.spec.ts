import { test, expect, type Browser, type Page } from "@playwright/test";
import {
  connectToApp,
  loadVault,
  createTempVault,
  removeTempVault,
  readVaultFile,
  expectOpenNote,
  tauriInvoke,
  rootIdOf,
  addVersion,
} from "./helpers";
import { gitAvailable, startTestRemote, type TestRemote } from "./git-remote";

/**
 * Backing notes up to a repository, from the "Not backed up" indicator to a second folder that
 * receives them, and new versions reaching the repository without anyone asking.
 *
 * The repository is a real one on this machine, served over HTTP with Basic authentication by
 * `git http-backend` (see git-remote.ts), so every exchange goes through the app's own transport.
 */

let browser: Browser;
let page: Page;

test.skip(!gitAvailable(), "git is not installed");

test.beforeAll(async () => {
  ({ browser, page } = await connectToApp());
});

test.afterAll(async () => {
  await browser?.close();
});

/** From the indicator, connect the open folder to `remote` with `token`. */
async function connectFromIndicator(p: Page, remote: TestRemote, token: string): Promise<void> {
  const dialog = p.getByTestId("backup-dialog");
  if ((await dialog.count()) === 0) {
    await p.getByTestId("backup-status").click();
    await expect(dialog).toBeVisible();
  }
  await expect(p.getByTestId("backup-url")).toBeFocused();
  await p.getByTestId("backup-url").fill(remote.url);
  await p.getByTestId("backup-token").fill(token);
  await p.getByTestId("backup-connect").click();
}

function versionsInRemote(remote: TestRemote): number {
  return Number(remote.git("rev-list", "--count", "refs/textree/notes") || "0");
}

test("backup: connect from the indicator, receive in another folder, send new versions in the background", async () => {
  test.setTimeout(90_000);
  const remote = await startTestRemote();
  const first = createTempVault({ "memo.md": "# memo\n\nwritten in the first folder\n" });
  const second = createTempVault({ "other.md": "# other\n\nwritten in the second folder\n" });
  const indicator = page.getByTestId("backup-status");
  const dialog = page.getByTestId("backup-dialog");

  try {
    // ── First folder: a recorded note, not backed up yet.
    await loadVault(page, first);
    await page.getByRole("treeitem", { name: /memo/ }).click();
    await expectOpenNote(page, "memo");
    await addVersion(page);
    await expect(indicator).toHaveText("Not backed up");
    await expect(indicator).toHaveAttribute("title", /only on this computer/);

    // The indicator is a button: it opens the dialog from the keyboard too.
    await indicator.focus();
    await page.keyboard.press("Enter");
    await expect(dialog).toBeVisible();
    await expect(dialog.getByRole("heading", { name: "Back up your notes" })).toBeVisible();

    // A wrong token is refused, said plainly, and nothing is kept.
    await connectFromIndicator(page, remote, "wrong-token");
    await expect(page.getByTestId("backup-error")).toContainText("access token");
    await expect(page.getByTestId("backup-url")).toBeVisible();
    await expect(page.getByTestId("backup-where")).toHaveCount(0);
    await expect(dialog).not.toContainText(/remote|push|commit|branch|merge/i);

    // The right one connects, exchanges at once, and the notes are backed up.
    await page.getByTestId("backup-token").fill(remote.token);
    await page.getByTestId("backup-connect").click();
    await expect(page.getByTestId("backup-where")).toContainText("127.0.0.1");
    await expect(page.getByTestId("backup-where")).toContainText("/repo");
    await expect(page.getByTestId("backup-last")).toContainText("Last synced just now");
    await expect(page.getByTestId("backup-outcome")).toHaveText("Everything is backed up.");
    expect(versionsInRemote(remote)).toBe(1);
    await expect(indicator).toHaveCount(0);
    await dialog.getByRole("button", { name: "Close" }).click();
    await expect(dialog).toHaveCount(0);

    // ── Second folder: connecting brings the first folder's note in.
    await loadVault(page, second);
    await page.getByRole("treeitem", { name: /other/ }).click();
    await expectOpenNote(page, "other");
    await expect(indicator).toHaveText("Not backed up");
    await connectFromIndicator(page, remote, remote.token);
    await expect(page.getByTestId("backup-where")).toBeVisible();
    await expect
      .poll(() => {
        try {
          return readVaultFile(second, "memo.md");
        } catch {
          return "";
        }
      })
      .toContain("written in the first folder");
    await expect(page.getByTestId("backup-notice")).toHaveText(/1 note arrived from your backup\./);
    await page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0);
    await expect(page.getByRole("treeitem", { name: /memo/ })).toBeVisible();
    await expect(indicator).toHaveCount(0);

    // ── First folder again: a new version reaches the repository without anything pressed.
    await loadVault(page, first);
    await page.getByRole("treeitem", { name: /memo/ }).click();
    await expectOpenNote(page, "memo");
    await expect(indicator).toHaveCount(0);
    await page.locator(".cm-content").click();
    await page.keyboard.press("Control+End");
    await page.keyboard.type("\nsecond thought");
    await expect.poll(() => readVaultFile(first, "memo.md"), { timeout: 5000 }).toContain("second thought");
    await addVersion(page);
    await expect.poll(() => versionsInRemote(remote), { timeout: 30_000 }).toBe(2);
    await expect(indicator).toHaveCount(0);

    // ── Settings shows the same backup, and stops it after an inline confirmation.
    await page.keyboard.press("Control+,");
    const settings = page.getByRole("dialog", { name: "Settings" });
    await expect(settings).toBeVisible();
    const section = page.getByTestId("settings-backup");
    await expect(section.getByTestId("backup-where")).toContainText("127.0.0.1");
    await section.getByTestId("backup-sync-now").click();
    await expect(section.getByTestId("backup-outcome")).toHaveText("Everything is backed up.");
    await section.getByTestId("backup-disconnect").click();
    await section.getByTestId("backup-disconnect-confirm").click();
    await expect(section.getByTestId("backup-url")).toBeVisible();
    await settings.getByRole("button", { name: "Close settings" }).click();
    await expect(settings).toHaveCount(0);
    await expect(indicator).toHaveText("Not backed up");
  } finally {
    // The access token is kept in the system's credential store; leave none behind.
    for (const vault of [first, second]) {
      await tauriInvoke(page, "disconnect_remote", { rootId: await rootIdOf(page, vault) }).catch(() => {});
    }
    removeTempVault(first);
    removeTempVault(second);
    await remote.close();
  }
});

test("backup: a repository that cannot be reached is said once, not every time the folder opens", async () => {
  test.setTimeout(90_000);
  const remote = await startTestRemote();
  const vault = createTempVault({ "memo.md": "# memo\n\nkept here\n" });
  const elsewhere = createTempVault({ "other.md": "# other\n" });
  const notice = page.getByTestId("backup-notice");

  try {
    await loadVault(page, vault);
    await page.getByRole("treeitem", { name: /memo/ }).click();
    await expectOpenNote(page, "memo");
    await connectFromIndicator(page, remote, remote.token);
    await expect(page.getByTestId("backup-where")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("backup-dialog")).toHaveCount(0);

    // The repository goes away (offline, server down): opening the folder says so.
    await remote.close();
    await loadVault(page, elsewhere);
    await loadVault(page, vault);
    await expect(notice).toContainText("Couldn't back up", { timeout: 30_000 });
    await notice.getByRole("button", { name: "Dismiss" }).click();
    await expect(notice).toHaveCount(0);

    // Opening it again fails the same way: nothing new to say.
    await loadVault(page, elsewhere);
    await loadVault(page, vault);
    // An exchange asked for now runs after the background one, so once it is done both are.
    await page.keyboard.press("Control+,");
    const settings = page.getByRole("dialog", { name: "Settings" });
    const section = page.getByTestId("settings-backup");
    await section.getByTestId("backup-sync-now").click();
    await expect(section.getByTestId("backup-sync-now")).toBeEnabled({ timeout: 30_000 });
    await expect(section.getByTestId("backup-error")).toBeVisible();
    await settings.getByRole("button", { name: "Close settings" }).click();
    await expect(settings).toHaveCount(0);
    await expect(notice).toHaveCount(0);
  } finally {
    await tauriInvoke(page, "disconnect_remote", { rootId: await rootIdOf(page, vault) }).catch(() => {});
    removeTempVault(vault);
    removeTempVault(elsewhere);
    await remote.close().catch(() => {});
  }
});
