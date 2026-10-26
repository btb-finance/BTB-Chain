// Type-level checks of a generated client (`btb codegen --ts`) against the SDK. Compiled by
// `npm run typecheck` and by test/codegen.test.ts; every `@ts-expect-error` must really error.
import { BtbClient, ContractError, type BtbError } from "@btb/client";
import { useMutation, useQuery } from "@btb/client/react";
import { api, Todo } from "../../../examples/todo/api/api";

declare const client: BtbClient;

export async function usage(): Promise<void> {
  const mine: [bigint, boolean, string][] = await client.query(api.list_mine);
  const first: [bigint, boolean, string] | undefined = mine[0];
  const id: bigint = await client.mutation(api.add, ["milk"]);
  const flipped: boolean = await client.mutation(api.toggle, [id]);
  const recent: Todo[] = await client.query(api.recent, [5]);
  const owner: Uint8Array | undefined = recent[0]?.owner;
  const [rows, cursor]: [[bigint, boolean, string][], Uint8Array] = await client.query(api.page_mine, [
    10,
    new Uint8Array(),
    true,
  ]);
  const stop: () => void = client.subscribe(api.list_mine, [], (r) => {
    const n: number = r.length;
    void n;
  });
  void [first, flipped, owner, rows, cursor, stop];

  // @ts-expect-error argument has the wrong type
  await client.mutation(api.add, [1n]);
  // @ts-expect-error arguments are required
  await client.mutation(api.add);
  // @ts-expect-error a query is not a mutation
  await client.mutation(api.list_mine);
  // @ts-expect-error a mutation is not a query
  await client.query(api.add, ["x"]);
  // @ts-expect-error no such function
  void api.nope;
  // @ts-expect-error the result is not a string
  const bad: string = await client.mutation(api.add, ["x"]);
  void bad;

  try {
    await client.mutation(api.add, [""]);
  } catch (e) {
    if (e instanceof ContractError) {
      const reason: string = e.reason;
      const err: BtbError = e;
      void [reason, err];
    }
  }
}

export function component(): void {
  const q = useQuery(api.list_mine);
  const rows: [bigint, boolean, string][] | undefined = q.data;
  const q2 = useQuery(api.recent, [3]);
  const t: Todo | undefined = q2.data?.[0];
  const [add] = useMutation(api.add);
  void add("x");
  // @ts-expect-error wrong argument type
  void add(1);
  // @ts-expect-error a mutation reference is required
  useMutation(api.list_mine);
  void [rows, t];
}
