// SPDX-License-Identifier: MPL-2.0
//! The three file formats (`.attrs.ron`, `.effect.ron`, `.ability.ron`, § 4), their loaders and
//! validators, and the [`AbilityAssets`] registry the runtime reads assets from (keyed by path).

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};
use std::path::Path;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::version;
use serde::{Deserialize, Serialize};

use super::cooked::CookedAbilityAsset;
use super::effects::{EffectAsset, Lifetime, MAX_MODIFIERS_PER_EFFECT};
use super::grants::{AbilityAsset, RunStateMode};
use super::modifiers::{Magnitude, ModOp, Modifier, RunStateTypes};
use super::{AbilityError, AttrId, Attribute, Attributes};
use crate::tags::{Tag, TagQuery, TagRegistry};

/// Current `.attrs.ron` version.
pub const ATTRIBUTES_FORMAT_VERSION: u32 = 1;
/// Current `.effect.ron` version.
pub const EFFECT_FORMAT_VERSION: u32 = 1;
/// Current `.ability.ron` version.
pub const ABILITY_FORMAT_VERSION: u32 = 1;

/// Attributes per entity above which `abilities.validate` warns `attributes_over_budget` (§ 10). A
/// warning: a file that loaded keeps loading.
pub const MAX_ATTRIBUTES_PER_ENTITY: usize = 64;

/// A derived value as authored (input names and weights).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DerivedDecl {
    /// `(attribute name, weight)` pairs.
    pub inputs: Vec<(String, f32)>,
    /// Constant offset.
    #[serde(default)]
    pub offset: f32,
}

/// One authored attribute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttributeDecl {
    /// Name, `[a-z][a-z0-9_]{0,47}`.
    pub name: String,
    /// Starting base.
    pub base: f32,
    /// Lower bound.
    pub min: f32,
    /// Upper bound.
    pub max: f32,
    /// Cap the upper bound by another attribute's `current`.
    #[serde(default)]
    pub max_from: Option<String>,
    /// Derived value.
    #[serde(default)]
    pub derived: Option<DerivedDecl>,
}

/// An authored attribute list (`.attrs.ron`, § 4.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttributesAsset {
    /// Format version (1).
    pub format_version: u32,
    /// What the list is for.
    #[serde(default)]
    pub description: String,
    /// The attributes, in declaration order.
    pub attributes: Vec<AttributeDecl>,
}

impl AttributesAsset {
    /// Builds the component (checks names, duplicates, ranges and cycles).
    pub fn build(&self) -> Result<Attributes, AbilityError> {
        Attributes::from_list(self.attributes.iter().map(|d| {
            let mut a = Attribute::new(&d.name, d.base, d.min, d.max);
            if let Some(other) = &d.max_from {
                a = a.with_max_from(other);
            }
            if let Some(derived) = &d.derived {
                let inputs: Vec<(&str, f32)> = derived.inputs.iter().map(|(n, w)| (n.as_str(), *w)).collect();
                a = a.with_derived(&inputs, derived.offset);
            }
            a
        }))
    }
}

/// `.attrs.ron` migration chain: v1 is the first version, so there are no steps.
#[must_use]
pub fn attributes_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// `.effect.ron` migration chain: v1 is the first version, so there are no steps.
#[must_use]
pub fn effect_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// `.ability.ron` migration chain: v1 is the first version, so there are no steps.
#[must_use]
pub fn ability_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape fingerprint of `.attrs.ron` (for `cargo xtask check-formats`).
#[must_use]
pub fn attributes_schema_fingerprint() -> String {
    use regia_core::schema::serde_field_names;
    regia_core::schema::join_field_lists(&[
        serde_field_names::<AttributesAsset>(),
        serde_field_names::<AttributeDecl>(),
        serde_field_names::<DerivedDecl>(),
    ])
}

/// The serde shape fingerprint of `.effect.ron`.
#[must_use]
pub fn effect_schema_fingerprint() -> String {
    use regia_core::schema::{serde_field_names, serde_variant_names};
    regia_core::schema::join_field_lists(&[
        serde_field_names::<EffectAsset>(),
        serde_field_names::<Modifier>(),
        serde_variant_names::<Magnitude>(),
        serde_variant_names::<ModOp>(),
        serde_variant_names::<Lifetime>(),
        serde_field_names::<super::effects::Pulse>(),
        serde_field_names::<super::effects::Chain>(),
        serde_field_names::<super::effects::EffectGrant>(),
        serde_field_names::<super::effects::SuspendRule>(),
        serde_field_names::<super::stacking::StackRule>(),
        serde_field_names::<super::stacking::OnAdd>(),
        serde_variant_names::<super::stacking::WhenFull>(),
        serde_variant_names::<super::stacking::OnLapse>(),
        serde_field_names::<super::cues::CueBinding>(),
        serde_variant_names::<super::cues::CueAt>(),
        serde_field_names::<super::modifiers::InlineCurve>(),
        serde_variant_names::<TagQuery>(),
    ])
}

/// The serde shape fingerprint of `.ability.ron`.
#[must_use]
pub fn ability_schema_fingerprint() -> String {
    use regia_core::schema::{serde_field_names, serde_variant_names};
    regia_core::schema::join_field_lists(&[
        serde_field_names::<AbilityAsset>(),
        serde_variant_names::<super::grants::Trigger>(),
        serde_variant_names::<super::grants::ChargeMode>(),
        serde_variant_names::<RunStateMode>(),
        serde_variant_names::<super::grants::WhenRunning>(),
        serde_variant_names::<super::grants::RunSite>(),
        serde_field_names::<super::cues::CueBinding>(),
        serde_variant_names::<TagQuery>(),
    ])
}

/// How bad a diagnostic is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    /// The asset is refused.
    Error,
    /// The asset loads; a reviewer should look.
    Warning,
}

impl Severity {
    /// Lowercase name.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One validation finding (§ 4.5).
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    /// Lowercase code.
    pub code: &'static str,
    /// Error or warning.
    pub severity: Severity,
    /// What is wrong.
    pub message: String,
    /// `(line, column)` in the file, when known.
    pub span: Option<(usize, usize)>,
}

impl Diagnostic {
    pub(crate) fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: Severity::Error,
            message: message.into(),
            span: None,
        }
    }

    fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: Severity::Warning,
            message: message.into(),
            span: None,
        }
    }
}

/// Parses one of the formats: a newer `format_version` is refused with `format_too_new`; a shape
/// error (an unknown field, a typo) is `unknown_field` or `parse_error` with a span.
pub fn parse_ron<T: for<'de> Deserialize<'de>>(text: &str, current: u32) -> Result<T, Diagnostic> {
    if let Ok(found) = version::peek_ron_version(text)
        && found > current
    {
        return Err(Diagnostic::error(
            "format_too_new",
            format!("format_version {found} is newer than {current}; upgrade the engine"),
        ));
    }
    ron::from_str(text).map_err(|e: ron::error::SpannedError| {
        let code = if matches!(e.code, ron::Error::NoSuchStructField { .. }) {
            "unknown_field"
        } else {
            "parse_error"
        };
        Diagnostic {
            code,
            severity: Severity::Error,
            message: e.code.to_string(),
            span: Some((e.span.start.line, e.span.start.col)),
        }
    })
}

/// Writes one of the formats canonically (pretty RON, trailing newline).
pub fn to_ron<T: Serialize>(value: &T) -> Result<String, AbilityError> {
    let mut text = ron::ser::to_string_pretty(value, regia_core::ron_pretty())
        .map_err(|e| AbilityError::Format(e.to_string()))?;
    text.push('\n');
    Ok(text)
}

/// An effect as [`AbilityAssets`] keeps it: the asset and what the recompute pass reads of it every
/// step, worked out once at insert (spec 53 § 10 budget).
#[derive(Debug, Clone)]
pub(crate) struct EffectEntry {
    /// The asset.
    pub(crate) asset: Arc<EffectAsset>,
    /// `AttrId::of` of each live modifier's attribute, in `live_mods` order.
    pub(crate) live_attributes: Box<[AttrId]>,
    /// Whether every live modifier reads only its own entry: no `when` query and an amount
    /// captured at application (not [`Magnitude::is_live`]). The recompute pass may skip an
    /// entity whose active effects are all fixed and unchanged (ADR-2012).
    pub(crate) fixed_mods: bool,
}

impl EffectEntry {
    fn new(asset: EffectAsset) -> Self {
        let live_attributes = asset.live_mods.iter().map(|m| AttrId::of(&m.attribute)).collect();
        let fixed_mods = asset
            .live_mods
            .iter()
            .all(|m| m.when.is_none() && !m.amount.is_live());
        Self {
            asset: Arc::new(asset),
            live_attributes,
            fixed_mods,
        }
    }
}

/// A fixed (unseeded, so the same on every run and platform) word-at-a-time hash for asset paths:
/// each 8-byte word is mixed in by rotate, xor and an odd multiply, the scheme of the public FxHash.
/// The effect map is looked up twice per live effect per step (spec 53 § 10 budget); an ordered map
/// of strings cost about 23 ns a lookup there, this about a quarter (ADR-2012).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PathHasher(u64);

impl PathHasher {
    const SEED: u64 = 0x517c_c1b7_2722_0a95;

    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(Self::SEED);
    }
}

impl Hasher for PathHasher {
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            self.add(u64::from_le_bytes(*word));
        }
        let mut tail = [0u8; 8];
        tail[..rest.len()].copy_from_slice(rest);
        // The length keeps "a" and "a\0" apart.
        self.add(u64::from_le_bytes(tail) ^ ((rest.len() as u64) << 59));
    }

    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

thread_local! {
    static ABILITY_CLONE_COUNT: Cell<u64> = const { Cell::new(0) };
}

/// How many times [`AbilityAssets::ability`] cloned an ability handle on this thread.
///
/// Grant setup clones once. An idle fixed step, with no asset reload, does not move this.
#[must_use]
pub fn ability_clone_count() -> u64 {
    ABILITY_CLONE_COUNT.with(|count| count.get())
}

/// The assets the runtime reads, keyed by asset path (§ 4; effects and abilities refer to each
/// other by path).
#[derive(Resource, Debug, Clone, Default)]
pub struct AbilityAssets {
    /// Hashed, not ordered: the per-step passes look effects up by path (spec 53 § 10 budget);
    /// [`AbilityAssets::effect_paths`] sorts.
    effects: HashMap<String, EffectEntry, BuildHasherDefault<PathHasher>>,
    abilities: BTreeMap<String, Arc<AbilityAsset>>,
    attributes: BTreeMap<String, Arc<AttributesAsset>>,
    /// Restamped on every [`AbilityAssets::insert_ability`] and [`AbilityAssets::insert_effect`].
    /// Grant step caches and held `WhileTagged` refusals store it and refresh when it moves (a cost
    /// or cooldown effect reload changes the activation check too). The stamps are process-wide
    /// (`next_generation`), so a fresh resource that replaces this one never repeats a stamp the
    /// caches hold (a per-resource counter restarted at 0; corr-framework-a L5).
    generation: u64,
}

/// A process-wide asset generation stamp (never 0, the stamp of an empty default resource).
fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl AbilityAssets {
    /// Adds or replaces an effect.
    pub fn insert_effect(&mut self, path: &str, asset: EffectAsset) {
        self.generation = next_generation();
        self.effects.insert(path.to_owned(), EffectEntry::new(asset));
    }

    /// Adds or replaces an ability.
    pub fn insert_ability(&mut self, path: &str, asset: AbilityAsset) {
        self.generation = next_generation();
        self.abilities.insert(path.to_owned(), Arc::new(asset));
    }

    /// Adds or replaces an attribute list.
    pub fn insert_attributes(&mut self, path: &str, asset: AttributesAsset) {
        self.attributes.insert(path.to_owned(), Arc::new(asset));
    }

    /// The effect at `path`.
    #[must_use]
    pub fn effect(&self, path: &str) -> Option<Arc<EffectAsset>> {
        self.effects.get(path).map(|entry| Arc::clone(&entry.asset))
    }

    /// The effect at `path` with its per-step data, borrowed (no reference count moves).
    pub(crate) fn effect_entry(&self, path: &str) -> Option<&EffectEntry> {
        self.effects.get(path)
    }

    /// The ability at `path`.
    #[must_use]
    pub fn ability(&self, path: &str) -> Option<Arc<AbilityAsset>> {
        let found = self.abilities.get(path).cloned();
        if found.is_some() {
            ABILITY_CLONE_COUNT.with(|count| count.set(count.get().saturating_add(1)));
        }
        found
    }

    /// The ability at `path`, borrowed. Does not move the reference count and does not count as
    /// an [`AbilityAssets::ability`] clone.
    pub(crate) fn ability_ref(&self, path: &str) -> Option<&AbilityAsset> {
        self.abilities.get(path).map(Arc::as_ref)
    }

    /// The generation [`AbilityAssets::insert_ability`] or [`AbilityAssets::insert_effect`] last
    /// published.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// The attribute list at `path`.
    #[must_use]
    pub fn attributes(&self, path: &str) -> Option<Arc<AttributesAsset>> {
        self.attributes.get(path).cloned()
    }

    /// Effect paths, sorted.
    pub fn effect_paths(&self) -> impl Iterator<Item = &str> {
        let mut paths: Vec<&str> = self.effects.keys().map(String::as_str).collect();
        paths.sort_unstable();
        paths.into_iter()
    }

    /// Ability paths, sorted.
    pub fn ability_paths(&self) -> impl Iterator<Item = &str> {
        self.abilities.keys().map(String::as_str)
    }

    /// Parses `text` as the format its path names and adds it; errors refuse the asset (warnings
    /// do not). Returns every diagnostic.
    pub fn load_text(
        &mut self,
        path: &str,
        text: &str,
        tags: Option<&TagRegistry>,
        run_types: Option<&RunStateTypes>,
    ) -> Vec<Diagnostic> {
        match parse_asset(path, text) {
            Ok(asset) => self.validate_and_insert(path, asset, tags, run_types),
            Err(d) => vec![d],
        }
    }

    /// Validates `asset` as its format's text load does (§ 4.5) and adds it under `path` unless a
    /// diagnostic is an error. Returns every diagnostic, errors first.
    pub(crate) fn validate_and_insert(
        &mut self,
        path: &str,
        asset: CookedAbilityAsset,
        tags: Option<&TagRegistry>,
        run_types: Option<&RunStateTypes>,
    ) -> Vec<Diagnostic> {
        let diags = self.validate_any(&asset, tags, run_types);
        if !has_errors(&diags) {
            match asset {
                CookedAbilityAsset::Attributes(a) => self.insert_attributes(path, a),
                CookedAbilityAsset::Effect(a) => self.insert_effect(path, a),
                CookedAbilityAsset::Ability(a) => self.insert_ability(path, a),
            }
        }
        diags
    }

    /// The § 4.5 diagnostics of `asset` against this registry, errors first.
    pub(crate) fn validate_any(
        &self,
        asset: &CookedAbilityAsset,
        tags: Option<&TagRegistry>,
        run_types: Option<&RunStateTypes>,
    ) -> Vec<Diagnostic> {
        let mut diags = match asset {
            CookedAbilityAsset::Attributes(a) => validate_attributes(a),
            CookedAbilityAsset::Effect(a) => validate_effect(a, tags),
            CookedAbilityAsset::Ability(a) => validate_ability(a, self, tags, run_types),
        };
        diags.sort_by_key(|d| d.severity == Severity::Warning);
        diags
    }

    /// Reads and adds a file (`path` is both the file and the key).
    pub fn load_file(
        &mut self,
        path: &Path,
        tags: Option<&TagRegistry>,
        run_types: Option<&RunStateTypes>,
    ) -> Result<Vec<Diagnostic>, AbilityError> {
        let text = std::fs::read_to_string(path)
            .map_err(|_| AbilityError::AssetNotFound(path.display().to_string()))?;
        Ok(self.load_text(&path.to_string_lossy(), &text, tags, run_types))
    }
}

/// Which format a path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    /// `.attrs.ron`.
    Attributes,
    /// `.effect.ron`.
    Effect,
    /// `.ability.ron`.
    Ability,
}

impl AssetKind {
    /// The format of `path`, by extension.
    #[must_use]
    pub fn of(path: &str) -> Option<Self> {
        if path.ends_with(".attrs.ron") {
            Some(Self::Attributes)
        } else if path.ends_with(".effect.ron") {
            Some(Self::Effect)
        } else if path.ends_with(".ability.ron") {
            Some(Self::Ability)
        } else {
            None
        }
    }
}

/// Parses `text` as the format `path` names (`parse_error` for a path that names none).
pub(crate) fn parse_asset(path: &str, text: &str) -> Result<CookedAbilityAsset, Diagnostic> {
    match AssetKind::of(path) {
        Some(AssetKind::Attributes) => {
            parse_ron(text, ATTRIBUTES_FORMAT_VERSION).map(CookedAbilityAsset::Attributes)
        }
        Some(AssetKind::Effect) => parse_ron(text, EFFECT_FORMAT_VERSION).map(CookedAbilityAsset::Effect),
        Some(AssetKind::Ability) => parse_ron(text, ABILITY_FORMAT_VERSION).map(CookedAbilityAsset::Ability),
        None => Err(Diagnostic::error(
            "parse_error",
            format!("`{path}` is not .attrs.ron, .effect.ron or .ability.ron"),
        )),
    }
}

/// Whether any diagnostic is an error.
#[must_use]
pub fn has_errors(diags: &[Diagnostic]) -> bool {
    diags.iter().any(|d| d.severity == Severity::Error)
}

/// Validates an attribute list.
#[must_use]
pub fn validate_attributes(asset: &AttributesAsset) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for d in &asset.attributes {
        if d.min > d.max {
            out.push(Diagnostic::error(
                "attribute_range",
                format!("`{}`: min {} > max {}", d.name, d.min, d.max),
            ));
        }
    }
    if let Err(e) = asset.build() {
        let code = e.reason();
        if code != "attribute_range" {
            out.push(Diagnostic::error(code, e.to_string()));
        }
    }
    if asset.attributes.len() > MAX_ATTRIBUTES_PER_ENTITY {
        out.push(Diagnostic::warning(
            "attributes_over_budget",
            format!(
                "{} attributes; the budget is {MAX_ATTRIBUTES_PER_ENTITY} per entity",
                asset.attributes.len()
            ),
        ));
    }
    out
}

fn check_tags(tags: &[Tag], registry: Option<&TagRegistry>, out: &mut Vec<Diagnostic>) {
    let Some(registry) = registry else {
        return;
    };
    for t in tags {
        if !registry.contains(*t) {
            out.push(Diagnostic::error(
                "undeclared_tag",
                format!("tag `{t}` is not declared"),
            ));
        }
    }
}

fn query_tags(q: &Option<TagQuery>) -> Vec<Tag> {
    q.as_ref().map(TagQuery::tags).unwrap_or_default()
}

/// Validates an effect (§ 4.5). Attribute names are checked for shape only (a modifier naming an
/// attribute the target lacks is skipped at application with `unknown_attribute`).
#[must_use]
pub fn validate_effect(asset: &EffectAsset, registry: Option<&TagRegistry>) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let modifiers = asset.base_edits.len() + asset.live_mods.len();
    if modifiers > MAX_MODIFIERS_PER_EFFECT {
        out.push(Diagnostic::warning(
            "effect_over_budget",
            format!("{modifiers} modifiers; the budget is {MAX_MODIFIERS_PER_EFFECT} per effect"),
        ));
    }
    if asset.is_instant() {
        if !asset.live_mods.is_empty() {
            out.push(Diagnostic::error(
                "effect_instant_live_mods",
                "an Instant effect cannot have live_mods",
            ));
        }
        if asset.pulse.is_some() {
            out.push(Diagnostic::error(
                "effect_pulse_on_instant",
                "an Instant effect cannot pulse",
            ));
        }
    }
    if let Lifetime::Timed(Magnitude::Const(s)) = &asset.lifetime
        && !(s.is_finite() && *s > 0.0)
    {
        out.push(Diagnostic::error(
            "effect_bad_lifetime",
            "a Timed length must be a finite number of seconds above 0",
        ));
    }
    if let Some(p) = asset.pulse
        && !(p.every.is_finite() && p.every > 0.0)
    {
        out.push(Diagnostic::error(
            "effect_bad_lifetime",
            "pulse.every must be above 0 seconds",
        ));
    }
    if !(0.0..=1.0).contains(&asset.chance) {
        out.push(Diagnostic::error(
            "effect_bad_lifetime",
            "chance must be in 0..=1",
        ));
    }
    if asset.stack.as_ref().is_some_and(|s| s.cap == 0) {
        out.push(Diagnostic::error(
            "stack_cap_zero",
            "stack.cap must be at least 1",
        ));
    }
    for m in asset.base_edits.iter().chain(&asset.live_mods) {
        if !super::attributes::valid_attribute_name(&m.attribute) {
            out.push(Diagnostic::error(
                "effect_unknown_attribute",
                format!("`{}` is not a valid attribute name", m.attribute),
            ));
        }
        if m.op == ModOp::Scale
            && let Magnitude::Const(v) = m.amount
            && v < 0.0
        {
            out.push(Diagnostic::warning(
                "modifier_negative_scale",
                format!("Scale {v} on `{}` flips the sign", m.attribute),
            ));
        }
        if let Magnitude::Curve(r) = &m.amount
            && r.table.is_empty()
            && !asset.curves.iter().any(|c| c.name == r.name)
        {
            out.push(Diagnostic::error(
                "reference_missing",
                format!("inline curve `{}` is not in `curves`", r.name),
            ));
        }
    }
    for c in &asset.curves {
        if !c.is_sorted() {
            out.push(Diagnostic::error(
                "curve_unsorted",
                format!(
                    "curve `{}` needs at least one point and strictly increasing x",
                    c.name
                ),
            ));
        }
    }
    let mut tags: Vec<Tag> = asset.status_tags.clone();
    tags.extend(&asset.kind_tags);
    tags.extend(query_tags(&asset.apply_if));
    tags.extend(query_tags(&asset.active_if));
    for q in asset.immunity.iter().chain(&asset.cleanses) {
        tags.extend(q.tags());
    }
    tags.extend(asset.cues.iter().map(|c| c.tag));
    check_tags(&tags, registry, &mut out);
    out
}

/// Validates an ability (§ 4.5); `assets` resolves the cost and cooldown references.
#[must_use]
pub fn validate_ability(
    asset: &AbilityAsset,
    assets: &AbilityAssets,
    registry: Option<&TagRegistry>,
    run_types: Option<&RunStateTypes>,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if let Some(cost) = &asset.cost {
        match assets.effect(cost) {
            None => out.push(Diagnostic::error(
                "reference_missing",
                format!("cost `{cost}` is not loaded"),
            )),
            Some(e) if !e.is_instant() => out.push(Diagnostic::error(
                "ability_bad_cost",
                format!("cost `{cost}` must be an Instant effect"),
            )),
            Some(_) => {}
        }
    }
    if let Some(cd) = &asset.cooldown {
        match assets.effect(cd) {
            None => out.push(Diagnostic::error(
                "reference_missing",
                format!("cooldown `{cd}` is not loaded"),
            )),
            Some(e) if !matches!(e.lifetime, Lifetime::Timed(_)) || e.status_tags.is_empty() => {
                out.push(Diagnostic::error(
                    "ability_bad_cooldown",
                    format!("cooldown `{cd}` must be a Timed effect with status_tags"),
                ));
            }
            Some(_) => {}
        }
    }
    if asset.run_state != RunStateMode::ScratchOnly && !run_types.is_some_and(|t| t.contains(&asset.run_type))
    {
        out.push(Diagnostic::error(
            "unknown_run_state",
            format!("run_type `{}` is not registered", asset.run_type),
        ));
    }
    if asset.max_run_ticks.is_none() {
        out.push(Diagnostic::warning(
            "ability_no_max_run_ticks",
            "no max_run_ticks: a driver must end every run",
        ));
    }
    let mut tags: Vec<Tag> = asset.kind_tags.clone();
    tags.extend(&asset.run_tags);
    for q in [
        &asset.cancels,
        &asset.blocks,
        &asset.owner_needs,
        &asset.owner_forbids,
        &asset.target_needs,
        &asset.target_forbids,
    ] {
        tags.extend(query_tags(q));
    }
    tags.extend(asset.cues.iter().map(|c| c.tag));
    check_tags(&tags, registry, &mut out);
    out
}

/// The effect at `path` in the world's [`AbilityAssets`].
pub(crate) fn effect(world: &World, path: &str) -> Option<Arc<EffectAsset>> {
    world.get_resource::<AbilityAssets>()?.effect(path)
}

/// The ability at `path` in the world's [`AbilityAssets`].
pub(crate) fn ability(world: &World, path: &str) -> Option<Arc<AbilityAsset>> {
    world.get_resource::<AbilityAssets>()?.ability(path)
}

#[cfg(test)]
mod path_hash_tests {
    use std::hash::{BuildHasher, BuildHasherDefault};

    use super::*;

    fn hash(text: &str) -> u64 {
        BuildHasherDefault::<PathHasher>::default().hash_one(text)
    }

    #[test]
    fn lengths_and_word_edges_hash_apart() {
        let texts = [
            "",
            "\0",
            "a",
            "a\0",
            "\0a",
            "abcdefg",
            "abcdefgh",
            "abcdefgh\0",
            "abcdefghi",
            "ab",
            "ba",
        ];
        for (i, a) in texts.iter().enumerate() {
            for b in &texts[i + 1..] {
                assert_ne!(hash(a), hash(b), "{a:?} and {b:?}");
            }
        }
        assert_eq!(hash("effects/burn.effect.ron"), hash("effects/burn.effect.ron"));
    }

    #[test]
    fn every_effect_is_found_and_paths_list_sorted() {
        let mut assets = AbilityAssets::default();
        // A fixed scramble of 0..500 (multiplying by a unit mod 500).
        let paths: Vec<String> = (0..500u32)
            .map(|i| format!("effects/e{}.effect.ron", (i * 317) % 500))
            .collect();
        for (i, path) in paths.iter().enumerate() {
            assets.insert_effect(path, EffectAsset::new(Lifetime::UntilRemoved));
            assert_eq!(assets.effect_paths().count(), i + 1);
        }
        for path in &paths {
            assert!(assets.effect(path).is_some(), "{path}");
            assert!(assets.effect_entry(path).is_some(), "{path}");
        }
        assert!(assets.effect("effects/e500.effect.ron").is_none());
        assert!(assets.effect("").is_none());
        let listed: Vec<&str> = assets.effect_paths().collect();
        let mut sorted: Vec<&str> = paths.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        assert_eq!(listed, sorted);
        // Replacing keeps one entry per path.
        assets.insert_effect(&paths[0], EffectAsset::new(Lifetime::Instant));
        assert_eq!(assets.effect_paths().count(), 500);
        assert!(assets.effect(&paths[0]).is_some_and(|a| a.is_instant()));
    }
}
