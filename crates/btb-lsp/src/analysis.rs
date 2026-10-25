//! Diagnostics for BTB contract sources, found by walking the `syn` syntax tree.

use proc_macro2::Span;
use quote::ToTokens;
use std::collections::HashMap;
use syn::spanned::Spanned;
use syn::visit::Visit;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// One finding. Lines are 0-based; columns count chars.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
    pub severity: Severity,
    pub start: (usize, usize),
    pub end: (usize, usize),
}

const KINDS: [&str; 4] = ["query", "mutation", "cron", "action"];

/// Free functions that write state.
const WRITE_FNS: [&str; 10] = [
    "storage_set",
    "storage_remove",
    "table_insert",
    "table_update",
    "table_delete",
    "token_transfer",
    "token_create_raw",
    "token_transfer_of_raw",
    "token_mint_raw",
    "token_burn_raw",
];

/// Associated functions `Type::f(..)` that write state (Table rows and `Token`).
const WRITE_ASSOC: [&str; 7] = [
    "insert", "update", "delete", "transfer", "mint", "burn", "create",
];
/// Types whose `insert`/`update`/... are ordinary and not BTB writes.
const PLAIN_TYPES: [&str; 12] = [
    "Vec",
    "String",
    "HashMap",
    "BTreeMap",
    "BTreeSet",
    "HashSet",
    "VecDeque",
    "Option",
    "Cell",
    "RefCell",
    "BinaryHeap",
    "Box",
];

/// Method calls `.f(args)` with this many arguments that write state (to avoid `Vec::insert`).
fn write_method(name: &str, args: usize) -> bool {
    matches!(
        (name, args),
        ("insert", 0) | ("update", 1) | ("delete", 1) | ("transfer", 2) | ("mint", 2) | ("burn", 1)
    )
}

/// Run every check on `src`. Returns nothing when the source does not parse.
pub fn analyze(src: &str) -> Vec<Diagnostic> {
    let Ok(file) = syn::parse_file(src) else {
        return Vec::new();
    };
    let mut v = V {
        out: Vec::new(),
        entries: Vec::new(),
        in_query: 0,
    };
    v.visit_file(&file);
    v.check_entries();
    v.out.sort_by_key(|d| (d.start, d.code));
    v.out
}

struct V {
    out: Vec<Diagnostic>,
    /// `(kind, name, span of the name)` of every entry point.
    entries: Vec<(&'static str, String, Span)>,
    in_query: usize,
}

fn range(s: Span) -> ((usize, usize), (usize, usize)) {
    let (a, b) = (s.start(), s.end());
    (
        (a.line.saturating_sub(1), a.column),
        (b.line.saturating_sub(1), b.column),
    )
}

fn entry_kind(attrs: &[syn::Attribute]) -> Option<&'static str> {
    for a in attrs {
        let segs: Vec<String> = a
            .path()
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect();
        let last = segs.last()?;
        let ok = segs.len() == 1 || segs.first().is_some_and(|f| f == "btb");
        if ok {
            if let Some(k) = KINDS.iter().find(|k| *k == last) {
                return Some(k);
            }
        }
    }
    None
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| a.path().is_ident("cfg") && a.meta.to_token_stream().to_string().contains("test"))
}

impl V {
    fn push(&mut self, code: &'static str, msg: String, sev: Severity, span: Span) {
        let (start, end) = range(span);
        self.out.push(Diagnostic {
            code,
            message: msg,
            severity: sev,
            start,
            end,
        });
    }

    fn check_entries(&mut self) {
        let mut seen: HashMap<(&str, String), usize> = HashMap::new();
        let mut by_name: HashMap<String, &'static str> = HashMap::new();
        let entries = std::mem::take(&mut self.entries);
        for (kind, name, span) in &entries {
            if !(1..=64).contains(&name.len())
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            {
                self.push(
                    "btb::entry-name",
                    format!("entry point name `{name}` must match [a-z0-9_]{{1,64}}"),
                    Severity::Error,
                    *span,
                );
            }
            if seen.insert((kind, name.clone()), 1).is_some() {
                self.push(
                    "btb::duplicate-entry",
                    format!("duplicate {kind} `{name}`: each entry point name may be defined once"),
                    Severity::Error,
                    *span,
                );
            } else if let Some(other) = by_name.insert(name.clone(), kind) {
                if other != *kind {
                    self.push(
                        "btb::duplicate-entry",
                        format!(
                            "`{name}` is already used by a {other}; clients call functions by name"
                        ),
                        Severity::Warning,
                        *span,
                    );
                }
            }
        }
    }

    fn write_in_query(&mut self, what: &str, span: Span) {
        if self.in_query > 0 {
            self.push(
                "btb::query-write",
                format!(
                    "queries cannot write state: `{what}` changes state (use a #[btb::mutation])"
                ),
                Severity::Error,
                span,
            );
        }
    }

    fn banned_root(&mut self, segs: &[String], span: Span) {
        let Some(first) = segs.first() else { return };
        let second = segs.get(1).map(String::as_str).unwrap_or("");
        match first.as_str() {
            "std" => {
                let why = match second {
                    "thread" => "threads are nondeterministic",
                    "time" => "system time is nondeterministic",
                    _ => "contracts are no_std; use `core` or `alloc`",
                };
                self.push(
                    "btb::forbidden-std",
                    format!("`std{}` is not allowed in contracts: {why}", if second.is_empty() { String::new() } else { format!("::{second}") }),
                    Severity::Error,
                    span,
                );
            }
            "rand" | "getrandom" | "fastrand" => self.push(
                "btb::forbidden-rand",
                format!("`{first}` is not allowed: randomness only comes from the chain beacon in mutations/crons marked `randomness`"),
                Severity::Error,
                span,
            ),
            "SystemTime" | "Instant" => self.push(
                "btb::forbidden-time",
                format!("`{first}`: system time is nondeterministic; use block_height()"),
                Severity::Error,
                span,
            ),
            _ => {}
        }
    }
}

fn tree_roots(t: &syn::UseTree, prefix: &mut Vec<String>, out: &mut Vec<(Vec<String>, Span)>) {
    match t {
        syn::UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            tree_roots(&p.tree, prefix, out);
            prefix.pop();
        }
        syn::UseTree::Name(n) => {
            let mut v = prefix.clone();
            v.push(n.ident.to_string());
            out.push((v, n.span()));
        }
        syn::UseTree::Rename(n) => {
            let mut v = prefix.clone();
            v.push(n.ident.to_string());
            out.push((v, n.span()));
        }
        syn::UseTree::Glob(g) => out.push((prefix.clone(), g.span())),
        syn::UseTree::Group(g) => {
            for i in &g.items {
                tree_roots(i, prefix, out);
            }
        }
    }
}

impl<'ast> Visit<'ast> for V {
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        if !is_cfg_test(&m.attrs) {
            syn::visit::visit_item_mod(self, m);
        }
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        if is_cfg_test(&f.attrs) {
            return;
        }
        let kind = entry_kind(&f.attrs);
        if let Some(k) = kind {
            self.entries
                .push((k, f.sig.ident.to_string(), f.sig.ident.span()));
        }
        let q = kind == Some("query");
        if q {
            self.in_query += 1;
        }
        syn::visit::visit_item_fn(self, f);
        if q {
            self.in_query -= 1;
        }
    }

    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        let mut roots = Vec::new();
        tree_roots(&u.tree, &mut Vec::new(), &mut roots);
        for (segs, span) in roots {
            self.banned_root(&segs, span);
        }
    }

    fn visit_item_extern_crate(&mut self, e: &'ast syn::ItemExternCrate) {
        self.banned_root(&[e.ident.to_string()], e.ident.span());
    }

    fn visit_path(&mut self, p: &'ast syn::Path) {
        let segs: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
        self.banned_root(&segs, p.span());
        if p.segments.len() == 1 {
            let id = &p.segments[0].ident;
            if id == "f32" || id == "f64" {
                self.push(
                    "btb::float",
                    format!("`{id}` is not allowed: floating point is nondeterministic (use integers or fixed point)"),
                    Severity::Error,
                    id.span(),
                );
            }
        }
        syn::visit::visit_path(self, p);
    }

    fn visit_lit_float(&mut self, l: &'ast syn::LitFloat) {
        self.push(
            "btb::float",
            "floating point literals are not allowed: floats are nondeterministic".into(),
            Severity::Error,
            l.span(),
        );
    }

    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*c.func {
            let segs: Vec<String> = p
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            if let Some(last) = segs.last() {
                let free = WRITE_FNS.contains(&last.as_str());
                let assoc = segs.len() >= 2 && WRITE_ASSOC.contains(&last.as_str()) && {
                    let ty = &segs[segs.len() - 2];
                    ty.starts_with(|c: char| c.is_ascii_uppercase())
                        && !PLAIN_TYPES.contains(&ty.as_str())
                };
                if free || assoc {
                    self.write_in_query(&segs.join("::"), c.func.span());
                }
            }
        }
        syn::visit::visit_expr_call(self, c);
    }

    fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
        let name = m.method.to_string();
        if write_method(&name, m.args.len()) {
            self.write_in_query(&format!(".{name}()"), m.method.span());
        }
        syn::visit::visit_expr_method_call(self, m);
    }
}
