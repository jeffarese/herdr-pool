/** How a shop shows its prices: its currency, and the locale that writes them. */
export type Currency = { code: string; locale: string };

export const USD: Currency = { code: "USD", locale: "en-US" };

const formats = new Map<string, Intl.NumberFormat>();

/** Prices are integer cents everywhere; this is the only place they become text. */
export function formatPrice(cents: number, currency: Currency = USD): string {
  const key = `${currency.locale}/${currency.code}`;
  let f = formats.get(key);
  if (!f) {
    f = new Intl.NumberFormat(currency.locale, { style: "currency", currency: currency.code });
    formats.set(key, f);
  }
  return f.format(cents / 100);
}

export function percentOf(cents: number, percent: number): number {
  return Math.round((cents * percent) / 100);
}
