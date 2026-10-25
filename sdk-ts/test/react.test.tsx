// @vitest-environment jsdom
import { act, cleanup, render, screen } from "@testing-library/react";
import { createElement as h, useState } from "react";
import { renderToString } from "react-dom/server";
import { afterEach, describe, expect, it } from "vitest";
import { BtbClient, BtbError, t, type Codec } from "../src/index.js";
import { BtbProvider, useMutation, useQuery } from "../src/react.js";

afterEach(cleanup);

class Fake extends BtbClient {
  subs: { name: string; args: unknown[]; on: (r: any) => void; err: (e: BtbError) => void; closed: boolean }[] = [];
  calls: { name: string; args: unknown[] }[] = [];
  next: (() => unknown) | undefined;
  constructor() {
    super({ url: "http://x" });
  }
  override subscribe(name: string, _a: Codec<any>, args: any, _r: Codec<any>, on: any, err?: any) {
    const s = { name, args, on, err, closed: false };
    this.subs.push(s);
    return () => {
      s.closed = true;
    };
  }
  override async mutation(name: string, _a: Codec<any>, args: any, _r: Codec<any>): Promise<any> {
    this.calls.push({ name, args });
    return this.next!();
  }
}

function List() {
  const q = useQuery("list", t.tuple(t.u64), [1n], t.vec(t.string));
  return h("div", { "data-testid": "o" }, q.loading ? "loading" : q.error ? `err:${q.error.message}` : q.data!.join(","));
}

describe("hooks", () => {
  it("useQuery is live and unsubscribes on unmount", () => {
    const c = new Fake();
    const { unmount } = render(h(BtbProvider, { client: c }, h(List)));
    expect(screen.getByTestId("o").textContent).toBe("loading");
    expect(c.subs).toHaveLength(1);
    act(() => c.subs[0]!.on(["a"]));
    expect(screen.getByTestId("o").textContent).toBe("a");
    act(() => c.subs[0]!.on(["a", "b"]));
    expect(screen.getByTestId("o").textContent).toBe("a,b");
    unmount();
    expect(c.subs[0]!.closed).toBe(true);
  });

  it("useQuery reports errors", () => {
    const c = new Fake();
    render(h(BtbProvider, { client: c }, h(List)));
    act(() => c.subs[0]!.err(new BtbError("boom", 400)));
    expect(screen.getByTestId("o").textContent).toBe("err:boom");
  });

  it("useQuery resubscribes when arguments change", () => {
    const c = new Fake();
    function App() {
      const [n, setN] = useState(1n);
      const q = useQuery("q", t.tuple(t.u64), [n], t.u64);
      return h("button", { onClick: () => setN(2n) }, String(q.data ?? "-"));
    }
    render(h(BtbProvider, { client: c }, h(App)));
    act(() => screen.getByRole("button").click());
    expect(c.subs).toHaveLength(2);
    expect(c.subs[0]!.closed).toBe(true);
    expect(c.subs[1]!.args).toEqual([2n]);
  });

  it("useMutation tracks pending and error", async () => {
    const c = new Fake();
    let mutate!: (s: string) => Promise<bigint>;
    function App() {
      const [m, st] = useMutation("add", t.tuple(t.string), t.u64);
      mutate = m;
      return h("div", { "data-testid": "s" }, `${st.pending}|${st.error?.message ?? ""}`);
    }
    render(h(BtbProvider, { client: c }, h(App)));
    let release!: (v: bigint) => void;
    c.next = () => new Promise((r) => (release = r));
    let p!: Promise<bigint>;
    act(() => {
      p = mutate("x");
    });
    expect(screen.getByTestId("s").textContent).toBe("true|");
    await act(async () => {
      release(7n);
      expect(await p).toBe(7n);
    });
    expect(screen.getByTestId("s").textContent).toBe("false|");
    expect(c.calls).toEqual([{ name: "add", args: ["x"] }]);

    c.next = () => Promise.reject(new BtbError("nope", 400));
    await act(async () => {
      await expect(mutate("y")).rejects.toThrow("nope");
    });
    expect(screen.getByTestId("s").textContent).toBe("false|nope");
  });

  it("server rendering shows the loading state; missing provider throws", () => {
    const c = new Fake();
    expect(renderToString(h(BtbProvider, { client: c }, h(List)))).toContain("loading");
    expect(() => renderToString(h(List))).toThrow(/BtbProvider/);
  });
});
