import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

/**
 * A repository to back notes up to, served on this machine the way a hosting site serves one:
 * smart HTTP with Basic authentication, answered by `git http-backend` run as a CGI program.
 *
 * Only the password part of the credentials is checked — hosting sites accept a token with any
 * user name, and the app sends one of its own choosing.
 */

export interface TestRemote {
  /** Address to connect to, e.g. http://127.0.0.1:1234/repo.git */
  url: string;
  /** The token the server accepts. */
  token: string;
  /** The bare repository on disk. */
  bare: string;
  /** Output of `git --git-dir=<bare> <args>`, trimmed; empty when git fails. */
  git: (...args: string[]) => string;
  close: () => Promise<void>;
}

/** Whether `git` can be run here. */
export function gitAvailable(): boolean {
  const probe = spawnSync("git", ["--version"], { encoding: "utf8" });
  return probe.status === 0;
}

export async function startTestRemote(token = "right-token"): Promise<TestRemote> {
  const root = mkdtempSync(join(tmpdir(), "textree-e2e-remote-"));
  const bare = join(root, "repo.git");
  run(["init", "--bare", bare]);
  run(["-C", bare, "config", "http.receivepack", "true"]);

  // Requests being answered. Closing the sockets does not stop the `git` program a request started
  // — it runs on in the repository's folder until it finishes, and a folder some program still has
  // open cannot be deleted on Windows (EPERM).
  const inFlight = new Set<Promise<void>>();
  const server = createServer((req, res) => {
    const handled: Promise<void> = answer(req, res, root, token)
      .catch(() => {
        if (!res.headersSent) res.writeHead(500);
        res.end();
      })
      .finally(() => inFlight.delete(handled));
    inFlight.add(handled);
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;

  return {
    url: `http://127.0.0.1:${port}/repo.git`,
    token,
    bare,
    git: (...args) => {
      const out = spawnSync("git", [`--git-dir=${bare}`, ...args], { encoding: "utf8" });
      return out.status === 0 ? out.stdout.trim() : "";
    },
    close: async () => {
      await closeServer(server);
      await Promise.allSettled([...inFlight]); // every `git` a request started has exited
      rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    },
  };
}

function run(args: string[]): void {
  const out = spawnSync("git", args, { encoding: "utf8" });
  if (out.status !== 0) throw new Error(`git ${args.join(" ")} failed: ${out.stderr}`);
}

function closeServer(server: Server): Promise<void> {
  return new Promise((resolve) => {
    server.closeAllConnections();
    server.close(() => resolve());
  });
}

async function readBody(req: IncomingMessage): Promise<Buffer> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) chunks.push(chunk as Buffer);
  return Buffer.concat(chunks);
}

/** The user name when the Basic credentials carry `token` as their password, otherwise null. */
function authorizedUser(header: string | undefined, token: string): string | null {
  const match = /^Basic\s+(.+)$/i.exec(header ?? "");
  if (!match) return null;
  const pair = Buffer.from(match[1], "base64").toString("utf8");
  const colon = pair.indexOf(":");
  if (colon < 0 || pair.slice(colon + 1) !== token) return null;
  return pair.slice(0, colon) || "anonymous";
}

async function answer(
  req: IncomingMessage,
  res: ServerResponse,
  projectRoot: string,
  token: string,
): Promise<void> {
  const body = await readBody(req);
  const user = authorizedUser(req.headers.authorization, token);
  if (!user) {
    res.writeHead(401, { "WWW-Authenticate": 'Basic realm="notes"', "Content-Length": "0" });
    res.end();
    return;
  }

  const target = req.url ?? "/";
  const q = target.indexOf("?");
  const path = q >= 0 ? target.slice(0, q) : target;
  const query = q >= 0 ? target.slice(q + 1) : "";

  const child = spawn("git", ["http-backend"], {
    env: {
      ...process.env,
      GIT_PROJECT_ROOT: projectRoot,
      GIT_HTTP_EXPORT_ALL: "1",
      REQUEST_METHOD: req.method ?? "GET",
      PATH_INFO: decodeURIComponent(path),
      QUERY_STRING: query,
      CONTENT_TYPE: req.headers["content-type"] ?? "",
      CONTENT_LENGTH: String(body.length),
      REMOTE_USER: user,
      REMOTE_ADDR: "127.0.0.1",
    },
    stdio: ["pipe", "pipe", "ignore"],
  });
  const out: Buffer[] = [];
  child.stdout.on("data", (chunk: Buffer) => out.push(chunk));
  const exited = new Promise<void>((resolve, reject) => {
    child.on("error", reject);
    child.on("close", () => resolve());
  });
  child.stdin.end(body);
  await exited;

  const raw = Buffer.concat(out);
  let end = raw.indexOf("\r\n\r\n");
  let gap = 4;
  if (end < 0) {
    end = raw.indexOf("\n\n");
    gap = 2;
  }
  if (end < 0) {
    end = raw.length;
    gap = 0;
  }
  const head = raw.subarray(0, end).toString("utf8");
  const content = raw.subarray(Math.min(end + gap, raw.length));
  let status = 200;
  let reason = "OK";
  const headers: Record<string, string> = {};
  for (const line of head.split(/\r?\n/)) {
    const colon = line.indexOf(":");
    if (colon < 0) continue;
    const key = line.slice(0, colon).trim();
    const value = line.slice(colon + 1).trim();
    if (key.toLowerCase() === "status") {
      const [code, ...rest] = value.split(" ");
      status = Number(code) || 200;
      reason = rest.join(" ") || reason;
    } else {
      headers[key] = value;
    }
  }
  headers["Content-Length"] = String(content.length);
  res.writeHead(status, reason, headers);
  res.end(content);
}
