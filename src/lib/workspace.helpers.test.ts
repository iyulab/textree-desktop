import { describe, expect, it } from "vitest";
import type { RootId } from "./ipc";
import { fileManagerAction, folderName, isNestingRefusal, movedRoot, sidebarName } from "./workspace.helpers";

const id = (s: string) => s as RootId;

describe("sidebar names", () => {
  it("is the folder's own name, whichever separator its path uses", () => {
    expect(folderName("/data/notes")).toBe("notes");
    expect(folderName("D:\\data\\work notes\\")).toBe("work notes");
    expect(folderName("/")).toBe("/");
  });

  it("is the name the person gave it, unless that is blank", () => {
    expect(sidebarName({ path: "/n/a", label: "Work" })).toBe("Work");
    expect(sidebarName({ path: "/n/a", label: "  " })).toBe("a");
    expect(sidebarName({ path: "/n/a" })).toBe("a");
  });
});

describe("moving a folder in the sidebar", () => {
  const ids = [id("a"), id("b"), id("c")];

  it("moves it up or down one place", () => {
    expect(movedRoot(ids, id("b"), -1)).toEqual(["b", "a", "c"]);
    expect(movedRoot(ids, id("b"), 1)).toEqual(["a", "c", "b"]);
  });

  it("leaves the order as it is at either end or for a folder not listed", () => {
    expect(movedRoot(ids, id("a"), -1)).toBe(ids);
    expect(movedRoot(ids, id("c"), 1)).toBe(ids);
    expect(movedRoot(ids, id("z"), 1)).toBe(ids);
  });
});

describe("adding a folder the sidebar refuses", () => {
  it("is told apart from other failures by what the sidebar says", () => {
    expect(isNestingRefusal("This folder is already in the sidebar inside Work.")).toBe(true);
    expect(isNestingRefusal("Work is inside this folder.")).toBe(true);
    expect(isNestingRefusal("Access is denied. (os error 5)")).toBe(false);
  });
});

describe("showing a folder in the file manager", () => {
  it("is called what the system calls it", () => {
    expect(fileManagerAction("MacIntel")).toBe("Show in Finder");
    expect(fileManagerAction("Win32")).toBe("Show in File Explorer");
  });
});
