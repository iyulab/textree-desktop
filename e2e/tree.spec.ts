import { test, expect, type Browser, type Page } from "@playwright/test";
import { connectToApp, loadVault, createTempVault, removeTempVault } from "./helpers";
import { existsSync } from "node:fs";
import { join } from "node:path";

/**
 * Tree expand/collapse. Toggle child visibility via the container's chevron
 * and assert in a real webview that the state persists to localStorage.
 */

let browser: Browser;
let page: Page;

test.beforeAll(async () => {
  ({ browser, page } = await connectToApp());
});

test.afterAll(async () => {
  await browser?.close();
});

test("expand/collapse children via container chevron + persist", async () => {
  const vault = createTempVault({
    "folder/child1.md": "a\n",
    "folder/child2.md": "b\n",
  });
  try {
    await loadVault(page, vault);

    // Default: expanded → children visible.
    await expect(page.getByRole("treeitem", { name: /child1/ })).toBeVisible();
    await expect(page.getByRole("treeitem", { name: /child2/ })).toBeVisible();

    // Collapse → children disappear. (exact: distinguish from the "Collapse sidebar" button)
    await page.getByRole("button", { name: "Collapse", exact: true }).click();
    await expect(page.getByRole("treeitem", { name: /child1/ })).toHaveCount(0);
    await expect(page.getByRole("treeitem", { name: /child2/ })).toHaveCount(0);

    // Collapsed state persists (1 path stored, in this vault's own list — earlier specs leave
    // lists of their own folders behind, so the key is picked by this vault's folder name).
    const folderName = vault.split(/[\\/]/).pop() ?? vault;
    const lists = await page.evaluate(() =>
      Object.keys(localStorage)
        .filter((k) => k.startsWith("textree-tree-collapsed:"))
        .map((k) => [k, localStorage.getItem(k)] as const),
    );
    // Keys name the folder by its path as the device compares paths — folded to lower case on
    // Windows — so the folder name is matched the same way.
    const mine = lists.find(([k]) => k.toLowerCase().endsWith(folderName.toLowerCase()));
    expect(mine ? JSON.parse(mine[1] ?? "[]") : [], JSON.stringify(lists)).toEqual(["folder"]);

    // Expand → children return.
    await page.getByRole("button", { name: "Expand", exact: true }).click();
    await expect(page.getByRole("treeitem", { name: /child1/ })).toBeVisible();
  } finally {
    removeTempVault(vault);
  }
});

test("keyboard: ↑↓ move · ←→ collapse/expand · Enter open", async () => {
  const vault = createTempVault({
    "folder/child1.md": "a\n",
    "folder/child2.md": "b\n",
  });
  try {
    await loadVault(page, vault);

    const folder = page.getByRole("treeitem", { name: /folder/ });
    await folder.focus();

    // ← collapse: hide children.
    await page.keyboard.press("ArrowLeft");
    await expect(page.getByRole("treeitem", { name: /child1/ })).toHaveCount(0);

    // → expand: children return.
    await page.keyboard.press("ArrowRight");
    await expect(page.getByRole("treeitem", { name: /child1/ })).toBeVisible();

    // ↓ move: focus moves to the next item (child1).
    await page.keyboard.press("ArrowDown");
    const focusedText = await page.evaluate(
      () => document.activeElement?.textContent ?? "",
    );
    expect(focusedText).toContain("child1");

    // Enter: open the focused note.
    await page.keyboard.press("Enter");
    await expect(page.locator(".note-name")).toHaveText("child1");
  } finally {
    removeTempVault(vault);
  }
});

test("keyboard: F2 inline rename · Delete delete", async () => {
  const vault = createTempVault({ "doomed-note.md": "x\n", "keep-note.md": "y\n" });
  try {
    await loadVault(page, vault);

    // F2 → inline rename input appears in the tree (no dialog).
    await page.getByRole("treeitem", { name: /doomed-note/ }).focus();
    await page.keyboard.press("F2");
    await expect(page.locator(".tree-rename-input")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator(".tree-rename-input")).toHaveCount(0);
    // Escape leaves the name unchanged.
    await expect(page.getByRole("treeitem", { name: /doomed-note/ })).toBeVisible();

    // Delete → the note leaves the folder (disappears from tree).
    await page.getByRole("treeitem", { name: /doomed-note/ }).focus();
    await page.keyboard.press("Delete");
    await expect(page.getByRole("treeitem", { name: /doomed-note/ })).toHaveCount(0);
    await expect(page.getByRole("treeitem", { name: /keep-note/ })).toBeVisible();
  } finally {
    removeTempVault(vault);
  }
});

test("keyboard move: Ctrl+X cut → Ctrl+V move into folder", async () => {
  const vault = createTempVault({
    "move-note.md": "a\n",
    "target-folder/existing.md": "b\n",
  });
  try {
    await loadVault(page, vault);

    await page.getByRole("treeitem", { name: /move-note/ }).focus();
    await page.keyboard.press("Control+x");
    await page.getByRole("treeitem", { name: /target-folder/ }).focus();
    await page.keyboard.press("Control+v");

    // Disk: moved into target-folder.
    await expect
      .poll(() => existsSync(join(vault, "target-folder", "move-note.md")))
      .toBe(true);
    expect(existsSync(join(vault, "move-note.md"))).toBe(false);
  } finally {
    removeTempVault(vault);
  }
});

test("keyboard adopt: Ctrl+V on leaf → promote then child", async () => {
  const vault = createTempVault({ "source.md": "a\n", "leaf.md": "b\n" });
  try {
    await loadVault(page, vault);

    await page.getByRole("treeitem", { name: /source/ }).focus();
    await page.keyboard.press("Control+x");
    await page.getByRole("treeitem", { name: /leaf/ }).focus();
    await page.keyboard.press("Control+v");

    // leaf is promoted to a container and source moves into it (adopt).
    await expect.poll(() => existsSync(join(vault, "leaf", "source.md"))).toBe(true);
  } finally {
    removeTempVault(vault);
  }
});

test("breadcrumb: show ancestor folder of a nested note", async () => {
  const vault = createTempVault({ "folder/child1.md": "a\n" });
  try {
    await loadVault(page, vault);

    await page.getByRole("treeitem", { name: /child1/ }).click();
    // Body header shows the ancestor folder (folder) + note name (child1).
    await expect(page.locator(".crumb")).toHaveText("folder");
    await expect(page.locator(".note-name")).toHaveText("child1");
  } finally {
    removeTempVault(vault);
  }
});
