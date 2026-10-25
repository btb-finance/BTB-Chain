import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { BtbClient, BtbError, t } from "../src/index.js";

const root = resolve(import.meta.dirname, "../..");
let proc: ChildProcess;
let url: string;

const Todos = t.vec(t.tuple(t.u64, t.bool, t.string));

beforeAll(async () => {
  const run = (cmd: string, args: string[], cwd: string) => {
    const r = spawnSync(cmd, args, { cwd, stdio: "inherit" });
    if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} failed`);
  };
  run("sh", ["examples/todo/build.sh"], root);
  run("cargo", ["build", "-q", "-p", "btb-dev"], root);
  const bin = resolve(process.env.CARGO_TARGET_DIR ?? resolve(root, "target"), "debug/btb-dev");
  expect(existsSync(bin)).toBe(true);
  proc = spawn(bin, [resolve(root, "examples/todo/todo.elf"), "--port", "0"], { stdio: ["ignore", "pipe", "inherit"] });
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

function nextResult<T>(wait = 10_000) {
  let resolveFn!: (v: T) => void;
  const queue: T[] = [];
  const waiters: ((v: T) => void)[] = [];
  resolveFn = (v) => (waiters.shift() ?? ((x: T) => queue.push(x)))(v);
  const next = () =>
    new Promise<T>((res, rej) => {
      const q = queue.shift();
      if (q !== undefined) return res(q);
      const timer = setTimeout(() => rej(new Error("timeout waiting for event")), wait);
      waiters.push((v) => {
        clearTimeout(timer);
        res(v);
      });
    });
  return { push: resolveFn, next };
}

describe("BtbClient against btb-dev", () => {
  it("adds a todo and receives it through subscribe", async () => {
    // Node has no global EventSource here, so this exercises the fetch-stream path.
    const alice = new BtbClient({ url, caller: "alice" });
    const events = nextResult<[bigint, boolean, string][]>();
    const stop = alice.subscribe("list_mine", t.unit, [], Todos, events.push);
    expect(await events.next()).toEqual([]);

    const id = await alice.mutation("add", t.tuple(t.string), ["buy milk"], t.u64);
    expect(id).toBe(0n);
    expect(await events.next()).toEqual([[0n, false, "buy milk"]]);

    expect(await alice.mutation("toggle", t.tuple(t.u64), [0n], t.bool)).toBe(true);
    expect(await events.next()).toEqual([[0n, true, "buy milk"]]);

    expect(await alice.query("list_mine", t.unit, [], Todos)).toEqual([[0n, true, "buy milk"]]);
    expect(await new BtbClient({ url, caller: "bob" }).query("list_mine", t.unit, [], Todos)).toEqual([]);
    stop();
  });

  it("surfaces server errors", async () => {
    const c = new BtbClient({ url, caller: "carol" });
    const e = await c.mutation("toggle", t.tuple(t.u64), [99n], t.bool).catch((x) => x);
    expect(e).toBeInstanceOf(BtbError);
    expect(e.status).toBe(400);
    expect(e.message).toContain("no such todo");
    const e2 = await c.query("nope", t.unit, [], t.u64).catch((x) => x);
    expect(e2.status).toBe(404);
  });
});
