export type Delivery = { url: string; event: string; body: unknown };
export type Send = (url: string, body: string) => Promise<{ status: number }>;

/** Posts one event to a subscriber. */
export async function deliver(d: Delivery, send: Send): Promise<number> {
  const res = await send(d.url, JSON.stringify({ event: d.event, data: d.body }));
  if (res.status >= 400) throw new Error(`webhook ${d.event} to ${d.url} failed: ${res.status}`);
  return res.status;
}
