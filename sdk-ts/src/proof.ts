// Merkle proofs on answers from a BTB hosting node (docs/HOSTING.md).
//
// A proof bundle says: at the state after block `height`, whose Merkle root is `state_root`,
// these full state keys held these values (or were absent). `verifyProof` checks the Jellyfish
// Merkle proofs of the bundle against its root and, when the bundle carries it, the header of
// block `height + 1` (whose `state_root` field names that root).
//
// What this proves: each value is exactly what the tree under the root holds. What it does not
// prove: that the root is the chain's. No validator signs state roots yet, so a root is only
// as trustworthy as where you got it. Pass `trustedRoot` (taken from a validator's
// `btb_getBlock`.`state_root_after`, or another independent host) to close that gap, or verify
// the consensus evidence in the bundle with the Rust verifier (`btb-host`), which checks the
// quorum certificates. The evidence is not checked here.

import { fromHex, toHex } from "./codec.js";

export class ProofError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProofError";
  }
}

export interface MerkleProof {
  /** One level per internal node on the path, root first: the 15 sibling hashes (hex). */
  siblings: string[][];
  /** The leaf where the path ends: `[key hash, value hash]` (hex), or null for an empty slot. */
  leaf: [string, string] | null;
}

export interface ProofEntry {
  /** Full state key (hex): `01 contract key` for contract storage. */
  key: string;
  /** Value (hex), or null when the key is proven absent. */
  value: string | null;
  proof: MerkleProof;
}

export interface ProofHeader {
  height: number;
  parent_hash: string;
  timestamp: number;
  state_root: string;
  tx_root: string;
  hash: string;
}

export interface ProofBundle {
  version: number;
  /** The state after this block. */
  height: number;
  state_root: string;
  entries: ProofEntry[];
  /** Header of block `height + 1`, if the host had executed it. */
  header: ProofHeader | null;
  /** Consensus evidence (opaque here; see the Rust verifier). */
  commit: { epoch: number; sync: string } | null;
}

export interface VerifiedProof {
  height: number;
  stateRoot: string;
  entries: { key: string; value: Uint8Array | null }[];
  /** The header of block `height + 1` was present, hashes correctly and names `stateRoot`. */
  headerBound: boolean;
  /** The bundle carries consensus evidence (not verified by this function). */
  hasConsensusEvidence: boolean;
}

export interface VerifyOptions {
  /** A root you trust (for example a validator's `state_root_after` for `height`): must match. */
  trustedRoot?: string;
  /** Keys (hex) and values that must be among the proven entries; `null` means proven absent. */
  expect?: { key: string; value: Uint8Array | null }[];
}

const EMPTY = new Uint8Array(32);

async function sha256(...parts: Uint8Array[]): Promise<Uint8Array> {
  let n = 0;
  for (const p of parts) n += p.length;
  const buf = new Uint8Array(n);
  let o = 0;
  for (const p of parts) {
    buf.set(p, o);
    o += p.length;
  }
  return new Uint8Array(await globalThis.crypto.subtle.digest("SHA-256", buf as BufferSource));
}

const utf8 = (s: string) => new TextEncoder().encode(s);
const eq = (a: Uint8Array, b: Uint8Array) => a.length === b.length && a.every((x, i) => x === b[i]);

function be64(n: number | bigint): Uint8Array {
  const out = new Uint8Array(8);
  new DataView(out.buffer).setBigUint64(0, BigInt(n));
  return out;
}

function hash32(hex: string, what: string): Uint8Array {
  let b: Uint8Array;
  try {
    b = fromHex(hex);
  } catch {
    throw new ProofError(`${what}: not hex`);
  }
  if (b.length !== 32) throw new ProofError(`${what}: expected 32 bytes`);
  return b;
}

function nibble(h: Uint8Array, depth: number): number {
  const b = h[depth >> 1]!;
  return depth % 2 === 0 ? b >> 4 : b & 0x0f;
}

/** Check one Jellyfish Merkle proof (mirrors `btb_runtime::merkle::verify`). */
export async function verifyMerkle(
  root: Uint8Array,
  key: Uint8Array,
  value: Uint8Array | null,
  proof: MerkleProof,
): Promise<boolean> {
  const kh = await sha256(key);
  const levels = proof.siblings.length;
  if (levels > 64 || proof.siblings.some((s) => s.length !== 15)) return false;
  let cur: Uint8Array;
  if (proof.leaf) {
    const lk = hash32(proof.leaf[0], "leaf key hash");
    const lv = hash32(proof.leaf[1], "leaf value hash");
    if (value) {
      if (!eq(lk, kh) || !eq(lv, await sha256(value))) return false;
    } else {
      // Another key's leaf: it must sit on this key's path and not be this key.
      if (eq(lk, kh)) return false;
      for (let d = 0; d < levels; d++) if (nibble(lk, d) !== nibble(kh, d)) return false;
    }
    cur = await sha256(new Uint8Array([0x00]), lk, lv);
  } else {
    if (value) return false;
    cur = EMPTY;
  }
  for (let d = levels - 1; d >= 0; d--) {
    const n = nibble(kh, d);
    const sib = proof.siblings[d]!;
    const parts: Uint8Array[] = [new Uint8Array([0x01])];
    let k = 0;
    for (let i = 0; i < 16; i++) parts.push(i === n ? cur : hash32(sib[k++]!, "sibling"));
    cur = await sha256(...parts);
  }
  return eq(cur, root);
}

/** The hash of a block header: `SHA256("btb.block.v1" || height || parent || timestamp || state_root || tx_root)`. */
export async function headerHash(h: ProofHeader): Promise<string> {
  return toHex(
    await sha256(
      utf8("btb.block.v1"),
      be64(h.height),
      hash32(h.parent_hash, "parent_hash"),
      be64(h.timestamp),
      hash32(h.state_root, "header state_root"),
      hash32(h.tx_root, "tx_root"),
    ),
  );
}

/**
 * Verify a proof bundle from a hosting node. Throws `ProofError` if anything does not check out;
 * otherwise returns the proven `(key, value)` pairs. See the file header for what that means.
 */
export async function verifyProof(bundle: ProofBundle, opts: VerifyOptions = {}): Promise<VerifiedProof> {
  const root = hash32(bundle.state_root, "state_root");
  if (opts.trustedRoot !== undefined && !eq(root, hash32(opts.trustedRoot, "trustedRoot"))) {
    throw new ProofError("state root differs from the trusted root");
  }
  const entries: VerifiedProof["entries"] = [];
  for (const [i, e] of bundle.entries.entries()) {
    const key = fromHex(e.key);
    const value = e.value === null ? null : fromHex(e.value);
    if (!(await verifyMerkle(root, key, value, e.proof))) throw new ProofError(`entry ${i} does not match the state root`);
    entries.push({ key: e.key, value });
  }
  for (const want of opts.expect ?? []) {
    const got = entries.find((e) => e.key === want.key);
    if (!got) throw new ProofError(`key ${want.key} is not in the bundle`);
    const same = want.value === null ? got.value === null : got.value !== null && eq(got.value, want.value);
    if (!same) throw new ProofError(`key ${want.key} has another value than expected`);
  }
  let headerBound = false;
  if (bundle.header) {
    const h = bundle.header;
    if (h.height !== bundle.height + 1) throw new ProofError("header is not the block after the proven state");
    if (!eq(hash32(h.state_root, "header state_root"), root)) throw new ProofError("header names another state root");
    if ((await headerHash(h)) !== h.hash) throw new ProofError("header hash does not match its fields");
    headerBound = true;
  } else if (bundle.commit) {
    throw new ProofError("consensus evidence without a header");
  }
  return {
    height: bundle.height,
    stateRoot: bundle.state_root,
    entries,
    headerBound,
    hasConsensusEvidence: bundle.commit !== null,
  };
}
