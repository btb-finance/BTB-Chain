import { type ArgsParam, type MutationRef, type QueryRef, isFnRef } from "./api.js";
import { type Codec, decode, encode, fromHex, toHex } from "./codec.js";
import type { ProofBundle } from "./proof.js";
import type { BtbAuth } from "./auth.js";
import { type TxKind, signTransaction } from "./tx.js";

export class BtbError extends Error {
  constructor(
    message: string,
    /** HTTP status, or 0 for network failures. */
    readonly status: number = 0,
  ) {
    super(message);
    this.name = "BtbError";
  }
}

/**
 * A contract function aborted the call (`abort("msg")`, or an `Err` returned by a function
 * declared `-> btb_sdk::Result<T>`). `reason` is the contract's message.
 */
export class ContractError extends BtbError {
  constructor(
    /** The message the contract aborted with. */
    readonly reason: string,
    status: number = 400,
  ) {
    super(`aborted: ${reason}`, status);
    this.name = "ContractError";
  }
}

/** Turn a failed HTTP response into a `ContractError` for aborts, else a `BtbError`. */
function httpError(text: string, status: number): BtbError {
  const message = errorMessage(text, status);
  const m = /^aborted: ([\s\S]*)$/.exec(message);
  return status === 400 && m ? new ContractError(m[1]!, status) : new BtbError(message, status);
}

export interface BtbClientOptions {
  /** Base URL of the node, e.g. `http://127.0.0.1:8787`. */
  url: string;
  /** Dev caller identity (sent as `?caller=`; 64 hex chars = address, other text is hashed). */
  caller?: string;
  /** Custom fetch (tests, polyfills). Defaults to `globalThis.fetch`. */
  fetch?: typeof fetch;
  /** Custom EventSource constructor. Defaults to `globalThis.EventSource` if present, else a fetch stream. */
  EventSource?: typeof EventSource | null;
  /** Delay before the fetch-stream fallback reconnects, in ms. Default 1000. */
  reconnectMs?: number;
  /**
   * Hosting (or edge) node that relays signed transactions (`POST /tx`) and earns the relay
   * share of their fees. Default: the node `url` points at, without a `/c/<contract>` suffix.
   */
  relayUrl?: string;
  /** Hex address of that node's relayer account. Default: asked from `GET /relay` on first use. */
  relayer?: string;
  /**
   * Session-key auth (`BtbAuth`). When someone is logged in, queries use the account as caller
   * and mutations are sent as session-signed transactions (`X-BTB-Tx`, for `btb-dev --auth
   * signed`).
   */
  auth?: BtbAuth;
}

/** A transaction submitted through a relay. */
export interface Submitted {
  /** Transaction id, hex. */
  id: string;
  /** Hex address of the relayer that was signed into the transaction. */
  relayer: string;
}

export type Unsubscribe = () => void;

export class BtbClient {
  readonly url: string;
  private readonly callerOpt: string | undefined;
  readonly auth: BtbAuth | undefined;
  private readonly fetchFn: typeof fetch;
  private readonly ES: typeof EventSource | null;
  private readonly reconnectMs: number;
  private readonly relayBase: string;
  private relayerHex: string | undefined;

  constructor(opts: BtbClientOptions) {
    this.url = opts.url.replace(/\/+$/, "");
    this.callerOpt = opts.caller;
    this.auth = opts.auth;
    this.fetchFn = opts.fetch ?? ((...a) => globalThis.fetch(...a));
    this.ES = opts.EventSource === undefined ? (globalThis.EventSource ?? null) : opts.EventSource;
    this.reconnectMs = opts.reconnectMs ?? 1000;
    this.relayBase = (opts.relayUrl ?? this.url.replace(/\/c\/[0-9a-fA-F]{64}$/, "")).replace(/\/+$/, "");
    this.relayerHex = opts.relayer?.toLowerCase();
  }

  /**
   * Where the file endpoints live (`/upload`, `/file/<root>`): the node `url` without a
   * `/c/<contract>` suffix.
   */
  get filesBase(): string {
    return this.url.replace(/\/c\/[^/]+$/, "");
  }

  /** `fetch` through the client's own fetch (used by the file helpers). */
  fetchRaw(input: string, init?: RequestInit): Promise<Response> {
    return this.fetchFn(input, init);
  }

  /** The caller identity sent with queries: the logged-in account, else the `caller` option. */
  get caller(): string | undefined {
    return this.auth?.identity?.account ?? this.callerOpt;
  }

  private async relayRequest<T>(path: string, init: RequestInit): Promise<T> {
    let res: Response;
    try {
      res = await this.fetchFn(`${this.relayBase}${path}`, init);
    } catch (e) {
      throw new BtbError(`network error: ${(e as Error).message}`);
    }
    const text = await res.text();
    if (!res.ok) throw new BtbError(errorMessage(text, res.status), res.status);
    return JSON.parse(text) as T;
  }

  /** The relayer address (hex) and chain id of the relay host (`GET /relay`). */
  async relayInfo(): Promise<{ relayer: string; chainId: bigint }> {
    const j = await this.relayRequest<{ relayer: string; chain_id: number | null }>("/relay", { method: "GET" });
    return { relayer: j.relayer, chainId: BigInt(j.chain_id ?? -1) };
  }

  /**
   * Sign a transaction that names the relay host as its relayer and submit it through the host
   * (`POST /tx`), which forwards it to validators. The relayer is part of the signed body, so the
   * host cannot change it. `seed` is the account's 32-byte Ed25519 seed.
   */
  async submit(args: { seed: Uint8Array; chainId: bigint; nonce: bigint; kind: TxKind }): Promise<Submitted> {
    if (this.relayerHex === undefined) this.relayerHex = (await this.relayInfo()).relayer.toLowerCase();
    const wire = await signTransaction(
      { chainId: args.chainId, nonce: args.nonce, relayer: fromHex(this.relayerHex), kind: args.kind },
      args.seed,
    );
    return this.submitSigned(wire);
  }

  /** Submit an already signed transaction. The host accepts it only if it names the host as relayer. */
  submitSigned(wire: Uint8Array): Promise<Submitted> {
    return this.relayRequest<Submitted>("/tx", {
      method: "POST",
      headers: { "content-type": "text/plain" },
      body: toHex(wire),
    });
  }

  /** The receipt of a transaction, or `null` while the host has not executed its block. */
  receipt(id: string): Promise<Record<string, unknown> | null> {
    return this.relayRequest(`/tx/${id}`, { method: "GET" });
  }

  /** Poll `receipt(id)` until the host has executed the transaction; rejects on timeout. */
  async waitForReceipt(id: string, timeoutMs = 60_000, pollMs = 200): Promise<Record<string, unknown>> {
    const end = Date.now() + timeoutMs;
    for (;;) {
      const r = await this.receipt(id);
      if (r) return r;
      if (Date.now() > end) throw new BtbError(`no receipt for ${id} after ${timeoutMs} ms`);
      await new Promise((res) => setTimeout(res, pollMs));
    }
  }

  private target(kind: "query" | "mutation" | "subscribe", name: string, args: Uint8Array): string {
    const q = new URLSearchParams();
    if (args.length > 0) q.set("args", toHex(args));
    if (this.caller !== undefined) q.set("caller", this.caller);
    const qs = q.toString();
    return `${this.url}/${kind}/${encodeURIComponent(name)}${qs ? `?${qs}` : ""}`;
  }

  private async call<A extends unknown[], R>(
    kind: "query" | "mutation",
    name: string,
    argsType: Codec<A>,
    args: A,
    resultType: Codec<R>,
  ): Promise<R> {
    let res: Response;
    try {
      const bytes = encode(argsType, args);
      const headers: Record<string, string> = {};
      if (kind === "mutation" && this.auth?.identity) headers["x-btb-tx"] = toHex(await this.auth.signCall(name, bytes));
      res = await this.fetchFn(this.target(kind, name, bytes), {
        method: kind === "query" ? "GET" : "POST",
        ...(Object.keys(headers).length > 0 ? { headers } : {}),
      });
    } catch (e) {
      throw new BtbError(`network error: ${(e as Error).message}`);
    }
    const text = await res.text();
    if (!res.ok) {
      // A refused signed transaction did not use its nonce: count from the chain again.
      if (kind === "mutation") this.auth?.resetNonce();
      throw httpError(text, res.status);
    }
    return decode(resultType, fromHex(text.trim()));
  }

  /** Run a query once. */
  query<A extends unknown[], R>(ref: QueryRef<A, R>, ...args: ArgsParam<A>): Promise<R>;
  query<A extends unknown[], R>(name: string, argsType: Codec<A>, args: A, resultType: Codec<R>): Promise<R>;
  query(first: unknown, ...rest: unknown[]): Promise<unknown> {
    if (isFnRef(first)) return this.call("query", first.name, first.args, (rest[0] ?? []) as unknown[], first.result);
    const [argsType, args, resultType] = rest as [Codec<unknown[]>, unknown[], Codec<unknown>];
    return this.call("query", first as string, argsType, args, resultType);
  }

  /** Run a mutation; resolves with its result once committed. */
  mutation<A extends unknown[], R>(ref: MutationRef<A, R>, ...args: ArgsParam<A>): Promise<R>;
  mutation<A extends unknown[], R>(name: string, argsType: Codec<A>, args: A, resultType: Codec<R>): Promise<R>;
  mutation(first: unknown, ...rest: unknown[]): Promise<unknown> {
    if (isFnRef(first)) return this.call("mutation", first.name, first.args, (rest[0] ?? []) as unknown[], first.result);
    const [argsType, args, resultType] = rest as [Codec<unknown[]>, unknown[], Codec<unknown>];
    return this.call("mutation", first as string, argsType, args, resultType);
  }

  /**
   * Run a query on a hosting node and get the result together with Merkle proofs of the storage
   * keys it read (`?proof=1`). Check them with `verifyProof`. `incomplete` is true when the query
   * also scanned key ranges, which a proof cannot cover (docs/HOSTING.md).
   */
  async queryWithProof<A extends unknown[], R>(
    name: string,
    argsType: Codec<A>,
    args: A,
    resultType: Codec<R>,
  ): Promise<{ result: R; height: number; incomplete: boolean; proof: ProofBundle }> {
    const base = this.target("query", name, encode(argsType, args));
    const url = `${base}${base.includes("?") ? "&" : "?"}proof=1`;
    let res: Response;
    try {
      res = await this.fetchFn(url, { method: "GET" });
    } catch (e) {
      throw new BtbError(`network error: ${(e as Error).message}`);
    }
    const text = await res.text();
    if (!res.ok) throw new BtbError(errorMessage(text, res.status), res.status);
    const j = JSON.parse(text) as { result: string; height: number; incomplete: boolean; proof: ProofBundle };
    return { result: decode(resultType, fromHex(j.result)), height: j.height, incomplete: j.incomplete, proof: j.proof };
  }

  /**
   * Fetch a proof bundle for storage keys of the contract this client points at (a hosting node
   * URL ending in `/c/<contract>`). `height` picks the state version; by default the newest one
   * whose next block is provably committed.
   */
  async proof(keys: Uint8Array[], height?: number): Promise<ProofBundle> {
    const m = /^(.*)\/c\/([0-9a-fA-F]{64})$/.exec(this.url);
    if (!m) throw new BtbError("proof() needs a hosting node url ending in /c/<contract>");
    const q = new URLSearchParams({ key: keys.map(toHex).join(",") });
    if (height !== undefined) q.set("height", String(height));
    let res: Response;
    try {
      res = await this.fetchFn(`${m[1]}/proof/${m[2]}?${q}`, { method: "GET" });
    } catch (e) {
      throw new BtbError(`network error: ${(e as Error).message}`);
    }
    const text = await res.text();
    if (!res.ok) throw new BtbError(errorMessage(text, res.status), res.status);
    return JSON.parse(text) as ProofBundle;
  }

  /**
   * Watch a query: `onResult` gets the current result, then a new one each time it changes.
   * Returns a function that stops the subscription.
   */
  subscribe<A extends unknown[], R>(
    ref: QueryRef<A, R>,
    args: A,
    onResult: (result: R) => void,
    onError?: (error: BtbError) => void,
  ): Unsubscribe;
  subscribe<A extends unknown[], R>(
    name: string,
    argsType: Codec<A>,
    args: A,
    resultType: Codec<R>,
    onResult: (result: R) => void,
    onError?: (error: BtbError) => void,
  ): Unsubscribe;
  subscribe(first: unknown, ...rest: unknown[]): Unsubscribe {
    let name: string;
    let argsType: Codec<unknown[]>;
    let args: unknown[];
    let resultType: Codec<unknown>;
    let onResult: (result: unknown) => void;
    let onError: ((error: BtbError) => void) | undefined;
    if (isFnRef(first)) {
      name = first.name;
      argsType = first.args;
      resultType = first.result;
      [args, onResult, onError] = rest as [unknown[], (r: unknown) => void, ((e: BtbError) => void)?];
    } else {
      name = first as string;
      [argsType, args, resultType, onResult, onError] = rest as [
        Codec<unknown[]>,
        unknown[],
        Codec<unknown>,
        (r: unknown) => void,
        ((e: BtbError) => void)?,
      ];
    }
    type R = unknown;
    const url = this.target("subscribe", name, encode(argsType, args));
    let closed = false;
    const deliver = (data: string) => {
      if (closed) return;
      let value: R;
      try {
        value = decode(resultType, fromHex(data.trim()));
      } catch (e) {
        onError?.(new BtbError(`bad result: ${(e as Error).message}`));
        return;
      }
      onResult(value);
    };
    const fail = (e: BtbError) => {
      if (!closed) onError?.(e);
    };

    if (this.ES) {
      const es = new this.ES(url);
      es.onmessage = (ev) => deliver(String(ev.data));
      // EventSource reconnects by itself on transient errors; readyState CLOSED means it gave up.
      es.onerror = () => {
        fail(new BtbError("subscription connection error"));
        if (es.readyState === 2) closed = true;
      };
      return () => {
        closed = true;
        es.close();
      };
    }

    const abort = new AbortController();
    const loop = async () => {
      while (!closed) {
        try {
          const res = await this.fetchFn(url, {
            headers: { accept: "text/event-stream" },
            signal: abort.signal,
          });
          if (!res.ok) {
            fail(httpError(await res.text(), res.status));
            if (res.status < 500) return; // a bad request will not get better
          } else if (res.body) {
            await readSse(res.body, deliver);
          }
        } catch (e) {
          if (closed) return;
          fail(new BtbError(`subscription connection error: ${(e as Error).message}`));
        }
        if (closed) return;
        await new Promise((r) => setTimeout(r, this.reconnectMs));
      }
    };
    void loop();
    return () => {
      closed = true;
      abort.abort();
    };
  }
}

function errorMessage(text: string, status: number): string {
  try {
    const j = JSON.parse(text) as { error?: unknown };
    if (typeof j.error === "string") return j.error;
  } catch {
    /* not JSON */
  }
  return text.trim() || `HTTP ${status}`;
}

/** Parse a Server-Sent Events body, calling `onData` once per event. */
async function readSse(body: ReadableStream<Uint8Array>, onData: (data: string) => void): Promise<void> {
  const reader = body.getReader();
  const dec = new TextDecoder();
  let buf = "";
  let data: string[] = [];
  for (;;) {
    const { done, value } = await reader.read();
    if (done) return;
    buf += dec.decode(value, { stream: true });
    let nl: number;
    while ((nl = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, nl).replace(/\r$/, "");
      buf = buf.slice(nl + 1);
      if (line === "") {
        if (data.length) onData(data.join("\n"));
        data = [];
      } else if (line.startsWith("data:")) {
        data.push(line.slice(5).replace(/^ /, ""));
      }
    }
  }
}
