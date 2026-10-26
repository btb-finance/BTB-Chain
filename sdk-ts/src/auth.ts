/**
 * Browser auth with session keys (docs/TS_SDK.md, docs/CHAIN.md "Session keys").
 *
 * `BtbAuth` creates an Ed25519 session key with WebCrypto (the private key is non-extractable
 * and lives in IndexedDB, so page scripts can use it but never read it), asks a wallet to sign
 * a scoped, expiring, budgeted `Grant` ONCE, and then signs every mutation with the session
 * key. On chain the transaction acts as the account (`caller()` = account) only inside the
 * grant: other contracts or functions, expiry and a spent fee budget are refused.
 *
 * A `Wallet` holds the account key. `LocalKeyWallet` keeps a seed in memory (demos, tests,
 * `btb-dev --auth signed`); a real wallet implements the same two methods.
 */
import { BtbClient } from "./client.js";
import { fromHex, toHex } from "./codec.js";
import {
  type SessionGrant,
  type SessionScope,
  type TxBody,
  type TxKind,
  accountAddress,
  callKind,
  grantKind,
  publicKeyOf,
  revokeKind,
  signTransaction,
  signTransactionWith,
} from "./tx.js";

/** The account key's owner. It signs the grant (and an optional revoke), nothing else. */
export interface Wallet {
  /** The 32-byte account address (`SHA256("btb.account.v1" || public key)`). */
  address(): Promise<Uint8Array>;
  /** Sign a transaction body; return the wire bytes. Called with `Session` transactions only. */
  signTransaction(body: TxBody): Promise<Uint8Array>;
}

/** A wallet over a 32-byte Ed25519 seed held in memory. For demos and tests only. */
export class LocalKeyWallet implements Wallet {
  constructor(private readonly seed: Uint8Array) {}
  static random(): LocalKeyWallet {
    return new LocalKeyWallet(crypto.getRandomValues(new Uint8Array(32)));
  }
  async address(): Promise<Uint8Array> {
    return accountAddress(await publicKeyOf(this.seed));
  }
  signTransaction(body: TxBody): Promise<Uint8Array> {
    return signTransaction(body, this.seed);
  }
}

/** Where a session key is kept between page loads. */
export interface KeyStore {
  get(name: string): Promise<StoredSession | undefined>;
  set(name: string, v: StoredSession): Promise<void>;
  delete(name: string): Promise<void>;
}

export interface StoredSession {
  privateKey: CryptoKey;
  publicKey: Uint8Array;
  account: Uint8Array;
  expiresHeight: bigint;
}

export class MemoryKeyStore implements KeyStore {
  private readonly m = new Map<string, StoredSession>();
  async get(name: string) {
    return this.m.get(name);
  }
  async set(name: string, v: StoredSession) {
    this.m.set(name, v);
  }
  async delete(name: string) {
    this.m.delete(name);
  }
}

/** IndexedDB store: a non-extractable `CryptoKey` is structured-cloneable, so the key bytes never exist in JS. */
export class IndexedDbKeyStore implements KeyStore {
  constructor(private readonly dbName = "btb-auth") {}
  private open(): Promise<IDBDatabase> {
    return new Promise((res, rej) => {
      const r = indexedDB.open(this.dbName, 1);
      r.onupgradeneeded = () => r.result.createObjectStore("sessions");
      r.onsuccess = () => res(r.result);
      r.onerror = () => rej(r.error);
    });
  }
  private async run<T>(mode: IDBTransactionMode, f: (s: IDBObjectStore) => IDBRequest<T>): Promise<T> {
    const db = await this.open();
    return new Promise<T>((res, rej) => {
      const req = f(db.transaction("sessions", mode).objectStore("sessions"));
      req.onsuccess = () => res(req.result);
      req.onerror = () => rej(req.error);
    }).finally(() => db.close());
  }
  get(name: string) {
    return this.run<StoredSession | undefined>("readonly", (s) => s.get(name));
  }
  async set(name: string, v: StoredSession) {
    await this.run("readwrite", (s) => s.put(v, name));
  }
  async delete(name: string) {
    await this.run("readwrite", (s) => s.delete(name));
  }
}

/** What the chain (or dev server) says about an account right now. */
export interface ChainInfo {
  chainId: bigint;
  /** Height of the next block. */
  height: bigint;
  /** The account's next nonce. */
  nonce: bigint;
}

/** How the grant reaches the chain and where the facts come from. */
export interface AuthTransport {
  info(account: Uint8Array): Promise<ChainInfo>;
  /** Submit a signed Session transaction (grant or revoke). */
  sendSession(wire: Uint8Array): Promise<void>;
  /** The relayer to sign into every transaction (a relay host accepts only its own). */
  relayer?(): Promise<Uint8Array | undefined>;
}

/** `btb-dev --auth signed`: `GET /auth/info`, `POST /auth/tx`. */
export function devTransport(url: string, fetchFn: typeof fetch = (...a) => globalThis.fetch(...a)): AuthTransport {
  const base = url.replace(/\/+$/, "");
  return {
    async info(account) {
      const r = await fetchFn(`${base}/auth/info?account=${toHex(account)}`);
      const j = (await r.json()) as { chain_id: number; height: number; nonce: number; error?: string };
      if (!r.ok) throw new Error(j.error ?? `auth/info ${r.status}`);
      return { chainId: BigInt(j.chain_id), height: BigInt(j.height), nonce: BigInt(j.nonce) };
    },
    async sendSession(wire) {
      const r = await fetchFn(`${base}/auth/tx`, { method: "POST", body: toHex(wire) });
      if (!r.ok) throw new Error(((await r.json().catch(() => ({}))) as { error?: string }).error ?? `auth/tx ${r.status}`);
    },
  };
}

/**
 * A hosting node that relays (`GET /account/<addr>`, `GET /relay`, `POST /tx`): chain facts come
 * from the host and every transaction names the host as relayer. `client` must point at the host.
 */
export function hostTransport(client: BtbClient, rootUrl: string, fetchFn: typeof fetch = (...a) => globalThis.fetch(...a)): AuthTransport {
  const base = rootUrl.replace(/\/+$/, "");
  let relayer: Uint8Array | undefined;
  return {
    async info(account) {
      const r = await fetchFn(`${base}/account/${toHex(account)}`);
      const j = (await r.json()) as { nonce: number; height: number; chain_id: number; error?: string };
      if (!r.ok) throw new Error(j.error ?? `account ${r.status}`);
      return { chainId: BigInt(j.chain_id), height: BigInt(j.height), nonce: BigInt(j.nonce) };
    },
    async sendSession(wire) {
      const { id } = await client.submitSigned(wire);
      const r = await client.waitForReceipt(id);
      if (r.status !== "success") throw new Error(`session transaction failed: ${JSON.stringify(r.status)}`);
    },
    async relayer() {
      relayer ??= fromHex((await client.relayInfo()).relayer);
      return relayer;
    },
  };
}

/** A hosting-node relay (`POST /tx`) in front of a cluster; the caller supplies chain facts. */
export function relayTransport(client: BtbClient, info: (account: Uint8Array) => Promise<ChainInfo>): AuthTransport {
  return {
    info,
    async sendSession(wire) {
      await client.submitSigned(wire);
    },
  };
}

export interface Identity {
  /** Hex account address. `caller()` in contracts. */
  account: string;
  /** Hex public key of the session key that signs. */
  sessionKey: string;
  expiresHeight: bigint;
}

export interface BtbAuthOptions {
  wallet: Wallet;
  transport: AuthTransport;
  /** The contract the app calls (the default scope). */
  contract: Uint8Array;
  /** Mutations the session may call; default: all of `contract`. */
  functions?: string[];
  /** Grant lifetime in blocks. Default 10_000. */
  ttlBlocks?: bigint;
  /** Fee budget in base units. Default 1_000_000_000 (1 BTB). */
  spendLimit?: bigint;
  /** Gas limit of each call. Default 1_000_000. */
  gasLimit?: bigint;
  store?: KeyStore;
  /** Key under which the session is stored. Default "default". */
  name?: string;
  /** Name this relayer (hex) in every transaction so a hosting node earns its share. Default: the transport's. */
  relayer?: string;
}

/** Session-key auth for one app. */
export class BtbAuth {
  readonly contract: Uint8Array;
  private readonly o: BtbAuthOptions;
  private readonly store: KeyStore;
  private readonly name: string;
  private session: StoredSession | undefined;
  private nextNonce: bigint | undefined;
  private readonly listeners = new Set<() => void>();
  private ident: Identity | null = null;

  constructor(o: BtbAuthOptions) {
    this.o = o;
    this.contract = o.contract;
    this.name = o.name ?? "default";
    this.store = o.store ?? (typeof indexedDB !== "undefined" ? new IndexedDbKeyStore() : new MemoryKeyStore());
  }

  private async relayer(): Promise<Uint8Array | undefined> {
    return this.o.relayer ? fromHex(this.o.relayer) : this.o.transport.relayer?.();
  }

  /** Who is logged in, or null. */
  get identity(): Identity | null {
    return this.ident;
  }

  subscribe(f: () => void): () => void {
    this.listeners.add(f);
    return () => this.listeners.delete(f);
  }

  private set(s: StoredSession | undefined) {
    this.session = s;
    this.nextNonce = undefined;
    this.ident = s
      ? { account: toHex(s.account), sessionKey: toHex(s.publicKey), expiresHeight: s.expiresHeight }
      : null;
    this.listeners.forEach((f) => f());
  }

  /** Load a stored session (call on page load). Resolves with the identity, or null. */
  async restore(): Promise<Identity | null> {
    const s = await this.store.get(this.name);
    if (s) {
      const { height } = await this.o.transport.info(s.account);
      if (height >= s.expiresHeight) {
        await this.store.delete(this.name);
        this.set(undefined);
        return null;
      }
    }
    this.set(s);
    return this.ident;
  }

  /** Create a session key and have the wallet sign its grant once. */
  async login(): Promise<Identity> {
    const account = await this.o.wallet.address();
    const info = await this.o.transport.info(account);
    // Non-extractable private key; the public key is always exportable.
    const pair = (await crypto.subtle.generateKey("Ed25519", false, ["sign", "verify"])) as CryptoKeyPair;
    const publicKey = new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey));
    const expiresHeight = info.height + (this.o.ttlBlocks ?? 10_000n);
    const scope: SessionScope = { contract: this.contract, functions: this.o.functions };
    const grant: SessionGrant = {
      sessionKey: publicKey,
      expiresHeight,
      allowed: [scope],
      spendLimit: this.o.spendLimit ?? 1_000_000_000n,
    };
    const wire = await this.o.wallet.signTransaction({
      chainId: info.chainId,
      nonce: info.nonce,
      relayer: await this.relayer(),
      kind: grantKind(grant),
    });
    await this.o.transport.sendSession(wire);
    const stored = { privateKey: pair.privateKey, publicKey, account, expiresHeight };
    await this.store.set(this.name, stored);
    this.set(stored);
    return this.ident!;
  }

  /** Forget the session key. With `revoke`, the wallet also signs a `Revoke` so the grant dies on chain. */
  async logout(opts: { revoke?: boolean } = {}): Promise<void> {
    const s = this.session;
    await this.store.delete(this.name);
    this.set(undefined);
    if (s && opts.revoke) {
      const info = await this.o.transport.info(s.account);
      const wire = await this.o.wallet.signTransaction({
        chainId: info.chainId,
        nonce: info.nonce,
        relayer: await this.relayer(),
        kind: revokeKind(s.publicKey),
      });
      await this.o.transport.sendSession(wire);
    }
  }

  /**
   * Send a mutation through a relay host (`client.submitSigned`) as the logged-in account and
   * wait for its receipt. For a chain, where mutations are transactions rather than `POST
   * /mutation`.
   */
  async send(client: BtbClient, fn: string, args: Uint8Array): Promise<Record<string, unknown>> {
    const wire = await this.signCall(fn, args);
    try {
      const { id } = await client.submitSigned(wire);
      return await client.waitForReceipt(id);
    } catch (e) {
      this.resetNonce();
      throw e;
    }
  }

  /** Forget the locally counted nonce (call after a signed transaction was refused). */
  resetNonce(): void {
    this.nextNonce = undefined;
  }

  /** Sign a mutation call with the session key; the nonce is the account's. */
  async signCall(fn: string, args: Uint8Array, kind?: (k: TxKind) => TxKind): Promise<Uint8Array> {
    const s = this.session;
    if (!s) throw new Error("not logged in");
    const info = await this.o.transport.info(s.account);
    // Our own counter covers calls still in flight; never go below the chain's.
    const nonce = this.nextNonce !== undefined && this.nextNonce > info.nonce ? this.nextNonce : info.nonce;
    const k = callKind(this.contract, fn, args, this.o.gasLimit ?? 1_000_000n);
    const wire = await signTransactionWith(
      {
        chainId: info.chainId,
        nonce,
        relayer: await this.relayer(),
        session: s.account,
        kind: kind ? kind(k) : k,
      },
      s.publicKey,
      s.privateKey,
    );
    this.nextNonce = nonce + 1n;
    return wire;
  }
}
