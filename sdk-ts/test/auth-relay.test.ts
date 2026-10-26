// Session keys against a real 4-validator cluster through a hosting node's relay. Started by the
// Rust test `ts_session_flow_through_the_relay` (crates/btb-host/tests/host.rs), which sets
// BTB_RELAY_ROOT, BTB_RELAY_CONTRACT and BTB_RELAY_SEED (a funded account). Skipped on its own.
import { describe, expect, it } from "vitest";
import { BtbAuth, BtbClient, BtbError, LocalKeyWallet, MemoryKeyStore, fromHex, hostTransport, t, encode } from "../src/index.js";

const root = process.env.BTB_RELAY_ROOT;
const contract = process.env.BTB_RELAY_CONTRACT;
const seed = process.env.BTB_RELAY_SEED;

describe.skipIf(!root)("session keys through the relay", () => {
  it("logs in once, increments as the account, and cannot leave the grant", async () => {
    const client = new BtbClient({ url: `${root}/c/${contract}` });
    const wallet = new LocalKeyWallet(fromHex(seed!));
    const auth = new BtbAuth({
      wallet,
      transport: hostTransport(client, root!),
      contract: fromHex(contract!),
      functions: ["increment"],
      store: new MemoryKeyStore(),
      gasLimit: 2_000_000n,
    });
    const id = await auth.login();
    const args = encode(t.tuple(t.u64), [2n]);
    const r1 = await auth.send(client, "increment", args);
    expect(r1.status).toBe("success");
    const r2 = await auth.send(client, "increment", args);
    expect(r2.status).toBe("success");
    // Outside the grant (`tick` is not `increment`): the validator refuses it before any block.
    const err = await auth.send(client, "get", new Uint8Array()).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(BtbError);
    expect((err as BtbError).message).toContain("FunctionNotAllowed");
    // The nonce is not stuck after a refusal.
    expect((await auth.send(client, "increment", args)).status).toBe("success");
    // Revoke: the next call is refused.
    const stale = await auth.signCall("increment", args);
    await auth.logout({ revoke: true });
    expect(id.account).toHaveLength(64);
    const refused = await client.submitSigned(stale).catch((e: unknown) => e);
    expect(refused).toBeInstanceOf(BtbError);
    expect((refused as BtbError).message).toContain("NoSuchSession");
  }, 120_000);
});
