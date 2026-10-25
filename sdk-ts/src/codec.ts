// Canonical BTB binary encoding (docs/ENCODING.md). Mirrors the `btb-codec` crate:
// little-endian fixed-width integers (bigint for 64/128 bits), u32 length prefixes,
// strict decoding (no trailing bytes, bool/option tags 0/1, valid UTF-8, length caps).

/** Default maximum length (bytes or elements) a decoder accepts. Same as `btb-codec`. */
export const DEFAULT_MAX_LEN = 1 << 24;

export class CodecError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "CodecError";
  }
}

/** Growable output buffer. */
export class Writer {
  private buf = new Uint8Array(64);
  private len = 0;
  private grow(n: number): void {
    if (this.len + n <= this.buf.length) return;
    let cap = this.buf.length;
    while (cap < this.len + n) cap *= 2;
    const next = new Uint8Array(cap);
    next.set(this.buf.subarray(0, this.len));
    this.buf = next;
  }
  byte(b: number): void {
    this.grow(1);
    this.buf[this.len++] = b;
  }
  bytes(b: Uint8Array): void {
    this.grow(b.length);
    this.buf.set(b, this.len);
    this.len += b.length;
  }
  finish(): Uint8Array {
    return this.buf.slice(0, this.len);
  }
}

/** Input cursor with a configurable length cap. */
export class Reader {
  pos = 0;
  constructor(
    readonly buf: Uint8Array,
    readonly maxLen: number = DEFAULT_MAX_LEN,
  ) {}
  take(n: number): Uint8Array {
    if (this.buf.length - this.pos < n) throw new CodecError("unexpected end of input");
    const out = this.buf.subarray(this.pos, this.pos + n);
    this.pos += n;
    return out;
  }
  /** A u32 length prefix, checked against the maximum before anything is allocated. */
  len(): number {
    const b = this.take(4);
    const n = (b[0]! | (b[1]! << 8) | (b[2]! << 16) | (b[3]! << 24)) >>> 0;
    if (n > this.maxLen) throw new CodecError(`length ${n} exceeds maximum ${this.maxLen}`);
    return n;
  }
  get remaining(): number {
    return this.buf.length - this.pos;
  }
}

/** Encoder + decoder for values of type `T`. */
export interface Codec<T> {
  encode(w: Writer, v: T): void;
  decode(r: Reader): T;
}

export type Infer<C> = C extends Codec<infer T> ? T : never;

/** Encode a value to bytes. */
export function encode<T>(c: Codec<T>, v: T): Uint8Array {
  const w = new Writer();
  c.encode(w, v);
  return w.finish();
}

/** Decode a whole buffer; rejects trailing bytes and over-long length prefixes. */
export function decode<T>(c: Codec<T>, bytes: Uint8Array, maxLen: number = DEFAULT_MAX_LEN): T {
  const r = new Reader(bytes, maxLen);
  const v = c.decode(r);
  if (r.remaining !== 0) throw new CodecError("trailing bytes after value");
  return v;
}

export function toHex(b: Uint8Array): string {
  let s = "";
  for (const x of b) s += x.toString(16).padStart(2, "0");
  return s;
}

export function fromHex(s: string): Uint8Array {
  if (s.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(s)) throw new CodecError("invalid hex");
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(2 * i, 2 * i + 2), 16);
  return out;
}

function uint(bits: number): Codec<bigint> {
  const n = bits / 8;
  const max = (1n << BigInt(bits)) - 1n;
  return {
    encode(w, v) {
      if (typeof v !== "bigint" || v < 0n || v > max) throw new CodecError(`value out of range for u${bits}`);
      for (let i = 0; i < n; i++) w.byte(Number((v >> BigInt(8 * i)) & 0xffn));
    },
    decode(r) {
      const b = r.take(n);
      let v = 0n;
      for (let i = n - 1; i >= 0; i--) v = (v << 8n) | BigInt(b[i]!);
      return v;
    },
  };
}

function sint(bits: number): Codec<bigint> {
  const u = uint(bits);
  const min = -(1n << BigInt(bits - 1));
  const max = (1n << BigInt(bits - 1)) - 1n;
  const mod = 1n << BigInt(bits);
  return {
    encode(w, v) {
      if (typeof v !== "bigint" || v < min || v > max) throw new CodecError(`value out of range for i${bits}`);
      u.encode(w, v < 0n ? v + mod : v);
    },
    decode(r) {
      const v = u.decode(r);
      return v > max ? v - mod : v;
    },
  };
}

/** Small integers (8/16/32 bits) are plain JS numbers. */
function small(bits: number, signed: boolean): Codec<number> {
  const c = signed ? sint(bits) : uint(bits);
  return {
    encode(w, v) {
      if (typeof v !== "number" || !Number.isInteger(v)) throw new CodecError(`expected integer for ${signed ? "i" : "u"}${bits}`);
      c.encode(w, BigInt(v));
    },
    decode: (r) => Number(c.decode(r)),
  };
}

const bool: Codec<boolean> = {
  encode: (w, v) => w.byte(v ? 1 : 0),
  decode(r) {
    const b = r.take(1)[0]!;
    if (b > 1) throw new CodecError(`invalid bool tag ${b}`);
    return b === 1;
  },
};

const bytes: Codec<Uint8Array> = {
  encode(w, v) {
    if (v.length > 0xffffffff) throw new CodecError("length exceeds u32");
    t.u32.encode(w, v.length);
    w.bytes(v);
  },
  decode: (r) => r.take(r.len()).slice(),
};

const string: Codec<string> = {
  encode(w, v) {
    if (/[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/.test(v))
      throw new CodecError("string contains a lone surrogate");
    bytes.encode(w, new TextEncoder().encode(v));
  },
  decode(r) {
    try {
      return new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(r.take(r.len()));
    } catch {
      throw new CodecError("invalid UTF-8");
    }
  },
};

function option<T>(inner: Codec<T>): Codec<T | null> {
  return {
    encode(w, v) {
      if (v === null || v === undefined) w.byte(0);
      else {
        w.byte(1);
        inner.encode(w, v);
      }
    },
    decode(r) {
      const tag = r.take(1)[0]!;
      if (tag === 0) return null;
      if (tag === 1) return inner.decode(r);
      throw new CodecError(`invalid option tag ${tag}`);
    },
  };
}

function vec<T>(inner: Codec<T>): Codec<T[]> {
  return {
    encode(w, v) {
      t.u32.encode(w, v.length);
      for (const x of v) inner.encode(w, x);
    },
    decode(r) {
      const n = r.len();
      const out: T[] = []; // never sized from the prefix
      for (let i = 0; i < n; i++) out.push(inner.decode(r));
      return out;
    },
  };
}

function array<T>(inner: Codec<T>, n: number): Codec<T[]> {
  return {
    encode(w, v) {
      if (v.length !== n) throw new CodecError(`expected array of length ${n}`);
      for (const x of v) inner.encode(w, x);
    },
    decode(r) {
      const out: T[] = [];
      for (let i = 0; i < n; i++) out.push(inner.decode(r));
      return out;
    },
  };
}

type Values<C extends readonly Codec<any>[]> = { [K in keyof C]: Infer<C[K]> };

function tuple<C extends readonly Codec<any>[]>(...items: C): Codec<Values<C>> {
  return {
    encode(w, v) {
      if (v.length !== items.length) throw new CodecError(`expected tuple of ${items.length}`);
      items.forEach((c, i) => c.encode(w, v[i]));
    },
    decode: (r) => items.map((c) => c.decode(r)) as unknown as Values<C>,
  };
}

/** A struct is the tuple of its fields in declaration order (key order of `fields`). */
function struct<F extends Record<string, Codec<any>>>(fields: F): Codec<{ [K in keyof F]: Infer<F[K]> }> {
  const keys = Object.keys(fields);
  return {
    encode(w, v) {
      for (const k of keys) fields[k]!.encode(w, v[k]);
    },
    decode(r) {
      const out: Record<string, unknown> = {};
      for (const k of keys) out[k] = fields[k]!.decode(r);
      return out as { [K in keyof F]: Infer<F[K]> };
    },
  };
}

const address: Codec<Uint8Array> = {
  encode(w, v) {
    if (v.length !== 32) throw new CodecError("address must be 32 bytes");
    w.bytes(v);
  },
  decode: (r) => r.take(32).slice(),
};

/** Type combinators. 8/16/32-bit integers are `number`; 64/128-bit are `bigint`. */
export const t = {
  bool,
  u8: small(8, false),
  u16: small(16, false),
  u32: small(32, false),
  u64: uint(64),
  u128: uint(128),
  i8: small(8, true),
  i16: small(16, true),
  i32: small(32, true),
  i64: sint(64),
  i128: sint(128),
  bytes,
  string,
  address,
  unit: tuple(),
  option,
  vec,
  array,
  tuple,
  struct,
};
