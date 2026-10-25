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

/// State-changing function run in a transaction.
#[proc_macro_attribute]
pub fn mutation(attr: TokenStream, item: TokenStream) -> TokenStream {
    entry("mutation", attr, item)
}

/// A mutation run on a schedule. Accepts an optional `every = "1h"`, which is validated as a
/// string but not yet used (the scheduler comes in Phase 2).
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
    if !attr.is_empty() {
        if kind != "cron" {
            return Err(syn::Error::new(
                attr.span(),
                "this attribute takes no arguments",
            ));
        }
        let _: syn::Ident = syn::parse2::<CronArgs>(attr)?.every;
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
    Ok(quote! {
        #func

        #[doc(hidden)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        #[no_mangle]
        pub extern "C" fn #export(ptr: *const u8, len: usize) {
            #decode
            #body
        }
    })
}

struct CronArgs {
    every: syn::Ident,
}

impl syn::parse::Parse for CronArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let every: syn::Ident = input.parse()?;
        if every != "every" {
            return Err(syn::Error::new(every.span(), "expected `every = \"...\"`"));
        }
        input.parse::<syn::Token![=]>()?;
        let _: LitStr = input.parse()?;
        Ok(CronArgs { every })
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
