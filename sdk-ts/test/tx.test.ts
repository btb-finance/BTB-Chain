import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  BtbClient,
  BtbError,
  callKind,
  encodeTxBody,
  fromHex,
  publicKeyOf,
  signTransaction,
  toHex,
  transactionId,
  transferKind,
} from "../src/index.js";

const fx = JSON.parse(readFileSync(new URL("./fixtures/tx.json", import.meta.url), "utf8")) as {
  seed: string;
  public_key: string;
  txs: { name: string; relayer: string | null; call: boolean; wire: string; id: string }[];
};
const seed = fromHex(fx.seed);

function kindOf(call: boolean) {
  return call
    ? callKind(new Uint8Array(32).fill(0xc0), "increment", Uint8Array.of(1, 2, 3), 500_000n)
    : transferKind(new Uint8Array(32).fill(9), 1_000n);
}

describe("transactions match the Rust encoder byte for byte", () => {
  it("public key", async () => {
    expect(toHex(await publicKeyOf(seed))).toBe(fx.public_key);
  });
  for (const c of fx.txs) {
    it(c.name, async () => {
      const wire = await signTransaction(
        {
          chainId: 7n,
          nonce: 3n,
          relayer: c.relayer ? fromHex(c.relayer) : undefined,
          kind: kindOf(c.call),
        },
        seed,
      );
      expect(toHex(wire)).toBe(c.wire);
      expect(toHex(await transactionId(wire))).toBe(c.id);
    });
  }
  it("a relayer sets the tag's high bit and follows it; none is the original encoding", () => {
    const plain = encodeTxBody({ chainId: 7n, nonce: 3n, kind: transferKind(new Uint8Array(32), 1n) });
    const relayed = encodeTxBody({
      chainId: 7n,
      nonce: 3n,
      relayer: new Uint8Array(32).fill(5),
      kind: transferKind(new Uint8Array(32), 1n),
    });
    expect(plain[16]).toBe(2);
    expect(relayed[16]).toBe(2 | 0x80);
    expect(relayed.length).toBe(plain.length + 32);
    expect(relayed.subarray(17, 49)).toEqual(new Uint8Array(32).fill(5));
  });
});

describe("submitting through a relay host", () => {
  function fakeHost() {
    const seen: { url: string; method: string; body?: string }[] = [];
    const fetchFn = (async (url: string, init?: RequestInit) => {
      seen.push({ url, method: init?.method ?? "GET", body: init?.body as string | undefined });
      if (url.endsWith("/relay")) {
        return new Response(JSON.stringify({ relayer: "05".repeat(32), chain_id: 7 }), { status: 200 });
      }
      if (url.endsWith("/tx")) return new Response(JSON.stringify({ id: "ab".repeat(32), relayer: "05".repeat(32) }), { status: 200 });
      return new Response(JSON.stringify({ error: "nope" }), { status: 404 });
    }) as unknown as typeof fetch;
    return { seen, fetchFn };
  }

  it("signs the host's relayer into the transaction and posts it to /tx", async () => {
    const { seen, fetchFn } = fakeHost();
    const c = new BtbClient({ url: `http://h:1/c/${"cd".repeat(32)}`, fetch: fetchFn });
    const r = await c.submit({ seed, chainId: 7n, nonce: 3n, kind: kindOf(false) });
    expect(r.id).toBe("ab".repeat(32));
    expect(seen.map((s) => `${s.method} ${s.url}`)).toEqual(["GET http://h:1/relay", "POST http://h:1/tx"]);
    const expected = fx.txs.find((t) => t.name === "transfer_relayed")!;
    expect(seen[1]!.body).toBe(expected.wire);
  });

  it("does not ask for the relayer when it is configured", async () => {
    const { seen, fetchFn } = fakeHost();
    const c = new BtbClient({ url: "http://h:1", relayer: "05".repeat(32), fetch: fetchFn });
    await c.submit({ seed, chainId: 7n, nonce: 3n, kind: kindOf(false) });
    expect(seen.map((s) => s.url)).toEqual(["http://h:1/tx"]);
  });

  it("reports a refusal", async () => {
    const fetchFn = (async () =>
      new Response(JSON.stringify({ error: "the transaction names a different relayer" }), { status: 400 })) as unknown as typeof fetch;
    const c = new BtbClient({ url: "http://h:1", relayer: "06".repeat(32), fetch: fetchFn });
    await expect(c.submit({ seed, chainId: 7n, nonce: 3n, kind: kindOf(false) })).rejects.toThrow(BtbError);
  });
});
