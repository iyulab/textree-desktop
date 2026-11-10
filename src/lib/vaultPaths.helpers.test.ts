import { describe, expect, it } from "vitest";
import {
  folderBody,
  joinPath,
  noteStem,
  parentDir,
  pathInside,
  remapAdopted,
  remapMoved,
  remapPromoted,
  relativeTo,
  remapRenamed,
  samePath,
} from "./vaultPaths.helpers";

describe("vault paths", () => {
  it("compare ignoring a trailing slash and case", () => {
    expect(samePath("A/n.md", "a/N.md")).toBe(true);
    expect(pathInside("a/n.md", "a/")).toBe(true);
    expect(pathInside("ab/n.md", "a")).toBe(false);
  });

  it("the vault itself is the empty path, above everything in it", () => {
    expect(pathInside("a/n.md", "")).toBe(true);
    expect(pathInside("n.md", "")).toBe(true);
    expect(parentDir("n.md")).toBe("");
    expect(parentDir("a/b/n.md")).toBe("a/b");
    expect(joinPath("", "n.md")).toBe("n.md");
    expect(joinPath("a", "n.md")).toBe("a/n.md");
  });

  it("an absolute path kept from before reads as the vault names it", () => {
    expect(relativeTo("C:\\Notes", "c:/notes/a/b.md")).toBe("a/b.md");
    expect(relativeTo("C:\\Notes\\", "c:\\NOTES\\Sub\\X.md")).toBe("Sub/X.md"); // the rest keeps its case
    expect(relativeTo("C:\\Notes\\", "C:\\Notes")).toBe("");
    expect(relativeTo("C:\\Notes", "C:\\Notes2\\b.md")).toBeNull();
    expect(relativeTo("C:\\Notes", "D:\\other\\b.md")).toBeNull();
  });

  it("name a note by its file and a folder's body by the folder", () => {
    expect(noteStem("a/Note.MD")).toBe("Note");
    expect(folderBody("x/a")).toBe("x/a/a.md");
    expect(folderBody("a")).toBe("a/a.md");
  });
});

describe("where a structure change puts a path", () => {
  it("a move carries everything under the moved node and nothing else", () => {
    const r = remapMoved("a", "dest/a");
    expect(r("a/x.md")).toBe("dest/a/x.md");
    expect(r("ab.md")).toBe("ab.md");
  });

  it("a move to the top of the vault", () => {
    const r = remapMoved("dest/a", "a");
    expect(r("dest/a/x.md")).toBe("a/x.md");
  });

  it("renaming a note moves only that note", () => {
    const r = remapRenamed("old.md", "new.md", false);
    expect(r("old.md")).toBe("new.md");
    expect(r("other.md")).toBe("other.md");
  });

  it("renaming a folder renames its body note with it and keeps the other notes' names", () => {
    const r = remapRenamed("old", "new", true);
    expect(r("old/old.md")).toBe("new/new.md");
    expect(r("old/child.md")).toBe("new/child.md");
    expect(r("old/sub/old.md")).toBe("new/sub/old.md");
  });

  it("a promoted note becomes its folder's body", () => {
    const r = remapPromoted("leaf.md", "leaf");
    expect(r("leaf.md")).toBe("leaf/leaf.md");
    expect(r("other.md")).toBe("other.md");
  });

  it("adopting moves the dropped note into the promoted one, which becomes the body", () => {
    const r = remapAdopted("guest.md", "host.md", "host/guest.md");
    expect(r("host.md")).toBe("host/host.md");
    expect(r("guest.md")).toBe("host/guest.md");
    expect(r("other.md")).toBe("other.md");
  });
});
