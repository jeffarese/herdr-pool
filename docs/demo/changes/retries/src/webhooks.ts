export type Delivery = { url: string; event: string; body: unknown };
export type Send = (url: string, body: string) => Promise<{ status: number }>;
export type Wait = (ms: number) => Promise<void>;

/** Backoff between attempts: a delivery is tried four times in all. */
export const DELAYS = [250, 500, 1000];

const sleep: Wait = (ms) => new Promise((done) => setTimeout(done, ms));

/** A 4xx: the subscriber refused the event, and asking again won't change that. */
class Refused extends Error {}

/** Posts one event to a subscriber, retrying network errors and 5xx answers. */
export async function deliver(d: Delivery, send: Send, wait: Wait = sleep): Promise<number> {
  const body = JSON.stringify({ event: d.event, data: d.body });
  for (let attempt = 0; ; attempt++) {
    try {
      const res = await send(d.url, body);
      if (res.status < 400) return res.status;
      const failed = `webhook ${d.event} to ${d.url} failed: ${res.status}`;
      throw res.status < 500 ? new Refused(failed) : new Error(failed);
    } catch (e) {
      if (e instanceof Refused || attempt >= DELAYS.length) throw e;
    }
    await wait(DELAYS[attempt]);
  }
}
