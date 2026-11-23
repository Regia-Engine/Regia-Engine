// SPDX-License-Identifier: MPL-2.0
//! Heightfield signed distance for probe rays (M15b).
//!
//! The distance is the same function [`regia_render::heightfield_distance`] uses,
//! evaluated on this map's bilinear height and central-difference slope. Render
//! code must not depend on this crate; the shared formula lives in `regia_render`.
//!
//! Provenance: the graph distance `(y − h) / sqrt(1 + |∇h|²)`.

use regia_render::heightfield_distance;

use crate::sampler::TerrainMap;

impl TerrainMap {
    /// Signed distance from `position` to the bilinear heightfield.
    ///
    /// Positive is above the surface. `position` is `(x, y, z)` in metres.
    #[must_use]
    pub fn signed_distance(&self, position: [f32; 3]) -> f32 {
        let h = self.sample_bilinear(position[0], position[2]);
        let n = self.normal_at(position[0], position[2]);
        let ny = n[1].abs().max(1.0e-4);
        let slope = (n[0] * n[0] + n[2] * n[2]).sqrt() / ny;
        heightfield_distance(position[1], h, slope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_map_distance_is_height_above_the_surface() {
        let map = TerrainMap::from_heights(2, 2, vec![0.0; 4], 10.0, 1.0, 0.0).expect("map");
        let above = map.signed_distance([0.0, 3.0, 0.0]);
        let below = map.signed_distance([0.0, -1.5, 0.0]);
        assert!((above - 3.0).abs() < 1.0e-4, "{above}");
        assert!((below - (-1.5)).abs() < 1.0e-4, "{below}");
    }

    #[test]
    fn sloped_map_matches_the_render_distance() {
        let map = TerrainMap::from_heights(2, 2, vec![0.0, 2.0, 0.0, 2.0], 2.0, 1.0, 0.0).expect("map");
        let y = 4.0;
        let got = map.signed_distance([0.0, y, 0.0]);
        let expect = heightfield_distance(y, 1.0, 0.5);
        assert!((got - expect).abs() < 1.0e-3, "terrain {got} render {expect}");
        assert!(got < y - 1.0, "a slope shortens the vertical gap");
    }
}
