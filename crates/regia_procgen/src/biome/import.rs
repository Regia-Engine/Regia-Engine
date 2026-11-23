// SPDX-License-Identifier: MPL-2.0
//! `.scatter.ron` → `.biome.ron` (spec 65 § 9, `regia procgen import-scatter`).
//!
//! One biome per scatter set, one layer per class (source `ScatterClass(set, class)`, so the
//! candidates are exactly the positions the scatter set places), one entry per weighted mesh
//! (weights carried over), render settings from the class. Scatter classes have no spacing rule, so
//! entries get zero footprints (a scatter class has no size intent to take one from, ADR-1665):
//! the imported biome places at exactly the scatter positions; only the mesh pick differs (a
//! different random stream with the same weights).

use std::collections::BTreeSet;

use regia_terrain::scatter::ScatterSet;

use crate::biome::def::{
    BiomeDef, BiomeEntry, BiomeLayer, CandidateSource, EntryRender, EntrySpawn, Footprint, Placement,
    ScaleRule, Yaw,
};

/// A name from a mesh path: the file stem, made unique with `_2`, `_3`, ...
fn entry_name(path: &str, taken: &mut BTreeSet<String>) -> String {
    let stem = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .split('.')
        .next()
        .unwrap_or("mesh");
    let base = if stem.is_empty() { "mesh" } else { stem };
    let mut name = base.to_string();
    let mut k = 2;
    while !taken.insert(name.clone()) {
        name = format!("{base}_{k}");
        k += 1;
    }
    name
}

/// The biome for a scatter set read from `set_path`.
pub fn biome_from_scatter(set: &ScatterSet, set_path: &str, name: &str) -> BiomeDef {
    let layers = set
        .classes
        .iter()
        .map(|class| {
            let mut taken = BTreeSet::new();
            let entries = class
                .meshes
                .iter()
                .map(|m| BiomeEntry {
                    weight: f64::from(m.weight),
                    footprint: Footprint {
                        core_m: Some(0.0),
                        spread_m: Some(0.0),
                    },
                    placement: Placement {
                        upright: f64::from(1.0 - class.align_to_normal.clamp(0.0, 1.0)),
                        yaw: Yaw::Random,
                        scale: ScaleRule::Keep,
                        scale_from_source: false,
                        sink_m: 0.0,
                        offset: None,
                    },
                    render: EntryRender {
                        cast_shadows: class.cast_shadows,
                        draw_distance_m: class.fade_end,
                        fade_m: (class.fade_end - class.fade_start).max(0.0),
                        ..EntryRender::default()
                    },
                    ..BiomeEntry::new(&entry_name(&m.mesh, &mut taken), EntrySpawn::Mesh(m.mesh.clone()))
                })
                .collect();
            BiomeLayer {
                source: CandidateSource::ScatterClass {
                    set: set_path.to_string(),
                    class: class.name.clone(),
                },
                entries,
                max_child_depth: 0,
                ..BiomeLayer::new(&class.name)
            }
        })
        .collect();
    BiomeDef {
        name: name.to_string(),
        layers,
        ..BiomeDef::default()
    }
}

/// Parses a `.scatter.ron` and converts it.
///
/// # Errors
///
/// The scatter set does not parse or validate.
pub fn biome_from_scatter_text(text: &str, set_path: &str, name: &str) -> Result<BiomeDef, String> {
    let set = ScatterSet::from_ron(text, None).map_err(|e| e.to_string())?;
    Ok(biome_from_scatter(&set, set_path, name))
}
