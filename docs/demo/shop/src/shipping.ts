export type Zone = "domestic" | "eu" | "world";

const RATES: Record<Zone, { base: number; perKg: number }> = {
  domestic: { base: 499, perKg: 120 },
  eu: { base: 999, perKg: 250 },
  world: { base: 1999, perKg: 400 },
};

/** Free at home over $50; otherwise a base rate and a rate per started kilo. */
export function shippingCents(zone: Zone, grams: number, subtotal: number): number {
  if (zone === "domestic" && subtotal >= 5000) return 0;
  const r = RATES[zone];
  return r.base + Math.ceil(grams / 1000) * r.perKg;
}
