import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  initialNoteSaveState,
  NoteSave,
  type LeaveQuestion,
  type NoteSaveDeps,
  type NoteSaveState,
} from "./noteSave";
import type { WriteOutcome } from "./ipc";

interface Call {
  path: string;
  text: string;
  expected: string;
  settle: (outcome: WriteOutcome | Error) => void;
}

/** A disk whose writes finish only when the test says so, in any order it chooses. */
function harness(opts: { active?: string; deps?: Partial<NoteSaveDeps> } = {}) {
  let active: string | null = opts.active ?? "A.md";
  const calls: Call[] = [];
  const asked: LeaveQuestion[] = [];
  let removedCount = 0;
  const copies: { path: string; text: string }[] = [];
  const state: NoteSaveState = initialNoteSaveState();
  const save = new NoteSave(state, {
    root: () => "/vault",
    activePath: () => active,
    write: (_root, path, text, expected) =>
      new Promise<WriteOutcome>((resolve, reject) => {
        calls.push({
          path,
          text,
          expected,
          settle: (o) => (o instanceof Error ? reject(o) : resolve(o)),
        });
      }),
    ask: (q) => asked.push(q),
    moved: (remap) => {
      if (active) active = remap(active);
    },
    removed: () => removedCount++,
    keepCopy: async (_root, path, text) => {
      copies.push({ path, text });
      return path.replace(/\.md$/, " (2).md");
    },
    ...opts.deps,
  });
  return {
    save,
    state,
    calls,
    asked,
    copies,
    removedCount: () => removedCount,
    open(path: string | null, text = "") {
      active = path;
      save.opened(text);
    },
  };
}

const written: WriteOutcome = { kind: "written" };

/** Let queued promise callbacks run (the save chain hops a few microtasks between writes). */
async function settle() {
  for (let i = 0; i < 30; i++) await Promise.resolve();
}

beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());

describe("NoteSave — autosave", () => {
  it("saves after a pause in typing, from what the note held when the edit began", async () => {
    const h = harness();
    h.save.opened("old");
    h.save.schedule("A.md", "n");
    h.save.schedule("A.md", "ne");
    h.save.schedule("A.md", "new");
    expect(h.calls).toHaveLength(0);
    vi.advanceTimersByTime(500);
    await settle();
    expect(h.calls.map((c) => [c.text, c.expected])).toEqual([["new", "old"]]);
    h.calls[0].settle(written);
    await settle();
    expect(h.state.dirty).toBe(false);
    expect(h.save.pending).toBeNull();
    expect(h.save.synced).toBe("new");
  });

  it("an edit made while a save runs starts from what that save put on disk", async () => {
    const h = harness();
    h.save.opened("0");
    h.save.schedule("A.md", "1");
    void h.save.flush();
    await settle();
    h.save.schedule("A.md", "2");
    h.calls[0].settle(written);
    await settle();
    expect(h.state.dirty).toBe(true);
    void h.save.flush();
    await settle();
    expect(h.calls[1]).toMatchObject({ text: "2", expected: "1" });
  });

  it("does not save on a timer while the person is asked which copy wins", async () => {
    const h = harness();
    h.save.schedule("A.md", "mine");
    h.save.markConflict("theirs");
    vi.advanceTimersByTime(1000);
    await settle();
    expect(h.calls).toHaveLength(0);
    h.save.keepMine();
    await settle();
    expect(h.calls[0]).toMatchObject({ text: "mine", expected: "theirs" });
  });
});

describe("NoteSave — switching notes", () => {
  it("edits typed into the note being left are written, not replaced by the first edit to the next", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    const leaving = h.save.beforeLeaving();
    await settle();
    h.calls[0].settle(written);
    expect(await leaving).toBe("saved");

    // The next note is still loading, so the editor on screen is still the old one.
    h.save.schedule("A.md", "a2");
    h.open("B.md", "b0");
    h.save.schedule("B.md", "b1");
    await settle();

    expect(h.calls.map((c) => [c.path, c.text, c.expected])).toEqual([
      ["A.md", "a1", "a0"],
      ["A.md", "a2", "a1"],
    ]);
    h.calls[1].settle(written);
    vi.advanceTimersByTime(500);
    await settle();
    expect(h.calls[2]).toMatchObject({ path: "B.md", text: "b1", expected: "b0" });
  });

  it("a note left behind that changed on disk is named, and does not raise the open note's banners", async () => {
    const h = harness();
    h.save.schedule("A.md", "a1");
    h.open("B.md");
    h.save.schedule("B.md", "b1");
    await settle();
    h.calls[0].settle({ kind: "conflict", disk: "elsewhere" });
    await settle();
    // The other change is not overwritten, and the edits are not dropped: they become a copy.
    expect(h.copies).toEqual([{ path: "A.md", text: "a1" }]);
    expect(h.state.saveError?.summary).toContain('"A (2)"');
    expect(h.save.stranded).toBe(0);
    expect(h.state.conflictDisk).toBeNull();
    expect(h.state.saveFailure).toBeNull();
  });

  it("a note left behind whose copy cannot be made keeps the edits and tries again", async () => {
    let fail = true;
    let active: string | null = "A.md";
    const writes: string[] = [];
    const copies: string[] = [];
    const save = new NoteSave(initialNoteSaveState(), {
      root: () => "/v",
      activePath: () => active,
      write: async (_r, path, text) => {
        writes.push(`${path}:${text}`);
        return path === "A.md" ? { kind: "conflict", disk: "elsewhere" } : written;
      },
      keepCopy: async (_r, path, text) => {
        if (fail) throw new Error("disk full");
        copies.push(`${path}:${text}`);
        return "A (2).md";
      },
    });
    save.opened("a0");
    save.schedule("A.md", "a1");
    active = "B.md";
    save.opened("b0");
    save.schedule("B.md", "b1");
    await settle();
    expect(save.stranded).toBe(1);
    expect(save.state.saveError?.summary).toContain('"A"');
    fail = false;
    await save.flush();
    expect(copies).toEqual(["A.md:a1"]);
    expect(save.stranded).toBe(0);
    expect(writes.filter((w) => w.startsWith("A.md"))).toEqual(["A.md:a1", "A.md:a1"]);
  });

  it("typing that goes on while leaving is saved rather than read as a failure", async () => {
    const h = harness();
    h.save.schedule("A.md", "1");
    const leaving = h.save.beforeLeaving();
    for (let i = 2; i <= 3; i++) {
      await settle();
      h.save.schedule("A.md", String(i));
      h.calls.at(-1)!.settle(written);
    }
    await settle();
    h.calls.at(-1)!.settle(written);
    expect(await leaving).toBe("saved");
    expect(h.calls.map((c) => c.text)).toEqual(["1", "2", "3"]);
    expect(h.state.saveFailure).toBeNull();
  });

  it("typing that never stops leaves the edits saving to this note: busy, not failed", async () => {
    const h = harness();
    h.save.schedule("A.md", "0");
    const leaving = h.save.beforeLeaving();
    let n = 0;
    let outcome: string | undefined;
    void leaving.then((o) => (outcome = o));
    while (outcome === undefined) {
      await settle();
      if (outcome !== undefined) break;
      h.save.schedule("A.md", String(++n));
      h.calls.at(-1)!.settle(written);
    }
    expect(outcome).toBe("busy");
    expect(h.asked).toEqual([]);
    expect(h.save.pending?.path).toBe("A.md");
  });

  it("a save that fails keeps the note from being left and points at the failure", async () => {
    const h = harness();
    h.save.schedule("A.md", "1");
    const leaving = h.save.beforeLeaving();
    await settle();
    h.calls[0].settle(new Error("disk full"));
    expect(await leaving).toBe("failed");
    expect(h.asked).toEqual(["failure"]);
    expect(h.state.saveFailure).not.toBeNull();
    expect(h.save.pending?.text).toBe("1");
  });

  it("a conflict found while leaving keeps the note open and asks", async () => {
    const h = harness();
    h.save.schedule("A.md", "mine");
    const leaving = h.save.beforeLeaving();
    await settle();
    h.calls[0].settle({ kind: "conflict", disk: "theirs" });
    expect(await leaving).toBe("asking");
    expect(h.asked).toEqual(["conflict"]);
    expect(h.state.conflictDisk).toBe("theirs");
  });

  it("a note found gone while leaving keeps its edits and asks whether to put it back", async () => {
    const h = harness();
    h.save.schedule("A.md", "mine");
    const leaving = h.save.beforeLeaving();
    await settle();
    h.calls[0].settle({ kind: "conflict", disk: null });
    expect(await leaving).toBe("asking");
    expect(h.asked).toEqual(["removed"]);
    expect(h.state.removed).toBe(true);
    expect(h.state.dirty).toBe(true);
    expect(h.removedCount()).toBe(1);
  });

  it("edits to a note left behind whose write fails are kept, retried, and warned about until they land", async () => {
    const h = harness();
    h.save.opened("a0");
    // Typed into the old note while the next one loads, then the first edit to the next note.
    h.save.schedule("A.md", "a2");
    h.open("B.md", "b0");
    h.save.schedule("B.md", "b1");
    await settle();
    h.calls[0].settle(new Error("disk full"));
    await settle();
    expect(h.save.stranded).toBe(1);
    expect(h.state.saveError?.summary).toContain('"A"');
    expect(h.state.saveFailure).toBeNull(); // the open note has nothing wrong with it

    // The next save writes the open note's edits, then tries the kept ones again.
    vi.advanceTimersByTime(500);
    await settle();
    expect(h.calls[1]).toMatchObject({ path: "B.md", text: "b1" });
    h.calls[1].settle(written);
    await settle();
    expect(h.calls[2]).toMatchObject({ path: "A.md", text: "a2", expected: "a0" });
    h.calls[2].settle(new Error("disk full"));
    await settle();
    // The open note saving fine does not hide that the other one's edits are still not on disk.
    expect(h.save.stranded).toBe(1);
    expect(h.state.saveError?.summary).toContain('"A"');

    h.save.schedule("B.md", "b2");
    vi.advanceTimersByTime(500);
    await settle();
    h.calls[3].settle(written);
    await settle();
    expect(h.calls[4]).toMatchObject({ path: "A.md", text: "a2", expected: "a0" });
    h.calls[4].settle(written);
    await settle();
    expect(h.save.stranded).toBe(0);
    expect(h.state.saveError).toBeNull();
  });

  it("a retry of kept edits that never finishes does not hold up the open note's save", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    h.open("B.md", "b0");
    h.save.schedule("B.md", "b1");
    await settle();
    h.calls[0].settle(new Error("disk full"));
    await settle();
    h.save.schedule("B.md", "b2");
    vi.advanceTimersByTime(500);
    await settle();
    // The open note is written before the kept edits are tried; the retry may hang for all it likes.
    expect(h.calls[1]).toMatchObject({ path: "B.md", text: "b2" });
    h.calls[1].settle(written);
    await settle();
    expect(h.calls[2]).toMatchObject({ path: "A.md", text: "a1" });
    expect(h.state.dirty).toBe(false);
  });

  it("a copy made for one note says so even while another note's edits still wait", async () => {
    const h = harness({ active: "C.md" });
    h.save.opened("c0");
    // Two notes left with edits that did not land.
    for (const [path, text] of [["A.md", "a1"], ["B.md", "b1"]] as const) {
      h.save.schedule(path, text);
    }
    h.save.schedule("C.md", "c1");
    await settle();
    h.calls[0].settle(new Error("disk full"));
    await settle();
    h.calls[1].settle(new Error("disk full"));
    await settle();
    expect(h.save.stranded).toBe(2);
    void h.save.flush();
    await settle();
    h.calls[2].settle(written); // C
    await settle();
    h.calls[3].settle({ kind: "conflict", disk: "elsewhere" }); // A: kept as a copy
    await settle();
    expect(h.state.saveError?.summary).toContain('"A (2)"');
    expect(h.state.saveError?.summary).toContain("Other edits are still waiting");
  });

  it("closing with kept edits still unwritten stays open once and warns, then lets the app close", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    h.open("B.md", "b0");
    h.save.schedule("B.md", "b1");
    await settle();
    h.calls[0].settle(new Error("disk full"));
    await settle();
    expect(h.save.stranded).toBe(1);

    const first = h.save.beforeClosing();
    await settle();
    h.calls[1].settle(written); // the open note's own edits
    await settle();
    h.calls[2].settle(new Error("disk full")); // the kept edits, tried again
    expect(await first).toBe("stay");
    expect(h.state.saveError?.summary).toContain("Close again");

    const second = h.save.beforeClosing();
    await settle();
    h.calls[3].settle(new Error("disk full"));
    expect(await second).toBe("close");
  });

  it("a write that never returns does not keep the app from closing: it warns once, then closes", async () => {
    const hung: string[] = [];
    const save = new NoteSave(initialNoteSaveState(), {
      root: () => "/v",
      activePath: () => "A.md",
      // The folder stopped answering: the write neither lands nor fails.
      write: (_r, path) => {
        hung.push(path);
        return new Promise<WriteOutcome>(() => {});
      },
      closeWaitMs: 10,
    });
    save.opened("a0");
    save.schedule("A.md", "a1");

    const first = save.beforeClosing();
    await vi.advanceTimersByTimeAsync(10);
    expect(await first).toBe("stay");
    expect(save.state.saveError?.summary).toContain("still being written");
    expect(save.state.saveError?.summary).toContain("Close again");
    expect(hung).toEqual(["A.md"]);

    const second = save.beforeClosing();
    await vi.advanceTimersByTimeAsync(10);
    expect(await second).toBe("close");
  });

  it("closing keeps a stuck edit with the vault it was typed in, after another vault opened", async () => {
    let root = "/one";
    let active: string | null = "A.md";
    const kept: [string, string][] = [];
    const save = new NoteSave(initialNoteSaveState(), {
      root: () => root,
      activePath: () => active,
      write: () => new Promise<WriteOutcome>(() => {}), // the first folder stopped answering
      keepStranded: async (vault, e) => {
        kept.push([vault, e.path]);
        return `id-${kept.length}`;
      },
      closeWaitMs: 10,
    });
    save.opened("a0");
    save.schedule("A.md", "a1");
    active = "B.md";
    save.opened("b0"); // A left, its write stuck
    await settle();
    root = "/two";
    active = null;
    save.closed();

    const closing = save.beforeClosing();
    await vi.advanceTimersByTimeAsync(10);
    expect(await closing).toBe("close");
    expect(kept).toEqual([["/one", "A.md"]]);
  });

  it("a write that comes back while the close waits is not mistaken for a stuck one", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    const closing = h.save.beforeClosing();
    await settle();
    h.calls[0].settle(written);
    expect(await closing).toBe("close");
    expect(h.state.saveError).toBeNull();
  });

  it("a later close warns again about edits kept after an earlier warning was answered", async () => {
    let active: string | null = "A.md";
    // Only the notes being left refuse the write; the open one saves fine, so a "stay" can only come
    // from the kept edits.
    const refusing = new Set<string>(["A.md"]);
    const save = new NoteSave(initialNoteSaveState(), {
      root: () => "/v",
      activePath: () => active,
      write: async (_r, path) => {
        if (refusing.has(path)) throw new Error("disk full");
        return written;
      },
    });
    const strand = async (path: string, next: string) => {
      save.opened("x");
      save.schedule(path, `${path} edit`);
      active = next;
      save.opened("y");
      save.schedule(next, "typing");
      await settle();
    };
    await strand("A.md", "B.md");
    expect(await save.beforeClosing()).toBe("stay");
    refusing.clear();
    await save.flush();
    expect(save.stranded).toBe(0);

    refusing.add("B.md");
    await strand("B.md", "C.md");
    expect(save.stranded).toBe(1);
    expect(await save.beforeClosing()).toBe("stay");
  });

  it("two kept edits to one note are both kept, not the later over the earlier", async () => {
    let active: string | null = "A.md";
    const disk = new Map<string, string>([["A.md", "a0"]]);
    let fail = true;
    const copies: string[] = [];
    const save = new NoteSave(initialNoteSaveState(), {
      root: () => "/v",
      activePath: () => active,
      write: async (_r, path, text, expected) => {
        if (fail) throw new Error("disk full");
        if ((disk.get(path) ?? "") !== expected) return { kind: "conflict", disk: disk.get(path) ?? null };
        disk.set(path, text);
        return written;
      },
      keepCopy: async (_r, _path, text) => {
        copies.push(text);
        return "A (2).md";
      },
    });
    for (const text of ["a2", "a3"]) {
      active = "A.md";
      save.opened("a0");
      save.schedule("A.md", text);
      active = "B.md";
      save.opened("b0");
      save.schedule("B.md", "b");
      await settle();
    }
    expect(save.stranded).toBe(2);
    fail = false;
    await save.flush();
    // The earlier lands; the later, typed over the same starting text, meets it as a change on disk.
    expect(disk.get("A.md")).toBe("a2");
    expect(copies).toEqual(["a3"]);
    expect(save.stranded).toBe(0);
  });

  it("kept edits landing on the note opened again bring the screen and the save baseline along", async () => {
    let active: string | null = "A.md";
    let fail = true;
    const shown: string[] = [];
    const save = new NoteSave(initialNoteSaveState(), {
      root: () => "/v",
      activePath: () => active,
      write: async () => {
        if (fail) throw new Error("disk full");
        return written;
      },
      landedOnOpen: (text) => shown.push(text),
    });
    save.opened("a0");
    save.schedule("A.md", "a2");
    active = "B.md";
    save.opened("b0");
    save.schedule("B.md", "b");
    await settle();
    // Back on A, which still shows what disk held.
    active = "A.md";
    save.opened("a0");
    fail = false;
    await save.flush();
    expect(shown).toEqual(["a2"]);
    expect(save.synced).toBe("a2");
  });

  it("kept edits are written into the vault they were typed in, even after another vault opens", async () => {
    let root = "/one";
    let active: string | null = "A.md";
    const roots: string[] = [];
    let fail = true;
    const save = new NoteSave(initialNoteSaveState(), {
      root: () => root,
      activePath: () => active,
      write: async (r) => {
        roots.push(r);
        if (fail) throw new Error("disk full");
        return written;
      },
    });
    save.opened("a0");
    save.schedule("A.md", "a1");
    active = "B.md";
    save.opened("b0");
    save.schedule("B.md", "b1");
    await settle();
    expect(save.stranded).toBe(1);

    root = "/two";
    active = null;
    save.closed();
    fail = false;
    await save.flush();
    expect(roots.at(-1)).toBe("/one");
    expect(save.stranded).toBe(0);
  });

  it("a note opened again before its left edits land shows them once they do", async () => {
    const shown: string[] = [];
    const h = harness({ deps: { landedOnOpen: (t) => shown.push(t) } });
    h.open("A.md", "a0");
    h.save.schedule("A.md", "a1");
    void h.save.flush();
    await settle();
    h.open("B.md", "b0"); // a1 still on its way
    h.open("A.md", "a0"); // read before a1 landed
    h.calls[0].settle(written);
    await settle();
    expect(shown).toEqual(["a1"]);

    h.save.schedule("A.md", "a1 more");
    void h.save.flush();
    await settle();
    expect(h.calls[1].expected).toBe("a1");
  });

  it("edits typed on a note opened again before its left edits landed do not write over them", async () => {
    const shown: string[] = [];
    const h = harness({ deps: { landedOnOpen: (t) => shown.push(t) } });
    h.open("A.md", "a0");
    h.save.schedule("A.md", "a1");
    void h.save.flush();
    await settle();
    h.open("B.md", "b0");
    h.open("A.md", "a0"); // read before a1 landed
    h.save.schedule("A.md", "a0 typed"); // typed on what the read showed
    h.calls[0].settle(written);
    await settle();
    expect(shown).toEqual([]); // not pulled out from under the typing

    void h.save.flush();
    await settle();
    // Based on what the edit was typed on: disk holds a1, so this meets it as a conflict.
    expect(h.calls[1].expected).toBe("a0");
  });

  it("a note opened again after its left edits landed is not reloaded", async () => {
    const shown: string[] = [];
    const h = harness({ deps: { landedOnOpen: (t) => shown.push(t) } });
    h.open("A.md", "a0");
    h.save.schedule("A.md", "a1");
    void h.save.flush();
    await settle();
    h.open("B.md", "b0");
    h.calls[0].settle(written);
    await settle();
    h.open("A.md", "a1");
    await settle();
    expect(shown).toEqual([]);
  });

  it("edits to a note left behind are written into its vault when another vault opens before their turn", async () => {
    let root = "/one";
    let active: string | null = "A.md";
    const calls: { root: string; text: string; settle: (o: WriteOutcome) => void }[] = [];
    const save = new NoteSave(initialNoteSaveState(), {
      root: () => root,
      activePath: () => active,
      write: (r, _path, text) => new Promise<WriteOutcome>((resolve) => calls.push({ root: r, text, settle: resolve })),
    });
    save.opened("a0");
    save.schedule("A.md", "a1");
    void save.flush();
    await settle();
    expect(calls).toHaveLength(1); // a1 in flight
    save.schedule("A.md", "a2");
    active = "B.md";
    save.opened("b0"); // A left with a2 queued behind a1
    expect(await save.beforeLeaving()).toBe("saved"); // nothing open is unsaved: the switch goes ahead
    root = "/two";
    active = null;
    save.closed();

    calls[0].settle(written);
    await settle();
    expect(calls.map((c) => [c.root, c.text])).toEqual([
      ["/one", "a1"],
      ["/one", "a2"],
    ]);
  });

  it("a save that lands after another vault opened is reported with the vault it was typed in", async () => {
    let root = "/one";
    const landed: [string, string, string][] = [];
    const calls: { settle: (o: WriteOutcome) => void }[] = [];
    const save = new NoteSave(initialNoteSaveState(), {
      root: () => root,
      activePath: () => null,
      write: () => new Promise<WriteOutcome>((resolve) => calls.push({ settle: resolve })),
      saved: (vault, path, text) => landed.push([vault, path, text]),
    });
    save.opened("a0");
    save.schedule("A.md", "a1");
    void save.flush();
    await settle();
    root = "/two"; // another vault opened while the write was out
    calls[0].settle(written);
    await settle();
    expect(landed).toEqual([["/one", "A.md", "a1"]]);
  });

  it("a note of the same name open in another vault is not the one a late answer is about", async () => {
    let root = "/one";
    let active: string | null = "A.md";
    const calls: { root: string; text: string; settle: (o: WriteOutcome) => void }[] = [];
    const copies: [string, string][] = [];
    const state = initialNoteSaveState();
    const save = new NoteSave(state, {
      root: () => root,
      activePath: () => active,
      write: (r, _path, text) => new Promise<WriteOutcome>((resolve) => calls.push({ root: r, text, settle: resolve })),
      keepCopy: async (r, path) => {
        copies.push([r, path]);
        return "A (2).md";
      },
    });
    save.opened("a0");
    save.schedule("A.md", "a1");
    active = "B.md";
    save.opened("b0"); // A left with a1 on its way
    expect(await save.beforeLeaving()).toBe("saved");
    root = "/two";
    active = "A.md"; // another vault's note, named the same
    save.opened("other");
    await settle();

    calls[0].settle({ kind: "conflict", disk: "changed elsewhere" });
    await settle();
    expect(copies).toEqual([["/one", "A.md"]]);
    expect(state.conflictDisk).toBeNull();
  });

  it("moving a note in one vault leaves a waiting edit to the same name in another vault where it is", async () => {
    let root = "/one";
    let active: string | null = "A.md";
    const calls: { root: string; path: string; settle: (o: WriteOutcome) => void }[] = [];
    const save = new NoteSave(initialNoteSaveState(), {
      root: () => root,
      activePath: () => active,
      write: (r, path) => new Promise<WriteOutcome>((resolve) => calls.push({ root: r, path, settle: resolve })),
      moved: (remap) => {
        if (active) active = remap(active);
      },
    });
    save.opened("a0");
    save.schedule("A.md", "a1");
    void save.flush();
    await settle();
    save.schedule("A.md", "a2"); // waits behind a1
    active = "B.md";
    save.opened("b0");
    root = "/two";
    active = "A.md";
    save.opened("other");
    await save.move(async () => (p) => (p === "A.md" ? "dest/A.md" : p), "A.md");

    calls[0].settle(written);
    await settle();
    expect(calls.map((c) => [c.root, c.path])).toEqual([
      ["/one", "A.md"],
      ["/one", "A.md"],
    ]);
  });

});

describe("NoteSave — answers", () => {
  it("taking the copy on disk drops the edits and cancels the pending save", async () => {
    const h = harness();
    h.save.schedule("A.md", "mine");
    h.save.markConflict("theirs");
    expect(h.save.takeDisk()).toBe("theirs");
    h.save.reloaded("theirs");
    vi.advanceTimersByTime(1000);
    await settle();
    expect(h.calls).toHaveLength(0);
    expect(h.state.dirty).toBe(false);
  });

  it("putting a removed note back somewhere else carries the edits typed meanwhile to the new path", async () => {
    const h = harness();
    h.save.schedule("A.md", "kept");
    h.save.markRemoved();
    h.save.schedule("A.md", "kept+more");
    h.save.putBack("kept", "A (2).md");
    expect(h.state.removed).toBe(false);
    expect(h.save.pending).toMatchObject({ path: "A (2).md", text: "kept+more", base: "kept" });
  });

  it("the note reappearing resumes saving", async () => {
    const h = harness();
    h.save.schedule("A.md", "mine");
    h.save.markRemoved();
    vi.advanceTimersByTime(1000);
    await settle();
    expect(h.calls).toHaveLength(0);
    expect(h.save.markPresent()).toBe(true);
    await settle();
    expect(h.calls[0]).toMatchObject({ text: "mine" });
  });
});

describe("NoteSave — moving the open note", () => {
  const into = (dir: string) => (p: string) => (p.startsWith("A.md") ? `${dir}/A.md` : p);

  it("a save scheduled during a move waits for it and lands at the new path", async () => {
    const h = harness();
    h.save.opened("a0");
    let finishMove!: () => void;
    const moving = h.save.move(
      () => new Promise((resolve) => (finishMove = () => resolve(into("dest")))),
    );
    h.save.schedule("A.md", "typed while moving");
    vi.advanceTimersByTime(500);
    await settle();
    expect(h.calls).toHaveLength(0); // nothing written to the old path mid-move

    finishMove();
    await moving;
    await settle();
    expect(h.calls.map((c) => [c.path, c.text, c.expected])).toEqual([
      ["dest/A.md", "typed while moving", "a0"],
    ]);
  });

  it("a save already running finishes before the move starts", async () => {
    const h = harness();
    h.save.schedule("A.md", "1");
    void h.save.flush();
    await settle();
    let relocated = false;
    const moving = h.save.move(async () => {
      relocated = true;
      return into("dest");
    });
    await settle();
    expect(relocated).toBe(false);
    h.calls[0].settle(written);
    await moving;
    expect(relocated).toBe(true);
  });

  it("the disk reference point is not moved by what the editor shows, only by loads and saves", async () => {
    const h = harness();
    h.save.opened("on disk");
    h.save.schedule("A.md", "unsaved");
    await h.save.move(async () => into("dest"));
    expect(h.save.synced).toBe("on disk");
    expect(h.save.pending).toMatchObject({ path: "dest/A.md", text: "unsaved", base: "on disk" });
  });

  it("a failed move leaves the edit where it was", async () => {
    const h = harness();
    h.save.schedule("A.md", "kept");
    await expect(h.save.move(() => Promise.reject(new Error("in use")))).rejects.toThrow("in use");
    expect(h.save.pending?.path).toBe("A.md");
  });
});

describe("NoteSave — unsaved marker", () => {
  it("reopening the open note while its edits are still saving keeps it marked unsaved", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    h.open("A.md", "a0"); // opened again before the save ran
    expect(h.state.dirty).toBe(true);
    void h.save.flush();
    await settle();
    h.calls[0].settle(written);
    await settle();
    expect(h.state.dirty).toBe(false);
  });

  it("an edit to the note being left does not mark the next one unsaved, and goes on its way", async () => {
    const h = harness();
    h.save.schedule("A.md", "a1");
    h.open("B.md", "b0");
    expect(h.state.dirty).toBe(false);
    expect(h.save.pending).toBeNull(); // no longer the open note's: an edit to a note left
    expect(h.save.writing).toBe(1);
    await settle();
    expect(h.calls.map((c) => [c.path, c.text])).toEqual([["A.md", "a1"]]);
  });

  it("follows the open note through a move", async () => {
    const h = harness();
    h.save.schedule("A.md", "a1");
    await h.save.move(async () => (p) => (p === "A.md" ? "dest/A.md" : p));
    expect(h.state.dirty).toBe(true);
  });
});

describe("NoteSave — saves to different notes", () => {
  it("edits typed while a save runs, then left, are written on top of that save — not kept as a copy", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    void h.save.flush();
    await settle();
    h.save.schedule("A.md", "a2"); // typed while "a1" is being written
    h.open("B.md", "b0");
    h.save.schedule("B.md", "b1"); // the edits to A are sent on their way
    await settle();
    h.calls[0].settle(written);
    await settle();
    const second = h.calls.find((c) => c.path === "A.md" && c.text === "a2");
    // It started from what the note held before "a1" — but "a1" is its own earlier edit, not a
    // change made elsewhere, so it replaces "a1".
    expect(second).toMatchObject({ expected: "a1" });
    expect(h.copies).toEqual([]);
  });

  it("a save to one note that never comes back does not hold up another note's", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    void h.save.flush();
    await settle();
    h.open("B.md", "b0");
    h.save.schedule("B.md", "b1");
    vi.advanceTimersByTime(500);
    await settle();
    expect(h.calls.map((c) => [c.path, c.text])).toEqual([
      ["A.md", "a1"],
      ["B.md", "b1"],
    ]);
  });

  it("an edit already being written is not written a second time when its note is left", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    void h.save.flush();
    await settle();
    h.open("B.md", "b0");
    h.save.schedule("B.md", "b1"); // would send A's edit again
    await settle();
    h.calls[0].settle(written);
    await settle();
    expect(h.calls.filter((c) => c.path === "A.md")).toHaveLength(1);
  });

  it("a move waits only for saves to what it moves", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    void h.save.flush();
    await settle(); // A's save hangs
    h.open("B.md", "b0");
    let moved = false;
    await h.save.move(async () => {
      moved = true;
      return (p) => p;
    }, "B.md");
    expect(moved).toBe(true);

    let movedA = false;
    const movingA = h.save.move(async () => {
      movedA = true;
      return (p) => p;
    }, "A.md");
    await settle();
    expect(movedA).toBe(false); // not while a save to it is still running
    h.calls[0].settle(written);
    await movingA;
    expect(movedA).toBe(true);
  });

  it("edits to a note left while it is being moved go where it went", async () => {
    const h = harness();
    h.save.opened("a0");
    let finishMove!: () => void;
    const moving = h.save.move(
      () => new Promise((resolve) => (finishMove = () => resolve((p) => p.replace("A.md", "dest/A.md")))),
      "A.md",
    );
    h.save.schedule("A.md", "typed");
    h.open("B.md", "b0");
    h.save.schedule("B.md", "b1"); // A's edits are queued while the move runs
    await settle();
    expect(h.calls.some((c) => c.path === "A.md")).toBe(false);
    finishMove();
    await moving;
    await settle();
    expect(h.calls.find((c) => c.text === "typed")).toMatchObject({ path: "dest/A.md" });
  });

  it("kept edits whose note was moved are tried at its new place", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    h.open("B.md", "b0");
    h.save.schedule("B.md", "b1");
    await settle();
    h.calls[0].settle(new Error("disk full")); // A's edits are kept
    await settle();
    expect(h.save.stranded).toBe(1);
    await h.save.move(async () => (p) => p.replace("A.md", "dest/A.md"), "A.md");
    void h.save.flush();
    await settle();
    expect(h.calls.at(-1)).toMatchObject({ path: "dest/A.md", text: "a1" });
  });
});

describe("NoteSave — a folder that stops answering", () => {
  it("a save running past the threshold says so, and stops saying so when it lands", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    void h.save.flush();
    await settle();
    vi.advanceTimersByTime(9_999);
    expect(h.state.stalled).toBe(false);
    vi.advanceTimersByTime(1);
    expect(h.state.stalled).toBe(true);
    h.calls[0].settle(written);
    await settle();
    expect(h.state.stalled).toBe(false);
  });

  it("leaving a note whose save has stalled goes ahead, and the edits land once, when the folder answers", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    void h.save.flush();
    await settle();
    vi.advanceTimersByTime(10_000);
    // Leaving waits for nothing more: the save is on its way, and the note cannot take it any sooner.
    expect(await h.save.beforeLeaving()).toBe("busy");

    h.open("B.md", "b0");
    expect(h.state.stalled).toBe(false); // what is on screen now is saving fine
    h.save.schedule("B.md", "b1");
    await settle();
    h.calls[0].settle(written);
    await settle();
    expect(h.calls.filter((c) => c.path === "A.md")).toHaveLength(1);
    expect(h.copies).toEqual([]);
    expect(h.save.stranded).toBe(0);
  });

  it("leaving the next note is not held by the stalled one left before it", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    void h.save.flush();
    await settle();
    vi.advanceTimersByTime(10_000);
    expect(await h.save.beforeLeaving()).toBe("busy");
    h.open("B.md", "b0"); // nothing typed here

    // Opening a third note, renaming this one: B has nothing unsaved, so nothing to wait for.
    expect(await h.save.beforeLeaving()).toBe("saved");
    expect(h.save.writing).toBe(1); // A's save, still on its way

    h.calls[0].settle(written);
    await settle();
    expect(h.calls.filter((c) => c.path === "A.md")).toHaveLength(1);
    expect(h.copies).toEqual([]);
    expect(h.save.writing).toBe(0);
  });

  it("the edits of a stalled note left behind are kept if the folder then refuses them", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    void h.save.flush();
    await settle();
    vi.advanceTimersByTime(10_000);
    expect(await h.save.beforeLeaving()).toBe("busy");
    h.open("B.md", "b0");
    h.save.schedule("B.md", "b1");
    await settle();
    h.calls[0].settle(new Error("network path not found"));
    await settle();
    expect(h.save.stranded).toBe(1);
    expect(h.state.saveError?.summary).toMatch(/not saved yet/);
  });

  it("a save to a note already left that hangs is not shown on the note that is open", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    h.open("B.md", "b0");
    h.save.schedule("B.md", "b1"); // A's edits go on their way and hang
    await settle();
    vi.advanceTimersByTime(10_000);
    expect(h.state.stalled).toBe(false);
  });

  it("closing waits for a save still on its way to a note already left", async () => {
    const h = harness();
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    h.open("B.md", "b0");
    h.save.schedule("B.md", "b1");
    vi.advanceTimersByTime(500);
    await settle();
    h.calls.find((c) => c.path === "B.md")!.settle(written);
    await settle();
    expect(h.save.pending).toBeNull();
    expect(h.save.writing).toBe(1); // A's save, hanging

    const closing = h.save.beforeClosing();
    await vi.advanceTimersByTimeAsync(5_000);
    expect(await closing).toBe("stay");
    expect(h.state.saveError?.summary).toMatch(/still being written/);
    const again = h.save.beforeClosing(); // the second close is not held past the wait
    await vi.advanceTimersByTimeAsync(5_000);
    expect(await again).toBe("close");
  });
});

/** Where kept edits outlast the app, in memory: id → what was kept. */
function outlasting() {
  const kept = new Map<string, { root: string; path: string; text: string; base: string }>();
  const forgotten: string[] = [];
  let next = 0;
  return {
    kept,
    forgotten,
    deps: {
      keepStranded: async (root: string, e: { path: string; text: string; base: string }) => {
        const id = `k${++next}`;
        kept.set(id, { root, ...e });
        return id;
      },
      forgetStranded: async (_root: string, id: string) => {
        kept.delete(id);
        forgotten.push(id);
      },
    } satisfies Partial<NoteSaveDeps>,
  };
}

describe("NoteSave — edits kept past closing the app", () => {
  it("edits to a note left that cannot be written are kept outside the app, and let go of once they land", async () => {
    const o = outlasting();
    const h = harness({ deps: o.deps });
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    h.open("B.md", "b0");
    await settle();
    h.calls[0].settle(new Error("network path not found"));
    await settle();
    expect([...o.kept.values()]).toEqual([{ root: "/vault", path: "A.md", text: "a1", base: "a0" }]);

    void h.save.flush();
    await settle();
    h.calls.at(-1)!.settle(written);
    await settle();
    expect(o.kept.size).toBe(0);
    expect(h.save.stranded).toBe(0);
  });

  it("closing with such edits kept closes at once: nothing is lost by it", async () => {
    const o = outlasting();
    const h = harness({ deps: o.deps });
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    h.open("B.md", "b0");
    await settle();
    h.calls[0].settle(new Error("network path not found"));
    await settle();

    const closing = h.save.beforeClosing();
    await settle();
    h.calls.at(-1)?.settle(new Error("still not found")); // the retry on the way out
    expect(await closing).toBe("close");
    expect(o.kept.size).toBe(1);
  });

  it("closing while a save hangs keeps it outside the app and closes the first time", async () => {
    const o = outlasting();
    const h = harness({ deps: { ...o.deps, closeWaitMs: 10 } });
    h.save.opened("a0");
    h.save.schedule("A.md", "a1");
    void h.save.flush();
    await settle(); // the folder never answers

    const closing = h.save.beforeClosing();
    await vi.advanceTimersByTimeAsync(10);
    expect(await closing).toBe("close");
    expect([...o.kept.values()].map((k) => k.text)).toEqual(["a1"]);
  });

  it("edits kept from an earlier run are tried when the folder opens, and let go of when they land", async () => {
    const o = outlasting();
    const h = harness({ deps: o.deps });
    h.open("B.md", "b0");
    h.save.adopt("/vault", [{ id: "old1", path: "A.md", text: "a1", base: "a0" }]);
    expect(h.save.stranded).toBe(1);

    void h.save.flush();
    await settle();
    expect(h.calls.at(-1)).toMatchObject({ path: "A.md", text: "a1", expected: "a0" });
    h.calls.at(-1)!.settle(written);
    await settle();
    expect(o.forgotten).toEqual(["old1"]);
    expect(h.save.stranded).toBe(0);
  });

  it("a kept edit the note already holds is taken as landed, not kept as a copy", async () => {
    // The save the app closed on landed after all, before it exited.
    const o = outlasting();
    const h = harness({ deps: o.deps });
    h.open("B.md", "b0");
    h.save.adopt("/vault", [{ id: "old1", path: "A.md", text: "a1", base: "a0" }]);
    void h.save.flush();
    await settle();
    h.calls.at(-1)!.settle({ kind: "conflict", disk: "a1" });
    await settle();
    expect(h.copies).toEqual([]);
    expect(o.forgotten).toEqual(["old1"]);
    expect(h.state.saveError).toBeNull();
  });

  it("a kept edit whose note is moved is kept again under its new place", async () => {
    const o = outlasting();
    const h = harness({ deps: o.deps });
    h.open("B.md", "b0");
    h.save.adopt("/vault", [{ id: "old1", path: "A.md", text: "a1", base: "a0" }]);
    await h.save.move(async () => (p) => p.replace("A.md", "dest/A.md"), "A.md");
    await settle();
    expect(o.forgotten).toEqual(["old1"]);
    expect([...o.kept.values()]).toEqual([{ root: "/vault", path: "dest/A.md", text: "a1", base: "a0" }]);
  });
});
