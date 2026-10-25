# BTB Binary Encoding

Language-neutral specification of how arguments and results are encoded for contract calls. The reference implementation is the `btb-codec` crate. Every language SDK must produce and accept exactly these bytes.

**Principles:** deterministic, **canonical** (one valid encoding per value; a decoder must reject everything else), no floats, no padding, no self-description (both sides know the type).

All integers are **little-endian**, fixed width, two's complement for signed types.

## Types

| Type | Layout |
| --- | --- |
| `bool` | 1 byte: `00` false, `01` true. Any other value is invalid. |
| `u8` `u16` `u32` `u64` `u128` | 1, 2, 4, 8, 16 bytes LE |
| `i8` `i16` `i32` `i64` `i128` | same widths, two's complement LE |
| `bytes` / `Vec<u8>` | `u32` LE length N, then N raw bytes |
| `string` | like bytes; content must be valid UTF-8 (shortest form, no surrogates) |
| `Option<T>` | `00` for none; `01` followed by the encoding of `T` for some. Other tags invalid. |
| `Vec<T>` | `u32` LE element count N, then N encodings of `T` |
| `[T; N]` | N encodings of `T`, no length prefix (N is part of the type) |
| tuple `(A, B, ...)` | encodings of each field in order, no prefix; `()` is zero bytes |
| `Address` | 32 raw bytes |

Structs are encoded as the tuple of their fields in declaration order. Enums are not yet specified.

## Decoding rules

A decoder MUST reject: truncated input; trailing bytes after the value; bool or option tags other than 0/1; invalid UTF-8; and any length prefix larger than its configured maximum (default 16,777,216 elements/bytes in `btb-codec`). Allocation must never be sized from an unchecked prefix.

## Structures used by HTTPS actions

Two structures are fixed so that contracts in any language and the validators agree ([ORACLES.md](ORACLES.md#3-custom-web-data-https-requests)). Both are plain concatenations of their fields, in order:

| Structure | Fields |
| --- | --- |
| `HttpRequest` (returned by an action's first phase) | `method: u8` (0 GET, 1 POST), `url: string`, `headers: vec<(string, string)>`, `body: bytes`, `max_response_bytes: u32`, `timeout_ms: u32` |
| `HttpResponse` (the input of a transform) | `status: u16`, `body: bytes` |

The result a validator signs is the encoding of the transform's return value in this same format (or, without a transform, the raw response body). A callback receives `(request id: [u8; 32], args: bytes, status: u8, data: bytes)`.

## Examples (hex)

| Value | Type | Bytes |
| --- | --- | --- |
| `true` | bool | `01` |
| `0x0102` | u16 | `02 01` |
| `0x01020304` | u32 | `04 03 02 01` |
| `-2` | i8 | `fe` |
| `-1` | i32 | `ff ff ff ff` |
| `1` | u128 | `01` then 15 x `00` |
| `"hi"` | string | `02 00 00 00 68 69` |
| `""` | string | `00 00 00 00` |
| `Some(5)` | Option<u8> | `01 05` |
| `None` | Option<u8> | `00` |
| `[1, 2]` | Vec<u16> | `02 00 00 00 01 00 02 00` |
| `[7, 8]` | Vec<u8> | `02 00 00 00 07 08` |
| `[1, 2]` | [u16; 2] | `01 00 02 00` |
| `(1, true)` | (u8, bool) | `01 01` |
| 32 x `ab` | Address | `ab` x 32 |

Invalid examples: `02` as bool; `02 00` as `Option<u8>`; `01 02` as `u8` (trailing byte); `01 00 00 00 ff` as string (bad UTF-8); `ff ff ff ff` as `Vec<u8>` (over max).
