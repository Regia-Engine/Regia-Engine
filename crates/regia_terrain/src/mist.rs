// SPDX-License-Identifier: MPL-2.0
//! The terrain as a valley-mist grid (ADR-1394).
//!
//! [`regia_render::ValleyMist`] reads the ground from a small height and concavity grid
//! ([`regia_render::MistHeightGrid`]). [`mist_grid_from_map`] samples a [`TerrainMap`] into
//! one, so a game that has a terrain writes
//!
//! ```ignore
//! let mut mist = ValleyMist::default();
//! mist.grid = mist_grid_from_map(&terrain.map, 32.0, 300.0).map(Arc::new);
//! world.insert_resource(mist);
//! ```

use regia_render::{MIST_GRID_MAX_SIDE, MistHeightGrid};

use crate::sampler::TerrainMap;

/// Samples `map` over its whole extent into a [`MistHeightGrid`] with texels about `cell_m`
/// apart (coarser when the map would need more than [`MIST_GRID_MAX_SIDE`] texels a side) and
/// a concavity window of `concavity_radius_m`.
///
/// `None` when `cell_m` is not positive and finite.
#[must_use]
pub fn mist_grid_from_map(map: &TerrainMap, cell_m: f32, concavity_radius_m: f32) -> Option<MistHeightGrid> {
    if !cell_m.is_finite() || cell_m <= 0.0 {
        return None;
    }
    let world = map.world_width();
    let min_cell = world / (MIST_GRID_MAX_SIDE - 1) as f32;
    let cell = cell_m.max(min_cell);
    let side = ((world / cell).ceil() as u32 + 1).clamp(2, MIST_GRID_MAX_SIDE);
    MistHeightGrid::from_fn(
        [-0.5 * world, -0.5 * world],
        cell,
        side,
        side,
        concavity_radius_m,
        |x, z| map.sample_bilinear(x, z),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bowl_map() -> TerrainMap {
        let n = 65_u32;
        let world = 1280.0_f32;
        let mut heights = Vec::new();
        for row in 0..n {
            for col in 0..n {
                let x = col as f32 / (n - 1) as f32 * world - world / 2.0;
                let z = row as f32 / (n - 1) as f32 * world - world / 2.0;
                heights.push(30.0 * ((x * x + z * z).sqrt() / 400.0).min(1.0).powi(2));
            }
        }
        TerrainMap::from_heights(n, n, heights, world, 1.0, 0.0).expect("bowl map")
    }

    #[test]
    fn the_grid_covers_the_map_and_matches_its_heights() {
        let map = bowl_map();
        let grid = mist_grid_from_map(&map, 20.0, 250.0).expect("grid");
        assert_eq!(grid.origin(), [-640.0, -640.0]);
        assert!(grid.width() >= 64);
        for (x, z) in [(0.0, 0.0), (300.0, -120.0), (-500.0, 500.0)] {
            let want = map.sample_bilinear(x, z);
            let got = grid.sample(x, z)[0];
            assert!((got - want).abs() < 1.5, "({x}, {z}): {got} vs {want}");
        }
    }

    #[test]
    fn the_bowl_floor_is_concave_and_the_rim_is_not() {
        let grid = mist_grid_from_map(&bowl_map(), 20.0, 250.0).expect("grid");
        assert!(grid.sample(0.0, 0.0)[1] > 5.0);
        assert!(grid.sample(600.0, 600.0)[1] < 1.0);
    }

    #[test]
    fn a_huge_map_coarsens_the_cell_and_a_bad_cell_is_refused() {
        let map = bowl_map();
        assert!(mist_grid_from_map(&map, 0.0, 100.0).is_none());
        assert!(mist_grid_from_map(&map, f32::NAN, 100.0).is_none());
        let grid = mist_grid_from_map(&map, 0.01, 100.0).expect("coarsened");
        assert!(grid.width() <= MIST_GRID_MAX_SIDE);
    }
}
