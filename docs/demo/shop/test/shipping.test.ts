import { describe, expect, it } from "vitest";
import { shippingCents, type Zone } from "../src/shipping";
import { int, rand } from "./rand";

describe("shipping", () => {
  it("is free at home over $50", () => expect(shippingCents("domestic", 2_000, 5_000)).toBe(0));
  it("charges per started kilo", () => expect(shippingCents("eu", 1_200, 0)).toBe(999 + 2 * 250));
  it("charges the world rate", () => expect(shippingCents("world", 500, 9_999)).toBe(1999 + 400));
  it("never gets cheaper with weight, over 1M parcels", () => {
    const r = rand(4);
    const zones: Zone[] = ["domestic", "eu", "world"];
    for (let i = 0; i < 1_000_000; i++) {
      const z = zones[int(r, 0, 2)];
      const g = int(r, 0, 30_000);
      const sub = int(r, 0, 4_999);
      expect(shippingCents(z, g + 1_000, sub)).toBeGreaterThanOrEqual(shippingCents(z, g, sub));
    }
  });
});
