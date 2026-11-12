/** localStorage key holding the absolute path of the last opened vault (device-bound). */
export const LAST_VAULT_KEY = "textree-last-vault";

export type StartupPlan = { action: "restore"; path: string } | { action: "default" };

/**
 * Decides what to open at startup: the folder this device opened last; else the first of the
 * notes folders the device keeps (`kept` — the list outlives this device's browser storage); else,
 * on a first run, the default vault, created or opened. Pure — the caller performs the
 * localStorage/IPC effects.
 */
export function decideStartup(lastVault: string | null, kept: readonly string[] = []): StartupPlan {
  if (lastVault && lastVault.length > 0) return { action: "restore", path: lastVault };
  const first = kept.find((path) => path.length > 0);
  if (first) return { action: "restore", path: first };
  return { action: "default" };
}
