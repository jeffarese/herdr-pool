import { describe, expect, it } from "vitest";
import { DELAYS, deliver, type Delivery } from "../src/webhooks";

const d: Delivery = { url: "https://hooks.example.com/orders", event: "order.paid", body: { id: 7 } };
const now = async () => {};

describe("deliver", () => {
  it("posts the event and its data", async () => {
    const sent: string[] = [];
    await deliver(d, async (_url, body) => {
      sent.push(body);
      return { status: 200 };
    });
    expect(JSON.parse(sent[0])).toEqual({ event: "order.paid", data: { id: 7 } });
  });
  it("returns the status", async () => expect(await deliver(d, async () => ({ status: 202 }))).toBe(202));
  it("fails on an error status", async () => {
    await expect(deliver(d, async () => ({ status: 404 }))).rejects.toThrow("failed: 404");
  });
  it("delivers 150k events in order", async () => {
    const seen: number[] = [];
    for (let i = 0; i < 150_000; i++) {
      await deliver({ ...d, body: { id: i } }, async (_url, body) => {
        seen.push(JSON.parse(body).data.id);
        return { status: 200 };
      });
    }
    expect(seen.every((id, i) => id === i)).toBe(true);
  });
});

describe("retries", () => {
  it("tries again after a 5xx, backing off", async () => {
    const statuses = [503, 502, 200];
    const waited: number[] = [];
    const status = await deliver(d, async () => ({ status: statuses.shift()! }), async (ms) => void waited.push(ms));
    expect(status).toBe(200);
    expect(waited).toEqual([250, 500]);
  });
  it("tries again after a network error", async () => {
    let calls = 0;
    const send = async () => {
      if (++calls === 1) throw new Error("ECONNRESET");
      return { status: 200 };
    };
    expect(await deliver(d, send, now)).toBe(200);
    expect(calls).toBe(2);
  });
  it("gives up after four attempts", async () => {
    let calls = 0;
    await expect(deliver(d, async () => (calls++, { status: 500 }), now)).rejects.toThrow("failed: 500");
    expect(calls).toBe(DELAYS.length + 1);
  });
  it("never retries a 4xx", async () => {
    let calls = 0;
    await expect(deliver(d, async () => (calls++, { status: 422 }), now)).rejects.toThrow("failed: 422");
    expect(calls).toBe(1);
  });
});
