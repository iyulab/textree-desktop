#!/usr/bin/env node
/**
 * Which source an assembled sidecar was built from.
 *
 * Assembled sidecars live in gitignored folders and are not rebuilt when their source changes, so
 * after a branch switch they can be builds of other code entirely — and the checks that use them
 * (`host:smoke --exe`, the E2E suite) would then verify that other build and pass. Assembling
 * records a stamp; the checks compare it with what they mean to test:
 *
 *   host    against the host source as it stands now (src-host/) and the app version it is
 *           stamped with (src-tauri/tauri.conf.json — assembling passes it to the build)
 *   canopy  against the renderer release the app ships (canopy-sidecar/) — it can also be built
 *           from a source checkout, to try a renderer change before it is released
 *
 * Stamps live under .cache/, not beside the payloads: everything under src-tauri/resources/ is
 * bundled into the installer. Each is tied to its payload by hash, so a payload replaced by hand
 * reads as unknown rather than inheriting a stamp that is not its own.
 *
 *   node scripts/sidecar-provenance.mjs stamp host               # assemble-host-sidecar.ps1 calls it
 *   node scripts/sidecar-provenance.mjs stamp canopy [checkout]  # assemble-canopy-sidecar.ps1 calls it
 *   node scripts/sidecar-provenance.mjs check <host|canopy>      # prints the verdict; exit 1 unless current/pinned
 */
import { existsSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  filesDigest,
  filesUnder,
  gitSourceState,
  judgeStamp,
  pendingDigest,
  readStamp as readStampAt,
  sha256,
  writeStamp as writeStampAt,
} from "@iyulab/tauri-kit-dev/provenance";
import { CANOPY_CLI, CANOPY_MANIFEST_DIR, CANOPY_STAGE, canopyPin } from "./canopy-stage.mjs";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");

/** The staged host executable — named as the platform names executables (the app looks for this name). */
export const HOST_EXE = `src-tauri/resources/host/textree-host${process.platform === "win32" ? ".exe" : ""}`;

export const SIDECARS = {
  host: { exe: HOST_EXE, source: "src-host" },
};
const APP_CONFIG = join(REPO, "src-tauri", "tauri.conf.json");

const stampPath = (kind) => join(REPO, ".cache", "sidecar-provenance", `${kind}.json`);
const readStamp = (kind) => readStampAt(stampPath(kind));
const writeStamp = (kind, record) => writeStampAt(stampPath(kind), record);
// ── host ────────────────────────────────────────────────────────────────────────────────────

/**
 * The source as it stands: the committed tree of the source folder plus a digest of anything
 * uncommitted in it (changes and untracked files), so a build of uncommitted work still matches
 * that same uncommitted work — and nothing else. The app version is part of it: the build is
 * stamped with it, so a version bump outside the source folder still makes an older build stale.
 */
export function sourceState(source) {
  const version = JSON.parse(readFileSync(APP_CONFIG, "utf8")).version ?? null;
  return { ...gitSourceState(REPO, source), version };
}

export function stamp(kind) {
  const { exe, source } = SIDECARS[kind];
  const record = { ...sourceState(source), exeSha256: sha256(readFileSync(join(REPO, exe))), assembledAt: new Date().toISOString() };
  writeStamp(kind, record);
  return record;
}

/**
 * Pure: stamp + exe hash + current source → verdict.
 * current  — built from exactly the source as it stands
 * stale    — built from other source (another commit, or other uncommitted changes) or for
 *            another app version
 * unknown  — no stamp, or the exe is not the one the stamp describes
 */
export function judge(record, exeSha256, now) {
  // A stamp from before versions were recorded has no `version` — it cannot vouch for one.
  const { tree, pending, version } = now;
  const r = judgeStamp(record, { digestKey: "exeSha256", digest: exeSha256, expected: { tree, pending, version } });
  return r.verdict === "unknown" ? r : { verdict: r.verdict === "match" ? "current" : "stale", built: r.built };
}

/** Verdict for `exePath`, or null when it is not the staged sidecar (nothing to compare against). */
export function check(kind, exePath) {
  const { exe, source } = SIDECARS[kind];
  const staged = join(REPO, exe);
  if (resolve(exePath).toLowerCase() !== staged.toLowerCase()) return null;
  if (!existsSync(staged)) return { verdict: "unknown" };
  const now = sourceState(source);
  return { ...judge(readStamp(kind), sha256(readFileSync(staged)), now), now };
}

export function describe(result) {
  const at = (s) => `${s.commit}${s.pending ? " + uncommitted changes" : ""}, version ${s.version ?? "unrecorded"}`;
  switch (result.verdict) {
    case "current":
      return `built from the current source (${at(result.built)})`;
    case "stale":
      return `built from ${at(result.built)}, but the source is now ${at(result.now)} — reassemble with scripts/assemble-host-sidecar.ps1`;
    default:
      return "of unknown origin (no assembly record, or replaced since) — reassemble with scripts/assemble-host-sidecar.ps1";
  }
}

// ── canopy ──────────────────────────────────────────────────────────────────────────────────

const CANOPY_IN_STAGE = join(CANOPY_STAGE, "node_modules", "@iyulab", "canopy");

/**
 * Digest of what decides the assembled renderer: the lock file it was installed from and the
 * renderer package's own files. The rest of node_modules follows from the lock file and the Node
 * runtime from the assembly script's pinned version; hashing them would only make stamping slow.
 */
function canopyStageDigest() {
  return filesDigest(CANOPY_STAGE, [
    join(CANOPY_STAGE, "package-lock.json"),
    ...filesUnder(CANOPY_IN_STAGE, { skip: ["node_modules"] }),
  ]);
}

/** The installed renderer's version, or null when the stage holds none. */
function installedCanopyVersion() {
  const manifest = join(CANOPY_IN_STAGE, "package.json");
  return existsSync(manifest) ? JSON.parse(readFileSync(manifest, "utf8")).version : null;
}

/**
 * `checkout`: the source checkout it was built from, or undefined for the pinned release. A stage
 * installed from the registry counts as the release only when its lock file is the committed one.
 */
export function stampCanopy(checkout) {
  const fromCheckout = checkout !== undefined;
  const committedLock = readFileSync(join(CANOPY_MANIFEST_DIR, "package-lock.json"));
  const record = {
    source: fromCheckout ? "checkout" : "registry",
    version: installedCanopyVersion(),
    commit: fromCheckout ? gitSourceState(checkout, ".").commit : null,
    pending: fromCheckout ? pendingDigest(checkout, ".") : null,
    lockSha256: fromCheckout ? null : sha256(committedLock),
    stageSha256: canopyStageDigest(),
    assembledAt: new Date().toISOString(),
  };
  writeStamp("canopy", record);
  return record;
}

/**
 * Pure: stamp + stage digest + pin + committed lock digest → verdict.
 * pinned    — the pinned release, installed from the committed lock file
 * unpinned  — built from a source checkout, or installed from another lock file
 * unknown   — no stamp, or the stage is not the one the stamp describes
 */
export function judgeCanopy(record, stageSha256, pin, lockSha256) {
  const r = judgeStamp(record, {
    digestKey: "stageSha256",
    digest: stageSha256,
    expected: { source: "registry", version: pin, lockSha256 },
  });
  return r.verdict === "unknown" ? { verdict: "unknown", pin } : { verdict: r.verdict === "match" ? "pinned" : "unpinned", built: r.built, pin };
}

export function checkCanopy() {
  const pin = canopyPin();
  if (!existsSync(CANOPY_CLI)) return { verdict: "unknown", pin };
  const lock = sha256(readFileSync(join(CANOPY_MANIFEST_DIR, "package-lock.json")));
  return judgeCanopy(readStamp("canopy"), canopyStageDigest(), pin, lock);
}

export function describeCanopy(result) {
  const reassemble = "reassemble with scripts/assemble-canopy-sidecar.ps1";
  switch (result.verdict) {
    case "pinned":
      return `canopy ${result.pin}, the release the app ships`;
    case "unpinned": {
      const b = result.built;
      const what =
        b.source === "checkout"
          ? `canopy ${b.version ?? "?"} built from ${b.commit?.slice(0, 7) ?? "a checkout"}${b.pending ? " + uncommitted changes" : ""}`
          : `canopy ${b.version ?? "?"} from another lock file`;
      return `${what}, not the release the app ships (${result.pin}) — for the shipped renderer, ${reassemble}`;
    }
    default:
      return `a renderer of unknown origin (no assembly record, or replaced since); the app ships canopy ${result.pin} — ${reassemble}`;
  }
}

// ── command line ────────────────────────────────────────────────────────────────────────────

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [action, kind, checkout] = process.argv.slice(2);
  const valid =
    (kind === "host" && ["stamp", "check"].includes(action)) ||
    (kind === "canopy" && ["stamp", "check"].includes(action));
  if (!valid) {
    console.error(
      "usage: node scripts/sidecar-provenance.mjs <stamp|check> host\n" +
        "       node scripts/sidecar-provenance.mjs stamp canopy [checkout]\n" +
        "       node scripts/sidecar-provenance.mjs check canopy",
    );
    process.exit(2);
  }
  if (kind === "canopy" && action === "stamp") {
    const r = stampCanopy(checkout ? resolve(checkout) : undefined);
    const from = r.source === "checkout" ? `built from ${r.commit.slice(0, 7)}${r.pending ? " + uncommitted changes" : ""}` : "from the registry";
    console.log(`[provenance] canopy sidecar stamped: ${r.version} ${from}`);
  } else if (kind === "canopy") {
    const r = checkCanopy();
    console.log(`[provenance] canopy sidecar: ${describeCanopy(r)}`);
    process.exit(r.verdict === "pinned" ? 0 : 1);
  } else if (action === "stamp") {
    const r = stamp(kind);
    console.log(`[provenance] ${kind} sidecar stamped: ${r.commit}${r.pending ? " + uncommitted changes" : ""}`);
  } else {
    const r = check(kind, join(REPO, SIDECARS[kind].exe));
    console.log(`[provenance] the ${kind} sidecar is ${describe(r)}`);
    process.exit(r.verdict === "current" ? 0 : 1);
  }
}
