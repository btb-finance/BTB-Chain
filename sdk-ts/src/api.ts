import type { Codec } from "./codec.js";

/**
 * A typed reference to a contract function: its name and the codecs of its arguments and result.
 * `btb codegen --ts` generates one per query and mutation (`api.list_mine`); pass it to the client
 * instead of repeating the name and codecs: `client.query(api.list_mine)`.
 */
export interface FnRef<K extends "query" | "mutation", A extends unknown[], R> {
  readonly kind: K;
  readonly name: string;
  readonly args: Codec<A>;
  readonly result: Codec<R>;
}

export type QueryRef<A extends unknown[], R> = FnRef<"query", A, R>;
export type MutationRef<A extends unknown[], R> = FnRef<"mutation", A, R>;

/** The arguments of a call: omitted when the function takes none. */
export type ArgsParam<A extends unknown[]> = A extends [] ? [args?: A] : [args: A];

/** The type of a reference's arguments / result. */
export type ArgsOf<F> = F extends FnRef<any, infer A, any> ? A : never;
export type ResultOf<F> = F extends FnRef<any, any, infer R> ? R : never;

export function queryRef<A extends unknown[], R>(name: string, args: Codec<A>, result: Codec<R>): QueryRef<A, R> {
  return { kind: "query", name, args, result };
}

export function mutationRef<A extends unknown[], R>(name: string, args: Codec<A>, result: Codec<R>): MutationRef<A, R> {
  return { kind: "mutation", name, args, result };
}

export function isFnRef(x: unknown): x is FnRef<"query" | "mutation", unknown[], unknown> {
  return typeof x === "object" && x !== null && "kind" in x && "name" in x && "args" in x && "result" in x;
}
