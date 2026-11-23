// SPDX-License-Identifier: MPL-2.0
//! Level variants (`docs/formats/level-variants.md`, ADR-7823).
//!
//! A level variant is a level defined as *another level plus differences*: a `.levelvariant.ron`
//! file names its base (a level or another variant) by asset id and stores only what differs:
//! entities it adds, base entities it removes (with their descendants), field overrides by entity
//! guid, component type and field path (the prefab instance override model,
//! [`crate::format::PrefabOverride`]), components it adds or removes, record edits (an entity's
//! name, parent or prefab instance record) and replaced or removed resources.
//!
//! Resolving a variant ([`resolve_level_variant`]) walks its chain down to a plain level and
//! applies every step base-most first, so a change to the base shows up in every variant the next
//! time it resolves unless that variant overrides the same field. A chain that reaches an id it
//! already passed is refused ([`LevelVariantError::Loop`], reason `variant_loop`); so is one
//! longer than [`MAX_LEVEL_VARIANT_CHAIN`] (`chain_too_long`).
//!
//! The editor computes a variant from the edited level with [`diff_levels`]; overrides are applied
//! to raw RON text ([`ron_path`]), so resolving needs no type registry and works in the cook.

pub mod ron_path;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use regia_core::id::Guid;
use serde::{Deserialize, Serialize};

use crate::format::{
    PrefabComponentAdd, PrefabComponentRemove, PrefabOverride, SceneAssetRef, SceneEntity, SceneFile,
};

/// The `.levelvariant.ron` format version this build reads and writes.
pub const LEVEL_VARIANT_FORMAT_VERSION: u32 = 1;

/// File name suffix of a level variant (`arena_night.levelvariant.ron`).
pub const LEVEL_VARIANT_SUFFIX: &str = ".levelvariant.ron";

/// The `.meta` kind of a level variant file.
pub const LEVEL_VARIANT_META_KIND: &str = "level_variant_ron";

/// Longest chain (variant of a variant of …) resolution follows.
pub const MAX_LEVEL_VARIANT_CHAIN: usize = 16;

/// The entity record fields a [`LevelRecordEdit`] may set.
pub const RECORD_FIELDS: [&str; 3] = ["name", "parent", "prefab_instance"];

/// Whether `path` names a level variant file.
#[must_use]
pub fn is_level_variant_path(path: &std::path::Path) -> bool {
    path.to_string_lossy()
        .to_ascii_lowercase()
        .ends_with(LEVEL_VARIANT_SUFFIX)
}

/// Errors from reading, validating or resolving a level variant.
#[derive(Debug, thiserror::Error)]
pub enum LevelVariantError {
    /// The text is not valid RON for the format.
    #[error("RON: {0}")]
    Ron(#[from] ron::error::SpannedError),
    /// Serializing failed.
    #[error("RON: {0}")]
    RonWrite(#[from] ron::Error),
    /// The file was written by a newer engine.
    #[error(
        "level variant format_version {found} is newer than this engine's {LEVEL_VARIANT_FORMAT_VERSION}; upgrade the engine"
    )]
    Newer {
        /// The version in the file.
        found: u32,
    },
    /// The structure is invalid (nil base, nil targets, duplicate added guids, an unknown record
    /// field).
    #[error("invalid level variant: {0}")]
    Invalid(String),
    /// A level of the chain could not be found or read.
    #[error("level {0} of the variant chain is missing: {1}")]
    BaseMissing(Guid, String),
    /// The chain reaches an id it already passed.
    #[error("level variant loop: {}", chain_text(.0))]
    Loop(Vec<Guid>),
    /// The chain is longer than [`MAX_LEVEL_VARIANT_CHAIN`].
    #[error("level variant chain is longer than {MAX_LEVEL_VARIANT_CHAIN}")]
    ChainTooLong,
    /// The resolved level is not a valid scene file.
    #[error("resolved level is invalid: {0}")]
    Resolved(String),
}

fn chain_text(chain: &[Guid]) -> String {
    chain.iter().map(Guid::to_string).collect::<Vec<_>>().join(" -> ")
}

impl LevelVariantError {
    /// The stable lowercase reason (agent `data.reason`, listed in `book/agents/level-variants.md`).
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Ron(_) | Self::RonWrite(_) | Self::Invalid(_) => "bad_variant",
            Self::Newer { .. } => "newer_version",
            Self::BaseMissing(..) => "base_missing",
            Self::Loop(_) => "variant_loop",
            Self::ChainTooLong => "chain_too_long",
            Self::Resolved(_) => "resolved_invalid",
        }
    }
}

/// One edit of a base entity's record: `field` is `name`, `parent` or `prefab_instance`, `value`
/// the RON of the new `Option` (`None`, `Some("Gate")`, …).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LevelRecordEdit {
    /// The base entity's guid.
    pub target: Guid,
    /// The record field (one of [`RECORD_FIELDS`]).
    pub field: String,
    /// The new value as raw RON.
    pub value: Box<ron::value::RawValue>,
}

/// The document of a `.levelvariant.ron` file: the base level and the differences. Example:
///
/// ```ron
/// LevelVariant(
///     format_version: 1,
///     base: "asset:8b6f2a11-0000-4000-8000-000000000001",
///     overrides: [(target: "…", component: "regia_scene::transform::Transform", path: ".translation", value: (5.0, 0.0, 0.0))],
///     removed_entities: ["…"],
///     added_entities: [(guid: "…", name: "Crate", components: {})],
/// )
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename = "LevelVariant")]
pub struct LevelVariantFile {
    /// Format version; `0` (missing, hand-written) loads as 1, newer is refused.
    #[serde(default)]
    pub format_version: u32,
    /// The base level or variant, by asset id.
    pub base: SceneAssetRef,
    /// Field overrides on base entities (`path` `""` replaces the whole component).
    #[serde(default)]
    pub overrides: Vec<PrefabOverride>,
    /// Components added to base entities.
    #[serde(default)]
    pub added_components: Vec<PrefabComponentAdd>,
    /// Components removed from base entities.
    #[serde(default)]
    pub removed_components: Vec<PrefabComponentRemove>,
    /// Record edits (name, parent, prefab instance) on base entities.
    #[serde(default)]
    pub record_edits: Vec<LevelRecordEdit>,
    /// Entities the variant adds (whole records; a parent may be a base entity).
    #[serde(default)]
    pub added_entities: Vec<SceneEntity>,
    /// Base entities removed, each with its descendants.
    #[serde(default)]
    pub removed_entities: Vec<Guid>,
    /// Resources the variant sets (whole values by type path).
    #[serde(default)]
    pub resources: BTreeMap<String, Box<ron::value::RawValue>>,
    /// Resources of the base the variant drops.
    #[serde(default)]
    pub removed_resources: Vec<String>,
}

impl LevelVariantFile {
    /// A variant of `base` with no differences yet.
    #[must_use]
    pub fn new(base: Guid) -> Self {
        Self {
            format_version: LEVEL_VARIANT_FORMAT_VERSION,
            base: SceneAssetRef(base),
            overrides: Vec::new(),
            added_components: Vec::new(),
            removed_components: Vec::new(),
            record_edits: Vec::new(),
            added_entities: Vec::new(),
            removed_entities: Vec::new(),
            resources: BTreeMap::new(),
            removed_resources: Vec::new(),
        }
    }

    /// The base level's asset id.
    #[must_use]
    pub fn base_guid(&self) -> Guid {
        self.base.guid()
    }

    /// Whether the variant changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.overrides.is_empty()
            && self.added_components.is_empty()
            && self.removed_components.is_empty()
            && self.record_edits.is_empty()
            && self.added_entities.is_empty()
            && self.removed_entities.is_empty()
            && self.resources.is_empty()
            && self.removed_resources.is_empty()
    }

    /// Parses a `.levelvariant.ron`, migrating older versions and refusing newer ones (the version
    /// is peeked first, so a newer file is refused even when its body no longer parses).
    ///
    /// # Errors
    ///
    /// Bad RON, a newer version, or a structural problem ([`Self::validate`]).
    pub fn from_ron_str(text: &str) -> Result<Self, LevelVariantError> {
        if let Ok(found) = regia_core::version::peek_ron_version(text)
            && found > LEVEL_VARIANT_FORMAT_VERSION
        {
            return Err(LevelVariantError::Newer { found });
        }
        let mut file: Self = ron::from_str(text)?;
        file.trim();
        file.migrate()?;
        file.validate()?;
        Ok(file)
    }

    /// The canonical RON text (starts with `LevelVariant(`).
    ///
    /// # Errors
    ///
    /// Serialization failed.
    pub fn to_ron_string(&self) -> Result<String, LevelVariantError> {
        let mut text = ron::ser::to_string_pretty(self, regia_ecs::serialize::canonical_pretty_config())?;
        text.push('\n');
        if let Some(rest) = text.strip_prefix('(') {
            text = format!("LevelVariant({rest}");
        }
        Ok(text)
    }

    /// Brings data written by an older engine up to [`LEVEL_VARIANT_FORMAT_VERSION`].
    ///
    /// # Errors
    ///
    /// A version newer than this build's.
    pub fn migrate(&mut self) -> Result<(), LevelVariantError> {
        match self.format_version {
            0 => {
                self.format_version = 1;
                Ok(())
            }
            LEVEL_VARIANT_FORMAT_VERSION => Ok(()),
            found => Err(LevelVariantError::Newer { found }),
        }
    }

    /// Structural checks: a non-nil base, non-nil targets, known record fields, and non-nil unique
    /// guids on the added entities. Whether targets exist in the base is checked when the variant
    /// resolves (a missing target is a warning, like a prefab instance's).
    ///
    /// # Errors
    ///
    /// [`LevelVariantError::Invalid`] naming the first problem.
    pub fn validate(&self) -> Result<(), LevelVariantError> {
        let invalid = |m: String| Err(LevelVariantError::Invalid(m));
        if self.base_guid().is_nil() {
            return invalid("the base level id is nil".into());
        }
        let targets = self
            .overrides
            .iter()
            .map(|o| o.target)
            .chain(self.added_components.iter().map(|a| a.target))
            .chain(self.removed_components.iter().map(|r| r.target))
            .chain(self.record_edits.iter().map(|r| r.target))
            .chain(self.removed_entities.iter().copied());
        for target in targets {
            if target.is_nil() {
                return invalid("an edit targets the nil guid".into());
            }
        }
        for edit in &self.record_edits {
            if !RECORD_FIELDS.contains(&edit.field.as_str()) {
                return invalid(format!(
                    "record field `{}` is not one of name, parent, prefab_instance",
                    edit.field
                ));
            }
        }
        for o in &self.overrides {
            ron_path::parse_path(&o.path).map_err(LevelVariantError::Invalid)?;
        }
        let mut guids = BTreeSet::new();
        for entity in &self.added_entities {
            if entity.guid.is_nil() {
                return invalid("an added entity has the nil guid".into());
            }
            if !guids.insert(entity.guid) {
                return invalid(format!("added entity {} appears twice", entity.guid));
            }
        }
        Ok(())
    }

    /// Trims captured raw values (their whitespace depends on where they sat in the file), as the
    /// scene loader does, so a parse then save is stable.
    fn trim(&mut self) {
        for o in &mut self.overrides {
            o.value = o.value.clone().trim_boxed();
        }
        for a in &mut self.added_components {
            a.value = a.value.clone().trim_boxed();
        }
        for r in &mut self.record_edits {
            r.value = r.value.clone().trim_boxed();
        }
        for value in self.resources.values_mut() {
            *value = value.clone().trim_boxed();
        }
        for entity in &mut self.added_entities {
            trim_entity(entity);
        }
    }

    /// Applies this step's differences to `scene` (the resolved base), pushing a warning for each
    /// edit whose target is gone. Order: record edits, removals (each with its descendants as
    /// they are after the record edits), component removals and additions, field overrides,
    /// added entities, resources.
    pub fn apply(&self, scene: &mut SceneFile, warnings: &mut Vec<String>) {
        let index = |scene: &SceneFile| -> HashMap<Guid, usize> {
            scene
                .entities
                .iter()
                .enumerate()
                .map(|(i, e)| (e.guid, i))
                .collect()
        };
        let mut at = index(scene);
        for edit in &self.record_edits {
            let Some(&i) = at.get(&edit.target) else {
                warnings.push(format!("record edit: entity {} is not in the base", edit.target));
                continue;
            };
            if let Err(err) = apply_record_edit(&mut scene.entities[i], edit) {
                warnings.push(format!("record edit on {}: {err}", edit.target));
            }
        }
        if !self.removed_entities.is_empty() {
            let mut doomed: BTreeSet<Guid> = BTreeSet::new();
            for guid in &self.removed_entities {
                if at.contains_key(guid) {
                    doomed.insert(*guid);
                } else {
                    warnings.push(format!("removed entity {guid} is not in the base"));
                }
            }
            // Descendants (as parented now): repeat until no new child joins.
            loop {
                let before = doomed.len();
                for entity in &scene.entities {
                    if entity.parent.is_some_and(|p| doomed.contains(&p)) {
                        doomed.insert(entity.guid);
                    }
                }
                if doomed.len() == before {
                    break;
                }
            }
            scene.entities.retain(|e| !doomed.contains(&e.guid));
            at = index(scene);
        }
        for remove in &self.removed_components {
            let removed = at
                .get(&remove.target)
                .and_then(|&i| scene.entities[i].components.remove(&remove.component));
            if removed.is_none() {
                warnings.push(format!(
                    "removed component {} is not on entity {}",
                    remove.component, remove.target
                ));
            }
        }
        for add in &self.added_components {
            match at.get(&add.target) {
                Some(&i) => {
                    scene.entities[i]
                        .components
                        .insert(add.component.clone(), add.value.clone());
                }
                None => warnings.push(format!(
                    "added component: entity {} is not in the base",
                    add.target
                )),
            }
        }
        for o in &self.overrides {
            let Some(&i) = at.get(&o.target) else {
                warnings.push(format!("override: entity {} is not in the base", o.target));
                continue;
            };
            let Some(current) = scene.entities[i].components.get(&o.component) else {
                warnings.push(format!("override: entity {} has no {}", o.target, o.component));
                continue;
            };
            let patched = ron_path::set(current.get_ron(), &o.path, o.value.get_ron()).and_then(|text| {
                ron::value::RawValue::from_boxed_ron(text.into_boxed_str()).map_err(|e| e.to_string())
            });
            match patched {
                Ok(value) => {
                    scene.entities[i].components.insert(o.component.clone(), value);
                }
                Err(err) => warnings.push(format!(
                    "override {}{} on {}: {err}",
                    o.component, o.path, o.target
                )),
            }
        }
        for added in &self.added_entities {
            match at.get(&added.guid) {
                Some(&i) => {
                    warnings.push(format!(
                        "added entity {} is also in the base now; the variant's record wins",
                        added.guid
                    ));
                    scene.entities[i] = added.clone();
                }
                None => {
                    at.insert(added.guid, scene.entities.len());
                    scene.entities.push(added.clone());
                }
            }
        }
        for key in &self.removed_resources {
            if scene.resources.remove(key).is_none() {
                warnings.push(format!("removed resource {key} is not in the base"));
            }
        }
        for (key, value) in &self.resources {
            scene.resources.insert(key.clone(), value.clone());
        }
    }

    /// Drops every difference about `entity`: its overrides, component and record edits, its
    /// removal, or (for an entity the variant added) the entity itself with the added entities
    /// parented under it. Returns how many entries went.
    pub fn revert_entity(&mut self, entity: Guid) -> usize {
        let before = self.len();
        self.overrides.retain(|o| o.target != entity);
        self.added_components.retain(|a| a.target != entity);
        self.removed_components.retain(|r| r.target != entity);
        self.record_edits.retain(|r| r.target != entity);
        self.removed_entities.retain(|g| *g != entity);
        let mut gone: BTreeSet<Guid> = BTreeSet::from([entity]);
        loop {
            let n = gone.len();
            for added in &self.added_entities {
                if gone.contains(&added.guid) || added.parent.is_some_and(|p| gone.contains(&p)) {
                    gone.insert(added.guid);
                }
            }
            if gone.len() == n {
                break;
            }
        }
        self.added_entities.retain(|a| !gone.contains(&a.guid));
        before - self.len()
    }

    /// Drops the overrides of `component` on `entity` at `path` and below it (`.a` also drops
    /// `.a.b`); the empty path drops every override of the component and its addition or
    /// removal. A `component` of `#name`, `#parent` or `#prefab_instance` reverts that record
    /// field. Returns how many entries went.
    pub fn revert_field(&mut self, entity: Guid, component: &str, path: &str) -> usize {
        let before = self.len();
        if let Some(field) = component.strip_prefix('#') {
            self.record_edits
                .retain(|r| !(r.target == entity && r.field == field));
            return before - self.len();
        }
        let path = if path == "." { "" } else { path };
        self.overrides.retain(|o| {
            let o_path = if o.path == "." { "" } else { o.path.as_str() };
            let under = path.is_empty()
                || o_path == path
                || o_path
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with(['.', '[']));
            !(o.target == entity && o.component == component && under)
        });
        if path.is_empty() {
            self.added_components
                .retain(|a| !(a.target == entity && a.component == component));
            self.removed_components
                .retain(|r| !(r.target == entity && r.component == component));
        }
        before - self.len()
    }

    /// How many difference entries the variant holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.overrides.len()
            + self.added_components.len()
            + self.removed_components.len()
            + self.record_edits.len()
            + self.added_entities.len()
            + self.removed_entities.len()
            + self.resources.len()
            + self.removed_resources.len()
    }

    /// How `entity` relates to the base.
    #[must_use]
    pub fn mark(&self, entity: Guid) -> EntityMark {
        if self.added_entities.iter().any(|a| a.guid == entity) {
            return EntityMark::Added;
        }
        let edited = self.overrides.iter().any(|o| o.target == entity)
            || self.added_components.iter().any(|a| a.target == entity)
            || self.removed_components.iter().any(|r| r.target == entity)
            || self.record_edits.iter().any(|r| r.target == entity);
        if edited {
            EntityMark::Overridden
        } else {
            EntityMark::Base
        }
    }

    /// The overridden `(component, path)` pairs of `entity`, sorted (a component added by the
    /// variant is listed with the empty path).
    #[must_use]
    pub fn overridden_fields(&self, entity: Guid) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .overrides
            .iter()
            .filter(|o| o.target == entity)
            .map(|o| (o.component.clone(), o.path.clone()))
            .chain(
                self.added_components
                    .iter()
                    .filter(|a| a.target == entity)
                    .map(|a| (a.component.clone(), String::new())),
            )
            .chain(
                self.record_edits
                    .iter()
                    .filter(|r| r.target == entity)
                    .map(|r| (format!("#{}", r.field), String::new())),
            )
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// How an entity of an open variant relates to its base (the Outliner and Inspector marks).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityMark {
    /// The entity comes from the base unchanged.
    Base,
    /// The entity comes from the base with differences.
    Overridden,
    /// The variant added the entity.
    Added,
}

impl EntityMark {
    /// The lowercase name the agent reports.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Overridden => "overridden",
            Self::Added => "added",
        }
    }
}

fn trim_entity(entity: &mut SceneEntity) {
    for value in entity.components.values_mut() {
        *value = value.clone().trim_boxed();
    }
    if let Some(instance) = &mut entity.prefab_instance {
        for o in &mut instance.overrides {
            o.value = o.value.clone().trim_boxed();
        }
        for a in &mut instance.added_components {
            a.value = a.value.clone().trim_boxed();
        }
        for child in &mut instance.added_children {
            trim_entity(child);
        }
    }
}

fn apply_record_edit(entity: &mut SceneEntity, edit: &LevelRecordEdit) -> Result<(), String> {
    let text = edit.value.get_ron();
    match edit.field.as_str() {
        "name" => entity.name = ron::from_str(text).map_err(|e| e.to_string())?,
        "parent" => entity.parent = ron::from_str(text).map_err(|e| e.to_string())?,
        "prefab_instance" => entity.prefab_instance = ron::from_str(text).map_err(|e| e.to_string())?,
        other => return Err(format!("unknown record field `{other}`")),
    }
    Ok(())
}

fn record_edit(target: Guid, field: &str, value: &impl Serialize) -> Option<LevelRecordEdit> {
    let text = ron::to_string(value).ok()?;
    let value = ron::value::RawValue::from_boxed_ron(text.into_boxed_str()).ok()?;
    Some(LevelRecordEdit {
        target,
        field: field.to_owned(),
        value,
    })
}

fn raw(text: &str) -> Option<Box<ron::value::RawValue>> {
    ron::value::RawValue::from_boxed_ron(text.to_owned().into_boxed_str()).ok()
}

/// The variant of `base_id` that turns `base` (the resolved base level) into `edited`. Entities
/// are matched by guid; component values are compared field by field ([`ron_path::diff`]), so a
/// field the variant does not change keeps following the base. Output lists are in a stable order
/// (removed guids sorted, added entities in `edited`'s order, overrides by entity, component,
/// path), so the same edit always writes the same bytes.
#[must_use]
pub fn diff_levels(base_id: Guid, base: &SceneFile, edited: &SceneFile) -> LevelVariantFile {
    let mut out = LevelVariantFile::new(base_id);
    let base_at: HashMap<Guid, &SceneEntity> = base.entities.iter().map(|e| (e.guid, e)).collect();
    let edited_guids: BTreeSet<Guid> = edited.entities.iter().map(|e| e.guid).collect();
    out.removed_entities = base
        .entities
        .iter()
        .map(|e| e.guid)
        .filter(|g| !edited_guids.contains(g))
        .collect();
    out.removed_entities.sort();
    let mut edited_sorted: Vec<&SceneEntity> = edited.entities.iter().collect();
    edited_sorted.sort_by_key(|e| e.guid);
    for entity in edited_sorted {
        let Some(old) = base_at.get(&entity.guid) else {
            continue;
        };
        let g = entity.guid;
        if old.name != entity.name {
            out.record_edits.extend(record_edit(g, "name", &entity.name));
        }
        if old.parent != entity.parent {
            out.record_edits.extend(record_edit(g, "parent", &entity.parent));
        }
        if old.prefab_instance != entity.prefab_instance {
            let same = match (&old.prefab_instance, &entity.prefab_instance) {
                (Some(a), Some(b)) => match (ron::to_string(a), ron::to_string(b)) {
                    (Ok(a), Ok(b)) => ron_path::same_value(&a, &b),
                    _ => false,
                },
                _ => false,
            };
            if !same {
                out.record_edits
                    .extend(record_edit(g, "prefab_instance", &entity.prefab_instance));
            }
        }
        for component in old.components.keys() {
            if !entity.components.contains_key(component) {
                out.removed_components.push(PrefabComponentRemove {
                    target: g,
                    component: component.clone(),
                });
            }
        }
        for (component, value) in &entity.components {
            match old.components.get(component) {
                None => out.added_components.push(PrefabComponentAdd {
                    target: g,
                    component: component.clone(),
                    value: value.clone(),
                }),
                Some(before) => {
                    for (path, text) in ron_path::diff(before.get_ron(), value.get_ron(), "") {
                        if let Some(value) = raw(&text) {
                            out.overrides.push(PrefabOverride {
                                target: g,
                                component: component.clone(),
                                path,
                                value,
                            });
                        }
                    }
                }
            }
        }
    }
    out.added_entities = edited
        .entities
        .iter()
        .filter(|e| !base_at.contains_key(&e.guid))
        .cloned()
        .collect();
    for (key, value) in &edited.resources {
        let same = base
            .resources
            .get(key)
            .is_some_and(|before| ron_path::same_value(before.get_ron(), value.get_ron()));
        if !same {
            out.resources.insert(key.clone(), value.clone());
        }
    }
    out.removed_resources = base
        .resources
        .keys()
        .filter(|k| !edited.resources.contains_key(*k))
        .cloned()
        .collect();
    out
}

/// What a variant chain resolves through: a plain level or another variant.
#[derive(Clone, Debug)]
pub enum LevelSource {
    /// A plain level (`.level.ron` or `.level.d`), already parsed.
    Level(SceneFile),
    /// A level variant.
    Variant(LevelVariantFile),
}

/// A resolved variant: the level it stands for and how it got there.
#[derive(Clone, Debug)]
pub struct ResolvedLevel {
    /// The level, a valid scene file.
    pub scene: SceneFile,
    /// The ids walked, the variant first and the plain level last.
    pub chain: Vec<Guid>,
    /// Edits whose target is gone from the base (they are skipped).
    pub warnings: Vec<String>,
}

/// Resolves the level source `id` (a level or a variant) into a level. `load` reads a base by id.
///
/// # Errors
///
/// [`LevelVariantError::Loop`] (`variant_loop`) when the chain reaches an id it already passed
/// (a variant naming itself included), [`LevelVariantError::ChainTooLong`], what `load` returns
/// for a missing base, and [`LevelVariantError::Resolved`] when the result is not a valid scene
/// (an added entity parented to a removed one, …).
pub fn resolve_level_variant(
    id: Guid,
    source: LevelSource,
    load: &mut dyn FnMut(Guid) -> Result<LevelSource, LevelVariantError>,
) -> Result<ResolvedLevel, LevelVariantError> {
    let _span = tracing::info_span!("level_variant_resolve", %id).entered();
    let mut chain = vec![id];
    let mut steps: Vec<LevelVariantFile> = Vec::new();
    let mut source = source;
    let mut scene = loop {
        match source {
            LevelSource::Level(scene) => break scene,
            LevelSource::Variant(file) => {
                let base = file.base_guid();
                steps.push(file);
                if chain.contains(&base) {
                    chain.push(base);
                    return Err(LevelVariantError::Loop(chain));
                }
                chain.push(base);
                if chain.len() > MAX_LEVEL_VARIANT_CHAIN + 1 {
                    return Err(LevelVariantError::ChainTooLong);
                }
                source = load(base)?;
            }
        }
    };
    let mut warnings = Vec::new();
    for step in steps.iter().rev() {
        step.apply(&mut scene, &mut warnings);
    }
    scene
        .validate()
        .map_err(|e| LevelVariantError::Resolved(e.to_string()))?;
    Ok(ResolvedLevel {
        scene,
        chain,
        warnings,
    })
}

/// The ordered serde field lists of the `.levelvariant.ron` document (`cargo xtask
/// check-formats`, `level-variant`). The edit record types are the scene format's, so a change
/// there trips this fingerprint too.
#[must_use]
pub fn level_variant_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<LevelVariantFile>(),
        regia_core::schema::serde_field_names::<LevelRecordEdit>(),
        regia_core::schema::serde_field_names::<PrefabOverride>(),
        regia_core::schema::serde_field_names::<PrefabComponentAdd>(),
        regia_core::schema::serde_field_names::<PrefabComponentRemove>(),
        regia_core::schema::serde_field_names::<SceneEntity>(),
    ])
}

/// The `from` versions of the registered migration steps (`0 → 1`: a file without
/// `format_version`).
#[must_use]
pub fn level_variant_migration_sources() -> Vec<u32> {
    vec![0]
}

/// Reads a level variant source text: a `LevelVariant(…)` document, or a level the cook already
/// resolved (a packaged game ships a variant flattened under its own name: the loader reads either).
///
/// # Errors
///
/// The text is neither.
pub fn read_source_text(text: &str) -> Result<LevelSource, LevelVariantError> {
    if is_flattened(text) {
        return SceneFile::from_ron_str(text)
            .map(LevelSource::Level)
            .map_err(|e| LevelVariantError::Invalid(e.to_string()));
    }
    LevelVariantFile::from_ron_str(text).map(LevelSource::Variant)
}

/// Whether a `.levelvariant.ron` text is a cooked (flattened) level rather than a variant.
fn is_flattened(text: &str) -> bool {
    let mut rest = text.trim_start();
    // Skip leading comments and `#![enable(…)]` attributes.
    loop {
        if let Some(after) = rest.strip_prefix("//") {
            rest = after.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if rest.starts_with("#!") {
            rest = rest.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    rest.starts_with("SceneFile")
}

/// Reads the level source of `id` at `path` for resolution: a variant file (raw or flattened by
/// the cook) through [`read_source_text`], anything else as a level in either form
/// ([`crate::level_dir::read_level_as_stored`]).
///
/// # Errors
///
/// [`LevelVariantError::BaseMissing`] naming `id` when the file cannot be read or parsed as a
/// level; a variant file's own parse error otherwise.
pub fn read_source_file(id: Guid, path: &std::path::Path) -> Result<LevelSource, LevelVariantError> {
    if is_level_variant_path(path) {
        let text = std::fs::read_to_string(path)
            .map_err(|e| LevelVariantError::BaseMissing(id, format!("{}: {e}", path.display())))?;
        return read_source_text(&text);
    }
    crate::level_dir::read_level_as_stored(path)
        .map(LevelSource::Level)
        .map_err(|e| LevelVariantError::BaseMissing(id, format!("{}: {e}", path.display())))
}

/// Reads and resolves the level variant `id` at `path`; `find` maps a base id to its source path
/// (a level in either form or another variant).
///
/// # Errors
///
/// What [`read_source_file`] and [`resolve_level_variant`] return; a base `find` does not know
/// is [`LevelVariantError::BaseMissing`].
pub fn resolve_variant_file(
    id: Guid,
    path: &std::path::Path,
    find: &dyn Fn(Guid) -> Option<std::path::PathBuf>,
) -> Result<ResolvedLevel, LevelVariantError> {
    let source = read_source_file(id, path)?;
    let mut load = |base: Guid| match find(base) {
        Some(file) => read_source_file(base, &file),
        None => Err(LevelVariantError::BaseMissing(
            base,
            "no level source has this id".into(),
        )),
    };
    let resolved = resolve_level_variant(id, source, &mut load)?;
    for warning in &resolved.warnings {
        tracing::warn!(target: "regia_scene", path = %path.display(), "level variant: {warning}");
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests;
