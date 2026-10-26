import { BtbError, type BtbClient } from "./client.js";
import { fromHex, toHex } from "./codec.js";

/** What an upload returns. */
export interface UploadResult {
  /** The file id (32 bytes): pass it to a mutation declared with a `t.address` parameter. */
  root: Uint8Array;
  /** Hex of `root`; also what `fileUrl` takes. */
  rootHex: string;
  /** Bytes. */
  size: number;
  /** Erasure-coding shape; a contract that pays for the file itself (`files::pin`) needs `size`, `k`, `m`. */
  k: number;
  m: number;
  /**
   * Is the file stored and paid on the server? Always true on `btb dev` (unless `store: false`).
   * On a hosting node it is false until a `StoreFile` transaction or a contract's pin is on chain;
   * then call `finishUpload`.
   */
  stored: boolean;
  /** Path of the file on the node, `/file/<root>`. */
  url: string;
}

export interface UploadOptions {
  /** Data pieces (default 4). */
  k?: number;
  /** Parity pieces (default 2). */
  m?: number;
  /** `btb dev` only: payment periods of the free `StoreFile` it records (default 1000). */
  periods?: number;
  /** `btb dev` only: `false` uploads without recording the file as stored. */
  store?: boolean;
}

type FileBody = Uint8Array | ArrayBuffer | Blob;

/**
 * Upload a file to `btb dev` or to a hosting node with an upload gateway, and get its root. The
 * bytes go off chain; only the root is passed to a mutation:
 *
 * ```ts
 * const f = await uploadFile(client, file);
 * await client.mutation(api.attach, "photo", f.root);   // the contract checks files::require_stored
 * <img src={fileUrl(client, f.root)} />
 * ```
 *
 * On a real chain the file must also be stored and paid before the contract accepts it: send
 * `StoreFile` (`btb store`) or let the contract pay with `files::pin`, then `finishUpload`.
 */
export async function uploadFile(client: BtbClient, data: FileBody, opts: UploadOptions = {}): Promise<UploadResult> {
  const q = new URLSearchParams();
  if (opts.k !== undefined) q.set("k", String(opts.k));
  if (opts.m !== undefined) q.set("m", String(opts.m));
  if (opts.periods !== undefined) q.set("periods", String(opts.periods));
  if (opts.store === false) q.set("store", "0");
  if (client.caller !== undefined) q.set("caller", client.caller);
  const qs = q.toString();
  let res: Response;
  try {
    res = await client.fetchRaw(`${client.filesBase}/upload${qs ? `?${qs}` : ""}`, {
      method: "POST",
      headers: { "content-type": "application/octet-stream" },
      body: data as BodyInit,
    });
  } catch (e) {
    throw new BtbError(`network error: ${(e as Error).message}`);
  }
  const text = await res.text();
  if (!res.ok) throw new BtbError(errorText(text, res.status), res.status);
  const j = JSON.parse(text) as { root: string; size: number; k: number; m: number; stored: boolean; url: string };
  return { root: fromHex(j.root), rootHex: j.root, size: j.size, k: j.k, m: j.m, stored: j.stored, url: j.url };
}

/**
 * After the file is stored on chain, have the node send its pieces to the storage nodes the
 * chain assigned. Resolves with how many pieces went out and which failed (a no-op on `btb dev`).
 * Rejects with status 409 while the chain does not know the file yet.
 */
export async function finishUpload(
  client: BtbClient,
  root: Uint8Array | string,
): Promise<{ pushed: number; failed: { index: number; error: string }[] }> {
  const hex = typeof root === "string" ? root : toHex(root);
  let res: Response;
  try {
    res = await client.fetchRaw(`${client.filesBase}/upload/${hex}/push`, { method: "POST" });
  } catch (e) {
    throw new BtbError(`network error: ${(e as Error).message}`);
  }
  const text = await res.text();
  if (!res.ok) throw new BtbError(errorText(text, res.status), res.status);
  return JSON.parse(text);
}

/**
 * URL of a stored file, for `<img src>`, downloads and `fetch`. The node verifies the pieces
 * against the root before it answers, and the response is immutable (cache forever).
 */
export function fileUrl(client: BtbClient | string, root: Uint8Array | string): string {
  const base = typeof client === "string" ? client.replace(/\/+$/, "").replace(/\/c\/[^/]+$/, "") : client.filesBase;
  const hex = typeof root === "string" ? root : toHex(root);
  if (!/^[0-9a-fA-F]{64}$/.test(hex)) throw new BtbError("a file root is 32 bytes (64 hex characters)");
  return `${base}/file/${hex.toLowerCase()}`;
}

function errorText(text: string, status: number): string {
  try {
    const j = JSON.parse(text) as { error?: string };
    if (j.error) return j.error;
  } catch {
    /* not JSON */
  }
  return text || `HTTP ${status}`;
}
