import { type Codec, decode, encode, fromHex, toHex } from "./codec.js";
import type { ProofBundle } from "./proof.js";

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
}

export type Unsubscribe = () => void;

export class BtbClient {
  readonly url: string;
  readonly caller: string | undefined;
  private readonly fetchFn: typeof fetch;
  private readonly ES: typeof EventSource | null;
  private readonly reconnectMs: number;

  constructor(opts: BtbClientOptions) {
    this.url = opts.url.replace(/\/+$/, "");
    this.caller = opts.caller;
    this.fetchFn = opts.fetch ?? ((...a) => globalThis.fetch(...a));
    this.ES = opts.EventSource === undefined ? (globalThis.EventSource ?? null) : opts.EventSource;
    this.reconnectMs = opts.reconnectMs ?? 1000;
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
      res = await this.fetchFn(this.target(kind, name, encode(argsType, args)), {
        method: kind === "query" ? "GET" : "POST",
      });
    } catch (e) {
      throw new BtbError(`network error: ${(e as Error).message}`);
    }
    const text = await res.text();
    if (!res.ok) throw new BtbError(errorMessage(text, res.status), res.status);
    return decode(resultType, fromHex(text.trim()));
  }

  /** Run a query once. */
  query<A extends unknown[], R>(name: string, argsType: Codec<A>, args: A, resultType: Codec<R>): Promise<R> {
    return this.call("query", name, argsType, args, resultType);
  }

  /** Run a mutation; resolves with its result once committed. */
  mutation<A extends unknown[], R>(name: string, argsType: Codec<A>, args: A, resultType: Codec<R>): Promise<R> {
    return this.call("mutation", name, argsType, args, resultType);
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
    name: string,
    argsType: Codec<A>,
    args: A,
    resultType: Codec<R>,
    onResult: (result: R) => void,
    onError?: (error: BtbError) => void,
  ): Unsubscribe {
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
            fail(new BtbError(errorMessage(await res.text(), res.status), res.status));
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
