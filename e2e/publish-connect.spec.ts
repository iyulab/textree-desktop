/**
 * E2E — in-app sign-in for web publishing (browser OAuth loopback + PKCE, then a publish token).
 *
 * Gated profile (cannot share an app instance with the normal suite, because the app reads its
 * backend addresses from the environment at launch):
 *
 *   Terminal 1:  $env:TEXTREE_AUTH_ISSUER = "http://127.0.0.1:8787"
 *                $env:TEXTREE_AUTH_CLIENT_ID = "e2e-client"
 *                $env:TEXTREE_API_BASE = "http://127.0.0.1:8788"
 *                npm run dev:e2e
 *   Terminal 2:  $env:TEXTREE_CONNECT_E2E = "mock"
 *                npm run test:e2e -- publish-connect
 *
 * Without TEXTREE_CONNECT_E2E the whole file skips, so the default suite never triggers a sign-in
 * against the real service (which would open a browser and block for three minutes).
 *
 * What this covers that unit tests cannot: the button in the app actually starts a loopback
 * listener, hands the identity provider a fresh state + nonce + PKCE challenge, accepts only the
 * callback carrying its own state, redeems the code, shows the publishing API the access token it
 * got, stores only the publish token that comes back, revokes the provider's session, and renders
 * a failure where a user would see it.
 *
 * Completing a sign-in is safe because the app under test is a development build, and those are
 * compiled to use their own credential entry (`secret_store.rs`) — the entry the installed app uses
 * is unreachable from here. The success test proves that separation rather than trusting it: it
 * reads both entries and fails if the installed app's one moved.
 *
 * Selectors come from Settings.svelte (Advanced section) and +page.svelte (publish banner).
 */
import { test, expect, type Browser, type Page } from "@playwright/test";
import { connectToApp, createTempVault, loadVault, removeTempVault } from "./helpers";
import { deliverCallback, MOCK_CLIENT_ID, startCloudMock, type CloudMock } from "./cloud-mock";
import {
  DEV_PUBLISH_TARGET,
  PRODUCTION_PUBLISH_TARGET,
  readCredential,
  sameRecord,
} from "./credential-probe";

const PROFILE = process.env.TEXTREE_CONNECT_E2E;

let browser: Browser;
let page: Page;
let mock: CloudMock;

test.beforeAll(async () => {
  if (PROFILE !== "mock") return;
  mock = await startCloudMock();
  ({ browser, page } = await connectToApp());
});

test.afterAll(async () => {
  await browser?.close();
  await mock?.close();
});

async function hasPublishToken(p: Page): Promise<boolean> {
  return p.evaluate(() =>
    (window as unknown as { __textreeTest: { hasPublishToken: () => Promise<boolean> } }).__textreeTest.hasPublishToken(),
  );
}

async function openPublishSettings(p: Page) {
  await p.keyboard.press("Control+p");
  await expect(p.getByTestId("palette-input")).toBeVisible();
  await p.getByTestId("palette-input").fill(">settings");
  await expect(p.getByTestId("palette-item").first()).toBeVisible();
  await p.keyboard.press("Enter");
  const dialog = p.getByRole("dialog", { name: "Settings" });
  await expect(dialog).toBeVisible();

  // The publishing controls live in a <details>. Expand it only when it is closed — clicking a
  // summary unconditionally toggles, so a blind click collapses a section a previous test left open.
  const advanced = dialog.locator("details.byo-advanced");
  if (!(await advanced.evaluate((el: HTMLDetailsElement) => el.open))) {
    await advanced.locator("summary").click();
  }
  await expect(advanced).toHaveAttribute("open", "");
  // The publishing controls, not the whole dialog: the Backup section has a Connect and a
  // Disconnect of its own.
  return advanced;
}

/** Closes Settings and waits for it to go away, so the next test starts from a known state. */
async function closeSettings(p: Page) {
  const dialog = p.getByRole("dialog", { name: "Settings" });
  for (let i = 0; i < 3 && (await dialog.isVisible()); i++) {
    await dialog.click({ position: { x: 5, y: 5 } }); // focus the dialog so it receives the key
    await p.keyboard.press("Escape");
    await p.waitForTimeout(200);
  }
  await expect(dialog).toBeHidden();
}

/** base64url, no padding — the shape both the state and the S256 challenge must have. */
const BASE64URL_32_BYTES = /^[A-Za-z0-9_-]{43}$/;

/** Returns the app to the not-yet-connected state, whatever a previous test left behind. */
async function disconnect(p: Page) {
  if (!(await hasPublishToken(p))) return;
  const dialog = await openPublishSettings(p);
  await dialog.getByRole("button", { name: "Disconnect", exact: true }).click();
  await expect.poll(() => hasPublishToken(p)).toBe(false);
  await closeSettings(p);
}

test.describe("in-app sign-in", () => {
  test.beforeEach(async () => {
    test.skip(PROFILE !== "mock", "Set TEXTREE_CONNECT_E2E=mock and launch the app against the mock backend.");
    // Every test here starts from "nothing stored yet". A development build keeps its token in its
    // own entry, so clearing it costs nothing and no longer has to be worked around by skipping.
    await disconnect(page);
    mock.reset();
  });

  test("hands the provider a fresh loopback port, state, nonce and PKCE challenge, then surfaces the failure", async () => {
    const dialog = await openPublishSettings(page);
    const connect = dialog.getByRole("button", { name: "Connect", exact: true });
    await expect(connect).toBeVisible();

    // Not awaited: the click resolves only when the whole sign-in settles.
    void connect.click();

    const auth = await mock.waitForAuthorize();
    expect(auth.clientId).toBe(MOCK_CLIENT_ID);
    expect(auth.port, "a loopback port was allocated").toMatch(/^\d{4,5}$/);
    expect(auth.state, "state is 32 bytes of base64url").toMatch(BASE64URL_32_BYTES);
    expect(auth.nonce, "nonce is 32 bytes of base64url").toMatch(BASE64URL_32_BYTES);
    expect(auth.codeChallenge, "challenge is a base64url SHA-256").toMatch(BASE64URL_32_BYTES);
    expect(auth.codeChallengeMethod).toBe("S256");
    expect(auth.scope, "no refresh token is asked for — the provider's session is not kept").toBe("openid");
    expect(auth.state).not.toBe(auth.codeChallenge);

    // Play the browser's part.
    expect(await deliverCallback(auth.port, "e2e-code", auth.state)).toBe(200);

    // The desktop redeems the code, sending the verifier — never the challenge.
    await expect
      .poll(() => mock.codeExchanges().length, { message: "the desktop redeemed the code" })
      .toBe(1);
    const body = mock.codeExchanges()[0];
    expect(body.grant_type).toBe("authorization_code");
    expect(body.code).toBe("e2e-code");
    expect(body.code_verifier, "verifier is 32 bytes of base64url").toMatch(BASE64URL_32_BYTES);
    expect(body.code_verifier).not.toBe(auth.codeChallenge);

    // The mock provider rejects, and the user sees why.
    await expect(dialog.getByText(/Failed:.*couldn't complete sign-in/i)).toBeVisible();
    expect(mock.apiAuthorizations(), "no access token existed to show the publishing API").toHaveLength(0);

    expect(await hasPublishToken(page), "a failed sign-in must not store anything").toBe(false);
    await closeSettings(page);
  });

  test("when the publishing API refuses the access token nothing is stored and the provider's session is revoked", async () => {
    mock.grantSignIn(); // the provider accepts; the publishing API (no token granted) refuses

    const dialog = await openPublishSettings(page);
    void dialog.getByRole("button", { name: "Connect", exact: true }).click();

    const auth = await mock.waitForAuthorize();
    expect(await deliverCallback(auth.port, "e2e-code", auth.state)).toBe(200);

    await expect(dialog.getByText(/Failed:.*expired or was invalid/i)).toBeVisible();
    expect(mock.apiAuthorizations()).toEqual(["Bearer e2e-access-token"]);
    await expect.poll(() => mock.revocations(), { message: "the provider session was revoked" }).toContain("e2e-access-token");

    expect(await hasPublishToken(page), "a refused exchange must not store anything").toBe(false);
    await closeSettings(page);
  });

  test("ignores a callback carrying someone else's state and keeps waiting for its own", async () => {
    const dialog = await openPublishSettings(page);
    void dialog.getByRole("button", { name: "Connect", exact: true }).click();

    const auth = await mock.waitForAuthorize();

    // A forged or stray callback must not release the code.
    expect(await deliverCallback(auth.port, "forged-code", "not-our-state")).toBe(404);
    expect(mock.codeExchanges(), "nothing was redeemed for the forged callback").toHaveLength(0);

    // The listener is still up and still bound to its own state.
    expect(await deliverCallback(auth.port, "e2e-code", auth.state)).toBe(200);
    await expect.poll(() => mock.codeExchanges().length).toBe(1);
    expect(mock.codeExchanges()[0].code).toBe("e2e-code");

    expect(await hasPublishToken(page)).toBe(false);
    await closeSettings(page);
  });

  test("a completed sign-in stores the token, and stores it away from the installed app", async () => {
    // Read both entries first: the one this build is expected to write, and the one it must never
    // touch. The second reading is what turns "we namespaced it" into something the suite checks.
    const productionBefore = readCredential(PRODUCTION_PUBLISH_TARGET);
    expect(readCredential(DEV_PUBLISH_TARGET).present, "starts disconnected").toBe(false);

    mock.grantSignIn();
    mock.grantToken("tk_e2e_granted");

    const dialog = await openPublishSettings(page);
    void dialog.getByRole("button", { name: "Connect", exact: true }).click();

    const auth = await mock.waitForAuthorize();
    expect(await deliverCallback(auth.port, "e2e-code", auth.state)).toBe(200);

    // The granted token reaches the keychain, and the UI switches to connected.
    await expect.poll(() => hasPublishToken(page), { message: "the token was stored" }).toBe(true);
    await expect(dialog.getByRole("button", { name: "Disconnect", exact: true })).toBeVisible();
    await expect(dialog.getByText(/Connected to web publishing/i)).toBeVisible();

    // The publishing API was shown the provider's access token, and that session is gone again: the
    // app keeps the publish token and nothing else.
    expect(mock.apiAuthorizations()).toEqual(["Bearer e2e-access-token"]);
    await expect.poll(() => mock.revocations(), { message: "the provider session was revoked" }).toContain("e2e-access-token");

    // It landed in this build's own entry...
    expect(readCredential(DEV_PUBLISH_TARGET).present, "the development entry now holds it").toBe(true);
    // ...and the entry the installed app reads is byte-for-byte where it was.
    expect(
      sameRecord(readCredential(PRODUCTION_PUBLISH_TARGET), productionBefore),
      `a test wrote to ${PRODUCTION_PUBLISH_TARGET} — the credential namespacing in secret_store.rs is gone, ` +
        "and a real publishing token has just been overwritten",
    ).toBe(true);

    // Disconnecting takes it back out, so the next test starts clean.
    await dialog.getByRole("button", { name: "Disconnect", exact: true }).click();
    await expect.poll(() => hasPublishToken(page)).toBe(false);
    expect(readCredential(DEV_PUBLISH_TARGET).present).toBe(false);
    await closeSettings(page);
  });

  test("publishing without a token starts the sign-in instead of dead-ending", async () => {
    const vault = createTempVault({ "note.md": "# Publish source\n" });
    try {
      await loadVault(page, vault);

      await page.keyboard.press("Control+p");
      await expect(page.getByTestId("palette-input")).toBeVisible();
      await page.getByTestId("palette-input").fill(">publish to web");
      await expect(page.getByTestId("palette-item").first()).toBeVisible();
      await page.keyboard.press("Enter");

      // What goes out is confirmed before anything else happens, the sign-in included. A new folder
      // has nothing chosen and nothing recorded yet: choose the whole folder, then record a version.
      const confirm = page.getByTestId("publish-confirm");
      await expect(confirm).toBeVisible();
      await confirm.getByTestId("publish-confirm-choose-all").click();
      await confirm.getByTestId("publish-confirm-add-versions").click();
      await expect(page.getByTestId("publish-confirm-go")).toBeEnabled({ timeout: 10000 });
      await page.getByTestId("publish-confirm-go").click();

      // No token is stored, so the publish path must run the sign-in first rather than telling the
      // user to go and paste one.
      const auth = await mock.waitForAuthorize();
      expect(auth.codeChallengeMethod).toBe("S256");

      expect(await deliverCallback(auth.port, "e2e-code", auth.state)).toBe(200);
      await expect.poll(() => mock.codeExchanges().length).toBe(1);

      // The failure surfaces on the publish banner, with a retry offered.
      const banner = page.locator(".publish-banner.error");
      await expect(banner).toBeVisible();
      await expect(banner.getByRole("button", { name: /retry/i })).toBeVisible();

      expect(await hasPublishToken(page)).toBe(false);
      await banner.getByRole("button", { name: /dismiss/i }).click();
    } finally {
      removeTempVault(vault);
    }
  });
});
