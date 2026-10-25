import { createContext, createElement, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import { type BtbClient, BtbError } from "./client.js";
import { type Codec, encode, toHex } from "./codec.js";

const Ctx = createContext<BtbClient | null>(null);

export function BtbProvider(props: { client: BtbClient; children?: ReactNode }) {
  return createElement(Ctx.Provider, { value: props.client }, props.children);
}

export function useBtbClient(): BtbClient {
  const c = useContext(Ctx);
  if (!c) throw new Error("useQuery/useMutation must be used inside <BtbProvider>");
  return c;
}

export interface QueryState<R> {
  data: R | undefined;
  error: BtbError | undefined;
  loading: boolean;
}

/** Live query: subscribes on mount, updates on every change, unsubscribes on unmount. */
export function useQuery<A extends unknown[], R>(
  name: string,
  argsType: Codec<A>,
  args: A,
  resultType: Codec<R>,
): QueryState<R> {
  const client = useBtbClient();
  const [state, setState] = useState<QueryState<R>>({ data: undefined, error: undefined, loading: true });
  // Codecs are usually created inline; key the effect on the encoded arguments instead.
  const key = toHex(encode(argsType, args));
  const latest = useRef({ argsType, args, resultType });
  latest.current = { argsType, args, resultType };

  useEffect(() => {
    setState((s) => (s.loading ? s : { ...s, loading: true }));
    const l = latest.current;
    return client.subscribe(
      name,
      l.argsType,
      l.args,
      l.resultType,
      (data) => setState({ data, error: undefined, loading: false }),
      (error) => setState((s) => ({ ...s, error, loading: false })),
    );
  }, [client, name, key]);

  return state;
}

export interface MutationState {
  pending: boolean;
  error: BtbError | undefined;
}

/** Returns `[mutate, {pending, error}]`. `mutate(args)` resolves with the result and rejects on failure. */
export function useMutation<A extends unknown[], R>(
  name: string,
  argsType: Codec<A>,
  resultType: Codec<R>,
): [(...args: A) => Promise<R>, MutationState] {
  const client = useBtbClient();
  const [state, setState] = useState<MutationState>({ pending: false, error: undefined });
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  const types = useRef({ argsType, resultType });
  types.current = { argsType, resultType };

  const mutate = useCallback(
    async (...args: A): Promise<R> => {
      if (alive.current) setState({ pending: true, error: undefined });
      try {
        const r = await client.mutation(name, types.current.argsType, args, types.current.resultType);
        if (alive.current) setState({ pending: false, error: undefined });
        return r;
      } catch (e) {
        const error = e instanceof BtbError ? e : new BtbError(String(e));
        if (alive.current) setState({ pending: false, error });
        throw error;
      }
    },
    [client, name],
  );
  return [mutate, state];
}
