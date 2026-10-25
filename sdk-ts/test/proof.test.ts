// Verifies a real bundle produced by a btb-host proof endpoint against a 4-validator cluster
// (fixtures/proof.json; regenerate with BTB_UPDATE_FIXTURES=1 cargo test -p btb-host --test host).
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { type ProofBundle, ProofError, attestBlockId, fromHex, headerHash, toHex, verifyMerkle, verifyProof } from "../src/index.js";

const fx = JSON.parse(readFileSync(resolve(import.meta.dirname, "fixtures/proof.json"), "utf8")) as {
  full_key: string;
  value: string;
  absent_key: string;
  bundle: ProofBundle;
  absent_bundle: ProofBundle;
  chain_id: number;
  epoch: { validators: { key: string; stake: string }[] };
};
const trust = { chainId: fx.chain_id, epochs: { 0: fx.epoch.validators } };
const fresh = () => structuredClone(fx.bundle);

describe("verifyProof", () => {
  it("accepts a real bundle", async () => {
    const v = await verifyProof(fx.bundle, { expect: [{ key: fx.full_key, value: fromHex(fx.value) }] });
    expect(v.headerBound).toBe(true);
    expect(v.hasConsensusEvidence).toBe(true);
    expect(toHex(v.entries[0]!.value!)).toBe(fx.value);
    expect(await headerHash(fx.bundle.header!)).toBe(fx.bundle.header!.hash);
    await verifyProof(fx.bundle, { trustedRoot: fx.bundle.state_root });
  });

  it("rejects a changed value, key, root or sibling", async () => {
    const a = fresh();
    a.entries[0]!.value = "ff" + a.entries[0]!.value!.slice(2);
    await expect(verifyProof(a)).rejects.toBeInstanceOf(ProofError);
    const b = fresh();
    b.entries[0]!.key = b.entries[0]!.key.slice(0, -2) + "00";
    await expect(verifyProof(b)).rejects.toBeInstanceOf(ProofError);
    const c = fresh();
    c.state_root = "00" + c.state_root.slice(2);
    await expect(verifyProof(c)).rejects.toBeInstanceOf(ProofError);
    const d = fresh();
    d.entries[0]!.proof.siblings[0]![0] = "00".repeat(32);
    await expect(verifyProof(d)).rejects.toBeInstanceOf(ProofError);
    const e = fresh();
    e.entries[0]!.value = null; // claims absence for a key that exists
    await expect(verifyProof(e)).rejects.toBeInstanceOf(ProofError);
  });

  it("rejects a bad header, a wrong trusted root and unmet expectations", async () => {
    const a = fresh();
    a.header!.timestamp += 1;
    await expect(verifyProof(a)).rejects.toThrow(/hash/);
    const b = fresh();
    b.header!.height += 1;
    await expect(verifyProof(b)).rejects.toThrow(/block after/);
    const c = fresh();
    c.header = null;
    await expect(verifyProof(c)).rejects.toThrow(/without a header/);
    await expect(verifyProof(fx.bundle, { trustedRoot: "11".repeat(32) })).rejects.toThrow(/trusted root/);
    await expect(verifyProof(fx.bundle, { expect: [{ key: fx.full_key, value: new Uint8Array(8) }] })).rejects.toThrow(
      /another value/,
    );
    await expect(verifyProof(fx.bundle, { expect: [{ key: "01", value: null }] })).rejects.toThrow(/not in the bundle/);
  });

  it("verifies absence and rejects a forged presence", async () => {
    const v = await verifyProof(fx.absent_bundle, { expect: [{ key: fx.absent_key, value: null }] });
    expect(v.entries[0]!.value).toBeNull();
    const forged = structuredClone(fx.absent_bundle);
    forged.entries[0]!.value = "01";
    await expect(verifyProof(forged)).rejects.toBeInstanceOf(ProofError);
    // An existing key cannot be passed off as absent either.
    const lie = structuredClone(fx.bundle);
    lie.entries[0]!.value = null;
    await expect(verifyProof(lie)).rejects.toBeInstanceOf(ProofError);
    expect(await verifyMerkle(fromHex(fx.bundle.state_root), new Uint8Array([1]), null, { siblings: [], leaf: null })).toBe(
      false,
    );
  });
});

describe("verifyProof: state root attestation", () => {
  it("accepts the real bundle against the epoch's validators", async () => {
    const v = await verifyProof(fx.bundle, { quorum: trust, expect: [{ key: fx.full_key, value: fromHex(fx.value) }] });
    expect(v.rootAttested).toBe(true);
    expect(v.attestedEpoch).toBe(0);
    expect(await attestBlockId(fx.bundle.attest!.block)).toBe(fx.bundle.attest!.block.id);
    // Without a trust anchor the attestation is checked for consistency but not as a quorum.
    expect((await verifyProof(fx.bundle)).rootAttested).toBe(false);
  });

  it("rejects a forged root, even with the entries replaced to fit", async () => {
    const t = fresh();
    t.state_root = "00" + t.state_root.slice(2);
    await expect(verifyProof(t, { quorum: trust })).rejects.toBeInstanceOf(ProofError);
    // Consistent lie about every root, no entries to contradict it: the signed digest does not fold.
    const u = fresh();
    u.entries = [];
    u.header = null;
    u.commit = null;
    u.state_root = "ab" + u.state_root.slice(2);
    u.attest!.roots[0] = u.state_root;
    await expect(verifyProof(u, { quorum: trust })).rejects.toThrow(/fold/);
  });

  it("rejects forged certificates and claims", async () => {
    const flip = (hex: string) => (hex.startsWith("00") ? "01" : "00") + hex.slice(2);
    const a = fresh();
    a.attest!.qc.votes[0]![1] = flip(a.attest!.qc.votes[0]![1]);
    await expect(verifyProof(a, { quorum: trust })).rejects.toThrow(/quorum/);
    const b = fresh();
    b.attest!.qc.votes = b.attest!.qc.votes.slice(0, 1);
    await expect(verifyProof(b, { quorum: trust })).rejects.toThrow(/quorum/);
    const c = fresh();
    c.attest!.block.exec = flip(c.attest!.block.exec);
    await expect(verifyProof(c, { quorum: trust })).rejects.toThrow(/block id/);
    const d = fresh();
    d.attest!.qc.block_id = flip(d.attest!.qc.block_id);
    await expect(verifyProof(d, { quorum: trust })).rejects.toThrow(/another block/);
    const e = fresh();
    e.attest!.prev_digest = "11".repeat(32);
    await expect(verifyProof(e, { quorum: trust })).rejects.toThrow(/fold/);
    const f = fresh();
    f.attest!.roots.pop();
    await expect(verifyProof(f, { quorum: trust })).rejects.toThrow(/roots/);
    // The same signatures duplicated to fake stake.
    const g = fresh();
    g.attest!.qc.votes = [g.attest!.qc.votes[0]!, g.attest!.qc.votes[0]!, g.attest!.qc.votes[0]!];
    await expect(verifyProof(g, { quorum: trust })).rejects.toThrow(/quorum/);
  });

  it("rejects validators who are not the epoch's, and missing attestations unless allowed", async () => {
    const other = { chainId: fx.chain_id, epochs: { 0: fx.epoch.validators.map((v) => ({ ...v, stake: "1" })) } };
    await expect(verifyProof(fx.bundle, { quorum: other })).rejects.toThrow(/quorum/);
    await expect(verifyProof(fx.bundle, { quorum: { ...trust, chainId: fx.chain_id + 1 } })).rejects.toThrow(/quorum/);
    await expect(verifyProof(fx.bundle, { quorum: { chainId: fx.chain_id, epochs: {} } })).rejects.toThrow(/validator set/);
    const bare = fresh();
    bare.attest = null;
    await expect(verifyProof(bare, { quorum: trust })).rejects.toThrow(/no state root attestation/);
    const v = await verifyProof(bare, { quorum: trust, allowUnattested: true });
    expect(v.rootAttested).toBe(false);
  });
});
