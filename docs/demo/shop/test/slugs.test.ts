import { describe, expect, it } from "vitest";
import { slug } from "../src/slugs";
import { int, rand } from "./rand";

describe("slug", () => {
  it("lowercases and dashes", () => expect(slug("Canvas Tote Bag")).toBe("canvas-tote-bag"));
  it("drops accents", () => expect(slug("Café Crème")).toBe("cafe-creme"));
  it("trims dashes", () => expect(slug("  -Sale!- ")).toBe("sale"));
  it("is idempotent, over 500k titles", () => {
    const r = rand(9);
    const chars = "abcdeé ABC-_!ñ01";
    for (let i = 0; i < 500_000; i++) {
      const s = Array.from({ length: int(r, 1, 16) }, () => chars[int(r, 0, chars.length - 1)]).join("");
      expect(slug(slug(s))).toBe(slug(s));
    }
  });
});
