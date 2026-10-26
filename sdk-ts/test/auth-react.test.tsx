// @vitest-environment jsdom
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { createElement as h } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { BtbAuth, BtbClient, LocalKeyWallet, MemoryKeyStore } from "../src/index.js";
import { BtbProvider, useAuth } from "../src/react.js";

afterEach(cleanup);

function Who() {
  const a = useAuth();
  return h(
    "div",
    null,
    h("span", { "data-testid": "id" }, a.loading ? "loading" : (a.identity?.account.slice(0, 8) ?? "anon")),
    h("button", { "data-testid": "in", onClick: () => void a.login() }),
    h("button", { "data-testid": "out", onClick: () => void a.logout() }),
  );
}

describe("useAuth", () => {
  it("login and logout update the identity", async () => {
    let nonce = 0n;
    const auth = new BtbAuth({
      wallet: LocalKeyWallet.random(),
      contract: new Uint8Array(32).fill(1),
      store: new MemoryKeyStore(),
      transport: {
        info: async () => ({ chainId: 1n, height: 10n, nonce }),
        sendSession: async () => void (nonce += 1n),
      },
    });
    const client = new BtbClient({ url: "http://x", auth });
    render(h(BtbProvider, { client }, h(Who)));
    await act(async () => {});
    expect(screen.getByTestId("id").textContent).toBe("anon");
    await act(async () => screen.getByTestId("in").click());
    await waitFor(() => expect(screen.getByTestId("id").textContent).toMatch(/^[0-9a-f]{8}$/));
    await act(async () => screen.getByTestId("out").click());
    await waitFor(() => expect(screen.getByTestId("id").textContent).toBe("anon"));
  });

  it("needs a client with auth", () => {
    const c = new BtbClient({ url: "http://x" });
    expect(() => render(h(BtbProvider, { client: c }, h(Who)))).toThrow(/auth/);
  });
});
