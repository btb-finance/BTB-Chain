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
#[proc_macro_attribute]
pub fn action(attr: TokenStream, item: TokenStream) -> TokenStream {
    entry("action", attr, item)
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
    if !attr.is_empty() {
        if kind != "cron" && kind != "mutation" {
            return Err(syn::Error::new(
                attr.span(),
                if kind == "query" || kind == "action" {
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
    let name = sig.ident.to_string();
    let name = name.strip_prefix("r#").unwrap_or(&name);
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
    let body = match sig.output {
        ReturnType::Default => quote! { #call; },
        ReturnType::Type(..) => quote! {
            let __result = #call;
            ::btb_sdk::__private::return_value(&__result);
        },
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
/// `insert`, `get`, `update`, `delete` and `scan`.
#[proc_macro_attribute]
pub fn table(attr: TokenStream, item: TokenStream) -> TokenStream {
    let st = parse_macro_input!(item as ItemStruct);
    if !attr.is_empty() {
        return syn::Error::new(
            TokenStream2::from(attr).span(),
            "no arguments supported yet",
        )
        .to_compile_error()
        .into();
    }
    expand_table(&st)
        .unwrap_or_else(|e| {
            let e = e.to_compile_error();
            quote!(#st #e)
        })
        .into()
}

fn expand_table(st: &ItemStruct) -> syn::Result<TokenStream2> {
    if !st.generics.params.is_empty() {
        return Err(syn::Error::new(
            st.generics.span(),
            "tables cannot be generic",
        ));
    }
    let name = &st.ident;
    let table_name = name.to_string();
    let (encode, decode) = match &st.fields {
        Fields::Named(f) => {
            let ids: Vec<_> = f.named.iter().map(|f| f.ident.as_ref().unwrap()).collect();
            (
                quote! { #(::btb_sdk::Encode::encode_to(&self.#ids, out);)* },
                quote! { Ok(#name { #(#ids: ::btb_sdk::Decode::decode_from(r)?,)* }) },
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
    })
}
