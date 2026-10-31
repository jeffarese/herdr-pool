import { percentOf } from "./money";

export type Code = { code: string; percent?: number; cents?: number; minCents?: number };

/** The subtotal after a discount code; never below zero. */
export function apply(subtotal: number, code: Code): number {
  if (code.minCents && subtotal < code.minCents) return subtotal;
  const off = code.percent ? percentOf(subtotal, code.percent) : (code.cents ?? 0);
  return Math.max(0, subtotal - off);
}
