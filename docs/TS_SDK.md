# TypeScript SDK (`@btb/client`)

MIT-licensed, no runtime dependencies. Lives in `sdk-ts/`. Entry points: `@btb/client` (codec and client) and `@btb/client/react` (hooks; React 18+ is an optional peer dependency).

It speaks the HTTP/SSE API of the dev server ([DEV_SERVER.md](DEV_SERVER.md)) with the canonical encoding ([ENCODING.md](ENCODING.md)). Arguments go as hex in `?args=`, results come back as hex, and the caller is sent as `?caller=` (dev identity only, no signatures yet).

## Codec

`t` holds combinators; each is a `Codec<T>` with encode and decode. `encode(codec, value)` returns bytes, `decode(codec, bytes, maxLen?)` is strict.

| Rust | TypeScript |
| --- | --- |
| `bool` | `t.bool` (`boolean`) |
| `u8` `u16` `u32` / `i8` `i16` `i32` | `t.u8` ... (`number`) |
| `u64` `u128` / `i64` `i128` | `t.u64` ... (`bigint`) |
| `Vec<u8>` | `t.bytes` (`Uint8Array`) |
| `String` | `t.string` |
| `Address` | `t.address` (32-byte `Uint8Array`) |
| `Option<T>` | `t.option(c)` (`T \| null`) |
| `Vec<T>` / `[T; N]` | `t.vec(c)` / `t.array(c, N)` |
| tuple / `()` | `t.tuple(a, b)` / `t.unit` |
| struct | `t.struct({ field: codec, ... })`, encoded as a tuple in key order |

The decoder rejects what the Rust codec rejects: truncation, trailing bytes, bool/option tags other than 0/1, invalid UTF-8, and length prefixes over 16,777,216 (never allocating from the prefix). The encoder also rejects out-of-range integers and lone surrogates.

## Client

```ts
import { BtbClient, t } from "@btb/client";

const client = new BtbClient({ url: "http://127.0.0.1:8787", caller: "alice" });
const Todos = t.vec(t.tuple(t.u64, t.bool, t.string));

const id = await client.mutation("add", t.tuple(t.string), ["buy milk"], t.u64);
const todos = await client.query("list_mine", t.unit, [], Todos);
const stop = client.subscribe("list_mine", t.unit, [], Todos, (rows) => console.log(rows));
```

`subscribe` uses `EventSource` when the runtime has one and a `fetch` stream reader otherwise (Node), reconnecting after 1 s on dropped streams. Errors are `BtbError` with the HTTP `status` and the server's message.

## Typed clients from the contract schema

`btb codegen --ts DIR` ([CLI.md](CLI.md#btb-codegen)) turns the schema in the contract's ELF into a module whose `api` object holds one typed reference per query and mutation, and whose table names are TypeScript interfaces with codecs. Nothing is repeated by hand:

```ts
import { BtbClient, ContractError } from "@btb/client";
import { api, Todo } from "./btb/api";

const client = new BtbClient({ url: "http://127.0.0.1:8787", caller: "alice" });

const id   = await client.mutation(api.add, ["buy milk"]);   // bigint
const mine = await client.query(api.list_mine);              // [bigint, boolean, string][], no args
const rows: Todo[] = await client.query(api.recent, [10]);   // table rows, typed
const stop = client.subscribe(api.list_mine, [], (rows) => render(rows));
```

A reference is `queryRef(name, argsCodec, resultCodec)` / `mutationRef(...)` (`QueryRef<A, R>`, `MutationRef<A, R>`, `FnRef`); `client.query`, `client.mutation`, `client.subscribe`, `useQuery` and `useMutation` accept it in place of the name-and-codecs form, which keeps working. Arguments are the tuple of the Rust parameters (`[text: string]`) and may be left out when the function takes none; passing a mutation to `query` (or the reverse), a wrong argument type or an unknown function is a compile error. Type mapping: `u8`/`u16`/`u32`/`i8`/`i16`/`i32` to `number`, `u64`/`u128`/`i64`/`i128` to `bigint`, `String` to `string`, `Address` and `Vec<u8>` to `Uint8Array`, `Option<T>` to `T | null`, `Vec<T>`/`[T; N]` to `T[]`, tuples to tuples, table structs to interfaces. `sdk-ts/test/codegen.test.ts` typechecks the generated `examples/todo/api/api.ts` with `strict` and `test/fixtures/usage.ts` pins the compile errors.

**Errors.** A contract that aborts, or returns `Err` from a `-> btb_sdk::Result<T>` function ([LANGUAGE.md](LANGUAGE.md#what-works-today-phase-1)), makes the call fail with HTTP 400 `aborted: <message>`. The client raises `ContractError` (a subclass of `BtbError`) with `reason` set to the contract's message:

```ts
try { await client.mutation(api.add, [""]); }
catch (e) { if (e instanceof ContractError) show(e.reason); }   // "text must be 1..=280 bytes"
```

Other failures (404 unknown function, malformed arguments, network errors) stay plain `BtbError`s.

## Hosting nodes and proofs

The same client talks to a hosting node ([HOSTING.md](HOSTING.md)); the contract is part of the URL: `new BtbClient({ url: "http://host:8788/c/<contract hex>" })`. `query` and `subscribe` are unchanged; `mutation` fails with status 501 (send a signed transaction to a validator).

- `await client.submit({ seed, chainId, nonce, kind })` signs a transaction (`transferKind(to, amount)` or `callKind(contract, fn, args, gasLimit)`, `src/tx.ts`, byte-identical to Rust in `test/fixtures/tx.json`) with the relay host's address as its `relayer` and posts it to the host's `POST /tx`; `relayUrl` and `relayer` options override the host (default: the `url` without `/c/<contract>`) and skip the `GET /relay` lookup. The host forwards it to validators and earns 20% of the fee ([HOSTING.md](HOSTING.md#relaying-transactions)). `client.receipt(id)` returns the receipt or `null`. Lower level: `signTransaction`, `encodeTxBody`, `publicKeyOf`, `transactionId`, `client.submitSigned(bytes)`.
- `client.queryWithProof(name, argsType, args, resultType)` returns `{ result, height, incomplete, proof }`: the result plus Merkle proofs of the storage keys the query read. `incomplete` is true when it also scanned ranges, which a proof cannot cover.
- `client.proof(keys, height?)` fetches a bundle for contract storage keys (default: the newest version whose next block is provably committed).
- `await verifyProof(bundle, { trustedRoot?, expect? })` (async, uses WebCrypto) checks every entry against the bundle's state root and the header of the next block, and throws `ProofError` otherwise. It returns the proven `(key, value)` pairs. `trustedRoot` (for example a validator's `state_root_after` for that height) pins the root; without it the root is only the host's claim. The quorum evidence in the bundle is checked by the Rust verifier (`btb_host::proof::verify_bundle`), not here. What is and is not provable: [HOSTING.md](HOSTING.md#proofs).

## Files

`uploadFile(client, data, opts?)` sends the bytes to the node's upload endpoint (`btb dev`, or a hosting node started with `--upload-dir`) and returns `{ root, rootHex, size, k, m, stored, url }`. `root` is a 32-byte `Uint8Array`, ready for a mutation parameter declared as `t.address` (`Address` in Rust). `fileUrl(client, root)` is the URL of the stored file for `<img src>` or `fetch` (`/file/<root>` on the node, without any `/c/<contract>` suffix; the node verifies the pieces against the root and the response is immutable). `finishUpload(client, root)` asks a hosting node to push the pieces to the storage nodes once the file is stored on chain (a no-op on `btb dev`; status 409 until the chain knows the file).

```ts
const f = await uploadFile(client, file);                  // bytes go off chain, the root comes back
await client.mutation(api.attach, ["photo", f.root]);       // the contract checks files::require_stored
img.src = fileUrl(client, f.root);
```

On `btb dev` the upload is stored and paid for you. On a chain the file must be paid for before a contract accepts it: have the contract pay (`files::pin` in the mutation, then `finishUpload`) or send a `Store` transaction (`btb store`). Design and statuses: [STORAGE.md](STORAGE.md#files-from-contracts). Tests: `sdk-ts/test/files.test.ts` (mocked fetch, and a real `btb-dev` running `examples/files`).

## In a plain page (no bundler)

`btb dev` serves only `web/index.html`, so the example pages inline the built client (`sdk-ts/dist`) in a `<script>` that defines a global `btb`; `node examples/sync-sdk.mjs` refreshes it ([examples/README-web.md](../examples/README-web.md)). Pages use `new btb.BtbClient({ url: location.origin, caller })` and `btb.t`. A page that also has a `<!-- btb-api:begin --><!-- btb-api:end -->` block gets the generated `examples/<app>/api/api.js` inlined as `const api` by the same script, so `examples/todo/web/index.html` calls `client.subscribe(api.list_mine, [], render)` and `client.mutation(api.add, [text])` with no build step; regenerate with `btb codegen --ts examples/todo/api` and then `node examples/sync-sdk.mjs`.

## React

```tsx
import { BtbProvider, useQuery, useMutation } from "@btb/client/react";

<BtbProvider client={client}><Todos /></BtbProvider>

function Todos() {
  const { data, error, loading } = useQuery("list_mine", t.unit, [], TodoList);
  const [add, { pending, error: addError }] = useMutation("add", t.tuple(t.string), t.u64);
  // add("buy milk")
}
```

`useQuery` is live (a subscription) and resubscribes when the name or encoded arguments change. `mutate` resolves with the result and rejects with `BtbError` (`ContractError` for a contract abort). With a generated reference: `useQuery(api.list_mine)`, `useQuery(api.recent, [10])`, `const [add] = useMutation(api.add)`, fully typed.

## Auth: session keys

`BtbAuth` is "log in once, then the app signs for you" ([CHAIN.md, Session keys](CHAIN.md#session-keys)):

```ts
const auth = new BtbAuth({
  wallet: new LocalKeyWallet(seed),          // demos; a real wallet implements Wallet
  transport: devTransport("http://127.0.0.1:8787"),   // btb-dev --auth signed
  contract, functions: ["add"],              // the grant's scope
  ttlBlocks: 10_000n, spendLimit: 1_000_000_000n,     // expiry and fee budget
});
const client = new BtbClient({ url, auth });
await auth.restore();      // on page load: reuse the stored session
await auth.login();        // creates the key, the wallet signs ONE grant
await client.mutation("add", ..., ["buy milk"], t.u64);   // signed by the session key
await auth.logout({ revoke: true });
```

- The session key is WebCrypto Ed25519 created non-extractable and stored as a `CryptoKey` in **IndexedDB** (`IndexedDbKeyStore`; `MemoryKeyStore` without a browser). Page scripts can use it to sign but cannot read its bytes. Anyone with script access to the page can still *use* it until expiry or revocation, which is why the grant is scoped and budgeted.
- `Wallet` is `{ address(), signTransaction(body) }` and signs only `Session` transactions. `LocalKeyWallet` holds a seed in memory: demos and tests, not for real funds.
- Transports: `devTransport(url)` (`GET /auth/info`, `POST /auth/tx`), `hostTransport(client, rootUrl)` (a relaying hosting node: `GET /account/<addr>`, `GET /relay`, `POST /tx`; every transaction names the host as relayer, the account needs funds for the grant fee). Against a chain, mutations are transactions: `auth.send(client, fn, args)` signs with the session key, submits through the relay and waits for the receipt. Against `btb-dev --auth signed`, `client.mutation` sends `X-BTB-Tx` itself.
- With an identity, `client.caller` (queries and subscriptions) is the account.
- Refused signed transactions (outside the grant, expired, revoked, over budget) come back as `BtbError` (status 401 on btb-dev, 400 through a relay) naming the reason, e.g. `FunctionNotAllowed`; the local nonce counter is reset.
- `signTransactionWith(body, publicKey, cryptoKey)`, `grantKind`, `revokeKind` and `TxBody.session` are the low-level pieces; encodings are checked byte for byte against `test/fixtures/session.json`.

React: `const { identity, loading, error, login, logout } = useAuth();` (the provider's client needs an `auth`).

## Tests

`cd sdk-ts && npm ci && npm test`. Codec cases are checked against `sdk-ts/test/fixtures/codec.json`, generated by `btb-codec`; `crates/btb-tests/tests/ts_fixtures.rs` fails when it is stale (`BTB_UPDATE_FIXTURES=1 cargo test -p btb-tests --test ts_fixtures` rewrites it). The integration tests build `examples/todo` and `btb-dev` and run against a real server (also `--auth signed`, `test/auth-dev.test.ts`). `test/auth-relay.test.ts` runs the session flow against a 4-validator cluster through a hosting node's relay; it is started by `ts_session_flow_through_the_relay` in `crates/btb-host/tests/host.rs`. `test/fixtures/session.json` is generated by `crates/btb-chain/tests/session.rs` (`BTB_WRITE_FIXTURES=1`). CI job: `sdk-ts`.
