// SPDX-License-Identifier: MPL-2.0
//! A road that conforms to the terrain surface without a second depth.
//!
//! Two forms, same surface:
//!
//! * [`RoadDecal`] is drawn in the terrain fragment shader. The fragment keeps the
//!   terrain's depth, so the road cannot z-fight the ground it replaces.
//! * [`RoadRibbon`] is a mesh for a host that draws its own pass. Every vertex sits
//!   on [`TerrainMap::sample_bilinear`] plus a lift smaller than the driveable
//!   agreement bound ([`crate::sim::DRIVEABLE_DELTA_M`]) and larger than the
//!   tested CDLOD interpolation error (≤ 1 cm, spec 26 § 1).
//!
//! The on-segment test is the standard clamped point-to-segment distance.

use crate::TerrainMap;
use crate::sim::DRIVEABLE_DELTA_M;

/// One straight road segment in the XZ plane, plus the colour the decal writes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RoadDecal {
    /// Segment start, world XZ metres.
    pub a: [f32; 2],
    /// Segment end, world XZ metres.
    pub b: [f32; 2],
    /// Half-width of the ribbon, metres. `<= 0` disables the decal.
    pub half_width: f32,
    /// Linear RGB the shader writes inside the ribbon.
    pub color: [f32; 3],
}

impl RoadDecal {
    /// A segment from `a` to `b`.
    pub fn new(a: [f32; 2], b: [f32; 2], half_width: f32, color: [f32; 3]) -> Self {
        Self {
            a,
            b,
            half_width,
            color,
        }
    }
}

/// True when `(x, z)` is within `half_width` of the segment `a`–`b`.
///
/// This is the CPU mirror of the WGSL road test in `terrain.wgsl`.
pub fn on_segment(a: [f32; 2], b: [f32; 2], half_width: f32, x: f32, z: f32) -> bool {
    if half_width <= 0.0 {
        return false;
    }
    let ab = [b[0] - a[0], b[1] - a[1]];
    let ap = [x - a[0], z - a[1]];
    let ab_len2 = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if ab_len2 <= 1e-8 {
        0.0
    } else {
        ((ap[0] * ab[0] + ap[1] * ab[1]) / ab_len2).clamp(0.0, 1.0)
    };
    let cx = a[0] + ab[0] * t;
    let cz = a[1] + ab[1] * t;
    let dx = x - cx;
    let dz = z - cz;
    dx * dx + dz * dz < half_width * half_width
}

/// A triangle mesh whose vertices lie on the visual heightfield.
#[derive(Clone, Debug)]
pub struct RoadRibbon {
    /// `position.xyz` plus a 1.0, row-major, one vertex per centerline sample per side.
    pub vertices: Vec<[f32; 4]>,
    /// Triangle list indices.
    pub indices: Vec<u32>,
    /// Lift above the bilinear surface, metres.
    pub lift_m: f32,
}

impl RoadRibbon {
    /// Builds a ribbon along `centerline` (world XZ), `half_width` metres either side.
    ///
    /// `lift_m` must be finite and inside `0.01..=` the driveable delta bound
    /// (1 cm through 0.25 m). The low end meets the tested CDLOD interpolation
    /// error; the high end stays inside the driveable agreement flag.
    ///
    /// # Errors
    ///
    /// Returns `TerrainMapError::BadWorldWidth` (carrying the half-width) when the
    /// centerline is shorter than two points, has a non-finite point, or the half-width is not finite and positive,
    /// and `TerrainMapError::BadHeightParams` when the lift is outside that range.
    pub fn conform(
        map: &TerrainMap,
        centerline: &[[f32; 2]],
        half_width: f32,
        lift_m: f32,
    ) -> Result<Self, crate::sampler::TerrainMapError> {
        if centerline.len() < 2
            || !half_width.is_finite()
            || half_width <= 0.0
            || centerline.iter().any(|p| !p[0].is_finite() || !p[1].is_finite())
        {
            return Err(crate::sampler::TerrainMapError::BadWorldWidth(half_width));
        }
        // 1 cm is the spec 26 base-surface agreement bound. The lift must clear it
        // and stay inside the driveable flag so a conforming road is not a delta.
        if !lift_m.is_finite() || !(0.01..=DRIVEABLE_DELTA_M).contains(&lift_m) {
            return Err(crate::sampler::TerrainMapError::BadHeightParams {
                scale: lift_m,
                offset: DRIVEABLE_DELTA_M,
            });
        }
        let mut vertices = Vec::with_capacity(centerline.len() * 2);
        let mut indices = Vec::with_capacity((centerline.len() - 1) * 6);
        for (i, p) in centerline.iter().enumerate() {
            let prev = if i == 0 { p } else { &centerline[i - 1] };
            let next = if i + 1 == centerline.len() {
                p
            } else {
                &centerline[i + 1]
            };
            let tx = next[0] - prev[0];
            let tz = next[1] - prev[1];
            let len = (tx * tx + tz * tz).sqrt().max(1e-6);
            // Left perpendicular in XZ.
            let lx = -tz / len * half_width;
            let lz = tx / len * half_width;
            for (ox, oz) in [(p[0] + lx, p[1] + lz), (p[0] - lx, p[1] - lz)] {
                let y = map.sample_bilinear(ox, oz) + lift_m;
                vertices.push([ox, y, oz, 1.0]);
            }
            if i + 1 < centerline.len() {
                let b = (i as u32) * 2;
                indices.extend_from_slice(&[b, b + 1, b + 2, b + 1, b + 3, b + 2]);
            }
        }
        Ok(Self {
            vertices,
            indices,
            lift_m,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_test_matches_a_hand_expanded_distance() {
        let a = [0.0, 0.0];
        let b = [10.0, 0.0];
        assert!(on_segment(a, b, 1.0, 5.0, 0.5));
        assert!(!on_segment(a, b, 1.0, 5.0, 1.5));
        // Past the end, the clamp keeps the end point: (12, 0) is 2 m from b.
        assert!(!on_segment(a, b, 1.0, 12.0, 0.0));
        assert!(on_segment(a, b, 2.5, 12.0, 0.0));
    }

    #[test]
    fn ribbon_vertices_sit_on_the_bilinear_surface() {
        let n = 17u32;
        let world = 64.0f32;
        let mut heights = Vec::with_capacity((n * n) as usize);
        for row in 0..n {
            for col in 0..n {
                let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
                let z = row as f32 / (n - 1) as f32 * world - world * 0.5;
                heights.push(3.0 + 0.15 * x + 0.05 * z);
            }
        }
        let map = TerrainMap::from_heights(n, n, heights, world, 8.0, 0.0).expect("map");
        let line = [[-8.0, -2.0], [0.0, 1.0], [8.0, 3.0]];
        let lift = 0.02;
        let ribbon = RoadRibbon::conform(&map, &line, 2.0, lift).expect("ribbon");
        assert_eq!(ribbon.vertices.len(), 6);
        assert_eq!(ribbon.indices.len(), 12);
        for v in &ribbon.vertices {
            let y = map.sample_bilinear(v[0], v[2]) + lift;
            assert!((v[1] - y).abs() < 1e-4, "vertex y {} left the surface {y}", v[1]);
        }
        // A lift inside (1 cm, 0.25 m) is the conforming band.
        assert!(RoadRibbon::conform(&map, &line, 2.0, 0.005).is_err());
        assert!(RoadRibbon::conform(&map, &line, 2.0, 0.3).is_err());
    }
}
