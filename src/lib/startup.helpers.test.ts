import { describe, expect, it } from "vitest";
import { decideStartup, LAST_VAULT_KEY } from "./startup.helpers";

describe("decideStartup", () => {
  it("restores a stored vault path", () => {
    // public-text: allow — a stand-in path, not anyone's home
    expect(decideStartup("/home/me/Notes")).toEqual({ action: "restore", path: "/home/me/Notes" });
  });

  it("falls back to the default vault when nothing is stored", () => {
    expect(decideStartup(null)).toEqual({ action: "default" });
  });

  it("treats an empty string as no stored vault", () => {
    expect(decideStartup("")).toEqual({ action: "default" });
  });

  it("opens the first kept notes folder when the device remembers none", () => {
    expect(decideStartup(null, ["/notes/a", "/notes/b"])).toEqual({ action: "restore", path: "/notes/a" });
  });

  it("prefers the folder opened last over the kept list", () => {
    expect(decideStartup("/notes/b", ["/notes/a", "/notes/b"])).toEqual({ action: "restore", path: "/notes/b" });
  });

  it("starts with the default vault when nothing is remembered or kept", () => {
    expect(decideStartup(null, [])).toEqual({ action: "default" });
    expect(decideStartup(null, [""])).toEqual({ action: "default" });
  });

  it("exposes the localStorage key", () => {
    expect(LAST_VAULT_KEY).toBe("textree-last-vault");
  });
});
