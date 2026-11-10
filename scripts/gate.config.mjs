/*
 * The merge gate's steps, in the order CI runs them, plus the checks CI does not run (clippy, the
 * host test suite). `--ship` is the pre-release gate: assemble both sidecars first (the renderer at
 * the commit a release ships), then every merge step, then the E2E suite and host:smoke on the
 * assembled host. Run with `npm run gate` (see package.json); the runner is tauri-kit-dev's.
 *
 * Without --ship the sidecars must be assembled already (scripts/assemble-*-sidecar.ps1) — the Rust
 * build bundles them. The release steps stay out of the plain gate: host:smoke needs a model cache
 * and E2E needs a desktop session, which a merge gate must not depend on. With --ship they run in
 * the order that keeps each honest: the assemblies come first, so the E2E suite renders with the
 * shipped canopy and host:smoke checks a build of the current source.
 */

import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { runningDevApps } from "./dev-processes.mjs";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const HOST_SOLUTION = "src-host/TextreeHost.slnx";
const CARGO_MANIFEST = "src-tauri/Cargo.toml";
const ship = (step) => ({ ...step, local: true, optIn: "--ship" });

export default {
  steps: [
    ship({ name: "assemble-canopy", cmd: "pwsh -NoProfile -File scripts/assemble-canopy-sidecar.ps1" }),
    ship({ name: "assemble-host", cmd: "pwsh -NoProfile -File scripts/assemble-host-sidecar.ps1" }),
    // Same order as .github/workflows/ci.yml; `local` marks steps CI does not run.
    { name: "audit", cmd: "npm audit --omit=dev --audit-level=high" },
    { name: "tokens", cmd: "npm run tokens:check" },
    { name: "check", cmd: "npm run check" },
    { name: "unit", cmd: "npm run test:unit" },
    { name: "build", cmd: "npm run build" },
    // The release build refuses Tauri crates and npm packages on different minor releases; a
    // development build does not check, so it is checked here before anything is tagged.
    { name: "tauri-pairs", cmd: "npm run tauri:pairs", local: true },
    { name: "restore", cmd: `dotnet restore ${HOST_SOLUTION}` },
    { name: "pins", cmd: "npm run pins:check" },
    { name: "notices", cmd: "npm run notices:check" }, // reads the host's restored graph
    { name: "host-test", cmd: `dotnet test ${HOST_SOLUTION} --no-restore`, local: true },
    { name: "fmt", cmd: `cargo fmt --manifest-path ${CARGO_MANIFEST} --check` },
    { name: "cargo-test", cmd: `cargo test --manifest-path ${CARGO_MANIFEST}` },
    { name: "clippy", cmd: `cargo clippy --manifest-path ${CARGO_MANIFEST} --all-targets -- -D warnings`, local: true },
    ship({ name: "e2e", cmd: "npm run e2e:run" }),
    ship({ name: "smoke", cmd: "npm run host:smoke -- --exe src-tauri/resources/host/textree-host.exe" }),
  ],

  preflight(selected) {
    for (const [sidecar, assembledBy] of [
      ["src-tauri/resources/canopy/node_modules/@iyulab/canopy/dist/cli.js", "assemble-canopy"],
      ["src-tauri/resources/host/textree-host.exe", "assemble-host"],
    ]) {
      if (!existsSync(join(REPO, sidecar)) && !selected.some((s) => s.name === assembledBy)) {
        console.warn(`[gate] warning: ${sidecar} is missing — assemble the sidecars first or the Rust steps may fail`);
      }
    }
    // A running dev build holds its own executable open, and cargo then fails to replace it with
    // "Access is denied (os error 5)" — a failure that names neither the cause nor the fix.
    if (!selected.some((s) => s.name.startsWith("cargo") || s.name === "clippy")) return;
    // Only the app's own executable is what cargo rebuilds; a host sidecar left running does not block it.
    const apps = runningDevApps(REPO).filter((a) => a.name === "textree.exe");
    if (apps.length > 0) {
      return (
        `the dev build is running (pid ${apps.map((a) => a.pid).join(", ")}); cargo cannot replace ` +
        `its executable. Close it (or run \`npm run e2e:run\`, which stops what it starts) and retry.`
      );
    }
  },
};
