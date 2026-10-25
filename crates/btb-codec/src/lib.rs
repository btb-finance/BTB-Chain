//! Canonical binary encoding shared by all BTB contract languages.
//!
//! One value has exactly one valid encoding. See `docs/ENCODING.md` for the
//! language-neutral specification. No floats, no system dependencies.
#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// Default maximum length (bytes, or elements for vectors) accepted by `decode`.
pub const DEFAULT_MAX_LEN: usize = 1 << 24;

/// Why decoding failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Input ended before the value was complete.
    UnexpectedEof,
    /// Bytes remained after the value.
    TrailingBytes,
    /// A bool tag other than 0 or 1.
    InvalidBool(u8),
    /// An option tag other than 0 or 1.
    InvalidOptionTag(u8),
    /// String bytes were not valid UTF-8.
    InvalidUtf8,
    /// A length prefix exceeded the configured maximum.
    LengthTooLarge { len: u64, max: usize },
}

/// Cursor over input bytes with a length limit.
pub struct Reader<'a> {
    buf: &'a [u8],
    max_len: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8], max_len: usize) -> Self {
        Reader { buf, max_len }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len()
    }

    /// Take exactly `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if n > self.buf.len() {
            return Err(DecodeError::UnexpectedEof);
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    /// Read a u32 LE length prefix and check it against the maximum.
    pub fn read_len(&mut self) -> Result<usize, DecodeError> {
        let len = u32::decode_from(self)?;
        if len as u64 > self.max_len as u64 {
            return Err(DecodeError::LengthTooLarge {
                len: len as u64,
                max: self.max_len,
            });
        }
        Ok(len as usize)
    }
}

/// Types with a canonical encoding.
pub trait Encode {
    fn encode_to(&self, out: &mut Vec<u8>);

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_to(&mut out);
        out
    }
}

/// Types decodable from the canonical encoding.
pub trait Decode: Sized {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError>;

    /// Decode a whole buffer with the default length limit.
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        Self::decode_with_max(bytes, DEFAULT_MAX_LEN)
    }

    /// Decode a whole buffer; rejects trailing bytes.
    fn decode_with_max(bytes: &[u8], max_len: usize) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes, max_len);
        let v = Self::decode_from(&mut r)?;
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(v)
    }
}

fn put_len(out: &mut Vec<u8>, len: usize) {
    let n = u32::try_from(len).expect("length exceeds u32::MAX");
    n.encode_to(out);
}

macro_rules! int_impl {
    ($($t:ty),*) => {$(
        impl Encode for $t {
            fn encode_to(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }
        }
        impl Decode for $t {
            fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
                let b = r.take(core::mem::size_of::<$t>())?;
                let mut a = [0u8; core::mem::size_of::<$t>()];
                a.copy_from_slice(b);
                Ok(<$t>::from_le_bytes(a))
            }
        }
    )*};
}
int_impl!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128);

impl Encode for bool {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.push(*self as u8);
    }
}
impl Decode for bool {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        match r.take(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(DecodeError::InvalidBool(b)),
        }
    }
}

impl Encode for str {
    fn encode_to(&self, out: &mut Vec<u8>) {
        put_len(out, self.len());
        out.extend_from_slice(self.as_bytes());
    }
}
impl Encode for String {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.as_str().encode_to(out);
    }
}
impl Decode for String {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let n = r.read_len()?;
        let b = r.take(n)?;
        core::str::from_utf8(b)
            .map(String::from)
            .map_err(|_| DecodeError::InvalidUtf8)
    }
}

impl<T: Encode> Encode for Option<T> {
    fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            None => out.push(0),
            Some(v) => {
                out.push(1);
                v.encode_to(out);
            }
        }
    }
}
impl<T: Decode> Decode for Option<T> {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        match r.take(1)?[0] {
            0 => Ok(None),
            1 => Ok(Some(T::decode_from(r)?)),
            b => Err(DecodeError::InvalidOptionTag(b)),
        }
    }
}

impl<T: Encode> Encode for [T] {
    fn encode_to(&self, out: &mut Vec<u8>) {
        put_len(out, self.len());
        for v in self {
            v.encode_to(out);
        }
    }
}
impl<T: Encode> Encode for Vec<T> {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.as_slice().encode_to(out);
    }
}
impl<T: Decode> Decode for Vec<T> {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let n = r.read_len()?;
        // Never trust the prefix for allocation: cap by remaining input.
        let mut v = Vec::with_capacity(n.min(r.remaining()));
        for _ in 0..n {
            v.push(T::decode_from(r)?);
        }
        Ok(v)
    }
}

impl<T: Encode, const N: usize> Encode for [T; N] {
    fn encode_to(&self, out: &mut Vec<u8>) {
        for v in self {
            v.encode_to(out);
        }
    }
}
impl<T: Decode, const N: usize> Decode for [T; N] {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let mut v = Vec::with_capacity(N.min(r.remaining()));
        for _ in 0..N {
            v.push(T::decode_from(r)?);
        }
        match v.try_into() {
            Ok(a) => Ok(a),
            Err(_) => unreachable!("exactly N elements were decoded"),
        }
    }
}

macro_rules! tuple_impl {
    ($(($($n:ident),+))*) => {$(
        impl<$($n: Encode),+> Encode for ($($n,)+) {
            #[allow(non_snake_case)]
            fn encode_to(&self, out: &mut Vec<u8>) {
                let ($($n,)+) = self;
                $($n.encode_to(out);)+
            }
        }
        impl<$($n: Decode),+> Decode for ($($n,)+) {
            fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
                Ok(($($n::decode_from(r)?,)+))
            }
        }
    )*};
}
tuple_impl! {
    (A) (A, B) (A, B, C) (A, B, C, D) (A, B, C, D, E) (A, B, C, D, E, F)
    (A, B, C, D, E, F, G) (A, B, C, D, E, F, G, H)
}

/// A 32-byte account or contract address, encoded as 32 raw bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address(pub [u8; 32]);

impl Encode for Address {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0);
    }
}
impl Decode for Address {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let mut a = [0u8; 32];
        a.copy_from_slice(r.take(32)?);
        Ok(Address(a))
    }
}
