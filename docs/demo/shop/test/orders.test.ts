import { describe, expect, it } from "vitest";
import { orderTotal } from "../src/orders";
import { int, rand } from "./rand";

describe("order totals", () => {
  const lines = [{ sku: "mug", cents: 1_200, qty: 2 }];
  it("adds tax and shipping", () => expect(orderTotal(lines, "DE", "eu", 800)).toBe(2_400 + 456 + 999 + 250));
  it("ships free at home over $50", () => expect(orderTotal([{ sku: "x", cents: 6_000, qty: 1 }], "US", "domestic", 900)).toBe(6_000));
  it("is never less than the goods, over 750k orders", () => {
    const r = rand(8);
    for (let i = 0; i < 750_000; i++) {
      const goods = [{ sku: "x", cents: int(r, 1, 20_000), qty: int(r, 1, 4) }];
      const t = orderTotal(goods, ["US", "DE", "ES"][int(r, 0, 2)], "eu", int(r, 0, 9_000));
      expect(t).toBeGreaterThanOrEqual(goods[0].cents * goods[0].qty);
    }
  });
});
