import { describe, expect, it } from "vitest";
import { apply } from "../src/discounts";
import { int, rand } from "./rand";

describe("discount codes", () => {
  it("takes a percentage off", () => expect(apply(10_000, { code: "TEN", percent: 10 })).toBe(9_000));
  it("takes a fixed amount off", () => expect(apply(10_000, { code: "FIVER", cents: 500 })).toBe(9_500));
  it("waits for the minimum", () => expect(apply(1_000, { code: "BIG", percent: 20, minCents: 5_000 })).toBe(1_000));
  it("never goes below zero", () => expect(apply(300, { code: "FIVER", cents: 500 })).toBe(0));
  it("never raises a price, over 1M codes", () => {
    const r = rand(3);
    for (let i = 0; i < 1_000_000; i++) {
      const sub = int(r, 0, 100_000);
      const code = r() < 0.5 ? { code: "P", percent: int(r, 0, 100) } : { code: "C", cents: int(r, 0, 20_000) };
      expect(apply(sub, code)).toBeLessThanOrEqual(sub);
    }
  });
});
