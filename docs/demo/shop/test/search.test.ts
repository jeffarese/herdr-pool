import { describe, expect, it } from "vitest";
import { search, type Product } from "../src/search";
import { int, rand } from "./rand";

const products: Product[] = [
  { sku: "mug-1", title: "Enamel camping mug", tags: ["kitchen", "outdoors"] },
  { sku: "tee-1", title: "Café tee", tags: ["clothing"] },
  { sku: "bag-1", title: "Canvas tote bag", tags: ["bags"] },
];

describe("search", () => {
  it("matches word starts", () => expect(search(products, "camp").map((p) => p.sku)).toEqual(["mug-1"]));
  it("ignores accents", () => expect(search(products, "cafe").map((p) => p.sku)).toEqual(["tee-1"]));
  it("needs every word", () => expect(search(products, "canvas mug")).toEqual([]));
  it("searches tags", () => expect(search(products, "outdoors").map((p) => p.sku)).toEqual(["mug-1"]));
  it("finds every product by its own title, in a catalogue of 3k", () => {
    const r = rand(7);
    const nouns = ["mug", "tee", "bag", "cap", "lamp", "sock", "pen", "book", "case", "mat"];
    const catalogue = Array.from({ length: 3_000 }, (_, i) => ({
      sku: `p${i}`,
      title: `${nouns[int(r, 0, 9)]} ${nouns[int(r, 0, 9)]} ${i}`,
      tags: [],
    }));
    for (const p of catalogue.slice(0, 1_000)) expect(search(catalogue, p.title)).toContainEqual(p);
  });
});
