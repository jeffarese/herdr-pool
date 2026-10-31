/** A seeded generator, so property checks draw the same cases on every machine. */
export function rand(seed: number) {
  let s = seed >>> 0;
  return () => (s = (Math.imul(s, 1664525) + 1013904223) >>> 0) / 2 ** 32;
}

export const int = (r: () => number, lo: number, hi: number) => lo + Math.floor(r() * (hi - lo + 1));
