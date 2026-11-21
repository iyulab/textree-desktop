/*
 * Keybinding pure helpers — matching keyboard events to bindings and formatting them for display.
 * Bindings are normalized strings: tokens joined by "+", e.g. "mod+n" or "mod+shift+n".
 * "mod" is the platform accelerator (Ctrl on Windows/Linux, Cmd on macOS); the rest are
 * "shift"/"alt" modifiers and a single key token. Kept pure (no DOM) so vitest can cover it.
 */

/** Minimal shape of a KeyboardEvent this module reads (so tests need no real DOM events). */
export interface KeyEventLike {
  ctrlKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
  key: string;
}

function parts(binding: string): string[] {
  return binding.toLowerCase().split("+");
}

/** True iff the event exactly satisfies the binding (modifiers must match precisely, not just include). */
export function matchKeybinding(binding: string, e: KeyEventLike): boolean {
  const tokens = parts(binding);
  const needMod = tokens.includes("mod");
  const needShift = tokens.includes("shift");
  const needAlt = tokens.includes("alt");
  const keyToken = tokens.find((t) => t !== "mod" && t !== "shift" && t !== "alt");

  const modOk = needMod ? e.ctrlKey || e.metaKey : !e.ctrlKey && !e.metaKey;
  return (
    modOk &&
    e.shiftKey === needShift &&
    e.altKey === needAlt &&
    keyToken !== undefined &&
    e.key.toLowerCase() === keyToken
  );
}

/**
 * True for native form controls where a global command accelerator must be suppressed: typing a
 * name into the rename / new-name `<input>` must not let Ctrl+N clobber the in-progress action.
 * Deliberately does NOT cover contenteditable — the editor is a valid place to fire "new note"
 * (starting a note while writing is the shortcut's purpose), so the editor stays unguarded.
 */
export function isFormFieldTag(tagName: string | null | undefined): boolean {
  const t = (tagName ?? "").toUpperCase();
  return t === "INPUT" || t === "TEXTAREA" || t === "SELECT";
}

/**
 * Renders a binding as the system writes shortcuts: "mod+shift+n" → "Ctrl+Shift+N", or on macOS
 * (`mac`) → "⇧⌘N".
 */
export function formatKeybinding(binding: string, mac = false): string {
  const tokens = parts(binding);
  const keyToken = tokens.find((t) => t !== "mod" && t !== "shift" && t !== "alt");
  const key = keyToken ? (keyToken.length === 1 ? keyToken.toUpperCase() : keyToken) : "";
  if (mac) {
    // The order and symbols macOS menus use: ⌥ ⇧ ⌘ then the key, run together.
    let out = "";
    if (tokens.includes("alt")) out += "⌥";
    if (tokens.includes("shift")) out += "⇧";
    if (tokens.includes("mod")) out += "⌘";
    return out + key;
  }
  const out: string[] = [];
  if (tokens.includes("mod")) out.push("Ctrl");
  if (tokens.includes("shift")) out.push("Shift");
  if (tokens.includes("alt")) out.push("Alt");
  if (key) out.push(key);
  return out.join("+");
}
