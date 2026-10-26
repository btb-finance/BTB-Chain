import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { BtbClient, BtbError, fileUrl, finishUpload, t, toHex, uploadFile } from "../src/index.js";

/** A client whose fetch records the request and answers with `reply`. */
function fake(url: string, reply: (url: URL, init?: RequestInit) => Response) {
  const calls: { url: URL; method: string; body: unknown }[] = [];
  const client = new BtbClient({
    url,
    caller: "alice",
    fetch: (async (u: string, init?: RequestInit) => {
      const parsed = new URL(u);
      calls.push({ url: parsed, method: init?.method ?? "GET", body: init?.body });
      return reply(parsed, init);
    }) as typeof fetch,
  });
  return { client, calls };
}

const ROOT = "ab".repeat(32);

describe("file helpers", () => {
  it("fileUrl strips the /c/<contract> suffix and checks the root", () => {
    const { client } = fake("http://node:8787/c/" + "cd".repeat(32), () => new Response(""));
    expect(fileUrl(client, ROOT)).toBe(`http://node:8787/file/${ROOT}`);
    expect(fileUrl("http://x/c/router/", new Uint8Array(32).fill(0xab))).toBe(`http://x/file/${ROOT}`);
    expect(fileUrl("http://x", ROOT.toUpperCase())).toBe(`http://x/file/${ROOT}`);
    expect(() => fileUrl("http://x", "abc")).toThrow(BtbError);
  });

  it("uploadFile posts the bytes and returns the root as bytes and hex", async () => {
    const { client, calls } = fake("http://x/c/token", () =>
      new Response(JSON.stringify({ root: ROOT, size: 3, k: 4, m: 2, stored: true, url: `/file/${ROOT}` })),
    );
    const r = await uploadFile(client, new Uint8Array([1, 2, 3]), { periods: 5, store: false });
    expect(calls[0]!.url.pathname).toBe("/upload");
    expect(calls[0]!.url.searchParams.get("periods")).toBe("5");
    expect(calls[0]!.url.searchParams.get("store")).toBe("0");
    expect(calls[0]!.method).toBe("POST");
    expect(toHex(r.root)).toBe(ROOT);
    expect(r).toMatchObject({ rootHex: ROOT, size: 3, k: 4, m: 2, stored: true });
  });

  it("surfaces server errors and finishUpload's 409", async () => {
    const { client } = fake("http://x", (url) =>
      url.pathname === "/upload"
        ? new Response(JSON.stringify({ error: "cannot store an empty file" }), { status: 400 })
        : new Response(JSON.stringify({ error: "store it first" }), { status: 409 }),
    );
    const e = await uploadFile(client, new Uint8Array()).catch((x) => x);
    expect(e).toBeInstanceOf(BtbError);
    expect(e.message).toBe("cannot store an empty file");
    const e2 = await finishUpload(client, ROOT).catch((x) => x);
    expect(e2.status).toBe(409);
  });
});

// ---- against a real btb-dev running examples/files ----------------------------------------

const root = resolve(import.meta.dirname, "../..");
let proc: ChildProcess;
let url: string;

beforeAll(async () => {
  const run = (cmd: string, args: string[]) => {
    const r = spawnSync(cmd, args, { cwd: root, stdio: "inherit" });
    if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} failed`);
  };
  run("sh", ["examples/files/build.sh"]);
  run("cargo", ["build", "-q", "-p", "btb-dev"]);
  const bin = resolve(process.env.CARGO_TARGET_DIR ?? resolve(root, "target"), "debug/btb-dev");
  expect(existsSync(bin)).toBe(true);
  proc = spawn(bin, [resolve(root, "examples/files/files.elf"), "--port", "0"], { stdio: ["ignore", "pipe", "inherit"] });
  url = await new Promise<string>((res, rej) => {
    let buf = "";
    proc.stdout!.on("data", (d) => {
      buf += d;
      const m = /listening on (http:\/\/\S+)/.exec(buf);
      if (m) res(m[1]!);
    });
    proc.on("exit", () => rej(new Error("btb-dev exited early")));
  });
}, 600_000);

afterAll(() => {
  proc?.kill();
});

describe("files against btb-dev", () => {
  it("upload, attach the root in a mutation, query file_info and fetch by URL", async () => {
    const client = new BtbClient({ url, caller: "alice" });
    const data = new Uint8Array(30_000).map((_, i) => (i * 31) % 251);
    const up = await uploadFile(client, data);
    expect(up.stored).toBe(true);
    expect(up.size).toBe(30_000);
    expect(up.root).toHaveLength(32);

    const info = await client.query("info", t.tuple(t.address), [up.root], t.tuple(t.u64, t.u8, t.u64));
    expect(info[0]).toBe(30_000n);
    expect(info[1]).toBe(1);
    const size = await client.mutation("attach", t.tuple(t.string, t.address), ["photo", up.root], t.u64);
    expect(size).toBe(30_000n);
    expect(await client.query("is_available", t.tuple(t.string), ["photo"], t.bool)).toBe(true);

    const res = await fetch(fileUrl(client, up.root));
    expect(res.status).toBe(200);
    expect(new Uint8Array(await res.arrayBuffer())).toEqual(data);
    expect(res.headers.get("etag")).toBe(`"${up.rootHex}"`);
    expect(await finishUpload(client, up.root)).toMatchObject({ pushed: 0 });
  });

  it("an unpaid upload and an unknown root are refused by the contract", async () => {
    const client = new BtbClient({ url, caller: "alice" });
    const up = await uploadFile(client, new Uint8Array(2_000).fill(7), { store: false });
    expect(up.stored).toBe(false);
    const info = await client.query("info", t.tuple(t.address), [up.root], t.tuple(t.u64, t.u8, t.u64));
    expect(info[1]).toBe(0);
    const e = await client.mutation("attach", t.tuple(t.string, t.address), ["x", up.root], t.u64).catch((x) => x);
    expect(e.reason).toBe("file is not stored");
    const none = await client.query("info", t.tuple(t.address), [new Uint8Array(32).fill(9)], t.tuple(t.u64, t.u8, t.u64));
    expect(none).toEqual([0n, 0, 0n]);
  });
});
