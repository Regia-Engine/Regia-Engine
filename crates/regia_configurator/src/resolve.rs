// SPDX-License-Identifier: MPL-2.0
//! `resolve(product, picks) → resolution` (plan `interfaces.md` § 9; the steps of the catalogue
//! research § 9.6 and the resolution format of the option graph schema), ported so the configurator
//! works offline (ADR-2534, ADR-2558). The published `resolve_vectors.jsonl` must match exactly.
//!
//! The rules, in order:
//!
//! 1. A product whose status is `not_orderable` or `duplicate` never resolves
//!    (`product_not_orderable`, with the status as `value`).
//! 2. Group names and value labels match ignoring ASCII case; the result carries the record's own
//!    spelling. A pick that chooses nothing (an empty label; an empty list for a multi group) is no
//!    pick. Two spellings of one group that pick different values are `duplicate_value`. A group
//!    the product lacks is `unknown_group`. For an axis pick: a list is
//!    `wrong_pick_type`, a label the axis lacks `unknown_value`, a value no longer offered
//!    `value_unavailable`. Any of these refuses here.
//! 3. Find the variant: exactly one live, priced variant must match every axis pick (a variant
//!    whose own axis value is unavailable is not offered). None is `not_offered`; more than one
//!    leaves the build open, with one `axis_open` per axis that still differs, listed in `open`.
//! 4. Check the add-ons: one picked for a variant it does not apply to is `addon_not_applicable`.
//!    Then each group that applies, in record order: a required group needs a value
//!    (`addon_required`, also for an empty list); a single group takes one label and a multi group a list
//!    (`wrong_pick_type`); then per label `unknown_value`, `value_unavailable`, `duplicate_value`.
//! 5. Pricing is the reference implementation's step and is **not** ported: Regia never prices, so
//!    `cost` is not in this resolution and `no_price` is never answered (an unpriced variant is
//!    simply not offered, step 3).
//! 6. The cart lines: a Shopify cart takes the variant, then every chosen add-on value in record
//!    order by its numeric code (`no_cart_code` without one); a manual supplier gets one
//!    made-to-order line, whose `attrs` hold each add-on's code (`no_cart_code` without one).
//! 7. The statement line: `<title> | <group>: <label> | ...`, every axis of the variant in record
//!    order, then every chosen add-on group in record order (a multi group's labels joined by
//!    `, `). A refused build has no statement.
//! 8. The geometry key: see [`geometry_key`]. A refused build has none.
//!
//! Every refusal carries a stable lowercase code ([`RESOLVE_REASONS`], book/agents/configurator.md).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::graph::{
    Addon, CartMode, GroupRef, OptionValue, ProductRecord, ProductStatus, Variant, is_numeric_code,
};

/// One pick: a label for a single-choice group, a list of labels for a multi group.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Pick {
    /// A single-choice group's label (an empty string means nothing is picked).
    One(String),
    /// A multi group's labels.
    Many(Vec<String>),
}

impl Pick {
    /// Whether the pick chooses nothing (an empty label or an empty list).
    pub fn is_empty(&self) -> bool {
        match self {
            Self::One(s) => s.is_empty(),
            Self::Many(v) => v.is_empty(),
        }
    }

    /// The chosen labels.
    pub fn labels(&self) -> Vec<&str> {
        match self {
            Self::One(s) if s.is_empty() => Vec::new(),
            Self::One(s) => vec![s.as_str()],
            Self::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// Option group name -> pick. Sorted, so a build's picks serialize in a stable order.
pub type Picks = BTreeMap<String, Pick>;

/// Every resolution reason code (the option graph schema's list), with what it means.
pub const RESOLVE_REASONS: &[(&str, &str)] = &[
    (
        "product_not_orderable",
        "The product is not_orderable or a duplicate listing (`value` holds the status); nothing resolves.",
    ),
    (
        "unknown_group",
        "A pick names an option group the product does not have.",
    ),
    ("unknown_value", "A pick names a label its group does not list."),
    (
        "value_unavailable",
        "The supplier no longer offers the value (available = false).",
    ),
    (
        "wrong_pick_type",
        "A multi group was given a single label, or any other group a list.",
    ),
    (
        "duplicate_value",
        "A multi group's list names a value twice, or two spellings of one group pick different values.",
    ),
    (
        "not_offered",
        "No live priced variant matches the axis picks: this combination is not offered.",
    ),
    (
        "axis_open",
        "More than one variant matches: this axis (`group`) still has to be picked; `open` lists them all.",
    ),
    (
        "addon_required",
        "A required add-on that applies to the variant has no value picked.",
    ),
    (
        "addon_not_applicable",
        "An add-on was picked for a variant it does not apply to.",
    ),
    (
        "no_price",
        "In the shared format for the reference implementation only: Regia never prices, so it never answers this.",
    ),
    (
        "no_cart_code",
        "A chosen add-on value has no usable code: not numeric for a Shopify cart, empty for a manual line.",
    ),
];

/// The one build caveat code, separate from [`RESOLVE_REASONS`]: the build path lists it in
/// `build().resolution.reasons` for a truncated listing (spec 42 § 3.2) while the resolution
/// stays `orderable`. It is not a refusal — `resolve()` never emits it — and the shared
/// option-graph schema's `reason.code` enum does not list it, so it stays out of the
/// refusal-code table consumers validate against.
pub const BUILD_CAVEAT_INCOMPLETE: &str = "incomplete";

/// One refusal, with the group and value it is about. Serializes as the option graph schema's
/// `reason` object: `{code, group?, value?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refusal {
    /// A code from [`RESOLVE_REASONS`].
    pub code: &'static str,
    /// The option group, when the refusal is about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The value label, when the refusal is about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// A sentence for a person (not part of the shared format; `Resolution::to_agent_json` adds it).
    #[serde(skip_serializing)]
    pub message: String,
}

impl Refusal {
    fn new(code: &'static str, group: Option<&str>, value: Option<&str>, message: String) -> Self {
        Self {
            code,
            group: group.map(str::to_owned),
            value: value.map(str::to_owned),
            message,
        }
    }
}

/// One Shopify cart line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CartLine {
    /// A variant id or an add-on value code.
    pub id: String,
    /// Always 1 for the lines `resolve()` writes.
    pub quantity: u32,
}

/// The made-to-order line of a manual supplier (a person places it on the supplier's site).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MadeToOrderLine {
    /// The variant's SKU.
    pub style_sku: Option<String>,
    /// The part of the variant key before `:` (the whole key when there is no `:`).
    pub variation_id: String,
    /// The part of the variant key after `:`.
    pub metal_code: Option<String>,
    /// Add-on group -> the chosen value's code; a multi group holds a list of codes.
    pub attrs: BTreeMap<String, Pick>,
    /// The statement line.
    pub statement: String,
}

impl MadeToOrderLine {
    /// The single-line text a build stores (`jewelry.build/1` keeps the line as a string):
    /// `<style_sku> | variation <id> | metal <code> | <group>: <code> | ...`, a multi group's codes
    /// joined with `+`.
    pub fn text(&self) -> String {
        let mut parts = vec![self.style_sku.clone().unwrap_or_else(|| "-".to_owned())];
        parts.push(format!("variation {}", self.variation_id));
        if let Some(m) = &self.metal_code {
            parts.push(format!("metal {m}"));
        }
        for (group, code) in &self.attrs {
            parts.push(format!("{group}: {}", code.labels().join("+")));
        }
        parts.join(" | ")
    }
}

/// What `resolve()` answers: the option graph schema's `resolution` object, without `cost`
/// (staff-only; Regia never prices).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Resolution {
    /// The product.
    pub product_key: String,
    /// Whether the picks name exactly one sold combination.
    pub orderable: bool,
    /// The variant's `house_variant_id`, when orderable.
    pub variant_key: Option<String>,
    /// The variant's SKU, when orderable.
    pub sku: Option<String>,
    /// The resolved picks in the record's spelling, when orderable: every axis of the variant
    /// (implied ones too), every chosen single add-on, and every multi add-on that applies (a list,
    /// empty when nothing is chosen). Empty when refused.
    pub picks: Picks,
    /// How the order is placed.
    #[serde(serialize_with = "ser_cart_mode")]
    pub cart_mode: CartMode,
    /// Shopify lines (empty for a manual supplier and for a refused build).
    pub cart_lines: Vec<CartLine>,
    /// The manual line (manual suppliers, when orderable).
    pub made_to_order_line: Option<MadeToOrderLine>,
    /// Every dimension named (module docs, step 7); `None` when refused.
    pub statement: Option<String>,
    /// The key 3D assets are stored under ([`geometry_key`]); `None` when refused.
    pub geometry_key: Option<String>,
    /// The axes still open when more than one variant matches.
    pub open: Vec<String>,
    /// Why it is refused, in the order found (empty when orderable).
    pub reasons: Vec<Refusal>,
}

fn ser_cart_mode<S: serde::Serializer>(mode: &CartMode, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(mode.as_str())
}

impl Resolution {
    /// The shared format: exactly the option graph schema's `resolution` fields, without `cost`.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// What `configurator.resolve` answers: [`Resolution::to_json`] plus `details`, each reason
    /// with a sentence for a person.
    pub fn to_agent_json(&self) -> Value {
        let mut out = self.to_json();
        let details: Vec<Value> = self
            .reasons
            .iter()
            .map(|r| {
                let mut d = serde_json::to_value(r).unwrap_or(Value::Null);
                d["message"] = Value::String(r.message.clone());
                d
            })
            .collect();
        out["details"] = Value::Array(details);
        out
    }

    /// The `resolution` object of a `jewelry.build/1` (no `product_key`, `picks`, `geometry_key` or
    /// `open`: the build holds the first three itself; the made-to-order line as its single-line text).
    pub fn to_build_json(&self) -> Value {
        json!({
            "orderable": self.orderable,
            "variant_key": self.variant_key,
            "sku": self.sku,
            "cart_mode": self.cart_mode.as_str(),
            "cart_lines": self.cart_lines,
            "made_to_order_line": self.made_to_order_line.as_ref().map(MadeToOrderLine::text),
            "statement": self.statement,
            "reasons": self.reasons,
        })
    }
}

/// Check every pick's group, shape, label and availability in the record's exact spelling (the
/// runtime canonicalizes first, [`ProductRecord::canonical_picks`]); refusals in group-name order.
pub(crate) fn check_picks(product: &ProductRecord, picks: &Picks) -> Vec<Refusal> {
    let mut out = Vec::new();
    for (group, pick) in picks {
        let Some(at) = product.group(group) else {
            out.push(Refusal::new(
                "unknown_group",
                Some(group),
                None,
                format!("`{}` has no option group `{group}`", product.product_key),
            ));
            continue;
        };
        let multi = matches!(at, GroupRef::Addon(i) if product.addons[i].multi);
        let shape_ok = matches!(pick, Pick::Many(_)) == multi;
        if !shape_ok {
            out.push(Refusal::new(
                "wrong_pick_type",
                Some(group),
                None,
                if multi {
                    format!("`{group}` is a multi group: pick a list of distinct labels")
                } else {
                    format!("`{group}` takes one label, not a list")
                },
            ));
            continue;
        }
        let values = product.group_values(at);
        let mut seen = std::collections::BTreeSet::new();
        for label in pick.labels() {
            match values.iter().find(|v| v.label == label) {
                None => out.push(Refusal::new(
                    "unknown_value",
                    Some(group),
                    Some(label),
                    format!("`{group}` has no value `{label}`"),
                )),
                Some(v) if !v.available => out.push(Refusal::new(
                    "value_unavailable",
                    Some(group),
                    Some(label),
                    format!("`{group}={label}` is no longer offered"),
                )),
                Some(_) if !seen.insert(label) => out.push(Refusal::new(
                    "duplicate_value",
                    Some(group),
                    Some(label),
                    format!("`{group}` lists `{label}` twice"),
                )),
                Some(_) => {}
            }
        }
    }
    out
}

pub(crate) fn variant_matches(product: &ProductRecord, variant: &Variant, picks: &Picks) -> bool {
    product.axes.iter().all(|axis| match picks.get(&axis.group) {
        Some(Pick::One(label)) if !label.is_empty() => variant.picks.get(&axis.group) == Some(label),
        _ => true,
    })
}

/// Whether `pick` chooses nothing for a group of this shape: an empty label always (the documented
/// "nothing picked"), an empty list only for a multi group (a list for a single-choice group stays
/// a `wrong_pick_type`).
fn chooses_nothing(pick: &Pick, multi: bool) -> bool {
    match pick {
        Pick::One(s) => s.is_empty(),
        Pick::Many(list) => multi && list.is_empty(),
    }
}

/// Whether two picks of one group (under two spellings of its name) choose the same values,
/// ignoring ASCII case and, for a list, order.
fn same_pick(product: &ProductRecord, at: GroupRef, a: &Pick, b: &Pick) -> bool {
    let norm = |p: &Pick| match product.canonical_pick(at, p) {
        Pick::One(s) => Pick::One(s),
        Pick::Many(mut list) => {
            list.sort();
            Pick::Many(list)
        }
    };
    norm(a) == norm(b)
}

/// The first value of `values` whose label equals `label` ignoring ASCII case.
fn find_value<'a>(values: &'a [OptionValue], label: &str) -> Option<&'a OptionValue> {
    values.iter().find(|v| v.label.eq_ignore_ascii_case(label))
}

/// The values chosen in one applicable add-on group, in record order, and any refusals.
///
/// A pick that chooses nothing (an empty label, or an empty list for a multi group) counts as no
/// pick, so a required group refuses with `addon_required` (an empty list never sells a required
/// multi group).
fn addon_choice<'a>(addon: &'a Addon, pick: Option<&Pick>) -> (Vec<&'a OptionValue>, Vec<Refusal>) {
    let group = addon.group.as_str();
    let pick = pick.filter(|p| !chooses_nothing(p, addon.multi));
    let Some(pick) = pick else {
        if addon.required {
            let message = format!("`{group}` is required");
            return (
                Vec::new(),
                vec![Refusal::new("addon_required", Some(group), None, message)],
            );
        }
        return (Vec::new(), Vec::new());
    };
    if matches!(pick, Pick::Many(_)) != addon.multi {
        let message = if addon.multi {
            format!("`{group}` is a multi group: pick a list of distinct labels")
        } else {
            format!("`{group}` takes one label, not a list")
        };
        return (
            Vec::new(),
            vec![Refusal::new("wrong_pick_type", Some(group), None, message)],
        );
    }
    let labels: Vec<&str> = match pick {
        Pick::One(s) => vec![s.as_str()],
        Pick::Many(list) => list.iter().map(String::as_str).collect(),
    };
    let mut refusals = Vec::new();
    let mut picked: Vec<&OptionValue> = Vec::new();
    for label in labels {
        match find_value(&addon.values, label) {
            None => refusals.push(Refusal::new(
                "unknown_value",
                Some(group),
                Some(label),
                format!("`{group}` has no value `{label}`"),
            )),
            Some(v) if !v.available => refusals.push(Refusal::new(
                "value_unavailable",
                Some(group),
                Some(&v.label),
                format!("`{group}={}` is no longer offered", v.label),
            )),
            Some(v) if picked.iter().any(|p| p.label == v.label) => refusals.push(Refusal::new(
                "duplicate_value",
                Some(group),
                Some(&v.label),
                format!("`{group}` lists `{}` twice", v.label),
            )),
            Some(v) => picked.push(v),
        }
    }
    picked.sort_by_key(|p| addon.values.iter().position(|v| v.label == p.label));
    (picked, refusals)
}

/// The compact canonical JSON of `value`: sorted keys, no spaces, numbers as parsed (`3.0` stays
/// `3.0`, unlike RFC 8785), strings without ASCII escaping.
fn compact_json(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                compact_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(k, _)| *k);
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                compact_json(item, out);
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// What a `gem` group adds for one value: the value's `{cabochon, count, cut, size_mm}` from its gem
/// facets, or its label when it has none of them.
fn gem_shape(value: &OptionValue) -> Value {
    let field = |name: &str| {
        value
            .gem
            .as_ref()
            .filter(|g| g.is_object())
            .and_then(|g| g.get(name))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let shape = json!({
        "cabochon": field("cabochon"),
        "count": field("count"),
        "cut": field("cut"),
        "size_mm": field("size_mm"),
    });
    if shape.as_object().is_some_and(|o| o.values().all(Value::is_null)) {
        Value::String(value.label.clone())
    } else {
        shape
    }
}

/// The `geometry_key` (`interfaces.md` § 0.2, the option graph schema's rule 8, 1.2.0): `sha256:` of
/// the compact canonical JSON of `{"design_key", "picks"}`, so every metal, stone and finish swap of
/// one shape shares a key. Each group counts by its `geometry_role` (`geometry` when the record leaves
/// it out): a `geometry` group adds its label (a multi group its list of labels in record order, when
/// not empty), a `gem` group the chosen value's shape (`{cabochon, count, cut, size_mm}`, so a
/// cabochon and a faceted stone are two models), and an `excluded` group nothing. A value with a
/// `geometry_pick` adds that object in place of its label or shape (1.2.0; read, never rebuilt).
///
/// A `gem` add-on with a `slot` enters under `slot:<slot>` instead of its group name (1.2.0), so the
/// per-metal copies of one slot's stone list key alike. When two picked groups of one build claim the
/// same slot, the first in record order takes `slot:<slot>` and a later one keeps its group name (the
/// reference implementation's rule: at worst one model is built twice, never two share one).
///
/// `picks` is in the record's own spelling, as [`Resolution::picks`] is.
pub fn geometry_key(product: &ProductRecord, picks: &Picks) -> String {
    let mut body = serde_json::Map::new();
    let mut add = |name: String, group: &str, role: Option<&str>, multi: bool, values: &[OptionValue]| {
        let Some(pick) = picks.get(group) else {
            return;
        };
        let labels = pick.labels();
        let chosen: Vec<&OptionValue> = values
            .iter()
            .filter(|v| labels.contains(&v.label.as_str()))
            .collect();
        if chosen.is_empty() || role == Some("excluded") {
            return;
        }
        // 1.2.0: a value's `geometry_pick` stands for it; else a gem group's stone shape, else the label.
        let one = |v: &OptionValue| match &v.geometry_pick {
            Some(g) => g.clone(),
            None if role == Some("gem") => gem_shape(v),
            None => Value::String(v.label.clone()),
        };
        let mut entries: Vec<Value> = chosen.into_iter().map(one).collect();
        let entry = if multi {
            Value::Array(entries)
        } else {
            entries.swap_remove(0)
        };
        // A slot an earlier group of this build has claimed: this group keys under its own name.
        // A name that is taken too gets a numbered suffix: an insert never overwrites another
        // group's entry (two picks collapsing into one key), whatever the groups are called.
        let name = if body.contains_key(&name) {
            let mut fallback = group.to_owned();
            let mut n = 1u32;
            while body.contains_key(&fallback) {
                fallback = format!("{group}#{n}");
                n += 1;
            }
            fallback
        } else {
            name
        };
        body.insert(name, entry);
    };
    for axis in &product.axes {
        add(
            axis.group.clone(),
            &axis.group,
            axis.geometry_role.as_deref(),
            false,
            &axis.values,
        );
    }
    for addon in &product.addons {
        // 1.2.0: a gem group that fills a slot counts as `slot:<slot>`, so the per-metal groups of
        // one slot (a gold and a titanium list of the same stones) key alike.
        let name = match (&addon.slot, addon.geometry_role.as_deref()) {
            (Some(slot), Some("gem")) if !slot.is_empty() => format!("slot:{slot}"),
            _ => addon.group.clone(),
        };
        add(
            name,
            &addon.group,
            addon.geometry_role.as_deref(),
            addon.multi,
            &addon.values,
        );
    }
    let mut text = String::new();
    compact_json(
        &json!({"design_key": product.design_key(), "picks": Value::Object(body)}),
        &mut text,
    );
    format!(
        "sha256:{}",
        crate::sha256::hex(&crate::sha256::Sha256::digest(text.as_bytes()))
    )
}

/// The live, priced variants of a product that name every axis, by their axis labels in axis order:
/// built once so a caller resolving every variant (opening a product) finds each match by lookup
/// instead of a scan of the whole list, which made that O(variants²).
pub(crate) struct VariantIndex<'a> {
    by_labels: std::collections::HashMap<Vec<&'a str>, Vec<&'a Variant>>,
}

impl<'a> VariantIndex<'a> {
    /// Index `product`'s offered variants (step 3's filter) that pick every axis.
    pub(crate) fn new(product: &'a ProductRecord) -> Self {
        let mut by_labels: std::collections::HashMap<Vec<&'a str>, Vec<&'a Variant>> =
            std::collections::HashMap::new();
        for v in product.variants.iter().filter(|v| product.variant_offered(v)) {
            let labels: Option<Vec<&str>> = product
                .axes
                .iter()
                .map(|axis| v.picks.get(&axis.group).map(String::as_str))
                .collect();
            if let Some(labels) = labels {
                by_labels.entry(labels).or_default().push(v);
            }
        }
        Self { by_labels }
    }

    /// Step 3's matches when `chosen` picks every axis (a variant that lacks an axis can never match
    /// such a pick, so the index holds them all); `None` when an axis is left open.
    fn matches(&self, product: &ProductRecord, chosen: &BTreeMap<&str, &str>) -> Option<Vec<&'a Variant>> {
        let labels: Option<Vec<&str>> = product
            .axes
            .iter()
            .map(|axis| chosen.get(axis.group.as_str()).copied())
            .collect();
        Some(self.by_labels.get(&labels?).cloned().unwrap_or_default())
    }
}

#[cfg(test)]
thread_local! {
    /// Variants step 3 looked at by scanning, for the tests that pin opening a product as linear.
    pub(crate) static VARIANT_SCANS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Resolve `picks` against `product` (module docs). Pure and deterministic; never prices.
pub fn resolve(product: &ProductRecord, picks: &Picks) -> Resolution {
    resolve_indexed(product, picks, None)
}

/// [`resolve`], finding the variant through `index` when the picks name every axis.
pub(crate) fn resolve_indexed(
    product: &ProductRecord,
    picks: &Picks,
    index: Option<&VariantIndex<'_>>,
) -> Resolution {
    let cart_mode = product.cart_mode().unwrap_or(CartMode::ManualLine);
    let refused = |reasons: Vec<Refusal>, open: Vec<String>| Resolution {
        product_key: product.product_key.clone(),
        orderable: false,
        variant_key: None,
        sku: None,
        picks: Picks::new(),
        cart_mode,
        cart_lines: Vec::new(),
        made_to_order_line: None,
        statement: None,
        geometry_key: None,
        open,
        reasons,
    };

    // 1. The product's status.
    match product.status() {
        ProductStatus::NotOrderable | ProductStatus::Unknown => {
            return refused(
                vec![Refusal::new(
                    "product_not_orderable",
                    None,
                    Some(&product.status),
                    format!(
                        "`{}` is not orderable (status `{}`)",
                        product.product_key, product.status
                    ),
                )],
                Vec::new(),
            );
        }
        ProductStatus::Duplicate => {
            return refused(
                vec![Refusal::new(
                    "product_not_orderable",
                    None,
                    Some(&product.status),
                    format!(
                        "`{}` duplicates `{}`",
                        product.product_key,
                        product.canonical_of.as_deref().unwrap_or("another listing")
                    ),
                )],
                Vec::new(),
            );
        }
        ProductStatus::Orderable | ProductStatus::Basic | ProductStatus::Incomplete => {}
    }

    // 2. Sort the picks into groups and check the axis picks. A pick that chooses nothing is no
    // pick. Two spellings of one group that pick the same values are one pick; two that pick
    // different values are refused (`duplicate_value`), never settled by which spelling sorts last.
    let mut reasons = Vec::new();
    let mut axis_picks: BTreeMap<&str, &Pick> = BTreeMap::new();
    let mut addon_picks: BTreeMap<&str, &Pick> = BTreeMap::new();
    for (key, pick) in picks {
        let Some(at) = product.group_folded(key) else {
            reasons.push(Refusal::new(
                "unknown_group",
                Some(key),
                None,
                format!("`{}` has no option group `{key}`", product.product_key),
            ));
            continue;
        };
        let (name, multi, into) = match at {
            GroupRef::Axis(i) => (product.axes[i].group.as_str(), false, &mut axis_picks),
            GroupRef::Addon(i) => (
                product.addons[i].group.as_str(),
                product.addons[i].multi,
                &mut addon_picks,
            ),
        };
        if chooses_nothing(pick, multi) {
            continue;
        }
        if let Some(earlier) = into.get(name) {
            if !same_pick(product, at, earlier, pick) {
                let value = pick.labels().join(", ");
                reasons.push(Refusal::new(
                    "duplicate_value",
                    Some(name),
                    Some(&value),
                    format!("`{name}` is picked twice, under two spellings, with different values"),
                ));
            }
            continue;
        }
        into.insert(name, pick);
    }
    let mut chosen_axes: BTreeMap<&str, &str> = BTreeMap::new();
    for axis in &product.axes {
        let group = axis.group.as_str();
        let Some(pick) = axis_picks.get(group) else {
            continue;
        };
        let Pick::One(raw) = pick else {
            reasons.push(Refusal::new(
                "wrong_pick_type",
                Some(group),
                None,
                format!("`{group}` takes one label, not a list"),
            ));
            continue;
        };
        match find_value(&axis.values, raw) {
            None => reasons.push(Refusal::new(
                "unknown_value",
                Some(group),
                Some(raw),
                format!("`{group}` has no value `{raw}`"),
            )),
            Some(v) if !v.available => reasons.push(Refusal::new(
                "value_unavailable",
                Some(group),
                Some(&v.label),
                format!("`{group}={}` is no longer offered", v.label),
            )),
            Some(v) => {
                chosen_axes.insert(group, v.label.as_str());
            }
        }
    }
    if !reasons.is_empty() {
        return refused(reasons, Vec::new());
    }

    // 3. Find the variant.
    let indexed = index.and_then(|ix| ix.matches(product, &chosen_axes));
    let matches: Vec<&Variant> = match indexed {
        Some(found) => found,
        None => product
            .variants
            .iter()
            .filter(|v| {
                #[cfg(test)]
                VARIANT_SCANS.with(|n| n.set(n.get() + 1));
                product.variant_offered(v)
                    && chosen_axes
                        .iter()
                        .all(|(group, label)| v.picks.get(*group).map(String::as_str) == Some(*label))
            })
            .collect(),
    };
    let variant = match matches.as_slice() {
        [] => {
            return refused(
                vec![Refusal::new(
                    "not_offered",
                    None,
                    None,
                    "this combination is not offered".to_owned(),
                )],
                Vec::new(),
            );
        }
        [one] => *one,
        many => {
            let open: Vec<String> = product
                .axes
                .iter()
                .filter(|axis| {
                    !chosen_axes.contains_key(axis.group.as_str())
                        && many
                            .iter()
                            .any(|v| v.picks.get(&axis.group) != many[0].picks.get(&axis.group))
                })
                .map(|axis| axis.group.clone())
                .collect();
            let mut reasons: Vec<Refusal> = open
                .iter()
                .map(|g| {
                    Refusal::new(
                        "axis_open",
                        Some(g),
                        None,
                        format!("{} variants match; pick `{g}`", many.len()),
                    )
                })
                .collect();
            if reasons.is_empty() {
                // Listed variants that no axis tells apart: nothing a pick could settle.
                reasons.push(Refusal::new(
                    "not_offered",
                    None,
                    None,
                    format!("{} variants have the same axis values", many.len()),
                ));
            }
            return refused(reasons, open);
        }
    };

    // 4. Check the add-ons.
    for addon in &product.addons {
        if addon_picks.contains_key(addon.group.as_str()) && !addon.applies(&variant.variant_key) {
            reasons.push(Refusal::new(
                "addon_not_applicable",
                Some(&addon.group),
                None,
                format!(
                    "`{}` does not apply to variant `{}`",
                    addon.group, variant.variant_key
                ),
            ));
        }
    }
    let mut chosen_addons: Vec<(&Addon, Vec<&OptionValue>)> = Vec::new();
    for addon in product.addons.iter().filter(|a| a.applies(&variant.variant_key)) {
        let (chosen, refusals) = addon_choice(addon, addon_picks.get(addon.group.as_str()).copied());
        reasons.extend(refusals);
        chosen_addons.push((addon, chosen));
    }
    if !reasons.is_empty() {
        return refused(reasons, Vec::new());
    }

    // 7 first: the cart lines and the made-to-order line carry the statement.
    let mut resolved = Picks::new();
    let mut parts = vec![if product.title.is_empty() {
        product.product_key.clone()
    } else {
        product.title.clone()
    }];
    for axis in &product.axes {
        if let Some(label) = variant.picks.get(&axis.group) {
            resolved.insert(axis.group.clone(), Pick::One(label.clone()));
            parts.push(format!("{}: {label}", axis.group));
        }
    }
    for (addon, chosen) in &chosen_addons {
        let labels: Vec<&str> = chosen.iter().map(|v| v.label.as_str()).collect();
        if addon.multi {
            resolved.insert(
                addon.group.clone(),
                Pick::Many(labels.iter().map(|l| (*l).to_owned()).collect()),
            );
        } else if let Some(first) = labels.first() {
            resolved.insert(addon.group.clone(), Pick::One((*first).to_owned()));
        }
        if !labels.is_empty() {
            parts.push(format!("{}: {}", addon.group, labels.join(", ")));
        }
    }
    let statement = parts.join(" | ");

    // 6. The cart lines. An add-on code that is one of the variant's own axis values adds no line
    // (catalogue research § 9.6 step 4).
    let axis_codes: Vec<&str> = product
        .axes
        .iter()
        .filter_map(|axis| {
            let label = variant.picks.get(&axis.group)?;
            axis.values.iter().find(|v| &v.label == label)?.code.as_deref()
        })
        .collect();
    let mut cart_lines = Vec::new();
    let mut attrs = BTreeMap::new();
    if cart_mode == CartMode::ShopifyLines {
        cart_lines.push(CartLine {
            id: variant.variant_key.clone(),
            quantity: 1,
        });
    }
    for (addon, chosen) in &chosen_addons {
        let mut codes = Vec::new();
        for v in chosen {
            let code = v.code.as_deref().map_or("", str::trim);
            let usable = match cart_mode {
                CartMode::ShopifyLines => is_numeric_code(code),
                CartMode::ManualLine => !code.is_empty(),
            };
            if !usable {
                reasons.push(Refusal::new(
                    "no_cart_code",
                    Some(&addon.group),
                    Some(&v.label),
                    format!("`{}={}` has no code for the order", addon.group, v.label),
                ));
            } else if cart_mode == CartMode::ManualLine {
                codes.push(code.to_owned());
            } else if !axis_codes.contains(&code) {
                cart_lines.push(CartLine {
                    id: code.to_owned(),
                    quantity: 1,
                });
            }
        }
        if addon.multi {
            attrs.insert(addon.group.clone(), Pick::Many(codes));
        } else if let Some(first) = codes.into_iter().next() {
            attrs.insert(addon.group.clone(), Pick::One(first));
        }
    }
    if !reasons.is_empty() {
        return refused(reasons, Vec::new());
    }

    let made_to_order_line = (cart_mode == CartMode::ManualLine).then(|| {
        let (variation_id, metal_code) = match variant.variant_key.split_once(':') {
            Some((v, m)) => (v.to_owned(), (!m.is_empty()).then(|| m.to_owned())),
            None => (variant.variant_key.clone(), None),
        };
        MadeToOrderLine {
            style_sku: variant.sku.clone(),
            variation_id,
            metal_code,
            attrs,
            statement: statement.clone(),
        }
    });
    let resolved_for_key = resolved.clone();
    Resolution {
        product_key: product.product_key.clone(),
        orderable: true,
        variant_key: Some(variant.variant_key.clone()),
        sku: variant.sku.clone(),
        picks: resolved,
        cart_mode,
        cart_lines,
        made_to_order_line,
        statement: Some(statement),
        geometry_key: Some(geometry_key(product, &resolved_for_key)),
        open: Vec::new(),
        reasons: Vec::new(),
    }
}
