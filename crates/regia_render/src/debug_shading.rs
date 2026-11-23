// SPDX-License-Identifier: MPL-2.0
//! The overdraw and LOD-coloring debug view modes (`specs/13` § 11, ADR-7211).
//!
//! Like the surface modes ([`crate::debug_surface`]) these draw by swapping each extracted mesh's
//! material, so they work on every path and platform with no shader change:
//!
//! - **Overdraw** ([`DebugViewMode::Overdraw`]): every mesh draws with one unlit, additive,
//!   double-sided material ([`overdraw_material`]) and no depth rejection between them (nothing is
//!   opaque), so each layer a pixel is covered by adds the same amount: the brighter, the more layers.
//! - **LOD coloring** ([`DebugViewMode::LodColor`]): every mesh draws unlit in the colour of the LOD
//!   level it was drawn at ([`LOD_COLORS`]: green, yellow, orange, red for LOD 0 to 3+).
//!
//! The ten other new modes (unlit, normals, base color, roughness, metallic, AO, light complexity,
//! shadow cascades, motion vectors, mip level) are drawn by the forward shader
//! ([`crate::lights_views::shading_view_code`]).

use std::sync::{Arc, OnceLock};

use crate::debug::DebugViewMode;
use crate::lab_frame::{ALPHA_ADDITIVE, LabMaterial};
use crate::runtime::ExtractedMesh;

/// What one overdraw layer adds (display-referred linear RGB).
pub const OVERDRAW_LAYER_COLOR: [f32; 3] = [0.10, 0.05, 0.015];

/// The colour of each LOD level (display-referred linear RGB): LOD 0, 1, 2 and 3 or coarser.
pub const LOD_COLORS: [[f32; 3]; 4] = [
    [0.1, 0.8, 0.15],
    [0.9, 0.85, 0.1],
    [0.95, 0.45, 0.05],
    [0.9, 0.08, 0.08],
];

/// Whether `mode` is one [`debug_shading_meshes`] draws.
#[must_use]
pub fn handles(mode: DebugViewMode) -> bool {
    matches!(mode, DebugViewMode::Overdraw | DebugViewMode::LodColor)
}

/// The material every mesh draws with in the overdraw view: unlit, additive, double sided.
#[must_use]
pub fn overdraw_material() -> LabMaterial {
    let [r, g, b] = OVERDRAW_LAYER_COLOR;
    LabMaterial {
        base_color: [r, g, b, 1.0],
        unlit: true,
        alpha_mode: ALPHA_ADDITIVE,
        double_sided: true,
        ..LabMaterial::default()
    }
}

/// The unlit material of LOD `level` in the LOD coloring view (levels past 3 take LOD 3's colour).
#[must_use]
pub fn lod_color_material(level: usize) -> LabMaterial {
    let [r, g, b] = LOD_COLORS[level.min(LOD_COLORS.len() - 1)];
    LabMaterial {
        base_color: [r, g, b, 1.0],
        unlit: true,
        ..LabMaterial::default()
    }
}

/// The LOD level a draw was extracted at, from its `lod_key` (`mesh_lod::mesh_lod_cache_key`:
/// 0 for LOD 0, the level shifted left by one otherwise).
#[must_use]
pub fn lod_level(lod_key: u64) -> usize {
    ((lod_key >> 1) & 3) as usize
}

struct Shared {
    overdraw: Arc<LabMaterial>,
    /// Per level, single and double sided.
    lod: [[Arc<LabMaterial>; 2]; 4],
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| {
        let lod = |level: usize| {
            let single = lod_color_material(level);
            let double = LabMaterial {
                double_sided: true,
                ..single.clone()
            };
            [Arc::new(single), Arc::new(double)]
        };
        Shared {
            overdraw: Arc::new(overdraw_material()),
            lod: [lod(0), lod(1), lod(2), lod(3)],
        }
    })
}

/// `meshes` with their materials swapped for `mode`; `None` when `mode` is not one of
/// [`handles`]' modes. The materials are shared, so a still frame uploads nothing new.
#[must_use]
pub fn debug_shading_meshes(meshes: &[ExtractedMesh], mode: DebugViewMode) -> Option<Vec<ExtractedMesh>> {
    if !handles(mode) {
        return None;
    }
    let _span = tracing::info_span!("debug_shading_meshes", meshes = meshes.len()).entered();
    let set = shared();
    Some(
        meshes
            .iter()
            .map(|m| {
                let mut twin = m.clone();
                twin.material = match mode {
                    DebugViewMode::Overdraw => Arc::clone(&set.overdraw),
                    _ => Arc::clone(&set.lod[lod_level(m.lod_key)][usize::from(m.material.double_sided)]),
                };
                // The swapped material reads no second UV set.
                twin.uv1 = None;
                twin
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lod_levels_follow_the_cache_key() {
        for level in 0..4 {
            assert_eq!(
                lod_level(crate::mesh_lod::mesh_lod_cache_key(level, false)),
                level
            );
        }
        assert_eq!(lod_level(crate::mesh_lod::mesh_lod_cache_key(7, false)), 3);
    }

    #[test]
    fn the_overdraw_material_adds_and_the_lod_colors_differ() {
        let m = overdraw_material();
        assert!(m.unlit && m.double_sided);
        assert_eq!(m.alpha_mode, ALPHA_ADDITIVE);
        for a in 0..4 {
            for b in a + 1..4 {
                assert_ne!(lod_color_material(a).base_color, lod_color_material(b).base_color);
            }
        }
        assert!(!handles(DebugViewMode::Lit));
        assert!(debug_shading_meshes(&[], DebugViewMode::Normals).is_none());
        assert_eq!(
            debug_shading_meshes(&[], DebugViewMode::Overdraw).map(|v| v.len()),
            Some(0)
        );
    }
}
