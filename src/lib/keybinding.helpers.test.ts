import { expect, test } from "vitest";
import {
  matchKeybinding,
  formatKeybinding,
  isFormFieldTag,
  type KeyEventLike,
} from "./keybinding.helpers";

const ev = (over: Partial<KeyEventLike>): KeyEventLike => ({
  ctrlKey: false,
  metaKey: false,
  shiftKey: false,
  altKey: false,
  key: "",
  ...over,
});

test("matchKeybinding: mod+n matches Ctrl+N", () => {
  expect(matchKeybinding("mod+n", ev({ ctrlKey: true, key: "n" }))).toBe(true);
});

test("matchKeybinding: mod maps to Cmd (metaKey) too", () => {
  expect(matchKeybinding("mod+n", ev({ metaKey: true, key: "n" }))).toBe(true);
});

test("matchKeybinding: key match is case-insensitive", () => {
  expect(matchKeybinding("mod+n", ev({ ctrlKey: true, key: "N" }))).toBe(true);
});

test("matchKeybinding: mod+n rejects when Shift is also held", () => {
  expect(matchKeybinding("mod+n", ev({ ctrlKey: true, shiftKey: true, key: "n" }))).toBe(false);
});

test("matchKeybinding: mod+shift+n requires Shift", () => {
  expect(matchKeybinding("mod+shift+n", ev({ ctrlKey: true, shiftKey: true, key: "n" }))).toBe(true);
  expect(matchKeybinding("mod+shift+n", ev({ ctrlKey: true, key: "n" }))).toBe(false);
});

test("matchKeybinding: a modifier binding rejects a bare key press", () => {
  expect(matchKeybinding("mod+n", ev({ key: "n" }))).toBe(false);
});

test("matchKeybinding: rejects a different key", () => {
  expect(matchKeybinding("mod+n", ev({ ctrlKey: true, key: "m" }))).toBe(false);
});

test("formatKeybinding: renders a Windows-first label", () => {
  expect(formatKeybinding("mod+n")).toBe("Ctrl+N");
  expect(formatKeybinding("mod+shift+n")).toBe("Ctrl+Shift+N");
});

test("isFormFieldTag: native form controls suppress accelerators", () => {
  expect(isFormFieldTag("INPUT")).toBe(true);
  expect(isFormFieldTag("textarea")).toBe(true); // case-insensitive
  expect(isFormFieldTag("SELECT")).toBe(true);
});

test("isFormFieldTag: the editor and other elements do not (editor Ctrl+N is intentional)", () => {
  expect(isFormFieldTag("DIV")).toBe(false); // contenteditable editor host
  expect(isFormFieldTag("BUTTON")).toBe(false);
  expect(isFormFieldTag(null)).toBe(false);
  expect(isFormFieldTag(undefined)).toBe(false);
});

test("formatKeybinding: on macOS, the symbols and order the system's menus use", () => {
  expect(formatKeybinding("mod+n", true)).toBe("⌘N");
  expect(formatKeybinding("mod+shift+n", true)).toBe("⇧⌘N");
  expect(formatKeybinding("mod+alt+shift+s", true)).toBe("⌥⇧⌘S");
  expect(formatKeybinding("mod+n", false)).toBe("Ctrl+N");
});
