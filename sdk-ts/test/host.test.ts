// Runs the unchanged client against a btb-host node. Started by the Rust test
// `sdk_ts_client_works_against_the_host` (crates/btb-host/tests/host.rs), which sets
// BTB_HOST_URL (http://host/c/<contract>), BTB_HOST_ROOT, BTB_HOST_CONTRACT and BTB_HOST_EXPECT
// (the counter's value when the test starts) and increments the counter while this runs.
// Skipped when run on its own.
import { describe, expect, it } from "vitest";
import { BtbClient, BtbError, ProofError, t, toHex, verifyProof } from "../src/index.js";

const url = process.env.BTB_HOST_URL;
const root = process.env.BTB_HOST_ROOT;
const contract = process.env.BTB_HOST_CONTRACT;
const start = BigInt(process.env.BTB_HOST_EXPECT ?? "0");

describe.skipIf(!url)("BtbClient against btb-host", () => {
  const client = new BtbClient({ url: url ?? "http://unused", caller: "alice" });

  it("queries like it does against btb-dev", async () => {
    expect(await client.query("get", t.unit, [], t.u64)).toBeGreaterThanOrEqual(start);
    const e = await client.query("nope", t.unit, [], t.u64).catch((x) => x);
    expect(e).toBeInstanceOf(BtbError);
    expect(e.status).toBe(404);
  });

  it("refuses mutations with a pointer to the validators", async () => {
    const e = await client.mutation("increment", t.tuple(t.u64), [1n], t.u64).catch((x) => x);
    expect(e).toBeInstanceOf(BtbError);
    expect(e.status).toBe(501);
    expect(e.message).toContain("validator");
  });

  it("receives pushes through subscribe", async () => {
    const seen: bigint[] = [];
    const stop = client.subscribe("get", t.unit, [], t.u64, (v) => seen.push(v));
    const deadline = Date.now() + 30_000;
    while (Date.now() < deadline && !(seen.length >= 3 && seen[seen.length - 1]! > seen[0]!)) {
      await new Promise((r) => setTimeout(r, 100));
    }
    stop();
    expect(seen.length).toBeGreaterThanOrEqual(2);
    for (let i = 1; i < seen.length; i++) expect(seen[i]!).toBeGreaterThan(seen[i - 1]!);
  });

  it("verifies the proof of a key read and rejects a tampered one", async () => {
    const r = await client.queryWithProof("get", t.unit, [], t.u64);
    expect(r.incomplete).toBe(false);
    const v = await verifyProof(r.proof);
    expect(v.entries).toHaveLength(1);
    const stored = v.entries[0]!.value!;
    expect(new DataView(stored.buffer, stored.byteOffset, 8).getBigUint64(0, true)).toBe(r.result);

    const bad = structuredClone(r.proof);
    bad.entries[0]!.value = toHex(new Uint8Array(8).fill(9));
    await expect(verifyProof(bad)).rejects.toBeInstanceOf(ProofError);
    // A root you trust from elsewhere must match.
    await expect(verifyProof(r.proof, { trustedRoot: "00".repeat(32) })).rejects.toThrow(/trusted root/);

    const anchored = await client.proof([new TextEncoder().encode("count")]);
    const av = await verifyProof(anchored);
    expect(av.headerBound).toBe(true);
    expect(av.hasConsensusEvidence).toBe(true);
    expect(root).toBeTruthy();
    expect(contract).toHaveLength(64);
  });
});
