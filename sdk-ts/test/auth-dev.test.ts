import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { BtbAuth, BtbClient, BtbError, LocalKeyWallet, MemoryKeyStore, devTransport, t, toHex } from "../src/index.js";

// `btb-dev --auth signed`: the session-key flow end to end (docs/DEV_SERVER.md).
const root = resolve(import.meta.dirname, "../..");
let proc: ChildProcess;
let url: string;
const DEV_CONTRACT = new Uint8Array(32).fill(0xde);
const Todos = t.vec(t.tuple(t.u64, t.bool, t.string));

beforeAll(async () => {
  const run = (cmd: string, args: string[]) => {
    const r = spawnSync(cmd, args, { cwd: root, stdio: "inherit" });
    if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} failed`);
  };
  run("sh", ["examples/todo/build.sh"]);
  run("cargo", ["build", "-q", "-p", "btb-dev"]);
  const bin = resolve(process.env.CARGO_TARGET_DIR ?? resolve(root, "target"), "debug/btb-dev");
  expect(existsSync(bin)).toBe(true);
  proc = spawn(bin, [resolve(root, "examples/todo/todo.elf"), "--port", "0", "--auth", "signed"], {
    stdio: ["ignore", "pipe", "inherit"],
  });
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

afterAll(() => proc?.kill());

describe("session keys against btb-dev --auth signed", () => {
  it("login once, then act as the account only inside the grant", async () => {
    const wallet = LocalKeyWallet.random();
    const auth = new BtbAuth({
      wallet,
      transport: devTransport(url),
      contract: DEV_CONTRACT,
      functions: ["add"],
      store: new MemoryKeyStore(),
    });
    const client = new BtbClient({ url, auth });

    // Unsigned mutations are refused in signed mode.
    const anon = new BtbClient({ url, caller: "alice" });
    await expect(anon.mutation("add", t.tuple(t.string), ["x"], t.u64)).rejects.toMatchObject({ status: 401 });

    const id = await auth.login();
    expect(id.account).toBe(toHex(await wallet.address()));
    expect(await client.mutation("add", t.tuple(t.string), ["buy milk"], t.u64)).toBe(0n);
    // The contract saw the account as caller(), not the session key.
    expect(await client.query("list_mine", t.unit, [], Todos)).toEqual([[0n, false, "buy milk"]]);

    // `toggle` is outside the grant.
    const err = await client.mutation("toggle", t.tuple(t.u64), [0n], t.bool).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(BtbError);
    expect((err as BtbError).status).toBe(401);
    expect((err as BtbError).message).toContain("FunctionNotAllowed");

    // A replayed transaction is refused: nonces are the account's.
    const wire = await auth.signCall("add", Uint8Array.of(1, 0, 0, 0, 97));
    const post = () => fetch(`${url}/mutation/add?args=${toHex(Uint8Array.of(1, 0, 0, 0, 97))}`, { method: "POST", headers: { "x-btb-tx": toHex(wire) } });
    expect((await post()).status).toBe(200);
    expect((await post()).status).toBe(401);

    // Revoke on logout: further calls are refused.
    await auth.logout({ revoke: true });
    await expect(client.mutation("add", t.tuple(t.string), ["y"], t.u64)).rejects.toMatchObject({ status: 401 });
  });

  it("a session of one account cannot act as another", async () => {
    const mk = () =>
      new BtbAuth({ wallet: LocalKeyWallet.random(), transport: devTransport(url), contract: DEV_CONTRACT, store: new MemoryKeyStore() });
    const [a, b] = [mk(), mk()];
    await a.login();
    await b.login();
    const ca = new BtbClient({ url, auth: a });
    const cb = new BtbClient({ url, auth: b });
    await ca.mutation("add", t.tuple(t.string), ["a's"], t.u64);
    expect(await cb.query("list_mine", t.unit, [], Todos)).toEqual([]);
  });
});
