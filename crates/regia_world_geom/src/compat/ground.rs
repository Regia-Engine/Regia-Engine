// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/tunnels.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 426-570, mark: adapt (Stage A: unchanged).
// Edits: module paths, doc comments (DO27 file and engine names dropped, missing docs added), helper
// visibility. Re-synced from regia-absorb-1 (d6ed37677) by W-02 (ADR-8167): `query_ground` is the
// column's `span_at`, `sample_floor_with_tunnels` lost `sticky` and `SampleFloorTunnels::previous`,
// `STEP_BOUND_MM` is 50 (was 200), `SUPPORT_RISE_M` and the rubble candidate are gone.

//! The floor query: the span of the column a point is in ([`crate::compat::surface`]), the ceiling
//! paired with it, and the floor sample a placed or spawned body reads.

use crate::compat::mesh::*;
use crate::compat::sampler::{FloorSample, FloorSurface, TerrainSampler};

/// Optional tunnel arm inputs for [`sample_floor_with_tunnels`].
///
/// Existing callers keep using [`crate::compat::sampler::sample_floor`], which
/// passes this default (flag off, no mesh).
#[derive(Debug, Clone, Copy, Default)]
pub struct SampleFloorTunnels<'a> {
    /// The tunnel collision mesh, when the map has one.
    pub mesh: Option<&'a TunnelCollisionMesh>,
    /// Config flag `tunnels`; default false — heightfield only.
    pub enabled: bool,
}

/// T3: the answer of the one "what is under me / over me" query
/// ([`crate::compat::surface`]): the floor of the span a point is in and the ceiling
/// over it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroundSupport {
    /// The surface under the point.
    pub floor: FloorSample,
    /// The lowest lid over [`GroundSupport::floor`]. `None` under the sky.
    pub ceiling: Option<f32>,
}

/// A tunnel floor with less clearance under its lid than this never carries
/// a placed or spawned body; the outdoor ground does instead. Matches
/// the host's unit collision radius (2.5 m).
pub const HULL_FIT_M: f32 = 2.5;

/// T3 acceptance: the largest discontinuity of the surface under a body
/// between two ticks, in millimetres. 50 mm is the gate the T3 lane proves
/// on the host step for every hull class and a body on foot; the plan's first
/// figure was 200.
pub const STEP_BOUND_MM: i32 = 50;

/// True when a reported step between ticks stays under the T3 acceptance
/// bound (a 0.30 m sill hop fails, 0.04 m passes).
pub fn step_ok(prev_mm: i32, next_mm: i32) -> bool {
    (next_mm - prev_mm).abs() <= STEP_BOUND_MM
}

/// A terrain hole disc in integer millimetres (T2). The removed
/// `ground_query` column design carried this; folded in as the deterministic
/// integer form of the cut-mask test the wired [`TerrainSampler::hole_at`]
/// checks in metres. Integer circle test in `i128`, so i32 deltas up to 2^32
/// cannot wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoleDisc {
    /// Disc centre x, millimetres.
    pub x_mm: i32,
    /// Disc centre z, millimetres.
    pub z_mm: i32,
    /// Disc radius, millimetres.
    pub r_mm: u32,
}

/// A set of cut discs where the heightfield is gone for a column.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HoleMask {
    /// The cut discs, in any order.
    pub discs: Vec<HoleDisc>,
}

impl HoleMask {
    /// True when `(x_mm, z_mm)` is cut out of the terrain. Integer circle
    /// test (`dx*dx + dz*dz <= r*r` in `i128`), so huge coordinates and
    /// small radii never overflow and the answer is bit-identical on every
    /// peer.
    pub fn cut_at(&self, x_mm: i32, z_mm: i32) -> bool {
        self.discs.iter().any(|d| {
            let dx = x_mm as i128 - d.x_mm as i128;
            let dz = z_mm as i128 - d.z_mm as i128;
            let r = d.r_mm as i128;
            dx * dx + dz * dz <= r * r
        })
    }
}

/// The T3 query at a point: the span of the column at `(x, z)` that `y` is
/// in ([`crate::compat::surface::Column::span_at`]). A body asks at its waist
/// ([`crate::compat::surface::BODY_REACH_M`] over its origin); a shell or a camera
/// asks at its own height. Pure function of its arguments: no history, no
/// step-height band.
pub fn query_ground(
    terrain: Option<&TerrainSampler>,
    mesh: Option<&TunnelCollisionMesh>,
    x: f32,
    z: f32,
    y: f32,
) -> GroundSupport {
    let span = crate::compat::surface::column(terrain, mesh, x, z).span_at(y);
    GroundSupport {
        floor: span.floor,
        ceiling: span.ceiling,
    }
}

/// Sample the floor for a placed or spawned body with the optional tunnel arm.
///
/// T3: this is [`query_ground`] plus the fit gate. When `tunnels.enabled` is
/// false or no mesh is present the answer is the heightfield. A tunnel floor
/// with less than [`HULL_FIT_M`] clearance under its lid is rejected in favor
/// of the outdoor ground.
pub fn sample_floor_with_tunnels(
    terrain: Option<&TerrainSampler>,
    x: f32,
    z: f32,
    hint_y: f32,
    tunnels: SampleFloorTunnels<'_>,
) -> FloorSample {
    let outdoor = outdoor_sample(terrain, x, z);
    let Some(mesh) = tunnels.mesh.filter(|_| tunnels.enabled) else {
        return outdoor;
    };
    if mesh.is_empty() {
        return outdoor;
    }
    let support = query_ground(terrain, Some(mesh), x, z, hint_y);
    if support.floor.surface != FloorSurface::TunnelFloor {
        return support.floor;
    }
    let fits = match support.ceiling {
        Some(lid) => lid - support.floor.point[1] >= HULL_FIT_M,
        // No ceiling over this floor: open cut, clearance unlimited.
        None => true,
    };
    if fits { support.floor } else { outdoor }
}

/// The outdoor heightfield floor at `(x, z)`; a flat floor at y = 0 without a terrain.
pub(crate) fn outdoor_sample(terrain: Option<&TerrainSampler>, x: f32, z: f32) -> FloorSample {
    match terrain {
        Some(terrain) => {
            let y = terrain.height_at_world(x, z);
            FloorSample {
                point: [x, y, z],
                normal: [0.0, 1.0, 0.0],
                surface: FloorSurface::Heightfield,
            }
        }
        None => FloorSample {
            point: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            surface: FloorSurface::Heightfield,
        },
    }
}
