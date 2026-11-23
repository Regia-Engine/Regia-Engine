// SPDX-License-Identifier: MPL-2.0
//! The source scan behind `api-snapshot`: one crate's public items as normalized text lines.
//!
//! The scan reads the crate's `.rs` files with `syn` and never compiles them, so it needs no
//! nightly compiler, runs in the preflight's cheap stage and gives the same answer on every host:
//! a `#[cfg(..)]` item is listed with its condition instead of being evaluated. It walks the
//! module tree from `lib.rs`, follows `pub use` through private modules (an item's public path is
//! where a user can name it) and attaches the inherent and trait impls to the public types.
//! `#[cfg(test)]` and `#[doc(hidden)]` items are not API. Items a macro invocation expands to are
//! invisible, which `docs/api-snapshots/README.md` lists as the known gap.
//!
//! [`scan_crate_with`] names sibling workspace crates that a module moved into (`MOVED_MODULES` in
//! `api_snapshot.rs`): a public whole-crate re-export of one of them (`pub use regia_nav_det as det;`,
//! the crate at `crates/<name>/src/lib.rs` next to the scanned one) is read like the module it stands
//! for: the sibling's public items are listed under the alias, and `crate::` inside the sibling means
//! the sibling (ADR-8877: `regia_nav::det` became the `regia_nav_det` crate with unchanged paths).
//! Every other re-export of an outside crate is kept as one line as written.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use quote::{ToTokens, quote};
use syn::{Attribute, Fields, FnArg, ImplItem, Item, Pat, TraitItem, UseTree, Visibility};

/// The `(module, name)` lookups in progress.
type Visiting = HashSet<(usize, String)>;

/// One line of a snapshot: the public path a user names and the normalized signature.
///
/// Several lines may share a key: an item defined twice under different `#[cfg]`s, or a method
/// in two impl blocks with different generics.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ApiLine {
    /// The item's public path inside the crate, without the crate name (`prelude::App`, `App::new`).
    pub key: String,
    /// The signature, with argument names and bodies dropped.
    pub signature: String,
}

/// Scans the crate whose root file is `lib_rs`: the sorted, deduplicated public API.
pub fn scan_crate(lib_rs: &Path) -> Result<Vec<ApiLine>> {
    scan_crate_with(lib_rs, &[])
}

/// [`scan_crate`], reading a public whole-crate re-export of each sibling workspace crate in `moved`
/// like the module it replaced (see the module docs).
pub fn scan_crate_with(lib_rs: &Path, moved: &[&str]) -> Result<Vec<ApiLine>> {
    let mut tree = Tree {
        moved: moved.iter().map(|m| (*m).to_owned()).collect(),
        ..Tree::default()
    };
    tree.load_root(lib_rs)?;
    tree.walk_exports();
    Ok(tree.lines())
}

/// Renders tokens with fixed spacing rules, so a new `proc-macro2` cannot flip every line.
pub fn render_tokens(tokens: TokenStream) -> String {
    let mut out = String::new();
    render_into(tokens, &mut out);
    out
}

fn render_into(tokens: TokenStream, out: &mut String) {
    let mut joint = false;
    for token in tokens {
        match token {
            TokenTree::Group(group) => {
                let (open, close) = match group.delimiter() {
                    Delimiter::Parenthesis => ("(", ")"),
                    Delimiter::Bracket => ("[", "]"),
                    Delimiter::Brace => ("{ ", " }"),
                    Delimiter::None => ("", ""),
                };
                if group.delimiter() == Delimiter::Brace {
                    push_word_gap(out);
                }
                out.push_str(open);
                render_into(group.stream(), out);
                out.push_str(close);
                joint = false;
            }
            TokenTree::Ident(ident) => {
                push_word_gap(out);
                out.push_str(&ident.to_string());
                joint = false;
            }
            TokenTree::Literal(literal) => {
                push_word_gap(out);
                out.push_str(&literal.to_string());
                joint = false;
            }
            TokenTree::Punct(punct) => {
                let ch = punct.as_char();
                if ch == '>' && !out.ends_with('-') && out.ends_with(", ") {
                    out.truncate(out.len() - 2);
                }
                if matches!(ch, '=' | '+' | '|' | '-') && !joint && !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
                out.push(ch);
                joint = punct.spacing() == Spacing::Joint;
                let colon = ch == ':' && !out.ends_with("::");
                if !joint && (colon || matches!(ch, ',' | ';' | '=' | '+' | '|') || out.ends_with("->")) {
                    out.push(' ');
                }
            }
        }
    }
    // rustfmt adds a trailing comma when it wraps a list: wrapping is no API change.
    while out.ends_with(' ') {
        out.pop();
    }
    if out.ends_with(',') {
        out.pop();
    }
}

/// A gap before a word, unless the previous character glues to it.
fn push_word_gap(out: &mut String) {
    let glued = out.chars().next_back().is_none_or(|c| {
        matches!(
            c,
            ' ' | '(' | '[' | '<' | ':' | '&' | '\'' | '!' | '#' | '.' | '*' | '?'
        )
    });
    if !glued {
        out.push(' ');
    }
}

fn render(item: impl ToTokens) -> String {
    render_tokens(item.into_token_stream())
}

/// An item or module a path can land on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Target {
    Item(usize),
    Module(usize),
    /// Outside this crate (or an enum variant): kept as written.
    Elsewhere(String),
}

/// A target with the written conditions along its resolution route.
struct Resolution {
    target: Target,
    cfg: Vec<String>,
}

impl Resolution {
    fn under(mut self, cfg: &[String]) -> Self {
        let mut conditions = cfg.to_vec();
        conditions.append(&mut self.cfg);
        self.cfg = conditions;
        self
    }
}

struct Module {
    parent: Option<usize>,
    /// The module `crate::` names from here: 0, or the alias module of a re-exported sibling crate.
    root: usize,
    name: String,
    public: bool,
    hidden: bool,
    cfg: Vec<String>,
    items: Vec<usize>,
    children: Vec<usize>,
    uses: Vec<UseLeaf>,
}

struct UseLeaf {
    public: bool,
    hidden: bool,
    /// The name it binds; `None` for a glob.
    name: Option<String>,
    path: Vec<String>,
    cfg: Vec<String>,
}

struct Defined {
    name: String,
    public: bool,
    hidden: bool,
    cfg: Vec<String>,
    item: Item,
}

struct ImplBlock {
    module: usize,
    cfg: Vec<String>,
    block: syn::ItemImpl,
}

/// An impl's signature before combining its conditions with the exported owner.
#[derive(Clone)]
struct ImplLine {
    suffix: String,
    cfg: Vec<String>,
    signature: String,
}

/// The crate's modules, items and exports.
#[derive(Default)]
struct Tree {
    modules: Vec<Module>,
    defined: Vec<Defined>,
    impls: Vec<ImplBlock>,
    macros: Vec<(String, Vec<String>)>,
    /// The workspace's `crates/` folder (the scanned crate's grandparent folder), where sibling crates live.
    crates_dir: Option<PathBuf>,
    /// The sibling crates a module moved into, read in place of their whole-crate re-export.
    moved: BTreeSet<String>,
    /// The sibling crates read so far through a whole-crate re-export (a cycle guard).
    grafted: BTreeSet<String>,
    /// Public path to item, with the conditions on the way there.
    item_paths: BTreeMap<usize, BTreeSet<(String, Vec<String>)>>,
    lines: BTreeSet<ApiLine>,
}

fn cfgs(attrs: &[Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("cfg"))
        .map(render)
        .collect()
}

/// Recognizes test-qualified predicates that are structurally false in non-test builds,
/// including unreachable test branches. Other configuration atoms stay unknown.
fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().filter(|a| a.path().is_ident("cfg")).any(|a| {
        a.parse_args::<syn::Expr>()
            .is_ok_and(|predicate| cfg_value(&predicate, false) == Some(false) && references_test(&predicate))
    })
}

/// Finds built-in test atoms even in operands after a decisive boolean value.
fn references_test(predicate: &syn::Expr) -> bool {
    match predicate {
        syn::Expr::Path(path) => path.path.is_ident("test"),
        syn::Expr::Call(call) => {
            let syn::Expr::Path(operator) = call.func.as_ref() else {
                return false;
            };
            (operator.path.is_ident("all") || operator.path.is_ident("any") || operator.path.is_ident("not"))
                && call.args.iter().any(references_test)
        }
        _ => false,
    }
}

/// Evaluates only `test`, boolean literals and cfg operators; `None` means unknown.
/// Rust expressions also represent cfg key-value options as assignments, which stay unknown.
fn cfg_value(predicate: &syn::Expr, test: bool) -> Option<bool> {
    match predicate {
        syn::Expr::Path(path) if path.path.is_ident("test") => Some(test),
        syn::Expr::Lit(lit) => match &lit.lit {
            syn::Lit::Bool(value) => Some(value.value),
            _ => None,
        },
        syn::Expr::Call(call) => {
            let syn::Expr::Path(operator) = call.func.as_ref() else {
                return None;
            };
            let args = &call.args;
            if operator.path.is_ident("not") {
                if args.len() != 1 {
                    return None;
                }
                return cfg_value(args.first()?, test).map(|value| !value);
            }
            let decisive = if operator.path.is_ident("all") {
                false
            } else if operator.path.is_ident("any") {
                true
            } else {
                return None;
            };
            let mut unknown = false;
            for arg in args {
                match cfg_value(arg, test) {
                    Some(value) if value == decisive => return Some(decisive),
                    None => unknown = true,
                    _ => {}
                }
            }
            if unknown { None } else { Some(!decisive) }
        }
        _ => None,
    }
}

fn doc_hidden(attrs: &[Attribute]) -> bool {
    attrs.iter().filter(|a| a.path().is_ident("doc")).any(|a| {
        a.parse_args::<TokenStream>()
            .is_ok_and(|t| render_tokens(t) == "hidden")
    })
}

/// The suffix of a deprecation line's key: `<item> deprecated`.
pub const DEPRECATED_SUFFIX: &str = " deprecated";

/// `#[deprecated(since = "..", note = "..")]`, `#[deprecated = ".."]` or a bare `#[deprecated]`,
/// as the signature of the item's deprecation line (`since 0.2.3: use X`).
fn deprecation(attrs: &[Attribute]) -> Option<String> {
    let attr = attrs.iter().find(|a| a.path().is_ident("deprecated"))?;
    let (mut since, mut note) = (String::new(), String::new());
    match &attr.meta {
        syn::Meta::List(_) => {
            let _ = attr.parse_nested_meta(|meta| {
                let value: syn::LitStr = meta.value()?.parse()?;
                if meta.path.is_ident("since") {
                    since = value.value();
                } else if meta.path.is_ident("note") {
                    note = value.value();
                }
                Ok(())
            });
        }
        syn::Meta::NameValue(pair) => {
            if let syn::Expr::Lit(lit) = &pair.value
                && let syn::Lit::Str(text) = &lit.lit
            {
                note = text.value();
            }
        }
        syn::Meta::Path(_) => {}
    }
    Some(format!(
        "since {}: {}",
        if since.is_empty() { "?" } else { &since },
        note
    ))
}

fn has_attr(attrs: &[Attribute], name: &str) -> bool {
    attrs.iter().any(|a| a.path().is_ident(name))
}

fn is_public(vis: &Visibility) -> bool {
    matches!(vis, Visibility::Public(_))
}

/// The traits in `#[derive(..)]` attributes, by last path segment, each with the `#[cfg(..)]` of the
/// `#[cfg_attr(<predicate>, derive(..))]` it came from (`None` for a plain derive). A conditional
/// derive is an impl a user can name only under its condition (regia_net's `ecs` feature, ADR-8032).
fn derives(attrs: &[Attribute]) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for attr in attrs {
        if attr.path().is_ident("derive") {
            let _ = attr.parse_nested_meta(|meta| {
                if let Some(last) = meta.path.segments.last() {
                    out.push((last.ident.to_string(), None));
                }
                Ok(())
            });
        } else if attr.path().is_ident("cfg_attr") {
            let Ok(parts) = attr
                .parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
            else {
                continue;
            };
            let mut parts = parts.into_iter();
            let Some(predicate) = parts.next() else {
                continue;
            };
            let condition = render(quote!(#[cfg(#predicate)]));
            for inner in parts {
                let syn::Meta::List(list) = inner else {
                    continue;
                };
                if !list.path.is_ident("derive") {
                    continue;
                }
                let _ = list.parse_nested_meta(|meta| {
                    if let Some(last) = meta.path.segments.last() {
                        out.push((last.ident.to_string(), Some(condition.clone())));
                    }
                    Ok(())
                });
            }
        }
    }
    out
}

fn path_attr(attrs: &[Attribute]) -> Option<String> {
    attrs.iter().filter(|a| a.path().is_ident("path")).find_map(|a| {
        let syn::Meta::NameValue(pair) = &a.meta else {
            return None;
        };
        let syn::Expr::Lit(lit) = &pair.value else {
            return None;
        };
        let syn::Lit::Str(text) = &lit.lit else {
            return None;
        };
        Some(text.value())
    })
}

/// `use name;` or `use name as alias;` naming a whole crate: `(name, alias)`. `self`, `crate`,
/// `super` and `_` aliases are not crates.
fn whole_crate(tree: &UseTree) -> Option<(String, String)> {
    let (name, alias) = match tree {
        UseTree::Name(n) => (n.ident.to_string(), n.ident.to_string()),
        UseTree::Rename(r) => (r.ident.to_string(), r.rename.to_string()),
        _ => return None,
    };
    let keyword = |s: &str| matches!(s, "self" | "crate" | "super" | "Self" | "_");
    (!keyword(&name) && !keyword(&alias)).then_some((name, alias))
}

fn flatten_use(tree: &UseTree, prefix: &mut Vec<String>, out: &mut Vec<(Option<String>, Vec<String>)>) {
    match tree {
        UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            flatten_use(&p.tree, prefix, out);
            prefix.pop();
        }
        UseTree::Name(n) => {
            let name = n.ident.to_string();
            let mut path = prefix.clone();
            // `use foo::{self}` binds `foo`.
            if name == "self" {
                let bound = prefix.last().cloned();
                out.push((bound, path));
            } else {
                path.push(name.clone());
                out.push((Some(name), path));
            }
        }
        UseTree::Rename(r) => {
            let alias = r.rename.to_string();
            let mut path = prefix.clone();
            if r.ident != "self" {
                path.push(r.ident.to_string());
            }
            // `pub use foo as _` binds no usable name.
            if alias != "_" {
                out.push((Some(alias), path));
            }
        }
        UseTree::Glob(_) => out.push((None, prefix.clone())),
        UseTree::Group(g) => {
            for item in &g.items {
                flatten_use(item, prefix, out);
            }
        }
    }
}

impl Tree {
    fn load_root(&mut self, lib_rs: &Path) -> Result<()> {
        let dir = lib_rs
            .parent()
            .with_context(|| format!("{} has no parent", lib_rs.display()))?
            .to_owned();
        self.crates_dir = dir.parent().and_then(Path::parent).map(Path::to_path_buf);
        let items = parse_items(lib_rs)?;
        self.modules.push(Module {
            parent: None,
            root: 0,
            name: String::new(),
            public: true,
            hidden: false,
            cfg: Vec::new(),
            items: Vec::new(),
            children: Vec::new(),
            uses: Vec::new(),
        });
        self.load_items(0, items, &dir, &dir)
    }

    /// `dir` is where child modules live. `#[path]` is relative to `path_base`: the folder of the
    /// file, or `dir` inside an inline module.
    fn load_items(&mut self, module: usize, items: Vec<Item>, dir: &Path, path_base: &Path) -> Result<()> {
        for item in items {
            self.load_item(module, item, dir, path_base)?;
        }
        Ok(())
    }

    fn load_item(&mut self, module: usize, item: Item, dir: &Path, path_base: &Path) -> Result<()> {
        if let Item::Use(u) = &item
            && is_public(&u.vis)
            && !test_only(&u.attrs)
            && !doc_hidden(&u.attrs)
            && let Some((krate, alias)) = whole_crate(&u.tree)
            && let Some(lib) = self.sibling_lib(&krate)
        {
            return self.graft(module, &krate, &alias, cfgs(&u.attrs), &lib);
        }
        let info: Option<(String, bool, Vec<Attribute>)> = match &item {
            Item::Mod(m) => {
                if !test_only(&m.attrs) {
                    self.load_module(module, m, dir, path_base)?;
                }
                None
            }
            Item::Use(u) => {
                if !test_only(&u.attrs) {
                    let mut leaves = Vec::new();
                    flatten_use(&u.tree, &mut Vec::new(), &mut leaves);
                    for (name, path) in leaves {
                        self.modules[module].uses.push(UseLeaf {
                            public: is_public(&u.vis),
                            hidden: doc_hidden(&u.attrs),
                            name,
                            path,
                            cfg: cfgs(&u.attrs),
                        });
                    }
                }
                None
            }
            Item::Impl(block) => {
                if !test_only(&block.attrs) && !doc_hidden(&block.attrs) {
                    self.impls.push(ImplBlock {
                        module,
                        cfg: cfgs(&block.attrs),
                        block: block.clone(),
                    });
                }
                None
            }
            Item::Macro(m) => {
                if let Some(name) = &m.ident
                    && has_attr(&m.attrs, "macro_export")
                    && !doc_hidden(&m.attrs)
                    && !test_only(&m.attrs)
                {
                    self.macros.push((name.to_string(), cfgs(&m.attrs)));
                }
                None
            }
            Item::Struct(i) => Some((i.ident.to_string(), is_public(&i.vis), i.attrs.clone())),
            Item::Enum(i) => Some((i.ident.to_string(), is_public(&i.vis), i.attrs.clone())),
            Item::Union(i) => Some((i.ident.to_string(), is_public(&i.vis), i.attrs.clone())),
            Item::Fn(i) => Some((i.sig.ident.to_string(), is_public(&i.vis), i.attrs.clone())),
            Item::Const(i) => Some((i.ident.to_string(), is_public(&i.vis), i.attrs.clone())),
            Item::Static(i) => Some((i.ident.to_string(), is_public(&i.vis), i.attrs.clone())),
            Item::Type(i) => Some((i.ident.to_string(), is_public(&i.vis), i.attrs.clone())),
            Item::Trait(i) => Some((i.ident.to_string(), is_public(&i.vis), i.attrs.clone())),
            _ => None,
        };
        if let Some((name, public, attrs)) = info
            && !test_only(&attrs)
        {
            let id = self.defined.len();
            self.defined.push(Defined {
                name,
                public,
                hidden: doc_hidden(&attrs),
                cfg: cfgs(&attrs),
                item,
            });
            self.modules[module].items.push(id);
        }
        Ok(())
    }

    /// `crates/<krate>/src/lib.rs` beside the scanned crate, for a `moved` crate not already being read.
    fn sibling_lib(&self, krate: &str) -> Option<PathBuf> {
        if !self.moved.contains(krate) || self.grafted.contains(krate) {
            return None;
        }
        let lib = self.crates_dir.as_ref()?.join(krate).join("src").join("lib.rs");
        lib.is_file().then_some(lib)
    }

    /// Reads the sibling crate at `lib` as a public module `alias` of `parent` whose `crate::` is itself.
    fn graft(&mut self, parent: usize, krate: &str, alias: &str, cfg: Vec<String>, lib: &Path) -> Result<()> {
        self.grafted.insert(krate.to_owned());
        let id = self.modules.len();
        self.modules.push(Module {
            parent: Some(parent),
            root: id,
            name: alias.to_owned(),
            public: true,
            hidden: false,
            cfg,
            items: Vec::new(),
            children: Vec::new(),
            uses: Vec::new(),
        });
        self.modules[parent].children.push(id);
        let dir = lib
            .parent()
            .with_context(|| format!("{} has no parent", lib.display()))?
            .to_owned();
        let items = parse_items(lib)?;
        self.load_items(id, items, &dir, &dir)
    }

    fn load_module(&mut self, parent: usize, m: &syn::ItemMod, dir: &Path, path_base: &Path) -> Result<()> {
        let name = m.ident.to_string();
        let id = self.modules.len();
        let root = self.modules[parent].root;
        self.modules.push(Module {
            parent: Some(parent),
            root,
            name: name.clone(),
            public: is_public(&m.vis),
            hidden: doc_hidden(&m.attrs),
            cfg: cfgs(&m.attrs),
            items: Vec::new(),
            children: Vec::new(),
            uses: Vec::new(),
        });
        self.modules[parent].children.push(id);
        if let Some((_, items)) = &m.content {
            let inner = dir.join(&name);
            return self.load_items(id, items.clone(), &inner, &inner);
        }
        let (file, child_dir) = if let Some(path) = path_attr(&m.attrs) {
            let file = path_base.join(path);
            let child = file.parent().map_or_else(|| dir.to_owned(), Path::to_path_buf);
            (file, child)
        } else {
            let flat = dir.join(format!("{name}.rs"));
            let nested = dir.join(&name).join("mod.rs");
            let file = if flat.is_file() { flat } else { nested };
            (file, dir.join(&name))
        };
        let items = parse_items(&file)?;
        let base = file.parent().map_or_else(|| dir.to_owned(), Path::to_path_buf);
        self.load_items(id, items, &child_dir, &base)
    }

    /// Everything `path` may name, seen from `from` (a name can be a module and a function).
    fn resolve(&self, from: usize, path: &[String], visiting: &mut Visiting) -> Vec<Resolution> {
        let Some(first) = path.first() else {
            return Vec::new();
        };
        let (mut module, mut rest, mut anchored) = (from, path, false);
        if first == "crate" || first == "self" {
            module = if first == "crate" {
                self.modules[from].root
            } else {
                from
            };
            rest = &rest[1..];
            anchored = true;
        }
        while rest.first().is_some_and(|s| s == "super") {
            module = self.modules[module].parent.unwrap_or(0);
            rest = &rest[1..];
            anchored = true;
        }
        let Some((head, tail)) = rest.split_first() else {
            return vec![Resolution {
                target: Target::Module(module),
                cfg: self.modules[module].cfg.clone(),
            }];
        };
        let found = self.lookup(module, head, visiting);
        if found.is_empty() {
            return if anchored {
                Vec::new()
            } else {
                vec![Resolution {
                    target: Target::Elsewhere(path.join("::")),
                    cfg: Vec::new(),
                }]
            };
        }
        if tail.is_empty() {
            return found;
        }
        let mut out = Vec::new();
        for resolution in found {
            match resolution.target {
                Target::Module(m) => {
                    let mut inner = vec!["self".to_owned()];
                    inner.extend_from_slice(tail);
                    out.extend(
                        self.resolve(m, &inner, visiting)
                            .into_iter()
                            .map(|r| r.under(&resolution.cfg)),
                    );
                }
                Target::Elsewhere(prefix) => {
                    out.push(Resolution {
                        target: Target::Elsewhere(format!("{prefix}::{}", tail.join("::"))),
                        cfg: resolution.cfg,
                    });
                }
                Target::Item(_) => out.push(Resolution {
                    target: Target::Elsewhere(rest.join("::")),
                    cfg: resolution.cfg,
                }),
            }
        }
        out
    }

    /// `name` as seen inside `module`: items, child modules, `use` names, then globs. `visiting`
    /// stops a `use` cycle (`pub use foo::foo` looks `foo` up again through itself).
    fn lookup(&self, module: usize, name: &str, visiting: &mut Visiting) -> Vec<Resolution> {
        let m = &self.modules[module];
        let mut out: Vec<Resolution> = m
            .items
            .iter()
            .filter(|i| self.defined[**i].name == name)
            .map(|i| Resolution {
                target: Target::Item(*i),
                cfg: self.defined[*i].cfg.clone(),
            })
            .collect();
        out.extend(
            m.children
                .iter()
                .filter(|c| self.modules[**c].name == name)
                .map(|c| Resolution {
                    target: Target::Module(*c),
                    cfg: self.modules[*c].cfg.clone(),
                }),
        );
        let key = (module, name.to_owned());
        if !visiting.insert(key.clone()) {
            return out;
        }
        for leaf in m.uses.iter().filter(|l| l.name.as_deref() == Some(name)) {
            out.extend(
                self.resolve(module, &leaf.path, visiting)
                    .into_iter()
                    .map(|r| r.under(&leaf.cfg)),
            );
        }
        if out.is_empty() {
            for leaf in m.uses.iter().filter(|l| l.name.is_none()) {
                for resolution in self.resolve(module, &leaf.path, visiting) {
                    if let Target::Module(inner) = resolution.target
                        && inner != module
                    {
                        out.extend(
                            self.lookup(inner, name, visiting)
                                .into_iter()
                                .map(|r| r.under(&resolution.cfg).under(&leaf.cfg)),
                        );
                    }
                }
            }
        }
        visiting.remove(&key);
        out
    }

    fn walk_exports(&mut self) {
        let mut stack = HashSet::new();
        self.walk(0, "", &[], &mut stack);
        self.emit_items();
        for (name, cfg) in std::mem::take(&mut self.macros) {
            self.line(
                format!("{name}!"),
                prefixed(&cfg, &format!("macro_rules! {name}")),
            );
        }
    }

    fn walk(&mut self, module: usize, prefix: &str, cfg: &[String], stack: &mut HashSet<usize>) {
        if !stack.insert(module) {
            return;
        }
        let items = self.modules[module].items.clone();
        for id in items {
            let defined = &self.defined[id];
            if defined.public && !defined.hidden {
                let path = join(prefix, &defined.name);
                self.add_item_path(id, path, cfg);
            }
        }
        let children = self.modules[module].children.clone();
        for child in children {
            let c = &self.modules[child];
            if c.public && !c.hidden {
                let name = c.name.clone();
                self.export_module(child, &join(prefix, &name), cfg, stack);
            }
        }
        let leaves: Vec<(Option<String>, Vec<String>, Vec<String>)> = self.modules[module]
            .uses
            .iter()
            .filter(|l| l.public && !l.hidden)
            .map(|l| (l.name.clone(), l.path.clone(), l.cfg.clone()))
            .collect();
        for (name, path, leaf_cfg) in leaves {
            let mut cfg = cfg.to_vec();
            cfg.extend(leaf_cfg);
            let targets = self.resolve(module, &path, &mut Visiting::new());
            match name {
                Some(name) => {
                    let public = join(prefix, &name);
                    for resolution in targets {
                        let mut cfg = cfg.clone();
                        cfg.extend(resolution.cfg);
                        self.export_target(resolution.target, &public, &cfg, stack);
                    }
                }
                None => {
                    for resolution in targets {
                        let mut cfg = cfg.clone();
                        cfg.extend(resolution.cfg);
                        match resolution.target {
                            Target::Module(inner) => self.walk(inner, prefix, &cfg, stack),
                            Target::Elsewhere(from) => {
                                let key = join(prefix, "*");
                                self.line(key, prefixed(&cfg, &format!("pub use {from}::*")));
                            }
                            Target::Item(id) => {
                                let key = join(prefix, "*");
                                let from = self.defined[id].name.clone();
                                self.line(key, prefixed(&cfg, &format!("pub use {from}::*")));
                            }
                        }
                    }
                }
            }
        }
        stack.remove(&module);
    }

    fn export_target(&mut self, target: Target, public: &str, cfg: &[String], stack: &mut HashSet<usize>) {
        match target {
            Target::Item(id) => self.add_item_path(id, public.to_owned(), cfg),
            Target::Module(m) => self.export_module(m, public, cfg, stack),
            Target::Elsewhere(from) => {
                self.line(public.to_owned(), prefixed(cfg, &format!("pub use {from}")));
            }
        }
    }

    fn export_module(&mut self, module: usize, public: &str, cfg: &[String], stack: &mut HashSet<usize>) {
        let mut cfg = cfg.to_vec();
        cfg.extend(self.modules[module].cfg.iter().cloned());
        self.line(
            public.to_owned(),
            prefixed(&cfg, &format!("pub mod {}", last_segment(public))),
        );
        self.walk(module, public, &cfg, stack);
    }

    fn add_item_path(&mut self, id: usize, path: String, cfg: &[String]) {
        self.item_paths
            .entry(id)
            .or_default()
            .insert((path, cfg.to_vec()));
    }

    /// One definition per item at its shortest public path; every other path is an alias line.
    fn emit_items(&mut self) {
        let methods = self.attach_impls();
        let item_paths = std::mem::take(&mut self.item_paths);
        for (id, paths) in &item_paths {
            let mut sorted: Vec<&(String, Vec<String>)> = paths.iter().collect();
            sorted.sort_by_key(|(p, _)| (p.matches("::").count(), p.len(), p.clone()));
            let Some((canonical, _)) = sorted.first() else {
                continue;
            };
            let canonical = canonical.clone();
            for (path, cfg) in sorted {
                let mut conditions = cfg.clone();
                conditions.extend(self.defined[*id].cfg.iter().cloned());
                if path != &canonical {
                    self.line(
                        path.clone(),
                        prefixed(&conditions, &format!("pub use {canonical}")),
                    );
                    continue;
                }
                // One spelling can have several export routes with different conditions.
                // Each needs its definition, rather than an alias back to itself.
                self.emit_item(*id, &canonical, cfg);
                for line in methods.get(id).into_iter().flatten() {
                    let signature = if line.suffix.ends_with(DEPRECATED_SUFFIX) {
                        // Deprecation markers retain their existing `since ..: ..` text.
                        line.signature.clone()
                    } else {
                        let mut cfg = conditions.clone();
                        cfg.extend(line.cfg.iter().cloned());
                        prefixed(&cfg, &line.signature)
                    };
                    self.line(format!("{canonical}{}", line.suffix), signature);
                }
            }
        }
    }

    fn emit_item(&mut self, id: usize, path: &str, cfg: &[String]) {
        let mut cfg = cfg.to_vec();
        cfg.extend(self.defined[id].cfg.iter().cloned());
        let item = self.defined[id].item.clone();
        match &item {
            Item::Struct(s) => self.emit_struct(s, path, &cfg),
            Item::Enum(e) => self.emit_enum(e, path, &cfg),
            Item::Union(u) => {
                let (name, generics) = (&u.ident, &u.generics);
                let wher = &generics.where_clause;
                let signature = render(quote!(pub union #name #generics #wher));
                self.line(path.to_owned(), prefixed(&cfg, &signature));
                self.emit_fields(path, &u.fields.named, &cfg);
            }
            Item::Fn(f) => {
                let signature = fn_signature(&f.sig);
                self.line(path.to_owned(), prefixed(&cfg, &format!("pub {signature}")));
            }
            Item::Const(c) => {
                let (name, ty) = (&c.ident, &c.ty);
                self.line(
                    path.to_owned(),
                    prefixed(&cfg, &render(quote!(pub const #name: #ty))),
                );
            }
            Item::Static(s) => {
                let (mutability, name, ty) = (&s.mutability, &s.ident, &s.ty);
                self.line(
                    path.to_owned(),
                    prefixed(&cfg, &render(quote!(pub static #mutability #name: #ty))),
                );
            }
            Item::Type(t) => {
                let (name, generics, ty) = (&t.ident, &t.generics, &t.ty);
                self.line(
                    path.to_owned(),
                    prefixed(&cfg, &render(quote!(pub type #name #generics = #ty))),
                );
            }
            Item::Trait(t) => self.emit_trait(t, path, &cfg),
            _ => {}
        }
        if let Some(text) = deprecation(item_attrs(&item)) {
            self.line(format!("{path}{DEPRECATED_SUFFIX}"), text);
        }
        for (derived, condition) in derives(item_attrs(&item)) {
            let mut cfg = cfg.clone();
            cfg.extend(condition);
            self.line(
                format!("{path} impl {derived}"),
                prefixed(&cfg, &format!("derive({derived})")),
            );
        }
    }

    fn emit_struct(&mut self, s: &syn::ItemStruct, path: &str, cfg: &[String]) {
        let (name, generics) = (&s.ident, &s.generics);
        let wher = &generics.where_clause;
        let exhaustive = !has_attr(&s.attrs, "non_exhaustive");
        let head = render(quote!(pub struct #name #generics));
        let tail = wher.as_ref().map_or(String::new(), |w| format!(" {}", render(w)));
        let marker = if exhaustive { "" } else { "#[non_exhaustive] " };
        let signature = match &s.fields {
            Fields::Unit => format!("{marker}{head}{tail};"),
            Fields::Unnamed(fields) => {
                let cells: Vec<String> = fields
                    .unnamed
                    .iter()
                    .map(|f| {
                        if is_public(&f.vis) {
                            let ty = &f.ty;
                            render(quote!(pub #ty))
                        } else {
                            "_".to_owned()
                        }
                    })
                    .collect();
                format!("{marker}{head}({}){tail};", cells.join(", "))
            }
            Fields::Named(fields) => {
                let all_public = fields.named.iter().all(|f| is_public(&f.vis));
                if exhaustive && all_public {
                    // A struct literal names every field, so a new field changes this item.
                    let cells: Vec<String> = fields.named.iter().map(field_cell).collect();
                    format!("{head}{tail} {{ {} }}", cells.join(", "))
                } else {
                    self.emit_fields(path, &fields.named, cfg);
                    format!("{marker}{head}{tail} {{ .. }}")
                }
            }
        };
        self.line(path.to_owned(), prefixed(cfg, &signature));
    }

    fn emit_fields<'a>(
        &mut self,
        path: &str,
        fields: impl IntoIterator<Item = &'a syn::Field>,
        cfg: &[String],
    ) {
        for field in fields.into_iter().filter(|f| is_public(&f.vis)) {
            let Some(name) = &field.ident else { continue };
            self.line(format!("{path}.{name}"), prefixed(cfg, &field_cell(field)));
            if let Some(text) = deprecation(&field.attrs) {
                self.line(format!("{path}.{name}{DEPRECATED_SUFFIX}"), text);
            }
        }
    }

    fn emit_enum(&mut self, e: &syn::ItemEnum, path: &str, cfg: &[String]) {
        let (name, generics) = (&e.ident, &e.generics);
        let wher = &generics.where_clause;
        let head = render(quote!(pub enum #name #generics #wher));
        let variants: Vec<(String, String, Option<String>)> = e
            .variants
            .iter()
            .filter(|v| !doc_hidden(&v.attrs) && !test_only(&v.attrs))
            .map(|v| {
                let mut plain = v.clone();
                plain.attrs.clear();
                strip_field_attrs(&mut plain.fields);
                (
                    v.ident.to_string(),
                    prefixed(&cfgs(&v.attrs), &render(&plain)),
                    deprecation(&v.attrs),
                )
            })
            .collect();
        if has_attr(&e.attrs, "non_exhaustive") {
            self.line(
                path.to_owned(),
                prefixed(cfg, &format!("#[non_exhaustive] {head} {{ .. }}")),
            );
            for (name, variant, deprecated) in variants {
                self.line(format!("{path}::{name}"), prefixed(cfg, &variant));
                if let Some(text) = deprecated {
                    self.line(format!("{path}::{name}{DEPRECATED_SUFFIX}"), text);
                }
            }
        } else {
            // A `match` lists every variant, so a new variant changes this item.
            let mut cells = Vec::new();
            for (name, variant, deprecated) in variants {
                cells.push(variant);
                if let Some(text) = deprecated {
                    self.line(format!("{path}::{name}{DEPRECATED_SUFFIX}"), text);
                }
            }
            self.line(
                path.to_owned(),
                prefixed(cfg, &format!("{head} {{ {} }}", cells.join(", "))),
            );
        }
    }

    fn emit_trait(&mut self, t: &syn::ItemTrait, path: &str, cfg: &[String]) {
        let mut required = Vec::new();
        for item in &t.items {
            let attrs = match item {
                TraitItem::Fn(f) => &f.attrs,
                TraitItem::Const(c) => &c.attrs,
                TraitItem::Type(ty) => &ty.attrs,
                _ => continue,
            };
            if test_only(attrs) {
                continue;
            }
            let mut cfg = cfg.to_vec();
            let mut local = Vec::new();
            match item {
                TraitItem::Fn(f) => {
                    local.extend(cfgs(&f.attrs));
                    if doc_hidden(&f.attrs) {
                        continue;
                    }
                    let signature = fn_signature(&f.sig);
                    if f.default.is_none() {
                        required.push(prefixed(&local, &format!("{signature};")));
                    } else {
                        cfg.extend(local);
                        self.line(
                            format!("{path}::{}", f.sig.ident),
                            prefixed(&cfg, &format!("{signature} {{ .. }}")),
                        );
                        if let Some(text) = deprecation(&f.attrs) {
                            self.line(format!("{path}::{}{DEPRECATED_SUFFIX}", f.sig.ident), text);
                        }
                    }
                }
                TraitItem::Const(c) => {
                    let (name, ty) = (&c.ident, &c.ty);
                    let signature = render(quote!(const #name: #ty));
                    if c.default.is_none() {
                        required.push(format!("{signature};"));
                    } else {
                        self.line(
                            format!("{path}::{name}"),
                            prefixed(&cfg, &format!("{signature} = ..")),
                        );
                    }
                }
                TraitItem::Type(ty) => {
                    let mut plain = ty.clone();
                    plain.attrs.clear();
                    plain.default = None;
                    required.push(render(&plain));
                }
                _ => {}
            }
        }
        let (unsafety, name, generics) = (&t.unsafety, &t.ident, &t.generics);
        let (colon, supertraits) = (&t.colon_token, &t.supertraits);
        let wher = &generics.where_clause;
        let head = render(quote!(pub #unsafety trait #name #generics #colon #supertraits #wher));
        let body = required.join(" ");
        let signature = if body.is_empty() {
            format!("{head} {{ .. }}")
        } else {
            format!("{head} {{ {body} .. }}")
        };
        self.line(path.to_owned(), prefixed(cfg, &signature));
    }

    /// Lines of each exported item's impls, with their conditions kept separate until emission.
    fn attach_impls(&self) -> HashMap<usize, Vec<ImplLine>> {
        let is_type = |id: usize| {
            matches!(
                &self.defined[id].item,
                Item::Struct(_) | Item::Enum(_) | Item::Union(_) | Item::Type(_)
            )
        };
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        for (id, defined) in self.defined.iter().enumerate() {
            if is_type(id) {
                by_name.entry(defined.name.as_str()).or_default().push(id);
            }
        }
        let owners = |module: usize, ty: &syn::Type| -> Vec<usize> {
            let syn::Type::Path(path) = ty else {
                return Vec::new();
            };
            let segments: Vec<String> = path.path.segments.iter().map(|s| s.ident.to_string()).collect();
            let targets: BTreeSet<usize> = self
                .resolve(module, &segments, &mut Visiting::new())
                .into_iter()
                .filter_map(|r| match r.target {
                    Target::Item(id) if is_type(id) => Some(id),
                    _ => None,
                })
                .collect();
            if !targets.is_empty() {
                return targets.into_iter().collect();
            }
            let Some(last) = segments.last() else {
                return Vec::new();
            };
            match by_name.get(last.as_str()).map(Vec::as_slice) {
                Some([only]) => vec![*only],
                _ => Vec::new(),
            }
        };
        let mut out: HashMap<usize, Vec<ImplLine>> = HashMap::new();
        for block in &self.impls {
            let impl_block = &block.block;
            let ids = owners(block.module, &impl_block.self_ty);
            if let Some((negative, trait_path, _)) = &impl_block.trait_ {
                if negative.is_some() {
                    continue;
                }
                let name = trait_path
                    .segments
                    .last()
                    .map_or_else(String::new, |s| s.ident.to_string());
                let generics = &impl_block.generics;
                let (self_ty, wher) = (&impl_block.self_ty, &generics.where_clause);
                let trait_tail = trait_path.segments.last();
                let header = render(quote!(impl #generics #trait_tail for #self_ty #wher));
                let line = ImplLine {
                    suffix: format!(" impl {name}"),
                    cfg: block.cfg.clone(),
                    signature: header,
                };
                let trait_owners = || {
                    let segments: Vec<String> =
                        trait_path.segments.iter().map(|s| s.ident.to_string()).collect();
                    self.resolve(block.module, &segments, &mut Visiting::new())
                        .into_iter()
                        .filter_map(|r| match r.target {
                            Target::Item(id) if matches!(&self.defined[id].item, Item::Trait(_)) => Some(id),
                            _ => None,
                        })
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>()
                };
                let ids = if ids.is_empty() { trait_owners() } else { ids };
                for id in ids {
                    out.entry(id).or_default().push(line.clone());
                }
                continue;
            }
            if ids.is_empty() {
                continue;
            }
            let generic = !impl_block.generics.params.is_empty();
            let header = if generic {
                let generics = &impl_block.generics;
                let (self_ty, wher) = (&impl_block.self_ty, &generics.where_clause);
                format!("{}: ", render(quote!(impl #generics #self_ty #wher)))
            } else {
                String::new()
            };
            let mut lines = Vec::new();
            for item in &impl_block.items {
                let (name, attrs, text) = match item {
                    ImplItem::Fn(f) if is_public(&f.vis) => (
                        f.sig.ident.to_string(),
                        &f.attrs,
                        format!("pub {}", fn_signature(&f.sig)),
                    ),
                    ImplItem::Const(c) if is_public(&c.vis) => {
                        let (name, ty) = (&c.ident, &c.ty);
                        (name.to_string(), &c.attrs, render(quote!(pub const #name: #ty)))
                    }
                    _ => continue,
                };
                if test_only(attrs) || doc_hidden(attrs) {
                    continue;
                }
                let mut cfg = block.cfg.clone();
                cfg.extend(cfgs(attrs));
                lines.push(ImplLine {
                    suffix: format!("::{name}"),
                    cfg,
                    signature: format!("{header}{text}"),
                });
                if let Some(note) = deprecation(attrs) {
                    lines.push(ImplLine {
                        suffix: format!("::{name}{DEPRECATED_SUFFIX}"),
                        cfg: Vec::new(),
                        signature: note,
                    });
                }
            }
            for id in ids {
                out.entry(id).or_default().extend(lines.iter().cloned());
            }
        }
        out
    }

    fn line(&mut self, key: String, signature: String) {
        self.lines.insert(ApiLine { key, signature });
    }

    fn lines(&self) -> Vec<ApiLine> {
        self.lines.iter().cloned().collect()
    }
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Struct(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::Const(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        _ => &[],
    }
}

fn strip_field_attrs(fields: &mut Fields) {
    match fields {
        Fields::Named(f) => f.named.iter_mut().for_each(|f| f.attrs.clear()),
        Fields::Unnamed(f) => f.unnamed.iter_mut().for_each(|f| f.attrs.clear()),
        Fields::Unit => {}
    }
}

fn field_cell(field: &syn::Field) -> String {
    let (name, ty) = (&field.ident, &field.ty);
    render(quote!(pub #name: #ty))
}

/// A function signature without argument names or attributes: renaming an argument is not an
/// API change.
fn fn_signature(sig: &syn::Signature) -> String {
    let mut plain = sig.clone();
    for input in &mut plain.inputs {
        match input {
            FnArg::Receiver(receiver) => receiver.attrs.clear(),
            FnArg::Typed(typed) => {
                typed.attrs.clear();
                *typed.pat = Pat::Wild(syn::PatWild {
                    attrs: Vec::new(),
                    underscore_token: syn::token::Underscore::default(),
                });
            }
        }
    }
    render(&plain)
}

fn prefixed(cfg: &[String], text: &str) -> String {
    let mut seen = BTreeSet::new();
    let mut out = String::new();
    for condition in cfg {
        if seen.insert(condition) {
            out.push_str(condition);
            out.push(' ');
        }
    }
    out.push_str(text);
    out
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}::{name}")
    }
}

fn last_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

fn parse_items(file: &Path) -> Result<Vec<Item>> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    match syn::parse_file(&text) {
        Ok(parsed) => Ok(parsed.items),
        Err(err) => bail!("parsing {}: {err}", file.display()),
    }
}

/// The crate root of a workspace member directory.
pub fn lib_rs(root: &Path, dir: &str) -> PathBuf {
    root.join(dir).join("src/lib.rs")
}
