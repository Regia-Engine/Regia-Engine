// SPDX-License-Identifier: MPL-2.0
//! Cooked glTF convention payloads (`docs/formats/gltf-conventions.md`).
//!
//! LOD groups, prefab sockets and selectable material variants produced by the
//! cook path so DO27 assets need no manual remapping at runtime.

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::id::AssetId;

/// Name prefix of glTF nodes that carry collision geometry only (`collision_<body>_<n>` hulls,
/// and the older single `collision_hull`). The cook and every glTF loader leave their meshes
/// out of rendering (ADR-1622); physics reads them by name.
pub const COLLISION_NODE_PREFIX: &str = "collision_";

/// Whether a glTF node named `name` is collision-only ([`COLLISION_NODE_PREFIX`], case
/// sensitive, the rule the cook has always applied).
///
/// ```
/// use regia_asset::is_collision_node_name;
///
/// assert!(is_collision_node_name("collision_hull"));
/// assert!(is_collision_node_name("collision_turret_0"));
/// assert!(!is_collision_node_name("Collision_hull")); // case sensitive
/// assert!(!is_collision_node_name("hull_collision_0"));
/// ```
#[must_use]
pub fn is_collision_node_name(name: &str) -> bool {
    name.starts_with(COLLISION_NODE_PREFIX)
}

/// One LOD level in an authored `_LOD0`…`_LODN` chain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LodLevel {
    /// LOD index (`0` = most detailed).
    pub level: u32,
    /// Cooked mesh sub-asset id for this level.
    pub mesh: AssetId,
    /// Relative screen size threshold (larger = more detail); LOD0 uses `1.0`.
    pub screen_relative_size: f32,
}

/// Authored LOD chain for one stem (`Body_LOD0` + `Body_LOD1` + `Body_LOD2`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LodGroup {
    /// Stem name without the `_LODn` suffix (`Body`).
    pub name: String,
    /// Levels sorted by ascending index.
    pub levels: Vec<LodLevel>,
}

/// A `socket_*` empty's local transform on a prefab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrefabSocket {
    /// Socket id with the `socket_` prefix stripped (`turret`).
    pub name: String,
    /// Column-major 4×4 local transform (metres, Regia Engine axes after forward convert).
    pub local_matrix: [f32; 16],
}

/// All sockets collected from a glTF for the cooked prefab.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct PrefabSocketSet {
    /// Named attachment points.
    pub sockets: Vec<PrefabSocket>,
}

/// One selectable material variant (e.g. Rem / A52 team skins).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MaterialVariant {
    /// Variant key (`Rem`, `A52`).
    pub name: String,
    /// Cooked material asset id.
    pub material: AssetId,
}

/// A set of mutually exclusive material variants for one slot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MaterialVariantSet {
    /// Slot stem shared by the variants (`hull` from `hull_Rem` / `hull_A52`).
    pub slot: String,
    /// Available variants.
    pub variants: Vec<MaterialVariant>,
}

impl MaterialVariantSet {
    /// Looks up a variant by name (case-sensitive).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<AssetId> {
        self.variants.iter().find(|v| v.name == name).map(|v| v.material)
    }
}

/// Runtime selection of one cooked material variant on a spawned prefab.
///
/// Built from a [`MaterialVariantSet`] the cook path stored for a slot
/// (`hull_Rem` / `hull_A52`). [`Self::select`] swaps [`Self::material`] to the
/// cooked material asset for that name.
#[derive(Component, Clone, Debug)]
pub struct PrefabInstance {
    /// Variant set cooked from the glTF.
    pub variant_set: MaterialVariantSet,
    /// Name of the active variant (`Rem`, `A52`).
    pub active_variant: String,
    /// Cooked material asset currently applied.
    pub material: AssetId,
}

impl PrefabInstance {
    /// Selects `initial` from `set`.
    ///
    /// Returns `None` when `initial` is not one of the set's variant names.
    #[must_use]
    pub fn new(set: MaterialVariantSet, initial: &str) -> Option<Self> {
        let material = set.get(initial)?;
        Some(Self {
            variant_set: set,
            active_variant: initial.to_string(),
            material,
        })
    }

    /// Switches the active material to `name`.
    ///
    /// Returns `false` and leaves the instance unchanged when `name` is not in
    /// the set.
    pub fn select(&mut self, name: &str) -> bool {
        let Some(material) = self.variant_set.get(name) else {
            return false;
        };
        self.active_variant = name.to_string();
        self.material = material;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_lookup() {
        let id = AssetId::new_random();
        let set = MaterialVariantSet {
            slot: "hull".into(),
            variants: vec![
                MaterialVariant {
                    name: "Rem".into(),
                    material: id,
                },
                MaterialVariant {
                    name: "A52".into(),
                    material: AssetId::new_random(),
                },
            ],
        };
        assert_eq!(set.get("Rem"), Some(id));
        assert!(set.get("missing").is_none());
    }

    #[test]
    fn prefab_select_swaps_material_and_rejects_unknown() {
        let rem = AssetId::new_random();
        let a52 = AssetId::new_random();
        let set = MaterialVariantSet {
            slot: "hull".into(),
            variants: vec![
                MaterialVariant {
                    name: "Rem".into(),
                    material: rem,
                },
                MaterialVariant {
                    name: "A52".into(),
                    material: a52,
                },
            ],
        };
        let mut inst = PrefabInstance::new(set, "Rem").expect("rem");
        assert_eq!(inst.material, rem);
        assert!(!inst.select("missing"));
        assert_eq!(inst.material, rem);
        assert_eq!(inst.active_variant, "Rem");
        assert!(inst.select("A52"));
        assert_eq!(inst.material, a52);
        assert_eq!(inst.active_variant, "A52");
        assert!(
            PrefabInstance::new(
                MaterialVariantSet {
                    slot: "hull".into(),
                    variants: vec![],
                },
                "Rem"
            )
            .is_none()
        );
    }
}
