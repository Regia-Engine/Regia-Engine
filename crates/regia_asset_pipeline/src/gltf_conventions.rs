// SPDX-License-Identifier: MPL-2.0
//! glTF convention helpers for the cook path (`docs/formats/gltf-conventions.md`).

use std::collections::BTreeMap;

use regia_asset::{
    AssetId, LodGroup, LodLevel, MaterialVariant, MaterialVariantSet, PrefabSocket, PrefabSocketSet,
};

/// Parse `Name_LOD2` → `Some(("Name", 2))`.
#[must_use]
pub fn parse_lod_name(name: &str) -> Option<(&str, u32)> {
    let lod_idx = name.rfind("_LOD")?;
    let stem = &name[..lod_idx];
    let suffix = &name[lod_idx + 4..];
    if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let level: u32 = suffix.parse().ok()?;
    if stem.is_empty() {
        return None;
    }
    Some((stem, level))
}

/// Default screen-size threshold for authored LOD `level`: `2^-level` (ADR-7869).
/// Every supported chain level has a strictly smaller threshold than its predecessor.
/// Powers of two are exact through level 149, including subnormals; higher levels return zero.
#[must_use]
pub fn default_lod_screen_size(level: u32) -> f32 {
    match level {
        0..=126 => f32::from_bits((127 - level) << 23),
        127..=149 => f32::from_bits(1 << (149 - level)),
        _ => 0.0,
    }
}

/// Build [`LodGroup`]s from `(node_name, mesh_id)` pairs that use `_LODn` suffixes, with the
/// importer's default screen sizes ([`default_lod_screen_size`]).
#[must_use]
pub fn build_lod_groups(entries: &[(String, AssetId)]) -> Vec<LodGroup> {
    build_lod_groups_with(entries, &crate::lod_classes::LodThresholdInputs::default()).0
}

/// [`build_lod_groups`] with screen sizes from data: each `_LODn` node's extras, then the class
/// table, then the default ([`crate::lod_classes`], ADR-260). Returns the groups and the
/// warnings (repaired or ignored values). With empty `inputs` the groups equal
/// [`build_lod_groups`]'s.
#[must_use]
pub fn build_lod_groups_with(
    entries: &[(String, AssetId)],
    inputs: &crate::lod_classes::LodThresholdInputs,
) -> (Vec<LodGroup>, Vec<String>) {
    let mut by_stem: BTreeMap<String, Vec<(u32, AssetId, Option<f32>)>> = BTreeMap::new();
    for (name, id) in entries {
        if let Some((stem, level)) = parse_lod_name(name) {
            let node = inputs.node_sizes.get(name).copied();
            by_stem
                .entry(stem.to_string())
                .or_default()
                .push((level, *id, node));
        }
    }
    let mut groups = Vec::new();
    let mut warnings = Vec::new();
    for (name, mut levels) in by_stem {
        levels.sort_by_key(|(l, _, _)| *l);
        // Only emit a group when at least LOD0 and one lower level exist.
        if levels.len() < 2 || levels[0].0 != 0 {
            continue;
        }
        // A node with several primitives lists its level once per primitive: resolve each
        // level once, so the chain checks compare distinct levels.
        let mut indices: Vec<u32> = Vec::new();
        let mut nodes: Vec<Option<f32>> = Vec::new();
        for (level, _, node) in &levels {
            if indices.last() == Some(level) {
                if let Some(last) = nodes.last_mut() {
                    *last = last.or(*node);
                }
            } else {
                indices.push(*level);
                nodes.push(*node);
            }
        }
        let (sizes, notes) = crate::lod_classes::resolve_lod_screen_sizes(
            &name,
            &indices,
            &nodes,
            inputs.class_sizes.as_deref(),
        );
        warnings.extend(notes);
        let size_of = |level: u32| {
            indices
                .iter()
                .position(|l| *l == level)
                .and_then(|i| sizes.get(i).copied())
                .unwrap_or_else(|| default_lod_screen_size(level))
        };
        groups.push(LodGroup {
            name,
            levels: levels
                .into_iter()
                .map(|(level, mesh, _)| LodLevel {
                    level,
                    mesh,
                    screen_relative_size: size_of(level),
                })
                .collect(),
        });
    }
    (groups, warnings)
}

/// Strip `socket_` and record the local matrix.
#[must_use]
pub fn socket_from_node(name: &str, local_matrix: [f32; 16]) -> Option<PrefabSocket> {
    let id = name.strip_prefix("socket_")?;
    if id.is_empty() {
        return None;
    }
    Some(PrefabSocket {
        name: id.to_string(),
        local_matrix,
    })
}

/// Collect sockets into a set.
#[must_use]
pub fn prefab_socket_set(sockets: Vec<PrefabSocket>) -> PrefabSocketSet {
    PrefabSocketSet { sockets }
}

/// Group materials named `stem_Rem` / `stem_A52` (and other `_Variant` suffixes) into sets.
///
/// Recognised faction suffixes: `Rem`, `A52`. A stem needs at least two recognised variants.
#[must_use]
pub fn build_material_variant_sets(materials: &[(String, AssetId)]) -> Vec<MaterialVariantSet> {
    const FACTIONS: &[&str] = &["Rem", "A52"];
    let mut by_slot: BTreeMap<String, Vec<MaterialVariant>> = BTreeMap::new();
    for (name, id) in materials {
        for faction in FACTIONS {
            let suffix = format!("_{faction}");
            if let Some(stem) = name.strip_suffix(&suffix) {
                if !stem.is_empty() {
                    by_slot
                        .entry(stem.to_string())
                        .or_default()
                        .push(MaterialVariant {
                            name: (*faction).to_string(),
                            material: *id,
                        });
                }
                break;
            }
        }
    }
    by_slot
        .into_iter()
        .filter(|(_, v)| v.len() >= 2)
        .map(|(slot, mut variants)| {
            variants.sort_by(|a, b| a.name.cmp(&b.name));
            MaterialVariantSet { slot, variants }
        })
        .collect()
}

/// Pack separate occlusion (R) and metallic-roughness (G=rough, B=metal) into ORM RGBA8.
///
/// Output is linear: R = occlusion, G = roughness, B = metallic, A = 255.
/// Missing occlusion becomes 255; missing MR channels default to rough=255, metal=0.
#[must_use]
pub fn pack_orm_rgba8(
    width: u32,
    height: u32,
    occlusion_r: Option<&[u8]>,
    metallic_roughness_rgba: Option<&[u8]>,
) -> Vec<u8> {
    let n = (width as usize).saturating_mul(height as usize);
    let mut out = vec![0u8; n * 4];
    for i in 0..n {
        let ao = occlusion_r
            .and_then(|b| b.get(i * 4).or_else(|| b.get(i)))
            .copied()
            .unwrap_or(255);
        let (rough, metal) = if let Some(mr) = metallic_roughness_rgba {
            let base = i * 4;
            let g = mr.get(base + 1).copied().unwrap_or(255);
            let b = mr.get(base + 2).copied().unwrap_or(0);
            (g, b)
        } else {
            (255, 0)
        };
        let o = i * 4;
        out[o] = ao;
        out[o + 1] = rough;
        out[o + 2] = metal;
        out[o + 3] = 255;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::AssetId;

    #[test]
    fn parses_lod_names() {
        assert_eq!(parse_lod_name("Body_LOD0"), Some(("Body", 0)));
        assert_eq!(parse_lod_name("Body_LOD2"), Some(("Body", 2)));
        assert_eq!(parse_lod_name("socket_turret"), None);
        assert_eq!(parse_lod_name("Body"), None);
    }

    #[test]
    fn lod_group_needs_chain() {
        let a = AssetId::new_random();
        let b = AssetId::new_random();
        let c = AssetId::new_random();
        let groups = build_lod_groups(&[
            ("Body_LOD0".into(), a),
            ("Body_LOD1".into(), b),
            ("Body_LOD2".into(), c),
            ("Solo_LOD0".into(), AssetId::new_random()),
        ]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "Body");
        assert_eq!(groups[0].levels.len(), 3);
        assert_eq!(groups[0].levels[0].mesh, a);
        assert_eq!(groups[0].levels[2].level, 2);
    }

    #[test]
    fn lod_groups_take_sizes_from_extras_then_class() {
        let ids: Vec<AssetId> = (0..5).map(|_| AssetId::new_random()).collect();
        let entries = vec![
            ("Hull_LOD0".to_string(), ids[0]),
            ("Hull_LOD0".to_string(), ids[1]), // a second primitive of LOD0
            ("Hull_LOD1".to_string(), ids[2]),
            ("Hull_LOD2".to_string(), ids[3]),
            ("Hull_LOD3".to_string(), ids[4]),
        ];
        let sizes = |groups: &[LodGroup]| -> Vec<f32> {
            groups[0].levels.iter().map(|l| l.screen_relative_size).collect()
        };
        // No data: today's defaults, bit for bit (and the same as build_lod_groups).
        let plain = build_lod_groups(&entries);
        assert_eq!(sizes(&plain), vec![1.0, 1.0, 0.5, 0.25, 0.125]);
        let mut inputs = crate::lod_classes::LodThresholdInputs::default();
        assert_eq!(build_lod_groups_with(&entries, &inputs), (plain, Vec::new()));
        inputs.class_sizes = Some(vec![1.2, 0.45, 0.16, 0.05]);
        let (groups, warnings) = build_lod_groups_with(&entries, &inputs);
        assert_eq!(sizes(&groups), vec![1.2, 1.2, 0.45, 0.16, 0.05]);
        assert!(warnings.is_empty(), "{warnings:?}");
        inputs.node_sizes.insert("Hull_LOD1".to_string(), 0.6);
        let (groups, _) = build_lod_groups_with(&entries, &inputs);
        assert_eq!(
            sizes(&groups),
            vec![1.2, 1.2, 0.6, 0.16, 0.05],
            "the node beats the class"
        );
    }

    #[test]
    fn sockets_strip_prefix() {
        let s = socket_from_node("socket_turret", [1.0; 16]).expect("socket");
        assert_eq!(s.name, "turret");
        assert!(socket_from_node("mesh", [0.0; 16]).is_none());
    }

    #[test]
    fn rem_a52_variants() {
        let rem = AssetId::new_random();
        let a52 = AssetId::new_random();
        let sets = build_material_variant_sets(&[
            ("hull_Rem".into(), rem),
            ("hull_A52".into(), a52),
            ("lonely_Rem".into(), AssetId::new_random()),
        ]);
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].slot, "hull");
        assert_eq!(sets[0].get("Rem"), Some(rem));
        assert_eq!(sets[0].get("A52"), Some(a52));
    }

    #[test]
    fn orm_packing_channels() {
        // 1×1: occlusion R=10, MR G=20 B=30
        let ao = [10u8, 0, 0, 255];
        let mr = [0u8, 20, 30, 255];
        let packed = pack_orm_rgba8(1, 1, Some(&ao), Some(&mr));
        assert_eq!(packed, vec![10, 20, 30, 255]);
    }
}
