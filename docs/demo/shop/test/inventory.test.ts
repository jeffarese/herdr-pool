import { describe, expect, it } from "vitest";
import { Stock } from "../src/inventory";
import { int, rand } from "./rand";

describe("stock", () => {
  it("reserves what is there", () => {
    const s = new Stock();
    s.add("mug", 3);
    expect(s.reserve("mug", 2)).toBe(true);
    expect(s.count("mug")).toBe(1);
  });
  it("takes nothing when there isn't enough", () => {
    const s = new Stock();
    s.add("mug", 1);
    expect(s.reserve("mug", 2)).toBe(false);
    expect(s.count("mug")).toBe(1);
  });
  it("never oversells, over 1M reservations", () => {
    const r = rand(6);
    const s = new Stock();
    let added = 0;
    let taken = 0;
    for (let i = 0; i < 1_000_000; i++) {
      if (r() < 0.3) {
        const n = int(r, 1, 5);
        s.add("tee", n);
        added += n;
      } else {
        const n = int(r, 1, 3);
        if (s.reserve("tee", n)) taken += n;
      }
      expect(s.count("tee")).toBe(added - taken);
    }
  });
});
