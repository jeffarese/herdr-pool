import { total, type Line } from "./cart";
import { shippingCents, type Zone } from "./shipping";
import { taxCents } from "./tax";

export function orderTotal(lines: Line[], country: string, zone: Zone, grams: number): number {
  const subtotal = total(lines);
  return subtotal + taxCents(subtotal, country) + shippingCents(zone, grams, subtotal);
}
