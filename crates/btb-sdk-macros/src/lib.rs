//! Attribute macros for BTB contracts: `#[btb::query]`, `#[btb::mutation]`, `#[btb::cron]`,
//! `#[btb::action]` and `#[btb::table]`. Use through `btb_sdk::btb`.
//!
//! A function macro keeps the original function and adds
//! `#[no_mangle] pub extern "C" fn btb_<kind>_<name>(ptr: *const u8, len: usize)`, the symbol
//! `btb-vm` looks up. It decodes the input as the tuple of the function arguments (see
//! `docs/ENCODING.md`), calls the function, encodes the result and passes it to `return_data`.
//! A function without a return type returns normally and produces no output.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{parse_macro_input, Fields, FnArg, ItemFn, ItemStruct, LitStr, ReturnType};

/// Read-only function; may be subscribed to. Cannot write state.
#[proc_macro_attribute]
pub fn query(attr: TokenStream, item: TokenStream) -> TokenStream {
    entry("query", attr, item)
}

/// State-changing function run in a transaction. `#[btb::mutation(randomness)]` lets it call
/// `random::*` (the chain's threshold-BLS beacon); it then always costs its full gas limit.
#[proc_macro_attribute]
pub fn mutation(attr: TokenStream, item: TokenStream) -> TokenStream {
    entry("mutation", attr, item)
}

/// A mutation run on a schedule. Accepts an optional `every = "N blocks"` (1 to 2^32), which
/// also exports a marker symbol `btb_every_<N>_<name>` that `btb deploy` reads into the
/// manifest's `crons`. Without `every` the cron has no schedule and never runs.
/// Wall-clock units are rejected: the chain schedules by block height only.
/// `#[btb::cron(randomness, every = "N blocks")]` also allows `random::*` (see `mutation`).
#[proc_macro_attribute]
pub fn cron(attr: TokenStream, item: TokenStream) -> TokenStream {
    entry("cron", attr, item)
}

/// A function that calls the outside world; validators agree on the result.
///
/// Its first phase (this function, run read-only on the requesting transaction) returns the
/// `HttpRequest` every validator makes. The agreed result arrives in the mutation
/// `<name>_callback(request_id, args, status, data)`; an optional `#[btb::transform]` of the same
/// name strips what differs between validators' responses first.
#[proc_macro_attribute]
pub fn action(attr: TokenStream, item: TokenStream) -> TokenStream {
    entry("action", attr, item)
}

/// The pure function that turns one validator's `HttpResponse` into the bytes the validators
/// sign, for the action of the same name. It runs with no state access, so it cannot read the
/// chain, the clock or randomness: same response in, same bytes out. Export `btb_transform_<name>`.
#[proc_macro_attribute]
pub fn transform(attr: TokenStream, item: TokenStream) -> TokenStream {
    entry("transform", attr, item)
}

fn entry(kind: &str, attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    match expand_entry(kind, attr.into(), &func) {
        Ok(ts) => ts.into(),
        Err(e) => {
            let e = e.to_compile_error();
            quote!(#func #e).into()
        }
    }
}

fn expand_entry(kind: &str, attr: TokenStream2, func: &ItemFn) -> syn::Result<TokenStream2> {
    let mut every_blocks = None;
    let mut randomness = false;
    // `#[btb::transform("weather")]` names the action it belongs to, so the transform can have
    // its own Rust name next to the action's (default: the function's own name).
    let mut export_name: Option<String> = None;
    if kind == "transform" && !attr.is_empty() {
        export_name = Some(syn::parse2::<LitStr>(attr.clone())?.value());
    } else if !attr.is_empty() {
        if kind != "cron" && kind != "mutation" {
            return Err(syn::Error::new(
                attr.span(),
                if kind == "query" || kind == "action" || kind == "transform" {
                    "queries and actions can never use randomness (only a mutation or cron can be marked `randomness`)"
                } else {
                    "this attribute takes no arguments"
                },
            ));
        }
        let args = syn::parse2::<EntryArgs>(attr)?;
        randomness = args.randomness;
        if let Some(lit) = &args.every {
            if kind != "cron" {
                return Err(syn::Error::new(
                    lit.span(),
                    "`every` is only valid on `#[btb::cron]`",
                ));
            }
            every_blocks = Some(parse_every(lit)?);
        }
    }
    let sig = &func.sig;
    if sig.asyncness.is_some() || !sig.generics.params.is_empty() || sig.variadic.is_some() {
        return Err(syn::Error::new(
            sig.span(),
            "contract functions cannot be async, generic or variadic",
        ));
    }
    let ident_name = sig.ident.to_string();
    let name = export_name.as_deref().unwrap_or(&ident_name);
    let name = name.strip_prefix("r#").unwrap_or(name);
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(syn::Error::new(
            sig.ident.span(),
            "entry-point names must be 1..=64 characters of [a-z0-9_]",
        ));
    }
    let export = format_ident!("btb_{}_{}", kind, name);
    let fn_name = &sig.ident;

    let mut types = Vec::new();
    let mut idents = Vec::new();
    for (i, arg) in sig.inputs.iter().enumerate() {
        match arg {
            FnArg::Typed(t) => {
                types.push(&*t.ty);
                idents.push(format_ident!("__arg{}", i));
            }
            FnArg::Receiver(r) => {
                return Err(syn::Error::new(
                    r.span(),
                    "contract functions cannot take self",
                ))
            }
        }
    }
    if types.len() > 8 {
        return Err(syn::Error::new(
            sig.inputs.span(),
            "at most 8 arguments are supported",
        ));
    }
    let decode = if types.is_empty() {
        quote! { let () = unsafe { ::btb_sdk::__private::decode_args::<()>(ptr, len) }; }
    } else {
        quote! {
            // SAFETY: the VM passes a pointer to `len` readable input bytes.
            let (#(#idents,)*) =
                unsafe { ::btb_sdk::__private::decode_args::<(#(#types,)*)>(ptr, len) };
        }
    };
    let call = quote! { #fn_name(#(#idents),*) };
    // `-> btb_sdk::Result<T>`: `Ok(v)` returns `v`, `Err(e)` aborts with the message.
    let (ret_ty, fallible) = match &sig.output {
        ReturnType::Default => (None, false),
        ReturnType::Type(_, ty) => match result_ok_type(ty) {
            Some(ok) => (Some(ok), true),
            None => (Some(&**ty), false),
        },
    };
    let body = match (&sig.output, fallible) {
        (ReturnType::Default, _) => quote! { #call; },
        (_, false) => quote! {
            let __result = #call;
            ::btb_sdk::__private::return_value(&__result);
        },
        (_, true) => quote! {
            match #call {
                ::core::result::Result::Ok(__ok) => ::btb_sdk::__private::return_value(&__ok),
                ::core::result::Result::Err(__err) => ::btb_sdk::__private::fail(__err),
            }
        },
    };
    // The schema marker: argument names and types, result type.
    let mut arg_descs = Vec::new();
    for (i, arg) in sig.inputs.iter().enumerate() {
        if let FnArg::Typed(t) = arg {
            let n = match &*t.pat {
                syn::Pat::Ident(p) => p.ident.to_string().trim_start_matches('_').to_string(),
                _ => String::new(),
            };
            let n = if n.is_empty() { format!("arg{i}") } else { n };
            arg_descs.push(format!("{n}:{}", type_desc(&t.ty)?));
        }
    }
    let ret_desc = match ret_ty {
        Some(t) if !matches!(t, syn::Type::Tuple(tt) if tt.elems.is_empty()) => type_desc(t)?,
        _ => String::new(),
    };
    let payload = format!(
        "a={};r={ret_desc}{}",
        arg_descs.join("|"),
        if fallible { ";e=1" } else { "" }
    );
    let schema_sym = format_ident!("btb_schema_fn_{}_{}_{}", kind, name, hex(&payload));
    let schema_marker = quote! {
        #[doc(hidden)]
        #[no_mangle]
        pub extern "C" fn #schema_sym() {}
    };
    let marker = every_blocks.map(|n| {
        let sym = format_ident!("btb_every_{}_{}", n, name);
        quote! {
            #[doc(hidden)]
            #[no_mangle]
            pub extern "C" fn #sym() {}
        }
    });
    let rand_marker = randomness.then(|| {
        let sym = format_ident!("btb_randomness_{}_{}", kind, name);
        quote! {
            #[doc(hidden)]
            #[no_mangle]
            pub extern "C" fn #sym() {}
        }
    });
    Ok(quote! {
        #func

        #[doc(hidden)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        #[no_mangle]
        pub extern "C" fn #export(ptr: *const u8, len: usize) {
            #decode
            #body
        }

        #marker
        #rand_marker
        #schema_marker
    })
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

/// `T` of `Result<T>` / `Result<T, E>` (any path ending in `Result`).
fn result_ok_type(ty: &syn::Type) -> Option<&syn::Type> {
    let syn::Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    if seg.ident != "Result" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(a) = &seg.arguments else {
        return None;
    };
    match a.args.first()? {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    }
}

/// The schema text form of a `btb-codec` type (see `btb_runtime::schema::Ty`). Types the macro
/// cannot see through become a name, resolved against the contract's tables on the host.
fn type_desc(ty: &syn::Type) -> syn::Result<String> {
    use syn::Type;
    Ok(match ty {
        Type::Paren(p) => type_desc(&p.elem)?,
        Type::Group(g) => type_desc(&g.elem)?,
        Type::Tuple(t) => {
            let items: syn::Result<Vec<_>> = t.elems.iter().map(type_desc).collect();
            format!("({})", items?.join(","))
        }
        Type::Array(a) => {
            let n = match &a.len {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Int(i),
                    ..
                }) => i.base10_parse::<u32>()?,
                other => {
                    return Err(syn::Error::new(
                        other.span(),
                        "array lengths in tables and entry points must be integer literals",
                    ))
                }
            };
            format!("arr<{},{n}>", type_desc(&a.elem)?)
        }
        Type::Path(p) => {
            let seg = p
                .path
                .segments
                .last()
                .ok_or_else(|| syn::Error::new(p.span(), "empty type path"))?;
            let id = seg.ident.to_string();
            let inner = || -> syn::Result<&syn::Type> {
                match &seg.arguments {
                    syn::PathArguments::AngleBracketed(a) if a.args.len() == 1 => {
                        match a.args.first() {
                            Some(syn::GenericArgument::Type(t)) => Ok(t),
                            _ => Err(syn::Error::new(seg.span(), "expected a type argument")),
                        }
                    }
                    _ => Err(syn::Error::new(seg.span(), "expected one type argument")),
                }
            };
            match id.as_str() {
                "bool" | "u8" | "u16" | "u32" | "u64" | "u128" | "i8" | "i16" | "i32" | "i64"
                | "i128" => id,
                "String" => "string".into(),
                "Address" | "Token" => "address".into(),
                "Vec" => {
                    let t = inner()?;
                    match type_desc(t)?.as_str() {
                        "u8" => "bytes".into(),
                        d => format!("vec<{d}>"),
                    }
                }
                "Option" => format!("opt<{}>", type_desc(inner()?)?),
                _ => id,
            }
        }
        other => {
            return Err(syn::Error::new(
                other.span(),
                "this type cannot be encoded as a contract argument, result or table field",
            ))
        }
    })
}

/// Largest schedule the chain accepts (`MAX_CRON_EVERY` in `btb-chain`).
const MAX_EVERY: u64 = 1 << 32;

/// `"N blocks"` (or `"1 block"`) to a block count in `1..=2^32`.
fn parse_every(lit: &LitStr) -> syn::Result<u64> {
    let v = lit.value();
    let err = |m: &str| syn::Error::new(lit.span(), m.to_string());
    let (num, unit) = v.trim().split_once(' ').ok_or_else(|| {
        err("expected `every = \"N blocks\"` (the chain schedules by block height)")
    })?;
    if unit.trim() != "blocks" && unit.trim() != "block" {
        return Err(err(
            "only block counts are supported: `every = \"N blocks\"`; time units need a block time",
        ));
    }
    match num.parse::<u64>() {
        Ok(n) if (1..=MAX_EVERY).contains(&n) => Ok(n),
        _ => Err(err("every must be 1 to 4294967296 blocks")),
    }
}

/// The comma-separated attribute arguments: `randomness` and (crons) `every = "N blocks"`,
/// each at most once, in any order.
struct EntryArgs {
    randomness: bool,
    every: Option<LitStr>,
}

impl syn::parse::Parse for EntryArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut args = EntryArgs {
            randomness: false,
            every: None,
        };
        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            if key == "randomness" {
                if args.randomness {
                    return Err(syn::Error::new(key.span(), "`randomness` given twice"));
                }
                args.randomness = true;
            } else if key == "every" {
                if args.every.is_some() {
                    return Err(syn::Error::new(key.span(), "`every` given twice"));
                }
                input.parse::<syn::Token![=]>()?;
                args.every = Some(input.parse()?);
            } else {
                return Err(syn::Error::new(
                    key.span(),
                    "expected `randomness` or `every = \"N blocks\"`",
                ));
            }
            if !input.is_empty() {
                input.parse::<syn::Token![,]>()?;
            }
        }
        Ok(args)
    }
}

/// Turn a struct into a table row: implements `Encode`/`Decode` (the tuple of its fields, in
/// order) and `btb_sdk::table::Table` with the struct name as table name, which gives
/// `insert`, `get`, `update`, `delete`, `scan` and `filter`.
///
/// Fields may be marked `#[index]` (a secondary index, ordered by value) or `#[primary]` (an
/// index whose value is unique; at most one per table). The macro exports the schema in the ELF
/// (so the host and the chain register the indexes) and generates typed lookups:
/// `Todo::by_owner(addr)`, `Todo::by_created_range(a..b)` and, for the primary key,
/// `Todo::get_by_<field>(v)`. Each returns or yields `(id, row)`.
///
/// `#[btb::table(version = N)]` sets the table's schema version (default 1);
/// `#[btb::table(version = N, of = "Table")]` declares an earlier row shape for a
/// `#[btb::migration]` (codec only). Trailing `Option` fields decode as `None` when a stored row
/// ends before them, so appending options is a compatible change.
#[proc_macro_attribute]
pub fn table(attr: TokenStream, item: TokenStream) -> TokenStream {
    let st = parse_macro_input!(item as ItemStruct);
    let args = match syn::parse::<TableArgs>(attr) {
        Ok(a) => a,
        Err(e) => return e.to_compile_error().into(),
    };
    expand_table(st, args)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Migrate the rows of a table to its new layout after a contract upgrade.
///
/// ```ignore
/// #[btb::table(version = 1, of = "Todo")]   // the old row type: same fields as the stored rows
/// pub struct TodoV1 { pub owner: Address, pub done: bool, pub text: String }
///
/// #[btb::table(version = 2)]
/// pub struct Todo { #[index] pub owner: Address, pub status: u8, pub text: String }
///
/// #[btb::migration(from = 1)]
/// fn todo_v1_to_v2(row: TodoV1) -> Todo {
///     Todo { owner: row.owner, status: row.done as u8, text: row.text }
/// }
/// ```
///
/// The function must be pure: the chain runs it row by row in a state-less VM (no storage, no
/// tables, no clock), charging its gas to the upgrade budget. It exports
/// `btb_transform_migrate_<table lowercased>_<from>` and a schema marker
/// `btb_schema_migrate_<Table>_<from>_<hex>`; the table is the name of the return type.
#[proc_macro_attribute]
pub fn migration(attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    match expand_migration(attr.into(), &func) {
        Ok(ts) => ts.into(),
        Err(e) => {
            let e = e.to_compile_error();
            quote!(#func #e).into()
        }
    }
}

fn expand_migration(attr: TokenStream2, func: &ItemFn) -> syn::Result<TokenStream2> {
    struct From(u32);
    impl syn::parse::Parse for From {
        fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
            let key: syn::Ident = input.parse()?;
            if key != "from" {
                return Err(syn::Error::new(key.span(), "expected `from = N`"));
            }
            input.parse::<syn::Token![=]>()?;
            let lit: syn::LitInt = input.parse()?;
            let n: u32 = lit.base10_parse()?;
            if n == 0 {
                return Err(syn::Error::new(lit.span(), "versions start at 1"));
            }
            Ok(From(n))
        }
    }
    let from = syn::parse2::<From>(attr)?.0;
    let sig = &func.sig;
    if sig.asyncness.is_some() || !sig.generics.params.is_empty() || sig.inputs.len() != 1 {
        return Err(syn::Error::new(
            sig.span(),
            "a migration takes exactly one argument (the old row) and is not async or generic",
        ));
    }
    let old_ty = match &sig.inputs[0] {
        FnArg::Typed(t) => &*t.ty,
        FnArg::Receiver(r) => {
            return Err(syn::Error::new(r.span(), "a migration cannot take self"))
        }
    };
    let new_ty = match &sig.output {
        ReturnType::Type(_, ty) => &**ty,
        ReturnType::Default => {
            return Err(syn::Error::new(
                sig.span(),
                "a migration returns the new row type",
            ))
        }
    };
    let table = match new_ty {
        syn::Type::Path(p) if p.qself.is_none() => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .ok_or_else(|| syn::Error::new(new_ty.span(), "empty type path"))?,
        _ => {
            return Err(syn::Error::new(
                new_ty.span(),
                "a migration returns a `#[btb::table]` struct",
            ))
        }
    };
    let export = format_ident!(
        "btb_transform_{}",
        btb_runtime_migration_entry(&table, from)
    );
    let marker = format_ident!("btb_schema_migrate_{}_{}_{}", table, from, hex("m=1"));
    let fn_name = &sig.ident;
    Ok(quote! {
        #func

        #[doc(hidden)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        #[no_mangle]
        pub extern "C" fn #export(ptr: *const u8, len: usize) {
            // SAFETY: the VM passes a pointer to `len` readable input bytes.
            let (__old,) = unsafe { ::btb_sdk::__private::decode_args::<(#old_ty,)>(ptr, len) };
            let __new: #new_ty = #fn_name(__old);
            ::btb_sdk::__private::return_value(&__new);
        }

        #[doc(hidden)]
        #[no_mangle]
        pub extern "C" fn #marker() {}
    })
}

/// Same as `btb_runtime::schema::migration_entry` (the macro crate cannot depend on the host).
fn btb_runtime_migration_entry(table: &str, from: u32) -> String {
    format!("migrate_{}_{from}", table.to_ascii_lowercase())
}

/// `#[btb::table(version = N, of = "Table")]`: the optional arguments of the table macro.
struct TableArgs {
    version: Option<u32>,
    of: Option<LitStr>,
}

impl syn::parse::Parse for TableArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut a = TableArgs {
            version: None,
            of: None,
        };
        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            input.parse::<syn::Token![=]>()?;
            if key == "version" {
                let lit: syn::LitInt = input.parse()?;
                let n: u32 = lit.base10_parse()?;
                if n == 0 {
                    return Err(syn::Error::new(lit.span(), "versions start at 1"));
                }
                if a.version.replace(n).is_some() {
                    return Err(syn::Error::new(key.span(), "`version` given twice"));
                }
            } else if key == "of" {
                if a.of.replace(input.parse()?).is_some() {
                    return Err(syn::Error::new(key.span(), "`of` given twice"));
                }
            } else {
                return Err(syn::Error::new(
                    key.span(),
                    "expected `version = N` or `of = \"Table\"`",
                ));
            }
            if !input.is_empty() {
                input.parse::<syn::Token![,]>()?;
            }
        }
        Ok(a)
    }
}

/// How an indexed field turns into a `Key`.
struct KeyKind {
    /// Argument type of `by_<field>(..)`.
    eq_param: TokenStream2,
    /// Key from the argument `v`.
    eq_key: TokenStream2,
    /// Element type of the range bounds of `by_<field>_range(..)`.
    elem: TokenStream2,
    /// Key from `v: &elem`.
    ref_key: TokenStream2,
}

fn key_kind(desc: &str, ty: &syn::Type) -> Option<KeyKind> {
    let k = |eq_param: TokenStream2, eq_key, elem: TokenStream2, ref_key| KeyKind {
        eq_param,
        eq_key,
        elem,
        ref_key,
    };
    Some(match desc {
        "u8" | "u16" | "u32" | "u64" | "bool" => k(
            quote!(#ty),
            quote!(::btb_sdk::host::Key::U64(v as u64)),
            quote!(#ty),
            quote!(::btb_sdk::host::Key::U64(*v as u64)),
        ),
        "i8" | "i16" | "i32" | "i64" => k(
            quote!(#ty),
            quote!(::btb_sdk::host::Key::I64(v as i64)),
            quote!(#ty),
            quote!(::btb_sdk::host::Key::I64(*v as i64)),
        ),
        "u128" => k(
            quote!(#ty),
            quote!(::btb_sdk::host::Key::U128(v)),
            quote!(#ty),
            quote!(::btb_sdk::host::Key::U128(*v)),
        ),
        "string" => k(
            quote!(&str),
            quote!(::btb_sdk::host::Key::Str(v)),
            quote!(::btb_sdk::__private::String),
            quote!(::btb_sdk::host::Key::Str(v.as_str())),
        ),
        "address" => k(
            quote!(#ty),
            quote!(::btb_sdk::host::Key::Bytes(&v.0)),
            quote!(#ty),
            quote!(::btb_sdk::host::Key::Bytes(&v.0)),
        ),
        "bytes" => k(
            quote!(&[u8]),
            quote!(::btb_sdk::host::Key::Bytes(v)),
            quote!(::btb_sdk::__private::Vec<u8>),
            quote!(::btb_sdk::host::Key::Bytes(v.as_slice())),
        ),
        d if d.starts_with("arr<u8,") => k(
            quote!(#ty),
            quote!(::btb_sdk::host::Key::Bytes(&v)),
            quote!(#ty),
            quote!(::btb_sdk::host::Key::Bytes(v)),
        ),
        _ => return None,
    })
}

/// `T` of `Option<T>`, else the type itself.
fn unwrap_option(ty: &syn::Type) -> &syn::Type {
    if let syn::Type::Path(p) = ty {
        if let Some(seg) = p.path.segments.last() {
            if seg.ident == "Option" {
                if let syn::PathArguments::AngleBracketed(a) = &seg.arguments {
                    if let Some(syn::GenericArgument::Type(t)) = a.args.first() {
                        return t;
                    }
                }
            }
        }
    }
    ty
}

fn expand_table(mut st: ItemStruct, args: TableArgs) -> syn::Result<TokenStream2> {
    if !st.generics.params.is_empty() {
        return Err(syn::Error::new(
            st.generics.span(),
            "tables cannot be generic",
        ));
    }
    // Field attributes `#[index]` and `#[primary]` are ours: read them, then remove them.
    let mut indexed: Vec<(usize, bool)> = Vec::new(); // (field position, primary)
    let mut descs: Vec<(String, String)> = Vec::new();
    for (pos, f) in st.fields.iter_mut().enumerate() {
        let (mut idx, mut pri) = (false, false);
        let mut keep = Vec::new();
        for a in f.attrs.drain(..) {
            if a.path().is_ident("index") {
                a.meta.require_path_only()?;
                idx = true;
            } else if a.path().is_ident("primary") {
                a.meta.require_path_only()?;
                pri = true;
            } else {
                keep.push(a);
            }
        }
        f.attrs = keep;
        let name = f
            .ident
            .as_ref()
            .map_or_else(|| format!("f{pos}"), |i| i.to_string());
        if (idx || pri) && f.ident.is_none() {
            return Err(syn::Error::new(
                f.span(),
                "#[index] and #[primary] need a named field",
            ));
        }
        if idx && pri {
            return Err(syn::Error::new(
                f.span(),
                "#[primary] already indexes the field",
            ));
        }
        if idx || pri {
            indexed.push((pos, pri));
        }
        descs.push((name, type_desc(&f.ty)?));
    }
    if indexed.iter().filter(|(_, p)| *p).count() > 1 {
        return Err(syn::Error::new(
            st.ident.span(),
            "a table has at most one #[primary] field",
        ));
    }
    if indexed.len() > 8 {
        return Err(syn::Error::new(
            st.ident.span(),
            "a table has at most 8 indexes",
        ));
    }
    let name = &st.ident;
    let table_name = name.to_string();
    // Trailing `Option` fields decode as `None` when the row ends before them: rows stored
    // before the field was appended (a compatible schema change) stay readable.
    let is_option = |ty: &syn::Type| !std::ptr::eq(unwrap_option(ty), ty);
    let first_trailing_opt = {
        let tys: Vec<&syn::Type> = st.fields.iter().map(|f| &f.ty).collect();
        let mut i = tys.len();
        while i > 0 && is_option(tys[i - 1]) {
            i -= 1;
        }
        i
    };
    let (encode, decode) = match &st.fields {
        Fields::Named(f) => {
            let ids: Vec<_> = f.named.iter().map(|f| f.ident.as_ref().unwrap()).collect();
            let reads = ids.iter().enumerate().map(|(i, id)| {
                if i >= first_trailing_opt {
                    quote! { #id: if r.remaining() == 0 {
                        ::core::option::Option::None
                    } else {
                        ::btb_sdk::Decode::decode_from(r)?
                    }, }
                } else {
                    quote! { #id: ::btb_sdk::Decode::decode_from(r)?, }
                }
            });
            (
                quote! { #(::btb_sdk::Encode::encode_to(&self.#ids, out);)* },
                quote! { Ok(#name { #(#reads)* }) },
            )
        }
        Fields::Unnamed(f) => {
            let idx: Vec<_> = (0..f.unnamed.len()).map(syn::Index::from).collect();
            (
                quote! { #(::btb_sdk::Encode::encode_to(&self.#idx, out);)* },
                quote! { Ok(#name(#(
                    { let _ = #idx; ::btb_sdk::Decode::decode_from(r)? }
                ,)*)) },
            )
        }
        Fields::Unit => (quote! {}, quote! { Ok(#name) }),
    };

    // Schema marker.
    let fields_txt = descs
        .iter()
        .map(|(n, d)| format!("{n}:{d}"))
        .collect::<Vec<_>>()
        .join("|");
    let idx_txt = indexed
        .iter()
        .filter(|(_, p)| !*p)
        .map(|(pos, _)| descs[*pos].0.clone())
        .collect::<Vec<_>>()
        .join(",");
    let pri_txt = indexed
        .iter()
        .find(|(_, p)| *p)
        .map(|(pos, _)| descs[*pos].0.clone())
        .unwrap_or_default();
    if let Some(of) = &args.of {
        // An earlier row shape: codec only, no table, no lookups. It exists so that a
        // migration can name the old row type, and so the chain can check it against the stored
        // schema.
        let version = args.version.ok_or_else(|| {
            syn::Error::new(
                of.span(),
                "an old row shape needs `version = N` (its version)",
            )
        })?;
        let table = of.value();
        if table.is_empty()
            || !table
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(syn::Error::new(
                of.span(),
                "`of` names a table (an identifier)",
            ));
        }
        let payload = format!("f={fields_txt};i=;p=");
        let sym = format_ident!("btb_schema_prev_{}_{}_{}", table, version, hex(&payload));
        return Ok(quote! {
            #st

            impl ::btb_sdk::Encode for #name {
                fn encode_to(&self, out: &mut ::btb_sdk::__private::Vec<u8>) {
                    let _ = &out;
                    #encode
                }
            }

            impl ::btb_sdk::Decode for #name {
                fn decode_from(
                    r: &mut ::btb_sdk::codec::Reader<'_>,
                ) -> ::core::result::Result<Self, ::btb_sdk::DecodeError> {
                    let _ = &r;
                    #decode
                }
            }

            #[doc(hidden)]
            #[no_mangle]
            pub extern "C" fn #sym() {}
        });
    }
    let version_txt = args.version.map_or(String::new(), |v| format!(";v={v}"));
    let payload = format!("f={fields_txt};i={idx_txt};p={pri_txt}{version_txt}");
    let schema_sym = format_ident!("btb_schema_table_{}_{}", table_name, hex(&payload));

    // Typed lookups.
    let mut lookups = Vec::new();
    for (pos, primary) in &indexed {
        let field = st.fields.iter().nth(*pos).expect("position is valid");
        let fid = field.ident.as_ref().expect("named");
        let fname = fid.to_string();
        let inner_ty = unwrap_option(&field.ty);
        let desc = type_desc(inner_ty)?;
        let Some(kk) = key_kind(&desc, inner_ty) else {
            return Err(syn::Error::new(
                field.ty.span(),
                "this field type cannot be indexed (use integers up to u64/i64, u128, bool, String, Address, [u8; N] or Vec<u8>)",
            ));
        };
        let (eq_param, eq_key, elem, ref_key) = (kk.eq_param, kk.eq_key, kk.elem, kk.ref_key);
        let by = format_ident!("by_{}", fid);
        let by_range = format_ident!("by_{}_range", fid);
        let doc_by = format!("Rows whose `{fname}` equals `v`, ordered by id.");
        let doc_range =
            format!("Rows whose `{fname}` is in `range`, ordered by `{fname}` then id.");
        lookups.push(quote! {
            #[doc = #doc_by]
            pub fn #by(v: #eq_param) -> ::btb_sdk::table::Lookup<Self> {
                ::btb_sdk::table::Lookup::eq(#fname, #eq_key)
            }

            #[doc = #doc_range]
            pub fn #by_range<R: ::core::ops::RangeBounds<#elem>>(
                range: R,
            ) -> ::btb_sdk::table::Lookup<Self> {
                use ::core::ops::Bound as B;
                let conv = |b: B<&#elem>| match b {
                    B::Unbounded => ::btb_sdk::table::OwnedBound::Unbounded,
                    B::Included(v) => ::btb_sdk::table::OwnedBound::Included(
                        ::btb_sdk::table::OwnedKey::of(#ref_key),
                    ),
                    B::Excluded(v) => ::btb_sdk::table::OwnedBound::Excluded(
                        ::btb_sdk::table::OwnedKey::of(#ref_key),
                    ),
                };
                ::btb_sdk::table::Lookup::range(
                    #fname,
                    conv(range.start_bound()),
                    conv(range.end_bound()),
                )
            }
        });
        if *primary {
            let get = format_ident!("get_by_{}", fid);
            let doc = format!("The row whose primary key `{fname}` is `v`, with its id.");
            lookups.push(quote! {
                #[doc = #doc]
                pub fn #get(v: #eq_param) -> ::core::option::Option<(u64, Self)> {
                    Self::#by(v).limit(1).next()
                }
            });
        }
    }
    let lookups_impl = if lookups.is_empty() {
        quote! {}
    } else {
        quote! { impl #name { #(#lookups)* } }
    };

    Ok(quote! {
        #st

        impl ::btb_sdk::Encode for #name {
            fn encode_to(&self, out: &mut ::btb_sdk::__private::Vec<u8>) {
                let _ = &out;
                #encode
            }
        }

        impl ::btb_sdk::Decode for #name {
            fn decode_from(
                r: &mut ::btb_sdk::codec::Reader<'_>,
            ) -> ::core::result::Result<Self, ::btb_sdk::DecodeError> {
                let _ = &r;
                #decode
            }
        }

        impl ::btb_sdk::table::Table for #name {
            const NAME: &'static str = #table_name;
        }

        #lookups_impl

        #[doc(hidden)]
        #[no_mangle]
        pub extern "C" fn #schema_sym() {}
    })
}
