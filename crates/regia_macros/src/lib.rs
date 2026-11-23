// SPDX-License-Identifier: MPL-2.0
//! Procedural macros: `#[derive(Script)]`, `#[derive(BtTask)]`, `#[derive(Replicate)]`,
//! `#[derive(NetMessage)]`, `#[derive(InputAction)]` (ADR-7070) and `#[derive(Asset)]`.
//!
//! Specification: `docs/specs/14-scene-and-framework.md` Part C.
//!
//! `#[derive(Script)]` implements `regia_framework::script::ReflectScript` for a script struct:
//! the constructor the framework calls when it (re)attaches the script to an entity, plus the
//! execution-order group from the optional struct-level `#[script(order = N)]` attribute, and the
//! reflect accessor hot reload snapshots and restores the script's fields through (`specs/20`
//! § 3, ADR-7206). The struct itself must also derive `Reflect`, `Default` and `Clone` (§ B2) —
//! the generated code calls `Default::default()` and views the instance as `PartialReflect`.
//!
//! `#[derive(BtTask)]` implements `regia_ai::task::BtTaskRegistration` for a behavior-tree leaf.
//!
//! `#[derive(Asset)]` implements `regia_asset::data_asset::Asset` for a custom data asset type
//! (`.data.ron`, spec 10 § 2.4, ADR-7232).

use proc_macro::TokenStream;
use proc_macro2::{TokenStream as TokenStream2, TokenTree};
use quote::quote;
use syn::{Data, DeriveInput, LitInt};

/// Longest `order = N` literal (in characters, digit separators `_` not counted) accepted
/// before parsing. A 32-bit binary spelling with `0b` prefix and an `i32` suffix is 37.
const MAX_ORDER_LITERAL_LEN: usize = 40;

/// Deepest delimiter nesting a derive input may have. `syn` parses types and expressions
/// recursively with no depth counter, so a pathological item is refused with a spanned error
/// before it reaches the parser.
const MAX_TOKEN_DEPTH: usize = 64;

/// Most prefix operators (`&`, `*`, `!`, `-`) allowed between two separators in one token stream.
/// `syn` recurses once per prefix operator in a type or expression (`&&&...u8`), and these are
/// punctuation, not groups, so the group depth cap does not bound them.
const MAX_PREFIX_RUN: usize = 128;

/// Rejects token trees nested deeper than `MAX_TOKEN_DEPTH` groups, or with more than
/// `MAX_PREFIX_RUN` prefix operators in a row (a comma, semicolon, `=`, literal or group ends a run).
fn check_depth(tokens: &TokenStream2, depth: usize) -> syn::Result<()> {
    let mut run = 0usize;
    for tree in tokens.clone() {
        match tree {
            TokenTree::Group(group) => {
                run = 0;
                if depth + 1 > MAX_TOKEN_DEPTH {
                    return Err(syn::Error::new(group.span(), "derive input is nested too deeply"));
                }
                check_depth(&group.stream(), depth + 1)?;
            }
            TokenTree::Punct(p) => match p.as_char() {
                '&' | '*' | '!' | '-' => {
                    run += 1;
                    if run > MAX_PREFIX_RUN {
                        return Err(syn::Error::new(
                            p.span(),
                            "derive input has too many prefix operators in a row",
                        ));
                    }
                }
                ',' | ';' | '=' => run = 0,
                _ => {}
            },
            TokenTree::Literal(_) => run = 0,
            TokenTree::Ident(_) => {}
        }
    }
    Ok(())
}

/// Parses derive input after the nesting guard.
fn parse_input(tokens: TokenStream2) -> syn::Result<DeriveInput> {
    check_depth(&tokens, 0)?;
    syn::parse2(tokens)
}

/// Parses `#[script(order = N)]` on the struct. Default `0`. A repeated `script` attribute or a
/// repeated `order` key is an error (the later value would silently win or lose).
fn script_order(attrs: &[syn::Attribute]) -> syn::Result<i32> {
    let mut found: Option<i32> = None;
    let mut seen_attr = false;
    for attr in attrs {
        if !attr.path().is_ident("script") {
            continue;
        }
        if seen_attr {
            return Err(syn::Error::new_spanned(
                attr,
                "duplicate `#[script]` attribute; put every key in one attribute",
            ));
        }
        seen_attr = true;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("order") {
                if found.is_some() {
                    return Err(meta.error("duplicate `order` key"));
                }
                let value = meta.value()?;
                // Bound the literal before `LitInt` parses it: that parse is quadratic in the
                // digit count, and an `i32` never needs more than a few dozen characters.
                let neg: Option<syn::Token![-]> = value.parse()?;
                let raw: proc_macro2::Literal = value.parse()?;
                if raw.to_string().chars().filter(|c| *c != '_').count() > MAX_ORDER_LITERAL_LEN {
                    return Err(meta.error("order must be an i32"));
                }
                let lit: LitInt = syn::parse2(quote!(#neg #raw))?;
                if !matches!(lit.suffix(), "" | "i32") {
                    return Err(meta.error("order must be an i32 literal (suffix `i32` or none)"));
                }
                found = Some(
                    lit.base10_parse::<i32>()
                        .map_err(|e| meta.error(format!("order must be an i32: {e}")))?,
                );
                Ok(())
            } else {
                Err(meta.error("unsupported attribute; expected `order = N`"))
            }
        })?;
    }
    Ok(found.unwrap_or(0))
}

/// Rejects `#[script(..)]` on fields or enum variants: the execution order belongs to the type,
/// so a field-level attribute would otherwise compile and be silently ignored.
fn reject_member_script_attrs(data: &Data) -> syn::Result<()> {
    let misplaced = "`#[script]` applies to the struct, not to its fields or variants";
    let check = |attrs: &[syn::Attribute]| -> syn::Result<()> {
        match attrs.iter().find(|a| a.path().is_ident("script")) {
            Some(attr) => Err(syn::Error::new_spanned(attr, misplaced)),
            None => Ok(()),
        }
    };
    match data {
        Data::Struct(s) => s.fields.iter().try_for_each(|f| check(&f.attrs)),
        Data::Enum(e) => e.variants.iter().try_for_each(|v| {
            check(&v.attrs)?;
            v.fields.iter().try_for_each(|f| check(&f.attrs))
        }),
        Data::Union(u) => u.fields.named.iter().try_for_each(|f| check(&f.attrs)),
    }
}

/// Expansion of `#[derive(Script)]`, separate from the `proc_macro` entry point so it can be
/// unit-tested.
fn expand_script(input: &DeriveInput) -> syn::Result<TokenStream2> {
    if matches!(input.data, Data::Union(_)) {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "Script cannot be derived for a union",
        ));
    }
    reject_member_script_attrs(&input.data)?;
    let order = script_order(&input.attrs)?;
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    Ok(quote! {
        impl #impl_generics regia_framework::script::ReflectScript for #name #ty_generics
        #where_clause
        {
            fn construct() -> ::std::boxed::Box<dyn regia_framework::script::Script>
            where
                Self: Sized,
            {
                ::std::boxed::Box::new(<Self as ::std::default::Default>::default())
            }

            fn order() -> i32 where Self: Sized {
                #order
            }

            fn script_type_path() -> &'static str where Self: Sized {
                ::std::any::type_name::<Self>()
            }

            fn reflect_script(
                script: &dyn regia_framework::script::Script,
            ) -> ::std::option::Option<&dyn regia_framework::script::PartialReflect>
            where
                Self: Sized,
            {
                regia_framework::script::downcast_reflect::<Self>(script)
            }

            fn reflect_script_mut(
                script: &mut dyn regia_framework::script::Script,
            ) -> ::std::option::Option<&mut dyn regia_framework::script::PartialReflect>
            where
                Self: Sized,
            {
                regia_framework::script::downcast_reflect_mut::<Self>(script)
            }
        }
    })
}

/// Implements `ReflectScript` (§ Part C): `construct` builds a default instance the framework
/// boxes as `dyn Script`; `order` is the execution-order group (§ B2), default `0`.
///
/// The generated impl adds no bounds — the type must already implement `Default` and
/// `Reflect` (the reflect accessor views the instance as `PartialReflect`), and the user writes
/// the `impl Script` block by hand, as in § B2's `Door` example.
#[proc_macro_derive(Script, attributes(script))]
pub fn derive_script(input: TokenStream) -> TokenStream {
    expand_script_tokens(input.into())
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// `#[derive(Script)]` on `proc_macro2` tokens: the nesting guard, the parse, then
/// [`expand_script`] (testable outside a macro call).
fn expand_script_tokens(tokens: TokenStream2) -> syn::Result<TokenStream2> {
    expand_script(&parse_input(tokens)?)
}

/// Expansion of `#[derive(BtTask)]`.
fn expand_bt_task(input: &DeriveInput) -> syn::Result<TokenStream2> {
    if matches!(input.data, Data::Union(_)) {
        return Err(syn::Error::new_spanned(
            &input.ident,
            format!(
                "`#[derive(BtTask)]` cannot be derived for the union `{}`",
                input.ident
            ),
        ));
    }
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    Ok(quote! {
        impl #impl_generics regia_ai::task::BtTaskRegistration for #name #ty_generics
        #where_clause
        {
            fn construct() -> ::std::boxed::Box<dyn regia_ai::task::BtTask>
            where
                Self: Sized,
            {
                ::std::boxed::Box::new(<Self as ::std::default::Default>::default())
            }

            fn task_type_path() -> &'static str where Self: Sized {
                ::std::any::type_name::<Self>()
            }
        }
    })
}

/// Implements `regia_ai::task::BtTaskRegistration` for a behavior-tree leaf task: the
/// constructor the tree uses when it resolves a `.bt.ron` leaf (`Default::default()`, boxed as
/// `dyn BtTask`) and the type path used to look the task up by name.
///
/// The type must also implement `Default`; the user writes `impl BtTask for MyTask` by hand
/// (the `tick` logic is not generatable).
#[proc_macro_derive(BtTask)]
pub fn derive_bt_task(input: TokenStream) -> TokenStream {
    expand_bt_task_tokens(input.into())
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// `#[derive(BtTask)]` on `proc_macro2` tokens: the nesting guard, the parse, then
/// [`expand_bt_task`] (testable outside a macro call).
fn expand_bt_task_tokens(tokens: TokenStream2) -> syn::Result<TokenStream2> {
    expand_bt_task(&parse_input(tokens)?)
}

// ---- #[derive(Replicate)] (docs/specs/71-entity-replication.md § 3) ---------------------------

/// How one field travels, from its `#[replicate(...)]` attribute.
enum FieldWire {
    /// `#[replicate(skip)]`: never travels.
    Skip,
    /// A field-codec expression (`regia_net::codec::Plain` without attributes).
    Codec(TokenStream2),
}

/// Reads `#[replicate(...)]` on one field. One attribute per field; a repeated key is an error.
fn field_wire(attrs: &[syn::Attribute]) -> syn::Result<FieldWire> {
    let mut found: Option<FieldWire> = None;
    for attr in attrs {
        if !attr.path().is_ident("replicate") {
            continue;
        }
        if found.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                "duplicate `#[replicate]` attribute; put one key per field",
            ));
        }
        let mut wire: Option<FieldWire> = None;
        attr.parse_nested_meta(|meta| {
            if wire.is_some() {
                return Err(meta.error("one `replicate` key per field"));
            }
            let path = &meta.path;
            if path.is_ident("skip") {
                wire = Some(FieldWire::Skip);
                return Ok(());
            }
            if path.is_ident("max_len") {
                let n: syn::Expr = meta.value()?.parse()?;
                wire = Some(FieldWire::Codec(quote! { regia_net::codec::MaxLen(#n) }));
                return Ok(());
            }
            let mut keys: Vec<(String, syn::Expr)> = Vec::new();
            if meta.input.peek(syn::token::Paren) {
                meta.parse_nested_meta(|inner| {
                    let Some(key) = inner.path.get_ident().map(|i| i.to_string()) else {
                        return Err(inner.error("expected `min`, `max` or `bits`"));
                    };
                    if keys.iter().any(|(k, _)| *k == key) {
                        return Err(inner.error(format!("duplicate `{key}`")));
                    }
                    let v: syn::Expr = inner.value()?.parse()?;
                    keys.push((key, v));
                    Ok(())
                })?;
            }
            let get = |k: &str| keys.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            let allow = |ok: &[&str]| -> syn::Result<()> {
                for (k, _) in &keys {
                    if !ok.contains(&k.as_str()) {
                        return Err(meta.error(format!("unknown key `{k}`")));
                    }
                }
                Ok(())
            };
            let need = |k: &str| {
                get(k).ok_or_else(|| meta.error(format!("missing `{k}`")))
            };
            let codec = if path.is_ident("quantize") {
                allow(&["min", "max", "bits"])?;
                let (min, max, bits) = (need("min")?, need("max")?, need("bits")?);
                let bits = checked_bits(&bits, 1)?;
                quote! { regia_net::codec::Quantize { min: #min, max: #max, bits: #bits } }
            } else if path.is_ident("angle") {
                allow(&["bits"])?;
                let bits = checked_bits(&need("bits")?, 2)?;
                quote! { regia_net::codec::Angle { bits: #bits } }
            } else if path.is_ident("unit_vec") {
                allow(&["bits"])?;
                let bits = checked_bits(&need("bits")?, 2)?;
                quote! { regia_net::codec::UnitVec { bits: #bits } }
            } else if path.is_ident("rotation") {
                allow(&["bits"])?;
                match get("bits") {
                    Some(bits) => {
                        let bits = checked_bits(&bits, 2)?;
                        quote! { regia_net::codec::Rotation { bits: #bits } }
                    }
                    None => quote! { regia_net::codec::Rotation { bits: 10 } },
                }
            } else {
                return Err(meta.error(
                    "expected `skip`, `quantize(..)`, `angle(..)`, `unit_vec(..)`, `rotation(..)` or `max_len = n`",
                ));
            };
            wire = Some(FieldWire::Codec(codec));
            Ok(())
        })?;
        found = Some(wire.ok_or_else(|| syn::Error::new_spanned(attr, "empty `#[replicate]`"))?);
    }
    Ok(found.unwrap_or(FieldWire::Codec(quote! { regia_net::codec::Plain })))
}

/// Refuses an out-of-range integer `bits` width, including parentheses, a cast, or unary `-`.
///
/// `layout()` hashes the `bits` field as stored, while encode and decode clamp. `checked_bits`
/// stores the clamped width so those two stay on the same value. A const or other non-literal
/// is not rejected here (the macro does not evaluate it); it is still clamped at the use site.
/// Rust has no unary `+`; syn rejects that token before this check.
fn check_bits(bits: &syn::Expr, min: u64) -> syn::Result<()> {
    let bad = || syn::Error::new_spanned(bits, format!("`bits` must be in {min}..=32"));
    match literal_bits(bits).map_err(|_| bad())? {
        Some(n) if n >= i128::from(min) && n <= 32 => Ok(()),
        Some(_) => Err(bad()),
        None => Ok(()),
    }
}

/// The integer a `bits` expression writes, when it is a literal. `None` for a const or a call.
fn literal_bits(expr: &syn::Expr) -> syn::Result<Option<i128>> {
    match expr {
        syn::Expr::Group(group) => literal_bits(&group.expr),
        syn::Expr::Paren(paren) => literal_bits(&paren.expr),
        syn::Expr::Cast(cast) => literal_bits(&cast.expr),
        syn::Expr::Unary(unary) => match unary.op {
            // `UnOp` is `*`, `!` and `-` only (`syn` 2.0). `!` and `*` are not integer widths.
            syn::UnOp::Neg(_) => match literal_bits(&unary.expr)? {
                Some(n) => Ok(Some(n.saturating_neg())),
                None => Ok(None),
            },
            _ => Ok(None),
        },
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(lit),
            ..
        }) => {
            // `LitInt`'s decimal conversion walks every digit. An in-range width is a few
            // characters; a longer token is rejected before that conversion.
            if lit.base10_digits().len() > MAX_ORDER_LITERAL_LEN {
                return Err(syn::Error::new_spanned(expr, "`bits` is out of range"));
            }
            lit.base10_parse::<i128>().map(Some)
        }
        _ => Ok(None),
    }
}

/// `bits` stored on the codec: range-checked when it is a literal, and always clamped so
/// `layout()` hashes the width encode and decode use.
fn checked_bits(bits: &syn::Expr, min: u32) -> syn::Result<TokenStream2> {
    check_bits(bits, u64::from(min))?;
    Ok(quote! { (#bits).clamp(#min, 32u32) })
}

/// One field of a struct or variant: its accessor, its name for the layout hash, type and wire.
struct ReplicateField {
    member: syn::Member,
    binding: syn::Ident,
    label: String,
    ty: syn::Type,
    wire: FieldWire,
}

fn replicate_fields(fields: &syn::Fields) -> syn::Result<Vec<ReplicateField>> {
    let mut out = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        let (member, label) = match &f.ident {
            Some(id) => (syn::Member::Named(id.clone()), id.to_string()),
            None => (syn::Member::Unnamed(syn::Index::from(i)), i.to_string()),
        };
        out.push(ReplicateField {
            member,
            binding: quote::format_ident!("__f{}", i),
            label,
            ty: f.ty.clone(),
            wire: field_wire(&f.attrs)?,
        });
    }
    Ok(out)
}

/// `Self { a: .., b: .. }`, `Self(.., ..)` or `Self` from per-field value expressions.
fn construct(
    path: TokenStream2,
    fields: &syn::Fields,
    values: &[TokenStream2],
    rf: &[ReplicateField],
) -> TokenStream2 {
    match fields {
        syn::Fields::Named(_) => {
            let names = rf.iter().map(|f| &f.member);
            quote! { #path { #( #names: #values ),* } }
        }
        syn::Fields::Unnamed(_) => quote! { #path ( #( #values ),* ) },
        syn::Fields::Unit => quote! { #path },
    }
}

fn layout_of(rf: &[ReplicateField]) -> Vec<TokenStream2> {
    rf.iter()
        .map(|f| {
            let label = &f.label;
            let ty = &f.ty;
            match &f.wire {
                FieldWire::Skip => quote! { h.write_str(#label); h.write_str("skip"); },
                FieldWire::Codec(c) => quote! {
                    h.write_str(#label);
                    <_ as regia_net::codec::FieldCodec<#ty>>::layout(&#c, h);
                },
            }
        })
        .collect()
}

/// Types of every field that travels (not `skip`), across all variants.
fn encoded_field_types(data: &Data) -> syn::Result<Vec<TokenStream2>> {
    let mut out = Vec::new();
    let mut add = |fields: &syn::Fields| -> syn::Result<()> {
        for f in fields {
            if !matches!(field_wire(&f.attrs)?, FieldWire::Skip) {
                let ty = &f.ty;
                out.push(quote! { #ty });
            }
        }
        Ok(())
    };
    match data {
        Data::Struct(s) => add(&s.fields)?,
        Data::Enum(e) => e.variants.iter().try_for_each(|v| add(&v.fields))?,
        Data::Union(_) => {}
    }
    Ok(out)
}

/// True when `ident` appears anywhere in `tokens`.
fn mentions_ident(tokens: TokenStream2, ident: &syn::Ident) -> bool {
    tokens.into_iter().any(|t| match t {
        TokenTree::Ident(i) => i == *ident,
        TokenTree::Group(g) => mentions_ident(g.stream(), ident),
        _ => false,
    })
}

/// Expansion of `#[derive(Replicate)]`.
fn expand_replicate(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let name_str = name.to_string();
    // `#[replicate]` is a field attribute; on the type it would be silently ignored.
    if let Some(attr) = input.attrs.iter().find(|a| a.path().is_ident("replicate")) {
        return Err(syn::Error::new_spanned(
            attr,
            "`#[replicate]` goes on fields, not on the type",
        ));
    }
    let mut generics = input.generics.clone();
    let encoded = encoded_field_types(&input.data)?;
    for p in generics.type_params_mut() {
        if encoded.iter().any(|ty| mentions_ident(ty.clone(), &p.ident)) {
            p.bounds.push(syn::parse_quote!(regia_net::codec::ReplicateCodec));
        }
    }
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let body = match &input.data {
        Data::Struct(data) => {
            let rf = replicate_fields(&data.fields)?;
            let mut encode = Vec::new();
            let mut decode = Vec::new();
            let mut eq = Vec::new();
            let mut enc_delta = Vec::new();
            let mut dec_delta = Vec::new();
            let mut keep = Vec::new();
            for f in &rf {
                let m = &f.member;
                match &f.wire {
                    FieldWire::Skip => {
                        decode.push(quote! { ::core::default::Default::default() });
                        dec_delta.push(quote! { ::core::clone::Clone::clone(&base.#m) });
                        keep.push(quote! { self.#m = ::core::clone::Clone::clone(&local.#m); });
                    }
                    FieldWire::Codec(c) => {
                        encode.push(quote! {
                            regia_net::codec::FieldCodec::encode(&#c, &self.#m, w, cx);
                        });
                        decode.push(quote! { regia_net::codec::FieldCodec::decode(&#c, r, cx)? });
                        eq.push(quote! {
                            && regia_net::codec::FieldCodec::eq(&#c, &self.#m, &other.#m, cx)
                        });
                        enc_delta.push(quote! {
                            if regia_net::codec::FieldCodec::eq(&#c, &self.#m, &base.#m, cx) {
                                w.write_bool(false);
                            } else {
                                w.write_bool(true);
                                regia_net::codec::FieldCodec::encode(&#c, &self.#m, w, cx);
                            }
                        });
                        dec_delta.push(quote! {
                            if r.read_bool()? {
                                regia_net::codec::FieldCodec::decode(&#c, r, cx)?
                            } else {
                                ::core::clone::Clone::clone(&base.#m)
                            }
                        });
                    }
                }
            }
            let full = construct(quote! { Self }, &data.fields, &decode, &rf);
            let delta = construct(quote! { Self }, &data.fields, &dec_delta, &rf);
            let layout = layout_of(&rf);
            quote! {
                fn encode(&self, w: &mut regia_net::bits::BitWriter, cx: &regia_net::codec::CodecContext) {
                    let _ = (&w, &cx);
                    #(#encode)*
                }
                fn decode(
                    r: &mut regia_net::bits::BitReader,
                    cx: &regia_net::codec::CodecContext,
                ) -> ::core::result::Result<Self, regia_net::bits::DecodeError> {
                    let _ = (&r, &cx);
                    ::core::result::Result::Ok(#full)
                }
                fn wire_eq(&self, other: &Self, cx: &regia_net::codec::CodecContext) -> bool {
                    let _ = (&other, &cx);
                    true #(#eq)*
                }
                fn encode_delta(
                    &self,
                    base: &Self,
                    w: &mut regia_net::bits::BitWriter,
                    cx: &regia_net::codec::CodecContext,
                ) {
                    let _ = (&base, &w, &cx);
                    #(#enc_delta)*
                }
                fn decode_delta(
                    base: &Self,
                    r: &mut regia_net::bits::BitReader,
                    cx: &regia_net::codec::CodecContext,
                ) -> ::core::result::Result<Self, regia_net::bits::DecodeError> {
                    let _ = (&base, &r, &cx);
                    ::core::result::Result::Ok(#delta)
                }
                fn layout(h: &mut regia_net::codec::LayoutHasher) {
                    h.write_str(#name_str);
                    #(#layout)*
                }
                fn keep_local(&mut self, local: &Self) {
                    let _ = &local;
                    #(#keep)*
                }
            }
        }
        Data::Enum(data) => {
            let mut enc_arms = Vec::new();
            let mut dec_arms = Vec::new();
            let mut eq_arms = Vec::new();
            let mut layout = Vec::new();
            for (vi, v) in data.variants.iter().enumerate() {
                if v.attrs.iter().any(|a| a.path().is_ident("replicate")) {
                    return Err(syn::Error::new_spanned(
                        v,
                        "`#[replicate]` goes on fields, not on variants",
                    ));
                }
                let vname = &v.ident;
                let vlabel = vname.to_string();
                let tag = vi as u64;
                let rf = replicate_fields(&v.fields)?;
                let bindings: Vec<&syn::Ident> = rf.iter().map(|f| &f.binding).collect();
                let pattern = match &v.fields {
                    syn::Fields::Named(_) => {
                        let names = rf.iter().map(|f| &f.member);
                        quote! { Self::#vname { #( #names: #bindings ),* } }
                    }
                    syn::Fields::Unnamed(_) => quote! { Self::#vname ( #( #bindings ),* ) },
                    syn::Fields::Unit => quote! { Self::#vname },
                };
                let encs = rf.iter().map(|f| {
                    let b = &f.binding;
                    match &f.wire {
                        FieldWire::Skip => quote! { let _ = #b; },
                        FieldWire::Codec(c) => {
                            quote! { regia_net::codec::FieldCodec::encode(&#c, #b, w, cx); }
                        }
                    }
                });
                let others: Vec<syn::Ident> =
                    (0..rf.len()).map(|i| quote::format_ident!("__g{}", i)).collect();
                let other_pattern = match &v.fields {
                    syn::Fields::Named(_) => {
                        let names = rf.iter().map(|f| &f.member);
                        quote! { Self::#vname { #( #names: #others ),* } }
                    }
                    syn::Fields::Unnamed(_) => quote! { Self::#vname ( #( #others ),* ) },
                    syn::Fields::Unit => quote! { Self::#vname },
                };
                let eqs = rf.iter().zip(&others).map(|(f, o)| {
                    let b = &f.binding;
                    match &f.wire {
                        FieldWire::Skip => quote! { && { let _ = (#b, #o); true } },
                        FieldWire::Codec(c) => {
                            quote! { && regia_net::codec::FieldCodec::eq(&#c, #b, #o, cx) }
                        }
                    }
                });
                eq_arms.push(quote! { (#pattern, #other_pattern) => true #(#eqs)*, });
                enc_arms.push(quote! {
                    #pattern => {
                        w.write_varint(#tag);
                        #(#encs)*
                    }
                });
                let decs: Vec<TokenStream2> = rf
                    .iter()
                    .map(|f| match &f.wire {
                        FieldWire::Skip => quote! { ::core::default::Default::default() },
                        FieldWire::Codec(c) => {
                            quote! { regia_net::codec::FieldCodec::decode(&#c, r, cx)? }
                        }
                    })
                    .collect();
                let build = construct(quote! { Self::#vname }, &v.fields, &decs, &rf);
                dec_arms.push(quote! { #tag => #build, });
                let fl = layout_of(&rf);
                layout.push(quote! { h.write_str(#vlabel); #(#fl)* });
            }
            let empty = data.variants.is_empty();
            let encode_body = if empty {
                quote! { match *self {} }
            } else {
                quote! { match self { #(#enc_arms)* } }
            };
            let eq_body = if empty {
                quote! { match *self {} }
            } else if data.variants.len() == 1 {
                quote! { match (self, other) { #(#eq_arms)* } }
            } else {
                quote! { match (self, other) { #(#eq_arms)* _ => false } }
            };
            quote! {
                fn encode(&self, w: &mut regia_net::bits::BitWriter, cx: &regia_net::codec::CodecContext) {
                    let _ = (&w, &cx);
                    #encode_body
                }
                fn wire_eq(&self, other: &Self, cx: &regia_net::codec::CodecContext) -> bool {
                    let _ = (&other, &cx);
                    #eq_body
                }
                fn decode(
                    r: &mut regia_net::bits::BitReader,
                    cx: &regia_net::codec::CodecContext,
                ) -> ::core::result::Result<Self, regia_net::bits::DecodeError> {
                    let _ = &cx;
                    ::core::result::Result::Ok(match r.read_varint()? {
                        #(#dec_arms)*
                        _ => return ::core::result::Result::Err(
                            regia_net::bits::DecodeError::Invalid("enum variant"),
                        ),
                    })
                }
                fn layout(h: &mut regia_net::codec::LayoutHasher) {
                    h.write_str(#name_str);
                    #(#layout)*
                }
            }
        }
        Data::Union(u) => {
            return Err(syn::Error::new_spanned(
                u.union_token,
                "`#[derive(Replicate)]` supports structs and enums",
            ));
        }
    };
    Ok(quote! {
        impl #impl_generics regia_net::codec::ReplicateCodec for #name #ty_generics #where_clause {
            #body
        }
    })
}

/// Implements `regia_net::codec::ReplicateCodec` for a struct or enum
/// (`docs/specs/71-entity-replication.md` § 3): a field-wise encoder with one change bit per field,
/// so a change to one field sends that field only.
///
/// Field attributes: `#[replicate(skip)]`, `#[replicate(quantize(min = a, max = b, bits = n))]`
/// (`f32`), `#[replicate(angle(bits = n))]` (`f32` radians), `#[replicate(unit_vec(bits = n))]`
/// (`Vec3`), `#[replicate(rotation(bits = n))]` (`Quat`, default 10 bits) and
/// `#[replicate(max_len = n)]` (`String` / `Vec`). The type must also implement `Clone`; skipped
/// fields need `Default`.
#[proc_macro_derive(Replicate, attributes(replicate))]
pub fn derive_replicate(input: TokenStream) -> TokenStream {
    expand_replicate_tokens(input.into())
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// `#[derive(Replicate)]` on `proc_macro2` tokens (testable outside a macro call).
fn expand_replicate_tokens(tokens: TokenStream2) -> syn::Result<TokenStream2> {
    expand_replicate(&parse_input(tokens)?)
}

// ---- #[derive(NetMessage)] (docs/specs/70-net-model-authority-messages.md § 6.1) --------------

/// Highest `#[net(version = n)]` accepted (the version is a `u32` in the protocol hash).
const MAX_NET_VERSION: u64 = u32::MAX as u64;

/// Longest `#[net(id = "..")]` string, in bytes.
const MAX_NET_ID_LEN: usize = 256;

/// The struct-level `#[net(...)]` keys of one message type.
#[derive(Default)]
struct NetAttrs {
    direction: Option<(&'static str, &'static str)>,
    channel: Option<&'static str>,
    rate: Option<(u32, u32)>,
    always_admit: bool,
    align_to_view: bool,
    skip_predicting_owner: bool,
    id: Option<String>,
    version: Option<u32>,
    validate: Option<syn::Path>,
    manual_codec: bool,
}

/// Reads a `key = "text"` value.
fn net_str(meta: &syn::meta::ParseNestedMeta) -> syn::Result<syn::LitStr> {
    meta.value()?.parse()
}

/// Reads an optional `= true` / `= false` after a flag key; a bare key is `true`.
fn net_flag(meta: &syn::meta::ParseNestedMeta) -> syn::Result<bool> {
    if meta.input.peek(syn::Token![=]) {
        let b: syn::LitBool = meta.value()?.parse()?;
        Ok(b.value)
    } else {
        Ok(true)
    }
}

/// Parses `"count/per_ticks"` (both at least 1).
fn parse_rate(lit: &syn::LitStr) -> syn::Result<(u32, u32)> {
    let text = lit.value();
    let bad = || {
        syn::Error::new(
            lit.span(),
            "`rate` is \"count/per_ticks\", both whole numbers of at least 1",
        )
    };
    if text.len() > 32 {
        return Err(bad());
    }
    let (a, b) = text.split_once('/').ok_or_else(bad)?;
    let count: u32 = a.trim().parse().map_err(|_| bad())?;
    let per: u32 = b.trim().parse().map_err(|_| bad())?;
    if count == 0 || per == 0 {
        return Err(bad());
    }
    Ok((count, per))
}

/// Reads every `#[net(...)]` attribute on the type. A repeated key is an error.
fn net_attrs(attrs: &[syn::Attribute]) -> syn::Result<NetAttrs> {
    let mut out = NetAttrs::default();
    let mut seen: Vec<String> = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("net") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            let Some(key) = meta.path.get_ident().map(|i| i.to_string()) else {
                return Err(meta.error("expected a `net` key"));
            };
            if seen.contains(&key) {
                return Err(meta.error(format!("duplicate `{key}`")));
            }
            seen.push(key.clone());
            match key.as_str() {
                "direction" => {
                    let lit = net_str(&meta)?;
                    out.direction = Some(match lit.value().as_str() {
                        "to_server" => ("ToServer", "ToServerMessage"),
                        "to_owner" => ("ToOwner", "ToOwnerMessage"),
                        "broadcast" => ("Broadcast", "BroadcastMessage"),
                        _ => {
                            return Err(syn::Error::new(
                                lit.span(),
                                "`direction` is \"to_server\", \"to_owner\" or \"broadcast\"",
                            ));
                        }
                    });
                }
                "channel" => {
                    let lit = net_str(&meta)?;
                    out.channel = Some(match lit.value().as_str() {
                        "reliable_ordered" => "ReliableOrdered",
                        "reliable_unordered" => "ReliableUnordered",
                        "unreliable" => "Unreliable",
                        "unreliable_sequenced" => "UnreliableSequenced",
                        _ => {
                            return Err(syn::Error::new(
                                lit.span(),
                                "`channel` is \"reliable_ordered\", \"reliable_unordered\", \"unreliable\" or \"unreliable_sequenced\"",
                            ));
                        }
                    });
                }
                "rate" => out.rate = Some(parse_rate(&net_str(&meta)?)?),
                "always_admit" => out.always_admit = net_flag(&meta)?,
                "align_to_view" => out.align_to_view = net_flag(&meta)?,
                "skip_predicting_owner" => out.skip_predicting_owner = net_flag(&meta)?,
                "id" => {
                    let lit = net_str(&meta)?;
                    let v = lit.value();
                    if v.is_empty() || v.len() > MAX_NET_ID_LEN || v.chars().any(char::is_control) {
                        return Err(syn::Error::new(
                            lit.span(),
                            "`id` is a non-empty string of at most 256 bytes without control characters",
                        ));
                    }
                    out.id = Some(v);
                }
                "version" => {
                    let lit: LitInt = meta.value()?.parse()?;
                    if lit.base10_digits().len() > MAX_ORDER_LITERAL_LEN {
                        return Err(syn::Error::new(lit.span(), "`version` is too long"));
                    }
                    let v: u64 = lit.base10_parse()?;
                    if v == 0 || v > MAX_NET_VERSION {
                        return Err(syn::Error::new(lit.span(), "`version` is a u32 of at least 1"));
                    }
                    out.version = Some(v as u32);
                }
                "validate" => {
                    // A path (`validate = Self::check`) or the same path in a string.
                    let value = meta.value()?;
                    out.validate = Some(if value.peek(syn::LitStr) {
                        let lit: syn::LitStr = value.parse()?;
                        lit.parse()?
                    } else {
                        value.parse()?
                    });
                }
                "codec" => {
                    let lit = net_str(&meta)?;
                    out.manual_codec = match lit.value().as_str() {
                        "derive" => false,
                        "manual" => true,
                        _ => {
                            return Err(syn::Error::new(lit.span(), "`codec` is \"derive\" or \"manual\""));
                        }
                    };
                }
                _ => {
                    return Err(meta.error(
                        "expected `direction`, `channel`, `rate`, `always_admit`, `align_to_view`, `skip_predicting_owner`, `id`, `version`, `validate` or `codec`",
                    ));
                }
            }
            Ok(())
        })?;
    }
    Ok(out)
}

/// Expansion of `#[derive(NetMessage)]`.
fn expand_net_message(input: &DeriveInput) -> syn::Result<TokenStream2> {
    if let Data::Union(u) = &input.data {
        return Err(syn::Error::new_spanned(
            u.union_token,
            "`#[derive(NetMessage)]` supports structs and enums",
        ));
    }
    let attrs = net_attrs(&input.attrs)?;
    let Some((direction, marker)) = attrs.direction else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(NetMessage)]` needs `#[net(direction = \"to_server\" | \"to_owner\" | \"broadcast\")]`",
        ));
    };
    let name = &input.ident;
    // A generic message is a message for the parameters that make it one: the supertraits go in
    // the where clause, so the impl never claims more than the type gives.
    let mut generics = input.generics.clone();
    if !generics.params.is_empty() {
        generics.make_where_clause().predicates.push(syn::parse_quote! {
            Self: regia_net::codec::ReplicateCodec + ::core::marker::Send + ::core::marker::Sync + 'static
        });
    }
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let direction = quote::format_ident!("{}", direction);
    let marker = quote::format_ident!("{}", marker);
    let mut items = vec![quote! {
        const DIRECTION: regia_net::message::Direction = regia_net::message::Direction::#direction;
    }];
    if let Some(channel) = attrs.channel {
        let channel = quote::format_ident!("{}", channel);
        items.push(quote! {
            const CHANNEL: regia_net::message::Channel = regia_net::message::Channel::#channel;
        });
    }
    if let Some((count, per)) = attrs.rate {
        items.push(quote! {
            const RATE_LIMIT: ::core::option::Option<regia_net::settings::RateLimit> =
                ::core::option::Option::Some(regia_net::settings::RateLimit { count: #count, per_ticks: #per });
        });
    }
    for (on, konst) in [
        (attrs.always_admit, "ALWAYS_ADMIT"),
        (attrs.align_to_view, "ALIGN_TO_VIEW"),
        (attrs.skip_predicting_owner, "SKIP_PREDICTING_OWNER"),
    ] {
        if on {
            let konst = quote::format_ident!("{}", konst);
            items.push(quote! { const #konst: bool = true; });
        }
    }
    if let Some(v) = attrs.version {
        items.push(quote! { const VERSION: u32 = #v; });
    }
    // The id: the attribute, else the module path plus the type name for a non-generic type
    // (what `type_name` reports today, but fixed by the language rather than by the compiler).
    // A generic type keeps the trait default, whose path includes its parameters.
    let id = match &attrs.id {
        Some(id) => Some(quote! { #id }),
        None if input.generics.params.is_empty() => {
            let plain = syn::ext::IdentExt::unraw(name).to_string();
            Some(quote! { ::core::concat!(::core::module_path!(), "::", #plain) })
        }
        None => None,
    };
    if let Some(id) = id {
        items.push(quote! {
            fn message_id() -> &'static str {
                #id
            }
        });
    }
    if let Some(path) = &attrs.validate {
        items.push(quote! {
            fn validate(
                &self,
                cx: &regia_net::message::ValidateContext,
            ) -> regia_net::message::Verdict {
                #path(self, cx)
            }
        });
    }
    let codec = if attrs.manual_codec {
        TokenStream2::new()
    } else {
        expand_replicate(input)?
    };
    Ok(quote! {
        #codec
        impl #impl_generics regia_net::message::NetMessage for #name #ty_generics #where_clause {
            #(#items)*
        }
        impl #impl_generics regia_net::message::#marker for #name #ty_generics #where_clause {}
    })
}

/// Implements `regia_net::message::NetMessage` (and the marker trait of its direction) for a
/// struct or enum (`docs/specs/70-net-model-authority-messages.md` § 6.1), plus its
/// `regia_net::codec::ReplicateCodec`: the fields travel in declaration order through the
/// codec `#[derive(Replicate)]` writes, with the same `#[replicate(..)]` field attributes, so the
/// bytes equal a hand-written impl that encodes the fields in order.
///
/// Type attribute `#[net(..)]` keys: `direction = "to_server" | "to_owner" | "broadcast"`
/// (required), `channel = "reliable_ordered" | "reliable_unordered" | "unreliable" |
/// "unreliable_sequenced"`, `rate = "count/per_ticks"`, the flags `always_admit`,
/// `align_to_view` and `skip_predicting_owner`, `id = ".."` (the stable message id; default:
/// module path plus type name), `version = n` (default 1), `validate = path` (a
/// `fn(&Self, &ValidateContext) -> Verdict`) and `codec = "manual"` (the type implements
/// `ReplicateCodec` itself, for example with `#[derive(Replicate)]`).
#[proc_macro_derive(NetMessage, attributes(net, replicate))]
pub fn derive_net_message(input: TokenStream) -> TokenStream {
    expand_net_message_tokens(input.into())
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// `#[derive(NetMessage)]` on `proc_macro2` tokens (testable outside a macro call).
fn expand_net_message_tokens(tokens: TokenStream2) -> syn::Result<TokenStream2> {
    expand_net_message(&parse_input(tokens)?)
}

/// The `ActionKind` variants `#[action(kind = ..)]` accepts.
const ACTION_KINDS: [&str; 4] = ["Button", "Axis1D", "Axis2D", "Axis3D"];

/// Longest action name `#[action(name = "..")]` accepts, in bytes.
const MAX_ACTION_NAME_LEN: usize = 128;

/// Parses `#[action(name = "..", kind = Kind)]` on one variant: the name (default: the variant's
/// identifier) and the `ActionKind` variant (default `Button`). A repeated attribute or key is an error.
fn action_attrs(variant: &syn::Variant) -> syn::Result<(String, syn::Ident)> {
    let mut name: Option<String> = None;
    let mut kind: Option<syn::Ident> = None;
    let mut seen_attr = false;
    for attr in &variant.attrs {
        if !attr.path().is_ident("action") {
            continue;
        }
        if seen_attr {
            return Err(syn::Error::new_spanned(
                attr,
                "duplicate `#[action]` attribute; put every key in one attribute",
            ));
        }
        seen_attr = true;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                if name.is_some() {
                    return Err(meta.error("duplicate `name` key"));
                }
                let lit: syn::LitStr = meta.value()?.parse()?;
                let value = lit.value();
                if value.is_empty() || value.len() > MAX_ACTION_NAME_LEN {
                    return Err(meta.error("an action name is 1 to 128 bytes"));
                }
                name = Some(value);
                Ok(())
            } else if meta.path.is_ident("kind") {
                if kind.is_some() {
                    return Err(meta.error("duplicate `kind` key"));
                }
                let ident: syn::Ident = meta.value()?.parse()?;
                if !ACTION_KINDS.iter().any(|k| ident == k) {
                    return Err(syn::Error::new_spanned(
                        &ident,
                        "kind must be one of Button, Axis1D, Axis2D, Axis3D",
                    ));
                }
                kind = Some(ident);
                Ok(())
            } else {
                Err(meta.error("unsupported attribute; expected `name = \"..\"` or `kind = Kind`"))
            }
        })?;
    }
    let name = name.unwrap_or_else(|| variant.ident.to_string());
    let kind = kind.unwrap_or_else(|| syn::Ident::new("Button", proc_macro2::Span::call_site()));
    Ok((name, kind))
}

/// Expansion of `#[derive(InputAction)]` (`docs/specs/11` § 7, ADR-7070).
fn expand_input_action(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "InputAction can only be derived for a fieldless enum",
        ));
    };
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "InputAction cannot be derived for a generic enum",
        ));
    }
    if data.variants.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "InputAction needs at least one variant",
        ));
    }
    let mut seen: Vec<String> = Vec::new();
    let mut idents = Vec::new();
    let mut names = Vec::new();
    let mut kinds = Vec::new();
    for variant in &data.variants {
        if !matches!(variant.fields, syn::Fields::Unit) {
            return Err(syn::Error::new_spanned(
                variant,
                "InputAction variants carry no fields",
            ));
        }
        let (name, kind) = action_attrs(variant)?;
        if seen.contains(&name) {
            return Err(syn::Error::new_spanned(
                variant,
                format!("two variants are named {name:?}"),
            ));
        }
        seen.push(name.clone());
        idents.push(variant.ident.clone());
        names.push(name);
        kinds.push(kind);
    }
    let ty = &input.ident;
    Ok(quote! {
        impl regia_input::InputAction for #ty {
            const ALL: &'static [Self] = &[#(Self::#idents),*];

            fn name(self) -> &'static str {
                match self {
                    #(Self::#idents => #names,)*
                }
            }

            fn kind(self) -> regia_input::ActionKind {
                match self {
                    #(Self::#idents => regia_input::ActionKind::#kinds,)*
                }
            }
        }
    })
}

/// Implements `regia_input::InputAction` for a fieldless enum (`docs/specs/11` § 7): one action per
/// variant, named after it unless `#[action(name = "..")]` renames it, of kind `Button` unless
/// `#[action(kind = Axis2D)]` (`Button`, `Axis1D`, `Axis2D` or `Axis3D`) says otherwise. The enum must
/// also be `Copy`; the generated code names `regia_input`, so that crate must be a dependency.
#[proc_macro_derive(InputAction, attributes(action))]
pub fn derive_input_action(input: TokenStream) -> TokenStream {
    expand_input_action_tokens(input.into())
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// `#[derive(InputAction)]` on `proc_macro2` tokens (testable outside a macro call).
fn expand_input_action_tokens(tokens: TokenStream2) -> syn::Result<TokenStream2> {
    expand_input_action(&parse_input(tokens)?)
}

#[cfg(test)]
mod input_action_tests {
    use super::*;

    fn expand(tokens: TokenStream2) -> Result<String, String> {
        expand_input_action_tokens(tokens)
            .map(|t| t.to_string())
            .map_err(|e| e.to_string())
    }

    #[test]
    fn names_and_kinds_come_from_variants_and_attributes() {
        let out = expand(quote!(
            enum A {
                Jump,
                #[action(name = "fire", kind = Axis1D)]
                Fire,
            }
        ))
        .expect("expands");
        assert!(out.contains("\"Jump\""), "{out}");
        assert!(out.contains("\"fire\""), "{out}");
        assert!(out.contains("ActionKind :: Axis1D"), "{out}");
        assert!(out.contains("ActionKind :: Button"), "{out}");
    }

    #[test]
    fn bad_inputs_are_spanned_errors() {
        assert!(
            expand(quote!(
                struct S;
            ))
            .is_err()
        );
        assert!(
            expand(quote!(
                enum E {}
            ))
            .is_err()
        );
        assert!(
            expand(quote!(
                enum E {
                    A(u8),
                }
            ))
            .is_err()
        );
        assert!(
            expand(quote!(
                enum E<T> {
                    A,
                    B(T),
                }
            ))
            .is_err()
        );
        assert!(
            expand(quote!(
                enum E {
                    #[action(kind = Wheel)]
                    A,
                }
            ))
            .is_err()
        );
        assert!(
            expand(quote!(
                enum E {
                    #[action(name = "x")]
                    A,
                    #[action(name = "x")]
                    B,
                }
            ))
            .is_err()
        );
        assert!(
            expand(quote!(
                enum E {
                    #[action(name = "")]
                    A,
                }
            ))
            .is_err()
        );
        assert!(
            expand(quote!(
                enum E {
                    #[action(oops = 1)]
                    A,
                }
            ))
            .is_err()
        );
        assert!(
            expand(quote!(
                enum E {
                    #[action(name = "a")]
                    #[action(kind = Axis2D)]
                    A,
                }
            ))
            .is_err()
        );
    }
}

// ---- #[derive(Asset)] (docs/specs/10-core.md § 2.4, ADR-7232) ------------------------------------

/// Expansion of `#[derive(Asset)]`: `impl regia_asset::data_asset::Asset for T {}`. The trait's
/// bounds (`Reflect`, `FromReflect`, `TypePath`, `GetTypeRegistration`, `Default`) carry the
/// checks, so a missing derive is a compile error at the use site.
fn expand_asset(input: &DeriveInput) -> syn::Result<TokenStream2> {
    if matches!(input.data, Data::Union(_)) {
        return Err(syn::Error::new_spanned(
            &input.ident,
            format!(
                "`#[derive(Asset)]` cannot be derived for the union `{}`",
                input.ident
            ),
        ));
    }
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    Ok(quote! {
        impl #impl_generics regia_asset::data_asset::Asset for #name #ty_generics #where_clause {}
    })
}

/// `#[derive(Asset)]`: declares a reflected type as a custom data asset stored in `.data.ron`
/// files (`docs/specs/10-core.md` § 2.4, `docs/formats/data-ron.md`). The type also derives
/// `Reflect` and `Default`; register it with
/// `regia_asset::data_asset::RegisterDataAssetExt::register_data_asset`, or add `Asset` to its
/// `#[reflect(...)]` list with `regia_asset::data_asset::ReflectAsset` in scope.
#[proc_macro_derive(Asset)]
pub fn derive_asset(input: TokenStream) -> TokenStream {
    expand_asset_tokens(input.into())
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// `#[derive(Asset)]` on `proc_macro2` tokens (testable outside a macro call).
fn expand_asset_tokens(tokens: TokenStream2) -> syn::Result<TokenStream2> {
    expand_asset(&parse_input(tokens)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proc_macro2::{Delimiter, Group};
    use syn::parse_quote;

    #[test]
    fn asset_expands_to_the_marker_impl_and_refuses_unions() {
        let out = expand_asset_tokens(quote!(
            struct WeaponStats<T: Clone>
            where
                T: Default,
            {
                damage: T,
            }
        ))
        .map(|t| t.to_string())
        .unwrap_or_else(|e| panic!("expands: {e}"));
        assert!(
            out.contains("impl < T : Clone > regia_asset :: data_asset :: Asset for WeaponStats < T > where T : Default"),
            "{out}"
        );
        let err = expand_asset_tokens(quote!(union U { a: u32 })).map(|t| t.to_string());
        assert!(err.is_err_and(|e| e.to_string().contains("union")));
    }

    fn replicate(input: DeriveInput) -> Result<String, String> {
        expand_replicate(&input)
            .map(|t| t.to_string())
            .map_err(|e| e.to_string())
    }

    #[test]
    fn replicate_expands_field_codecs() {
        let out = replicate(parse_quote! {
            struct Health {
                #[replicate(quantize(min = 0.0, max = 100.0, bits = 10))]
                hp: f32,
                #[replicate(skip)]
                local: u8,
                #[replicate(rotation)]
                aim: Quat,
                name: String,
            }
        })
        .expect("expands");
        assert!(out.contains("Quantize"), "{out}");
        assert!(out.contains("Rotation"), "{out}");
        assert!(out.contains("Plain"), "{out}");
        assert!(out.contains("decode_delta"), "{out}");
        assert!(out.contains("keep_local"), "{out}");
        let out = replicate(parse_quote! {
            enum Pose { Idle, Run { #[replicate(angle(bits = 8))] heading: f32 }, Jump(u8) }
        })
        .expect("enum expands");
        assert!(out.contains("Angle"), "{out}");
    }

    #[test]
    fn replicate_rejects_bad_attributes() {
        for bad in [
            replicate(parse_quote! { struct A { #[replicate(quantize(min = 0.0, bits = 4))] x: f32 } }),
            replicate(parse_quote! { struct A { #[replicate(spin)] x: f32 } }),
            replicate(parse_quote! { struct A { #[replicate(skip)] #[replicate(skip)] x: f32 } }),
            replicate(parse_quote! { struct A { #[replicate(angle(bits = 4, bits = 5))] x: f32 } }),
            replicate(parse_quote! { struct A { #[replicate(angle(bits = 1))] x: f32 } }),
            replicate(parse_quote! { struct A { #[replicate(angle(bits = 64))] x: f32 } }),
            replicate(
                parse_quote! { struct A { #[replicate(quantize(min = 0.0, max = 1.0, bits = 0))] x: f32 } },
            ),
            replicate(parse_quote! { struct A { #[replicate(unit_vec(bits = 33))] x: f32 } }),
            replicate(parse_quote! { struct A { #[replicate(rotation(bits = 1))] x: f32 } }),
            replicate(parse_quote! { enum E { #[replicate(skip)] A } }),
            replicate(parse_quote! { union U { a: u8 } }),
        ] {
            assert!(bad.is_err(), "{bad:?}");
        }
    }

    /// In-range integer-literal `bits` still expand; the bounds are the codec clamps.
    #[test]
    fn replicate_accepts_in_range_bits() {
        let out = replicate(parse_quote! {
            struct A {
                #[replicate(quantize(min = 0.0, max = 1.0, bits = 1))] a: f32,
                #[replicate(angle(bits = 32))] b: f32,
                #[replicate(unit_vec(bits = 2))] c: f32,
                #[replicate(rotation(bits = 10))] d: f32,
                #[replicate(quantize(min = 0.0, max = 1.0, bits = (1)))] e: f32,
                #[replicate(angle(bits = (-(-32))))] f: f32,
                #[replicate(rotation(bits = (10)))] g: f32,
                #[replicate(unit_vec(bits = 8 as u32))] h: f32,
                #[replicate(angle(bits = 0x20))] i: f32,
                #[replicate(unit_vec(bits = 32u32))] j: f32,
            }
        });
        assert!(out.is_ok(), "{out:?}");
    }

    /// Parentheses, unary `-`, hex, suffixes and `as` casts are still integer widths. An
    /// out-of-range spelling must be rejected: the bare-literal check does not see it, and the
    /// codecs would otherwise clamp on the wire while `layout()` hashes the raw value.
    /// Rust has no unary `+` (`syn::UnOp` is `*`, `!` and `-`); that token fails to parse.
    #[test]
    fn replicate_rejects_disguised_out_of_range_bits() {
        for bad in [
            replicate(parse_quote! { struct A { #[replicate(angle(bits = (64)))] x: f32 } }),
            replicate(parse_quote! { struct A { #[replicate(angle(bits = ((1))))] x: f32 } }),
            replicate(
                parse_quote! { struct A { #[replicate(quantize(min = 0.0, max = 1.0, bits = 0x40))] x: f32 } },
            ),
            replicate(parse_quote! { struct A { #[replicate(unit_vec(bits = -1))] x: f32 } }),
            replicate(parse_quote! { struct A { #[replicate(rotation(bits = 0x1))] x: f32 } }),
            replicate(parse_quote! { struct A { #[replicate(angle(bits = 64 as u32))] x: f32 } }),
            replicate(parse_quote! { struct A { #[replicate(angle(bits = --40))] x: f32 } }),
            replicate(parse_quote! { struct A { #[replicate(unit_vec(bits = 64u32))] x: f32 } }),
        ] {
            let err = bad.expect_err("disguised bits must be rejected");
            assert!(err.contains("bits"), "{err}");
        }
    }

    /// `layout()` hashes the `bits` field as stored. The emitted codec must clamp that field
    /// so a const wider than the wire does not hash a different width from the one encode uses.
    #[test]
    fn replicate_clamps_bits_before_they_are_hashed() {
        let out = replicate(parse_quote! {
            struct A { #[replicate(angle(bits = WIDTH))] x: f32 }
        })
        .expect("const bits still expand");
        assert!(
            out.contains("clamp"),
            "raw bits are hashed by layout(); the emitted codec must clamp: {out}"
        );
    }

    fn script(input: DeriveInput) -> Result<String, String> {
        expand_script(&input)
            .map(|t| t.to_string())
            .map_err(|e| e.to_string())
    }

    #[test]
    fn struct_level_order_is_emitted() {
        let out = script(parse_quote! {
            #[script(order = -100)]
            struct Door { open: bool }
        })
        .expect("expands");
        assert!(out.contains("100"), "{out}");
        assert!(script(parse_quote! { #[script(order = 7i32)] struct A { x: u8 } }).is_ok());
    }

    #[test]
    fn field_level_script_attribute_is_an_error() {
        let err = script(parse_quote! {
            struct Door { #[script(order = -100)] open: bool }
        })
        .expect_err("field attribute rejected");
        assert!(err.contains("not to its fields"), "{err}");
    }

    #[test]
    fn variant_level_script_attribute_is_an_error() {
        let err = script(parse_quote! {
            enum Phase { #[script(order = 1)] Boot, Play }
        })
        .expect_err("variant attribute rejected");
        assert!(err.contains("not to its fields"), "{err}");
    }

    #[test]
    fn duplicate_attribute_or_key_is_an_error() {
        let dup_attr = script(parse_quote! {
            #[script()]
            #[script(order = -100)]
            struct A { x: u8 }
        })
        .expect_err("duplicate attribute");
        assert!(dup_attr.contains("duplicate"), "{dup_attr}");
        let dup_key = script(parse_quote! {
            #[script(order = 1, order = -100)]
            struct A { x: u8 }
        })
        .expect_err("duplicate key");
        assert!(dup_key.contains("duplicate"), "{dup_key}");
    }

    #[test]
    fn suffix_other_than_i32_is_an_error() {
        let err = script(parse_quote! { #[script(order = 256u8)] struct A { x: u8 } })
            .expect_err("suffix rejected");
        assert!(err.contains("i32"), "{err}");
    }

    #[test]
    fn raw_ident_unit_and_generic_types_expand() {
        assert!(script(parse_quote! { struct r#move { step: f32 } }).is_ok());
        assert!(script(parse_quote! { struct Mutator; }).is_ok());
        let out = script(parse_quote! {
            struct Heal<T: Default> where T: Clone { amount: T }
        })
        .expect("generic expands");
        assert!(out.contains("Heal < T >"), "{out}");
        assert!(out.contains("impl < T : Default >"), "{out}");
        assert!(!out.contains("REGIA_SCRIPT_ORDER"), "{out}");
    }

    #[test]
    fn union_is_an_error() {
        let err = script(parse_quote! { union U { a: u32, b: f32 } }).expect_err("union");
        assert!(err.contains("union"), "{err}");
    }

    #[test]
    fn bt_task_uses_default_and_type_name() {
        let out = expand_bt_task(&parse_quote! { struct Wait<'a, const N: usize>(&'a u8); })
            .expect("struct expansion")
            .to_string();
        assert!(out.contains("Default > :: default ()"), "{out}");
        assert!(out.contains("type_name :: < Self >"), "{out}");
        assert!(out.contains("Wait < 'a , N >"), "{out}");
    }

    #[test]
    fn bt_task_union_is_a_spanned_error_naming_the_type() {
        let err = expand_bt_task_tokens(parse_quote! { union Unsupported { number: u32 } })
            .expect_err("union rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("cannot be derived for the union `Unsupported`"),
            "{msg}"
        );
    }

    // Audit 2: the `order` literal is bounded before it is parsed.

    fn order_of(src: &str) -> syn::Result<i32> {
        let input: DeriveInput = syn::parse_str(src)?;
        script_order(&input.attrs)
    }

    #[test]
    fn order_parses_and_defaults() {
        assert_eq!(order_of("struct A { x: i32 }").ok(), Some(0));
        assert_eq!(order_of("#[script(order = 7)] struct A { x: i32 }").ok(), Some(7));
    }

    #[test]
    fn order_rejects_overlong_literal() {
        let digits = "9".repeat(100_000);
        let src = format!("#[script(order = {digits})] struct A {{ x: i32 }}");
        assert!(order_of(&src).is_err());
    }

    #[test]
    fn order_accepts_negative() {
        assert_eq!(
            order_of("#[script(order = -42)] struct A { x: i32 }").ok(),
            Some(-42)
        );
    }

    #[test]
    fn order_accepts_binary_and_separated_literals() {
        let bin = format!("0b1{}", "0".repeat(30));
        assert_eq!(
            order_of(&format!("#[script(order = {bin})] struct A {{ x: i32 }}")).ok(),
            Some(1 << 30)
        );
        let min = format!("-0b1{}", "0".repeat(31));
        assert_eq!(
            order_of(&format!("#[script(order = {min})] struct A {{ x: i32 }}")).ok(),
            Some(i32::MIN)
        );
        let sep = format!("1{}", "_".repeat(32));
        assert_eq!(
            order_of(&format!("#[script(order = {sep})] struct A {{ x: i32 }}")).ok(),
            Some(1)
        );
    }

    #[test]
    fn replicate_on_the_type_is_an_error() {
        let err = replicate(parse_quote! {
            #[replicate(skip)]
            struct Secrets { token: String }
        })
        .expect_err("type-level attribute");
        assert!(err.contains("not on the type"), "{err}");
    }

    #[test]
    fn replicate_bounds_only_encoded_type_params() {
        let out = replicate(parse_quote! {
            struct Comp<T> { #[replicate(skip)] _p: PhantomData<T>, hp: u32 }
        })
        .expect("expands");
        assert!(!out.contains("ReplicateCodec for Comp < T > where"), "{out}");
        assert!(!out.contains("T : regia_net"), "{out}");
        let out = replicate(parse_quote! { struct Wrap<T> { inner: T } }).expect("expands");
        assert!(out.contains("T : regia_net :: codec :: ReplicateCodec"), "{out}");
    }

    #[test]
    fn order_rejects_out_of_range() {
        assert!(order_of("#[script(order = 4294967296)] struct A { x: i32 }").is_err());
    }

    #[test]
    fn raw_identifier_name_expands_to_valid_tokens() {
        // Audit 2 checked the hidden order const's name; that const is gone (audit 1), so the
        // check is that a raw-identifier type still expands and names the type as written.
        let out = script(parse_quote! { struct r#move { step: f32 } }).expect("raw ident expands");
        assert!(out.contains("for r#move"), "{out}");
    }

    // Audit 3: nesting guard and the token-level expanders.

    fn nested(depth: usize) -> TokenStream2 {
        let mut inner = quote!(bool);
        for _ in 0..depth {
            inner = TokenStream2::from(TokenTree::Group(Group::new(Delimiter::None, inner)));
        }
        quote!(struct S { field: #inner })
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_crash() {
        assert!(expand_script_tokens(nested(1_000)).is_err());
        assert!(expand_bt_task_tokens(nested(1_000)).is_err());
        assert!(expand_script_tokens(nested(8)).is_ok());
    }

    #[test]
    fn long_prefix_operator_chain_is_an_error_not_a_crash() {
        let refs = TokenStream2::from_iter((0..5_000).map(|_| quote!(&)));
        let ptrs = TokenStream2::from_iter((0..5_000).map(|_| quote!(*const)));
        let a = quote!(struct S { v: #refs u8 });
        let b = quote!(struct S { v: #ptrs u8 });
        assert!(expand_script_tokens(a.clone()).is_err());
        assert!(expand_bt_task_tokens(a).is_err());
        assert!(expand_script_tokens(b).is_err());
        // Ordinary reference and pointer fields still expand.
        assert!(
            expand_script_tokens(quote!(
                struct S {
                    a: &'static str,
                    b: *const u8,
                }
            ))
            .is_ok()
        );
    }

    #[test]
    fn raw_ident_struct_does_not_panic() {
        let out = expand_script_tokens(quote!(
            struct r#type {
                a: u8,
            }
        ));
        assert!(out.is_ok_and(|t| t.to_string().contains("ReflectScript for r#type")));
    }

    /// Hot reload (spec 20 § 3, ADR-7206): the derive emits both reflect accessors, each a
    /// type-checked downcast through the framework helper.
    #[test]
    fn the_derive_emits_the_reflect_accessors() {
        let out = expand_script_tokens(quote!(
            struct S {
                a: u8,
            }
        ))
        .map(|t| t.to_string())
        .unwrap_or_default();
        assert!(out.contains("fn reflect_script ("), "{out}");
        assert!(out.contains("fn reflect_script_mut ("), "{out}");
        assert!(out.contains("downcast_reflect :: < Self >"), "{out}");
        assert!(out.contains("downcast_reflect_mut :: < Self >"), "{out}");
    }

    #[test]
    fn order_attribute_is_read() {
        let out = expand_script_tokens(quote!(
            #[script(order = -3)]
            struct S {
                a: u8,
            }
        ));
        assert!(out.is_ok_and(|t| t.to_string().contains("3i32")));
    }

    #[test]
    fn bad_order_is_an_error() {
        // Audit 3 also asserted that a unit struct is an error; audit 1 deliberately allows unit
        // scripts (`raw_ident_unit_and_generic_types_expand`), ADR-1313.
        assert!(
            expand_script_tokens(quote!(
                #[script(order = 99999999999)]
                struct S {
                    a: u8,
                }
            ))
            .is_err()
        );
    }

    // #[derive(NetMessage)] (ADR-2415).

    fn net_message(input: DeriveInput) -> Result<String, String> {
        expand_net_message(&input)
            .map(|t| t.to_string())
            .map_err(|e| e.to_string())
    }

    #[test]
    fn net_message_expands_trait_marker_and_codec() {
        let out = net_message(parse_quote! {
            #[net(direction = "to_server", channel = "unreliable", rate = "10/60", always_admit)]
            struct Fire { power: u32, #[replicate(quantize(min = 0.0, max = 1.0, bits = 8))] spread: f32 }
        })
        .expect("expands");
        assert!(out.contains("Direction :: ToServer"), "{out}");
        assert!(out.contains("Channel :: Unreliable"), "{out}");
        assert!(out.contains("count : 10u32 , per_ticks : 60u32"), "{out}");
        assert!(out.contains("const ALWAYS_ADMIT : bool = true"), "{out}");
        assert!(out.contains("ToServerMessage for Fire"), "{out}");
        assert!(out.contains("ReplicateCodec for Fire"), "{out}");
        assert!(out.contains("Quantize"), "{out}");
        assert!(out.contains("module_path"), "{out}");
        assert!(!out.contains("const VERSION"), "{out}");
        let out = net_message(parse_quote! {
            #[net(direction = "broadcast", id = "game.boom", version = 3, codec = "manual")]
            #[net(validate = Self::check, skip_predicting_owner = false)]
            enum Boom { Small, Big(u8) }
        })
        .expect("enum expands");
        assert!(out.contains("BroadcastMessage for Boom"), "{out}");
        assert!(out.contains("\"game.boom\""), "{out}");
        assert!(out.contains("const VERSION : u32 = 3u32"), "{out}");
        assert!(out.contains("Self :: check (self , cx)"), "{out}");
        assert!(!out.contains("ReplicateCodec"), "{out}");
        assert!(!out.contains("SKIP_PREDICTING_OWNER"), "{out}");
    }

    #[test]
    fn net_message_id_of_generic_and_raw_types() {
        // A generic type keeps the trait default (its type path names the parameters).
        let out = net_message(parse_quote! {
            #[net(direction = "to_owner")]
            struct Wrap<T> { inner: T }
        })
        .expect("generic expands");
        assert!(!out.contains("message_id"), "{out}");
        assert!(
            out.contains("ToOwnerMessage for Wrap < T > where Self :"),
            "{out}"
        );
        // A raw identifier is named without its `r#`, as `type_name` names it.
        let out = net_message(parse_quote! { #[net(direction = "to_owner")] struct r#move { n: u8 } })
            .expect("raw expands");
        assert!(out.contains("\"move\""), "{out}");
    }

    #[test]
    fn net_message_rejects_bad_attributes() {
        for bad in [
            net_message(parse_quote! { struct A { x: u8 } }),
            net_message(parse_quote! { #[net(direction = "sideways")] struct A { x: u8 } }),
            net_message(parse_quote! { #[net(direction = "to_server", channel = "carrier")] struct A; }),
            net_message(parse_quote! { #[net(direction = "to_server", rate = "10")] struct A; }),
            net_message(parse_quote! { #[net(direction = "to_server", rate = "0/60")] struct A; }),
            net_message(parse_quote! { #[net(direction = "to_server", rate = "1/x")] struct A; }),
            net_message(parse_quote! { #[net(direction = "to_server", version = 0)] struct A; }),
            net_message(parse_quote! { #[net(direction = "to_server", version = 4294967296)] struct A; }),
            net_message(parse_quote! { #[net(direction = "to_server", id = "")] struct A; }),
            net_message(parse_quote! { #[net(direction = "to_server", id = "a\nb")] struct A; }),
            net_message(parse_quote! { #[net(direction = "to_server", codec = "magic")] struct A; }),
            net_message(parse_quote! { #[net(direction = "to_server", speed = 3)] struct A; }),
            net_message(
                parse_quote! { #[net(direction = "to_server")] #[net(direction = "to_owner")] struct A; },
            ),
            net_message(parse_quote! { #[net(direction = "to_server")] union U { a: u8 } }),
            net_message(
                parse_quote! { #[net(direction = "to_server")] struct A { #[replicate(spin)] x: f32 } },
            ),
        ] {
            assert!(bad.is_err(), "{bad:?}");
        }
        let long = "x".repeat(MAX_NET_ID_LEN + 1);
        let src = format!("#[net(direction = \"to_server\", id = \"{long}\")] struct A;");
        let input: DeriveInput = syn::parse_str(&src).expect("parses");
        assert!(expand_net_message(&input).is_err());
    }

    #[test]
    fn net_message_deep_nesting_is_an_error_not_a_crash() {
        let mut inner = quote!(u8);
        for _ in 0..1_000 {
            inner = TokenStream2::from(TokenTree::Group(Group::new(Delimiter::None, inner)));
        }
        assert!(
            expand_net_message_tokens(quote!(#[net(direction = "to_server")] struct S { f: #inner }))
                .is_err()
        );
    }
}

#[cfg(test)]
mod compiler_fixtures;
