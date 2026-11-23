// SPDX-License-Identifier: MPL-2.0
//! The Abilities inspector (`docs/specs/53-abilities-attributes-effects.md` § 6, ADR-2031): an entity's
//! `Attributes` (base editable, current with its breakdown), `AppliedEffects` (with a remove button) and
//! `AbilityBook` (grant, revoke; start while playing). Agent surface: `editor.abilities_panel_*`
//! (`book/agents/editor-data-panels.md`).
//!
//! The panel reads the play world while a play session runs and the edit world otherwise. Each editor frame
//! [`pump`] runs the operations the buttons and the agent queued on that world (with the
//! `regia_framework::abilities` calls `abilities.*` uses) and takes a snapshot, which the panel draws and
//! `editor.abilities_panel_get` returns. Without a play session an agent call runs at once.
//!
//! The debug overlay group `abilities` (off by default) draws a state ring under every entity with an
//! ability book (ready green, running yellow, cooling blue) and one tick above it per live effect.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_framework::abilities::{
    self as ab, AbilityAssets, AbilityBook, AbilityState, AppliedEffects, AttrId, AttributeBreakdown,
    Attributes, EffectId, GrantId, GrantOptions, StartContext,
};
use regia_scene::prelude::{GlobalTransform, Name, Transform};
use serde_json::{Value, json};

use crate::audio_gizmo::Segment;
use crate::data_panels::{PanelError, opt_f32, str_param};
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/editor-data-panels.md";

pub mod form;
mod undo;

/// Most entities the list shows (in id order).
pub const MAX_LISTED: usize = 256;

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("abilities_panel_{code}"), message)
}

fn from_ability_error(e: &ab::AbilityError) -> PanelError {
    PanelError::new(e.reason(), e.to_string())
}

/// Which world the snapshot came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Source {
    /// The edit world.
    #[default]
    Edit,
    /// The play-in-editor world.
    Play,
}

impl Source {
    /// `edit` or `play`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Play => "play",
        }
    }
}

/// One attribute row.
#[derive(Clone, Debug, PartialEq)]
pub struct AttributeRow {
    /// Name.
    pub name: String,
    /// Base value.
    pub base: f32,
    /// Current value.
    pub current: f32,
    /// Clamp minimum.
    pub min: f32,
    /// Clamp maximum.
    pub max: f32,
    /// Whether it is derived (its base cannot be edited).
    pub derived: bool,
    /// The breakdown, one line per phase and modifier (the hover text).
    pub breakdown: Vec<String>,
}

/// One live effect row.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectRow {
    /// Effect id.
    pub id: u64,
    /// Asset path.
    pub asset: String,
    /// Level.
    pub level: u32,
    /// Stack count.
    pub stacks: u32,
    /// Ticks left, for a timed effect.
    pub remaining_ticks: Option<u32>,
    /// Whether it is suspended.
    pub suspended: bool,
}

/// One grant row.
#[derive(Clone, Debug, PartialEq)]
pub struct GrantRow {
    /// Grant id.
    pub id: u32,
    /// Ability asset path.
    pub asset: String,
    /// Level.
    pub level: u32,
    /// `ready`, `running`, `ending`, `cooling` (or `revoked`).
    pub state: &'static str,
}

/// An entity with attributes or an ability book, as the list and the overlay see it.
#[derive(Clone, Debug, PartialEq)]
pub struct ListedEntity {
    /// The entity, in the snapshot's world.
    pub entity: Entity,
    /// Its name.
    pub name: Option<String>,
    /// Its world position.
    pub position: Option<Vec3>,
    /// The ring state: `running` when a grant runs, else `cooling` when one cools down, else `ready`.
    pub state: &'static str,
    /// Live effects.
    pub effects: usize,
}

/// What the inspector shows of one world.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// The world it came from.
    pub source: Source,
    /// Whether the world runs the ability plugin (`AbilityAssets` present).
    pub available: bool,
    /// Entities with attributes or an ability book, by id (at most [`MAX_LISTED`]).
    pub entities: Vec<ListedEntity>,
    /// The inspected entity.
    pub entity: Option<Entity>,
    /// Its attributes.
    pub attributes: Vec<AttributeRow>,
    /// Its live effects.
    pub effects: Vec<EffectRow>,
    /// Its grants.
    pub grants: Vec<GrantRow>,
    /// Loaded ability assets (what `grant` accepts).
    pub abilities: Vec<String>,
    /// Loaded effect assets (what `apply_effect` accepts), sorted.
    pub effect_assets: Vec<String>,
}

/// What the last operation did.
#[derive(Clone, Debug, PartialEq)]
pub struct OpRecord {
    /// The operation.
    pub op: String,
    /// The world it ran on.
    pub source: Source,
    /// Its answer, on success.
    pub answer: Option<Value>,
    /// Its error, on failure.
    pub error: Option<PanelError>,
}

impl OpRecord {
    fn json(&self) -> Value {
        json!({
            "op": self.op,
            "source": self.source.name(),
            "ok": self.error.is_none(),
            "answer": self.answer,
            "error": self.error.as_ref().map(PanelError::json),
        })
    }
}

/// An `.attrs.ron`, `.effect.ron` or `.ability.ron` open in the panel's asset editor.
#[derive(Clone, Debug, PartialEq)]
pub struct AssetDoc {
    /// Path under `assets/` (also the asset's key, as cost and cooldown references name it).
    pub path: String,
    /// The text being edited.
    pub text: String,
    /// The text on disk.
    pub saved: String,
    /// What validating `text` found (the checks of `abilities.validate`).
    pub diagnostics: Vec<ab::Diagnostic>,
}

impl AssetDoc {
    /// The typed form's value tree of the text (`None` while the text does not parse).
    #[must_use]
    pub fn form(&self) -> Option<Value> {
        form::form_value(form::FormKind::of(&self.path)?, &self.text).ok()
    }

    /// The top-level form field each diagnostic is shown beside (`None`: the asset as a whole),
    /// in diagnostic order.
    #[must_use]
    pub fn diagnostic_fields(&self, tree: Option<&Value>) -> Vec<Option<String>> {
        let kind = form::FormKind::of(&self.path);
        let fields = tree.map(form::top_fields).unwrap_or_default();
        self.diagnostics
            .iter()
            .map(|d| kind.and_then(|kind| form::diagnostic_field(kind, d, fields.iter().copied())))
            .collect()
    }

    fn json(&self) -> Value {
        let tree = self.form();
        let fields = self.diagnostic_fields(tree.as_ref());
        json!({
            "path": self.path,
            "dirty": self.text != self.saved,
            "ok": !ab::has_errors(&self.diagnostics),
            "text": self.text,
            "comments_warning": form::has_comments(&self.text).then_some(form::COMMENTS_WARNING),
            "form": tree,
            "diagnostics": self.diagnostics.iter().zip(fields).map(|(d, field)| json!({
                "code": d.code, "severity": d.severity.code(), "message": d.message,
                "span": d.span.map(|(l, c)| json!({ "line": l, "column": c })),
                "field": field,
            })).collect::<Vec<_>>(),
        })
    }
}

const ASSET_SUFFIXES: &[&str] = &[".attrs.ron", ".effect.ron", ".ability.ron"];

/// Validates `text` as the asset at `path` (under `assets/`) like `abilities.validate`: against the
/// world's loaded assets, tags and run-state types, with the effects an ability names read from the project
/// when the world has not loaded them.
#[must_use]
pub fn validate_asset(
    world: &World,
    assets_dir: Option<&std::path::Path>,
    path: &str,
    text: &str,
) -> Vec<ab::Diagnostic> {
    let _span = tracing::debug_span!(target: "regia_editor", "abilities_panel_validate").entered();
    let mut scratch = world.get_resource::<AbilityAssets>().cloned().unwrap_or_default();
    let tags = world.get_resource::<regia_framework::tags::TagRegistry>();
    let run_types = world.get_resource::<ab::RunStateTypes>();
    if path.ends_with(".ability.ron")
        && let (Ok(ability), Some(dir)) = (
            ab::parse_ron::<ab::AbilityAsset>(text, ab::ABILITY_FORMAT_VERSION),
            assets_dir,
        )
    {
        for reference in ability.cost.iter().chain(&ability.cooldown) {
            if scratch.effect(reference).is_some() {
                continue;
            }
            if let Ok(file) =
                crate::data_panels::confined(dir, reference, &[".effect.ron"], "abilities_panel")
                && let Ok(effect) = std::fs::read_to_string(file)
            {
                scratch.load_text(reference, &effect, tags, run_types);
            }
        }
    }
    scratch.load_text(path, text, tags, run_types)
}

/// The Abilities inspector's state (an edit-world resource).
#[derive(Resource, Default)]
pub struct AbilitiesPanel {
    /// The pinned entity (in the snapshot's world); `None` follows the editor selection (edit) or the first
    /// listed entity (play).
    pub pinned: Option<Entity>,
    /// Whether the `abilities` overlay group draws.
    pub overlay: bool,
    /// The last snapshot.
    pub snapshot: Snapshot,
    /// Operations waiting for the next [`pump`].
    pub pending: Vec<Value>,
    /// What the last operation did.
    pub last: Option<OpRecord>,
    /// The asset editor's open file.
    pub asset: Option<AssetDoc>,
    grant_input: String,
    /// The attribute set the Add attributes button adds (a path under `assets/`).
    attrs_input: String,
    /// The effect the Apply effect button applies.
    effect_input: String,
    base_text: std::collections::BTreeMap<String, f32>,
    asset_input: String,
    /// The asset editor shows the raw RON text instead of the typed form (ADR-7472).
    advanced: bool,
    /// The last refused typed edit: (field, message), drawn beside that field.
    field_error: Option<(String, String)>,
    /// The egui frame that last applied a form edit: a second pass of the same frame (egui may run the
    /// UI twice) sees the same click and must not add or remove a second entry.
    form_frame: Option<u64>,
    /// A typed edit rewrote the open file's text, which had comments, since it was opened or saved.
    comments_dropped: bool,
    /// The open text's enum paths and their variants (computed again when the text changes).
    form_enums: Option<(
        String,
        std::collections::BTreeMap<String, &'static [&'static str]>,
    )>,
    /// The open asset's undo and redo texts (sprint w11-forms-undo-autosave): every typed form commit
    /// and every agent `text` is one step; a run of keystrokes in the raw text editor is one step.
    asset_history: AssetHistory,
    /// The egui frame that last ran an Undo or Redo click (egui may run the UI twice a frame).
    history_frame: Option<u64>,
}

/// Most undo steps the asset editor keeps (the oldest is dropped first).
pub const ASSET_UNDO_DEPTH: usize = 128;

/// The asset editor's undo and redo texts.
#[derive(Debug, Default)]
struct AssetHistory {
    undo: Vec<String>,
    redo: Vec<String>,
    /// The last step came from typing in the raw text editor: the next keystroke joins it.
    typing: bool,
}

impl AssetHistory {
    /// Records `before` (the text an edit replaced) as one step; `typing` joins a run of
    /// keystrokes into the step its first keystroke made.
    fn record(&mut self, before: &str, typing: bool) {
        if !(typing && self.typing) {
            if self.undo.len() == ASSET_UNDO_DEPTH {
                self.undo.remove(0);
            }
            self.undo.push(before.to_owned());
        }
        self.redo.clear();
        self.typing = typing;
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// One form control's edit: every one is a [`form::set_field`] of the value it makes.
#[derive(Clone, Debug, PartialEq)]
enum FormEdit {
    /// Set the value at the path.
    Set(String, Value),
    /// Append a default-filled entry to the list at the path.
    Add(String),
    /// Remove entry `.1` of the list at the path.
    Remove(String, usize),
    /// Switch the enum at the path to the variant.
    Variant(String, String),
}

impl AbilitiesPanel {
    /// The panel as the agent reads it.
    #[must_use]
    pub fn json(&self) -> Value {
        let s = &self.snapshot;
        json!({
            "source": s.source.name(),
            "available": s.available,
            "pinned": self.pinned.map(Entity::to_bits),
            "overlay": self.overlay,
            "pending": self.pending.len(),
            "entity": s.entity.map(Entity::to_bits),
            "entities": s.entities.iter().map(|e| json!({
                "entity": e.entity.to_bits(), "name": e.name, "state": e.state, "effects": e.effects,
                "position": e.position.map(|p| [p.x, p.y, p.z]),
            })).collect::<Vec<_>>(),
            "attributes": s.attributes.iter().map(|a| json!({
                "name": a.name, "base": a.base, "current": a.current, "min": a.min, "max": a.max,
                "derived": a.derived, "breakdown": a.breakdown,
            })).collect::<Vec<_>>(),
            "effects": s.effects.iter().map(|e| json!({
                "id": e.id, "asset": e.asset, "level": e.level, "stacks": e.stacks,
                "remaining_ticks": e.remaining_ticks, "suspended": e.suspended,
            })).collect::<Vec<_>>(),
            "grants": s.grants.iter().map(|g| json!({
                "id": g.id, "asset": g.asset, "level": g.level, "state": g.state,
            })).collect::<Vec<_>>(),
            "abilities": s.abilities,
            "effect_assets": s.effect_assets,
            "last": self.last.as_ref().map(OpRecord::json),
            "asset": self.asset.as_ref().map(AssetDoc::json),
        })
    }
}

fn position(world: &World, e: Entity) -> Option<Vec3> {
    world
        .get::<GlobalTransform>(e)
        .map(|g| Vec3::from(g.0.translation))
        .filter(|p| *p != Vec3::ZERO)
        .or_else(|| world.get::<Transform>(e).map(|t| t.translation))
}

fn ring_state(world: &World, e: Entity) -> &'static str {
    let states: Vec<AbilityState> = world
        .get::<AbilityBook>(e)
        .map(|b| {
            b.grants
                .iter()
                .filter_map(|g| ab::ability_state(world, e, g.id))
                .collect()
        })
        .unwrap_or_default();
    if states
        .iter()
        .any(|s| matches!(s, AbilityState::Running | AbilityState::Ending))
    {
        "running"
    } else if states.contains(&AbilityState::Cooling) {
        "cooling"
    } else {
        "ready"
    }
}

fn breakdown_lines(b: &AttributeBreakdown) -> Vec<String> {
    let mut out = vec![format!("base {}", b.base)];
    for phase in &b.phases {
        for m in &phase.modifiers {
            out.push(format!(
                "  {} {} x{} (effect {}) -> {:+}",
                m.op.code(),
                m.amount,
                m.stacks,
                m.effect.0,
                m.contribution
            ));
        }
        out.push(format!("{}: {}", phase.phase, phase.value_after));
    }
    if b.unclamped != b.current {
        out.push(format!(
            "clamped {} to [{}, {}]",
            b.unclamped, b.clamp.0, b.clamp.1
        ));
    }
    out.push(format!("current {}", b.current));
    out
}

/// The snapshot of `world` for `wanted` (else the first listed entity).
#[must_use]
pub fn snapshot(world: &mut World, source: Source, wanted: Option<Entity>) -> Snapshot {
    let _span = tracing::trace_span!(target: "regia_editor", "abilities_panel_snapshot").entered();
    let mut query = world.query_filtered::<Entity, Or<(With<Attributes>, With<AbilityBook>)>>();
    let mut ids: Vec<Entity> = query.iter(world).collect();
    ids.sort_by_key(|e| e.to_bits());
    ids.truncate(MAX_LISTED);
    let world: &World = world;
    let entities: Vec<ListedEntity> = ids
        .iter()
        .map(|&e| ListedEntity {
            entity: e,
            name: world.get::<Name>(e).map(|n| n.as_str().to_owned()).or_else(|| {
                world
                    .get::<regia_ecs::ecs::name::Name>(e)
                    .map(|n| n.as_str().to_owned())
            }),
            position: position(world, e),
            state: ring_state(world, e),
            effects: world.get::<AppliedEffects>(e).map_or(0, |l| l.list.len()),
        })
        .collect();
    let entity = wanted
        .filter(|e| world.get_entity(*e).is_ok())
        .or_else(|| entities.first().map(|l| l.entity));
    let mut abilities: Vec<String> = world
        .get_resource::<AbilityAssets>()
        .map(|a| a.ability_paths().map(str::to_owned).collect())
        .unwrap_or_default();
    abilities.sort();
    let effect_assets: Vec<String> = world
        .get_resource::<AbilityAssets>()
        .map(|a| a.effect_paths().map(str::to_owned).collect())
        .unwrap_or_default();
    let mut snap = Snapshot {
        source,
        available: world.contains_resource::<AbilityAssets>(),
        entities,
        entity,
        abilities,
        effect_assets,
        ..Snapshot::default()
    };
    let Some(e) = entity else {
        return snap;
    };
    if let Some(attrs) = world.get::<Attributes>(e) {
        snap.attributes = attrs
            .list()
            .iter()
            .map(|a| AttributeRow {
                name: a.name.clone(),
                base: a.base,
                current: a.current,
                min: a.min,
                max: a.max,
                derived: a.derived.is_some(),
                breakdown: ab::explain_attribute(world, e, a.id)
                    .map(|b| breakdown_lines(&b))
                    .unwrap_or_default(),
            })
            .collect();
    }
    if let Some(list) = world.get::<AppliedEffects>(e) {
        snap.effects = list
            .list
            .iter()
            .map(|x| EffectRow {
                id: x.id.0,
                asset: x.asset.clone(),
                level: x.level,
                stacks: u32::from(x.stacks),
                remaining_ticks: (x.duration_ticks > 0).then_some(x.remaining_ticks),
                suspended: x.suspended,
            })
            .collect();
    }
    if let Some(book) = world.get::<AbilityBook>(e) {
        snap.grants = book
            .grants
            .iter()
            .map(|g| GrantRow {
                id: g.id.0,
                asset: g.asset.clone(),
                level: g.level,
                state: ab::ability_state(world, e, g.id).map_or("revoked", AbilityState::code),
            })
            .collect();
    }
    snap
}

fn entity_of(world: &World, params: &Value, fallback: Option<Entity>) -> Result<Entity, PanelError> {
    let e = match params.get("entity") {
        None | Some(Value::Null) => fallback,
        Some(v) => Some(
            v.as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| err("bad_value", "`entity` must be an entity id"))?,
        ),
    }
    .ok_or_else(|| err("no_entity", "no entity is inspected"))?;
    if world.get_entity(e).is_err() {
        return Err(PanelError::new(
            "entity_not_found",
            format!("entity {} does not exist", e.to_bits()),
        ));
    }
    Ok(e)
}

fn grant_of(params: &Value) -> Result<GrantId, PanelError> {
    params
        .get("grant")
        .and_then(Value::as_u64)
        .and_then(|g| u32::try_from(g).ok())
        .map(GrantId)
        .ok_or_else(|| err("bad_value", "`grant` must be a grant id"))
}

/// Runs one operation (`params.op`: `set_base`, `remove_effect`, `grant`, `revoke`, `activate`,
/// `add_attributes`, `apply_effect`) on `world` for the inspected entity (or `params.entity`).
/// `activate` runs only in the play world. `add_attributes` and `apply_effect` leave the world as
/// the agent's `abilities.add_attributes` and `abilities.apply_effect` do, and in the edit world
/// each is one undo step.
///
/// # Errors
/// `abilities_panel_bad_op`, `abilities_panel_bad_value`, `abilities_panel_no_entity`,
/// `abilities_panel_unavailable` (no ability plugin), `abilities_panel_not_playing`, or the
/// `regia_framework::abilities` reason (`unknown_attribute`, `attribute_derived`, `effect_not_found`,
/// `asset_not_found`, ...).
pub fn run_op(
    world: &mut World,
    source: Source,
    inspected: Option<Entity>,
    params: &Value,
) -> Result<Value, PanelError> {
    let op = params.get("op").and_then(Value::as_str).unwrap_or_default();
    if !matches!(
        op,
        "set_base" | "remove_effect" | "grant" | "revoke" | "activate" | "add_attributes" | "apply_effect"
    ) {
        return Err(err("bad_op", format!("unknown operation `{op}`")));
    }
    let e = entity_of(world, params, inspected)?;
    if matches!(op, "add_attributes" | "apply_effect") {
        let before = undo::AbilityComponents::of(world, e);
        let (label, answer) = if op == "add_attributes" {
            add_attributes(world, e, params)?
        } else {
            apply_effect(world, e, params)?
        };
        if source == Source::Edit {
            undo::record(world, e, label, before);
        }
        return Ok(answer);
    }
    match op {
        "set_base" => {
            let name = str_param(params, "attribute").map_err(|x| err("bad_value", x.message))?;
            let base = opt_f32(params, "base")
                .map_err(|x| PanelError::new("attribute_not_finite", x.message))?
                .ok_or_else(|| PanelError::new("attribute_not_finite", "`base` must be a finite number"))?;
            let id = AttrId::of(name);
            match world.get::<Attributes>(e) {
                None => return Err(PanelError::new("no_attributes", "the entity has no attributes")),
                Some(a) if a.attribute(id).is_none() => {
                    return Err(PanelError::new(
                        "unknown_attribute",
                        format!("no attribute `{name}`"),
                    ));
                }
                Some(_) => {}
            }
            let (old, new) = ab::set_base(world, e, id, base).map_err(|x| from_ability_error(&x))?;
            Ok(json!({ "old": old, "new": new }))
        }
        "remove_effect" => {
            let id = params
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| err("bad_value", "`id` must be an effect id"))?;
            if !world.contains_resource::<AbilityAssets>() {
                return Err(err("unavailable", "the world runs no ability plugin"));
            }
            ab::remove_effect(world, e, EffectId(id)).map_err(|x| from_ability_error(&x))?;
            Ok(json!({ "removed": 1 }))
        }
        "grant" => {
            let path = str_param(params, "ability").map_err(|x| err("bad_value", x.message))?;
            if !world.contains_resource::<AbilityAssets>() {
                return Err(err("unavailable", "the world runs no ability plugin"));
            }
            let level = match params.get("level") {
                None | Some(Value::Null) => 1,
                Some(v) => v
                    .as_u64()
                    .and_then(|l| u32::try_from(l).ok())
                    .ok_or_else(|| err("bad_value", "`level` must be an unsigned integer"))?,
            };
            let opts = GrantOptions {
                level,
                ..GrantOptions::default()
            };
            match ab::grant_ability(world, e, path, opts) {
                Ok(g) => Ok(json!({ "grant": g.0 })),
                Err(ab::AbilityError::StartRefused(cause)) => {
                    Ok(json!({ "grant": null, "cause": cause.code() }))
                }
                Err(x) => Err(from_ability_error(&x)),
            }
        }
        "revoke" => {
            let g = grant_of(params)?;
            if !world.contains_resource::<AbilityAssets>() {
                return Err(err("unavailable", "the world runs no ability plugin"));
            }
            ab::revoke_ability(world, e, g, false).map_err(|x| from_ability_error(&x))?;
            Ok(json!({ "revoked": true }))
        }
        _ => {
            if source != Source::Play {
                return Err(err(
                    "not_playing",
                    "abilities start only while a play session runs",
                ));
            }
            let g = grant_of(params)?;
            if !world.contains_resource::<AbilityAssets>() {
                return Err(err("unavailable", "the world runs no ability plugin"));
            }
            let ctx = StartContext {
                picks: None,
                signal: None,
                intent: ab::IntentSeq(0),
            };
            Ok(match ab::activate(world, e, g, ctx) {
                Ok(()) => json!({ "started": true }),
                Err(r) => json!({ "started": false, "cause": r.cause.code() }),
            })
        }
    }
}

/// `add_attributes`: gives `e` the attributes of the `.attrs.ron` at `params.asset` (its asset key,
/// a path under `assets/`, optionally spelled with the `assets/` prefix), read from the project's
/// `assets/` when the world has not loaded it, under the key as given. For a loaded set it leaves the
/// world as `abilities.add_attributes` does; for an unloaded one too with the prefixed spelling, which
/// is the path the agent method reads from the project root (it refuses the bare key). Returns the
/// undo label and the answer.
fn add_attributes(world: &mut World, e: Entity, params: &Value) -> Result<(String, Value), PanelError> {
    let path = str_param(params, "asset").map_err(|x| err("bad_value", x.message))?;
    if !world.contains_resource::<AbilityAssets>() {
        return Err(err("unavailable", "the world runs no ability plugin"));
    }
    if world.resource::<AbilityAssets>().attributes(path).is_none() {
        let dir = crate::mesh_source::project_root(world).map(|p| p.join("assets"));
        // `assets/attrs/x.attrs.ron` names the same file as `attrs/x.attrs.ron` (rev-claude-64 (5)
        // finding 1); the key stays as given.
        let under_assets = path.strip_prefix("assets/").unwrap_or(path);
        let text = dir
            .and_then(|dir| {
                crate::data_panels::confined(&dir, under_assets, &[".attrs.ron"], "abilities_panel").ok()
            })
            .and_then(|file| std::fs::read_to_string(file).ok())
            .ok_or_else(|| {
                PanelError::new(
                    "asset_not_found",
                    format!("`{path}` is not loaded and cannot be read from the project"),
                )
            })?;
        let tags = world
            .get_resource::<regia_framework::tags::TagRegistry>()
            .cloned();
        let diagnostics = world
            .resource_mut::<AbilityAssets>()
            .load_text(path, &text, tags.as_ref(), None);
        if let Some(d) = diagnostics.iter().find(|d| d.severity == ab::Severity::Error) {
            let reason = if d.code == "format_too_new" {
                "format_too_new"
            } else {
                "asset_not_found"
            };
            return Err(PanelError::new(
                reason,
                format!("`{path}`: {} ({})", d.message, d.code),
            ));
        }
    }
    let n = ab::add_attributes(world, e, path).map_err(|x| from_ability_error(&x))?;
    Ok((format!("Add attributes {path}"), json!({ "attributes": n })))
}

/// `apply_effect`: applies the loaded effect `params.effect` (at `params.level`, default the
/// effect's own) to `e`, as `abilities.apply_effect` does. Returns the undo label and the answer.
fn apply_effect(world: &mut World, e: Entity, params: &Value) -> Result<(String, Value), PanelError> {
    // The level is checked before the asset, like `abilities.apply_effect` (ADR-1809).
    let level = match params.get("level") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|l| u32::try_from(l).ok())
                .ok_or_else(|| err("bad_value", "`level` must be an unsigned integer"))?,
        ),
    };
    let path = str_param(params, "effect").map_err(|x| err("bad_value", x.message))?;
    if !world.contains_resource::<AbilityAssets>() {
        return Err(err("unavailable", "the world runs no ability plugin"));
    }
    if world.resource::<AbilityAssets>().effect(path).is_none() {
        return Err(PanelError::new(
            "effect_not_found",
            format!("effect `{path}` is not loaded"),
        ));
    }
    let mut request = ab::ApplyEffect::new(path, e);
    if let Some(level) = level {
        request.level = level;
    }
    let out = ab::apply_effect(world, request).map_err(|x| from_ability_error(&x))?;
    Ok((
        format!("Apply effect {path}"),
        json!({ "outcome": out.outcome.code(), "id": out.id.map(|i| i.0), "stacks": out.stacks }),
    ))
}

impl AbilitiesPanel {
    /// Opens `path` (under `assets/`) in the asset editor and validates it.
    ///
    /// # Errors
    /// `abilities_panel_no_project`, `abilities_panel_bad_path`, `abilities_panel_file_not_found`.
    pub fn open_asset(
        &mut self,
        world: &World,
        project: Option<&std::path::Path>,
        path: &str,
    ) -> Result<(), PanelError> {
        let dir = project
            .map(|p| p.join("assets"))
            .ok_or_else(|| err("no_project", "open a project first"))?;
        let file = crate::data_panels::confined(&dir, path, ASSET_SUFFIXES, "abilities_panel")?;
        let text =
            std::fs::read_to_string(&file).map_err(|e| err("file_not_found", format!("{path}: {e}")))?;
        let diagnostics = validate_asset(world, Some(&dir), path, &text);
        self.field_error = None;
        self.comments_dropped = false;
        self.asset_history.clear();
        self.asset = Some(AssetDoc {
            path: path.to_owned(),
            saved: text.clone(),
            text,
            diagnostics,
        });
        Ok(())
    }

    /// Replaces the open asset's text and validates it again (the editor does this on every change).
    ///
    /// # Errors
    /// `abilities_panel_no_asset`.
    pub fn set_asset_text(
        &mut self,
        world: &World,
        project: Option<&std::path::Path>,
        text: &str,
    ) -> Result<(), PanelError> {
        self.replace_text(world, project, text, Some(false))
    }

    /// Puts `text` in the open asset and validates it again. `record`: `Some(typing)` records the
    /// replaced text as an undo step when the text changes (`typing` joins a run of raw-editor
    /// keystrokes into one step); `None` (undo, redo) records nothing.
    fn replace_text(
        &mut self,
        world: &World,
        project: Option<&std::path::Path>,
        text: &str,
        record: Option<bool>,
    ) -> Result<(), PanelError> {
        let doc = self
            .asset
            .as_mut()
            .ok_or_else(|| err("no_asset", "open an asset first"))?;
        if let Some(typing) = record
            && doc.text != text
        {
            self.asset_history.record(&doc.text, typing);
        }
        doc.text = text.to_owned();
        let dir = project.map(|p| p.join("assets"));
        doc.diagnostics = validate_asset(world, dir.as_deref(), &doc.path, &doc.text);
        Ok(())
    }

    /// How many undo and redo steps the open asset has.
    #[must_use]
    pub fn asset_history_depth(&self) -> (usize, usize) {
        (self.asset_history.undo.len(), self.asset_history.redo.len())
    }

    /// Puts back the open asset's text before its last edit (a typed form commit, an agent `text`, or
    /// a run of raw-editor keystrokes) and validates it again; the undone text can be redone. The file
    /// is not written: Save writes it, and the dirty flag compares with the saved text.
    ///
    /// # Errors
    /// `abilities_panel_no_asset`, `abilities_panel_nothing_to_undo`.
    pub fn undo_asset(&mut self, world: &World, project: Option<&std::path::Path>) -> Result<(), PanelError> {
        let doc = self
            .asset
            .as_ref()
            .ok_or_else(|| err("no_asset", "open an asset first"))?;
        let previous = self
            .asset_history
            .undo
            .pop()
            .ok_or_else(|| err("nothing_to_undo", format!("{}: nothing to undo", doc.path)))?;
        let current = doc.text.clone();
        self.asset_history.redo.push(current);
        self.asset_history.typing = false;
        self.field_error = None;
        self.replace_text(world, project, &previous, None)
    }

    /// Applies again the edit the last [`Self::undo_asset`] took back.
    ///
    /// # Errors
    /// `abilities_panel_no_asset`, `abilities_panel_nothing_to_redo`.
    pub fn redo_asset(&mut self, world: &World, project: Option<&std::path::Path>) -> Result<(), PanelError> {
        let doc = self
            .asset
            .as_ref()
            .ok_or_else(|| err("no_asset", "open an asset first"))?;
        let next = self
            .asset_history
            .redo
            .pop()
            .ok_or_else(|| err("nothing_to_redo", format!("{}: nothing to redo", doc.path)))?;
        let current = doc.text.clone();
        if self.asset_history.undo.len() == ASSET_UNDO_DEPTH {
            self.asset_history.undo.remove(0);
        }
        self.asset_history.undo.push(current);
        self.asset_history.typing = false;
        self.field_error = None;
        self.replace_text(world, project, &next, None)
    }

    /// Sets one field of the open asset through the typed form (ADR-7472): the asset's text becomes
    /// the canonical text with `field` set to `value` (a JSON value in the format's serde form) and
    /// is validated like typed text. The text is unchanged on error.
    ///
    /// # Errors
    /// `abilities_panel_no_asset`, `abilities_panel_parse` (the text does not parse; edit it as
    /// text), `abilities_panel_no_field`, `abilities_panel_bad_value`.
    pub fn set_asset_field(
        &mut self,
        world: &World,
        project: Option<&std::path::Path>,
        field: &str,
        value: &Value,
    ) -> Result<(), PanelError> {
        let doc = self
            .asset
            .as_ref()
            .ok_or_else(|| err("no_asset", "open an asset first"))?;
        let kind = form::FormKind::of(&doc.path)
            .ok_or_else(|| err("bad_path", format!("{} is not an ability asset", doc.path)))?;
        let had_comments = form::has_comments(&doc.text);
        match form::set_field(kind, &doc.text, field, value) {
            Ok(text) => {
                self.field_error = None;
                self.comments_dropped |= had_comments && !form::has_comments(&text);
                self.set_asset_text(world, project, &text)
            }
            Err(e) => {
                self.field_error = Some((field.to_owned(), e.message.clone()));
                Err(e)
            }
        }
    }

    /// Writes the open asset atomically; refused while validation finds an error.
    ///
    /// # Errors
    /// `abilities_panel_no_asset`, `abilities_panel_invalid_asset`, `abilities_panel_no_project`,
    /// `abilities_panel_io_failed`.
    pub fn save_asset(&mut self, project: Option<&std::path::Path>) -> Result<String, PanelError> {
        let doc = self
            .asset
            .as_mut()
            .ok_or_else(|| err("no_asset", "open an asset first"))?;
        if let Some(first) = doc.diagnostics.iter().find(|d| d.severity == ab::Severity::Error) {
            return Err(err("invalid_asset", format!("{}: {}", first.code, first.message)));
        }
        let dir = project
            .map(|p| p.join("assets"))
            .ok_or_else(|| err("no_project", "open a project first"))?;
        let file = crate::data_panels::confined(&dir, &doc.path, ASSET_SUFFIXES, "abilities_panel")?;
        crate::data_panels::write_atomic(&file, &doc.text, "abilities_panel")?;
        doc.saved = doc.text.clone();
        let path = doc.path.clone();
        self.comments_dropped = false;
        // A save ends a run of keystrokes: the next one starts its own step.
        self.asset_history.typing = false;
        Ok(path)
    }

    /// Applies one form control's edit as the [`form::set_field`] of the value it makes (the list with
    /// one more or one fewer entry, the enum in its new variant), so the text is the one an agent's
    /// `field`/`value` edit of the same value writes. A refusal is drawn beside the field.
    fn form_edit(
        &mut self,
        world: &World,
        project: Option<&std::path::Path>,
        edit: &FormEdit,
    ) -> Result<(), PanelError> {
        let (field, value) = {
            let doc = self
                .asset
                .as_ref()
                .ok_or_else(|| err("no_asset", "open an asset first"))?;
            let kind = form::FormKind::of(&doc.path)
                .ok_or_else(|| err("bad_path", format!("{} is not an ability asset", doc.path)))?;
            let made = match edit {
                FormEdit::Set(field, value) => Ok((field.clone(), value.clone())),
                FormEdit::Add(field) => {
                    form::list_with_entry(kind, &doc.text, field).map(|v| (field.clone(), v))
                }
                FormEdit::Remove(field, index) => {
                    form::list_without_entry(kind, &doc.text, field, *index).map(|v| (field.clone(), v))
                }
                FormEdit::Variant(field, variant) => {
                    form::variant_value(kind, &doc.text, field, variant).map(|v| (field.clone(), v))
                }
            };
            match made {
                Ok(made) => made,
                Err(e) => {
                    let field = match edit {
                        FormEdit::Set(f, _)
                        | FormEdit::Add(f)
                        | FormEdit::Remove(f, _)
                        | FormEdit::Variant(f, _) => f,
                    };
                    self.field_error = Some((field.clone(), e.message.clone()));
                    return Err(e);
                }
            }
        };
        self.set_asset_field(world, project, &field, &value)
    }
}

/// Runs the queued operations on `world` and takes the snapshot. `selection` is the entity to inspect
/// when nothing is pinned.
pub fn pump_world(panel: &mut AbilitiesPanel, world: &mut World, source: Source, selection: Option<Entity>) {
    let _span = tracing::trace_span!(target: "regia_editor", "abilities_panel_pump").entered();
    let wanted = panel.pinned.or(selection);
    for params in std::mem::take(&mut panel.pending) {
        let op = params
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let result = run_op(world, source, wanted.or(panel.snapshot.entity), &params);
        panel.last = Some(OpRecord {
            op,
            source,
            answer: result.as_ref().ok().cloned(),
            error: result.err(),
        });
    }
    panel.snapshot = snapshot(world, source, wanted);
}

/// Each editor frame: runs queued operations on the play world while a session runs, else on the edit
/// world, and refreshes the snapshot.
pub fn pump(world: &mut World, session: Option<&mut crate::pie::PieSession>) {
    let Some(mut panel) = world.remove_resource::<AbilitiesPanel>() else {
        return;
    };
    match session {
        Some(session) => {
            if panel.snapshot.source != Source::Play {
                // A new session: edit-world ids mean nothing there.
                panel.pinned = None;
            }
            pump_world(&mut panel, session.app.world_mut(), Source::Play, None);
        }
        None => {
            if panel.snapshot.source != Source::Edit {
                panel.pinned = None;
            }
            let selection = world
                .get_resource::<crate::selection::Selection>()
                .and_then(crate::selection::Selection::primary);
            pump_world(&mut panel, world, Source::Edit, selection);
        }
    }
    world.insert_resource(panel);
}

fn ring_color(state: &str) -> egui::Color32 {
    match state {
        "running" => egui::Color32::from_rgb(235, 205, 60),
        "cooling" => egui::Color32::from_rgb(90, 150, 240),
        _ => egui::Color32::from_rgb(90, 210, 110),
    }
}

/// The `abilities` overlay group's segments (empty while the group is off): a state ring at each listed
/// entity's feet and a tick per live effect above it.
#[must_use]
pub fn abilities_overlay_segments(world: &World) -> Vec<Segment> {
    let Some(panel) = world.get_resource::<AbilitiesPanel>() else {
        return Vec::new();
    };
    if !panel.overlay {
        return Vec::new();
    }
    const SIDES: usize = 16;
    const RADIUS: f32 = 0.6;
    let mut out = Vec::new();
    for listed in &panel.snapshot.entities {
        let Some(at) = listed.position else {
            continue;
        };
        let color = ring_color(listed.state);
        let point = |i: usize| {
            let a = std::f32::consts::TAU * i as f32 / SIDES as f32;
            at + Vec3::new(a.cos() * RADIUS, 0.05, a.sin() * RADIUS)
        };
        for i in 0..SIDES {
            out.push((point(i), point(i + 1), color));
        }
        for k in 0..listed.effects.min(8) {
            let x = (k as f32 - (listed.effects.min(8) as f32 - 1.0) * 0.5) * 0.15;
            let base = at + Vec3::new(x, 2.1, 0.0);
            out.push((
                base,
                base + Vec3::new(0.0, 0.2, 0.0),
                egui::Color32::from_rgb(230, 120, 220),
            ));
        }
    }
    out
}

// ------------------------------------------------------------------------------------------- drawing

/// What the asset editor's widgets asked for this frame.
#[derive(Default)]
struct AssetActions {
    open: Option<String>,
    text: Option<String>,
    /// A typed form edit.
    field: Option<FormEdit>,
    save: bool,
    /// Undo (`Some(true)`) or redo (`Some(false)`) the last asset edit.
    history: Option<bool>,
}

/// Draws the panel.
pub fn abilities_panel(ui: &mut egui::Ui, world: &mut World) {
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<AbilitiesPanel>().unwrap_or_default();
    draw(ui, &mut panel);
    let mut actions = AssetActions::default();
    draw_asset(ui, &mut panel, &mut actions);
    let mut result = Ok(());
    if let Some(path) = actions.open {
        result = panel.open_asset(world, project.as_deref(), &path);
    }
    if let Some(text) = actions.text {
        // Keystrokes in the raw editor: one undo step per run.
        result = result.and_then(|()| panel.replace_text(world, project.as_deref(), &text, Some(true)));
    }
    // One undo or redo per egui frame (a second pass of the frame sees the same click).
    let frame = ui.ctx().cumulative_frame_nr();
    if let Some(undo) = actions.history
        && panel.history_frame != Some(frame)
    {
        panel.history_frame = Some(frame);
        result = result.and_then(|()| {
            if undo {
                panel.undo_asset(world, project.as_deref())
            } else {
                panel.redo_asset(world, project.as_deref())
            }
        });
    }
    // One form edit per egui frame: a second pass of the frame sees the same click (an Add entry
    // must not append twice).
    let frame = ui.ctx().cumulative_frame_nr();
    if let Some(edit) = actions.field
        && panel.form_frame != Some(frame)
    {
        panel.form_frame = Some(frame);
        // A refused edit is drawn beside its field (`field_error`), not as the panel's last error.
        let _ = panel.form_edit(world, project.as_deref(), &edit);
    }
    if actions.save {
        result = result.and_then(|()| panel.save_asset(project.as_deref()).map(|_| ()));
    }
    if let Err(e) = result {
        panel.last = Some(OpRecord {
            op: "asset".to_owned(),
            source: panel.snapshot.source,
            answer: None,
            error: Some(e),
        });
    }
    world.insert_resource(panel);
}

fn diagnostic_color(severity: ab::Severity) -> egui::Color32 {
    if severity == ab::Severity::Error {
        egui::Color32::from_rgb(230, 90, 90)
    } else {
        egui::Color32::from_rgb(230, 170, 80)
    }
}

fn draw_asset(ui: &mut egui::Ui, panel: &mut AbilitiesPanel, actions: &mut AssetActions) {
    ui.separator();
    egui::CollapsingHeader::new("Asset (.attrs / .effect / .ability)")
        .default_open(panel.asset.is_some())
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut panel.asset_input).hint_text("effects/burn.effect.ron"),
                );
                if ui.button("Open").clicked() {
                    actions.open = Some(panel.asset_input.trim().to_owned());
                }
            });
            let Some(doc) = &panel.asset else {
                return;
            };
            ui.horizontal(|ui| {
                ui.monospace(if doc.text == doc.saved {
                    doc.path.clone()
                } else {
                    format!("{} *", doc.path)
                });
                let ok = !ab::has_errors(&doc.diagnostics);
                if ui
                    .add_enabled(ok && doc.text != doc.saved, egui::Button::new("Save"))
                    .clicked()
                {
                    actions.save = true;
                }
                let (undo_depth, redo_depth) =
                    (panel.asset_history.undo.len(), panel.asset_history.redo.len());
                if crate::agent_parity::action_enabled(
                    ui,
                    undo_depth > 0,
                    &crate::agent_parity::ABILITIES_ASSET_UNDO,
                    None,
                )
                .clicked()
                {
                    actions.history = Some(true);
                }
                if crate::agent_parity::action_enabled(
                    ui,
                    redo_depth > 0,
                    &crate::agent_parity::ABILITIES_ASSET_REDO,
                    None,
                )
                .clicked()
                {
                    actions.history = Some(false);
                }
                crate::agent_parity::action_toggle(
                    ui,
                    &crate::agent_parity::ABILITIES_ASSET_ADVANCED,
                    &mut panel.advanced,
                );
            });
            let tree = {
                let _span =
                    tracing::trace_span!(target: "regia_editor", "abilities_asset_form_parse").entered();
                doc.form()
            };
            let has_form = tree.is_some();
            let fields = doc.diagnostic_fields(tree.as_ref());
            if panel
                .form_enums
                .as_ref()
                .is_none_or(|(text, _)| *text != doc.text)
            {
                let enums = form::FormKind::of(&doc.path)
                    .map(|kind| form::enum_paths(kind, &doc.text))
                    .unwrap_or_default();
                panel.form_enums = Some((doc.text.clone(), enums));
            }
            let enums = panel
                .form_enums
                .as_ref()
                .map(|(_, e)| e.clone())
                .unwrap_or_default();
            if has_form && !panel.advanced {
                // Before the first typed edit of a file with comments: the edit drops them.
                if form::has_comments(&doc.text) {
                    ui.colored_label(diagnostic_color(ab::Severity::Warning), form::COMMENTS_WARNING);
                } else if panel.comments_dropped && doc.text != doc.saved {
                    ui.colored_label(
                        diagnostic_color(ab::Severity::Warning),
                        "A typed edit dropped the file's comments: Save writes the canonical text (open the \
                         file again to get them back).",
                    );
                }
            }
            match tree {
                Some(tree) if !panel.advanced => {
                    // The typed form: one row per top-level field, its messages beside it.
                    let mut beside: std::collections::BTreeMap<&str, Vec<(egui::Color32, String)>> =
                        std::collections::BTreeMap::new();
                    for (d, field) in doc.diagnostics.iter().zip(&fields) {
                        if let Some(field) = field {
                            beside
                                .entry(field.as_str())
                                .or_default()
                                .push((diagnostic_color(d.severity), format!("{}: {}", d.code, d.message)));
                        }
                    }
                    if let Some((field, message)) = &panel.field_error {
                        let top = field.split(['.', '[']).next().unwrap_or(field);
                        beside
                            .entry(top)
                            .or_default()
                            .push((diagnostic_color(ab::Severity::Error), message.clone()));
                    }
                    let _span =
                        tracing::trace_span!(target: "regia_editor", "abilities_asset_form").entered();
                    egui::Grid::new("abilities-asset-form")
                        .num_columns(3)
                        .striped(true)
                        .show(ui, |ui| {
                            for (key, value) in tree.as_object().into_iter().flatten() {
                                ui.label(key.as_str());
                                if key == form::READ_ONLY_FIELD {
                                    // The file's format version: shown, never edited here.
                                    ui.weak(value.to_string())
                                        .on_hover_text("the file's format version (not edited in the form)");
                                } else if let Some(edit) = form_widget(ui, key, value, &enums) {
                                    actions.field = Some(edit);
                                }
                                ui.vertical(|ui| {
                                    for (color, text) in beside.get(key.as_str()).into_iter().flatten() {
                                        ui.colored_label(*color, text);
                                    }
                                });
                                ui.end_row();
                            }
                        });
                }
                _ => {
                    if !panel.advanced {
                        ui.weak("The text does not parse: fix it here, then the form returns.");
                    }
                    let mut text = doc.text.clone();
                    if ui
                        .add(
                            egui::TextEdit::multiline(&mut text)
                                .code_editor()
                                .desired_rows(10)
                                .desired_width(f32::INFINITY),
                        )
                        .changed()
                    {
                        actions.text = Some(text);
                    }
                }
            }
            // Messages with no field (and every message in the raw view) under the editor.
            for (d, field) in doc.diagnostics.iter().zip(&fields) {
                if field.is_some() && !panel.advanced && has_form {
                    continue;
                }
                let at = d
                    .span
                    .map(|(l, c)| format!(" (line {l}, column {c})"))
                    .unwrap_or_default();
                ui.colored_label(
                    diagnostic_color(d.severity),
                    format!("{}{at}: {}", d.code, d.message),
                );
            }
            if doc.diagnostics.is_empty() {
                ui.weak("valid");
            }
        });
}

/// A number widget for the float at `path`, starting at `f`; returns the edit when it changed.
fn float_widget(ui: &mut egui::Ui, path: &str, mut f: f64) -> Option<FormEdit> {
    let response = ui.add(egui::DragValue::new(&mut f).speed(0.01).max_decimals(6));
    if crate::a11y_audit::name(response, egui::WidgetType::DragValue, path).changed() {
        serde_json::Number::from_f64(f).map(|n| FormEdit::Set(path.to_owned(), Value::Number(n)))
    } else {
        None
    }
}

/// The variant picker of the enum value at `path` (`variants` are the enum's); returns the switch when
/// another variant is picked.
fn variant_widget(ui: &mut egui::Ui, path: &str, current: &str, variants: &[&str]) -> Option<FormEdit> {
    let mut edit = None;
    egui::ComboBox::from_id_salt(("abilities-form-variant", path))
        .selected_text(current)
        .show_ui(ui, |ui| {
            for &variant in variants {
                if ui.selectable_label(variant == current, variant).clicked() && variant != current {
                    edit = Some(FormEdit::Variant(path.to_owned(), variant.to_owned()));
                }
            }
        });
    edit
}

/// The widget of one form value at `path`; returns the edit when the value changed. Lists and
/// records nest; a list adds and removes entries; an enum value (`enums` lists the form's) picks its
/// variant; a `none` is shown as such (set it in Advanced).
fn form_widget(
    ui: &mut egui::Ui,
    path: &str,
    value: &Value,
    enums: &std::collections::BTreeMap<String, &'static [&'static str]>,
) -> Option<FormEdit> {
    let mut edit = None;
    if let Some(variants) = enums.get(path) {
        let current = match value {
            Value::String(unit) => Some(unit.as_str()),
            Value::Object(fields) if fields.len() == 1 => fields.keys().next().map(String::as_str),
            _ => None,
        };
        if let Some(current) = current {
            edit = variant_widget(ui, path, current, variants);
            if value.is_string() {
                // A unit variant has nothing more to show.
                return edit;
            }
        }
    }
    match value {
        Value::Bool(on) => {
            let mut on = *on;
            if crate::a11y_audit::bare_checkbox(ui, &mut on, path).changed() {
                edit = Some(FormEdit::Set(path.to_owned(), Value::Bool(on)));
            }
        }
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                let mut i = i;
                let response = ui.add(egui::DragValue::new(&mut i));
                if crate::a11y_audit::name(response, egui::WidgetType::DragValue, path).changed() {
                    edit = Some(FormEdit::Set(path.to_owned(), Value::from(i)));
                }
            } else if let Some(f) = n.as_f64() {
                edit = float_widget(ui, path, f);
            }
        }
        // A non-finite number written by hand (`NaN`, `inf`): its spelling, and a number widget
        // that replaces it (rev-claude-59 inspector-forms finding 4).
        Value::Object(_) if form::tree::non_finite(value).is_some() => {
            let spelling = value[form::tree::NON_FINITE].as_str().unwrap_or("NaN");
            ui.horizontal(|ui| {
                ui.colored_label(diagnostic_color(ab::Severity::Warning), spelling)
                    .on_hover_text("not a finite number in the file: drag or type a value to replace it");
                edit = float_widget(ui, path, 0.0);
            });
        }
        Value::String(text) => {
            let mut text = text.clone();
            let response = ui.add(egui::TextEdit::singleline(&mut text).desired_width(220.0));
            if crate::a11y_audit::name(response, egui::WidgetType::TextEdit, path).changed() {
                edit = Some(FormEdit::Set(path.to_owned(), Value::String(text)));
            }
        }
        Value::Null => {
            ui.weak("none");
        }
        Value::Array(items) => {
            ui.vertical(|ui| {
                if items.is_empty() {
                    ui.weak("[ ]");
                } else {
                    egui::CollapsingHeader::new(format!("{} items", items.len()))
                        .id_salt(("abilities-form", path))
                        .show(ui, |ui| {
                            for (i, item) in items.iter().enumerate() {
                                let child = format!("{path}[{i}]");
                                ui.horizontal(|ui| {
                                    ui.weak(format!("[{i}]"));
                                    if let Some(e) = form_widget(ui, &child, item, enums) {
                                        edit = Some(e);
                                    }
                                    if ui
                                        .small_button("Remove entry")
                                        .on_hover_text(format!("Remove {child}"))
                                        .clicked()
                                    {
                                        edit = Some(FormEdit::Remove(path.to_owned(), i));
                                    }
                                });
                            }
                        });
                }
                if ui
                    .small_button("Add entry")
                    .on_hover_text(format!("Append a default-filled entry to {path}"))
                    .clicked()
                {
                    edit = Some(FormEdit::Add(path.to_owned()));
                }
            });
        }
        Value::Object(fields) => {
            ui.vertical(|ui| {
                for (key, item) in fields {
                    let child = format!("{path}.{key}");
                    ui.horizontal(|ui| {
                        ui.weak(key.as_str());
                        if let Some(e) = form_widget(ui, &child, item, enums) {
                            edit = Some(e);
                        }
                    });
                }
            });
        }
    }
    edit
}

fn draw(ui: &mut egui::Ui, panel: &mut AbilitiesPanel) {
    let source = panel.snapshot.source;
    crate::data_panels::header(
        ui,
        "Abilities",
        match source {
            Source::Edit => "edit world",
            Source::Play => "play world (live)",
        },
    );
    ui.horizontal(|ui| {
        ui.checkbox(&mut panel.overlay, "Overlay")
            .on_hover_text("Debug overlay group `abilities`");
        if !panel.snapshot.available {
            ui.weak("no ability plugin in this world");
        }
    });
    let mut ops: Vec<Value> = Vec::new();
    let mut pin: Option<Option<Entity>> = None;
    egui::ComboBox::from_id_salt("abilities-entity")
        .selected_text(
            panel
                .snapshot
                .entity
                .map_or_else(|| "no entity".to_owned(), |e| entity_label(&panel.snapshot, e)),
        )
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(panel.pinned.is_none(), "follow selection")
                .clicked()
            {
                pin = Some(None);
            }
            for listed in &panel.snapshot.entities {
                let label = entity_label(&panel.snapshot, listed.entity);
                if ui
                    .selectable_label(panel.pinned == Some(listed.entity), label)
                    .clicked()
                {
                    pin = Some(Some(listed.entity));
                }
            }
        });
    if let Some(pin) = pin {
        panel.pinned = pin;
    }
    let snap = panel.snapshot.clone();
    if snap.entity.is_none() {
        ui.weak("Select an entity with Attributes or an AbilityBook.");
    }
    if !snap.attributes.is_empty() {
        ui.strong("Attributes");
        egui::Grid::new("abilities-attributes")
            .striped(true)
            .num_columns(4)
            .show(ui, |ui| {
                ui.weak("name");
                ui.weak("base");
                ui.weak("current");
                ui.weak("range");
                ui.end_row();
                for a in &snap.attributes {
                    ui.label(&a.name);
                    let value = panel.base_text.entry(a.name.clone()).or_insert(a.base);
                    let response = ui.add_enabled(!a.derived, egui::DragValue::new(value).speed(0.1));
                    if response.drag_stopped() || response.lost_focus() {
                        if *value != a.base && value.is_finite() {
                            ops.push(json!({ "op": "set_base", "attribute": a.name, "base": *value }));
                        }
                        panel.base_text.remove(&a.name);
                    } else if !response.has_focus() && !response.dragged() {
                        *value = a.base;
                    }
                    ui.label(format!("{}", a.current))
                        .on_hover_text(a.breakdown.join("\n"));
                    ui.weak(format!("[{}, {}]", a.min, a.max));
                    ui.end_row();
                }
            });
    }
    // Spec 53 § 6 parity with `abilities.add_attributes`: one undo step in the edit world.
    ui.horizontal(|ui| {
        let response = ui.add(
            egui::TextEdit::singleline(&mut panel.attrs_input)
                .hint_text("attrs/hero.attrs.ron")
                .desired_width(200.0),
        );
        crate::a11y_audit::name(response, egui::WidgetType::TextEdit, "attribute set");
        if ui
            .add_enabled(
                !panel.attrs_input.trim().is_empty() && snap.entity.is_some(),
                egui::Button::new("Add attributes"),
            )
            .clicked()
        {
            ops.push(json!({ "op": "add_attributes", "asset": panel.attrs_input.trim() }));
        }
    });
    ui.strong("Live effects");
    if snap.effects.is_empty() {
        ui.weak("none");
    }
    for e in &snap.effects {
        ui.horizontal(|ui| {
            ui.monospace(format!("#{} {}", e.id, e.asset));
            ui.weak(format!(
                "lvl {} ×{}{}{}",
                e.level,
                e.stacks,
                e.remaining_ticks
                    .map(|t| format!(" · {t} ticks"))
                    .unwrap_or_default(),
                if e.suspended { " · suspended" } else { "" }
            ));
            if ui.small_button("Remove").clicked() {
                ops.push(json!({ "op": "remove_effect", "id": e.id }));
            }
        });
    }
    // Spec 53 § 6 parity with `abilities.apply_effect`: one undo step in the edit world.
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("abilities-apply-effect")
            .selected_text(if panel.effect_input.is_empty() {
                "effect…"
            } else {
                panel.effect_input.as_str()
            })
            .show_ui(ui, |ui| {
                crate::virtual_list::rows(
                    ui,
                    "abilities-apply-effect-rows",
                    snap.effect_assets.len(),
                    crate::virtual_list::DROP_DOWN_HEIGHT,
                    |ui, i| {
                        let path = &snap.effect_assets[i];
                        ui.selectable_value(&mut panel.effect_input, path.clone(), path);
                    },
                );
            });
        if ui
            .add_enabled(
                !panel.effect_input.is_empty() && snap.entity.is_some(),
                egui::Button::new("Apply effect"),
            )
            .clicked()
        {
            ops.push(json!({ "op": "apply_effect", "effect": panel.effect_input }));
        }
    });
    ui.strong("Abilities");
    for g in &snap.grants {
        ui.horizontal(|ui| {
            ui.monospace(format!("#{} {}", g.id, g.asset));
            ui.colored_label(ring_color(g.state), g.state);
            if ui
                .add_enabled(source == Source::Play, egui::Button::new("Start").small())
                .on_disabled_hover_text("Abilities start while playing")
                .clicked()
            {
                ops.push(json!({ "op": "activate", "grant": g.id }));
            }
            if ui.small_button("Revoke").clicked() {
                ops.push(json!({ "op": "revoke", "grant": g.id }));
            }
        });
    }
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("abilities-grant")
            .selected_text(if panel.grant_input.is_empty() {
                "ability…"
            } else {
                panel.grant_input.as_str()
            })
            .show_ui(ui, |ui| {
                // A screenful at a time (ADR-7306): every ability asset of the project.
                crate::virtual_list::rows(
                    ui,
                    "abilities-grant-rows",
                    snap.abilities.len(),
                    crate::virtual_list::DROP_DOWN_HEIGHT,
                    |ui, i| {
                        let path = &snap.abilities[i];
                        ui.selectable_value(&mut panel.grant_input, path.clone(), path);
                    },
                );
            });
        if ui
            .add_enabled(
                !panel.grant_input.is_empty() && snap.entity.is_some(),
                egui::Button::new("Grant"),
            )
            .clicked()
        {
            ops.push(json!({ "op": "grant", "ability": panel.grant_input }));
        }
    });
    if let Some(last) = &panel.last
        && let Some(e) = &last.error
    {
        e.draw(ui);
    }
    for mut op in ops {
        if let Some(e) = snap.entity {
            op["entity"] = json!(e.to_bits());
        }
        panel.pending.push(op);
    }
}

fn entity_label(snap: &Snapshot, e: Entity) -> String {
    let name = snap
        .entities
        .iter()
        .find(|l| l.entity == e)
        .and_then(|l| l.name.clone());
    match name {
        Some(n) => format!("{n} ({})", e.to_bits()),
        None => format!("entity {}", e.to_bits()),
    }
}

// --------------------------------------------------------------------------------------- agent surface

fn playing(world: &World) -> bool {
    world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|s| s.mode.is_some())
}

fn edit_selection(world: &World) -> Option<Entity> {
    world
        .get_resource::<crate::selection::Selection>()
        .and_then(crate::selection::Selection::primary)
}

/// Refreshes the snapshot from the edit world (no play session) and answers the panel.
fn refreshed(world: &mut World) -> Value {
    let mut panel = world.remove_resource::<AbilitiesPanel>().unwrap_or_default();
    if !playing(world) {
        let selection = edit_selection(world);
        pump_world(&mut panel, world, Source::Edit, selection);
    }
    let out = panel.json();
    world.insert_resource(panel);
    out
}

fn panel_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(refreshed(world))
}

fn panel_select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut panel = world.remove_resource::<AbilitiesPanel>().unwrap_or_default();
    let result = (|| -> Result<(), PanelError> {
        match params.get("entity") {
            None => {}
            Some(Value::Null) => panel.pinned = None,
            Some(v) => {
                panel.pinned = Some(
                    v.as_u64()
                        .and_then(Entity::try_from_bits)
                        .ok_or_else(|| err("bad_value", "`entity` must be an entity id or null"))?,
                );
            }
        }
        match params.get("overlay") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(on)) => panel.overlay = *on,
            Some(_) => return Err(err("bad_value", "`overlay` must be a boolean")),
        }
        Ok(())
    })();
    world.insert_resource(panel);
    result.map_err(|e| e.rpc(HINT))?;
    Ok(refreshed(world))
}

fn panel_call(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut panel = world.remove_resource::<AbilitiesPanel>().unwrap_or_default();
    if playing(world) {
        // The play world belongs to the editor loop: the call runs at the next pump.
        panel.pending.push(params.clone());
        let queued = panel.pending.len();
        world.insert_resource(panel);
        return Ok(json!({ "queued": true, "pending": queued }));
    }
    let selection = edit_selection(world);
    // A fresh snapshot first: with nothing pinned or selected, the first listed entity is the subject.
    pump_world(&mut panel, world, Source::Edit, selection);
    let wanted = panel.pinned.or(selection).or(panel.snapshot.entity);
    let op = params
        .get("op")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let result = run_op(world, Source::Edit, wanted, params);
    panel.last = Some(OpRecord {
        op,
        source: Source::Edit,
        answer: result.as_ref().ok().cloned(),
        error: result.as_ref().err().cloned(),
    });
    pump_world(&mut panel, world, Source::Edit, selection);
    world.insert_resource(panel);
    let answer = result.map_err(|e| e.rpc(HINT))?;
    Ok(json!({ "queued": false, "answer": answer, "panel": refreshed(world) }))
}

fn panel_asset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<AbilitiesPanel>().unwrap_or_default();
    // Whether this call's typed edit rewrote a text that had comments (the warning, not an error).
    let mut comments_dropped = false;
    let result = (|| -> Result<Option<String>, PanelError> {
        if let Some(path) = params.get("path").and_then(Value::as_str) {
            panel.open_asset(world, project.as_deref(), path)?;
        }
        match params.get("text") {
            None | Some(Value::Null) => {}
            Some(Value::String(text)) => panel.set_asset_text(world, project.as_deref(), text)?,
            Some(_) => return Err(err("bad_value", "`text` must be a string")),
        }
        match params.get("field") {
            None | Some(Value::Null) => {}
            Some(Value::String(field)) => {
                let value = params
                    .get("value")
                    .ok_or_else(|| err("bad_value", "`value` is required with `field`"))?;
                let had = panel.asset.as_ref().is_some_and(|d| form::has_comments(&d.text));
                panel.set_asset_field(world, project.as_deref(), field, value)?;
                comments_dropped = had;
            }
            Some(_) => return Err(err("bad_value", "`field` must be a string")),
        }
        let flag = |key: &str| match params.get(key) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(err("bad_value", format!("`{key}` must be a boolean"))),
        };
        if flag("undo")? {
            panel.undo_asset(world, project.as_deref())?;
        }
        if flag("redo")? {
            panel.redo_asset(world, project.as_deref())?;
        }
        if params.get("save").and_then(Value::as_bool).unwrap_or(false) {
            return panel.save_asset(project.as_deref()).map(Some);
        }
        if panel.asset.is_none() {
            return Err(err("no_asset", "give `path` to open an asset"));
        }
        Ok(None)
    })();
    let (undo_depth, redo_depth) = panel.asset_history_depth();
    let out = json!({
        "asset": panel.asset.as_ref().map(AssetDoc::json),
        "written": result.as_ref().ok().cloned().flatten(),
        "comments_dropped": comments_dropped,
        "undo_depth": undo_depth,
        "redo_depth": redo_depth,
    });
    world.insert_resource(panel);
    result.map_err(|e| e.rpc(HINT))?;
    Ok(out)
}

const fn param(
    name: &'static str,
    kind: &'static str,
    required: bool,
    description: &'static str,
) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

const STATE: &str = "{ source, available, pinned, overlay, pending, entity, entities: [{ entity, name, state, effects, \
    position }], attributes: [{ name, base, current, min, max, derived, breakdown }], effects: [{ id, asset, level, stacks, \
    remaining_ticks, suspended }], grants: [{ id, asset, level, state }], abilities, effect_assets, last: { op, source, ok, answer, \
    error } }";

const SELECT_PARAMS: &[ParamSchema] = &[
    param(
        "entity",
        "number",
        false,
        "Pin this entity (in the shown world); null follows the selection.",
    ),
    param("overlay", "boolean", false, "Draw the `abilities` overlay group."),
];
const CALL_PARAMS: &[ParamSchema] = &[
    param(
        "op",
        "string",
        true,
        "set_base | remove_effect | grant | revoke | activate (play only) | add_attributes | apply_effect",
    ),
    param(
        "entity",
        "number",
        false,
        "The entity (default: the inspected one).",
    ),
    param("attribute", "string", false, "set_base: the attribute."),
    param("base", "number", false, "set_base: the new base value."),
    param("id", "number", false, "remove_effect: the live effect id."),
    param("ability", "string", false, "grant: the loaded ability asset."),
    param(
        "level",
        "number",
        false,
        "grant, apply_effect: the level (default 1).",
    ),
    param("grant", "number", false, "revoke, activate: the grant id."),
    param(
        "asset",
        "string",
        false,
        "add_attributes: the .attrs.ron (its asset key, a path under assets/; an assets/ prefix is accepted).",
    ),
    param(
        "effect",
        "string",
        false,
        "apply_effect: the loaded effect asset.",
    ),
];

const ASSET_PARAMS: &[ParamSchema] = &[
    param(
        "path",
        "string",
        false,
        "Open this .attrs.ron, .effect.ron or .ability.ron (under assets/).",
    ),
    param(
        "text",
        "string",
        false,
        "Replace the open asset's text (validated at once).",
    ),
    param(
        "save",
        "boolean",
        false,
        "Write the open asset (refused while it has an error).",
    ),
    param(
        "field",
        "string",
        false,
        "Typed form: set this field (`chance`, `lifetime.Timed.Const`, `live_mods[0].op`) of the open asset to \
         `value`; the text becomes the canonical text, validated at once (a list or an enum value is set whole: \
         that adds or removes entries and switches variants). The file's comments are not kept: \
         `comments_warning` says so before, `comments_dropped` after.",
    ),
    param(
        "value",
        "any",
        false,
        "Typed form: the field's new value in the format's serde form (number, string, list, record).",
    ),
    param(
        "undo",
        "boolean",
        false,
        "Put back the open asset's text before its last edit (a `field` or `text` edit; not written until          `save`). Runs after `text` and `field`, before `save`.",
    ),
    param(
        "redo",
        "boolean",
        false,
        "Apply again the edit the last `undo` took back (runs after `undo`).",
    ),
];

/// The Abilities inspector's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.abilities_panel_get",
                tool: "abilities_panel_get",
                description: "The Abilities inspector (spec 53 § 6): the inspected entity's attributes with their \
                    breakdown, live effects and grants, from the play world while playing.",
                params: &[],
                result: STATE,
                mutating: false,
                permission: Permission::Read,
            },
            panel_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.abilities_panel_select",
                tool: "abilities_panel_select",
                description: "Pin the inspected entity or follow the selection; switch the `abilities` overlay \
                    group.",
                params: SELECT_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_select as Handler,
        ),
        (
            MethodSchema {
                name: "editor.abilities_panel_call",
                tool: "abilities_panel_call",
                description: "Do what an inspector button does: set a base value, remove an effect, grant, revoke, \
                    start an ability (while playing), add an attribute set or apply an effect (one undo step each \
                    in the edit world). Queued for the next frame while playing.",
                params: CALL_PARAMS,
                result: "{ queued, pending?, answer?, panel? }",
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_call as Handler,
        ),
        (
            MethodSchema {
                name: "editor.abilities_panel_asset",
                tool: "abilities_panel_asset",
                description: "The inspector's asset editor: open an .attrs.ron, .effect.ron or .ability.ron, replace \
                    its text or set one typed field, with live validation (abilities.validate's checks; each \
                    diagnostic names the form field it is shown beside), undo and redo those edits, save it.",
                params: ASSET_PARAMS,
                result: "{ asset: { path, dirty, ok, text, comments_warning, form, diagnostics: [{ code, severity, \
                    message, span, field }] }, written, comments_dropped, undo_depth, redo_depth }",
                mutating: true,
                permission: Permission::Assets,
            },
            panel_asset as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod form_tests;
