/** What is left of each SKU; a reservation either takes all it asks for or nothing. */
export class Stock {
  private left = new Map<string, number>();

  add(sku: string, n: number) {
    this.left.set(sku, this.count(sku) + n);
  }

  reserve(sku: string, n: number): boolean {
    const have = this.count(sku);
    if (n <= 0 || have < n) return false;
    this.left.set(sku, have - n);
    return true;
  }

  count(sku: string) {
    return this.left.get(sku) ?? 0;
  }
}
