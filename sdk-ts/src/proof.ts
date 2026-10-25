// Merkle proofs on answers from a BTB hosting node (docs/HOSTING.md).
//
// A proof bundle says: at the state after block `height`, whose Merkle root is `state_root`,
// these full state keys held these values (or were absent). `verifyProof` checks the Jellyfish
// Merkle proofs of the bundle against its root and, when the bundle carries it, the header of
// block `height + 1` (whose `state_root` field names that root).
//
// What this proves: each value is exactly what the tree under the root holds. By itself that
// says nothing about whether the root is the chain's. Three ways to close that gap:
//
//  * `quorum` (recommended): the validators of an epoch (as returned by a validator's
//    `btb_getEpoch`, a trust anchor you supply) attested the root. Every consensus block
//    carries a claim about the execution of one of its ancestors and honest validators vote
//    only if it matches their own execution, so the bundle's `attest` section (a block, the
//    quorum certificate for it, and the state roots up to the claimed height) proves that a
//    quorum of the epoch's stake executed the chain to exactly this root. `verifyProof` checks
//    the Ed25519 signatures, the stake threshold, the block id and the digest chain. See
//    docs/CONSENSUS.md ("Execution attestation") and docs/HOSTING.md.
//  * `trustedRoot`: a root you got elsewhere (a validator's `btb_getBlock`.`state_root_after`
//    or another independent host) must equal the bundle's.
//  * the Rust verifier (`btb-host`) also checks the consensus evidence for the next block's
//    transactions (`commit`), which is not checked here.

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

export interface AttestQc {
  round: number;
  block_id: string;
  /** `[validator index, signature hex]`, sorted by index. */
  votes: [number, string][];
}

export interface AttestBlock {
  round: number;
  height: number;
  parent: string;
  justify: AttestQc;
  proposer: number;
  payload_hash: string;
  /** The execution claim (hex): attested height (u64 LE), digest chain value, receipts root. */
  exec: string;
  id: string;
}

/** Proof that a quorum of an epoch's validators attested the bundle's state root. */
export interface Attestation {
  epoch: number;
  block: AttestBlock;
  qc: AttestQc;
  /** Digest chain value before the bundle's height (null at height 0). */
  prev_digest: string | null;
  /** State roots after blocks `height ..= claimed height`; the first is the bundle's root. */
  roots: string[];
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
  /** Quorum attestation of `state_root` (absent on bundles without enough following blocks). */
  attest?: Attestation | null;
}

export interface VerifiedProof {
  height: number;
  stateRoot: string;
  entries: { key: string; value: Uint8Array | null }[];
  /** The header of block `height + 1` was present, hashes correctly and names `stateRoot`. */
  headerBound: boolean;
  /** The bundle carries consensus evidence for the next block (not verified by this function). */
  hasConsensusEvidence: boolean;
  /** A quorum of the supplied validators of `attestedEpoch` attested `stateRoot`. */
  rootAttested: boolean;
  attestedEpoch: number | null;
}

/** A validator as `btb_getEpoch` returns it (extra fields are ignored). */
export interface ValidatorInfo {
  /** Consensus key (32 bytes hex). */
  key: string;
  /** Stake (decimal string or number). */
  stake: string | number;
}

/** Validator sets you trust, by epoch, and the chain they belong to. */
export interface QuorumTrust {
  /** The chain id of genesis (`btb_chainId`). */
  chainId: number | bigint;
  /** Epoch number to that epoch's validators in consensus order. */
  epochs: Record<number, ValidatorInfo[]>;
}

export interface VerifyOptions {
  /** A root you trust (for example a validator's `state_root_after` for `height`): must match. */
  trustedRoot?: string;
  /**
   * Validator sets to check the state root attestation against. When given, a bundle without
   * a valid attestation for one of these epochs is rejected unless `allowUnattested` is set.
   */
  quorum?: QuorumTrust;
  /** With `quorum`: accept a bundle that carries no attestation (`rootAttested` stays false). */
  allowUnattested?: boolean;
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

function le(n: number | bigint, bytes: number): Uint8Array {
  const out = new Uint8Array(bytes);
  const v = new DataView(out.buffer);
  if (bytes === 8) v.setBigUint64(0, BigInt(n), true);
  else v.setUint32(0, Number(n), true);
  return out;
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

// ------------------------------------------------------------------ state root attestation

async function qcDigest(q: AttestQc): Promise<Uint8Array> {
  const parts: Uint8Array[] = [le(q.round, 8), hash32(q.block_id, "qc block_id"), le(q.votes.length, 4)];
  for (const [i, sig] of q.votes) {
    const sb = fromHex(sig);
    if (sb.length !== 64) throw new ProofError("signature: expected 64 bytes");
    parts.push(le(i, 4), sb);
  }
  return sha256(utf8("BTB/qc/v1"), ...parts);
}

/** The id of a consensus block: what the validators' votes sign (mirrors `Block::id`). */
export async function attestBlockId(b: AttestBlock): Promise<string> {
  const exec = fromHex(b.exec);
  return toHex(
    await sha256(
      utf8("BTB/block/v2"),
      le(b.round, 8),
      le(b.height, 8),
      hash32(b.parent, "block parent"),
      await qcDigest(b.justify),
      le(b.proposer, 4),
      hash32(b.payload_hash, "payload_hash"),
      le(exec.length, 4),
      exec,
    ),
  );
}

interface SetContext {
  genesis: Uint8Array;
  keys: Uint8Array[];
  stakes: bigint[];
  total: bigint;
}

async function setContext(chainId: number | bigint, epoch: number, vals: ValidatorInfo[]): Promise<SetContext> {
  const keys = vals.map((v) => hash32(v.key, "validator key"));
  const stakes = vals.map((v) => BigInt(v.stake));
  const digestParts: Uint8Array[] = [utf8("BTB/valset/v1")];
  keys.forEach((k, i) => digestParts.push(k, le(stakes[i]!, 8)));
  const setDigest = await sha256(...digestParts);
  const chain = new Uint8Array([...utf8("btb-chain-"), ...le(chainId, 8)]);
  const genesis = await sha256(utf8("BTB/genesis/v2"), le(chain.length, 8), chain, le(epoch, 8), setDigest);
  return { genesis, keys, stakes, total: stakes.reduce((a, b) => a + b, 0n) };
}

/** Whether `qc` holds votes of more than 2/3 of the stake of `set` (mirrors `Context::verify_qc`). */
async function verifyQc(set: SetContext, qc: AttestQc): Promise<boolean> {
  if (qc.round <= 0 || qc.round > 2 ** 53) return false;
  const msg = new Uint8Array([
    ...utf8("BTB/vote/v1"),
    ...set.genesis,
    ...le(qc.round, 8),
    ...hash32(qc.block_id, "qc block_id"),
  ]);
  let stake = 0n;
  let prev = -1;
  for (const [who, sigHex] of qc.votes) {
    if (!Number.isInteger(who) || who <= prev || who >= set.keys.length) return false;
    prev = who;
    const sig = fromHex(sigHex);
    if (sig.length !== 64) return false;
    try {
      const key = await globalThis.crypto.subtle.importKey("raw", set.keys[who]! as BufferSource, "Ed25519", false, [
        "verify",
      ]);
      if (!(await globalThis.crypto.subtle.verify("Ed25519", key, sig as BufferSource, msg as BufferSource))) return false;
    } catch {
      return false;
    }
    stake += set.stakes[who]!;
  }
  return stake * 3n > set.total * 2n;
}

/**
 * Check an attestation against the bundle: the certificate is for the block, the block's claim
 * covers the bundle's height, and the roots fold into the claimed digest chain value. Throws
 * `ProofError`; the quorum check against validators is separate (`verifyQc`).
 */
async function checkAttestation(bundle: ProofBundle, a: Attestation): Promise<void> {
  const claim = fromHex(a.block.exec);
  if (claim.length !== 72) throw new ProofError("attestation: malformed claim");
  const kClaim = Number(new DataView(claim.buffer, claim.byteOffset, 8).getBigUint64(0, true));
  const digest = claim.slice(8, 40);
  if ((await attestBlockId(a.block)) !== a.block.id) throw new ProofError("attestation: block id does not match its fields");
  if (a.qc.block_id !== a.block.id || a.qc.round !== a.block.round) {
    throw new ProofError("attestation: the certificate is for another block");
  }
  if (kClaim < bundle.height || kClaim >= a.block.height) {
    throw new ProofError("attestation: the claim does not cover this height");
  }
  if (a.roots.length !== kClaim - bundle.height + 1 || a.roots.length > 256) {
    throw new ProofError("attestation: wrong number of roots");
  }
  if (a.roots[0] !== bundle.state_root) throw new ProofError("attestation: the attested root is not the bundle's root");
  let d: Uint8Array;
  if (bundle.height === 0) {
    d = await sha256(utf8("BTB/exec/genesis/v1"), hash32(a.roots[0]!, "root"));
  } else {
    if (a.prev_digest === null) throw new ProofError("attestation: missing previous digest");
    d = await sha256(utf8("BTB/exec/v1"), hash32(a.prev_digest, "prev_digest"), le(bundle.height, 8), hash32(a.roots[0]!, "root"));
  }
  for (let i = 1; i < a.roots.length; i++) {
    d = await sha256(utf8("BTB/exec/v1"), d, le(bundle.height + i, 8), hash32(a.roots[i]!, "root"));
  }
  if (!eq(d, digest)) throw new ProofError("attestation: the roots do not fold into the attested digest");
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
  let rootAttested = false;
  let attestedEpoch: number | null = null;
  if (opts.quorum) {
    const a = bundle.attest;
    if (!a) {
      if (!opts.allowUnattested) throw new ProofError("no state root attestation in the bundle");
    } else {
      await checkAttestation(bundle, a);
      const vals = opts.quorum.epochs[a.epoch];
      if (!vals) {
        if (!opts.allowUnattested) throw new ProofError(`no validator set supplied for epoch ${a.epoch}`);
      } else {
        const set = await setContext(opts.quorum.chainId, a.epoch, vals);
        if (!(await verifyQc(set, a.qc))) {
          throw new ProofError("attestation: the certificate is not a quorum of the epoch's validators");
        }
        rootAttested = true;
        attestedEpoch = a.epoch;
      }
    }
  }
  return {
    height: bundle.height,
    stateRoot: bundle.state_root,
    entries,
    headerBound,
    hasConsensusEvidence: bundle.commit !== null,
    rootAttested,
    attestedEpoch,
  };
}
