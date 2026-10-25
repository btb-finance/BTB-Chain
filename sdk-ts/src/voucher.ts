/**
 * Payment-channel vouchers: the TypeScript twin of the `btb-voucher` crate.
 *
 * A voucher says "on chain `chainId`, in channel `channelId`, the payer has paid `amount` in
 * total". Amounts are cumulative and are bigints (base units, u64). The signed message is
 * `SHA256("btb.voucher.v1" || chain_id:u64le || channel_id[32] || amount:u64le || nonce:u64le)`
 * signed with Ed25519, byte for byte what Rust produces (`test/fixtures/voucher.json`).
 *
 * Uses WebCrypto (Ed25519 and SHA-256: Node 20+, current browsers), so everything is async.
 * Verification here follows WebCrypto, which is slightly looser than the chain's strict check
 * (for example it may accept some non-canonical signatures); the chain has the last word.
 */

const DOMAIN = new TextEncoder().encode("btb.voucher.v1");
const U64_MAX = (1n << 64n) - 1n;
/** PKCS#8 prefix for an Ed25519 private key; the 32-byte seed follows. */
const PKCS8_PREFIX = Uint8Array.from([
  0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
]);

export const VOUCHER_SIZE = 8 + 32 + 8 + 8 + 64;

export interface Voucher {
  chainId: bigint;
  /** 32 bytes. */
  channelId: Uint8Array;
  /** Cumulative amount paid, base units. */
  amount: bigint;
  nonce: bigint;
}

export interface SignedVoucher {
  voucher: Voucher;
  /** 64 bytes. */
  signature: Uint8Array;
}

export class VoucherError extends Error {
  constructor(
    readonly code:
      | "wrong_chain"
      | "wrong_channel"
      | "bad_signature"
      | "not_increasing"
      | "stale_nonce"
      | "exceeds_deposit"
      | "bad_amount"
      | "bad_length",
    message?: string,
  ) {
    super(message ?? code);
    this.name = "VoucherError";
  }
}

function checkU64(x: bigint, what: string): void {
  if (typeof x !== "bigint" || x < 0n || x > U64_MAX) {
    throw new VoucherError("bad_amount", `${what} is not a u64`);
  }
}

function checkChannel(id: Uint8Array): void {
  if (id.length !== 32) throw new VoucherError("bad_length", "channel id must be 32 bytes");
}

function u64le(x: bigint): Uint8Array {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, x, true);
  return b;
}

function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

const bufs = (b: Uint8Array): ArrayBuffer =>
  b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength) as ArrayBuffer;

/** The 32-byte hash the payer signs. */
export async function voucherDigest(v: Voucher): Promise<Uint8Array> {
  checkU64(v.chainId, "chainId");
  checkU64(v.amount, "amount");
  checkU64(v.nonce, "nonce");
  checkChannel(v.channelId);
  const msg = concat(DOMAIN, u64le(v.chainId), v.channelId, u64le(v.amount), u64le(v.nonce));
  return new Uint8Array(await crypto.subtle.digest("SHA-256", bufs(msg)));
}

async function importSeed(seed: Uint8Array): Promise<CryptoKey> {
  if (seed.length !== 32) throw new VoucherError("bad_length", "seed must be 32 bytes");
  return crypto.subtle.importKey("pkcs8", bufs(concat(PKCS8_PREFIX, seed)), "Ed25519", false, [
    "sign",
  ]);
}

/** Sign a voucher with the payer's 32-byte Ed25519 seed. */
export async function signVoucher(v: Voucher, seed: Uint8Array): Promise<SignedVoucher> {
  const digest = await voucherDigest(v);
  const sig = await crypto.subtle.sign("Ed25519", await importSeed(seed), bufs(digest));
  return { voucher: v, signature: new Uint8Array(sig) };
}

/** Whether `payerKey` (32-byte Ed25519 public key) signed exactly this voucher. */
export async function verifyVoucher(sv: SignedVoucher, payerKey: Uint8Array): Promise<boolean> {
  if (sv.signature.length !== 64 || payerKey.length !== 32) return false;
  try {
    const key = await crypto.subtle.importKey("raw", bufs(payerKey), "Ed25519", false, ["verify"]);
    const digest = await voucherDigest(sv.voucher);
    return await crypto.subtle.verify("Ed25519", key, bufs(sv.signature), bufs(digest));
  } catch {
    return false;
  }
}

/** Wire form, 120 bytes: what `ChannelOp::Close` carries. */
export function encodeVoucher(sv: SignedVoucher): Uint8Array {
  const v = sv.voucher;
  checkU64(v.chainId, "chainId");
  checkU64(v.amount, "amount");
  checkU64(v.nonce, "nonce");
  checkChannel(v.channelId);
  if (sv.signature.length !== 64) throw new VoucherError("bad_length", "signature must be 64 bytes");
  return concat(u64le(v.chainId), v.channelId, u64le(v.amount), u64le(v.nonce), sv.signature);
}

export function decodeVoucher(b: Uint8Array): SignedVoucher {
  if (b.length !== VOUCHER_SIZE) throw new VoucherError("bad_length", "voucher must be 120 bytes");
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  return {
    voucher: {
      chainId: dv.getBigUint64(0, true),
      channelId: b.slice(8, 40),
      amount: dv.getBigUint64(40, true),
      nonce: dv.getBigUint64(48, true),
    },
    signature: b.slice(56),
  };
}

/** Payer side: counts what has been spent in one channel and signs the next voucher. */
export class VoucherSpender {
  private _spent = 0n;
  private _nonce = 0n;
  constructor(
    readonly chainId: bigint,
    readonly channelId: Uint8Array,
    private deposit: bigint,
    private readonly seed: Uint8Array,
  ) {
    checkU64(chainId, "chainId");
    checkU64(deposit, "deposit");
    checkChannel(channelId);
  }
  get spent(): bigint {
    return this._spent;
  }
  get remaining(): bigint {
    return this.deposit - this._spent;
  }
  addDeposit(amount: bigint): void {
    checkU64(amount, "amount");
    if (this.deposit + amount > U64_MAX) throw new VoucherError("bad_amount", "deposit overflow");
    this.deposit += amount;
  }
  /** Pay `price` more. Never signs above the deposit; nothing changes if it throws. */
  async pay(price: bigint): Promise<SignedVoucher> {
    checkU64(price, "price");
    if (price === 0n) throw new VoucherError("bad_amount", "price is zero");
    const amount = this._spent + price;
    if (amount > U64_MAX) throw new VoucherError("bad_amount", "amount overflow");
    if (amount > this.deposit) throw new VoucherError("exceeds_deposit");
    const nonce = this._nonce + 1n;
    const sv = await signVoucher(
      { chainId: this.chainId, channelId: this.channelId, amount, nonce },
      this.seed,
    );
    this._spent = amount;
    this._nonce = nonce;
    return sv;
  }
}

/** Payee (host) side: accepts vouchers for one channel and keeps the highest. */
export class VoucherLedger {
  private _best: SignedVoucher | null = null;
  constructor(
    readonly chainId: bigint,
    readonly channelId: Uint8Array,
    readonly payerKey: Uint8Array,
    private deposit: bigint,
  ) {
    checkChannel(channelId);
  }
  get earned(): bigint {
    return this._best?.voucher.amount ?? 0n;
  }
  /** The voucher to submit on chain to claim `earned`. */
  get best(): SignedVoucher | null {
    return this._best;
  }
  addDeposit(amount: bigint): void {
    checkU64(amount, "amount");
    this.deposit += amount;
  }
  /** Check and keep a voucher; returns the newly earned amount. Throws a `VoucherError`. */
  async accept(sv: SignedVoucher): Promise<bigint> {
    const v = sv.voucher;
    if (v.chainId !== this.chainId) throw new VoucherError("wrong_chain");
    if (v.channelId.length !== 32 || !v.channelId.every((x, i) => x === this.channelId[i])) {
      throw new VoucherError("wrong_channel");
    }
    if (!(await verifyVoucher(sv, this.payerKey))) throw new VoucherError("bad_signature");
    if (this._best) {
      if (v.amount <= this._best.voucher.amount) throw new VoucherError("not_increasing");
      if (v.nonce <= this._best.voucher.nonce) throw new VoucherError("stale_nonce");
    }
    if (v.amount > this.deposit) throw new VoucherError("exceeds_deposit");
    const delta = v.amount - this.earned;
    this._best = sv;
    return delta;
  }
}
