// SPDX-License-Identifier: MPL-2.0
//! Crater and scorch deformation hooks (spec 26 § 1, spec 25 § 4 Terrain row).
//!
//! Craters displace the height mirror ([`TerrainMap`]) — the same mirror the GPU height
//! texture is refreshed from — and return the affected sample rectangle so the update is a
//! dirty-rect re-upload rather than a full one. Scorch marks are kept as albedo-darkening
//! data consumed by the terrain shader (and later the decal system, M14).
//!
//! In a sim-bridged game (DO27) the shell replays the simulation's crater/scorch events into
//! these hooks on both the render side and its own sampler, keeping the two in lock-step; the
//! engine never writes back into the simulation.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::sampler::TerrainMap;

/// Maximum scorch marks carried in the terrain uniform (shader loop bound).
pub const MAX_SCORCH_MARKS: usize = 16;

/// Errors returned for invalid deformations.
#[derive(Clone, Debug, Error)]
pub enum DeformError {
    /// Crater radius must be finite and positive.
    #[error("crater radius must be finite and > 0, got {0}")]
    BadRadius(f32),
    /// Crater centre must be finite.
    #[error("crater center must be finite, got {0:?}")]
    BadCentre([f32; 2]),
    /// Crater depth/rim must be finite.
    #[error("crater depth {depth} and rim {rim} must be finite")]
    BadDepth {
        /// The rejected depth.
        depth: f32,
        /// The rejected rim height.
        rim: f32,
    },
}

/// Half-open rectangle of affected samples `[col_start, col_end) × [row_start, row_end)`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DirtyRect {
    /// First affected sample column.
    pub col_start: u32,
    /// One past the last affected sample column.
    pub col_end: u32,
    /// First affected sample row.
    pub row_start: u32,
    /// One past the last affected sample row.
    pub row_end: u32,
}

impl DirtyRect {
    /// `true` when no samples are affected.
    pub fn is_empty(&self) -> bool {
        self.col_end <= self.col_start || self.row_end <= self.row_start
    }

    /// Grows the rectangle to hold sample `(col, row)`; `None` starts it there.
    pub(crate) fn include(rect: Option<Self>, col: u32, row: u32) -> Self {
        match rect {
            None => Self {
                col_start: col,
                col_end: col + 1,
                row_start: row,
                row_end: row + 1,
            },
            Some(r) => Self {
                col_start: r.col_start.min(col),
                col_end: r.col_end.max(col + 1),
                row_start: r.row_start.min(row),
                row_end: r.row_end.max(row + 1),
            },
        }
    }
}

/// A crater deformation: a smooth depression with a raised rim (in-house profile:
/// `(1 - t²)²` bowl plus a `sin²` rim bump, `t` = normalised radius).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Crater {
    /// Crater centre `(x, z)`, metres.
    pub center: [f32; 2],
    /// Crater radius, metres.
    pub radius: f32,
    /// Depth of the depression at the centre, metres (positive = dug out).
    pub depth: f32,
    /// Rim height, metres.
    pub rim_height: f32,
}

impl Crater {
    /// Validates the crater.
    ///
    /// # Errors
    ///
    /// See [`DeformError`].
    pub fn validate(&self) -> Result<(), DeformError> {
        if !self.center[0].is_finite() || !self.center[1].is_finite() {
            return Err(DeformError::BadCentre(self.center));
        }
        if !self.radius.is_finite() || self.radius <= 0.0 {
            return Err(DeformError::BadRadius(self.radius));
        }
        if !self.depth.is_finite() || !self.rim_height.is_finite() {
            return Err(DeformError::BadDepth {
                depth: self.depth,
                rim: self.rim_height,
            });
        }
        Ok(())
    }

    /// Height displacement at radius `r` from the centre (0 outside).
    pub fn displacement(&self, r: f32) -> f32 {
        if r >= self.radius {
            return 0.0;
        }
        let t = (r / self.radius).clamp(0.0, 1.0);
        let bowl = (1.0 - t * t) * (1.0 - t * t);
        let rim = (std::f32::consts::PI * t).sin().powi(2);
        -self.depth * bowl + self.rim_height * rim
    }
}

/// The sample rectangle a crater touches (border-clamped, half-open).
pub fn crater_dirty_rect(map: &TerrainMap, crater: &Crater) -> DirtyRect {
    let [u0, v0] = map.world_to_grid(crater.center[0] - crater.radius, crater.center[1] - crater.radius);
    let [u1, v1] = map.world_to_grid(crater.center[0] + crater.radius, crater.center[1] + crater.radius);
    let clamp_col = |v: f32| (v.floor().max(0.0) as u32).min(map.width().saturating_sub(1));
    let clamp_row = |v: f32| (v.floor().max(0.0) as u32).min(map.height().saturating_sub(1));
    DirtyRect {
        col_start: clamp_col(u0),
        col_end: (clamp_col(u1) + 1).min(map.width()),
        row_start: clamp_row(v0),
        row_end: (clamp_row(v1) + 1).min(map.height()),
    }
}

/// Applies a crater to the height mirror and returns the affected sample rectangle.
///
/// After this call the CPU sampler and (once [`crate::gpu::TerrainGpu::update_heights`] runs)
/// the GPU height texture agree again — sim and render stay consistent by construction.
///
/// On a [`crate::Terrain`] component this call alone does not complete the deformation: picks
/// (the `regia_pick` terrain ray) and CDLOD culling read the component's
/// [`crate::MinMaxPyramid`], which this function does not touch. Also call
/// `terrain.minmax.update_region(&terrain.map, &dirty)` with the returned rectangle, or the
/// crater stays invisible to picking and culling until the pyramid is next rebuilt. The
/// engine's own edit paths (brush strokes, runtime height patches) already refresh the
/// pyramid; only this direct-map path leaves it to the caller.
///
/// # Errors
///
/// See [`DeformError`].
///
/// # Example
///
/// ```
/// use regia_terrain::{Crater, MinMaxPyramid, QuadNode, TerrainMap, apply_crater};
///
/// let mut map = TerrainMap::from_heights(65, 65, vec![10.0; 65 * 65], 64.0, 1.0, 0.0)
///     .expect("valid map");
/// let mut minmax = MinMaxPyramid::build(&map, 4).expect("pyramid");
///
/// let crater = Crater { center: [0.0, 0.0], radius: 8.0, depth: 3.0, rim_height: 0.5 };
/// let dirty = apply_crater(&mut map, &crater).expect("valid crater");
///
/// // Picks and CDLOD culling read the pyramid, so refresh it over the dirty rectangle:
/// minmax.update_region(&map, &dirty);
/// let [floor, _] = minmax.node_min_max(QuadNode::root(4));
/// assert!(floor < 10.0, "the pyramid bounds the dug-out floor");
/// ```
pub fn apply_crater(map: &mut TerrainMap, crater: &Crater) -> Result<DirtyRect, DeformError> {
    crater.validate()?;
    let rect = crater_dirty_rect(map, crater);
    if rect.is_empty() {
        return Ok(rect);
    }
    for row in rect.row_start..rect.row_end {
        for col in rect.col_start..rect.col_end {
            let [x, z] = map.sample_to_world(col, row);
            let r = ((x - crater.center[0]).powi(2) + (z - crater.center[1]).powi(2)).sqrt();
            let delta = crater.displacement(r);
            if delta != 0.0 {
                let index = row as usize * map.width() as usize + col as usize;
                let old = map.height_at_index(index);
                map.set_height(index, old + delta);
            }
        }
    }
    Ok(rect)
}

/// A scorch mark: albedo darkening over a disc (consumed by the terrain shader now, by the
/// pooled decal system in M14).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScorchMark {
    /// Centre `(x, z)`, metres.
    pub center: [f32; 2],
    /// Radius, metres.
    pub radius: f32,
    /// Darkening strength in `0..=1`.
    pub strength: f32,
}

/// A bounded list of scorch marks for one terrain (older marks are dropped when full).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScorchList {
    /// The marks.
    pub marks: Vec<ScorchMark>,
}

impl ScorchList {
    /// Appends a mark, dropping the oldest when [`MAX_SCORCH_MARKS`] is exceeded.
    pub fn push(&mut self, mark: ScorchMark) {
        if self.marks.len() >= MAX_SCORCH_MARKS {
            self.marks.remove(0);
        }
        self.marks.push(mark);
    }

    /// Packs the marks into `vec4(x, z, radius, strength)` rows plus a trailing count row, the
    /// layout of the WGSL `Scorch` uniform.
    pub fn pack(&self) -> [[f32; 4]; MAX_SCORCH_MARKS + 1] {
        let mut rows = [[0.0f32; 4]; MAX_SCORCH_MARKS + 1];
        for (i, mark) in self.marks.iter().take(MAX_SCORCH_MARKS).enumerate() {
            rows[i] = [mark.center[0], mark.center[1], mark.radius, mark.strength];
        }
        rows[MAX_SCORCH_MARKS] = [self.marks.len().min(MAX_SCORCH_MARKS) as f32, 0.0, 0.0, 0.0];
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_map() -> TerrainMap {
        TerrainMap::from_heights(65, 65, vec![10.0; 65 * 65], 64.0, 1.0, 0.0).expect("valid map")
    }

    #[test]
    fn crater_profile_shape() {
        let crater = Crater {
            center: [0.0, 0.0],
            radius: 10.0,
            depth: 2.0,
            rim_height: 1.0,
        };
        assert!(
            (crater.displacement(0.0) + 2.0).abs() < 1e-5,
            "full depth at center"
        );
        // t = 0.5: bowl (1 - t^2)^2 = 0.5625, rim sin^2(pi t) = 1 -> -2 * 0.5625 + 1.
        assert!(
            (crater.displacement(5.0) + 0.125).abs() < 1e-4,
            "midpoint: {}",
            crater.displacement(5.0)
        );
        assert!(
            crater.displacement(9.0) > 0.0,
            "rim raised near the edge: {}",
            crater.displacement(9.0)
        );
        assert!(crater.displacement(10.0) == 0.0);
        assert!(crater.displacement(11.0) == 0.0);
    }

    #[test]
    fn apply_crater_lowers_centre_and_lifts_rim() {
        let mut map = flat_map();
        let crater = Crater {
            center: [0.0, 0.0],
            radius: 8.0,
            depth: 2.0,
            rim_height: 1.2,
        };
        let rect = apply_crater(&mut map, &crater).expect("valid crater");
        assert!(!rect.is_empty());
        let centre = map.sample_bilinear(0.0, 0.0);
        assert!(
            (centre - 8.0).abs() < 1e-2,
            "center lowered by the full depth: {centre}"
        );
        let rim = map.sample_bilinear(6.5, 0.0);
        assert!(rim > 10.0, "rim raised: {rim}");
        let outside = map.sample_bilinear(30.0, 30.0);
        assert_eq!(outside, 10.0);
    }

    #[test]
    fn dirty_rect_only_covers_the_footprint() {
        let mut map = flat_map();
        let crater = Crater {
            center: [20.0, -12.0],
            radius: 5.0,
            depth: 1.0,
            rim_height: 0.1,
        };
        let before: Vec<f32> = map.heights().to_vec();
        let rect = apply_crater(&mut map, &crater).expect("valid crater");
        let after = map.heights();
        for row in 0..map.height() {
            for col in 0..map.width() {
                let idx = row as usize * map.width() as usize + col as usize;
                let in_rect = col >= rect.col_start
                    && col < rect.col_end
                    && row >= rect.row_start
                    && row < rect.row_end;
                if !in_rect {
                    assert_eq!(
                        before[idx], after[idx],
                        "sample ({col},{row}) changed outside the dirty rect"
                    );
                }
            }
        }
    }

    #[test]
    fn dirty_rect_clamps_to_the_map() {
        let map = flat_map();
        let crater = Crater {
            center: [-500.0, -500.0],
            radius: 30.0,
            depth: 1.0,
            rim_height: 0.1,
        };
        let rect = crater_dirty_rect(&map, &crater);
        assert_eq!(rect.col_start, 0);
        assert_eq!(rect.row_start, 0);
        // The crater never meets the map: at most the clamped corner cell, never the whole map.
        assert!(rect.col_end <= 1, "col_end {}", rect.col_end);
        assert!(rect.row_end <= 1, "row_end {}", rect.row_end);
    }

    #[test]
    fn dirty_rect_clamps_at_the_positive_edge() {
        let map = flat_map();
        // Straddles the +x/+z edge (32 m): grid column 54 (= 22 + 32) up to the map end.
        let crater = Crater {
            center: [32.0, 32.0],
            radius: 10.0,
            depth: 1.0,
            rim_height: 0.1,
        };
        let rect = crater_dirty_rect(&map, &crater);
        assert_eq!((rect.col_start, rect.col_end), (54, map.width()));
        assert_eq!((rect.row_start, rect.row_end), (54, map.height()));
    }

    #[test]
    fn rejects_invalid_craters() {
        let mut map = flat_map();
        assert!(matches!(
            apply_crater(
                &mut map,
                &Crater {
                    center: [0.0, 0.0],
                    radius: -1.0,
                    depth: 1.0,
                    rim_height: 0.0,
                }
            ),
            Err(DeformError::BadRadius(_))
        ));
        assert!(matches!(
            apply_crater(
                &mut map,
                &Crater {
                    center: [0.0, f32::NAN],
                    radius: 1.0,
                    depth: 1.0,
                    rim_height: 0.0,
                }
            ),
            Err(DeformError::BadCentre(_))
        ));
        for (depth, rim_height) in [(f32::INFINITY, 0.0), (1.0, f32::NAN), (f32::NAN, 0.0)] {
            assert!(matches!(
                apply_crater(
                    &mut map,
                    &Crater {
                        center: [0.0, 0.0],
                        radius: 1.0,
                        depth,
                        rim_height,
                    }
                ),
                Err(DeformError::BadDepth { .. })
            ));
        }
    }

    #[test]
    fn nan_crater_depth_is_bad_depth() {
        let mut map = flat_map();
        let before: Vec<f32> = (0..map.width() * map.height())
            .map(|i| map.height_at_index(i as usize))
            .collect();
        for (depth, rim_height) in [(f32::NAN, 0.0), (1.0, f32::INFINITY)] {
            let crater = Crater {
                center: [0.0, 0.0],
                radius: 1.0,
                depth,
                rim_height,
            };
            assert!(matches!(
                apply_crater(&mut map, &crater),
                Err(DeformError::BadDepth { .. })
            ));
        }
        let after: Vec<f32> = (0..map.width() * map.height())
            .map(|i| map.height_at_index(i as usize))
            .collect();
        assert_eq!(before, after, "a rejected crater must not touch the map");
    }

    #[test]
    fn sampler_and_surface_stay_consistent_after_deformation() {
        // After a crater, the rendered surface (re-derived from the height mirror) must still
        // match the sampler: both read the same mutated mirror.
        let mut map = flat_map();
        // Add some variation so the comparison is meaningful.
        for row in 0..map.height() {
            for col in 0..map.width() {
                let [x, z] = map.sample_to_world(col, row);
                let h = 10.0 + 0.5 * (x * 0.2).sin() * (z * 0.15).cos();
                let idx = row as usize * map.width() as usize + col as usize;
                map.set_height(idx, h);
            }
        }
        apply_crater(
            &mut map,
            &Crater {
                center: [3.0, -2.0],
                radius: 9.0,
                depth: 3.0,
                rim_height: 0.6,
            },
        )
        .expect("valid crater");
        let step = map.sample_spacing();
        for i in 0..2000 {
            let t = i as f32 * 0.031;
            let x = -30.0 + (t * 7.3).sin() * 30.0;
            let z = -30.0 + (t * 5.1).cos() * 30.0;
            let surface = crate::cdlod::terrain_surface_height(&map, step, x, z);
            let sampled = map.sample_bilinear(x, z);
            let inside_crater = ((x - 3.0).powi(2) + (z + 2.0).powi(2)).sqrt() < 10.0;
            // Outside the crater the smooth-field bound holds (≤ 1 cm). Inside, the crater's
            // curvature widens the triangle-vs-bilinear gap; both read the same mutated
            // mirror, so the two move together — the render/sim consistency this test guards.
            let bound = if inside_crater { 0.05 } else { 0.01 };
            assert!(
                (surface - sampled).abs() <= bound,
                "post-crater mismatch {surface} vs {sampled} at ({x},{z})"
            );
        }
    }

    #[test]
    fn scorch_list_packs_bounded() {
        let mut list = ScorchList::default();
        for i in 0..(MAX_SCORCH_MARKS + 5) {
            list.push(ScorchMark {
                center: [i as f32, 0.0],
                radius: 2.0,
                strength: 0.5,
            });
        }
        assert_eq!(list.marks.len(), MAX_SCORCH_MARKS);
        let packed = list.pack();
        assert_eq!(packed[MAX_SCORCH_MARKS][0], MAX_SCORCH_MARKS as f32);
        assert_eq!(packed[0][0], 5.0, "oldest marks dropped first");
    }
}
