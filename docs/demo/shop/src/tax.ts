const RATES: Record<string, number> = { US: 0, DE: 19, FR: 20, GB: 20, ES: 21 };

export const taxCents = (cents: number, country: string) => Math.round((cents * (RATES[country] ?? 0)) / 100);
