import { existsSync } from "node:fs";
import { join } from "node:path";
import { test, expect, type Browser, type Page } from "@playwright/test";
import {
  connectToApp,
  loadVault,
  createTempVault,
  removeTempVault,
  stallOpening,
  releaseOpening,
  tauriInvoke,
} from "./helpers";

// A folder that does not answer when opened — a disconnected network or sync drive. The window must
// not wait on it, and opening another folder instead is how the person gets out.

let browser: Browser;
let page: Page;

test.beforeAll(async () => {
  ({ browser, page } = await connectToApp());
});

test.afterAll(async () => {
  releaseOpening();
  await browser?.close();
});

test("a folder that does not answer when opened: another one can be opened meanwhile, and it stays", async () => {
  const slow = createTempVault({ "slow-note.md": "# slow\n" });
  const quick = createTempVault({ "quick-note.md": "# quick\n" });
  try {
    stallOpening(slow);
    // Not awaited: this opening hangs until released.
    const hung = page.evaluate(
      (v) =>
        (window as unknown as { __textreeTest: { loadVault: (p: string) => Promise<boolean> } }).__textreeTest.loadVault(v),
      slow,
    );
    await page.waitForTimeout(500);

    // With the opening held on the main thread this would wait for it — and the test would never
    // release it, so it would time out here.
    await loadVault(page, quick);
    await expect(page.getByRole("treeitem", { name: /quick-note/ })).toBeVisible();

    releaseOpening();
    expect(await hung, "the overtaken opening reports that it did not switch").toBe(false);
    // Nothing of the slow folder arrives after the fact.
    await page.waitForTimeout(500);
    await expect(page.getByRole("treeitem", { name: /quick-note/ })).toBeVisible();
    await expect(page.getByRole("treeitem", { name: /slow-note/ })).toHaveCount(0);
  } finally {
    releaseOpening();
    removeTempVault(slow);
    removeTempVault(quick);
  }
});

test("a folder of the sidebar that does not answer says it is opening, can be stopped, and the others open", async () => {
  const main = createTempVault({ "main-note.md": "# main\n" });
  // What an earlier release kept inside the folder: carried out of it once the folder is read.
  const slow = createTempVault({ "slow-note.md": "# slow\n", ".textree/favorites.json": '["slow-note.md"]' });
  const after = createTempVault({ "after-note.md": "# after\n" });
  const name = (p: string) => new RegExp(p.split(/[/\\]/).pop()!);
  const section = (p: string) => page.getByRole("region", { name: name(p) });
  const bridge = (n: string) =>
    page.evaluate((x) => (window as unknown as { __textreeTest: Record<string, () => Promise<void>> }).__textreeTest[x](), n);
  try {
    await loadVault(page, main);
    await tauriInvoke(page, "workspace_add", { path: slow });
    await tauriInvoke(page, "workspace_add", { path: after });
    stallOpening(slow);
    const listing = bridge("openList"); // not awaited: the slow folder holds it until stopped

    await expect(section(slow)).toContainText("Opening…");
    await section(slow).getByRole("button", { name: "Stop" }).click();
    await expect(section(slow)).toContainText("Not opened");
    await listing;
    await expect(section(after).getByRole("treeitem", { name: /after-note/ })).toBeVisible();

    // Opened again while the stopped opening still hangs: the stopped one answers after the new one
    // started, and must neither undo it nor leave the folder unread.
    await section(slow).getByRole("button", { name: "Open" }).click();
    await expect(section(slow)).toContainText("Opening…");
    releaseOpening();
    await expect(section(slow).getByRole("treeitem", { name: /slow-note/ })).toBeVisible();
    await expect.poll(() => existsSync(join(slow, ".textree"))).toBe(false);
  } finally {
    releaseOpening();
    for (const p of [slow, after]) {
      const ws = await tauriInvoke<{ roots: { id: string; path: string }[] }>(page, "workspace_list");
      const r = ws.roots.find((x) => x.path === p);
      if (r) await tauriInvoke(page, "workspace_remove", { id: r.id }).catch(() => {});
    }
    await loadVault(page, main);
    removeTempVault(main);
    removeTempVault(slow);
    removeTempVault(after);
  }
});
