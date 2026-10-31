import { describe, expect, it } from "vitest";
import { taxCents } from "../src/tax";
import { int, rand } from "./rand";

describe("tax", () => {
  it("adds VAT in Germany", () => expect(taxCents(10_000, "DE")).toBe(1_900));
  it("adds none in the US", () => expect(taxCents(10_000, "US")).toBe(0));
  it("adds none where it doesn't know", () => expect(taxCents(10_000, "XX")).toBe(0));
  it("stays within a cent of exact over 1.2M prices", () => {
    const r = rand(5);
    for (let i = 0; i < 1_500_000; i++) {
      const cents = int(r, 0, 1_000_000);
      expect(Math.abs(taxCents(cents, "ES") - (cents * 21) / 100)).toBeLessThanOrEqual(0.5);
    }
  });
});
