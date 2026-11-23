// SPDX-License-Identifier: MPL-2.0
//! Data layers: named sets of entities that gameplay or the editor switch together
//! (`docs/specs/37-per-entity-levels-layers-instances.md` § 4; format `layers` v1,
//! `docs/formats/layers-ron.md`).
//!
//! - A **definition** asset (`*.layers.ron`, [`LayerDefs`]) names layers: kind, debug color,
//!   parent. It lives in the [`LayerDefsLibrary`] once loaded.
//! - A level **binds** definitions through its [`LevelLayers`] resource (initial states and
//!   per-level settings).
//! - An entity's **membership** is its [`LayerMembership`] component.
//!
//! [`LayerTable`] holds each layer's *requested* state; the *effective* state is
//! `min(requested, effective(parent))`. An entity is present at the **lowest** effective state of
//! its layers (spawned only when all are at least [`LayerState::Parked`], simulated only when all
//! are [`LayerState::Live`]). [`apply_layer_switches`] applies the states at one fixed point of the
//! frame (the [`LayerSwitch`] set in `PostUpdate`, before transform propagation): `Absent`
//! members are despawned (their records stay in the level's template in the [`LevelLibrary`]),
//! `Parked` members carry [`LayerParked`] and are hidden, `Live` members are normal. Spawning
//! goes through the same guid-ordered path as `open_level`.
//!
//! Descendants that carry no [`LayerMembership`] of their own (a model's part entities, things a
//! script attached) follow their member: they carry [`LayerParked`] with `inherited` set, so the
//! physics, script, AI, picking and render queries skip the whole subtree. A descendant with a
//! membership is governed by it (a mismatch is the `layer_parent_mismatch` finding).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

use regia_core::id::Guid;
use regia_ecs::ecs::system::SystemState;
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::components::{EntityGuid, Visibility};
use crate::format::{SceneEntity, SceneFile, SceneFormatError};
use crate::level::{LevelId, LevelLibrary, LevelManager, LevelMember, LevelState};

/// Version of the `.layers.ron` format.
pub const LAYERS_FORMAT_VERSION: u32 = 1;

/// The serde field-list fingerprint of `.layers.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn layers_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<LayerDefs>(),
        regia_core::schema::serde_field_names::<LayerDef>(),
    ])
}
/// Type path of [`LevelLayers`] in a scene file's `resources`.
pub const LEVEL_LAYERS_PATH: &str = "regia_scene::layers::LevelLayers";
/// Type path of [`LayerMembership`] in a scene entity's `components`.
pub const LAYER_MEMBERSHIP_PATH: &str = "regia_scene::layers::LayerMembership";

/// Where a layer exists.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LayerKind {
    /// Only in the editor; dropped from cooked output with its members (unless a member also
    /// belongs to a runtime layer). See [`strip_editor_only`].
    EditorOnly,
    /// Cooked and switchable by gameplay.
    #[default]
    Runtime,
}

/// The run-time state of a layer. Ordered: `Absent < Parked < Live`.
#[derive(
    Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub enum LayerState {
    /// Members are not spawned (their records stay in the level's template).
    Absent,
    /// Members are spawned, carry [`LayerParked`], are hidden, not simulated and not pickable.
    Parked,
    /// Members are spawned, visible and simulated.
    #[default]
    Live,
}

impl LayerState {
    /// `"absent"`, `"parked"` or `"live"` (the agent protocol's spelling).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Parked => "parked",
            Self::Live => "live",
        }
    }

    /// Parses the agent spelling; also accepts `unloaded`/`loaded`/`activated` (the spec 37 § 8
    /// table's wording).
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "absent" | "unloaded" => Some(Self::Absent),
            "parked" | "loaded" => Some(Self::Parked),
            "live" | "activated" => Some(Self::Live),
            _ => None,
        }
    }
}

/// Which peers resolve a layer. Meaningful once networking exists; see [`Side`] for who may
/// write.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LayerPeers {
    /// Server and clients (written by the server).
    #[default]
    Everyone,
    /// Clients only (each client writes its own).
    ClientsOnly,
    /// Server only.
    ServerOnly,
}

/// Who is writing a layer state. [`Side::Local`] (single player, listen server, tests, the
/// editor) owns every layer; [`Side::Server`] owns `Everyone` and `ServerOnly` layers;
/// [`Side::Client`] owns `ClientsOnly` layers. A write by a side that does not own the layer is a
/// no-op returning `false` (reason `side_not_owner`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Side {
    /// Owns every layer.
    #[default]
    Local,
    /// A dedicated or listen server writing as the authority.
    Server,
    /// A client.
    Client,
}

impl Side {
    /// Whether this side may write a layer with `peers`.
    pub fn owns(self, peers: LayerPeers) -> bool {
        match self {
            Self::Local => true,
            Self::Server => matches!(peers, LayerPeers::Everyone | LayerPeers::ServerOnly),
            Self::Client => peers == LayerPeers::ClientsOnly,
        }
    }
}

/// One layer of a definition asset.
#[derive(Reflect, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[reflect(Default)]
pub struct LayerDef {
    /// Stable key: ASCII `[a-z0-9_]{1,48}`, unique per definition asset.
    pub name: String,
    /// Editor-only or runtime.
    #[serde(default)]
    pub kind: LayerKind,
    /// Debug color (linear RGB) for editor overlays.
    #[serde(default)]
    pub debug_color: (f32, f32, f32),
    /// Parent layer name (a child is never more active than its parent).
    #[serde(default)]
    pub parent: Option<String>,
}

/// A layer definition asset (`*.layers.ron`, format `layers` v1).
#[derive(Reflect, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[reflect(Default)]
pub struct LayerDefs {
    /// [`LAYERS_FORMAT_VERSION`].
    pub format_version: u32,
    /// The layers.
    #[serde(default)]
    pub layers: Vec<LayerDef>,
}

impl LayerDefs {
    /// Parses `.layers.ron` text; a newer `format_version` is refused.
    pub fn from_ron_str(text: &str) -> Result<Self, SceneFormatError> {
        let version = regia_core::version::peek_ron_version(text)
            .map_err(|e| SceneFormatError::Invalid(format!("cannot read format_version: {e}")))?;
        if version > LAYERS_FORMAT_VERSION {
            return Err(SceneFormatError::UnsupportedVersion {
                found: version,
                supported: LAYERS_FORMAT_VERSION,
            });
        }
        Ok(ron::from_str(text)?)
    }

    /// Canonical `.layers.ron` text.
    pub fn to_ron_string(&self) -> Result<String, SceneFormatError> {
        let mut text = ron::ser::to_string_pretty(self, regia_ecs::serialize::canonical_pretty_config())?;
        text.push('\n');
        if let Some(rest) = text.strip_prefix('(') {
            text = format!("LayerDefs({rest}");
        }
        Ok(text)
    }

    /// Looks a layer up by name.
    pub fn get(&self, name: &str) -> Option<&LayerDef> {
        self.layers.iter().find(|l| l.name == name)
    }
}

/// A level's use of one layer: initial state and per-level settings.
#[derive(Reflect, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[reflect(Default)]
#[serde(default)]
pub struct LayerEntry {
    /// Layer name (resolved against the bound definitions in order).
    pub layer: String,
    /// State at level load.
    pub initial: LayerState,
    /// Shown in the editor viewport.
    pub editor_visible: bool,
    /// Loaded in the editor while working.
    pub editor_resident: bool,
    /// Which peers resolve the layer.
    pub peers: LayerPeers,
}

impl Default for LayerEntry {
    fn default() -> Self {
        Self {
            layer: String::new(),
            initial: LayerState::Live,
            editor_visible: true,
            editor_resident: true,
            peers: LayerPeers::Everyone,
        }
    }
}

/// The layer binding of a level (a resource stored in the level's `resources`).
#[derive(Resource, Reflect, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[reflect(Resource, Default)]
#[serde(default)]
pub struct LevelLayers {
    /// Bound definition assets (`asset:<guid>`), in resolution order.
    pub defs: Vec<String>,
    /// Per-layer settings; layers without an entry start `Live` with peers `Everyone`.
    pub entries: Vec<LayerEntry>,
    /// Definitions carried by the level itself: a packaged level ships with its bound
    /// definitions folded in, because a package has no `.meta` ids to look them up by. They are
    /// searched before [`Self::defs`]. Empty in authored levels.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub embedded: Vec<LayerDefs>,
}

impl LevelLayers {
    /// Reads the binding from a scene file's resources (`None` when the level binds no layers).
    pub fn from_scene(scene: &SceneFile) -> Option<Result<Self, SceneFormatError>> {
        scene
            .resources
            .get(LEVEL_LAYERS_PATH)
            .map(|raw| ron::from_str::<Self>(raw.get_ron()).map_err(SceneFormatError::from))
    }

    /// The asset ids of the bound definitions (unparsable entries are skipped; the layer table's
    /// rebuild reports them as warnings).
    pub fn def_ids(&self) -> Vec<Guid> {
        self.defs.iter().filter_map(|text| parse_def_ref(text)).collect()
    }
}

/// A `defs` entry (`asset:<guid>` or a bare guid) as an asset id.
fn parse_def_ref(text: &str) -> Option<Guid> {
    text.strip_prefix("asset:").unwrap_or(text).parse().ok()
}

impl LevelLayers {
    /// The `defs` entries that are not asset ids, in file order.
    fn unparsable_defs(&self) -> impl Iterator<Item = &str> {
        self.defs
            .iter()
            .map(String::as_str)
            .filter(|text| parse_def_ref(text).is_none())
    }
}

/// The layers an entity belongs to (stored with the entity, so a change dirties one entity
/// file). Unknown names are kept, reported by validation, and do not govern the entity.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[reflect(Component, Default)]
pub struct LayerMembership {
    /// Layer names.
    pub layers: Vec<String>,
}

impl LayerMembership {
    /// Reads the membership from a scene record (`None` when it has none or it does not parse).
    pub fn from_record(record: &SceneEntity) -> Option<Self> {
        note_from_record();
        record
            .components
            .get(LAYER_MEMBERSHIP_PATH)
            .and_then(|raw| ron::from_str(raw.get_ron()).ok())
    }
}

/// `from_record` calls since [`reset_layer_scan_stats`].
static LAYER_FROM_RECORD: AtomicU64 = AtomicU64::new(0);
/// Scene records walked by a dirty layer apply since [`reset_layer_scan_stats`].
static LAYER_RECORD_SCANS: AtomicU64 = AtomicU64::new(0);

fn note_from_record() {
    LAYER_FROM_RECORD.fetch_add(1, Ordering::Relaxed);
}

fn note_record_scan() {
    LAYER_RECORD_SCANS.fetch_add(1, Ordering::Relaxed);
}

/// Zero the layer-switch scan counters.
pub fn reset_layer_scan_stats() {
    LAYER_FROM_RECORD.store(0, Ordering::Relaxed);
    LAYER_RECORD_SCANS.store(0, Ordering::Relaxed);
}

/// `(from_record calls, scene records walked)` since the last [`reset_layer_scan_stats`].
#[must_use]
pub fn layer_scan_stats() -> (u64, u64) {
    (
        LAYER_FROM_RECORD.load(Ordering::Relaxed),
        LAYER_RECORD_SCANS.load(Ordering::Relaxed),
    )
}

/// Marker on spawned members of a `Parked` layer and on their descendants without a membership:
/// hidden, not simulated, not pickable. Holds the visibility to restore when the layer goes
/// `Live`. Systems that must not step parked entities filter with `Without<LayerParked>`
/// (spec 37 § 13; `book/agents/levels-layers-instances.md` lists them).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerParked {
    /// The entity's [`Visibility`] before it was parked.
    pub restore: Visibility,
    /// Set on a descendant that follows its member: its own `Visibility` was left alone (the
    /// member's `Hidden` reaches it through the hierarchy) and goes with the member.
    pub inherited: bool,
}

/// An effective layer state changed (written once per change, in layer-name order within a
/// frame).
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct LayerChanged {
    /// Layer name.
    pub layer: String,
    /// Previous effective state.
    pub from: LayerState,
    /// New effective state.
    pub to: LayerState,
}

/// System set that applies layer switches (`PostUpdate`, before transform propagation).
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LayerSwitch;

/// Loaded layer definition assets, keyed by asset id (the seam the asset server fills, like
/// [`LevelLibrary`]).
#[derive(Resource, Clone, Debug, Default)]
pub struct LayerDefsLibrary {
    defs: HashMap<Guid, LayerDefs>,
}

impl LayerDefsLibrary {
    /// Registers a definition asset.
    pub fn insert(&mut self, id: Guid, defs: LayerDefs) {
        self.defs.insert(id, defs);
    }

    /// Looks one up.
    pub fn get(&self, id: Guid) -> Option<&LayerDefs> {
        self.defs.get(&id)
    }
}

/// Errors of [`LayerTable::request`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayerError {
    /// No bound layer has this name.
    #[error("layer_unknown: {0}")]
    Unknown(String),
}

impl LayerError {
    /// The agent `data.reason` string.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Unknown(_) => "layer_unknown",
        }
    }
}

#[derive(Clone, Debug)]
struct LayerRecord {
    kind: LayerKind,
    parent: Option<String>,
    requested: LayerState,
    peers: LayerPeers,
    members: usize,
}

/// A read-only view of one layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerInfo<'a> {
    /// Name.
    pub name: &'a str,
    /// Kind.
    pub kind: LayerKind,
    /// Parent layer.
    pub parent: Option<&'a str>,
    /// Last requested state.
    pub requested: LayerState,
    /// `min(requested, effective(parent))`.
    pub effective: LayerState,
    /// Member entities (spawned or absent) counted at the last apply.
    pub member_count: usize,
    /// Which peers resolve it.
    pub peers: LayerPeers,
}

/// The layers of the active levels: requested and effective states. Rebuilt when the set of
/// active levels changes; requests mark it dirty so [`apply_layer_switches`] applies them.
#[derive(Resource, Clone, Debug, Default)]
pub struct LayerTable {
    layers: BTreeMap<String, LayerRecord>,
    /// Active levels the table was built for, with their load generation.
    built_for: Vec<(LevelId, u64)>,
    /// Effective states at the last apply (for [`LayerChanged`]).
    last_effective: BTreeMap<String, LayerState>,
    /// Member records currently despawned because a layer is `Absent`.
    absent: BTreeMap<LevelId, BTreeSet<Guid>>,
    dirty: bool,
    /// Problems found while building (layer_ambiguous, unknown entries, missing definitions).
    warnings: Vec<String>,
}

/// Parent chains longer than this are treated as cycles (definitions are validated anyway).
const MAX_LAYER_DEPTH: usize = 64;

impl LayerTable {
    /// Adds (or replaces) a layer directly: tests and tools that build a table without assets.
    pub fn insert_layer(&mut self, name: &str, parent: Option<&str>, initial: LayerState, peers: LayerPeers) {
        self.layers.insert(
            name.to_owned(),
            LayerRecord {
                kind: LayerKind::Runtime,
                parent: parent.map(str::to_owned),
                requested: initial,
                peers,
                members: 0,
            },
        );
        self.dirty = true;
    }

    /// Every layer, ordered by name.
    pub fn layers(&self) -> impl Iterator<Item = LayerInfo<'_>> {
        self.layers.iter().map(|(name, record)| LayerInfo {
            name,
            kind: record.kind,
            parent: record.parent.as_deref(),
            requested: record.requested,
            effective: self.effective(name).unwrap_or(LayerState::Absent),
            member_count: record.members,
            peers: record.peers,
        })
    }

    /// The last requested state of a layer.
    pub fn requested(&self, layer: &str) -> Option<LayerState> {
        self.layers.get(layer).map(|r| r.requested)
    }

    /// `min(requested, effective(parent))`; `None` for an unknown layer.
    pub fn effective(&self, layer: &str) -> Option<LayerState> {
        let mut record = self.layers.get(layer)?;
        let mut state = record.requested;
        for _ in 0..MAX_LAYER_DEPTH {
            let Some(parent) = record.parent.as_deref().and_then(|p| self.layers.get(p)) else {
                return Some(state);
            };
            state = state.min(parent.requested);
            record = parent;
        }
        Some(LayerState::Absent)
    }

    /// Requests a state. `recursive` also sets every descendant. Returns `Ok(true)` when a
    /// requested state changed, `Ok(false)` when it already was that state or `side` does not own
    /// the layer, `Err` for an unknown layer. Never panics on user data.
    pub fn request(
        &mut self,
        layer: &str,
        state: LayerState,
        recursive: bool,
        side: Side,
    ) -> Result<bool, LayerError> {
        let Some(record) = self.layers.get(layer) else {
            return Err(LayerError::Unknown(layer.to_owned()));
        };
        if !side.owns(record.peers) {
            return Ok(false);
        }
        let mut targets = vec![layer.to_owned()];
        if recursive {
            targets.extend(self.descendants(layer));
        }
        let mut changed = false;
        for name in targets {
            if let Some(record) = self.layers.get_mut(&name)
                && side.owns(record.peers)
                && record.requested != state
            {
                record.requested = state;
                changed = true;
            }
        }
        self.dirty |= changed;
        Ok(changed)
    }

    /// Every descendant of `layer` (name order).
    fn descendants(&self, layer: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut frontier = vec![layer.to_owned()];
        while let Some(current) = frontier.pop() {
            for (name, record) in &self.layers {
                if record.parent.as_deref() == Some(current.as_str()) && !out.contains(name) && name != layer
                {
                    out.push(name.clone());
                    frontier.push(name.clone());
                }
            }
        }
        out.sort();
        out
    }

    /// True when any of `names` has effective state `state`.
    pub fn any_in_state(&self, names: &[&str], state: LayerState) -> bool {
        names.iter().any(|n| self.effective(n) == Some(state))
    }

    /// True when every one of `names` has effective state `state` (false for an empty list or an
    /// unknown name).
    pub fn all_in_state(&self, names: &[&str], state: LayerState) -> bool {
        !names.is_empty() && names.iter().all(|n| self.effective(n) == Some(state))
    }

    /// Whether `layer` is known.
    pub fn contains(&self, layer: &str) -> bool {
        self.layers.contains_key(layer)
    }

    /// The peers setting of a layer.
    pub fn peers(&self, layer: &str) -> Option<LayerPeers> {
        self.layers.get(layer).map(|r| r.peers)
    }

    /// The state at which an entity with `membership` is present: the lowest effective state of
    /// its known layers; `Live` when none of its layers is known.
    pub fn presence(&self, membership: &LayerMembership) -> LayerState {
        membership
            .layers
            .iter()
            .filter_map(|name| self.effective(name))
            .min()
            .unwrap_or(LayerState::Live)
    }

    /// Problems found while building the table.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The table a session starts from: the same layers at the level's `initial` states
    /// (`binding`, `Live` without an entry). Editor-only layers are absent, as in a cooked level.
    #[must_use]
    pub fn at_initial_states(&self, binding: Option<&LevelLayers>) -> Self {
        let mut table = Self::default();
        for (name, record) in &self.layers {
            let initial = binding
                .and_then(|b| b.entries.iter().find(|entry| entry.layer == *name))
                .map_or(LayerState::Live, |entry| entry.initial);
            let requested = match record.kind {
                LayerKind::EditorOnly => LayerState::Absent,
                LayerKind::Runtime => initial,
            };
            table.layers.insert(
                name.clone(),
                LayerRecord {
                    requested,
                    members: 0,
                    ..record.clone()
                },
            );
        }
        table.dirty = !table.layers.is_empty();
        table
    }

    /// Forgets what the table was built from: the next [`apply_layer_switches`] rebuilds the
    /// layers (a definition asset changed). Requested states stay.
    pub fn invalidate(&mut self) {
        self.built_for.clear();
    }

    /// Marks the table so the next [`apply_layer_switches`] re-evaluates every member.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Takes the active levels of `world` as the ones the table was built for, so the next
    /// [`apply_layer_switches`] keeps its layers and requested states instead of rebuilding them:
    /// a play session's table, copied from the edit world at its initial states
    /// ([`LayerTable::at_initial_states`]) and bound to the play level. Members are re-evaluated.
    pub fn bind_to_active_levels(&mut self, world: &World) {
        self.built_for = built_key(world, &active_levels(world));
        self.dirty = true;
    }

    /// Template records currently despawned because a layer is `Absent` (every level).
    #[must_use]
    pub fn despawned_count(&self) -> usize {
        self.absent.values().map(BTreeSet::len).sum()
    }

    /// Rebuilds the layer records for the active `levels` (sorted by id): definitions from the
    /// binding and `defs`, initial states from each level's [`LevelLayers`]. Layers already known
    /// keep their requested state; new ones, and those of the levels in `reloaded` (a level opened
    /// again under its id starts over), start at their binding's `initial`.
    fn rebuild(
        &mut self,
        levels: &[(LevelId, std::sync::Arc<crate::level::Level>)],
        defs: &LayerDefsLibrary,
        reloaded: &BTreeSet<LevelId>,
    ) {
        let mut layers: BTreeMap<String, LayerRecord> = BTreeMap::new();
        let mut warnings = Vec::new();
        for (id, level) in levels {
            let binding = match LevelLayers::from_scene(&level.scene) {
                None => continue,
                Some(Ok(binding)) => binding,
                Some(Err(err)) => {
                    warnings.push(format!("level {id}: LevelLayers does not parse: {err}"));
                    continue;
                }
            };
            let mut seen: BTreeMap<String, String> = BTreeMap::new();
            let mut assets: Vec<(String, &LayerDefs)> = binding
                .embedded
                .iter()
                .map(|asset| (format!("embedded in level {id}"), asset))
                .collect();
            for text in binding.unparsable_defs() {
                warnings.push(format!(
                    "level {id}: layer definition reference `{text}` is not an asset id; its layers are not bound"
                ));
            }
            for def_id in binding.def_ids() {
                match defs.get(def_id) {
                    Some(asset) => assets.push((def_id.to_string(), asset)),
                    None => warnings.push(format!("level {id}: layer definition {def_id} is not loaded")),
                }
            }
            for (def_id, asset) in assets {
                for def in &asset.layers {
                    if let Some(first) = seen.get(&def.name) {
                        warnings.push(format!(
                            "layer_ambiguous: `{}` is in definitions {first} and {def_id}; the first wins",
                            def.name
                        ));
                        continue;
                    }
                    seen.insert(def.name.clone(), def_id.clone());
                    let entry = binding.entries.iter().find(|e| e.layer == def.name);
                    let known = self
                        .layers
                        .get(&def.name)
                        .filter(|_| !reloaded.contains(id))
                        .map(|r| r.requested);
                    layers.entry(def.name.clone()).or_insert_with(|| LayerRecord {
                        kind: def.kind,
                        parent: def.parent.clone(),
                        requested: known.unwrap_or_else(|| entry.map_or(LayerState::Live, |e| e.initial)),
                        peers: entry.map_or(LayerPeers::Everyone, |e| e.peers),
                        members: 0,
                    });
                }
            }
            for entry in &binding.entries {
                if !seen.contains_key(&entry.layer) {
                    warnings.push(format!(
                        "layer_unknown: level {id} binds `{}` which no definition names",
                        entry.layer
                    ));
                }
            }
        }
        break_parent_cycles(&mut layers, &mut warnings);
        for warning in &warnings {
            tracing::warn!(target: "regia_scene", "{warning}");
        }
        self.layers = layers;
        self.warnings = warnings;
        self.dirty = true;
    }
}

/// Cuts the parent link of every layer whose ancestry loops or runs past `MAX_LAYER_DEPTH`, so
/// `effective` and `requested` agree for definitions the asset loader never validated (embedded
/// ones). Layers are visited in name order, so the result is deterministic.
fn break_parent_cycles(layers: &mut BTreeMap<String, LayerRecord>, warnings: &mut Vec<String>) {
    let names: Vec<String> = layers.keys().cloned().collect();
    for name in names {
        let mut seen = BTreeSet::new();
        seen.insert(name.clone());
        let mut cursor = layers.get(&name).and_then(|r| r.parent.clone());
        let mut broken = false;
        while let Some(parent) = cursor {
            if !seen.insert(parent.clone()) || seen.len() > MAX_LAYER_DEPTH {
                broken = true;
                break;
            }
            cursor = layers.get(&parent).and_then(|r| r.parent.clone());
        }
        if broken && let Some(record) = layers.get_mut(&name) {
            warnings.push(format!(
                "layer_parent_invalid: `{name}` has a parent chain that loops or is deeper than {MAX_LAYER_DEPTH}; its parent is ignored"
            ));
            record.parent = None;
        }
    }
}

/// Active levels and their data, sorted by id.
fn active_levels(world: &World) -> Vec<(LevelId, std::sync::Arc<crate::level::Level>)> {
    let (Some(manager), Some(library)) = (
        world.get_resource::<LevelManager>(),
        world.get_resource::<LevelLibrary>(),
    ) else {
        return Vec::new();
    };
    let mut out: Vec<_> = manager
        .levels()
        .filter(|(_, state)| *state == LevelState::Active)
        .filter_map(|(id, _)| library.get(id).map(|level| (id, level.clone())))
        .collect();
    out.sort_by_key(|(id, _)| *id);
    out
}

/// `true` when `built_for` is exactly the active levels that have data, with their current load
/// generations. Walks the manager and looks ids up; it does not build the level list.
fn active_key_matches(world: &World, built_for: &[(LevelId, u64)]) -> bool {
    let (Some(manager), Some(library)) = (
        world.get_resource::<LevelManager>(),
        world.get_resource::<LevelLibrary>(),
    ) else {
        return built_for.is_empty();
    };
    let mut count = 0usize;
    for (id, state) in manager.levels() {
        if state != LevelState::Active || library.get(id).is_none() {
            continue;
        }
        let Ok(pos) = built_for.binary_search_by_key(&id, |entry| entry.0) else {
            return false;
        };
        if built_for[pos].1 != manager.load_generation(id) {
            return false;
        }
        count += 1;
    }
    count == built_for.len()
}

/// An idle switch: a table is present, nothing was requested, and the active levels still match
/// the key it was built for. No table means there is nothing to apply.
fn layer_switch_is_idle(world: &World) -> bool {
    match world.get_resource::<LayerTable>() {
        None => true,
        Some(table) if table.dirty => false,
        Some(table) => active_key_matches(world, &table.built_for),
    }
}

/// The active levels with their load generations: what a table is built for.
fn built_key(
    world: &World,
    levels: &[(LevelId, std::sync::Arc<crate::level::Level>)],
) -> Vec<(LevelId, u64)> {
    let manager = world.get_resource::<LevelManager>();
    levels
        .iter()
        .map(|(id, _)| (*id, manager.map_or(0, |m| m.load_generation(*id))))
        .collect()
}

/// Applies requested layer states to the world (registered in the [`LayerSwitch`] set by
/// [`crate::ScenePlugin`]). Does nothing unless the table is dirty or the set of active levels
/// changed. An idle frame compares the active levels to the key already stored on the table and
/// allocates nothing.
pub fn apply_layer_switches(world: &mut World) {
    if layer_switch_is_idle(world) {
        return;
    }
    let Some(mut table) = world.remove_resource::<LayerTable>() else {
        return;
    };
    let levels = active_levels(world);
    let built = built_key(world, &levels);
    if built != table.built_for {
        let defs = world
            .get_resource::<LayerDefsLibrary>()
            .cloned()
            .unwrap_or_default();
        // A level whose generation moved was opened again: it respawned every member, so nothing
        // of it is absent any more and its layers start over.
        let reloaded: BTreeSet<LevelId> = built
            .iter()
            .filter(|now| table.built_for.iter().any(|old| old.0 == now.0 && old.1 != now.1))
            .map(|(id, _)| *id)
            .collect();
        table.rebuild(&levels, &defs, &reloaded);
        table
            .absent
            .retain(|level, _| built.iter().any(|(id, _)| id == level) && !reloaded.contains(level));
        table.built_for = built;
    }
    if table.dirty {
        let _span = tracing::info_span!("layer_switch", layers = table.layers.len()).entered();
        apply_states(world, &mut table, &levels);
        table.dirty = false;
        let mut changes = Vec::new();
        for info in table.layers() {
            let before = table.last_effective.get(info.name).copied();
            if before.is_some_and(|b| b != info.effective) {
                changes.push(LayerChanged {
                    layer: info.name.to_owned(),
                    from: before.unwrap_or(info.effective),
                    to: info.effective,
                });
            }
        }
        table.last_effective = table.layers().map(|i| (i.name.to_owned(), i.effective)).collect();
        for change in changes {
            world.write_message(change);
        }
    }
    world.insert_resource(table);
}

/// One spawned layer member: guid (none for a member spawned without an [`EntityGuid`]), entity,
/// presence, parked marker, level.
type MemberRow = (
    Option<Guid>,
    Entity,
    LayerState,
    Option<LayerParked>,
    Option<LevelId>,
);

/// Respawns absent members that are wanted again, then despawns, parks and unparks spawned
/// members to match the table. Members are processed in guid order so entity ids are a function
/// of the input sequence only. A member without an [`EntityGuid`] (spawned by a script or a tool)
/// is switched too, after the guid carriers in entity order: it parks and goes live with its
/// layers, and an `Absent` layer parks it, since it has no template record to come back from
/// (ADR-7200, as a member with no record parks under ADR-1953 (14)).
fn apply_states(
    world: &mut World,
    table: &mut LayerTable,
    levels: &[(LevelId, std::sync::Arc<crate::level::Level>)],
) {
    // One pass per level: membership and the parent-to-children index. Member counts and the
    // respawn walk both read the index, so a parent chain does not rescan the file per rank.
    for record in table.layers.values_mut() {
        record.members = 0;
    }
    let mut indexes = Vec::with_capacity(levels.len());
    for (_, level) in levels {
        let index = index_level(level);
        for membership in index.membership.values() {
            for name in &membership.layers {
                if let Some(layer) = table.layers.get_mut(name) {
                    layer.members += 1;
                }
            }
        }
        indexes.push(index);
    }

    // 1. Respawn absent members whose presence is now at least Parked.
    for ((level_id, level), index) in levels.iter().zip(indexes.iter()) {
        let Some(absent) = table.absent.get(level_id).cloned() else {
            continue;
        };
        let wanted: BTreeSet<Guid> = absent
            .iter()
            .copied()
            .filter(|guid| {
                index
                    .membership
                    .get(guid)
                    .is_some_and(|membership| table.presence(membership) >= LayerState::Parked)
            })
            .collect();
        if wanted.is_empty() {
            continue;
        }
        let wanted = with_following_children(&index.children, &index.membership, &absent, wanted);
        // A failed respawn keeps the guids absent so a later switch tries again.
        if respawn_members(world, *level_id, &level.scene, &wanted)
            && let Some(set) = table.absent.get_mut(level_id)
        {
            set.retain(|g| !wanted.contains(g));
        }
    }

    // 2. Despawn / park / unpark spawned members.
    let mut members: Vec<MemberRow> = Vec::new();
    {
        let mut query = QueryState::<(
            Entity,
            Option<&EntityGuid>,
            &LayerMembership,
            Option<&LayerParked>,
            Option<&LevelMember>,
        )>::new(world);
        for (entity, guid, membership, parked, member) in query.iter(world) {
            members.push((
                guid.map(|g| g.0),
                entity,
                table.presence(membership),
                parked.copied(),
                member.map(LevelMember::level),
            ));
        }
    }
    // Guid carriers in guid order, then the guid-less members in entity order.
    members.sort_by_key(|(guid, entity, ..)| (guid.is_none(), *guid, *entity));
    // Only an entity with a template record can come back from a despawn.
    let templates: BTreeMap<LevelId, BTreeSet<Guid>> = levels
        .iter()
        .map(|(id, level)| (*id, level.scene.entities.iter().map(|r| r.guid).collect()))
        .collect();
    // Members whose marker changed: their descendants follow below.
    let mut toggled: Vec<(Entity, bool)> = Vec::new();
    for (guid, entity, presence, parked, level) in members {
        let template = level.and_then(|l| templates.get(&l));
        let has_template = guid.is_some_and(|guid| template.is_some_and(|guids| guids.contains(&guid)));
        if world.get_entity(entity).is_err() {
            // Taken by an ancestor's despawn: remember it so it comes back with its layer (a
            // member present while its parent is absent is `layer_parent_mismatch`).
            if let (Some(level), Some(guid)) = (level.filter(|_| has_template), guid) {
                table.absent.entry(level).or_default().insert(guid);
            }
            continue;
        }
        match (presence, level) {
            (LayerState::Absent, Some(level)) if has_template => {
                // The despawn takes the subtree, so whatever has a template record is remembered
                // now, with or without a membership and whatever its guid order.
                let gone = template_subtree(world, entity, template);
                table.absent.entry(level).or_default().extend(gone);
                if let Ok(entity_mut) = world.get_entity_mut(entity) {
                    entity_mut.despawn();
                }
                // `has_template` implies a guid.
                if let Some(guid) = guid {
                    table.absent.entry(level).or_default().insert(guid);
                }
            }
            (LayerState::Absent | LayerState::Parked, _) => {
                if parked.is_none()
                    && let Ok(mut entity_mut) = world.get_entity_mut(entity)
                {
                    let restore = entity_mut.get::<Visibility>().copied().unwrap_or_default();
                    entity_mut.insert((
                        LayerParked {
                            restore,
                            inherited: false,
                        },
                        Visibility::Hidden,
                    ));
                    toggled.push((entity, true));
                }
            }
            (LayerState::Live, _) => {
                if let Some(parked) = parked
                    && let Ok(mut entity_mut) = world.get_entity_mut(entity)
                {
                    entity_mut.remove::<LayerParked>();
                    entity_mut.insert(parked.restore);
                    toggled.push((entity, false));
                }
            }
        }
    }
    for (entity, parked) in toggled {
        mark_descendants(world, entity, parked);
    }
}

/// Applies the layer rules to a member spawned outside the level's own spawn, such as an entity of a
/// streamed cell (spec 78 § 10.1). The rules are the ones [`apply_layer_switches`] uses, so a later
/// switch can always undo what this did:
///
/// - an `Absent` member with a record in its level's template is despawned with its subtree and
///   remembered as absent, so the switch back respawns it from the template;
/// - an `Absent` member with no template record (a streamed cell's: the level's scene does not hold
///   cell entities) and a `Parked` member are parked: hidden, not simulated, not picked (ADR-7411);
/// - a `Live` member is left alone.
///
/// Calling it again on an already parked member changes nothing (the restore visibility stays the
/// one from before the park). Returns whether the entity stays. An entity with no membership, or a
/// world with no layer table, is left alone.
pub fn apply_member_presence(world: &mut World, entity: Entity) -> bool {
    let Ok(entity_ref) = world.get_entity(entity) else {
        return false;
    };
    let presence = match (
        entity_ref.get::<LayerMembership>(),
        world.get_resource::<LayerTable>(),
    ) {
        (Some(membership), Some(table)) => table.presence(membership),
        _ => return true,
    };
    let parked = entity_ref.get::<LayerParked>().is_some();
    let guid = entity_ref.get::<EntityGuid>().map(|g| g.0);
    let level = entity_ref.get::<LevelMember>().map(LevelMember::level);
    let template: Option<BTreeSet<Guid>> = level.and_then(|id| {
        let level = world.get_resource::<LevelLibrary>()?.get(id)?;
        Some(level.scene.entities.iter().map(|r| r.guid).collect())
    });
    let has_template = guid.is_some_and(|g| template.as_ref().is_some_and(|t| t.contains(&g)));
    match (presence, level, guid) {
        (LayerState::Absent, Some(level), Some(guid)) if has_template => {
            let mut gone = template_subtree(world, entity, template.as_ref());
            gone.push(guid);
            if let Ok(entity_mut) = world.get_entity_mut(entity) {
                entity_mut.despawn();
            }
            if let Some(mut table) = world.get_resource_mut::<LayerTable>() {
                table.absent.entry(level).or_default().extend(gone);
            }
            false
        }
        (LayerState::Absent | LayerState::Parked, ..) => {
            if !parked && let Ok(mut entity_mut) = world.get_entity_mut(entity) {
                let restore = entity_mut.get::<Visibility>().copied().unwrap_or_default();
                entity_mut.insert((
                    LayerParked {
                        restore,
                        inherited: false,
                    },
                    Visibility::Hidden,
                ));
                mark_descendants(world, entity, true);
            }
            true
        }
        (LayerState::Live, ..) => true,
    }
}

/// Parks or unparks `entity` the way the layer switch parks a member: parking stores its
/// [`Visibility`] in a [`LayerParked`] (not inherited) and hides it, unparking restores it; its
/// descendants without a membership follow. For entities that follow something other than their
/// own membership, such as a procedural generator's output root following its generator
/// (spec 62 § 7.3, ADR-7214). Returns whether anything changed; a marker that was inherited is
/// replaced by the entity's own.
pub fn set_parked(world: &mut World, entity: Entity, parked: bool) -> bool {
    let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
        return false;
    };
    let marker = entity_mut.get::<LayerParked>().copied();
    match (parked, marker) {
        (true, Some(m)) if !m.inherited => return false,
        (true, Some(m)) => {
            entity_mut.insert(LayerParked {
                restore: m.restore,
                inherited: false,
            });
            entity_mut.insert(Visibility::Hidden);
        }
        (true, None) => {
            let restore = entity_mut.get::<Visibility>().copied().unwrap_or_default();
            entity_mut.insert((
                LayerParked {
                    restore,
                    inherited: false,
                },
                Visibility::Hidden,
            ));
        }
        (false, Some(m)) => {
            entity_mut.remove::<LayerParked>();
            if !m.inherited {
                entity_mut.insert(m.restore);
            }
        }
        (false, None) => return false,
    }
    mark_descendants(world, entity, parked);
    true
}

/// The guids under `root` that have a record in `template`. Only live entities count, so what
/// gameplay removed earlier does not come back with its layer.
fn template_subtree(world: &World, root: Entity, template: Option<&BTreeSet<Guid>>) -> Vec<Guid> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(entity) = stack.pop() {
        if let Some(children) = world.get::<Children>(entity) {
            for child in children.iter() {
                stack.push(child);
                if let Some(guid) = world.get::<EntityGuid>(child).map(|g| g.0)
                    && template.is_some_and(|guids| guids.contains(&guid))
                {
                    found.push(guid);
                }
            }
        }
    }
    found
}

/// Membership parsed once, and each parent's children in file order.
struct LevelIndex {
    membership: HashMap<Guid, LayerMembership>,
    children: HashMap<Guid, Vec<Guid>>,
}

/// One pass over `level`: parse each membership and record direct children.
fn index_level(level: &crate::level::Level) -> LevelIndex {
    let mut membership = HashMap::new();
    let mut children: HashMap<Guid, Vec<Guid>> = HashMap::new();
    for record in &level.scene.entities {
        note_record_scan();
        if let Some(found) = LayerMembership::from_record(record) {
            membership.insert(record.guid, found);
        }
        if let Some(parent) = record.parent {
            children.entry(parent).or_default().push(record.guid);
        }
    }
    LevelIndex { membership, children }
}

/// `wanted` plus the absent records with no membership under it: those have no state of their own
/// and follow their parent. The child lists are the level's file order; `respawn_members` sorts
/// by guid, so entity ids stay a function of the input.
fn with_following_children(
    children: &HashMap<Guid, Vec<Guid>>,
    membership: &HashMap<Guid, LayerMembership>,
    absent: &BTreeSet<Guid>,
    mut wanted: BTreeSet<Guid>,
) -> BTreeSet<Guid> {
    let mut stack: Vec<Guid> = wanted.iter().copied().collect();
    while let Some(parent) = stack.pop() {
        let Some(kids) = children.get(&parent) else {
            continue;
        };
        for child in kids {
            if absent.contains(child) && !membership.contains_key(child) && wanted.insert(*child) {
                stack.push(*child);
            }
        }
    }
    wanted
}

/// Sets or clears the inherited [`LayerParked`] on the descendants of `root` that have no
/// membership. A descendant with one governs itself and what hangs under it.
fn mark_descendants(world: &mut World, root: Entity, parked: bool) {
    let children_of = |world: &World, entity: Entity| -> Vec<Entity> {
        world
            .get::<Children>(entity)
            .map_or_else(Vec::new, |children| children.iter().collect())
    };
    let mut stack = children_of(world, root);
    while let Some(entity) = stack.pop() {
        if world.get::<LayerMembership>(entity).is_some() {
            continue;
        }
        stack.extend(children_of(world, entity));
        let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
            continue;
        };
        match entity_mut.get::<LayerParked>().copied() {
            None if parked => {
                let restore = entity_mut.get::<Visibility>().copied().unwrap_or_default();
                entity_mut.insert(LayerParked {
                    restore,
                    inherited: true,
                });
            }
            Some(marker) if !parked && marker.inherited => {
                entity_mut.remove::<LayerParked>();
            }
            _ => {}
        }
    }
}

/// Whether any entity is parked: [`follow_parked_parents`] has nothing to do otherwise.
pub fn any_parked(parked: Query<(), With<LayerParked>>) -> bool {
    !parked.is_empty()
}

/// Entities that got a new parent since the last run.
type ReparentedQuery<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static ChildOf, Option<&'static LayerParked>),
    (Changed<ChildOf>, Without<LayerMembership>),
>;

/// `PostUpdate`, in [`LayerSwitch`]: entities attached under a parked entity after the switch (a
/// model's parts arriving while its owner is parked) follow it, and entities moved out of a
/// parked subtree stop following it.
pub fn follow_parked_parents(
    world: &mut World,
    state: &mut SystemState<FollowParkedParams<'static, 'static>>,
) {
    let _span = tracing::debug_span!("follow_parked_parents").entered();
    let Ok((changed, mut orphaned, parked)) = state.get_mut(world) else {
        return;
    };
    let mut updates: Vec<(Entity, bool)> = Vec::new();
    for (entity, link, own) in &changed {
        match (parked.contains(link.parent()), own) {
            (true, None) => updates.push((entity, true)),
            (false, Some(marker)) if marker.inherited => updates.push((entity, false)),
            _ => {}
        }
    }
    for entity in orphaned.read() {
        if parked.get(entity).is_ok_and(|marker| marker.inherited) {
            updates.push((entity, false));
        }
    }
    for (entity, follow) in updates {
        if let Ok(mut entity_mut) = world.get_entity_mut(entity) {
            if follow {
                let restore = entity_mut.get::<Visibility>().copied().unwrap_or_default();
                entity_mut.insert(LayerParked {
                    restore,
                    inherited: true,
                });
            } else {
                entity_mut.remove::<LayerParked>();
            }
        }
        mark_descendants(world, entity, follow);
    }
}

/// The system parameters of [`follow_parked_parents`].
pub type FollowParkedParams<'w, 's> = (
    ReparentedQuery<'w, 's>,
    RemovedComponents<'w, 's, ChildOf>,
    Query<'w, 's, &'static LayerParked>,
);

/// Spawns the records `wanted` of a level's template, parenting them to live ancestors (adopted
/// by guid). Records whose parent is neither wanted nor alive spawn as roots.
/// Returns whether the spawn succeeded (a failed one spawns nothing).
fn respawn_members(world: &mut World, level: LevelId, scene: &SceneFile, wanted: &BTreeSet<Guid>) -> bool {
    let mut live: HashMap<Guid, Entity> = HashMap::new();
    {
        let mut query = QueryState::<(Entity, &EntityGuid, &LevelMember)>::new(world);
        for (entity, guid, member) in query.iter(world) {
            if member.level() == level {
                live.insert(guid.0, entity);
            }
        }
    }
    // A level reloaded in place respawned everything already: never duplicate a live guid.
    let wanted: BTreeSet<Guid> = wanted.iter().filter(|g| !live.contains_key(g)).copied().collect();
    let mut subset = SceneFile::new();
    let mut adopt: HashMap<Guid, Entity> = HashMap::new();
    for record in scene.entities.iter().filter(|r| wanted.contains(&r.guid)) {
        let mut record = record.clone();
        if let Some(parent) = record.parent
            && !wanted.contains(&parent)
        {
            match live.get(&parent) {
                Some(entity) => {
                    adopt.insert(parent, *entity);
                }
                None => record.parent = None,
            }
        }
        subset.entities.push(record);
    }
    for guid in adopt.keys() {
        subset.entities.push(SceneEntity {
            guid: *guid,
            ..SceneEntity::default()
        });
    }
    subset.entities.sort_by_key(|e| e.guid);
    let opts = crate::spawn::SpawnOptions {
        level: Some(level),
        ..Default::default()
    };
    let old_children = crate::level::children_of_adopted(world, &adopt);
    match crate::spawn::spawn_scene_adopting(world, &subset, &opts, &adopt) {
        Ok(spawned) => {
            for warning in &spawned.warnings {
                tracing::warn!(target: "regia_scene", "layer respawn in level {level}: {warning}");
            }
            // Same file, same space as the level open: fresh roots follow the floating origin, or
            // go back under the placed instance that streams the level.
            if crate::placed::streamed_by(world, level).is_some() {
                crate::placed::reattach_streamed(world, level);
            } else {
                crate::level::shift_new_roots_to_origin(world, &spawned.roots, &adopt, &old_children);
            }
            true
        }
        Err(err) => {
            tracing::warn!(target: "regia_scene", "layer respawn in level {level}: {err}");
            false
        }
    }
}

/// A validation finding (spec 37 § 4.5). `fatal` findings make a definition unusable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerProblem {
    /// Reason string (`layer_parent_invalid`, `layer_unknown`, `layer_ambiguous`,
    /// `layer_parent_mismatch`, `layer_bad_name`, `layer_duplicate`, `layer_editor_parent`).
    pub reason: &'static str,
    /// Human-readable detail.
    pub detail: String,
    /// Whether the finding is fatal for the definition.
    pub fatal: bool,
}

/// Whether `name` is a valid layer name (`[a-z0-9_]{1,48}`).
pub fn valid_layer_name(name: &str) -> bool {
    (1..=48).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Validates a definition asset: names, duplicates, missing parents and cycles
/// (`layer_parent_invalid`, fatal), editor-only parents of runtime layers.
pub fn validate_defs(defs: &LayerDefs) -> Vec<LayerProblem> {
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for def in &defs.layers {
        if !valid_layer_name(&def.name) {
            problems.push(LayerProblem {
                reason: "layer_bad_name",
                detail: format!("`{}` is not [a-z0-9_]{{1,48}}", def.name),
                fatal: true,
            });
        }
        if !seen.insert(def.name.as_str()) {
            problems.push(LayerProblem {
                reason: "layer_duplicate",
                detail: format!("`{}` is defined twice", def.name),
                fatal: true,
            });
        }
    }
    for def in &defs.layers {
        let Some(parent) = &def.parent else { continue };
        let Some(parent_def) = defs.get(parent) else {
            problems.push(LayerProblem {
                reason: "layer_parent_invalid",
                detail: format!("`{}` names missing parent `{parent}`", def.name),
                fatal: true,
            });
            continue;
        };
        if parent_def.kind == LayerKind::EditorOnly && def.kind == LayerKind::Runtime {
            problems.push(LayerProblem {
                reason: "layer_editor_parent",
                detail: format!("runtime layer `{}` has editor-only parent `{parent}`", def.name),
                fatal: false,
            });
        }
        // Cycle: walk up at most layers.len() steps.
        let mut cursor = Some(parent.as_str());
        let mut steps = 0;
        while let Some(name) = cursor {
            if name == def.name {
                problems.push(LayerProblem {
                    reason: "layer_parent_invalid",
                    detail: format!("`{}` is its own ancestor", def.name),
                    fatal: true,
                });
                break;
            }
            steps += 1;
            if steps > defs.layers.len() {
                break;
            }
            cursor = defs.get(name).and_then(|d| d.parent.as_deref());
        }
    }
    problems
}

/// Validates a level against the definitions it binds: memberships naming an unbound layer
/// (`layer_unknown`), a name in two bound definitions (`layer_ambiguous`), and a member whose
/// entity-parent is in layers the child is not in (`layer_parent_mismatch`).
pub fn validate_level(scene: &SceneFile, bound: &[&LayerDefs]) -> Vec<LayerProblem> {
    let mut problems = Vec::new();
    let mut known: BTreeMap<&str, usize> = BTreeMap::new();
    for defs in bound {
        for def in &defs.layers {
            *known.entry(def.name.as_str()).or_default() += 1;
        }
    }
    for (name, count) in &known {
        if *count > 1 {
            problems.push(LayerProblem {
                reason: "layer_ambiguous",
                detail: format!("`{name}` is in {count} bound definitions"),
                fatal: false,
            });
        }
    }
    let memberships: BTreeMap<Guid, LayerMembership> = scene
        .entities
        .iter()
        .filter_map(|r| LayerMembership::from_record(r).map(|m| (r.guid, m)))
        .collect();
    for record in &scene.entities {
        // At run time a membership that does not parse is "no layers", so the entity stays visible
        // when its layer is `Absent`: the validator is the one place that can say so.
        if record.components.contains_key(LAYER_MEMBERSHIP_PATH) && !memberships.contains_key(&record.guid) {
            problems.push(LayerProblem {
                reason: "layer_membership_invalid",
                detail: format!("{}: LayerMembership does not parse", record.guid),
                fatal: false,
            });
        }
        let own = memberships.get(&record.guid);
        for name in own.iter().flat_map(|m| m.layers.iter()) {
            if !known.contains_key(name.as_str()) {
                problems.push(LayerProblem {
                    reason: "layer_unknown",
                    detail: format!("entity {} names unbound layer `{name}`", record.guid),
                    fatal: false,
                });
            }
        }
        let Some(parent) = record.parent else { continue };
        let Some(parent_layers) = memberships.get(&parent).filter(|m| !m.layers.is_empty()) else {
            continue;
        };
        let missing: Vec<&String> = parent_layers
            .layers
            .iter()
            .filter(|l| !own.is_some_and(|m| m.layers.contains(l)))
            .collect();
        if !missing.is_empty() {
            problems.push(LayerProblem {
                reason: "layer_parent_mismatch",
                detail: format!(
                    "entity {} is not in its parent's layers {missing:?}; the parent can be absent while it is present",
                    record.guid
                ),
                fatal: false,
            });
        }
    }
    problems
}

/// `scene` without the records in `dropped` and their descendants.
fn without_subtrees(scene: &SceneFile, mut dropped: BTreeSet<Guid>) -> SceneFile {
    loop {
        let before = dropped.len();
        for record in &scene.entities {
            if record.parent.is_some_and(|p| dropped.contains(&p)) {
                dropped.insert(record.guid);
            }
        }
        if dropped.len() == before {
            break;
        }
    }
    let mut out = scene.clone();
    out.entities.retain(|r| !dropped.contains(&r.guid));
    out
}

/// Drops the entities carrying the component `type_path`, with their descendants, and answers how
/// many records went (the cook's strip of dev-only content).
#[must_use]
pub fn strip_component_holders(scene: &SceneFile, type_path: &str) -> (SceneFile, usize) {
    let holders: BTreeSet<Guid> = scene
        .entities
        .iter()
        .filter(|record| record.components.contains_key(type_path))
        .map(|record| record.guid)
        .collect();
    let mut out = without_subtrees(scene, holders);
    let mut removed = scene.entities.len() - out.entities.len();
    for record in &mut out.entities {
        removed += drop_added_children(record, &|child| child.components.contains_key(type_path));
    }
    (out, removed)
}

/// Removes the records `drop` picks, with their descendants, from the children a prefab instance
/// record adds, at any depth, and answers how many went.
fn drop_added_children(record: &mut SceneEntity, drop: &dyn Fn(&SceneEntity) -> bool) -> usize {
    let Some(instance) = &mut record.prefab_instance else {
        return 0;
    };
    let mut dropped: BTreeSet<Guid> = instance
        .added_children
        .iter()
        .filter(|child| drop(child))
        .map(|child| child.guid)
        .collect();
    loop {
        let before = dropped.len();
        for child in &instance.added_children {
            if child.parent.is_some_and(|parent| dropped.contains(&parent)) {
                dropped.insert(child.guid);
            }
        }
        if dropped.len() == before {
            break;
        }
    }
    instance
        .added_children
        .retain(|child| !dropped.contains(&child.guid));
    let nested: usize = instance
        .added_children
        .iter_mut()
        .map(|child| drop_added_children(child, drop))
        .sum();
    dropped.len() + nested
}

/// The cook step for layers: drops members of editor-only layers (unless they also belong to a
/// runtime layer) and their descendants, and removes editor-only names from memberships.
pub fn strip_editor_only(scene: &SceneFile, bound: &[&LayerDefs]) -> SceneFile {
    let kind_of = |name: &str| bound.iter().find_map(|d| d.get(name)).map(|d| d.kind);
    let editor_only = |record: &SceneEntity| {
        LayerMembership::from_record(record).is_some_and(|m| {
            let editor = m.layers.iter().any(|l| kind_of(l) == Some(LayerKind::EditorOnly));
            let runtime = m.layers.iter().any(|l| kind_of(l) != Some(LayerKind::EditorOnly));
            editor && !runtime
        })
    };
    let dropped: BTreeSet<Guid> = scene
        .entities
        .iter()
        .filter(|record| editor_only(record))
        .map(|record| record.guid)
        .collect();
    let mut out = without_subtrees(scene, dropped);
    for record in &mut out.entities {
        drop_added_children(record, &editor_only);
        clear_editor_names(record, &kind_of);
    }
    out
}

/// Removes editor-only names from a record's membership and from the children it adds.
fn clear_editor_names(record: &mut SceneEntity, kind_of: &dyn Fn(&str) -> Option<LayerKind>) {
    if let Some(mut m) = LayerMembership::from_record(record) {
        let before = m.layers.len();
        m.layers.retain(|l| kind_of(l) != Some(LayerKind::EditorOnly));
        if m.layers.len() != before
            && let Ok(text) = ron::to_string(&m)
            && let Ok(raw) = ron::value::RawValue::from_boxed_ron(text.into_boxed_str())
        {
            record.components.insert(LAYER_MEMBERSHIP_PATH.to_owned(), raw);
        }
    }
    if let Some(instance) = &mut record.prefab_instance {
        for child in &mut instance.added_children {
            clear_editor_names(child, kind_of);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(parent: Option<&str>) -> LayerRecord {
        LayerRecord {
            kind: LayerKind::Runtime,
            parent: parent.map(str::to_owned),
            requested: LayerState::Live,
            peers: LayerPeers::Everyone,
            members: 0,
        }
    }

    #[test]
    fn parent_cycles_and_overlong_chains_are_cut() {
        let mut layers = BTreeMap::new();
        layers.insert("a".to_owned(), record(Some("b")));
        layers.insert("b".to_owned(), record(Some("a")));
        layers.insert("self_loop".to_owned(), record(Some("self_loop")));
        layers.insert("ok_child".to_owned(), record(Some("ok_root")));
        layers.insert("ok_root".to_owned(), record(None));
        let mut warnings = Vec::new();
        break_parent_cycles(&mut layers, &mut warnings);
        // `a` is cut first (name order); `b` then reaches a root.
        assert_eq!(layers["a"].parent, None);
        assert_eq!(layers["b"].parent.as_deref(), Some("a"));
        assert_eq!(layers["self_loop"].parent, None);
        assert_eq!(layers["ok_child"].parent.as_deref(), Some("ok_root"));
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().all(|w| w.starts_with("layer_parent_invalid")));

        // A chain of 100 layers: the cut leaves no chain longer than the depth cap.
        let mut chain = BTreeMap::new();
        for i in 0..100 {
            let parent = (i + 1 < 100).then(|| format!("c{:03}", i + 1));
            chain.insert(format!("c{i:03}"), record(parent.as_deref()));
        }
        break_parent_cycles(&mut chain, &mut Vec::new());
        let table = LayerTable {
            layers: chain,
            ..LayerTable::default()
        };
        for i in 0..100 {
            let name = format!("c{i:03}");
            assert_eq!(table.effective(&name), Some(LayerState::Live), "{name}");
        }
    }

    fn table() -> LayerTable {
        let mut t = LayerTable::default();
        t.insert_layer("quest_a", None, LayerState::Live, LayerPeers::Everyone);
        t.insert_layer(
            "quest_a_boss",
            Some("quest_a"),
            LayerState::Live,
            LayerPeers::Everyone,
        );
        t.insert_layer("night", None, LayerState::Parked, LayerPeers::Everyone);
        t.insert_layer("server", None, LayerState::Live, LayerPeers::ServerOnly);
        t
    }

    fn plain(id: u128, parent: Option<u128>) -> SceneEntity {
        SceneEntity {
            guid: Guid::from_u128(id),
            parent: parent.map(Guid::from_u128),
            ..SceneEntity::default()
        }
    }

    fn holding(mut record: SceneEntity, type_path: &str, value: &str) -> SceneEntity {
        let raw = ron::value::RawValue::from_boxed_ron(value.to_owned().into_boxed_str()).expect("ron");
        record.components.insert(type_path.to_owned(), raw);
        record
    }

    fn adding(mut record: SceneEntity, children: Vec<SceneEntity>) -> SceneEntity {
        let mut instance = crate::prefab::PrefabInstance::default().to_data();
        instance.added_children = children;
        record.prefab_instance = Some(instance);
        record
    }

    fn added(record: &SceneEntity) -> Vec<u128> {
        record
            .prefab_instance
            .iter()
            .flat_map(|instance| &instance.added_children)
            .map(|child| child.guid.as_u128())
            .collect()
    }

    #[test]
    fn the_test_strip_reaches_the_children_a_prefab_instance_adds() {
        const TEST: &str = "game::FunctionalTest";
        let nested = adding(plain(0x4, None), vec![holding(plain(0x5, None), TEST, "()")]);
        let host = adding(
            plain(0x1, None),
            vec![
                holding(plain(0x2, None), TEST, "()"),
                plain(0x3, Some(0x2)),
                nested,
            ],
        );
        let mut scene = SceneFile::new();
        scene.entities.push(host);
        let (out, removed) = strip_component_holders(&scene, TEST);
        assert_eq!(removed, 3, "the holder, its child and the nested holder");
        assert_eq!(added(&out.entities[0]), [0x4]);
        let kept = &out.entities[0]
            .prefab_instance
            .as_ref()
            .expect("instance")
            .added_children[0];
        assert!(added(kept).is_empty());
    }

    #[test]
    fn the_editor_strip_reaches_the_children_a_prefab_instance_adds() {
        let defs = LayerDefs::from_ron_str(
            r#"LayerDefs(format_version: 1, layers: [
                (name: "blockout", kind: EditorOnly, debug_color: (0.0, 0.0, 0.0), parent: None),
                (name: "night", kind: Runtime, debug_color: (0.0, 0.0, 0.0), parent: None),
            ])"#,
        )
        .expect("defs");
        let host = adding(
            plain(0x1, None),
            vec![
                holding(
                    plain(0x2, None),
                    LAYER_MEMBERSHIP_PATH,
                    r#"(layers: ["blockout"])"#,
                ),
                holding(
                    plain(0x3, None),
                    LAYER_MEMBERSHIP_PATH,
                    r#"(layers: ["blockout", "night"])"#,
                ),
            ],
        );
        let mut scene = SceneFile::new();
        scene.entities.push(host);
        let out = strip_editor_only(&scene, &[&defs]);
        assert_eq!(added(&out.entities[0]), [0x3]);
        let kept = &out.entities[0]
            .prefab_instance
            .as_ref()
            .expect("instance")
            .added_children[0];
        let membership = LayerMembership::from_record(kept).expect("membership");
        assert_eq!(membership.layers, ["night"]);
    }

    #[test]
    fn child_is_clamped_by_parent_and_recursion_sets_descendants() {
        let mut t = table();
        assert_eq!(
            t.request("quest_a", LayerState::Parked, false, Side::Local),
            Ok(true)
        );
        assert_eq!(t.requested("quest_a_boss"), Some(LayerState::Live));
        assert_eq!(t.effective("quest_a_boss"), Some(LayerState::Parked));
        assert_eq!(
            t.request("quest_a", LayerState::Parked, false, Side::Local),
            Ok(false)
        );
        assert_eq!(
            t.request("quest_a", LayerState::Absent, true, Side::Local),
            Ok(true)
        );
        assert_eq!(t.requested("quest_a_boss"), Some(LayerState::Absent));
        assert_eq!(
            t.request("nope", LayerState::Live, false, Side::Local),
            Err(LayerError::Unknown("nope".into()))
        );
    }

    #[test]
    fn presence_is_the_lowest_effective_state() {
        let t = table();
        let both = LayerMembership {
            layers: vec!["quest_a".into(), "night".into()],
        };
        assert_eq!(t.presence(&both), LayerState::Parked);
        let unknown = LayerMembership {
            layers: vec!["missing".into()],
        };
        assert_eq!(t.presence(&unknown), LayerState::Live);
        assert!(t.all_in_state(&["quest_a", "quest_a_boss"], LayerState::Live));
        assert!(t.any_in_state(&["quest_a", "night"], LayerState::Parked));
        assert!(!t.all_in_state(&[], LayerState::Live));
    }

    #[test]
    fn writes_by_a_side_that_does_not_own_the_layer_are_no_ops() {
        let mut t = table();
        assert_eq!(
            t.request("server", LayerState::Absent, false, Side::Client),
            Ok(false)
        );
        assert_eq!(t.requested("server"), Some(LayerState::Live));
        assert_eq!(
            t.request("server", LayerState::Absent, false, Side::Server),
            Ok(true)
        );
        assert_eq!(
            t.request("quest_a", LayerState::Absent, false, Side::Client),
            Ok(false)
        );
    }

    #[test]
    fn definitions_round_trip_and_validate() {
        let text = r#"LayerDefs(
  format_version: 1,
  layers: [
    (name: "night_props", kind: Runtime, debug_color: (0.1, 0.1, 0.5), parent: None),
    (name: "quest_a", kind: Runtime, debug_color: (0.8, 0.3, 0.1), parent: None),
    (name: "quest_a_boss", kind: Runtime, debug_color: (0.9, 0.1, 0.1), parent: Some("quest_a")),
    (name: "blockout", kind: EditorOnly, debug_color: (0.5, 0.5, 0.5), parent: None),
  ],
)"#;
        let defs = LayerDefs::from_ron_str(text).expect("parse");
        assert_eq!(defs.layers.len(), 4);
        assert!(validate_defs(&defs).is_empty());
        let back = LayerDefs::from_ron_str(&defs.to_ron_string().expect("ron")).expect("reparse");
        assert_eq!(back, defs);

        let mut bad = defs.clone();
        bad.layers[1].parent = Some("quest_a_boss".into());
        assert!(
            validate_defs(&bad)
                .iter()
                .any(|p| p.reason == "layer_parent_invalid" && p.fatal)
        );
        bad.layers[0].parent = Some("ghost".into());
        assert!(validate_defs(&bad).iter().any(|p| p.detail.contains("ghost")));
        let newer = text.replace("format_version: 1", "format_version: 2");
        assert!(matches!(
            LayerDefs::from_ron_str(&newer),
            Err(SceneFormatError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn state_names_parse_both_spellings() {
        for state in [LayerState::Absent, LayerState::Parked, LayerState::Live] {
            assert_eq!(LayerState::parse(state.as_str()), Some(state));
        }
        assert_eq!(LayerState::parse("activated"), Some(LayerState::Live));
        assert_eq!(LayerState::parse("x"), None);
    }

    fn chain_app(count: u32, root_layer: bool) -> regia_app::App {
        let defs_id = regia_core::id::Guid::from_u128(0xd0);
        let mut entities = String::new();
        // Leaf first. A parent-before-child file closes in one pass of the old fixpoint, because
        // the wanted set grows as the scan moves forward. Leaf first takes one full scan per rank.
        for index in (0..count).rev() {
            let guid = regia_core::id::Guid::from_u128(u128::from(index) + 1);
            let parent = if index == 0 {
                String::new()
            } else {
                format!(
                    "parent: Some(\"{}\"),",
                    regia_core::id::Guid::from_u128(u128::from(index))
                )
            };
            let membership = if root_layer && index == 0 {
                "\"regia_scene::layers::LayerMembership\": (layers: [\"hide\"]),"
            } else {
                ""
            };
            entities.push_str(&format!(
                "(guid: \"{guid}\", {parent} components: {{\
                    \"regia_scene::transform::Transform\": (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),\
                    {membership}\
                }}),"
            ));
        }
        let resources = if root_layer {
            format!(
                "resources: {{ \"regia_scene::layers::LevelLayers\": (\
                    defs: [\"asset:{defs_id}\"],\
                    entries: [(layer: \"hide\", initial: Live, editor_visible: true, editor_resident: true, peers: Everyone)],\
                ) }},"
            )
        } else {
            String::new()
        };
        let text = format!(
            "SceneFile(format_version: {}, entities: [{entities}], {resources})",
            crate::format::SCENE_FORMAT_VERSION
        );
        let mut app = regia_app::App::new();
        app.add_plugins(crate::ScenePlugin);
        if root_layer {
            let defs = LayerDefs::from_ron_str(
                r#"LayerDefs(format_version: 1, layers: [
                    (name: "hide", kind: Runtime, debug_color: (0.2, 0.2, 0.2), parent: None),
                ])"#,
            )
            .expect("defs");
            app.world_mut()
                .resource_mut::<LayerDefsLibrary>()
                .insert(defs_id, defs);
        }
        let id = crate::level::LevelId(regia_core::id::Guid::from_u128(0x1e));
        let level = crate::level::Level::from_ron_str(id, &text).expect("level");
        app.world_mut()
            .resource_mut::<crate::level::LevelLibrary>()
            .insert(level);
        crate::level::open_level(app.world_mut(), id, crate::level::LoadMode::Replace).expect("open");
        // Build the table against the open level before the measured apply.
        apply_layer_switches(app.world_mut());
        app
    }

    /// A leaf-first parent chain under an absent member comes back without a scan per rank.
    /// The absent set still holds the whole chain after the switch off.
    #[test]
    fn dirty_switch_scans_each_record_a_bounded_number_of_times() {
        const COUNT: u32 = 2_000;
        let mut app = chain_app(COUNT, true);
        let changed = app
            .world_mut()
            .resource_mut::<LayerTable>()
            .request("hide", LayerState::Absent, false, Side::Local)
            .expect("known layer");
        assert!(changed);
        apply_layer_switches(app.world_mut());
        assert_eq!(
            app.world().resource::<LayerTable>().despawned_count(),
            COUNT as usize,
            "the absent member takes its membership-less chain with it"
        );

        // Bringing the chain back is the walk that used to rescan the level once per rank.
        app.world_mut()
            .resource_mut::<LayerTable>()
            .request("hide", LayerState::Parked, false, Side::Local)
            .expect("known layer");
        reset_layer_scan_stats();
        apply_layer_switches(app.world_mut());
        let (parses, scans) = layer_scan_stats();
        let bound = u64::from(COUNT) * 4;
        assert!(
            scans <= bound,
            "record scans {scans} for {COUNT} entities (bound {bound}); a chain must not rescan per rank"
        );
        assert!(
            parses <= bound,
            "from_record calls {parses} for {COUNT} entities (bound {bound})"
        );
        assert_eq!(
            app.world().resource::<LayerTable>().despawned_count(),
            0,
            "parking the layer respawns the chain"
        );
    }

    /// A dirty apply over a level with no memberships reads each record a constant number of
    /// times. It does not call `from_record` once per entity per rank.
    #[test]
    fn dirty_switch_without_memberships_parses_each_record_once() {
        const COUNT: u32 = 500;
        let mut app = chain_app(COUNT, false);
        app.world_mut().resource_mut::<LayerTable>().mark_dirty();
        reset_layer_scan_stats();
        apply_layer_switches(app.world_mut());
        let (parses, scans) = layer_scan_stats();
        let bound = u64::from(COUNT) * 3;
        assert!(
            parses <= bound,
            "from_record calls {parses} for {COUNT} membership-less entities"
        );
        assert!(scans <= bound, "record scans {scans}");
        assert_eq!(app.world().resource::<LayerTable>().despawned_count(), 0);
    }
}
