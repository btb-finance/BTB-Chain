//! `target/btb/<name>.schema.json` and the typed TypeScript module of `btb codegen --ts`.
//!
//! Both are pure functions of the schema in the ELF (`btb_runtime::schema`), so the output is
//! reproducible byte for byte and can be checked in and diffed.

use btb_runtime::schema::{FnSchema, Schema, TableSchema, Ty};

/// JSON string literal.
fn js(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The schema as JSON: tables (fields, indexes, primary key) and functions (kind, name, argument
/// and result types in the text form of `btb_runtime::schema::Ty`).
pub fn schema_json(name: &str, s: &Schema) -> String {
    let tables: Vec<String> = s
        .tables
        .iter()
        .map(|t| {
            let fields: Vec<String> = t
                .fields
                .iter()
                .map(|(n, ty)| format!("{{\"name\": {}, \"type\": {}}}", js(n), js(&ty.to_string())))
                .collect();
            let idx: Vec<String> = t
                .indexes
                .iter()
                .filter(|i| t.primary.as_ref() != Some(i))
                .map(|i| js(i))
                .collect();
            format!(
                "    {{\n      \"name\": {},\n      \"fields\": [{}],\n      \"indexes\": [{}],\n      \"primary\": {}\n    }}",
                js(&t.name),
                fields.join(", "),
                idx.join(", "),
                t.primary.as_deref().map_or("null".into(), js)
            )
        })
        .collect();
    let fns: Vec<String> = s
        .functions
        .iter()
        .map(|f| {
            let args: Vec<String> = f
                .args
                .iter()
                .map(|(n, ty)| format!("{{\"name\": {}, \"type\": {}}}", js(n), js(&ty.to_string())))
                .collect();
            format!(
                "    {{\"kind\": {}, \"name\": {}, \"args\": [{}], \"returns\": {}, \"fallible\": {}}}",
                js(&f.kind),
                js(&f.name),
                args.join(", "),
                f.ret.as_ref().map_or("null".into(), |t| js(&t.to_string())),
                f.fallible
            )
        })
        .collect();
    let list = |v: &[String]| {
        if v.is_empty() {
            String::new()
        } else {
            format!("\n{}\n  ", v.join(",\n"))
        }
    };
    format!(
        "{{\n  \"contract\": {},\n  \"tables\": [{}],\n  \"functions\": [{}]\n}}\n",
        js(name),
        list(&tables),
        list(&fns)
    )
}

/// TypeScript type of a schema type, as written in the interfaces and signatures.
fn ts_type(ty: &Ty) -> String {
    match ty {
        Ty::Bool => "boolean".into(),
        Ty::U8 | Ty::U16 | Ty::U32 | Ty::I8 | Ty::I16 | Ty::I32 => "number".into(),
        Ty::U64 | Ty::U128 | Ty::I64 | Ty::I128 => "bigint".into(),
        Ty::Str => "string".into(),
        Ty::Address | Ty::Bytes => "Uint8Array".into(),
        Ty::Vec(t) | Ty::Arr(t, _) => {
            let inner = ts_type(t);
            if inner.contains(" | ") {
                format!("({inner})[]")
            } else {
                format!("{inner}[]")
            }
        }
        Ty::Opt(t) => format!("{} | null", ts_type(t)),
        Ty::Tuple(ts) => format!(
            "[{}]",
            ts.iter().map(ts_type).collect::<Vec<_>>().join(", ")
        ),
        Ty::Named(n) => n.clone(),
    }
}

/// Codec expression (`@btb/client`'s `t`) of a schema type.
fn ts_codec(ty: &Ty) -> String {
    match ty {
        Ty::Bool => "t.bool".into(),
        Ty::U8 => "t.u8".into(),
        Ty::U16 => "t.u16".into(),
        Ty::U32 => "t.u32".into(),
        Ty::U64 => "t.u64".into(),
        Ty::U128 => "t.u128".into(),
        Ty::I8 => "t.i8".into(),
        Ty::I16 => "t.i16".into(),
        Ty::I32 => "t.i32".into(),
        Ty::I64 => "t.i64".into(),
        Ty::I128 => "t.i128".into(),
        Ty::Str => "t.string".into(),
        Ty::Address => "t.address".into(),
        Ty::Bytes => "t.bytes".into(),
        Ty::Vec(x) => format!("t.vec({})", ts_codec(x)),
        Ty::Opt(x) => format!("t.option({})", ts_codec(x)),
        Ty::Arr(x, n) => format!("t.array({}, {n})", ts_codec(x)),
        Ty::Tuple(xs) => format!(
            "t.tuple({})",
            xs.iter().map(ts_codec).collect::<Vec<_>>().join(", ")
        ),
        Ty::Named(n) => n.clone(),
    }
}

fn key(name: &str) -> String {
    let ident = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ident {
        name.to_string()
    } else {
        js(name)
    }
}

/// Every named type in `ty` is a table of the schema.
fn resolves(ty: &Ty, s: &Schema) -> bool {
    match ty {
        Ty::Named(n) => s.table(n).is_some(),
        Ty::Vec(t) | Ty::Opt(t) | Ty::Arr(t, _) => resolves(t, s),
        Ty::Tuple(ts) => ts.iter().all(|t| resolves(t, s)),
        _ => true,
    }
}

/// Is the function part of the client API (query or mutation, with types we can name)?
fn exported(f: &FnSchema, s: &Schema) -> Result<(), String> {
    if f.kind != "query" && f.kind != "mutation" {
        return Err(String::new());
    }
    let bad = f
        .args
        .iter()
        .map(|(_, t)| t)
        .chain(f.ret.iter())
        .find(|t| !resolves(t, s));
    match bad {
        Some(t) => Err(format!(
            "{} {}: type {t} has no schema (use primitives, tuples, Vec, Option or a #[btb::table] struct); skipped",
            f.kind, f.name
        )),
        None => Ok(()),
    }
}

/// The generated files of `btb codegen`.
pub struct Generated {
    /// `api.ts`: typed source for TypeScript projects.
    pub ts: String,
    /// `api.js`: the same without types, for pages with no build step.
    pub js: String,
    /// `api.d.ts`: declarations for `api.js`.
    pub dts: String,
    /// Functions that could not be generated (printed by the CLI).
    pub warnings: Vec<String>,
}

fn table_order(s: &Schema) -> Vec<&TableSchema> {
    // A table that mentions another comes after it; the rest keep name order.
    let mut out: Vec<&TableSchema> = Vec::new();
    let mut left: Vec<&TableSchema> = s.tables.iter().collect();
    while !left.is_empty() {
        let before = left.len();
        let mut rest = Vec::new();
        for t in left {
            let ready = t.fields.iter().all(|(_, ty)| {
                let mut ok = true;
                visit_named(ty, &mut |n| {
                    ok &= n == t.name || out.iter().any(|o| o.name == n);
                });
                ok
            });
            if ready {
                out.push(t);
            } else {
                rest.push(t);
            }
        }
        left = rest;
        if left.len() == before {
            out.append(&mut left);
        }
    }
    out
}

fn visit_named(ty: &Ty, f: &mut impl FnMut(&str)) {
    match ty {
        Ty::Named(n) => f(n),
        Ty::Vec(t) | Ty::Opt(t) | Ty::Arr(t, _) => visit_named(t, f),
        Ty::Tuple(ts) => ts.iter().for_each(|t| visit_named(t, f)),
        _ => {}
    }
}

/// Generate the typed client module for `name`'s schema.
pub fn typescript(name: &str, s: &Schema) -> Generated {
    let mut warnings = Vec::new();
    let tables = table_order(s);

    let mut interfaces = String::new(); // interface Todo { .. } for .ts and .d.ts
    let mut codecs_ts = String::new(); // const Todo: Codec<Todo> = t.struct(..)
    let mut codecs_dts = String::new();
    let mut codecs_js = String::new();
    for t in &tables {
        interfaces.push_str(&format!("export interface {} {{\n", t.name));
        for (n, ty) in &t.fields {
            interfaces.push_str(&format!("  {}: {};\n", key(n), ts_type(ty)));
        }
        interfaces.push_str("}\n\n");
        let fields: Vec<String> = t
            .fields
            .iter()
            .map(|(n, ty)| format!("  {}: {},\n", key(n), ts_codec(ty)))
            .collect();
        let body = format!("t.struct({{\n{}}})", fields.concat());
        codecs_ts.push_str(&format!(
            "/** Codec of a `{0}` row. */\nexport const {0}: Codec<{0}> = {body};\n\n",
            t.name
        ));
        codecs_js.push_str(&format!(
            "/** Codec of a `{0}` row. */\nexport const {0} = {body};\n\n",
            t.name
        ));
        codecs_dts.push_str(&format!("export declare const {0}: Codec<{0}>;\n", t.name));
    }

    let mut entries_ts = String::new();
    let mut entries_js = String::new();
    let mut entries_dts = String::new();
    for f in &s.functions {
        match exported(f, s) {
            Ok(()) => {}
            Err(m) => {
                if !m.is_empty() {
                    warnings.push(m);
                }
                continue;
            }
        }
        let args_ty = format!(
            "[{}]",
            f.args
                .iter()
                .map(|(n, ty)| format!(
                    "{}: {}",
                    n.replace(|c: char| !c.is_ascii_alphanumeric() && c != '_', "_"),
                    ts_type(ty)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let ret_ty = f.ret.as_ref().map_or("void".to_string(), ts_type);
        let args_codec = format!(
            "t.tuple({})",
            f.args
                .iter()
                .map(|(_, ty)| ts_codec(ty))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let ret_codec = f.ret.as_ref().map_or("t.unit".to_string(), ts_codec);
        let (maker, reftype) = if f.kind == "query" {
            ("queryRef", "QueryRef")
        } else {
            ("mutationRef", "MutationRef")
        };
        let doc = format!(
            "  /** {} `{}`{}. */\n",
            f.kind,
            f.name,
            if f.fallible {
                " (aborts with a message on error)"
            } else {
                ""
            }
        );
        // A function without a result decodes zero bytes as `[]` (the unit tuple).
        let (ret_ty, ret_codec) = if f.ret.is_none() {
            ("[]".to_string(), "t.unit".to_string())
        } else {
            (ret_ty, ret_codec)
        };
        entries_ts.push_str(&format!(
            "{doc}  {}: {maker}<{args_ty}, {ret_ty}>({}, {args_codec}, {ret_codec}),\n",
            key(&f.name),
            js(&f.name)
        ));
        entries_js.push_str(&format!(
            "{doc}  {}: {maker}({}, {args_codec}, {ret_codec}),\n",
            key(&f.name),
            js(&f.name)
        ));
        entries_dts.push_str(&format!(
            "{doc}  readonly {}: {reftype}<{args_ty}, {ret_ty}>;\n",
            key(&f.name)
        ));
    }

    let banner = format!(
        "// Generated by `btb codegen` from the schema of the `{name}` contract. Do not edit.\n"
    );
    let ts = format!(
        "{banner}import {{ t, queryRef, mutationRef, type Codec, type QueryRef, type MutationRef }} from \"@btb/client\";\n\n{interfaces}{codecs_ts}/** Typed references to the contract's queries and mutations: `client.query(api.list_mine)`. */\nexport const api = {{\n{entries_ts}}} as const;\n"
    );
    let js_out = format!(
        "{banner}import {{ t, queryRef, mutationRef }} from \"@btb/client\";\n\n{codecs_js}/** Typed references to the contract's queries and mutations: `client.query(api.list_mine)`. */\nexport const api = {{\n{entries_js}}};\n"
    );
    let dts = format!(
        "{banner}import type {{ Codec, QueryRef, MutationRef }} from \"@btb/client\";\n\n{interfaces}{codecs_dts}\nexport declare const api: {{\n{entries_dts}}};\n"
    );
    Generated {
        ts,
        js: js_out,
        dts,
        warnings,
    }
}
