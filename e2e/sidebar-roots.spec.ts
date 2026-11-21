import { test, expect, type Browser, type Page } from "@playwright/test";
import {
  connectToApp,
  createTempVault,
  dragNodeOnto,
  expectOpenNote,
  listVaultDir,
  loadVault,
  removeTempVault,
  tauriInvoke,
  answerFolderDialog,
  dialogsAsked,
  rootIdOf,
} from "./helpers";

// Several notes folders in the sidebar at once. Each keeps its own notes; working in one makes it
// the folder the window is in; a note never moves from one to another.

let browser: Browser;
let page: Page;

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

function section(name: RegExp) {
  return page.getByRole("region", { name });
}

test("two notes folders side by side: a note of the same name opens from its own folder", async () => {
  const work = createTempVault({ "plan.md": "# plan\n\nwork plan\n", "only-work.md": "# w\n" });
  const home = createTempVault({ "plan.md": "# plan\n\nhome plan\n" });
  try {
    await loadVault(page, work);
    expect(await addRoot(home)).toBe(true);
    const workName = new RegExp(work.split("/").pop()!);
    const homeName = new RegExp(home.split("/").pop()!);
    await expect(section(workName)).toBeVisible();
    await expect(section(homeName)).toBeVisible();

    await section(homeName).getByRole("treeitem", { name: /plan/ }).click();
    await expectOpenNote(page, "plan");
    await expect(page.locator(".cm-content")).toContainText("home plan");

    await section(workName).getByRole("treeitem", { name: /plan/ }).click();
    await expect(page.locator(".cm-content")).toContainText("work plan");

    // Content search looks in every notes folder the sidebar shows, and each hit names its folder.
    const workLabel = work.split("/").pop()!;
    const homeLabel = home.split("/").pop()!;
    await page.keyboard.press("Control+p");
    await page.getByTestId("palette-input").fill("/plan");
    const scope = page.getByTestId("palette-scope");
    await expect(scope.getByRole("radio", { name: "All notes folders" })).toHaveAttribute("aria-checked", "true");
    const items = page.getByTestId("palette-item");
    await expect(items).toHaveCount(2);
    await expect(items.filter({ hasText: homeLabel })).toHaveCount(1);
    await expect(items.filter({ hasText: workLabel })).toHaveCount(1);

    // Tab: only the folder the window is in.
    await page.keyboard.press("Tab");
    await expect(scope.getByRole("radio", { name: "This notes folder" })).toHaveAttribute("aria-checked", "true");
    await expect(items).toHaveCount(1);
    await expect(items.first()).toContainText(workLabel);
    await page.getByTestId("palette-input").fill("/");
    await expect(page.getByTestId("palette-status")).toHaveText(`Searches the notes in ${workLabel} only`);

    // Back to all of them; a hit in the other folder opens there.
    await page.keyboard.press("Tab");
    await page.getByTestId("palette-input").fill("/home plan");
    await expect(items.filter({ hasText: homeLabel }).first()).toBeVisible();
    await items.filter({ hasText: homeLabel }).first().dispatchEvent("mousedown");
    await expect(page.getByTestId("palette-overlay")).toHaveCount(0);
    await expect(page.locator(".cm-content")).toContainText("home plan");
  } finally {
    await loadVault(page, work); // alone again, for the specs after this one
    removeTempVault(work);
    removeTempVault(home);
  }
});

test("a note dragged onto another notes folder stays where it is, and the sidebar says why", async () => {
  const a = createTempVault({ "keep.md": "# keep\n", "box/inside.md": "# inside\n" });
  const b = createTempVault({ "dest/there.md": "# there\n" });
  try {
    await loadVault(page, a);
    expect(await addRoot(b)).toBe(true);
    const aName = new RegExp(a.split("/").pop()!);
    const bName = new RegExp(b.split("/").pop()!);
    await dragNodeOnto(
      page,
      section(aName).getByRole("treeitem", { name: /keep/ }),
      section(bName).getByRole("treeitem", { name: /dest/ }),
    );
    await expect(page.locator(".op-error")).toContainText("Notes can't be moved to another notes folder.");
    expect(listVaultDir(a, "")).toContain("keep.md");
    expect(listVaultDir(b, "dest")).toEqual(["there.md"]);
  } finally {
    await loadVault(page, a);
    removeTempVault(a);
    removeTempVault(b);
  }
});

test("taking a notes folder out of the sidebar leaves the folder and its notes where they are", async () => {
  const a = createTempVault({ "a.md": "# a\n" });
  const b = createTempVault({ "b.md": "# b\n" });
  try {
    await loadVault(page, a);
    expect(await addRoot(b)).toBe(true);
    const bName = new RegExp(b.split("/").pop()!);
    await section(bName).getByRole("button", { name: /Notes folder menu/ }).click();
    await page.getByRole("menuitem", { name: "Remove from sidebar" }).click();
    await expect(section(bName)).toHaveCount(0);
    expect(listVaultDir(b, "")).toContain("b.md");
    // One folder left: the sidebar shows it as it always did, without a name above it.
    await expect(page.getByRole("treeitem", { name: /^a/ })).toBeVisible();
  } finally {
    await loadVault(page, a);
    removeTempVault(a);
    removeTempVault(b);
  }
});

test("a notes folder is named and moved in the sidebar, and the folder on disk keeps its name", async () => {
  const a = createTempVault({ "a.md": "# a\n" });
  const b = createTempVault({ "b.md": "# b\n" });
  try {
    await loadVault(page, a);
    await tauriInvoke(page, "workspace_add", { path: a }); // listed first, as a folder opened before
    expect(await addRoot(b)).toBe(true);
    const bName = new RegExp(b.split("/").pop()!);
    await section(bName).getByRole("button", { name: /Notes folder menu/ }).click();
    await page.getByRole("menuitem", { name: "Rename in sidebar…" }).click();
    const input = page.getByRole("textbox", { name: "Name in sidebar" });
    await input.fill("Home notes");
    await input.press("Enter");
    await expect(section(/Home notes/)).toBeVisible();
    expect(listVaultDir(b, "")).toContain("b.md"); // the folder itself is as it was

    await section(/Home notes/).getByRole("button", { name: /Notes folder menu/ }).click();
    await page.getByRole("menuitem", { name: "Move up" }).click();
    await expect
      .poll(() => page.locator(".root-section").evaluateAll((els) => els.map((e) => e.getAttribute("aria-label"))))
      .toEqual(["Home notes", expect.stringMatching(/textree-e2e-/)]);
  } finally {
    await loadVault(page, a);
    removeTempVault(a);
    removeTempVault(b);
  }
});

test("a notes folder's notes are hidden and shown again from its name", async () => {
  const a = createTempVault({ "a.md": "# a\n" });
  const b = createTempVault({ "seen.md": "# seen\n" });
  try {
    await loadVault(page, a);
    expect(await addRoot(b)).toBe(true);
    const bName = new RegExp(b.split("/").pop()!);
    await section(bName).getByRole("button", { name: "Hide notes" }).click();
    await expect(section(bName).getByRole("treeitem", { name: /seen/ })).toHaveCount(0);
    await section(bName).getByRole("button", { name: "Show notes" }).click();
    await expect(section(bName).getByRole("treeitem", { name: /seen/ })).toBeVisible();
  } finally {
    await loadVault(page, a);
    removeTempVault(a);
    removeTempVault(b);
  }
});

test("a folder inside one the sidebar shows is refused", async () => {
  const outer = createTempVault({ "inner/n.md": "# n\n", "o.md": "# o\n" });
  try {
    await loadVault(page, outer);
    await tauriInvoke(page, "workspace_add", { path: outer });
    const refusal = await tauriInvoke(page, "workspace_add", { path: `${outer}/inner` }).then(
      () => null,
      (e: unknown) => String(e),
    );
    expect(refusal).toContain("This folder is already in the sidebar inside");
  } finally {
    removeTempVault(outer);
  }
});

test("the palette finds notes of every open notes folder, says which, and opens one in its folder", async () => {
  const work = createTempVault({ "plan.md": "# plan\n\nwork plan\n" });
  const home = createTempVault({ "plan.md": "# plan\n\nhome plan\n" });
  try {
    await loadVault(page, work);
    expect(await addRoot(home)).toBe(true);
    const homeFolder = home.split(/[/\\]/).pop()!;

    await page.keyboard.press("Control+p");
    await page.getByTestId("palette-input").fill("plan");
    const rows = page.getByTestId("palette-item");
    await expect(rows).toHaveCount(2);
    await rows.filter({ hasText: homeFolder }).click();

    await expectOpenNote(page, "plan");
    await expect(page.locator(".cm-content")).toContainText("home plan");
    await expect(page.locator(".root-head.active")).toContainText(homeFolder);
  } finally {
    await loadVault(page, work);
    removeTempVault(work);
    removeTempVault(home);
  }
});

test("Tab reaches each notes folder's tree after moving within another", async () => {
  const a = createTempVault({ "one.md": "# one\n", "two.md": "# two\n" });
  const b = createTempVault({ "other.md": "# other\n" });
  try {
    await loadVault(page, a);
    expect(await addRoot(b)).toBe(true);
    const aName = new RegExp(a.split(/[/\\]/).pop()!);
    const bName = new RegExp(b.split(/[/\\]/).pop()!);
    await section(aName).getByRole("treeitem", { name: /one/ }).focus();
    await page.keyboard.press("ArrowDown"); // the tab stop moves to «two» in this folder
    await expect(section(aName).getByRole("treeitem", { name: /two/ })).toBeFocused();

    const target = section(bName).getByRole("treeitem", { name: /other/ });
    for (let i = 0; i < 20 && !(await target.evaluate((el) => el === document.activeElement)); i++) {
      await page.keyboard.press("Tab");
    }
    await expect(target).toBeFocused();
  } finally {
    await loadVault(page, a);
    removeTempVault(a);
    removeTempVault(b);
  }
});

test("links and linked mentions stay within each note's notes folder", async () => {
  // «Only-there» exists only in the other folder: a link to it from here stays unresolved.
  const a = createTempVault({
    "Hub.md": "# Hub\n\nSee [[Target]] and [[Only-there]].\n",
    "Target.md": "# Target\n\nhere target\n",
  });
  const b = createTempVault({
    "Elsewhere.md": "# Elsewhere\n\nAlso [[Target]].\n",
    "Target.md": "# Target\n\nthere target\n",
    "Only-there.md": "# Only there\n",
  });
  try {
    await loadVault(page, a);
    expect(await addRoot(b)).toBe(true);
    const aName = new RegExp(a.split(/[/\\]/).pop()!);
    const bName = new RegExp(b.split(/[/\\]/).pop()!);

    await section(aName).getByRole("treeitem", { name: /Hub/ }).click();
    await expect(page.locator(".cm-lp-wikilink-unresolved", { hasText: "Only-there" })).toBeVisible();
    await page.locator(".cm-lp-wikilink", { hasText: "Target" }).click();
    await expect(page.locator(".cm-content")).toContainText("here target");
    // Linked from its own folder's «Hub», not from the other folder's «Elsewhere».
    const mentions = page.getByRole("region", { name: "Linked mentions" });
    await expect(mentions).toContainText("Hub");
    await expect(mentions).not.toContainText("Elsewhere");

    await section(bName).getByRole("treeitem", { name: /Target/ }).click();
    await expect(page.locator(".cm-content")).toContainText("there target");
    await expect(mentions).toContainText("Elsewhere");
    await expect(mentions).not.toContainText("Hub");
  } finally {
    await loadVault(page, a);
    removeTempVault(a);
    removeTempVault(b);
  }
});

test("Add notes folder… through its dialog: a pick joins the sidebar, closing it changes nothing", async () => {
  const work = createTempVault({ "w.md": "# w\n" });
  const more = createTempVault({ "picked.md": "# picked\n" });
  const moreName = new RegExp(more.split("/").pop()!);
  const add = page.locator(".sidebar-head .vault-name");
  try {
    await loadVault(page, work);
    const asked = dialogsAsked().length;

    answerFolderDialog(null);
    await add.click();
    await expect.poll(() => dialogsAsked().length).toBe(asked + 1);
    await expect(section(moreName)).toHaveCount(0);

    answerFolderDialog(more);
    await add.click();
    await expect(section(moreName).getByRole("treeitem", { name: /picked/ })).toBeVisible();
    expect(dialogsAsked().slice(asked)).toEqual(["Add notes folder", "Add notes folder"]);
  } finally {
    await loadVault(page, work);
    removeTempVault(work);
    removeTempVault(more);
  }
});

test("Use with AI, off: the folder's menu says so, and the app refuses AI requests about it", async () => {
  const work = createTempVault({ "w.md": "# w\n" });
  const workName = new RegExp(work.split("/").pop()!);
  // Alone in the sidebar the folder has no section; its menu sits by its name at the sidebar's head.
  const menu = () => page.getByRole("button", { name: new RegExp(`Notes folder menu — ${workName.source}`) });
  const useWithAi = page.getByRole("menuitemcheckbox", { name: /Use with AI/ });
  let listed: string | undefined;
  try {
    await loadVault(page, work);
    // The app keeps every folder it opens in the device's list; the test bridge's open does not.
    const out = await tauriInvoke<{ roots: { id: string; path: string }[] }>(page, "workspace_add", { path: work });
    listed = await rootIdOf(page, work);
    expect(out.roots.map((r) => r.id)).toContain(listed);
    await menu().click();
    await expect(useWithAi).toHaveAttribute("aria-checked", "true");
    // Naming, moving and removing have no meaning for the only folder.
    for (const item of [/Rename in sidebar/, /Move up/, /Move down/, /Remove from sidebar/]) {
      await expect(page.getByRole("menuitem", { name: item })).toHaveCount(0);
    }
    await useWithAi.click();

    await menu().click();
    await expect(useWithAi).toHaveAttribute("aria-checked", "false");
    await page.keyboard.press("Escape");
    const rootId = await rootIdOf(page, work);
    await expect(
      tauriInvoke(page, "semantic_search", { rootId, query: "w", scope: null, limit: 5 }),
    ).rejects.toThrow(/not used with AI/);

    await menu().click();
    await useWithAi.click(); // back on, for the specs after this one
    await menu().click();
    await expect(useWithAi).toHaveAttribute("aria-checked", "true");
    await page.keyboard.press("Escape");
  } finally {
    await page.keyboard.press("Escape"); // a menu a failure left open must not cover the next spec
    if (listed) await tauriInvoke(page, "workspace_remove", { id: listed }).catch(() => {});
    await loadVault(page, work);
    removeTempVault(work);
  }
});
