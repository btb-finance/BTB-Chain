import { createContext, createElement, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import { type ArgsParam, type MutationRef, type QueryRef, isFnRef } from "./api.js";
import { type BtbClient, BtbError } from "./client.js";
import { type Codec, encode, toHex } from "./codec.js";
import type { Identity } from "./auth.js";

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
export function useQuery<A extends unknown[], R>(ref: QueryRef<A, R>, ...args: ArgsParam<A>): QueryState<R>;
export function useQuery<A extends unknown[], R>(
  name: string,
  argsType: Codec<A>,
  args: A,
  resultType: Codec<R>,
): QueryState<R>;
export function useQuery(first: unknown, ...rest: unknown[]): QueryState<unknown> {
  type R = unknown;
  type A = unknown[];
  let name: string;
  let argsType: Codec<A>;
  let args: A;
  let resultType: Codec<R>;
  if (isFnRef(first)) {
    name = first.name;
    argsType = first.args;
    resultType = first.result;
    args = (rest[0] ?? []) as A;
  } else {
    name = first as string;
    [argsType, args, resultType] = rest as [Codec<A>, A, Codec<R>];
  }
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
  ref: MutationRef<A, R>,
): [(...args: A) => Promise<R>, MutationState];
export function useMutation<A extends unknown[], R>(
  name: string,
  argsType: Codec<A>,
  resultType: Codec<R>,
): [(...args: A) => Promise<R>, MutationState];
export function useMutation(first: unknown, ...rest: unknown[]): [(...args: unknown[]) => Promise<unknown>, MutationState] {
  type R = unknown;
  type A = unknown[];
  let name: string;
  let argsType: Codec<A>;
  let resultType: Codec<R>;
  if (isFnRef(first)) {
    name = first.name;
    argsType = first.args;
    resultType = first.result;
  } else {
    name = first as string;
    [argsType, resultType] = rest as [Codec<A>, Codec<R>];
  }
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

export interface AuthState {
  /** The logged-in account and session key, or null. */
  identity: Identity | null;
  /** True while a login/restore is running. */
  loading: boolean;
  error: Error | undefined;
  /** Create a session key and have the wallet sign its grant (once). */
  login: () => Promise<Identity>;
  /** Forget the session key; `revoke` also revokes the grant on chain. */
  logout: (opts?: { revoke?: boolean }) => Promise<void>;
}

/** Session-key auth of the provider's client (`new BtbClient({ auth })`): identity, login, logout. */
export function useAuth(): AuthState {
  const auth = useBtbClient().auth;
  if (!auth) throw new Error("useAuth needs a BtbClient created with an `auth` option");
  const [identity, setIdentity] = useState(auth.identity);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<Error | undefined>(undefined);
  useEffect(() => {
    const unsub = auth.subscribe(() => setIdentity(auth.identity));
    auth
      .restore()
      .catch((e: unknown) => setError(e as Error))
      .finally(() => setLoading(false));
    return unsub;
  }, [auth]);
  const run = useCallback(
    async <T,>(f: () => Promise<T>): Promise<T> => {
      setLoading(true);
      setError(undefined);
      try {
        return await f();
      } catch (e) {
        setError(e as Error);
        throw e;
      } finally {
        setLoading(false);
      }
    },
    [],
  );
  const login = useCallback(() => run(() => auth.login()), [auth, run]);
  const logout = useCallback((opts?: { revoke?: boolean }) => run(() => auth.logout(opts)), [auth, run]);
  return { identity, loading, error, login, logout };
}
