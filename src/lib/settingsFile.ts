import type { RootId } from "./ipc";
/**
 * Reads one of a folder's settings files for the stores that keep them (favorites, order, views).
 *
 * A file that cannot be read is moved aside — kept, under a name that says so — and the store starts
 * from its default. Without that, the store's next save would write the default over it. When it
 * cannot even be moved aside, the store is told not to write that file at all this time.
 */

import { readSidecar, setAsideSidecar } from "./ipc";
import { parseSettings } from "./settingsFile.helpers";

export interface SettingsRead<T> {
  /** What the file holds, or null to start from the default. */
  value: T | null;
  /** The name the unreadable file was kept under, when it was moved aside. */
  setAside: string | null;
  /** Whether saving over this file is safe. */
  writable: boolean;
}

export async function readSettings<T>(
  root: RootId,
  rel: string,
  isShape: (value: unknown) => value is T,
): Promise<SettingsRead<T>> {
  let raw: string | null;
  try {
    raw = await readSidecar(root, rel);
  } catch (e) {
    // Not known to be absent, not known to be damaged: leave it alone.
    console.warn(`Settings read failed (${rel}):`, e);
    return { value: null, setAside: null, writable: false };
  }
  const parsed = parseSettings(raw, isShape);
  if (parsed.kind === "ok") return { value: parsed.value, setAside: null, writable: true };
  if (parsed.kind === "absent") return { value: null, setAside: null, writable: true };
  try {
    return { value: null, setAside: await setAsideSidecar(root, rel), writable: true };
  } catch (e) {
    console.warn(`Settings file could not be read or set aside (${rel}):`, e);
    return { value: null, setAside: null, writable: false };
  }
}
