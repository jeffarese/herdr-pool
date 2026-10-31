export type Product = { sku: string; title: string; tags: string[] };

const words = (s: string) =>
  s.toLowerCase().normalize("NFD").replace(/[̀-ͯ]/g, "").split(/[^a-z0-9]+/).filter(Boolean);

/** Products whose title or tags start with every word of the query, by title. */
export function search(products: Product[], query: string): Product[] {
  const q = words(query);
  return products
    .filter((p) => {
      const have = words(`${p.title} ${p.tags.join(" ")}`);
      return q.every((w) => have.some((h) => h.startsWith(w)));
    })
    .sort((a, b) => a.title.localeCompare(b.title));
}
