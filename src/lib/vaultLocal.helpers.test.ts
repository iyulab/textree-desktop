import { describe, expect, it } from "vitest";
import { moveVaultLists, readVaultList, vaultListKey, writeVaultList } from "./vaultLocal.helpers";

function memory(): Storage {
  const m = new Map<string, string>();
  return {
    get length() {
      return m.size;
    },
    clear: () => m.clear(),
    getItem: (k) => m.get(k) ?? null,
    key: (i) => [...m.keys()][i] ?? null,
    removeItem: (k) => void m.delete(k),
    setItem: (k, v) => void m.set(k, v),
  };
}

describe("lists kept per vault on this device", () => {
  it("each vault keeps its own list, even of notes with the same names", () => {
    const s = memory();
    writeVaultList(s, "recent", "C:\\one", ["a.md"]);
    writeVaultList(s, "recent", "C:\\two", ["b.md"]);
    expect(readVaultList(s, "recent", "c:/one/")).toEqual(["a.md"]);
    expect(readVaultList(s, "recent", "C:\\two")).toEqual(["b.md"]);
  });

  it("the first time, takes this vault's entries from the list kept for all of them", () => {
    const s = memory();
    s.setItem("recent", JSON.stringify(["C:\\one\\a.md", "C:\\two\\b.md", "C:\\one\\sub\\c.md", "C:\\one"]));
    expect(readVaultList(s, "recent", "C:\\one")).toEqual(["a.md", "sub/c.md"]);
    expect(s.getItem("recent")).not.toBeNull(); // the other vault still finds its entries
    expect(s.getItem(vaultListKey("recent", "C:\\one"))).toBeNull(); // nothing written by reading
  });

  it("a damaged list or no storage reads as empty", () => {
    const s = memory();
    s.setItem(vaultListKey("recent", "C:\\one"), "not json");
    s.setItem("recent", "also not");
    expect(readVaultList(s, "recent", "C:\\one")).toEqual([]);
    expect(readVaultList(undefined, "recent", "C:\\one")).toEqual([]);
  });
});

describe("a notes folder located somewhere else", () => {
  it("takes its lists along, and never over lists its new place has", () => {
    const s = memory();
    writeVaultList(s, "recent", "C:\\Notes", ["a.md"]);
    writeVaultList(s, "folded", "C:\\Notes", ["x"]);
    writeVaultList(s, "folded", "D:\\Notes", ["y"]);
    moveVaultLists(s, ["recent", "folded"], "C:\\Notes", "D:\\Notes");
    expect(readVaultList(s, "recent", "D:\\Notes")).toEqual(["a.md"]);
    expect(s.getItem(vaultListKey("recent", "C:\\Notes"))).toBeNull();
    expect(readVaultList(s, "folded", "D:\\Notes")).toEqual(["y"]);
    expect(readVaultList(s, "folded", "C:\\Notes")).toEqual(["x"]);
  });

  it("changes nothing when the path is the same however written", () => {
    const s = memory();
    writeVaultList(s, "recent", "/n/Notes", ["a.md"]);
    moveVaultLists(s, ["recent"], "/n/Notes", "/n/Notes/");
    expect(readVaultList(s, "recent", "/n/Notes")).toEqual(["a.md"]);
    moveVaultLists(undefined, ["recent"], "/n/Notes", "/m"); // no storage: nothing to do
  });
});
