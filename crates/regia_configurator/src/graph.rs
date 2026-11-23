// SPDX-License-Identifier: MPL-2.0
//! The option graph's product record (`jewelry.option_graph/1`; plan `interfaces.md` § 2, the record
//! layout of the catalogue research § 9.5), read for the configurator runtime (ADR-2534).
//!
//! Only the fields the runtime needs are typed; every other field is accepted and ignored, so a
//! record that gains optional fields within the major version still loads (`interfaces.md` § 0.1).
//! A variant's or add-on value's dealer `cost` is read only to tell a priced variant from an
//! unpriced one: it is never stored in a build, never returned and never priced (Regia prices
//! nothing).

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::resolve::{Pick, Picks};

/// The schema ids an option-graph product record may carry: the interface name and the
/// extractor's earlier draft name.
pub const OPTION_GRAPH_SCHEMAS: &[&str] = &["jewelry.option_graph/1", "optiongraph/1"];

/// The values of an option group's `geometry_role`.
pub const GEOMETRY_ROLES: &[&str] = &["geometry", "gem", "excluded"];

/// The gap code of a listing whose variation list stopped early (catalogue research § 9.8).
pub const TRUNCATED_VARIATIONS: &str = "TRUNCATED_VARIATIONS";

/// One product's option graph: its option groups, its live variants and the rules between them.
#[derive(Debug, Clone, Deserialize)]
pub struct ProductRecord {
    /// `jewelry.option_graph/1` (or the draft `optiongraph/1`).
    pub schema: String,
    /// The catalogue copy the record was extracted from.
    #[serde(default)]
    pub snapshot_id: String,
    /// The extractor's hash of this record.
    #[serde(default)]
    pub content_hash: Option<String>,
    /// `<house>/<house_key>`.
    pub product_key: String,
    /// `<house>/<sku_base>`, else the `product_key`.
    #[serde(default)]
    pub design_key: Option<String>,
    /// The canonical listing when this one is a duplicate.
    #[serde(default)]
    pub canonical_of: Option<String>,
    /// Product title, used in the statement line.
    #[serde(default)]
    pub title: String,
    /// `orderable`, `basic`, `incomplete`, `not_orderable` or `duplicate`.
    #[serde(default = "default_status")]
    pub status: String,
    /// The option groups linked to variants (one value per variant).
    #[serde(default)]
    pub axes: Vec<Axis>,
    /// The live variants.
    #[serde(default)]
    pub variants: Vec<Variant>,
    /// The option groups not linked to variants (gems, finishes, extras).
    #[serde(default)]
    pub addons: Vec<Addon>,
    /// How an order is placed.
    #[serde(default)]
    pub cart: Cart,
    /// Gap and flag rows (`TRUNCATED_VARIATIONS`, `SLOT_MAP`, ...): strings or `{code, ...}` objects.
    #[serde(default)]
    pub gaps: Vec<Value>,
    /// The gem slots (`slots[]`): what each stone position is and, from 1.2.0, where its stones sit.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub slots: Vec<Slot>,
}

/// A JSON `null` list reads as an empty one.
fn null_as_empty<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

/// One gem slot of a product record (`slots[]` in the option graph schema). Only what a viewer
/// needs is typed; the record's other slot fields are ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Slot {
    /// The slot id (`s1`, ...). Per-metal gem groups that fill it name it in their `slot`, and the
    /// geometry key counts them under `slot:<slot>` (option graph 1.2.0).
    pub slot: String,
    /// What the position is (`center`, `position_1`, ...).
    #[serde(default)]
    pub role: String,
    /// Stones in the slot.
    #[serde(default = "one")]
    pub count: u32,
    /// The cut, when the record knows it.
    #[serde(default)]
    pub cut: Option<String>,
    /// Where each of the slot's stones sits on the front photo, as `[x, y]` in `0..=1` from the
    /// image's top left (option graph 1.2.0; optional, `None` when the record leaves it out).
    /// Checked on load: every entry is two finite numbers inside the unit square.
    #[serde(default)]
    pub positions_norm: Option<Vec<[f64; 2]>>,
}

fn one() -> u32 {
    1
}

fn default_status() -> String {
    "orderable".to_owned()
}

/// A product record's `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductStatus {
    /// Priced and orderable.
    Orderable,
    /// Plain hardware that is never modelled as a product.
    Basic,
    /// The listing is known to be partial (for example a truncated variation list).
    Incomplete,
    /// No live priced variant.
    NotOrderable,
    /// A duplicate listing of `canonical_of`.
    Duplicate,
    /// A status this build does not know; treated as not orderable.
    Unknown,
}

impl ProductStatus {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Orderable => "orderable",
            Self::Basic => "basic",
            Self::Incomplete => "incomplete",
            Self::NotOrderable => "not_orderable",
            Self::Duplicate => "duplicate",
            Self::Unknown => "unknown",
        }
    }
}

/// An option group linked to variants.
#[derive(Debug, Clone, Deserialize)]
pub struct Axis {
    /// The group name, as shown by the supplier (`Material`, `Gauge`).
    pub group: String,
    /// What the group means (`metal`, `thread`, `gauge`, `gem_size`, ...).
    #[serde(default)]
    pub kind: String,
    /// How the group enters the geometry key: `geometry` (the default), `gem` or `excluded`.
    #[serde(default)]
    pub geometry_role: Option<String>,
    /// The group's values.
    #[serde(default)]
    pub values: Vec<OptionValue>,
}

/// One value of an option group.
#[derive(Debug, Clone, Deserialize)]
pub struct OptionValue {
    /// `<group>=<label>`.
    #[serde(default)]
    pub value_key: String,
    /// The label the supplier shows; picks name values by it.
    pub label: String,
    /// The supplier's code for the value (a Shopify id or an attribute code), as a string.
    #[serde(default, deserialize_with = "code_string")]
    pub code: Option<String>,
    /// Whether the supplier still offers it (`available = 0` is never accepted).
    #[serde(default = "yes")]
    pub available: bool,
    /// Gem facts of a gem value (`material`, `class`, `cut`, `size_mm`, ...).
    #[serde(default)]
    pub gem: Option<Value>,
    /// What the value is as geometry, when its label carries more than the shape (option graph
    /// 1.2.0): for example a style's body text and its stones' `{cabochon, count, cut, size_mm}`,
    /// without the stone materials. The geometry key hashes it in place of the label, so values
    /// that differ only in stone or metal share a key.
    #[serde(default)]
    pub geometry_pick: Option<Value>,
}

fn yes() -> bool {
    true
}

/// A code is a JSON string or number; both are kept as text.
fn code_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let raw = Option::<Value>::deserialize(d)?;
    Ok(match raw {
        Some(Value::String(s)) if !s.is_empty() => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

/// A live variant: one value of every axis, and the supplier's key and SKU for it.
#[derive(Debug, Clone, Deserialize)]
pub struct Variant {
    /// `house_variant_id`.
    pub variant_key: String,
    /// The supplier's SKU.
    #[serde(default)]
    pub sku: Option<String>,
    /// Axis group -> value label.
    #[serde(default)]
    pub picks: BTreeMap<String, String>,
    /// Dealer cost, read only to tell a priced variant (staff-only; never stored or returned).
    /// Absent in a client pack, which has its costs stripped; present but `null` means unpriced.
    #[serde(default, deserialize_with = "present_cost")]
    cost: Option<Option<f64>>,
    /// A retired variant.
    #[serde(default)]
    pub discontinued: bool,
}

impl Variant {
    /// Whether the variant is live and priced: not discontinued, and with a cost above zero
    /// when the record carries the field (a client pack has its costs stripped; the extractor
    /// lists priced variants only, so a `null` cost is an unpriced variant).
    pub fn is_live(&self) -> bool {
        !self.discontinued && self.cost.is_none_or(|c| c.is_some_and(|c| c > 0.0))
    }
}

/// A present `cost` field, `null` included (only an absent field stays `None`).
fn present_cost<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<f64>>, D::Error> {
    Option::<f64>::deserialize(d).map(Some)
}

/// An option group not linked to variants.
#[derive(Debug, Clone, Deserialize)]
pub struct Addon {
    /// The group name.
    pub group: String,
    /// What the group means (`gem`, `finish`, `anodize`, `thread`, ...).
    #[serde(default)]
    pub kind: String,
    /// A required group needs exactly one value.
    #[serde(default)]
    pub required: bool,
    /// A multi group takes any subset of its values.
    #[serde(default)]
    pub multi: bool,
    /// The variants it applies to; absent or `null` means every variant (an empty list: none).
    #[serde(default)]
    pub applies_to: Option<Vec<String>>,
    /// The gem slot it fills, if any.
    #[serde(default)]
    pub slot: Option<String>,
    /// How the group enters the geometry key: `geometry` (the default), `gem` or `excluded`.
    #[serde(default)]
    pub geometry_role: Option<String>,
    /// The group's values.
    #[serde(default)]
    pub values: Vec<OptionValue>,
}

impl Addon {
    /// Whether the group applies to `variant_key`.
    pub fn applies(&self, variant_key: &str) -> bool {
        match &self.applies_to {
            None => true,
            Some(list) => list.iter().any(|k| k == variant_key),
        }
    }
}

/// How an order for the product is placed.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Cart {
    /// The supplier's shop platform.
    #[serde(default)]
    pub platform: Option<String>,
    /// `shopify_lines` or `manual_line`; empty when the record does not say.
    #[serde(default)]
    pub mode: String,
}

/// The cart mode of a resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CartMode {
    /// Lines added to a Shopify cart: the variant, then every chosen add-on value by code.
    ShopifyLines,
    /// One made-to-order line a person places on the supplier's site.
    ManualLine,
}

impl CartMode {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ShopifyLines => "shopify_lines",
            Self::ManualLine => "manual_line",
        }
    }
}

/// Why a document could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    /// A stable lowercase reason (`configurator_bad_document`, ...).
    pub reason: &'static str,
    /// What is wrong, naming the file or field.
    pub message: String,
}

impl LoadError {
    pub(crate) fn bad(message: impl Into<String>) -> Self {
        Self {
            reason: "configurator_bad_document",
            message: message.into(),
        }
    }
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

impl std::error::Error for LoadError {}

/// Where a pick's group lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupRef {
    /// `axes[i]`.
    Axis(usize),
    /// `addons[i]`.
    Addon(usize),
}

impl ProductRecord {
    /// Parse a product record from JSON bytes and check what the runtime relies on: a known
    /// schema id, a cart mode, unique group names, unique variant keys, and variant picks that
    /// name known axes and labels.
    pub fn from_json(bytes: &[u8]) -> Result<Self, LoadError> {
        let record: Self = serde_json::from_slice(bytes)
            .map_err(|e| LoadError::bad(format!("option graph product record: {e}")))?;
        record.validate()?;
        Ok(record)
    }

    /// Parse from an already parsed JSON value (see [`ProductRecord::from_json`]).
    pub fn from_value(value: Value) -> Result<Self, LoadError> {
        let record: Self = serde_json::from_value(value)
            .map_err(|e| LoadError::bad(format!("option graph product record: {e}")))?;
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> Result<(), LoadError> {
        if !OPTION_GRAPH_SCHEMAS.contains(&self.schema.as_str()) {
            return Err(LoadError::bad(format!(
                "`{}` is not an option graph product record (schema `{}`)",
                self.product_key, self.schema
            )));
        }
        if !self.product_key.contains('/') {
            return Err(LoadError::bad(format!(
                "product_key `{}` is not `<house>/<key>`",
                self.product_key
            )));
        }
        if self.cart_mode().is_none() {
            return Err(LoadError::bad(format!(
                "`{}` has no cart mode (cart.mode is `{}`)",
                self.product_key, self.cart.mode
            )));
        }
        let roles = self
            .axes
            .iter()
            .map(|a| (&a.group, &a.geometry_role))
            .chain(self.addons.iter().map(|a| (&a.group, &a.geometry_role)));
        for (group, role) in roles {
            if let Some(role) = role
                && !GEOMETRY_ROLES.contains(&role.as_str())
            {
                return Err(LoadError::bad(format!(
                    "`{}`: `{group}` has the geometry_role `{role}`",
                    self.product_key
                )));
            }
        }
        let values = self
            .axes
            .iter()
            .map(|a| (&a.group, &a.values))
            .chain(self.addons.iter().map(|a| (&a.group, &a.values)));
        for (group, values) in values {
            if let Some(value) = values.iter().find(|v| {
                v.geometry_pick
                    .as_ref()
                    .is_some_and(|pick| !pick.is_object() && !pick.is_null())
            }) {
                return Err(LoadError::bad(format!(
                    "`{}`: `{group}` value `{}` has a geometry_pick that is not an object",
                    self.product_key, value.label
                )));
            }
        }
        // Picks name groups and labels ignoring ASCII case (`group_folded`, `canonical_pick`), so
        // two that differ only by case are one name: the first could never be picked (the last
        // spelling wins), and such a record could not sell the combinations it lists.
        let mut groups = std::collections::BTreeSet::new();
        for g in self
            .axes
            .iter()
            .map(|a| &a.group)
            .chain(self.addons.iter().map(|a| &a.group))
        {
            if g.is_empty() || !groups.insert(g.to_ascii_lowercase()) {
                return Err(LoadError::bad(format!(
                    "`{}` names the option group `{g}` twice, ignoring case (or an empty group)",
                    self.product_key
                )));
            }
        }
        let group_values = self
            .axes
            .iter()
            .map(|a| (&a.group, &a.values))
            .chain(self.addons.iter().map(|a| (&a.group, &a.values)));
        for (group, values) in group_values {
            let mut labels = std::collections::BTreeSet::new();
            if let Some(v) = values
                .iter()
                .find(|v| !labels.insert(v.label.to_ascii_lowercase()))
            {
                return Err(LoadError::bad(format!(
                    "`{}`: `{group}` lists the value `{}` twice, ignoring case",
                    self.product_key, v.label
                )));
            }
        }
        let mut slot_ids = std::collections::BTreeSet::new();
        for s in &self.slots {
            if s.slot.is_empty() || !slot_ids.insert(s.slot.as_str()) {
                return Err(LoadError::bad(format!(
                    "`{}` names the gem slot `{}` twice (or an empty slot id)",
                    self.product_key, s.slot
                )));
            }
            let positions = s.positions_norm.as_deref().unwrap_or_default();
            // The schema: one center per stone of the slot, at least one.
            if s.positions_norm.is_some() && positions.len() != s.count as usize {
                return Err(LoadError::bad(format!(
                    "`{}`: slot `{}` has {} positions_norm for {} stones",
                    self.product_key,
                    s.slot,
                    positions.len(),
                    s.count
                )));
            }
            if let Some((i, p)) = positions
                .iter()
                .enumerate()
                .find(|(_, p)| !p.iter().all(|x| x.is_finite() && (0.0..=1.0).contains(x)))
            {
                return Err(LoadError::bad(format!(
                    "`{}`: slot `{}` positions_norm[{i}] = {p:?} is outside the unit square",
                    self.product_key, s.slot
                )));
            }
        }
        let mut keys = std::collections::BTreeSet::new();
        for v in &self.variants {
            if !keys.insert(v.variant_key.as_str()) {
                return Err(LoadError::bad(format!(
                    "`{}` lists the variant `{}` twice",
                    self.product_key, v.variant_key
                )));
            }
            for (group, label) in &v.picks {
                let Some(axis) = self.axes.iter().find(|a| &a.group == group) else {
                    return Err(LoadError::bad(format!(
                        "variant `{}` picks the unknown axis `{group}`",
                        v.variant_key
                    )));
                };
                if !axis.values.iter().any(|x| &x.label == label) {
                    return Err(LoadError::bad(format!(
                        "variant `{}` picks `{group}={label}`, which the axis does not list",
                        v.variant_key
                    )));
                }
            }
        }
        Ok(())
    }

    /// The house slug (the part of the product key before `/`).
    pub fn house(&self) -> &str {
        self.product_key.split_once('/').map_or("", |(h, _)| h)
    }

    /// The design key, else the product key.
    pub fn design_key(&self) -> &str {
        self.design_key.as_deref().unwrap_or(&self.product_key)
    }

    /// The gem slot with this id.
    pub fn slot(&self, id: &str) -> Option<&Slot> {
        self.slots.iter().find(|s| s.slot == id)
    }

    /// The parsed status.
    pub fn status(&self) -> ProductStatus {
        match self.status.as_str() {
            "orderable" => ProductStatus::Orderable,
            "basic" => ProductStatus::Basic,
            "incomplete" => ProductStatus::Incomplete,
            "not_orderable" => ProductStatus::NotOrderable,
            "duplicate" => ProductStatus::Duplicate,
            _ => ProductStatus::Unknown,
        }
    }

    /// The cart mode, `None` when the record names none this build knows.
    pub fn cart_mode(&self) -> Option<CartMode> {
        match self.cart.mode.as_str() {
            "shopify_lines" => Some(CartMode::ShopifyLines),
            "manual_line" => Some(CartMode::ManualLine),
            _ => None,
        }
    }

    /// The gap and flag codes, in record order.
    pub fn gap_codes(&self) -> Vec<String> {
        self.gaps
            .iter()
            .filter_map(|g| match g {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("code").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .collect()
    }

    /// Whether the listing is known to be partial: status `incomplete` or a
    /// `TRUNCATED_VARIATIONS` flag. Such a listing is never presented as complete.
    pub fn is_incomplete(&self) -> bool {
        self.status() == ProductStatus::Incomplete
            || self.gap_codes().iter().any(|c| c == TRUNCATED_VARIATIONS)
    }

    /// Find an option group by name.
    pub fn group(&self, name: &str) -> Option<GroupRef> {
        if let Some(i) = self.axes.iter().position(|a| a.group == name) {
            return Some(GroupRef::Axis(i));
        }
        self.addons
            .iter()
            .position(|a| a.group == name)
            .map(GroupRef::Addon)
    }

    /// The `geometry_role` a record gives a group, `None` when it leaves it out.
    pub fn group_role(&self, name: &str) -> Option<&str> {
        match self.group(name)? {
            GroupRef::Axis(i) => self.axes[i].geometry_role.as_deref(),
            GroupRef::Addon(i) => self.addons[i].geometry_role.as_deref(),
        }
    }

    /// The `kind` of a group (empty when unknown).
    pub fn group_kind(&self, name: &str) -> &str {
        match self.group(name) {
            Some(GroupRef::Axis(i)) => &self.axes[i].kind,
            Some(GroupRef::Addon(i)) => &self.addons[i].kind,
            None => "",
        }
    }

    /// Find an option group by name ignoring ASCII case, axes first; of groups that differ only
    /// by case the last in the record wins (the reference implementation's rule).
    pub fn group_folded(&self, name: &str) -> Option<GroupRef> {
        if let Some(i) = self.axes.iter().rposition(|a| a.group.eq_ignore_ascii_case(name)) {
            return Some(GroupRef::Axis(i));
        }
        self.addons
            .iter()
            .rposition(|a| a.group.eq_ignore_ascii_case(name))
            .map(GroupRef::Addon)
    }

    /// `picks` in the record's own spelling: group names and labels that match ignoring ASCII case
    /// are rewritten, anything else is kept as given (so the refusal can name it).
    ///
    /// Two spellings of one group that choose the same values become one pick. Two that choose
    /// different values are both kept under the spellings that were given, so the resolver reports
    /// `duplicate_value` instead of keeping whichever spelling sorts last.
    pub fn canonical_picks(&self, picks: &Picks) -> Picks {
        let mut unknown = Picks::new();
        let mut folded: BTreeMap<String, (GroupRef, Vec<(String, Pick)>)> = BTreeMap::new();
        for (group, pick) in picks {
            match self.group_folded(group) {
                Some(at) => {
                    let name = match at {
                        GroupRef::Axis(i) => self.axes[i].group.clone(),
                        GroupRef::Addon(i) => self.addons[i].group.clone(),
                    };
                    folded
                        .entry(name)
                        .or_insert_with(|| (at, Vec::new()))
                        .1
                        .push((group.clone(), pick.clone()));
                }
                None => {
                    unknown.insert(group.clone(), pick.clone());
                }
            }
        }
        let mut out = unknown;
        for (name, (at, spellings)) in folded {
            let canons: Vec<Pick> = spellings
                .iter()
                .map(|(_, p)| self.canonical_pick(at, p))
                .collect();
            let agree = canons.windows(2).all(|w| same_canonical_pick(&w[0], &w[1]));
            if agree {
                if let Some(canon) = canons.into_iter().next() {
                    out.insert(name, canon);
                }
            } else {
                for (orig, pick) in spellings {
                    out.insert(orig, pick);
                }
            }
        }
        out
    }

    /// One pick's labels in the record's own spelling.
    pub fn canonical_pick(&self, at: GroupRef, pick: &Pick) -> Pick {
        let values = self.group_values(at);
        let spell = |label: &String| {
            values
                .iter()
                .find(|v| v.label.eq_ignore_ascii_case(label))
                .map_or_else(|| label.clone(), |v| v.label.clone())
        };
        match pick {
            Pick::One(label) => Pick::One(spell(label)),
            Pick::Many(labels) => Pick::Many(labels.iter().map(spell).collect()),
        }
    }

    /// The values of a group.
    pub fn group_values(&self, at: GroupRef) -> &[OptionValue] {
        match at {
            GroupRef::Axis(i) => &self.axes[i].values,
            GroupRef::Addon(i) => &self.addons[i].values,
        }
    }

    /// Whether an add-on value is acceptable for this product's order: available, with a numeric
    /// code for a Shopify cart and any code for a manual line (a chosen add-on without one refuses
    /// the build).
    pub fn addon_value_acceptable(&self, value: &OptionValue) -> bool {
        let code = value.code.as_deref().map_or("", str::trim);
        value.available
            && match self.cart_mode() {
                Some(CartMode::ShopifyLines) => is_numeric_code(code),
                _ => !code.is_empty(),
            }
    }

    /// Whether a variant is offered: live and priced, and every axis value it picks still
    /// available (a value the supplier no longer offers is never accepted, picked or not).
    pub fn variant_offered(&self, variant: &Variant) -> bool {
        variant.is_live()
            && variant.picks.iter().all(|(group, label)| {
                self.axes
                    .iter()
                    .find(|a| &a.group == group)
                    .and_then(|a| a.values.iter().find(|v| &v.label == label))
                    .is_some_and(|v| v.available)
            })
    }

    /// Whether a variant can be sold at all: offered, and every required add-on that applies to
    /// it offers at least one acceptable value.
    pub fn variant_sellable(&self, variant: &Variant) -> bool {
        self.variant_offered(variant)
            && self
                .addons
                .iter()
                .filter(|a| a.required && a.applies(&variant.variant_key))
                .all(|a| a.values.iter().any(|v| self.addon_value_acceptable(v)))
    }
}

/// Whether two already-canonical picks choose the same labels. A list's order does not matter.
fn same_canonical_pick(a: &Pick, b: &Pick) -> bool {
    match (a, b) {
        (Pick::One(x), Pick::One(y)) => x == y,
        (Pick::Many(x), Pick::Many(y)) => {
            let (mut x, mut y) = (x.clone(), y.clone());
            x.sort();
            y.sort();
            x == y
        }
        _ => false,
    }
}

/// A Shopify add-on code is all ASCII digits.
pub fn is_numeric_code(code: &str) -> bool {
    !code.is_empty() && code.bytes().all(|b| b.is_ascii_digit())
}

/// The `product_slug` / `design_slug` of a key (`interfaces.md` § 0.2): the part after
/// `<house>/`, lowercased, with every character outside `a-z0-9._-` replaced by `-`.
pub fn slug(key: &str) -> String {
    let tail = key.split_once('/').map_or(key, |(_, t)| t);
    tail.chars()
        .flat_map(char::to_lowercase)
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_follow_the_interface_rule() {
        assert_eq!(slug("house-b/p4879"), "p4879");
        assert_eq!(slug("house-a/ED-MQBEZ"), "ed-mqbez");
        assert_eq!(slug("house-a/Two Words+X"), "two-words-x");
    }

    fn size_record() -> ProductRecord {
        ProductRecord::from_value(serde_json::json!({
            "schema": "jewelry.option_graph/1",
            "product_key": "house-a/p1",
            "cart": {"mode": "shopify_lines"},
            "axes": [{
                "group": "Size",
                "kind": "gem_size",
                "values": [{"label": "S0"}, {"label": "S1"}]
            }],
        }))
        .expect("record")
    }

    /// Two ASCII-case spellings of one group that choose different values must both survive.
    /// Collapsing them used to keep whichever spelling sorts last, and `load_build` would sell it.
    #[test]
    fn disagreeing_spellings_of_one_group_are_both_kept() {
        let picks: Picks = serde_json::from_value(serde_json::json!({
            "Size": "S0",
            "size": "S1"
        }))
        .expect("picks");
        let got = size_record().canonical_picks(&picks);
        assert_eq!(got.get("Size"), Some(&Pick::One("S0".to_owned())), "{got:?}");
        assert_eq!(got.get("size"), Some(&Pick::One("S1".to_owned())), "{got:?}");
    }

    /// Spellings that name the same value collapse to the record's own spelling and label.
    #[test]
    fn agreeing_spellings_collapse_to_the_record_spelling() {
        let picks: Picks = serde_json::from_value(serde_json::json!({
            "Size": "s0",
            "SIZE": "S0"
        }))
        .expect("picks");
        let got = size_record().canonical_picks(&picks);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got.get("Size"), Some(&Pick::One("S0".to_owned())));
    }

    fn record(slots: Value) -> Result<ProductRecord, LoadError> {
        ProductRecord::from_value(serde_json::json!({
            "schema": "jewelry.option_graph/1", "product_key": "house-a/p1",
            "cart": {"mode": "shopify_lines"}, "slots": slots,
        }))
    }

    #[test]
    fn slot_positions_are_read_when_present_and_optional() {
        let r = record(serde_json::json!([
            {"slot": "s1", "role": "position_1", "count": 2,
             "positions_norm": [[0.074, 0.627], [0.926, 0.627]]},
            {"slot": "s2", "role": "center", "count": 1},
        ]))
        .expect("loads");
        assert_eq!(
            r.slot("s1").and_then(|s| s.positions_norm.clone()),
            Some(vec![[0.074, 0.627], [0.926, 0.627]])
        );
        assert_eq!(r.slot("s2").map(|s| s.positions_norm.is_none()), Some(true));
        assert!(record(Value::Null).expect("null slots").slots.is_empty());
        assert!(
            ProductRecord::from_value(serde_json::json!({
                "schema": "jewelry.option_graph/1", "product_key": "house-a/p1",
                "cart": {"mode": "shopify_lines"},
            }))
            .expect("no slots")
            .slots
            .is_empty()
        );
    }

    #[test]
    fn bad_slot_positions_are_refused_with_the_slot_named() {
        let outside = record(serde_json::json!([
            {"slot": "s1", "role": "center", "count": 1, "positions_norm": [[0.5, 1.5]]},
        ]))
        .expect_err("outside the unit square");
        assert_eq!(outside.reason, "configurator_bad_document");
        assert!(outside.message.contains("`s1` positions_norm[0]"), "{outside}");
        for bad in [
            serde_json::json!([[0.5]]),
            serde_json::json!([[0.1, 0.2, 0.3]]),
            serde_json::json!([["a", 0.2]]),
            serde_json::json!([0.5, 0.5]),
        ] {
            let e = record(serde_json::json!([{"slot": "s1", "role": "center", "count": 1,
                "positions_norm": bad}]))
            .expect_err("not a list of [x, y]");
            assert_eq!(e.reason, "configurator_bad_document", "{bad}");
        }
        let twice = record(serde_json::json!([
            {"slot": "s1", "role": "center", "count": 1},
            {"slot": "s1", "role": "accent", "count": 1},
        ]))
        .expect_err("duplicate slot");
        assert!(twice.message.contains("gem slot `s1` twice"), "{twice}");
    }

    /// Audit (sprint-corr-configurator): groups `Size` and `size` loaded, but picks fold onto the
    /// last spelling, so the first group could never be picked and `open` answered
    /// `configurator_not_orderable`; a label listed twice (`S0`, `s0`) made the second spelling
    /// unpickable the same way. Both are refused on load, naming the group.
    #[test]
    fn names_that_differ_only_by_case_are_refused() {
        let doc = |axes: Value| {
            ProductRecord::from_value(serde_json::json!({
                "schema": "jewelry.option_graph/1", "product_key": "house-a/p1",
                "cart": {"mode": "shopify_lines"}, "axes": axes,
            }))
        };
        let groups = doc(serde_json::json!([
            {"group": "Size", "values": [{"label": "S0"}]},
            {"group": "size", "values": [{"label": "a"}]},
        ]))
        .expect_err("two spellings of one group");
        assert_eq!(groups.reason, "configurator_bad_document");
        assert!(groups.message.contains("`size` twice"), "{groups}");
        let labels = doc(serde_json::json!([
            {"group": "Size", "values": [{"label": "S0"}, {"label": "s0"}]},
        ]))
        .expect_err("two spellings of one label");
        assert!(
            labels.message.contains("`Size` lists the value `s0` twice"),
            "{labels}"
        );
        assert!(
            doc(serde_json::json!([
                {"group": "Size", "values": [{"label": "S0"}, {"label": "S1"}]},
                {"group": "Gauge", "values": [{"label": "S0"}]},
            ]))
            .is_ok(),
            "one label in two groups is fine"
        );
    }

    #[test]
    fn codes_are_numeric_text() {
        assert!(is_numeric_code("47649841250598"));
        assert!(!is_numeric_code("6a"));
        assert!(!is_numeric_code(""));
    }
}
