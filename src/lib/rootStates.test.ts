import { describe, expect, it } from "vitest";
import { RootStates } from "./rootStates";

describe("RootStates", () => {
  it("makes a folder's state once and gives the same one back", () => {
    const states = new RootStates<string, { n: number }>();
    let made = 0;
    const make = () => ({ n: ++made });
    const a = states.of("a", make);
    expect(states.of("a", make)).toBe(a);
    expect(made).toBe(1);
  });

  it("keeps each folder's state apart", () => {
    const states = new RootStates<string, string[]>();
    states.of("a", () => []).push("a note");
    expect(states.of("b", () => [])).toEqual([]);
    expect(states.get("a")).toEqual(["a note"]);
  });

  it("lets go of every folder but one", () => {
    const states = new RootStates<string, number>();
    states.of("a", () => 1);
    states.of("b", () => 2);
    states.of("c", () => 3);
    states.keepOnly("b");
    expect(states.ids()).toEqual(["b"]);
    expect(states.get("a")).toBeUndefined();
  });

  it("drops one folder and makes a fresh state when it is asked for again", () => {
    const states = new RootStates<string, { n: number }>();
    const first = states.of("a", () => ({ n: 1 }));
    states.drop("a");
    expect(states.get("a")).toBeUndefined();
    expect(states.of("a", () => ({ n: 2 }))).not.toBe(first);
  });
});
