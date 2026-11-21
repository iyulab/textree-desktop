// Lists of notes this device keeps for each vault (recently opened, folders collapsed) — in the
// browser storage, one list per vault. Paths in them are relative to the vault, so a list kept
// without its vault would mix two vaults' notes of the same name.

import { relativeTo } from "./vaultPaths.helpers";

/** The storage key of `base`'s list for the vault at `root`. */
export function vaultListKey(base: string, root: string): string {
  return `${base}:${root.replace(/\\/g, "/").replace(/\/+$/, "").toLowerCase()}`;
}

function parsed(raw: string | null): string[] | null {
  if (raw === null) return null;
  try {
    const v: unknown = JSON.parse(raw);
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : null;
  } catch {
    return null;
  }
}

/**
 * `base`'s list for the vault at `root`. The first time, the list kept for every vault at once
 * (absolute paths, from before) gives this vault its entries; the old list stays for the others.
 */
export function readVaultList(store: Storage | undefined, base: string, root: string): string[] {
  if (!store) return [];
  try {
    const own = parsed(store.getItem(vaultListKey(base, root)));
    if (own) return own;
    const shared = parsed(store.getItem(base)) ?? [];
    return shared.map((p) => relativeTo(root, p)).filter((p): p is string => p !== null && p !== "");
  } catch {
    return [];
  }
}

export function writeVaultList(store: Storage | undefined, base: string, root: string, list: string[]): void {
  try {
    store?.setItem(vaultListKey(base, root), JSON.stringify(list));
  } catch {
    // Storage may be unavailable or full: the list is a convenience.
  }
}

/**
 * Moves the device-local lists kept for the notes folder at `from` (under each of `bases`) to the
 * folder's new path `to` — after it was located somewhere else. A list the new path already has is
 * kept, not overwritten; the old one is then left as it was.
 */
export function moveVaultLists(store: Storage | undefined, bases: readonly string[], from: string, to: string): void {
  if (!store) return;
  for (const base of bases) {
    try {
      const [old, now] = [vaultListKey(base, from), vaultListKey(base, to)];
      const kept = store.getItem(old);
      if (kept === null || old === now || store.getItem(now) !== null) continue;
      store.setItem(now, kept);
      store.removeItem(old);
    } catch {
      // Storage may be unavailable or full: the lists are a convenience.
    }
  }
}
