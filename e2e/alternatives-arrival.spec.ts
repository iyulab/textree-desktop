import { test, expect, type Browser, type Page } from "@playwright/test";
import {
  connectToApp,
  loadVault,
  createTempVault,
  removeTempVault,
  readVaultFile,
  expectOpenNote,
  writeVaultFile,
  tauriInvoke,
  rootIdOf,
  addVersion,
  refsUnder,
} from "./helpers";
import { gitAvailable, startTestRemote, type TestRemote } from "./git-remote";

/**
 * A note changed in two folders that share a repository. The folder that hears about the other
 * change last keeps it as an alternative instead of combining the two, says so on the note, and
 * lets a person decide. The decision then reaches the other folder without asking there again.
 *
 * The repository is a real one on this machine, served over HTTP by `git http-backend` (see
 * git-remote.ts), so every exchange goes through the app's own transport.
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

/** Connects the open folder to `remote` and waits for the first exchange to finish. */
async function connect(p: Page, remote: TestRemote): Promise<void> {
  await p.getByTestId("backup-status").click();
  const dialog = p.getByTestId("backup-dialog");
  await expect(dialog).toBeVisible();
  await p.getByTestId("backup-url").fill(remote.url);
  await p.getByTestId("backup-token").fill(remote.token);
  await p.getByTestId("backup-connect").click();
  await expect(p.getByTestId("backup-last")).toContainText("Last synced just now");
  await p.keyboard.press("Escape");
  await expect(dialog).toHaveCount(0);
}

/** Replaces the open note's text, waits for it to be saved, and adds it as a version. */
async function rewriteAndAdd(p: Page, vault: string, rel: string, text: string, marker: string): Promise<void> {
  await p.locator(".cm-content").click();
  await p.keyboard.press("Control+a");
  await p.keyboard.insertText(text);
  await expect.poll(() => readVaultFile(vault, rel), { timeout: 5000 }).toContain(marker);
  await addVersion(p);
}

function noteInRemote(remote: TestRemote, rel: string): string {
  try {
    return remote.git("show", `refs/textree/notes:${rel}`);
  } catch {
    return "";
  }
}

function readIfThere(vault: string, rel: string): string {
  try {
    return readVaultFile(vault, rel);
  } catch {
    return "";
  }
}

test("alternatives: a note changed in two folders arrives as an alternative, and the decision travels", async () => {
  test.setTimeout(120_000);
  const remote = await startTestRemote();
  const first = createTempVault({ "plan.md": "# Plan\n\nWe meet on Monday.\n" });
  const second = createTempVault({ "other.md": "# Other\n" });
  const notice = page.getByTestId("alternative-notice");

  try {
    // Both folders start from the same recorded note.
    await loadVault(page, first);
    await page.getByRole("treeitem", { name: /plan/ }).click();
    await expectOpenNote(page, "plan");
    await addVersion(page);
    await connect(page, remote);

    await loadVault(page, second);
    await page.getByRole("treeitem", { name: /other/ }).click(); // the indicator sits with an open note
    await expectOpenNote(page, "other");
    await connect(page, remote);
    await expect.poll(() => readIfThere(second, "plan.md"), { timeout: 30_000 }).toContain("Monday");

    // The second folder changes it and its version reaches the repository.
    await page.getByRole("treeitem", { name: /plan/ }).click();
    await expectOpenNote(page, "plan");
    await rewriteAndAdd(page, second, "plan.md", "# Plan\n\nWe meet on Tuesday.\n", "Tuesday");
    await expect.poll(() => noteInRemote(remote, "plan.md"), { timeout: 30_000 }).toContain("Tuesday");

    // The first folder changed it too, while it was not open — as a computer that was offline
    // would. Opening it exchanges, and the version from elsewhere is kept as an alternative.
    writeVaultFile(first, "plan.md", "# Plan\n\nWe meet on Wednesday.\n");
    await tauriInvoke(page, "commit_notes", { rootId: await rootIdOf(page, first), rels: ["plan.md"], message: "plan" });
    await loadVault(page, first);
    await page.getByRole("treeitem", { name: /plan/ }).click();
    await expectOpenNote(page, "plan");
    await expect(notice).toContainText("This note also changed elsewhere.", { timeout: 30_000 });
    // Nothing was combined: the file is what was written here.
    expect(readVaultFile(first, "plan.md")).toContain("Wednesday");
    expect(readVaultFile(first, "plan.md")).not.toContain("Tuesday");

    // Compare shows where it came from and what differs, word by word.
    await page.getByTestId("alternative-compare").click();
    await expect(page.getByTestId("alternatives")).toContainText("The alternative is from");
    await expect(page.getByTestId("alternative-current").locator("del")).toHaveText(["Wednesday"]);
    await expect(page.getByTestId("alternative-text").locator("ins")).toHaveText(["Tuesday"]);

    // Keep this folder's version. The other is kept as ended, and this folder joins up.
    await page.getByTestId("alternative-use-current").click();
    await expect(page.getByTestId("alternatives")).toHaveCount(0);
    await expect(notice).toHaveCount(0);
    expect(readVaultFile(first, "plan.md")).toContain("Wednesday");
    expect(refsUnder(first, "refs/textree/ended/")).toHaveLength(1);
    await expect.poll(() => noteInRemote(remote, "plan.md"), { timeout: 30_000 }).toContain("Wednesday");

    // The second folder takes the decision in: no question there, the note is what was kept.
    await loadVault(page, second);
    await expect.poll(() => readVaultFile(second, "plan.md"), { timeout: 30_000 }).toContain("Wednesday");
    await page.getByRole("treeitem", { name: /plan/ }).click();
    await expectOpenNote(page, "plan");
    await expect(page.locator(".cm-content")).toContainText("Wednesday");
    await expect(notice).toHaveCount(0);
    expect(refsUnder(second, "refs/textree/alternatives/")).toEqual([]);
  } finally {
    for (const vault of [first, second]) {
      await tauriInvoke(page, "disconnect_remote", { rootId: await rootIdOf(page, vault) }).catch(() => {});
    }
    // The folder left open may still be finishing its exchange; removing it waits for that.
    for (const vault of [first, second]) {
      await expect
        .poll(
          () => {
            try {
              removeTempVault(vault);
              return true;
            } catch {
              return false;
            }
          },
          { timeout: 15_000 },
        )
        .toBe(true);
    }
    await remote.close();
  }
});
