//! Contract-declared schema: tables, indexes and function signatures.
//!
//! `#[btb::table]` and the function macros export one marker symbol per item in the contract's
//! ELF (like the cron and randomness markers). The symbol name carries the description:
//!
//! ```text
//! btb_schema_table_<Name>_<hex>            hex of  f=owner:address|done:bool;i=owner;p=id
//! btb_schema_fn_<kind>_<name>_<hex>        hex of  a=text:string|n:u64;r=u64;e=1
//! btb_schema_prev_<Name>_<ver>_<hex>       an earlier shape of table <Name> (same payload as a table)
//! btb_schema_migrate_<Name>_<from>_<hex>   a migration from version <from> of <Name> (payload `m=1`)
//! ```
//!
//! A table payload may end in `;v=<version>` (default 1). A migration is run by the chain as the
//! pure entry `btb_transform_migrate_<name lowercased>_<from>`: old row bytes in, new row bytes
//! out (see [`migration_entry`] and `evolve`).
//!
//! Types are written in a small text grammar over the `btb-codec` types (see [`Ty`]). The host
//! and the chain derive index extractors from it, so a contract no longer needs `Cargo.toml`
//! metadata: the same ELF decides the same indexes on every node. Everything here is
//! deterministic and bounded.

use crate::layout::IndexValue;
use crate::tables::{IndexDef, MAX_INDEXES};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// Prefix of the table markers.
pub const TABLE_PREFIX: &str = "btb_schema_table_";
/// Prefix of the function markers.
pub const FN_PREFIX: &str = "btb_schema_fn_";
/// Prefix of the markers of earlier shapes of a table (`#[btb::table(version = N, of = "T")]`).
pub const PREV_PREFIX: &str = "btb_schema_prev_";
/// Prefix of the migration markers (`#[btb::migration(from = N)]`).
pub const MIGRATE_PREFIX: &str = "btb_schema_migrate_";

/// Name of the pure entry point (`btb_transform_<this>`) that migrates rows of `table` from
/// `from`. The table name is lowercased because entry names are `[a-z0-9_]`.
pub fn migration_entry(table: &str, from: u32) -> String {
    format!("migrate_{}_{from}", table.to_ascii_lowercase())
}

const MAX_TABLES: usize = 64;
const MAX_PREVIOUS: usize = 64;
const MAX_FUNCTIONS: usize = 256;
const MAX_FIELDS: usize = 64;
const MAX_PAYLOAD: usize = 16 * 1024;
const MAX_DEPTH: usize = 8;

/// A `btb-codec` type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ty {
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    I8,
    I16,
    I32,
    I64,
    I128,
    Str,
    Address,
    Bytes,
    Vec(Box<Ty>),
    Opt(Box<Ty>),
    Arr(Box<Ty>, u32),
    Tuple(Vec<Ty>),
    /// A struct declared with `#[btb::table]` (its fields are in the schema).
    Named(String),
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Bool => f.write_str("bool"),
            Ty::U8 => f.write_str("u8"),
            Ty::U16 => f.write_str("u16"),
            Ty::U32 => f.write_str("u32"),
            Ty::U64 => f.write_str("u64"),
            Ty::U128 => f.write_str("u128"),
            Ty::I8 => f.write_str("i8"),
            Ty::I16 => f.write_str("i16"),
            Ty::I32 => f.write_str("i32"),
            Ty::I64 => f.write_str("i64"),
            Ty::I128 => f.write_str("i128"),
            Ty::Str => f.write_str("string"),
            Ty::Address => f.write_str("address"),
            Ty::Bytes => f.write_str("bytes"),
            Ty::Vec(t) => write!(f, "vec<{t}>"),
            Ty::Opt(t) => write!(f, "opt<{t}>"),
            Ty::Arr(t, n) => write!(f, "arr<{t},{n}>"),
            Ty::Tuple(ts) => {
                f.write_str("(")?;
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{t}")?;
                }
                f.write_str(")")
            }
            Ty::Named(n) => f.write_str(n),
        }
    }
}

impl Ty {
    /// Parse the text form (`u64`, `vec<(u64,bool)>`, `arr<u8,4>`, `opt<address>`, `Todo`).
    pub fn parse(s: &str) -> Result<Ty, String> {
        let mut p = Parser {
            s: s.as_bytes(),
            i: 0,
            depth: 0,
        };
        let t = p.ty()?;
        if p.i != s.len() {
            return Err(format!("trailing characters in type {s:?}"));
        }
        Ok(t)
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected {:?} at {}", c as char, self.i))
        }
    }

    fn ident(&mut self) -> String {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }

    fn ty(&mut self) -> Result<Ty, String> {
        self.depth += 1;
        if self.depth > 16 {
            return Err("type nested too deeply".into());
        }
        let r = self.ty_inner();
        self.depth -= 1;
        r
    }

    fn ty_inner(&mut self) -> Result<Ty, String> {
        if self.peek() == Some(b'(') {
            self.i += 1;
            let mut items = Vec::new();
            while self.peek() != Some(b')') {
                items.push(self.ty()?);
                if self.peek() == Some(b',') {
                    self.i += 1;
                } else {
                    break;
                }
            }
            self.eat(b')')?;
            return Ok(Ty::Tuple(items));
        }
        let id = self.ident();
        Ok(match id.as_str() {
            "" => return Err(format!("expected a type at {}", self.i)),
            "bool" => Ty::Bool,
            "u8" => Ty::U8,
            "u16" => Ty::U16,
            "u32" => Ty::U32,
            "u64" => Ty::U64,
            "u128" => Ty::U128,
            "i8" => Ty::I8,
            "i16" => Ty::I16,
            "i32" => Ty::I32,
            "i64" => Ty::I64,
            "i128" => Ty::I128,
            "string" => Ty::Str,
            "address" => Ty::Address,
            "bytes" => Ty::Bytes,
            "vec" | "opt" => {
                self.eat(b'<')?;
                let inner = Box::new(self.ty()?);
                self.eat(b'>')?;
                if id == "vec" {
                    Ty::Vec(inner)
                } else {
                    Ty::Opt(inner)
                }
            }
            "arr" => {
                self.eat(b'<')?;
                let inner = Box::new(self.ty()?);
                self.eat(b',')?;
                let n = self.ident();
                let n: u32 = n.parse().map_err(|_| "bad array length".to_string())?;
                self.eat(b'>')?;
                Ty::Arr(inner, n)
            }
            _ => Ty::Named(id),
        })
    }
}

/// How a row's fields are looked up by name while walking an encoding.
type Structs = BTreeMap<String, Vec<Ty>>;

impl Ty {
    /// Encoded size when it does not depend on the value.
    fn fixed_size(&self, st: &Structs, depth: usize) -> Option<usize> {
        if depth > MAX_DEPTH {
            return None;
        }
        Some(match self {
            Ty::Bool | Ty::U8 | Ty::I8 => 1,
            Ty::U16 | Ty::I16 => 2,
            Ty::U32 | Ty::I32 => 4,
            Ty::U64 | Ty::I64 => 8,
            Ty::U128 | Ty::I128 => 16,
            Ty::Address => 32,
            Ty::Str | Ty::Bytes | Ty::Vec(_) | Ty::Opt(_) => return None,
            Ty::Arr(t, n) => t.fixed_size(st, depth + 1)?.checked_mul(*n as usize)?,
            Ty::Tuple(ts) => {
                let mut sum = 0usize;
                for t in ts {
                    sum = sum.checked_add(t.fixed_size(st, depth + 1)?)?;
                }
                sum
            }
            Ty::Named(n) => {
                let mut sum = 0usize;
                for t in st.get(n)? {
                    sum = sum.checked_add(t.fixed_size(st, depth + 1)?)?;
                }
                sum
            }
        })
    }

    /// Smallest possible encoding; `None` if a named type is unknown or nested too deeply.
    fn min_size(&self, st: &Structs, depth: usize) -> Option<usize> {
        if depth > MAX_DEPTH {
            return None;
        }
        Some(match self {
            Ty::Str | Ty::Bytes | Ty::Vec(_) => 4,
            Ty::Opt(_) => 1,
            Ty::Arr(t, n) => t.min_size(st, depth + 1)?.checked_mul(*n as usize)?,
            Ty::Tuple(ts) => {
                let mut sum = 0usize;
                for t in ts {
                    sum = sum.checked_add(t.min_size(st, depth + 1)?)?;
                }
                sum
            }
            Ty::Named(n) => {
                let mut sum = 0usize;
                for t in st.get(n)? {
                    sum = sum.checked_add(t.min_size(st, depth + 1)?)?;
                }
                sum
            }
            fixed => fixed.fixed_size(st, depth)?,
        })
    }

    /// Check the type is well formed: names resolve and no collection has zero-size elements
    /// (so walking an encoding is bounded by its length).
    fn validate(&self, st: &Structs, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err("type nested too deeply (or recursive)".into());
        }
        match self {
            Ty::Vec(t) | Ty::Arr(t, _) => {
                t.validate(st, depth + 1)?;
                if t.min_size(st, 0) == Some(0) {
                    return Err(format!("elements of {self} have no size"));
                }
            }
            Ty::Opt(t) => t.validate(st, depth + 1)?,
            Ty::Tuple(ts) => {
                for t in ts {
                    t.validate(st, depth + 1)?;
                }
            }
            Ty::Named(n) => {
                let fields = st.get(n).ok_or_else(|| format!("unknown type {n:?}"))?;
                for t in fields {
                    t.validate(st, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Advance `pos` past one value of this type.
    fn skip(&self, d: &[u8], pos: &mut usize, st: &Structs, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err("type nested too deeply".into());
        }
        if let Some(n) = self.fixed_size(st, depth) {
            return advance(d, pos, n);
        }
        match self {
            Ty::Str | Ty::Bytes => {
                let n = read_u32(d, pos)? as usize;
                advance(d, pos, n)
            }
            Ty::Vec(t) => {
                let n = read_u32(d, pos)? as usize;
                // Every element takes at least one byte (validated), so this bounds the loop.
                if n > d.len().saturating_sub(*pos) {
                    return Err("vector longer than the row".into());
                }
                for _ in 0..n {
                    t.skip(d, pos, st, depth + 1)?;
                }
                Ok(())
            }
            Ty::Opt(t) => match take(d, pos, 1)?[0] {
                0 => Ok(()),
                1 => t.skip(d, pos, st, depth + 1),
                b => Err(format!("invalid option tag {b}")),
            },
            Ty::Arr(t, n) => {
                if (*n as usize) > d.len().saturating_sub(*pos) {
                    return Err("array longer than the row".into());
                }
                for _ in 0..*n {
                    t.skip(d, pos, st, depth + 1)?;
                }
                Ok(())
            }
            Ty::Tuple(ts) => {
                for t in ts {
                    t.skip(d, pos, st, depth + 1)?;
                }
                Ok(())
            }
            Ty::Named(n) => {
                for t in st.get(n).ok_or("unknown type")? {
                    t.skip(d, pos, st, depth + 1)?;
                }
                Ok(())
            }
            _ => Err("unreachable fixed type".into()),
        }
    }

    /// Is this `Option<_>`? A row may end before its trailing options (they read as `None`).
    pub fn is_opt(&self) -> bool {
        matches!(self, Ty::Opt(_))
    }

    /// What kind of index value this type makes, if it can be indexed.
    pub fn index_kind(&self) -> Option<IndexKind> {
        match self {
            Ty::Bool | Ty::U8 | Ty::U16 | Ty::U32 | Ty::U64 => Some(IndexKind::U64),
            Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 => Some(IndexKind::I64),
            Ty::U128 => Some(IndexKind::U128),
            Ty::Str => Some(IndexKind::Str),
            Ty::Address | Ty::Bytes => Some(IndexKind::Bytes),
            Ty::Arr(t, _) if **t == Ty::U8 => Some(IndexKind::Bytes),
            Ty::Opt(t) => t.index_kind(),
            _ => None,
        }
    }

    /// Read the index value of an indexable type at `pos`. `Ok(None)` for `Option::None`.
    fn read_key(&self, d: &[u8], pos: &mut usize) -> Result<Option<IndexValue>, String> {
        let int = |d: &[u8], pos: &mut usize, n: usize| -> Result<u64, String> {
            let b = take(d, pos, n)?;
            let mut a = [0u8; 8];
            a[..n].copy_from_slice(b);
            Ok(u64::from_le_bytes(a))
        };
        let signed = |v: u64, bits: u32| -> i64 {
            let shift = 64 - bits;
            ((v << shift) as i64) >> shift
        };
        Ok(Some(match self {
            Ty::Bool => match take(d, pos, 1)?[0] {
                0 => IndexValue::U64(0),
                1 => IndexValue::U64(1),
                b => return Err(format!("invalid bool tag {b}")),
            },
            Ty::U8 => IndexValue::U64(int(d, pos, 1)?),
            Ty::U16 => IndexValue::U64(int(d, pos, 2)?),
            Ty::U32 => IndexValue::U64(int(d, pos, 4)?),
            Ty::U64 => IndexValue::U64(int(d, pos, 8)?),
            Ty::I8 => IndexValue::I64(signed(int(d, pos, 1)?, 8)),
            Ty::I16 => IndexValue::I64(signed(int(d, pos, 2)?, 16)),
            Ty::I32 => IndexValue::I64(signed(int(d, pos, 4)?, 32)),
            Ty::I64 => IndexValue::I64(int(d, pos, 8)? as i64),
            Ty::U128 => {
                let b = take(d, pos, 16)?;
                IndexValue::U128(u128::from_le_bytes(b.try_into().expect("16 bytes")))
            }
            Ty::Str => {
                let n = read_u32(d, pos)? as usize;
                let b = take(d, pos, n)?;
                IndexValue::Str(
                    String::from_utf8(b.to_vec()).map_err(|_| "invalid UTF-8".to_string())?,
                )
            }
            Ty::Address => IndexValue::Bytes(take(d, pos, 32)?.to_vec()),
            Ty::Bytes => {
                let n = read_u32(d, pos)? as usize;
                IndexValue::Bytes(take(d, pos, n)?.to_vec())
            }
            Ty::Arr(t, n) if **t == Ty::U8 => {
                IndexValue::Bytes(take(d, pos, *n as usize)?.to_vec())
            }
            Ty::Opt(t) => match take(d, pos, 1)?[0] {
                0 => return Ok(None),
                1 => return t.read_key(d, pos),
                b => return Err(format!("invalid option tag {b}")),
            },
            _ => return Err("type cannot be indexed".into()),
        }))
    }
}

/// The index value family of a field type (what `Key` the contract must query with).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexKind {
    U64,
    I64,
    U128,
    Bytes,
    Str,
}

fn take<'a>(d: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], String> {
    let end = pos.checked_add(n).ok_or("row too short")?;
    let b = d.get(*pos..end).ok_or("row too short")?;
    *pos = end;
    Ok(b)
}

fn advance(d: &[u8], pos: &mut usize, n: usize) -> Result<(), String> {
    take(d, pos, n).map(|_| ())
}

fn read_u32(d: &[u8], pos: &mut usize) -> Result<u32, String> {
    let b = take(d, pos, 4)?;
    Ok(u32::from_le_bytes(b.try_into().expect("4 bytes")))
}

/// A `#[btb::table]` struct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableSchema {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
    /// Fields with `#[index]` (and the primary key), in field order.
    pub indexes: Vec<String>,
    /// The `#[primary]` field: indexed and unique.
    pub primary: Option<String>,
    /// Schema version of the table (`#[btb::table(version = N)]`, default 1). Raise it when the
    /// row layout changes incompatibly and provide a `#[btb::migration(from = old)]`.
    pub version: u32,
}

/// One contract function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnSchema {
    /// `query`, `mutation`, `cron`, `action` or `transform`.
    pub kind: String,
    pub name: String,
    pub args: Vec<(String, Ty)>,
    /// The value it returns (`None`: nothing).
    pub ret: Option<Ty>,
    /// Declared `-> btb_sdk::Result<T>`: an `Err` aborts with its message.
    pub fallible: bool,
}

/// Everything a contract declares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schema {
    pub tables: Vec<TableSchema>,
    pub functions: Vec<FnSchema>,
    /// Earlier shapes of tables, declared so that migrations can name the old row type. Sorted by
    /// (table, version).
    pub previous: Vec<TableSchema>,
    /// Declared migrations `(table, from version)`, sorted.
    pub migrations: Vec<(String, u32)>,
}

impl TableSchema {
    pub fn field(&self, name: &str) -> Option<&Ty> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, t)| t)
    }

    pub fn is_unique(&self, index: &str) -> bool {
        self.primary.as_deref() == Some(index)
    }

    /// The index value family of index `index`.
    pub fn index_kind(&self, index: &str) -> Option<IndexKind> {
        self.field(index)?.index_kind()
    }
}

impl Schema {
    pub fn table(&self, name: &str) -> Option<&TableSchema> {
        self.tables.iter().find(|t| t.name == name)
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty() && self.functions.is_empty()
    }

    fn structs(&self) -> Structs {
        self.tables
            .iter()
            .map(|t| {
                (
                    t.name.clone(),
                    t.fields.iter().map(|(_, t)| t.clone()).collect(),
                )
            })
            .collect()
    }

    /// Read the schema markers of a contract ELF (empty when it declares none).
    pub fn from_elf(elf: &[u8]) -> Result<Schema, String> {
        let syms = btb_vm::elf::export_list(elf).map_err(|e| format!("invalid contract: {e:?}"))?;
        let mut s = Schema::default();
        for (sym, _) in syms {
            if let Some(rest) = sym.strip_prefix(TABLE_PREFIX) {
                let (name, hex) = rest
                    .rsplit_once('_')
                    .ok_or_else(|| format!("bad schema symbol {sym:?}"))?;
                s.tables.push(parse_table(name, &unhex(hex)?)?);
            } else if let Some(rest) = sym.strip_prefix(PREV_PREFIX) {
                let bad = || format!("bad schema symbol {sym:?}");
                let (head, hex) = rest.rsplit_once('_').ok_or_else(bad)?;
                let (name, ver) = head.rsplit_once('_').ok_or_else(bad)?;
                let mut t = parse_table(name, &unhex(hex)?)?;
                t.version = ver.parse().map_err(|_| bad())?;
                s.previous.push(t);
            } else if let Some(rest) = sym.strip_prefix(MIGRATE_PREFIX) {
                let bad = || format!("bad schema symbol {sym:?}");
                let (head, hex) = rest.rsplit_once('_').ok_or_else(bad)?;
                unhex(hex)?;
                let (name, from) = head.rsplit_once('_').ok_or_else(bad)?;
                s.migrations
                    .push((name.to_string(), from.parse().map_err(|_| bad())?));
            } else if let Some(rest) = sym.strip_prefix(FN_PREFIX) {
                let (head, hex) = rest
                    .rsplit_once('_')
                    .ok_or_else(|| format!("bad schema symbol {sym:?}"))?;
                let (kind, name) = head
                    .split_once('_')
                    .ok_or_else(|| format!("bad schema symbol {sym:?}"))?;
                s.functions.push(parse_fn(kind, name, &unhex(hex)?)?);
            }
            if s.tables.len() > MAX_TABLES
                || s.functions.len() > MAX_FUNCTIONS
                || s.previous.len() > MAX_PREVIOUS
                || s.migrations.len() > MAX_PREVIOUS
            {
                return Err("too many schema entries".into());
            }
        }
        s.previous
            .sort_by(|a, b| (&a.name, a.version).cmp(&(&b.name, b.version)));
        s.migrations.sort();
        s.tables.sort_by(|a, b| a.name.cmp(&b.name));
        s.functions
            .sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));
        s.validate()?;
        Ok(s)
    }

    /// Check the schema is consistent (what the chain requires to accept a deploy).
    pub fn validate(&self) -> Result<(), String> {
        let st = self.structs();
        for (i, t) in self.tables.iter().enumerate() {
            if self.tables[..i].iter().any(|o| o.name == t.name) {
                return Err(format!("table {} declared twice", t.name));
            }
            if t.fields.len() > MAX_FIELDS {
                return Err(format!("table {}: too many fields", t.name));
            }
            for (j, (f, ty)) in t.fields.iter().enumerate() {
                if t.fields[..j].iter().any(|(g, _)| g == f) {
                    return Err(format!("table {}: field {f} declared twice", t.name));
                }
                ty.validate(&st, 0)
                    .map_err(|e| format!("table {}.{f}: {e}", t.name))?;
            }
            if t.indexes.len() > MAX_INDEXES {
                return Err(format!("table {}: more than {MAX_INDEXES} indexes", t.name));
            }
            for ix in &t.indexes {
                let ty = t
                    .field(ix)
                    .ok_or_else(|| format!("table {}: index on unknown field {ix}", t.name))?;
                if ty.index_kind().is_none() {
                    return Err(format!(
                        "table {}.{ix}: type {ty} cannot be indexed (use integers up to u64/i64, u128, bool, String, Address or bytes)",
                        t.name
                    ));
                }
            }
            if let Some(p) = &t.primary {
                if !t.indexes.contains(p) {
                    return Err(format!("table {}: primary {p} is not an index", t.name));
                }
                if matches!(t.field(p), Some(Ty::Opt(_))) {
                    return Err(format!("table {}: primary {p} cannot be optional", t.name));
                }
            }
        }
        for (i, f) in self.functions.iter().enumerate() {
            if self.functions[..i]
                .iter()
                .any(|o| o.kind == f.kind && o.name == f.name)
            {
                return Err(format!("function {} {} declared twice", f.kind, f.name));
            }
        }
        for (i, p) in self.previous.iter().enumerate() {
            if self.previous[..i]
                .iter()
                .any(|o| o.name == p.name && o.version == p.version)
            {
                return Err(format!(
                    "table {} version {} declared twice",
                    p.name, p.version
                ));
            }
            if p.fields.len() > MAX_FIELDS {
                return Err(format!("table {} v{}: too many fields", p.name, p.version));
            }
            for (f, ty) in &p.fields {
                ty.validate(&st, 0)
                    .map_err(|e| format!("table {} v{}.{f}: {e}", p.name, p.version))?;
            }
            match self.table(&p.name) {
                None => {
                    return Err(format!(
                        "old shape v{} of unknown table {}",
                        p.version, p.name
                    ))
                }
                Some(t) if t.version <= p.version => {
                    return Err(format!(
                        "table {}: old shape v{} must have a lower version than the table (v{})",
                        p.name, p.version, t.version
                    ))
                }
                Some(_) => {}
            }
        }
        for (i, (t, from)) in self.migrations.iter().enumerate() {
            if self.migrations[..i].contains(&(t.clone(), *from)) {
                return Err(format!("migration of {t} from v{from} declared twice"));
            }
            if self.table(t).is_none() {
                return Err(format!("migration of unknown table {t}"));
            }
        }
        Ok(())
    }

    /// The declared old shape `version` of `table`.
    pub fn previous_shape(&self, table: &str, version: u32) -> Option<&TableSchema> {
        self.previous
            .iter()
            .find(|p| p.name == table && p.version == version)
    }

    /// Is there a migration of `table` from `version`?
    pub fn has_migration(&self, table: &str, from: u32) -> bool {
        self.migrations.contains(&(table.to_string(), from))
    }

    /// Check `row` is exactly one well-formed encoding of table `table`'s fields (what a
    /// migration must produce): every field parses and no byte is left over. A row that ends
    /// early where only trailing `Option` fields remain is accepted (they read as `None`).
    pub fn validate_row(&self, table: &str, row: &[u8]) -> Result<(), String> {
        let t = self
            .table(table)
            .ok_or_else(|| format!("unknown table {table}"))?;
        let st = self.structs();
        let mut pos = 0;
        for (name, ty) in &t.fields {
            if pos == row.len() && ty.is_opt() {
                continue;
            }
            ty.skip(row, &mut pos, &st, 0)
                .map_err(|e| format!("field {name}: {e}"))?;
        }
        if pos != row.len() {
            return Err(format!(
                "{} trailing bytes after the last field",
                row.len() - pos
            ));
        }
        Ok(())
    }

    /// Index definitions for every table (index extraction runs from the field types).
    pub fn index_defs(&self) -> Result<Vec<(String, Vec<IndexDef>)>, String> {
        let st = Arc::new(self.structs());
        self.tables
            .iter()
            .map(|t| {
                let defs = t
                    .indexes
                    .iter()
                    .map(|ix| {
                        let k = t
                            .fields
                            .iter()
                            .position(|(n, _)| n == ix)
                            .ok_or_else(|| format!("unknown index field {ix}"))?;
                        let tys: Arc<Vec<Ty>> =
                            Arc::new(t.fields.iter().map(|(_, ty)| ty.clone()).collect());
                        let st = st.clone();
                        let def = IndexDef::new(ix, move |row| {
                            let mut pos = 0;
                            for ty in &tys[..k] {
                                // A row stored before trailing `Option` fields were appended
                                // ends early; the missing fields read as `None`.
                                if pos == row.len() && ty.is_opt() {
                                    continue;
                                }
                                ty.skip(row, &mut pos, &st, 0)?;
                            }
                            if pos == row.len() && tys[k].is_opt() {
                                return Ok(None);
                            }
                            tys[k].read_key(row, &mut pos)
                        });
                        Ok(if t.is_unique(ix) { def.unique() } else { def })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Ok((t.name.clone(), defs))
            })
            .collect()
    }
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) || s.len() > 2 * MAX_PAYLOAD {
        return Err("bad schema payload".into());
    }
    s.as_bytes()
        .chunks(2)
        .map(|c| {
            let h = std::str::from_utf8(c).map_err(|_| "bad hex")?;
            u8::from_str_radix(h, 16).map_err(|_| "bad hex".to_string())
        })
        .collect()
}

fn sections(payload: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let s = std::str::from_utf8(payload).map_err(|_| "schema payload is not UTF-8")?;
    let mut m = BTreeMap::new();
    for part in s.split(';').filter(|p| !p.is_empty()) {
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| format!("bad schema section {part:?}"))?;
        m.insert(k.to_string(), v.to_string());
    }
    Ok(m)
}

fn typed_list(s: &str) -> Result<Vec<(String, Ty)>, String> {
    s.split('|')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (n, t) = p
                .split_once(':')
                .ok_or_else(|| format!("bad field {p:?}"))?;
            Ok((n.to_string(), Ty::parse(t)?))
        })
        .collect()
}

fn parse_table(name: &str, payload: &[u8]) -> Result<TableSchema, String> {
    let m = sections(payload)?;
    let fields = typed_list(m.get("f").map_or("", String::as_str))?;
    let ix: Vec<String> = m
        .get("i")
        .map_or("", String::as_str)
        .split(',')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let primary = m.get("p").filter(|p| !p.is_empty()).cloned();
    // Field order, primary included.
    let indexes = fields
        .iter()
        .map(|(n, _)| n)
        .filter(|n| ix.contains(n) || primary.as_ref() == Some(n))
        .cloned()
        .collect();
    let version = match m.get("v") {
        None => 1,
        Some(v) => v
            .parse::<u32>()
            .ok()
            .filter(|v| *v >= 1)
            .ok_or_else(|| format!("bad table version {v:?}"))?,
    };
    Ok(TableSchema {
        name: name.to_string(),
        fields,
        indexes,
        primary,
        version,
    })
}

fn parse_fn(kind: &str, name: &str, payload: &[u8]) -> Result<FnSchema, String> {
    let m = sections(payload)?;
    let r = m.get("r").map_or("", String::as_str);
    Ok(FnSchema {
        kind: kind.to_string(),
        name: name.to_string(),
        args: typed_list(m.get("a").map_or("", String::as_str))?,
        ret: if r.is_empty() {
            None
        } else {
            Some(Ty::parse(r)?)
        },
        fallible: m.get("e").is_some_and(|e| e == "1"),
    })
}
