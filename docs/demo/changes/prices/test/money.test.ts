import { describe, expect, it } from "vitest";
import { formatPrice, percentOf } from "../src/money";
import { int, rand } from "./rand";

// Intl puts a no-break space between number and symbol in some locales.
const plain = (s: string) => s.replace(/\s/g, " ");

describe("formatPrice", () => {
  it("shows dollars and cents", () => expect(formatPrice(1999)).toBe("$19.99"));
  it("pads the cents", () => expect(formatPrice(500)).toBe("$5.00"));
  it("shows zero", () => expect(formatPrice(0)).toBe("$0.00"));
  it("groups thousands", () => expect(formatPrice(12_345_678)).toBe("$123,456.78"));
  it("shows euros the German way", () => expect(plain(formatPrice(1999, { code: "EUR", locale: "de-DE" }))).toBe("19,99 €"));
  it("shows pounds", () => expect(formatPrice(1999, { code: "GBP", locale: "en-GB" })).toBe("£19.99"));
});

describe("percentOf", () => {
  it("rounds to the cent", () => expect(percentOf(999, 15)).toBe(150));
  it("agrees with exact arithmetic on 1M prices", () => {
    const r = rand(1);
    for (let i = 0; i < 1_000_000; i++) {
      const cents = int(r, 0, 1_000_000);
      const p = int(r, 0, 100);
      expect(percentOf(cents, p)).toBe(Math.round((cents * p) / 100));
    }
  });
});
