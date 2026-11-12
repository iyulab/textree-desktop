import { test, expect, type Browser, type Page } from "@playwright/test";
import { connectToApp } from "./helpers";

/**
 * The window stays on the app. A page that tries to go elsewhere — a script setting the address,
 * a link in a note, a new window — leaves the window where it was: the app's own page, still
 * working. (The web view is held to the app's own origins on Windows, where this suite runs.)
 */

let browser: Browser;
let page: Page;

test.beforeAll(async () => {
  ({ browser, page } = await connectToApp());
});

test.afterAll(async () => {
  await browser?.close();
});

test("a page that sends the window to another address stays where it is", async () => {
  const own = new URL(page.url()).origin;
  await page.evaluate(() => {
    window.location.href = "https://example.com/elsewhere";
  });
  // The refused navigation never commits: give it the time one would take, then look.
  await page.waitForTimeout(1500);
  expect(new URL(page.url()).origin).toBe(own);
  // Still the app, still answering its own bridge.
  expect(await page.evaluate(() => typeof (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__)).toBe(
    "object",
  );
});

test("a new window a page opens is never created", async () => {
  const before = browser.contexts().flatMap((c) => c.pages()).length;
  await page.evaluate(() => {
    window.open("https://example.com/popup", "_blank");
  });
  await page.waitForTimeout(1500);
  const after = browser.contexts().flatMap((c) => c.pages()).length;
  expect(after).toBe(before);
  expect(new URL(page.url()).hostname).not.toBe("example.com");
});
