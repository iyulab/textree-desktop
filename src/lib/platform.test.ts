import { describe, it, expect } from "vitest";
import { isMacUserAgent } from "./platform";

describe("isMacUserAgent", () => {
  it("knows the macOS web view", () => {
    expect(isMacUserAgent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)")).toBe(true);
  });
  it("is not fooled by the Windows web view", () => {
    expect(isMacUserAgent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36 Edg/130.0")).toBe(false);
  });
});
