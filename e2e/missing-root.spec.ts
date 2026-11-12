import { test, expect, type Browser, type Page } from "@playwright/test";
import { existsSync, renameSync } from "node:fs";
import {
  answerFolderDialog,
  connectToApp,
  createTempVault,
  dialogsAsked,
  loadVault,
  removeTempVault,
  tauriInvoke,
} from "./helpers";

// A notes folder of the sidebar that is not there any more — moved, renamed, or on a drive that is
// not connected — stays in the sidebar where it was, says so, and can be pointed at where it is now.

let browser: Browser;
let page: Page;

test.beforeAll(async () => {
  ({ browser, page } = await connectToApp());
});

test.afterAll(async () => {
  await browser?.close();
});

type Bridge = {
  addRoot: (p: string) => Promise<boolean>;
  openList: () => Promise<void>;
  retryMissing: () => Promise<void>;
};

function bridge<K extends keyof Bridge>(name: K, ...args: Parameters<Bridge[K]>): Promise<Awaited<ReturnType<Bridge[K]>>> {
  return page.evaluate(
    ([n, a]) => {
      const b = (window as unknown as { __textreeTest: Record<string, (...x: unknown[]) => unknown> }).__textreeTest;
      return b[n as string](...(a as unknown[]));
    },
    [name, args] as const,
  ) as Promise<Awaited<ReturnType<Bridge[K]>>>;
}

type Listed = { roots: { id: string; path: string }[] };

async function idOf(path: string): Promise<string> {
  const ws = await tauriInvoke<Listed>(page, "workspace_list");
  return ws.roots.find((r) => r.path === path)!.id;
}

/** Moves a folder the window just let go of — on Windows its watch may take a moment to close. */
async function moveFolder(from: string, to: string): Promise<void> {
  for (let i = 0; ; i++) {
    try {
      renameSync(from, to);
      return;
    } catch (e) {
      if (i >= 30) throw e;
      await new Promise((r) => setTimeout(r, 100));
    }
  }
}

function section(name: RegExp) {
  return page.getByRole("region", { name });
}

test("a folder that was moved is shown as not found, and Locate… brings it back with its name", async () => {
  const main = createTempVault({ "main.md": "# main\n" });
  const away = createTempVault({ "field-note.md": "# field note\n" });
  const moved = `${away}-moved`;
  try {
    await loadVault(page, main);
    expect(await bridge("addRoot", away)).toBe(true);
    await tauriInvoke(page, "workspace_relabel", { id: await idOf(away), label: "Field notes" });
    await loadVault(page, main); // the other folder is let go, as when the app was closed
    await moveFolder(away, moved);

    await bridge("openList");
    const gone = section(/Field notes/);
    await expect(gone).toContainText("Notes folder not found");

    // Through the button and the dialog it opens; the dialog is answered with where it is now.
    const asked = dialogsAsked().length;
    answerFolderDialog(moved);
    await gone.getByRole("button", { name: "Locate…" }).first().click();
    const back = section(/Field notes/);
    await expect(back.getByRole("treeitem", { name: /field note/ })).toBeVisible();
    await expect(back).not.toContainText("Notes folder not found");
    const listed = await tauriInvoke<Listed>(page, "workspace_list");
    expect(listed.roots.some((r) => r.path === away)).toBe(false);
    expect(listed.roots.some((r) => r.path === moved)).toBe(true);
    expect(dialogsAsked().slice(asked)).toEqual(["Locate Field notes"]);
  } finally {
    await tauriInvoke(page, "workspace_remove", { id: await idOf(moved) }).catch(() => {});
    await loadVault(page, main);
    removeTempVault(main);
    removeTempVault(existsSync(moved) ? moved : away);
  }
});

test("a folder put back is found again, and one taken out of the sidebar is left on disk", async () => {
  const main = createTempVault({ "main.md": "# main\n" });
  const drive = createTempVault({ "on-drive.md": "# on drive\n" });
  const elsewhere = `${drive}-unplugged`;
  const name = new RegExp(drive.split(/[/\\]/).pop()!);
  try {
    await loadVault(page, main);
    expect(await bridge("addRoot", drive)).toBe(true);
    await loadVault(page, main);
    await moveFolder(drive, elsewhere);
    await bridge("openList");
    await expect(section(name)).toContainText("Notes folder not found");

    await moveFolder(elsewhere, drive); // the drive is connected again
    await bridge("retryMissing");
    await expect(section(name).getByRole("treeitem", { name: /on drive/ })).toBeVisible();

    await loadVault(page, main);
    await moveFolder(drive, elsewhere);
    await bridge("openList");
    await section(name).getByRole("button", { name: "Remove from sidebar" }).click();
    await expect(section(name)).toHaveCount(0);
    expect(existsSync(elsewhere)).toBe(true);
    const listed = await tauriInvoke<Listed>(page, "workspace_list");
    expect(listed.roots.some((r) => r.path === drive)).toBe(false);
  } finally {
    await loadVault(page, main);
    removeTempVault(main);
    removeTempVault(existsSync(drive) ? drive : elsewhere);
  }
});
