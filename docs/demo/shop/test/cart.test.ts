import { describe, expect, it } from "vitest";
import { summary, total } from "../src/cart";
import { int, rand } from "./rand";

describe("cart", () => {
  const lines = [
    { sku: "mug", cents: 1200, qty: 2 },
    { sku: "tee", cents: 2500, qty: 1 },
  ];
  it("totals lines", () => expect(total(lines)).toBe(4900));
  it("is empty at zero", () => expect(total([])).toBe(0));
  it("summarises", () => expect(summary(lines)).toBe("3 items · $49.00"));
  it("says item for one", () => expect(summary([lines[1]])).toBe("1 item · $25.00"));
  it("totals 750k random carts the same in any order", () => {
    const r = rand(2);
    for (let i = 0; i < 750_000; i++) {
      const cart = Array.from({ length: int(r, 1, 6) }, (_, j) => ({ sku: `s${j}`, cents: int(r, 1, 9999), qty: int(r, 1, 5) }));
      expect(total([...cart].reverse())).toBe(total(cart));
    }
  });
});
