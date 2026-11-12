import { test, expect, type Browser, type Page } from "@playwright/test";
import { execFileSync } from "node:child_process";
import {
  connectToApp,
  createTempVault,
  expectOpenNote,
  loadVault,
  removeTempVault,
  rootIdOf,
  tauriInvoke,
  addVersion,
} from "./helpers";
import { gitAvailable, startTestRemote } from "./git-remote";

/**
 * Two notes folders inside one repository, side by side in the sidebar. They record into that
 * repository, so they share its backup: connecting one connects the other, and a version added
 * in either reaches the same remote.
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

async function addRoot(path: string): Promise<boolean> {
  return page.evaluate(
    (p) =>
      (window as unknown as { __textreeTest: { addRoot: (p: string) => Promise<boolean> } }).__textreeTest.addRoot(p),
    path,
  );
}

test("folders of one repository share its backup", async () => {
  test.setTimeout(90_000);
  const remote = await startTestRemote();
  const work = createTempVault({
    "journal/day.md": "# day\n\nin the journal\n",
    "manual/howto.md": "# howto\n\nin the manual\n",
  });
  execFileSync("git", ["init", "-q", work]);
  const journal = `${work}/journal`;
  const manual = `${work}/manual`;
  const indicator = page.getByTestId("backup-status");
  const dialog = page.getByTestId("backup-dialog");
  const versions = () => Number(remote.git("rev-list", "--count", "refs/textree/notes") || "0");

  try {
    await loadVault(page, journal);
    expect(await addRoot(manual)).toBe(true);
    const journalSection = page.getByRole("region", { name: /journal/ });
    const manualSection = page.getByRole("region", { name: /manual/ });

    // ── Journal: a version, then connect.
    await journalSection.getByRole("treeitem", { name: /day/ }).click();
    await expectOpenNote(page, "day");
    await addVersion(page);
    await expect(indicator).toHaveText("Not backed up");
    await indicator.click();
    await page.getByTestId("backup-url").fill(remote.url);
    await page.getByTestId("backup-token").fill(remote.token);
    await page.getByTestId("backup-connect").click();
    await expect(page.getByTestId("backup-outcome")).toHaveText("Everything is backed up.");
    await dialog.getByRole("button", { name: "Close" }).click();
    expect(versions()).toBe(1);

    // ── Manual: already connected — the same repository, the same remote.
    await manualSection.getByRole("treeitem", { name: /howto/ }).click();
    await expectOpenNote(page, "howto");
    await expect(indicator).toHaveCount(0);
    await addVersion(page);
    await expect.poll(versions, { timeout: 30_000 }).toBe(2);
    await expect(indicator).toHaveCount(0);

    // ── Stopping the backup from one stops it for both.
    await tauriInvoke(page, "disconnect_remote", { rootId: await rootIdOf(page, manual) });
    await journalSection.getByRole("treeitem", { name: /day/ }).click();
    await expectOpenNote(page, "day");
    await expect(indicator).toHaveText("Not backed up");
  } finally {
    await tauriInvoke(page, "disconnect_remote", { rootId: await rootIdOf(page, journal) }).catch(() => {});
    await loadVault(page, journal).catch(() => {}); // alone again, for the specs after this one
    removeTempVault(work);
  }
});
