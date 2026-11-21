import { test, expect, type Browser, type Page } from "@playwright/test";
import {
  connectToApp,
  loadVault,
  createTempVault,
  removeTempVault,
  dragNodeOnto,
} from "./helpers";
import { existsSync, writeFileSync } from "node:fs";
import { join } from "node:path";

let browser: Browser;
let page: Page;

test.beforeAll(async () => {
  ({ browser, page } = await connectToApp());
});

test.afterAll(async () => {
  await browser?.close();
});

const exists = (vault: string, rel: string) => existsSync(join(vault, rel));

test("＋note → instant Untitled note, header focused, typing renames the file", async () => {
  const vault = createTempVault({ "existing.md": "x\n" });
  try {
    await loadVault(page, vault);
    await page.getByRole("button", { name: "New note" }).click();

    // No dialog — an "Untitled" note is created, opened, and the header title input is focused.
    await expect(page.locator(".title-input")).toBeFocused();
    await expect(page.locator(".title-input")).toHaveValue("Untitled");
    await expect.poll(() => exists(vault, "Untitled.md")).toBe(true);

    // The pre-selected "Untitled" text is replaced by typing the first char (not fill,
    // which would replace regardless of selection) → proves focusSelect's select-all.
    await page.locator(".title-input").pressSequentially("new-note");
    await page.locator(".title-input").press("Enter");
    await expect(page.getByRole("treeitem", { name: /new-note/ })).toBeVisible();
    await expect.poll(() => exists(vault, "new-note.md")).toBe(true);
    await expect.poll(() => exists(vault, "Untitled.md")).toBe(false);

    // A second New note is «Untitled» again — the first was renamed. The file is written after the click
    // returns, so it is waited for.
    await page.getByRole("button", { name: "New note" }).click();
    await expect.poll(() => exists(vault, "Untitled.md")).toBe(true);
  } finally {
    removeTempVault(vault);
  }
});

test("first H1 names an Untitled note on editor blur", async () => {
  const vault = createTempVault({ "existing.md": "x\n" });
  try {
    await loadVault(page, vault);
    await page.getByRole("button", { name: "New note" }).click();
    await expect(page.locator(".title-input")).toBeFocused();

    // Move into the body and type the first H1, then blur by switching notes.
    await page.locator(".cm-content").click();
    await page.locator(".cm-content").pressSequentially("# meeting-notes");
    await page.getByRole("treeitem", { name: /existing/ }).click();

    await expect.poll(() => exists(vault, "meeting-notes.md"), { timeout: 5000 }).toBe(true);
    expect(exists(vault, "Untitled.md")).toBe(false);
    await expect(page.getByRole("treeitem", { name: /meeting-notes/ })).toBeVisible();
  } finally {
    removeTempVault(vault);
  }
});

test("first H1 auto-numbers when the name is taken", async () => {
  const vault = createTempVault({ "meeting-notes.md": "existing\n" });
  try {
    await loadVault(page, vault);
    await page.getByRole("button", { name: "New note" }).click();
    await expect(page.locator(".title-input")).toBeFocused();

    await page.locator(".cm-content").click();
    await page.locator(".cm-content").pressSequentially("# meeting-notes");
    await page.getByRole("treeitem", { name: /meeting-notes$/ }).click(); // blur via the existing note

    await expect.poll(() => exists(vault, "meeting-notes (1).md"), { timeout: 5000 }).toBe(true);
    expect(exists(vault, "meeting-notes.md")).toBe(true); // the existing note is untouched
    expect(exists(vault, "Untitled.md")).toBe(false);
  } finally {
    removeTempVault(vault);
  }
});

test("editing the first H1 of a NAMED note does not rename it", async () => {
  // Core boundary regression: a named note's H1 edits must NOT rename (protects inbound links).
  const vault = createTempVault({ "my-note.md": "body\n" });
  try {
    await loadVault(page, vault);
    await page.getByRole("treeitem", { name: /my-note/ }).click();
    await expect(page.locator(".cm-content")).toBeVisible();

    await page.locator(".cm-content").click();
    await page.keyboard.press("Control+Home");
    await page.locator(".cm-content").pressSequentially("# forced-title\n");
    await page.getByRole("button", { name: "New note" }).click(); // blur the editor
    await expect(page.getByRole("treeitem", { name: /Untitled/ })).toBeVisible();
    // A wrongful rename would land asynchronously (the renaming tests above wait up to 5 s for
    // theirs): give it that long before concluding it did not happen.
    await page.waitForTimeout(2000);

    expect(exists(vault, "my-note.md")).toBe(true);
    expect(exists(vault, "forced-title.md")).toBe(false);
  } finally {
    removeTempVault(vault);
  }
});

test("first H1 with reserved characters is sanitized into the filename", async () => {
  const vault = createTempVault({ "existing.md": "x\n" });
  try {
    await loadVault(page, vault);
    await page.getByRole("button", { name: "New note" }).click();
    await expect(page.locator(".title-input")).toBeFocused();

    await page.locator(".cm-content").click();
    await page.locator(".cm-content").pressSequentially("# Plan: A/B");
    await page.getByRole("treeitem", { name: /existing/ }).click();

    // ":" and "/" are replaced with spaces → "Plan A B".
    await expect.poll(() => exists(vault, "Plan A B.md"), { timeout: 5000 }).toBe(true);
    expect(exists(vault, "Untitled.md")).toBe(false);
  } finally {
    removeTempVault(vault);
  }
});

test("＋folder → create directory on disk", async () => {
  const vault = createTempVault({ "existing.md": "x\n" });
  try {
    await loadVault(page, vault);
    await page.getByRole("button", { name: "New folder" }).click();
    await page.locator(".name-input").fill("new-folder");
    await page.locator(".name-input").press("Enter");

    await expect(page.getByRole("treeitem", { name: /new-folder/ })).toBeVisible();
    expect(exists(vault, "new-folder")).toBe(true);
  } finally {
    removeTempVault(vault);
  }
});

test("rename → inline rename file on disk", async () => {
  const vault = createTempVault({ "old-name.md": "content\n" });
  try {
    await loadVault(page, vault);
    await page.getByRole("treeitem", { name: /old-name/ }).click();
    await page.getByRole("button", { name: "Rename", exact: true }).click();
    await page.locator(".tree-rename-input").fill("new-name");
    await page.locator(".tree-rename-input").press("Enter");

    await expect(page.getByRole("treeitem", { name: /new-name/ })).toBeVisible();
    expect(exists(vault, "new-name.md")).toBe(true);
    expect(exists(vault, "old-name.md")).toBe(false);
  } finally {
    removeTempVault(vault);
  }
});

test("rename → illegal name keeps the input open with an inline error", async () => {
  const vault = createTempVault({ "normal.md": "content\n" });
  try {
    await loadVault(page, vault);
    await page.getByRole("treeitem", { name: /normal/ }).click();
    await page.getByRole("button", { name: "Rename", exact: true }).click();
    // A name containing a path separator is rejected by is_valid_name.
    await page.locator(".tree-rename-input").fill("a/b");
    await page.locator(".tree-rename-input").press("Enter");

    // Input stays open, an inline error shows, and the file is NOT renamed.
    await expect(page.locator(".tree-rename-input")).toBeVisible();
    await expect(page.locator(".tree-rename-error")).toBeVisible();
    expect(exists(vault, "normal.md")).toBe(true);
  } finally {
    removeTempVault(vault);
  }
});

test("rename → inline rename folder (directory + folder note on disk)", async () => {
  const vault = createTempVault({ "old-folder/old-folder.md": "note\n" });
  try {
    await loadVault(page, vault);
    await page.getByRole("treeitem", { name: /old-folder/ }).click();
    await page.getByRole("button", { name: "Rename", exact: true }).click();
    await page.locator(".tree-rename-input").fill("new-folder");
    await page.locator(".tree-rename-input").press("Enter");

    await expect(page.getByRole("treeitem", { name: /new-folder/ })).toBeVisible();
    expect(exists(vault, "new-folder")).toBe(true);
    expect(exists(vault, "old-folder")).toBe(false);
  } finally {
    removeTempVault(vault);
  }
});

// Regression: the inline-rename blur guard must reset between sessions. A blur-away commit
// (no Enter) arms the guard but fires no unmount-blur to consume it; if the next rename
// session does not start clean, its first commit is silently swallowed.
test("rename → blur-away commit, then a second rename both land (guard reset)", async () => {
  const vault = createTempVault({ "alpha.md": "a\n", "beta.md": "b\n" });
  try {
    await loadVault(page, vault);

    // Rename alpha by clicking another node (commit via blur, not Enter).
    await page.getByRole("treeitem", { name: /alpha/ }).click();
    await page.getByRole("button", { name: "Rename", exact: true }).click();
    await page.locator(".tree-rename-input").fill("alpha2");
    await page.getByRole("treeitem", { name: /beta/ }).click();
    await expect(page.getByRole("treeitem", { name: /alpha2/ })).toBeVisible();
    expect(exists(vault, "alpha2.md")).toBe(true);

    // The next rename must land on its FIRST commit (a leaked guard would swallow it).
    await page.getByRole("treeitem", { name: /beta/ }).click();
    await page.getByRole("button", { name: "Rename", exact: true }).click();
    await page.locator(".tree-rename-input").fill("beta2");
    await page.locator(".tree-rename-input").press("Enter");
    await expect(page.getByRole("treeitem", { name: /beta2/ })).toBeVisible();
    expect(exists(vault, "beta2.md")).toBe(true);
    expect(exists(vault, "beta.md")).toBe(false);
  } finally {
    removeTempVault(vault);
  }
});

test("delete → the note leaves the folder (original disappears)", async () => {
  const vault = createTempVault({ "delete-target-note.md": "content\n" });
  try {
    await loadVault(page, vault);
    await page.getByRole("treeitem", { name: /delete-target-note/ }).click();
    await page.getByRole("button", { name: "Delete", exact: true }).click();

    await expect(page.getByRole("treeitem", { name: /delete-target-note/ })).toHaveCount(0);
    expect(exists(vault, "delete-target-note.md")).toBe(false);
  } finally {
    removeTempVault(vault);
  }
});

test("the key goes to the note that has focus even after a note appears above it", async () => {
  const vault = createTempVault({ "mango.md": "mango\n", "zebra.md": "zebra\n" });
  try {
    await loadVault(page, vault);
    const zebra = page.getByRole("treeitem", { name: /zebra/ });
    await zebra.focus();
    // A note arrives from outside and sorts above the focused one; the tree redraws around the focus.
    writeFileSync(join(vault, "apple.md"), "apple\n");
    await expect(page.getByRole("treeitem", { name: /apple/ })).toBeVisible();

    await page.keyboard.press("Delete");

    await expect(page.getByRole("treeitem", { name: /zebra/ })).toHaveCount(0);
    expect(exists(vault, "zebra.md")).toBe(false);
    expect(exists(vault, "apple.md")).toBe(true);
    expect(exists(vault, "mango.md")).toBe(true);
  } finally {
    removeTempVault(vault);
  }
});

test("create after vault switch targets new vault — stale selection isolation (regression)", async () => {
  // Regression: if loadVault does not clear the previous vault's selectedNode,
  // ＋note's targetParent points at the previous vault path and creates in the wrong place.
  const v1 = createTempVault({ "v1note.md": "1\n" });
  const v2 = createTempVault({ "v2note.md": "2\n" });
  try {
    await loadVault(page, v1);
    await page.getByRole("treeitem", { name: /v1note/ }).click(); // set selectedNode
    await expect(page.locator(".cm-content")).toBeVisible();

    await loadVault(page, v2); // vault switch — previous selection must be invalidated
    await page.getByRole("button", { name: "New note" }).click();
    // dialog-free: an Untitled note is created in v2; header title input is focused.
    await expect(page.locator(".title-input")).toBeFocused();
    await page.locator(".title-input").pressSequentially("newer");
    await page.locator(".title-input").press("Enter");

    await expect.poll(() => exists(v2, "newer.md"), { timeout: 5000 }).toBe(true);
    expect(exists(v1, "newer.md")).toBe(false);
  } finally {
    removeTempVault(v1);
    removeTempVault(v2);
  }
});

test("DnD note → move into folder", async () => {
  const vault = createTempVault({
    "move-note.md": "content to move\n",
    "target-folder/target-folder.md": "folder body\n",
  });
  try {
    await loadVault(page, vault);
    const src = page.getByRole("treeitem", { name: /move-note/ });
    const dst = page.getByRole("treeitem", { name: /target-folder/ });
    await expect(src).toBeVisible();
    await dragNodeOnto(page, src, dst);

    await expect.poll(() => exists(vault, "target-folder/move-note.md"), { timeout: 5000 }).toBe(true);
    expect(exists(vault, "move-note.md")).toBe(false);
  } finally {
    removeTempVault(vault);
  }
});

test("＋child → promote leaf then create child note", async () => {
  const vault = createTempVault({ "parent.md": "parent body\n" });
  try {
    await loadVault(page, vault);
    await page.getByRole("treeitem", { name: /parent/ }).click();
    await page.getByRole("button", { name: "Add child note" }).click();
    // dialog-free: leaf is promoted, an Untitled child note is created + header focused.
    await expect(page.locator(".title-input")).toBeFocused();
    await page.locator(".title-input").pressSequentially("child");
    await page.locator(".title-input").press("Enter");

    // Promote: parent.md → parent/parent.md, and create parent/child.md.
    await expect.poll(() => exists(vault, "parent/parent.md"), { timeout: 5000 }).toBe(true);
    // The title is applied by a rename after the body is saved — it lands a moment after Enter.
    await expect.poll(() => exists(vault, "parent/child.md"), { timeout: 5000 }).toBe(true);
    expect(exists(vault, "parent.md")).toBe(false);
  } finally {
    removeTempVault(vault);
  }
});

test("DnD drop onto leaf → adopt (promote then child)", async () => {
  const vault = createTempVault({
    "drop-source.md": "source content\n",
    "drop-target.md": "target content\n",
  });
  try {
    await loadVault(page, vault);
    const src = page.getByRole("treeitem", { name: /drop-source/ });
    const dst = page.getByRole("treeitem", { name: /drop-target/ });
    await expect(src).toBeVisible();
    await dragNodeOnto(page, src, dst);

    // drop-target is promoted to a container and drop-source becomes its child.
    await expect.poll(() => exists(vault, "drop-target/drop-target.md"), { timeout: 5000 }).toBe(true);
    expect(exists(vault, "drop-target/drop-source.md")).toBe(true);
    expect(exists(vault, "drop-source.md")).toBe(false);
  } finally {
    removeTempVault(vault);
  }
});
