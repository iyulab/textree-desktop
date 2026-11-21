import { describe, expect, it } from "vitest";
import { decodePicked, encodePicked, samePicked } from "./treeDrag.helpers";

describe("a node picked up in the sidebar", () => {
  it("carries its notes folder with its path", () => {
    const picked = { root: "r1", path: "a/b.md" };
    expect(decodePicked(encodePicked(picked))).toEqual(picked);
  });

  it("is nothing when the drag carries something else", () => {
    expect(decodePicked(undefined)).toBeNull();
    expect(decodePicked("")).toBeNull();
    expect(decodePicked("a/b.md")).toBeNull();
    expect(decodePicked('{"path":"a.md"}')).toBeNull();
  });

  it("is the same node only in the same notes folder", () => {
    expect(samePicked({ root: "r1", path: "a.md" }, { root: "r1", path: "a.md" })).toBe(true);
    expect(samePicked({ root: "r1", path: "a.md" }, { root: "r2", path: "a.md" })).toBe(false);
    expect(samePicked(null, { root: "r1", path: "a.md" })).toBe(false);
  });
});
