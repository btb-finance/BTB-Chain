import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  type AuthTransport,
  type ChainInfo,
  BtbAuth,
  BtbClient,
  LocalKeyWallet,
  MemoryKeyStore,
  accountAddress,
  callKind,
  fromHex,
  grantKind,
  publicKeyOf,
  revokeKind,
  signTransaction,
  toHex,
  transactionId,
  encodeTxBody,
} from "../src/index.js";

const fx = JSON.parse(readFileSync(new URL("./fixtures/session.json", import.meta.url), "utf8")) as {
  account_seed: string;
  session_seed: string;
  account: string;
  session_key: string;
  txs: { name: string; wire: string; id: string }[];
};
const accountSeed = fromHex(fx.account_seed);
const sessionSeed = fromHex(fx.session_seed);
const want = (n: string) => fx.txs.find((t) => t.name === n)!;

describe("session transactions match the Rust encoder byte for byte", () => {
  it("addresses", async () => {
    expect(toHex(await accountAddress(await publicKeyOf(accountSeed)))).toBe(fx.account);
    expect(toHex(await publicKeyOf(sessionSeed))).toBe(fx.session_key);
  });
  const call = callKind(new Uint8Array(32).fill(0xc0), "increment", Uint8Array.of(1, 2, 3), 500_000n);
  it("grant", async () => {
    const kind = grantKind({
      sessionKey: fromHex(fx.session_key),
      expiresHeight: 1000n,
      allowed: [
        { contract: new Uint8Array(32).fill(0xc0), functions: ["increment", "put"] },
        { contract: new Uint8Array(32).fill(0xc1) },
      ],
      spendLimit: 5_000_000n,
    });
    const wire = await signTransaction({ chainId: 7n, nonce: 4n, kind }, accountSeed);
    expect(toHex(wire)).toBe(want("grant").wire);
    expect(toHex(await transactionId(wire))).toBe(want("grant").id);
  });
  it("revoke", async () => {
    const wire = await signTransaction({ chainId: 7n, nonce: 5n, kind: revokeKind(fromHex(fx.session_key)) }, accountSeed);
    expect(toHex(wire)).toBe(want("revoke").wire);
  });
  it("session call, with and without a relayer", async () => {
    const account = fromHex(fx.account);
    const a = await signTransaction({ chainId: 7n, nonce: 6n, session: account, kind: call }, sessionSeed);
    expect(toHex(a)).toBe(want("session_call").wire);
    const b = await signTransaction(
      { chainId: 7n, nonce: 6n, relayer: new Uint8Array(32).fill(5), session: account, kind: call },
      sessionSeed,
    );
    expect(toHex(b)).toBe(want("session_call_relayed").wire);
  });
  it("tag bits: relayer 0x80, session 0x40", () => {
    const body = (o: object) => encodeTxBody({ chainId: 1n, nonce: 1n, kind: revokeKind(new Uint8Array(32)), ...o });
    expect(body({})[16]).toBe(22);
    expect(body({ session: new Uint8Array(32) })[16]).toBe(22 | 0x40);
    expect(body({ session: new Uint8Array(32), relayer: new Uint8Array(32) })[16]).toBe(22 | 0xc0);
  });
});

/** A fake chain that keeps heights and nonces and records what it was sent. */
function fakeChain() {
  const sent: Uint8Array[] = [];
  const info: ChainInfo = { chainId: 7n, height: 100n, nonce: 0n };
  const transport: AuthTransport = {
    async info() {
      return { ...info };
    },
    async sendSession(w) {
      sent.push(w);
      info.nonce += 1n;
    },
  };
  return { sent, info, transport };
}

describe("BtbAuth", () => {
  const contract = new Uint8Array(32).fill(0xc0);
  const mk = (extra = {}) => {
    const chain = fakeChain();
    const wallet = new LocalKeyWallet(accountSeed);
    const auth = new BtbAuth({ wallet, transport: chain.transport, contract, store: new MemoryKeyStore(), ...extra });
    return { chain, wallet, auth };
  };

  it("login signs one grant with the wallet and then calls are signed by the session key", async () => {
    const { chain, auth } = mk({ functions: ["increment"], ttlBlocks: 50n, spendLimit: 777n });
    expect(auth.identity).toBeNull();
    const id = await auth.login();
    expect(id.account).toBe(fx.account);
    expect(id.expiresHeight).toBe(150n);
    expect(chain.sent.length).toBe(1);
    // The grant on the wire is what the wallet signed for exactly this session key.
    const expected = await signTransaction(
      {
        chainId: 7n,
        nonce: 0n,
        kind: grantKind({
          sessionKey: fromHex(id.sessionKey),
          expiresHeight: 150n,
          allowed: [{ contract, functions: ["increment"] }],
          spendLimit: 777n,
        }),
      },
      accountSeed,
    );
    expect(toHex(chain.sent[0]!)).toBe(toHex(expected));

    const w1 = await auth.signCall("increment", Uint8Array.of(1));
    const w2 = await auth.signCall("increment", Uint8Array.of(2));
    // Wire layout: nonce at 8, tag at 16 (session bit set), account follows, session key at the end.
    for (const [w, n] of [[w1, 1n], [w2, 2n]] as const) {
      expect(new DataView(w.buffer, w.byteOffset).getBigUint64(8, true)).toBe(n + 0n); // account nonce 1 after grant
      expect(w[16]).toBe(1 | 0x40);
      expect(toHex(w.subarray(17, 49))).toBe(fx.account);
      expect(toHex(w.subarray(w.length - 96, w.length - 64))).toBe(id.sessionKey);
    }
    // The key never left WebCrypto: it is not the wallet's key.
    expect(id.sessionKey).not.toBe(toHex(await publicKeyOf(accountSeed)));
  });

  it("the session key is non-extractable", async () => {
    const store = new MemoryKeyStore();
    const { auth } = mk({ store });
    await auth.login();
    const s = await store.get("default");
    expect(s!.privateKey.extractable).toBe(false);
    await expect(crypto.subtle.exportKey("pkcs8", s!.privateKey)).rejects.toThrow();
  });

  it("restore finds a stored session and drops an expired one", async () => {
    const store = new MemoryKeyStore();
    const a = mk({ store, ttlBlocks: 10n });
    await a.auth.login();
    const b = new BtbAuth({ wallet: a.wallet, transport: a.chain.transport, contract, store });
    expect((await b.restore())?.account).toBe(fx.account);
    a.chain.info.height = 110n; // the grant expires at 110
    const c = new BtbAuth({ wallet: a.wallet, transport: a.chain.transport, contract, store });
    expect(await c.restore()).toBeNull();
    expect(await store.get("default")).toBeUndefined();
  });

  it("logout forgets the key, and revoke has the wallet send a Revoke", async () => {
    const { chain, auth } = mk();
    const id = await auth.login();
    await auth.logout({ revoke: true });
    expect(auth.identity).toBeNull();
    expect(chain.sent.length).toBe(2);
    const expected = await signTransaction(
      { chainId: 7n, nonce: 1n, kind: revokeKind(fromHex(id.sessionKey)) },
      accountSeed,
    );
    expect(toHex(chain.sent[1]!)).toBe(toHex(expected));
    await expect(auth.signCall("increment", new Uint8Array())).rejects.toThrow("not logged in");
  });

  it("the client sends mutations signed by the session and queries as the account", async () => {
    const { auth } = mk();
    await auth.login();
    const seen: { url: string; headers?: Record<string, string> }[] = [];
    const fetchFn = (async (url: string, init?: RequestInit) => {
      seen.push({ url, headers: init?.headers as Record<string, string> });
      return new Response("", { status: 200 });
    }) as unknown as typeof fetch;
    const c = new BtbClient({ url: "http://dev:1", fetch: fetchFn, auth });
    await c.mutation("increment", { encode: () => new Uint8Array(), decode: () => [] } as never, [], { decode: () => 0 } as never).catch(() => {});
    expect(seen[0]!.headers!["x-btb-tx"]).toMatch(/^[0-9a-f]+$/);
    expect(c.caller).toBe(fx.account);
  });
});
