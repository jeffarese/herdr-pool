import { type Currency, formatPrice, USD } from "./money";

export type Line = { sku: string; cents: number; qty: number };

export const total = (lines: Line[]) => lines.reduce((sum, l) => sum + l.cents * l.qty, 0);

export function summary(lines: Line[], currency: Currency = USD): string {
  const items = lines.reduce((n, l) => n + l.qty, 0);
  return `${items} item${items === 1 ? "" : "s"} · ${formatPrice(total(lines), currency)}`;
}
