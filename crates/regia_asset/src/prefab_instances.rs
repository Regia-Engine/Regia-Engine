// SPDX-License-Identifier: MPL-2.0
//! GPU-instanced copies of an imported model's nodes (`docs/formats/prefab-instances.md`, ADR-3016).
//!
//! A glTF node with `EXT_mesh_gpu_instancing` draws its mesh once per instance. The importer keeps
//! that as data: one [`PrefabInstanceSet`], the glTF's `prefab/instances` sub-asset
//! ([`PREFAB_INSTANCES_SUB_PATH`]), lists each instanced node's cooked mesh parts with one matrix per
//! instance. The geometry is cooked once. A file without the extension gets no such sub-asset, so its
//! cook is byte for byte what it was before.
//!
//! A part's vertices are already in the model's space (node transforms and the importer's axis turn
//! baked), and a consumer that ignores this set draws the single copy a viewer without the extension
//! would draw. A matrix here is relative to that baked copy: instance `i` of a part draws at
//! `model_world * transforms[i] * vertex`. The node's own transform is `W` and the authored instance
//! transform is `I`, so `transforms[i] = W * I * W⁻¹`.

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::cooked_header::{CookedHeader, decode_cooked_file};
use crate::id::AssetId;
use crate::loader::AssetTypeTag;

/// [`PrefabInstanceSet::format_version`] written by this build.
pub const PREFAB_INSTANCES_FORMAT_VERSION: u32 = 1;

/// Reflect type path of the cooked payload (the `FCKD` header's type tag hashes it).
pub const PREFAB_INSTANCES_TYPE_PATH: &str = "regia_asset::PrefabInstanceSet";

/// Sub-asset path of a glTF's instance set (`AssetId::sub_asset(gltf, "prefab/instances")`).
pub const PREFAB_INSTANCES_SUB_PATH: &str = "prefab/instances";

/// Most instanced nodes one set may hold.
pub const MAX_INSTANCED_NODES: usize = 1 << 16;

/// Most mesh parts of one instanced node.
pub const MAX_INSTANCED_PARTS: usize = 1 << 16;

/// Most instances of one node.
pub const MAX_NODE_INSTANCES: usize = 1 << 24;

/// Most bytes of one node name.
pub const MAX_INSTANCED_NODE_NAME_BYTES: usize = 1024;

/// One node drawn many times.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrefabInstancedNode {
    /// The glTF node's name (`Unnamed` when it has none).
    pub node: String,
    /// The node's cooked mesh sub-assets, one per primitive (also parts of the `model` sub-asset).
    pub parts: Vec<AssetId>,
    /// Column-major model-space matrices, one per instance, relative to the baked parts (see the
    /// module docs).
    pub transforms: Vec<[f32; 16]>,
}

/// Every instanced node of one imported model. Written only for a file that has one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrefabInstanceSet {
    /// [`PREFAB_INSTANCES_FORMAT_VERSION`].
    pub format_version: u32,
    /// Instanced nodes, in the order the node walk meets them.
    pub nodes: Vec<PrefabInstancedNode>,
}

/// Why a [`PrefabInstanceSet`] was refused.
#[derive(Debug, Error)]
pub enum PrefabInstancesError {
    /// The payload does not decode.
    #[error("cooked prefab instances do not decode: {0}")]
    Postcard(#[from] postcard::Error),
    /// A version this build does not read (the cooker rebuilds it).
    #[error("prefab instance set version {found} (this build reads {supported})")]
    UnsupportedVersion {
        /// Version in the payload.
        found: u32,
        /// [`PREFAB_INSTANCES_FORMAT_VERSION`].
        supported: u32,
    },
    /// A rule of the format is broken.
    #[error("prefab instance set node {index}: {reason}")]
    Invalid {
        /// Index of the offending node.
        index: usize,
        /// Which rule.
        reason: String,
    },
    /// The file is a cooked asset of another type.
    #[error("cooked file is not a prefab instance set")]
    WrongType,
    /// The cooked file's framing is damaged.
    #[error("cooked file: {0}")]
    Cooked(String),
}

impl PrefabInstancesError {
    /// The agent error code: lowercase ASCII, digits and `_`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Postcard(_) | Self::Cooked(_) => "prefab_instances_corrupt",
            Self::UnsupportedVersion { .. } => "prefab_instances_version",
            Self::Invalid { .. } => "prefab_instances_invalid",
            Self::WrongType => "prefab_instances_wrong_type",
        }
    }
}

impl PrefabInstanceSet {
    /// A set at the current version.
    #[must_use]
    pub fn new(nodes: Vec<PrefabInstancedNode>) -> Self {
        Self {
            format_version: PREFAB_INSTANCES_FORMAT_VERSION,
            nodes,
        }
    }

    /// How many instances the whole set holds.
    #[must_use]
    pub fn instance_count(&self) -> usize {
        self.nodes.iter().map(|n| n.transforms.len()).sum()
    }

    /// The entry for a node name.
    #[must_use]
    pub fn node(&self, name: &str) -> Option<&PrefabInstancedNode> {
        self.nodes.iter().find(|n| n.node == name)
    }

    /// Serialize to postcard bytes.
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Deserialize and validate. The version is read and checked before the rest.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, PrefabInstancesError> {
        let (version, _): (u32, _) = postcard::take_from_bytes(bytes)?;
        if version != PREFAB_INSTANCES_FORMAT_VERSION {
            return Err(PrefabInstancesError::UnsupportedVersion {
                found: version,
                supported: PREFAB_INSTANCES_FORMAT_VERSION,
            });
        }
        let set: Self = postcard::from_bytes(bytes)?;
        set.validate()?;
        Ok(set)
    }

    /// Checks every rule of `docs/formats/prefab-instances.md`: finite matrices, at least one part
    /// and one instance per node, and the size limits.
    pub fn validate(&self) -> Result<(), PrefabInstancesError> {
        if self.format_version != PREFAB_INSTANCES_FORMAT_VERSION {
            return Err(PrefabInstancesError::UnsupportedVersion {
                found: self.format_version,
                supported: PREFAB_INSTANCES_FORMAT_VERSION,
            });
        }
        let invalid = |index: usize, reason: &str| PrefabInstancesError::Invalid {
            index,
            reason: reason.to_string(),
        };
        if self.nodes.len() > MAX_INSTANCED_NODES {
            return Err(invalid(0, "more nodes than MAX_INSTANCED_NODES"));
        }
        for (i, node) in self.nodes.iter().enumerate() {
            if node.node.len() > MAX_INSTANCED_NODE_NAME_BYTES {
                return Err(invalid(i, "name longer than MAX_INSTANCED_NODE_NAME_BYTES"));
            }
            if node.parts.is_empty() || node.parts.len() > MAX_INSTANCED_PARTS {
                return Err(invalid(i, "needs 1 to MAX_INSTANCED_PARTS parts"));
            }
            if node.transforms.is_empty() || node.transforms.len() > MAX_NODE_INSTANCES {
                return Err(invalid(i, "needs 1 to MAX_NODE_INSTANCES instances"));
            }
            if node.transforms.iter().flatten().any(|v| !v.is_finite()) {
                return Err(invalid(i, "a transform is not finite"));
            }
        }
        Ok(())
    }
}

impl AssetTypeTag {
    /// Tag for [`PrefabInstanceSet`].
    pub fn prefab_instances() -> Self {
        Self(regia_core::id::TypePathHash::of(PREFAB_INSTANCES_TYPE_PATH))
    }
}

/// Decodes a cooked instance-set file (`FCKD` header plus payload) and validates it.
pub fn load_prefab_instances_bytes(
    bytes: &[u8],
) -> Result<(CookedHeader, PrefabInstanceSet), PrefabInstancesError> {
    let (header, payload) =
        decode_cooked_file(bytes).map_err(|e| PrefabInstancesError::Cooked(e.to_string()))?;
    if header.type_tag != AssetTypeTag::prefab_instances().0 {
        return Err(PrefabInstancesError::WrongType);
    }
    let set = PrefabInstanceSet::from_postcard(payload)?;
    Ok((header, set))
}

/// The version of a cooked instance-set file, `Ok(None)` for a cooked file of another type
/// (`regia upgrade-project` probe; `.fckd` is shared by every cooked type).
pub fn prefab_instances_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let (header, payload) = decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    if header.type_tag != AssetTypeTag::prefab_instances().0 {
        return Ok(None);
    }
    postcard::take_from_bytes::<u32>(payload)
        .map(|(version, _)| Some(version))
        .map_err(|e| e.to_string())
}

/// The `from` versions with a registered migration step (none: version 1 is the first).
#[must_use]
pub fn prefab_instances_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The format's field names, for the frozen-format gate (`docs/formats/format-hashes.ron`).
#[must_use]
pub fn prefab_instances_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<PrefabInstanceSet>(),
        serde_field_names::<PrefabInstancedNode>(),
    ])
}

/// Which variant of `KHR_materials_variants` (or of the naming convention) a spawned prefab shows,
/// across every slot of the model.
///
/// Built from the sets the cook stored ([`crate::MaterialVariantSet`]). A `KHR_materials_variants`
/// file lists every variant name in every slot, the primitive's default material where it has no
/// mapping, so [`Self::select`] changes the whole model at once. A slot without the name keeps its
/// material.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct PrefabVariantSelection {
    /// The sets of the model.
    pub sets: Vec<crate::MaterialVariantSet>,
    /// The active variant, `None` for the materials the model was authored with.
    pub active: Option<String>,
}

impl PrefabVariantSelection {
    /// A selection showing the default materials.
    #[must_use]
    pub fn new(sets: Vec<crate::MaterialVariantSet>) -> Self {
        Self { sets, active: None }
    }

    /// Every variant name of the model, sorted and without repeats.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .sets
            .iter()
            .flat_map(|set| set.variants.iter().map(|v| v.name.clone()))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Makes `name` active. Returns `false` and changes nothing when no slot has it.
    pub fn select(&mut self, name: &str) -> bool {
        if self.sets.iter().any(|set| set.get(name).is_some()) {
            self.active = Some(name.to_string());
            true
        } else {
            false
        }
    }

    /// Back to the materials the model was authored with.
    pub fn clear(&mut self) {
        self.active = None;
    }

    /// The material the active variant gives `slot`; `None` means the slot keeps its own.
    #[must_use]
    pub fn material(&self, slot: &str) -> Option<AssetId> {
        let active = self.active.as_deref()?;
        self.sets.iter().find(|set| set.slot == slot)?.get(active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MaterialVariant, MaterialVariantSet};

    fn set() -> PrefabInstanceSet {
        PrefabInstanceSet::new(vec![PrefabInstancedNode {
            node: "trees".into(),
            parts: vec![AssetId::new_random()],
            transforms: vec![[
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 2.0, 0.0, 0.0, 1.0,
            ]],
        }])
    }

    #[test]
    fn round_trips_through_postcard() {
        let set = set();
        let bytes = set.to_postcard().expect("encode");
        assert_eq!(PrefabInstanceSet::from_postcard(&bytes).expect("decode"), set);
        assert_eq!(set.instance_count(), 1);
        assert!(set.node("trees").is_some() && set.node("rocks").is_none());
    }

    #[test]
    fn refuses_a_newer_version_with_a_code() {
        let mut newer = set();
        newer.format_version = PREFAB_INSTANCES_FORMAT_VERSION + 1;
        let bytes = newer.to_postcard().expect("encode");
        let error = PrefabInstanceSet::from_postcard(&bytes).expect_err("newer");
        assert_eq!(error.code(), "prefab_instances_version");
    }

    #[test]
    fn refuses_broken_rules() {
        let check = |edit: &dyn Fn(&mut PrefabInstanceSet)| {
            let mut bad = set();
            edit(&mut bad);
            bad.validate().expect_err("refused").code()
        };
        assert_eq!(
            check(&|s| s.nodes[0].transforms[0][3] = f32::NAN),
            "prefab_instances_invalid"
        );
        assert_eq!(
            check(&|s| s.nodes[0].transforms.clear()),
            "prefab_instances_invalid"
        );
        assert_eq!(check(&|s| s.nodes[0].parts.clear()), "prefab_instances_invalid");
    }

    #[test]
    fn the_file_version_probe_skips_other_types() {
        let bytes = set().to_postcard().expect("encode");
        let header = CookedHeader {
            format_version: crate::COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::prefab_instances().0,
            asset_id: AssetId::new_random(),
            importer_version: crate::pack_importer_version(1, 12, 0),
            dependencies: vec![],
            payload_len: bytes.len() as u64,
        };
        let file = crate::encode_cooked_file(&header, &bytes).expect("file");
        assert_eq!(
            prefab_instances_file_version(&file).expect("probe"),
            Some(PREFAB_INSTANCES_FORMAT_VERSION)
        );
        assert!(load_prefab_instances_bytes(&file).is_ok());
        let other = CookedHeader {
            type_tag: AssetTypeTag::collision_set().0,
            ..header
        };
        let file = crate::encode_cooked_file(&other, &bytes).expect("file");
        assert_eq!(prefab_instances_file_version(&file).expect("probe"), None);
        assert_eq!(
            load_prefab_instances_bytes(&file).expect_err("wrong").code(),
            "prefab_instances_wrong_type"
        );
    }

    #[test]
    fn variant_selection_switches_every_slot_and_rejects_unknown_names() {
        let (red, blue, plain) = (
            AssetId::new_random(),
            AssetId::new_random(),
            AssetId::new_random(),
        );
        let variant = |name: &str, material| MaterialVariant {
            name: name.into(),
            material,
        };
        let mut selection = PrefabVariantSelection::new(vec![
            MaterialVariantSet {
                slot: "hull/primitive_0".into(),
                variants: vec![variant("red", red), variant("blue", blue)],
            },
            MaterialVariantSet {
                slot: "trim/primitive_0".into(),
                variants: vec![variant("red", plain), variant("blue", plain)],
            },
        ]);
        assert_eq!(selection.names(), vec!["blue".to_string(), "red".to_string()]);
        assert_eq!(selection.material("hull/primitive_0"), None, "default first");
        assert!(selection.select("blue"));
        assert_eq!(selection.material("hull/primitive_0"), Some(blue));
        assert_eq!(selection.material("trim/primitive_0"), Some(plain));
        assert!(!selection.select("green"));
        assert_eq!(selection.active.as_deref(), Some("blue"), "unchanged");
        assert_eq!(selection.material("no/such_slot"), None);
        selection.clear();
        assert_eq!(selection.material("hull/primitive_0"), None);
    }

    #[test]
    fn the_fingerprint_names_the_fields() {
        let print = prefab_instances_schema_fingerprint();
        for name in ["format_version", "nodes", "node", "parts", "transforms"] {
            assert!(print.contains(name), "{name} missing from {print}");
        }
    }
}
