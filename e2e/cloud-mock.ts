import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";

/**
 * Stand-in for the identity provider and the publishing backend, so the in-app sign-in can be
 * driven end to end without reaching the real services.
 *
 * Both refuse by default and succeed only when a test asks (`grantSignIn`, `grantToken`), so a test
 * that means to cover a failure path cannot accidentally complete a sign-in. Completing one is safe
 * because a development build stores credentials under its own namespace and cannot reach the
 * installed app's entry (see `secret_store.rs`); `credential-probe.ts` checks that separation still
 * holds rather than trusting it.
 *
 * Ports are fixed because the app reads its base URLs from the environment at launch, before a
 * test process exists to negotiate them.
 */

export const MOCK_AUTH_PORT = 8787;
export const MOCK_API_PORT = 8788;
export const MOCK_AUTH_BASE = `http://127.0.0.1:${MOCK_AUTH_PORT}`;
export const MOCK_API_BASE = `http://127.0.0.1:${MOCK_API_PORT}`;
export const MOCK_CLIENT_ID = "e2e-client";

/** The query the desktop put on the authorization URL. */
export interface AuthorizeParams {
  clientId: string;
  /** Loopback port the desktop is listening on (from its redirect URI). */
  port: string;
  state: string;
  nonce: string;
  codeChallenge: string;
  codeChallengeMethod: string;
  scope: string;
}

export interface CloudMock {
  /** Waits for the desktop to open the authorization URL and returns its query. */
  waitForAuthorize(timeoutMs?: number): Promise<AuthorizeParams>;
  /** Form bodies the desktop POSTed to the provider's token endpoint, oldest first. */
  codeExchanges(): Record<string, string>[];
  /** `Authorization` headers the desktop sent to the publishing API, oldest first. */
  apiAuthorizations(): string[];
  /** Tokens the desktop asked the provider to revoke, oldest first. */
  revocations(): string[];
  /** Makes the provider accept the next code exchanges. Rejection is the default. */
  grantSignIn(): void;
  /** Makes the publishing API issue this publish token. Refusal (401) is the default. */
  grantToken(token: string): void;
  /** Forgets what was recorded and reverts to refusing, so the next sign-in starts clean. */
  reset(): void;
  close(): Promise<void>;
}

function listen(server: Server, port: number): Promise<void> {
  return new Promise((res, rej) => {
    server.once("error", rej);
    server.listen(port, "127.0.0.1", () => res());
  });
}

function close(server: Server): Promise<void> {
  // The browser that opened the stub page holds its socket open, and close() alone waits for every
  // live connection — which would hang teardown until the tab goes away.
  server.closeAllConnections();
  return new Promise((res) => server.close(() => res()));
}

function readBody(req: IncomingMessage): Promise<string> {
  return new Promise((res) => {
    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", () => res(raw));
  });
}

const b64url = (v: unknown) => Buffer.from(typeof v === "string" ? v : JSON.stringify(v)).toString("base64url");

/** An ID token for this sign-in. The desktop reads its claims but does not check a signature. */
function idToken(nonce: string): string {
  const claims = { iss: MOCK_AUTH_BASE, aud: [MOCK_CLIENT_ID], sub: "e2e-user", exp: Math.floor(Date.now() / 1000) + 600, nonce };
  return `${b64url({ alg: "RS256", typ: "JWT" })}.${b64url(claims)}.signature`;
}

export async function startCloudMock(): Promise<CloudMock> {
  let authorize: AuthorizeParams | null = null;
  let signInGranted = false;
  let granted: string | null = null;
  const exchanges: Record<string, string>[] = [];
  const authorizations: string[] = [];
  const revoked: string[] = [];

  const provider = createServer(async (req: IncomingMessage, res: ServerResponse) => {
    const url = new URL(req.url ?? "/", MOCK_AUTH_BASE);
    const json = (status: number, body: unknown) => {
      res.writeHead(status, { "content-type": "application/json", connection: "close" });
      res.end(JSON.stringify(body));
    };
    if (url.pathname === "/.well-known/openid-configuration") {
      json(200, {
        issuer: MOCK_AUTH_BASE,
        authorization_endpoint: `${MOCK_AUTH_BASE}/authorize`,
        token_endpoint: `${MOCK_AUTH_BASE}/token`,
        revocation_endpoint: `${MOCK_AUTH_BASE}/revoke`,
      });
      return;
    }
    if (url.pathname === "/authorize") {
      const redirect = new URL(url.searchParams.get("redirect_uri") ?? "http://127.0.0.1:0/");
      authorize = {
        clientId: url.searchParams.get("client_id") ?? "",
        port: redirect.port,
        state: url.searchParams.get("state") ?? "",
        nonce: url.searchParams.get("nonce") ?? "",
        codeChallenge: url.searchParams.get("code_challenge") ?? "",
        codeChallengeMethod: url.searchParams.get("code_challenge_method") ?? "",
        scope: url.searchParams.get("scope") ?? "",
      };
      // Deliberately no redirect: the test delivers the callback itself, so the assertions do not
      // depend on how the machine's default browser handles a redirect to a loopback address.
      res.writeHead(200, { "content-type": "text/html; charset=utf-8", connection: "close" });
      res.end("<!doctype html><title>sign-in stub</title><p>Test stub — this tab can be closed.</p>");
      return;
    }
    if (req.method === "POST" && url.pathname === "/token") {
      exchanges.push(Object.fromEntries(new URLSearchParams(await readBody(req))));
      if (signInGranted && authorize) {
        json(200, { access_token: "e2e-access-token", token_type: "Bearer", expires_in: 600, id_token: idToken(authorize.nonce) });
        return;
      }
      json(400, { error: "invalid_grant" });
      return;
    }
    if (req.method === "POST" && url.pathname === "/revoke") {
      revoked.push(new URLSearchParams(await readBody(req)).get("token") ?? "");
      json(200, {});
      return;
    }
    res.writeHead(404).end();
  });

  const api = createServer(async (req: IncomingMessage, res: ServerResponse) => {
    const url = new URL(req.url ?? "/", MOCK_API_BASE);
    if (req.method === "POST" && url.pathname === "/tokens/desktop") {
      await readBody(req);
      authorizations.push(String(req.headers.authorization ?? ""));
      if (granted !== null) {
        res.writeHead(200, { "content-type": "application/json", connection: "close" });
        res.end(JSON.stringify({ token: granted }));
        return;
      }
      res.writeHead(401, { connection: "close" });
      res.end();
      return;
    }
    res.writeHead(404).end();
  });

  await listen(provider, MOCK_AUTH_PORT);
  await listen(api, MOCK_API_PORT);

  return {
    async waitForAuthorize(timeoutMs = 15_000): Promise<AuthorizeParams> {
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        if (authorize) return authorize;
        await new Promise((r) => setTimeout(r, 100));
      }
      throw new Error(
        `The desktop never opened the authorization URL on ${MOCK_AUTH_BASE}. ` +
          `Launch the app with TEXTREE_AUTH_ISSUER=${MOCK_AUTH_BASE} TEXTREE_AUTH_CLIENT_ID=${MOCK_CLIENT_ID} ` +
          `TEXTREE_API_BASE=${MOCK_API_BASE}.`,
      );
    },
    codeExchanges: () => exchanges.slice(),
    apiAuthorizations: () => authorizations.slice(),
    revocations: () => revoked.slice(),
    grantSignIn() {
      signInGranted = true;
    },
    grantToken(token: string) {
      granted = token;
    },
    reset() {
      authorize = null;
      signInGranted = false;
      granted = null;
      exchanges.length = 0;
      authorizations.length = 0;
      revoked.length = 0;
    },
    async close() {
      await Promise.all([close(provider), close(api)]);
    },
  };
}

/** Delivers a loopback callback the way the browser would, and returns the HTTP status. */
export async function deliverCallback(port: string, code: string, state: string): Promise<number> {
  const res = await fetch(`http://127.0.0.1:${port}/callback?code=${encodeURIComponent(code)}&state=${encodeURIComponent(state)}`);
  return res.status;
}
