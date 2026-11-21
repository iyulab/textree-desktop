import { describe, expect, it } from "vitest";
import { tabStop } from "./treeFocus.helpers";

describe("the tree item Tab goes to", () => {
  it("is the one focused last in its folder", () => {
    const focused = { root: "a", path: "notes/plan.md" };
    expect(tabStop(focused, "a", "notes/plan.md", false, 3)).toBe(0);
    expect(tabStop(focused, "a", "top.md", true, 0)).toBe(-1);
  });

  it("is each other folder's first item, even with a note of the same name", () => {
    const focused = { root: "a", path: "plan.md" };
    expect(tabStop(focused, "b", "plan.md", true, 1)).toBe(-1);
    expect(tabStop(focused, "b", "first.md", true, 0)).toBe(0);
    expect(tabStop(focused, "b", "inside.md", false, 0)).toBe(-1);
  });

  it("is a folder's first item before anything was focused", () => {
    expect(tabStop(null, "a", "first.md", true, 0)).toBe(0);
    expect(tabStop(null, "a", "second.md", true, 1)).toBe(-1);
  });
});
