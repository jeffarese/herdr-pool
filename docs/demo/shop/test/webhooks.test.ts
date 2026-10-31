import { describe, expect, it } from "vitest";
import { deliver, type Delivery } from "../src/webhooks";

const d: Delivery = { url: "https://hooks.example.com/orders", event: "order.paid", body: { id: 7 } };

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
