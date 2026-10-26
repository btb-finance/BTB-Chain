import { describe, expect, it } from "vitest";
import { BtbClient, BtbError, ContractError, encode, fromHex, mutationRef, queryRef, t, toHex } from "../src/index.js";

const hexBody = (c: Parameters<typeof encode>[0], v: never) => toHex(encode(c, v));

/** A client whose fetch records the request and answers with `reply`. */
function fake(reply: (url: URL, init?: RequestInit) => Response) {
  const calls: { url: URL; method: string }[] = [];
  const client = new BtbClient({
    url: "http://x",
    caller: "alice",
    fetch: (async (u: string, init?: RequestInit) => {
      const url = new URL(u);
      calls.push({ url, method: init?.method ?? "GET" });
      return reply(url, init);
    }) as typeof fetch,
  });
  return { client, calls };
}

const add = mutationRef("add", t.tuple(t.string), t.u64);
const list = queryRef("list_mine", t.tuple(), t.vec(t.tuple(t.u64, t.bool, t.string)));

describe("schema-typed function references", () => {
  it("query(ref) needs no arguments when the function takes none", async () => {
    const { client, calls } = fake(() => new Response(hexBody(list.result, [[1n, true, "a"]] as never)));
    expect(await client.query(list)).toEqual([[1n, true, "a"]]);
    expect(calls[0]!.url.pathname).toBe("/query/list_mine");
    expect(calls[0]!.url.searchParams.get("args")).toBeNull();
    expect(calls[0]!.method).toBe("GET");
  });

  it("mutation(ref, args) encodes the arguments with the reference's codec", async () => {
    const { client, calls } = fake(() => new Response(hexBody(t.u64, 7n as never)));
    expect(await client.mutation(add, ["milk"])).toBe(7n);
    expect(calls[0]!.method).toBe("POST");
    expect(calls[0]!.url.pathname).toBe("/mutation/add");
    expect(fromHex(calls[0]!.url.searchParams.get("args")!)).toEqual(encode(t.tuple(t.string), ["milk"]));
  });

  it("the name-and-codec form still works", async () => {
    const { client } = fake(() => new Response(hexBody(t.u64, 3n as never)));
    expect(await client.mutation("add", t.tuple(t.string), ["x"], t.u64)).toBe(3n);
  });

  it("a contract abort becomes a ContractError carrying the contract's message", async () => {
    const { client } = fake(
      () => new Response(JSON.stringify({ error: "aborted: text must be 1..=280 bytes" }), { status: 400 }),
    );
    const e = await client.mutation(add, [""]).catch((x) => x);
    expect(e).toBeInstanceOf(ContractError);
    expect(e).toBeInstanceOf(BtbError);
    expect(e.reason).toBe("text must be 1..=280 bytes");
    expect(e.status).toBe(400);
    expect(e.message).toBe("aborted: text must be 1..=280 bytes");
  });

  it("other failures stay plain BtbErrors", async () => {
    const { client } = fake(() => new Response(JSON.stringify({ error: "no such function" }), { status: 404 }));
    const e = await client.query(list).catch((x) => x);
    expect(e).toBeInstanceOf(BtbError);
    expect(e).not.toBeInstanceOf(ContractError);
    expect(e.status).toBe(404);
    // A 400 that is not an abort (bad arguments) is not a contract error either.
    const { client: c2 } = fake(() => new Response(JSON.stringify({ error: "bad args" }), { status: 400 }));
    expect(await c2.query(list).catch((x) => x)).not.toBeInstanceOf(ContractError);
  });

  it("subscribe(ref, args, cb) decodes pushed results", async () => {
    const sse = `data: ${hexBody(list.result, [] as never)}\n\ndata: ${hexBody(list.result, [[2n, false, "b"]] as never)}\n\n`;
    const { client } = fake(
      () =>
        new Response(
          new ReadableStream({
            start(c) {
              c.enqueue(new TextEncoder().encode(sse));
              c.close();
            },
          }),
        ),
    );
    const seen: unknown[] = [];
    const stop = client.subscribe(list, [], (r) => seen.push(r));
    await new Promise((r) => setTimeout(r, 50));
    stop();
    expect(seen).toEqual([[], [[2n, false, "b"]]]);
  });
});
