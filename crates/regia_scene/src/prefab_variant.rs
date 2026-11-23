// SPDX-License-Identifier: MPL-2.0
//! Prefab variants (`docs/specs/83-prefab-profiler-volumes.md` § 2, ADR-2323).
//!
//! A variant is a prefab defined as *another prefab plus edits*: a `.variant.ron` file names its
//! base (a prefab or another variant) and carries the same edit lists an instance record does
//! (field overrides, added and removed components, added and removed children). Instantiating a
//! variant spawns its base template and applies every step of the chain, base-most first, before
//! the instance's own record, so:
//!
//! - a change to the base shows up in every variant the next time it is instantiated (variants
//!   never copy the base's values);
//! - the base's local ids stay valid, so an override written against the base also works on an
//!   instance of the variant;
//! - the instance wins over the variant, which wins over its own base.
//!
//! Variants are registered in the [`PrefabLibrary`] beside templates
//! ([`PrefabLibrary::insert_variant`]); an id names one or the other, never both.

use std::sync::Arc;

use regia_core::id::Guid;
use serde::{Deserialize, Serialize};

use crate::format::{PrefabInstanceData, SceneAssetRef, SceneEntity};
use crate::prefab::{PrefabLibrary, PrefabTemplate};
use crate::spawn::SceneSpawnError;

/// The `.variant.ron` format version this build reads and writes.
pub const PREFAB_VARIANT_FORMAT_VERSION: u32 = 1;

/// File name suffix of a prefab variant (`rock_mossy.variant.ron`).
pub const PREFAB_VARIANT_SUFFIX: &str = ".variant.ron";

/// Longest variant chain (variant of a variant of …) instantiation follows; a longer chain is
/// reported as a cycle.
pub const MAX_VARIANT_CHAIN: usize = 16;

/// Errors from reading or validating a [`PrefabVariantFile`].
#[derive(Debug, thiserror::Error)]
pub enum PrefabVariantError {
    /// The text is not valid RON for the format.
    #[error("RON: {0}")]
    Ron(#[from] ron::error::SpannedError),
    /// Serializing failed.
    #[error("RON: {0}")]
    RonWrite(#[from] ron::Error),
    /// The file was written by a newer engine.
    #[error(
        "prefab variant format_version {found} is newer than this engine's {PREFAB_VARIANT_FORMAT_VERSION}; upgrade the engine"
    )]
    Newer {
        /// The version in the file.
        found: u32,
    },
    /// The structure is invalid (nil base, nil targets, duplicate added-child guids).
    #[error("invalid prefab variant: {0}")]
    Invalid(String),
}

/// The document of a `.variant.ron` file: the base prefab and the edits that make the variant.
///
/// `base` is an ordinary prefab instance record: `base.prefab` names the base (a prefab or a
/// variant) and its edit lists are the variant's. Example:
///
/// ```ron
/// PrefabVariant(
///     format_version: 1,
///     base: (
///         prefab: "asset:8b6f2a11-0000-4000-8000-000000000001",
///         overrides: [(target: "…", component: "…::PointLight", path: ".intensity", value: 800.0)],
///         removed_children: ["…"],
///     ),
/// )
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename = "PrefabVariant")]
pub struct PrefabVariantFile {
    /// Format version; `0` (missing, hand-written) loads as 1, newer is refused.
    #[serde(default)]
    pub format_version: u32,
    /// The base prefab and the variant's edits on it.
    pub base: PrefabInstanceData,
}

impl PrefabVariantFile {
    /// A variant of `base` with no edits yet.
    pub fn new(base: Guid) -> Self {
        Self {
            format_version: PREFAB_VARIANT_FORMAT_VERSION,
            base: PrefabInstanceData::plain(SceneAssetRef(base)),
        }
    }

    /// A variant whose edits are an instance record's (the editor's "save instance as variant").
    pub fn from_instance(instance: &PrefabInstanceData) -> Self {
        Self {
            format_version: PREFAB_VARIANT_FORMAT_VERSION,
            base: instance.clone(),
        }
    }

    /// The base prefab's asset id.
    pub fn base_guid(&self) -> Guid {
        self.base.prefab.guid()
    }

    /// Parses a `.variant.ron`, migrating older versions and refusing newer ones (the version
    /// is peeked first, so a newer file is refused even when its body no longer parses).
    pub fn from_ron_str(text: &str) -> Result<Self, PrefabVariantError> {
        if let Ok(found) = regia_core::version::peek_ron_version(text)
            && found > PREFAB_VARIANT_FORMAT_VERSION
        {
            return Err(PrefabVariantError::Newer { found });
        }
        let mut file: Self = ron::from_str(text)?;
        trim_record(&mut file.base);
        file.migrate()?;
        file.validate()?;
        Ok(file)
    }

    /// The canonical RON text (starts with `PrefabVariant(`).
    pub fn to_ron_string(&self) -> Result<String, PrefabVariantError> {
        let mut text = ron::ser::to_string_pretty(self, regia_ecs::serialize::canonical_pretty_config())?;
        text.push('\n');
        if let Some(rest) = text.strip_prefix('(') {
            text = format!("PrefabVariant({rest}");
        }
        Ok(text)
    }

    /// Brings data written by an older engine up to [`PREFAB_VARIANT_FORMAT_VERSION`].
    pub fn migrate(&mut self) -> Result<(), PrefabVariantError> {
        match self.format_version {
            // Hand-written without a version: the v1 shape.
            0 => {
                self.format_version = 1;
                Ok(())
            }
            PREFAB_VARIANT_FORMAT_VERSION => Ok(()),
            found => Err(PrefabVariantError::Newer { found }),
        }
    }

    /// Structural checks: a non-nil base, non-nil edit targets, and non-nil unique guids on the
    /// added children (recursively). Whether targets exist in the base is checked when the
    /// variant is instantiated (a missing target is a warning, like an instance's).
    pub fn validate(&self) -> Result<(), PrefabVariantError> {
        if self.base_guid().is_nil() {
            return Err(PrefabVariantError::Invalid("the base prefab id is nil".into()));
        }
        crate::format::validate_instance_targets(&self.base, self.base_guid())
            .map_err(|e| PrefabVariantError::Invalid(e.to_string()))?;
        let mut guids = std::collections::BTreeSet::new();
        crate::format::validate_added_children(&self.base.added_children, &mut guids, 0)
            .map_err(|e| PrefabVariantError::Invalid(e.to_string()))?;
        Ok(())
    }
}

/// Trims captured raw values (their whitespace depends on where they sat in the file), as the
/// scene loader does, so a parse then save is stable.
fn trim_record(record: &mut PrefabInstanceData) {
    for entry in &mut record.overrides {
        entry.value = entry.value.clone().trim_boxed();
    }
    for add in &mut record.added_components {
        add.value = add.value.clone().trim_boxed();
    }
    for child in &mut record.added_children {
        trim_entity(child);
    }
}

fn trim_entity(entity: &mut SceneEntity) {
    for value in entity.components.values_mut() {
        *value = value.clone().trim_boxed();
    }
    if let Some(instance) = &mut entity.prefab_instance {
        trim_record(instance);
    }
}

/// The ordered serde field lists of the `.variant.ron` document (`cargo xtask check-formats`,
/// `prefab-variant`). The edit record types are the scene format's, so a change there trips this
/// fingerprint too.
pub fn prefab_variant_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<PrefabVariantFile>(),
        regia_core::schema::serde_field_names::<PrefabInstanceData>(),
        regia_core::schema::serde_field_names::<crate::format::PrefabOverride>(),
        regia_core::schema::serde_field_names::<crate::format::PrefabComponentAdd>(),
        regia_core::schema::serde_field_names::<crate::format::PrefabComponentRemove>(),
        regia_core::schema::serde_field_names::<SceneEntity>(),
    ])
}

/// The `from` versions of the registered migration steps (`0 → 1`: a file without
/// `format_version`).
pub fn prefab_variant_migration_sources() -> Vec<u32> {
    vec![0]
}

/// A registered variant: its id, its base and its edits.
#[derive(Clone, Debug, PartialEq)]
pub struct PrefabVariant {
    /// The variant's asset id (the `.variant.ron` file's `.meta` id).
    pub asset: Guid,
    /// The base it edits (a template or another variant).
    pub base: Guid,
    /// The edits, in instance-record form (`edits.prefab` is the base).
    pub edits: PrefabInstanceData,
}

impl PrefabVariant {
    /// The variant `asset` from its file.
    ///
    /// # Errors
    ///
    /// [`SceneSpawnError::Invalid`] when the variant names itself as its base.
    pub fn from_file(asset: Guid, file: &PrefabVariantFile) -> Result<Self, SceneSpawnError> {
        let base = file.base_guid();
        if base == asset {
            return Err(SceneSpawnError::Invalid(format!(
                "prefab variant {asset} names itself as its base"
            )));
        }
        Ok(Self {
            asset,
            base,
            edits: file.base.clone(),
        })
    }
}

/// What instantiating an id needs: the base template at the bottom of the chain and every variant
/// step above it, base-most first (empty for a plain prefab).
#[derive(Clone, Debug)]
pub struct ResolvedPrefab {
    /// The template whose entities are spawned.
    pub template: Arc<PrefabTemplate>,
    /// Variant steps to apply after spawning, base-most first.
    pub chain: Vec<Arc<PrefabVariant>>,
}

impl PrefabLibrary {
    /// Registers a variant (replacing a template registered under the same id).
    pub fn insert_variant(&mut self, variant: PrefabVariant) {
        self.revision = crate::prefab::next_revision();
        self.templates.remove(&variant.asset);
        self.variants.insert(variant.asset, Arc::new(variant));
    }

    /// Looks a variant up.
    pub fn variant(&self, asset: Guid) -> Option<&Arc<PrefabVariant>> {
        self.variants.get(&asset)
    }

    /// The ids from `asset` down to the template at the bottom of its chain: `[asset]` for a plain
    /// prefab, `[variant, …, base]` for a variant. Stops (without error) at an unregistered id, a
    /// cycle or [`MAX_VARIANT_CHAIN`]; [`Self::resolve`] reports those.
    pub fn variant_chain(&self, asset: Guid) -> Vec<Guid> {
        let mut chain = vec![asset];
        let mut cursor = asset;
        while let Some(variant) = self.variants.get(&cursor) {
            if chain.contains(&variant.base) || chain.len() > MAX_VARIANT_CHAIN {
                break;
            }
            chain.push(variant.base);
            cursor = variant.base;
        }
        chain
    }

    /// Every registered variant whose chain passes through `asset` (directly or through other
    /// variants), sorted by id: the prefabs to re-instantiate when `asset` changes.
    pub fn dependent_variants(&self, asset: Guid) -> Vec<Guid> {
        let mut out: Vec<Guid> = self
            .variants
            .keys()
            .copied()
            .filter(|id| *id != asset && self.variant_chain(*id)[1..].contains(&asset))
            .collect();
        out.sort();
        out
    }

    /// Resolves `asset` to its base template and variant steps.
    ///
    /// # Errors
    ///
    /// [`SceneSpawnError::PrefabNotLoaded`] naming the first unregistered id of the chain;
    /// [`SceneSpawnError::Invalid`] for a variant cycle or a chain longer than
    /// [`MAX_VARIANT_CHAIN`].
    pub fn resolve(&self, asset: Guid) -> Result<ResolvedPrefab, SceneSpawnError> {
        let mut chain: Vec<Arc<PrefabVariant>> = Vec::new();
        let mut cursor = asset;
        loop {
            if let Some(template) = self.templates.get(&cursor) {
                chain.reverse();
                return Ok(ResolvedPrefab {
                    template: template.clone(),
                    chain,
                });
            }
            let Some(variant) = self.variants.get(&cursor) else {
                return Err(SceneSpawnError::PrefabNotLoaded(cursor));
            };
            chain.push(variant.clone());
            if chain.iter().any(|step| step.asset == variant.base) {
                return Err(SceneSpawnError::Invalid(format!(
                    "prefab variant cycle: {asset} reaches {} again through {}",
                    variant.base, variant.asset
                )));
            }
            if chain.len() > MAX_VARIANT_CHAIN {
                return Err(SceneSpawnError::Invalid(format!(
                    "prefab variant chain of {asset} is longer than {MAX_VARIANT_CHAIN}"
                )));
            }
            cursor = variant.base;
        }
    }
}

/// The prefabs instantiated by a record's added children (recursively): a variant needs them
/// loaded before it can spawn.
pub(crate) fn nested_assets(record: &PrefabInstanceData) -> Vec<Guid> {
    let mut out = Vec::new();
    let mut stack: Vec<&SceneEntity> = record.added_children.iter().collect();
    while let Some(child) = stack.pop() {
        if let Some(instance) = &child.prefab_instance {
            out.push(instance.prefab.guid());
            stack.extend(instance.added_children.iter());
        }
    }
    out
}

#[cfg(test)]
mod tests;
