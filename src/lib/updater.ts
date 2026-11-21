import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { stopHost } from "./ipc";

export interface UpdateInfo {
  version: string;
  notes: string;
  handle: Update;
}

/**
 * Checks for an available update. Returns null when up to date or when the updater is
 * unavailable (e.g. dev build, non-Windows). Never throws to the caller — startup must
 * proceed even if the update check fails (graceful degradation).
 */
export async function checkForUpdate(): Promise<UpdateInfo | null> {
  // A development or end-to-end build is not what the published update replaces: offering it would
  // install the released app over the one being worked on, and what the window shows would depend
  // on whether a newer release happens to be out.
  if (import.meta.env.DEV || import.meta.env.VITE_TEXTREE_E2E === "1") return null;
  try {
    const update = await check();
    if (!update) return null;
    return { version: update.version, notes: update.body ?? "", handle: update };
  } catch {
    return null;
  }
}

/**
 * Downloads and installs the given update, then relaunches. Only called after explicit
 * user consent (button click) — never silently (content-safety guard).
 */
export async function applyUpdate(info: UpdateInfo): Promise<void> {
  // Stop the local-AI sidecar before the installer runs. The NSIS updater hard-kills
  // only the main binary, which orphans textree-host.exe and leaves it holding a file
  // lock on its own .exe — so the installer cannot overwrite it ("Error opening file
  // for writing"). Stopping it here unloads the model gracefully; the installer's
  // PREINSTALL hook is the backstop if this races or the host was orphaned earlier.
  // Best-effort: never block the update if the host refuses to stop.
  try {
    await stopHost();
  } catch {
    // ignore — the PREINSTALL hook kills any surviving host before extraction
  }
  await info.handle.downloadAndInstall();
  await relaunch();
}
