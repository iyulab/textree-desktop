import { describe, it, expect } from "vitest";
import {
  paletteMode,
  paletteTerm,
  paletteListState,
  paletteStatusText,
  contentSearchFolders,
} from "./palette.helpers";

describe("paletteMode", () => {
  it("'>' prefix is command", () => expect(paletteMode(">new note")).toBe("command"));
  it("'/' prefix is content", () => expect(paletteMode("/full text")).toBe("content"));
  it("otherwise file", () => expect(paletteMode("note")).toBe("file"));
  it("empty string is file", () => expect(paletteMode("")).toBe("file"));
});

describe("paletteTerm", () => {
  it("command strips '>' + left trim", () => expect(paletteTerm(">  open")).toBe("open"));
  it("content strips '/' + left trim", () => expect(paletteTerm("/  search")).toBe("search"));
  it("file keeps the original", () => expect(paletteTerm("note ")).toBe("note "));
});

describe("paletteListState", () => {
  it("reports results whenever there is at least one match", () => {
    expect(paletteListState({ mode: "file", term: "no", count: 3, searching: false })).toBe(
      "results",
    );
  });

  it("keeps showing results during an in-flight content search (no flicker to empty)", () => {
    expect(paletteListState({ mode: "content", term: "abc", count: 2, searching: true })).toBe(
      "results",
    );
  });

  it("reports searching for a content query in flight with no results yet", () => {
    expect(paletteListState({ mode: "content", term: "abc", count: 0, searching: true })).toBe(
      "searching",
    );
  });

  it("reports no-results for a settled content search that found nothing", () => {
    expect(paletteListState({ mode: "content", term: "abc", count: 0, searching: false })).toBe(
      "no-results",
    );
  });

  it("reports no-results when a file query matches nothing", () => {
    expect(paletteListState({ mode: "file", term: "zzz", count: 0, searching: false })).toBe(
      "no-results",
    );
  });

  it("reports no-results when a command query matches nothing", () => {
    expect(paletteListState({ mode: "command", term: "zzz", count: 0, searching: false })).toBe(
      "no-results",
    );
  });

  it("reports empty (not no-results) when the query is blank and nothing is listed", () => {
    expect(paletteListState({ mode: "file", term: "", count: 0, searching: false })).toBe("empty");
  });

  it("reports empty for a blank content query (idle, before typing)", () => {
    expect(paletteListState({ mode: "content", term: "", count: 0, searching: false })).toBe(
      "empty",
    );
  });

  it("reports searching for semantic mode in-flight", () => {
    expect(paletteListState({ mode: "semantic", term: "ideas", count: 0, searching: true })).toBe("searching");
  });
});

describe("paletteMode semantic", () => {
  it("treats ? prefix as semantic", () => {
    expect(paletteMode("?ideas about trees")).toBe("semantic");
  });
  it("strips ? for the term", () => {
    expect(paletteTerm("?ideas")).toBe("ideas");
  });
  it("keeps existing modes", () => {
    expect(paletteMode(">cmd")).toBe("command");
    expect(paletteMode("/text")).toBe("content");
    expect(paletteMode("file")).toBe("file");
  });
});

describe("paletteStatusText", () => {
  it("says where content and semantic search look when several folders are shown", () => {
    expect(paletteStatusText("empty", "content", "Work")).toBe("Searches the notes in Work only");
    expect(paletteStatusText("searching", "semantic", "Work")).toBe("Searching Work…");
    expect(paletteStatusText("no-results", "content", "Work")).toBe("No matches in Work");
    expect(paletteStatusText("results", "content", "Work")).toBeNull();
  });
  it("keeps the plain rows with one folder, and for finding by name", () => {
    expect(paletteStatusText("empty", "content", null)).toBeNull();
    expect(paletteStatusText("searching", "content", null)).toBe("Searching…");
    expect(paletteStatusText("no-results", "file", "Work")).toBe("No matches found");
    expect(paletteStatusText("empty", "file", "Work")).toBeNull();
  });
});

describe("content search over several notes folders", () => {
  it("looking in all of them says so only when searching or when nothing matched", () => {
    expect(paletteStatusText("empty", "content", "Work", "all")).toBeNull();
    expect(paletteStatusText("searching", "content", "Work", "all")).toBe("Searching all notes folders…");
    expect(paletteStatusText("no-results", "content", "Work", "all")).toBe("No matches in any notes folder");
  });
  it("semantic search stays in the open folder whatever content search is set to", () => {
    expect(paletteStatusText("empty", "semantic", "Work", "all")).toBe("Searches the notes in Work only");
  });
  it("asks every shown folder in sidebar order, or the open one alone", () => {
    expect(contentSearchFolders("all", ["a", "b", "c"], "b")).toEqual(["a", "b", "c"]);
    expect(contentSearchFolders("here", ["a", "b", "c"], "b")).toEqual(["b"]);
    expect(contentSearchFolders("all", ["a"], "a")).toEqual(["a"]);
    expect(contentSearchFolders("all", ["a", "b"], null)).toEqual([]);
  });
});
