import { describe, expect, it } from "vitest";
import type { RootId } from "./ipc";
import { folderName, movedRoot, sidebarName } from "./workspace.helpers";

const id = (s: string) => s as RootId;

describe("sidebar names", () => {
  it("is the folder's own name, whichever separator its path uses", () => {
    expect(folderName("/Users/me/notes")).toBe("notes");
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
