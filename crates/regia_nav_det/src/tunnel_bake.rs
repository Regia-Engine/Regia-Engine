// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/tunnels.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 161-186 (`TunnelsState::bake_navmesh`) and 426-463 (`navmesh_ground`), mark: as-is (Stage A: unchanged).
// Edits: the spec 121 § 4.2 seam (the body of `bake_navmesh` is `TunnelNavBake::bake` of `TunnelNavMeshBake`, so
// `TunnelsState<NavMesh>` arms it); module paths (`crate::pathing::navmesh` -> `super::navmesh`, `crate::math` ->
// `regia_detsim::math`); the hover cliff (`Terrain::Hover.limits().2`, 0.78) is the bake's `hover_cliff` setting,
// default `MoveClass::HOVER.cliff` (the same value); doc comments (DO27 file, plan and map names dropped, missing
// docs added); two `// det-scan: allow` lines on the usize index spans in `navmesh_ground` (integer
// `Ord::max`, not a float order method). The file is byte-identical between regia-absorb-2 (e6c84ba6d)
// and regia-absorb-3.

//! The tunnel navmesh bake: the seam between the world-geometry compat layer and the deterministic
//! navmesh (spec 121 § 4.2, `TunnelNavBake`).
//!
//! `regia_world_geom::compat::state::TunnelsState<N>` arms a map's tunnels and asks a
//! [`TunnelNavBake`] for its navmesh; [`TunnelNavMeshBake`] is that bake for [`NavMesh`]: one
//! [`Corridor`] per sampled passage (tube, gallery, ramp) and a walkable [`GroundRaster`] from the
//! terrain ([`navmesh_ground`]), baked by [`NavMesh::bake`]. The ramp tops link to the ground at each
//! mouth and the passages join at the junctions, so a route can cross the whole system on the mesh.
//! A host arms [`NavTunnelsState`] (`TunnelsState<NavMesh>`):
//!
//! ```
//! use regia_nav_det::navmesh::{MeshAgent, PolyKind};
//! use regia_nav_det::tunnel_bake::{NavTunnelsState, TunnelNavMeshBake};
//! use regia_world_geom::compat::fixtures::switchbacks;
//! use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
//!
//! let terrain = TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).expect("grid"), 1280.0)
//!     .with_zero_counts(16384);
//! let tunnels =
//!     NavTunnelsState::for_layout_with_nav(switchbacks(), Some(&terrain), &TunnelNavMeshBake::default());
//! let navmesh = tunnels.navmesh.as_deref().expect("a terrain gives a navmesh");
//! assert!(navmesh.has_tunnels());
//! // A hull route: the hover agent, a tunnel metre at 75 % of a ground metre.
//! let hull = MeshAgent::hover(2.5);
//! if let Some(plan) = navmesh.find_path_scaled([-300.0, -256.0, 150.0], [300.0, -256.0, 150.0], &hull, 75) {
//!     let underground = plan.polys.iter().any(|p| navmesh.poly_kind(*p) == Some(PolyKind::Tunnel));
//!     println!("{} waypoints, underground {underground}", plan.waypoints.len());
//! }
//! ```
//!
//! The bake is a pure function of the sampled layout and the terrain (IEEE `+ - * /` and
//! [`regia_detsim::math::sqrt`] in the source order), so every peer bakes the same bytes and the same
//! [`NavMesh::bake_id`].
//!
//! The module also lets the deterministic grid read the compat terrain: [`TerrainSampler`] implements
//! [`GridGround`] (its own heights, patches and centre), so a host builds the grid and the tunnel
//! layer from the terrain it arms the tunnels with.

use std::sync::Arc;

use regia_detsim::math;
use regia_world_geom::compat::layout::SampledLayout;
use regia_world_geom::compat::sampler::TerrainSampler;
use regia_world_geom::compat::state::{TunnelNavBake, TunnelsState};

use super::grid::{GridGround, MoveClass, patch_signature};
use super::navmesh::{BakeParts, Corridor, GroundRaster, NavMesh};

/// The tunnel arm of a host that links the deterministic navmesh: `TunnelsState` with a [`NavMesh`]
/// baked beside the collision mesh (arm it with the `*_with_nav` constructors and
/// [`TunnelNavMeshBake`]).
pub type NavTunnelsState = TunnelsState<NavMesh>;

/// The [`TunnelNavBake`] for [`NavMesh`]: tunnel corridors from the sampled centrelines on a ground
/// raster from the terrain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TunnelNavMeshBake {
    /// The steepest ground (rise over run) the ground raster marks walkable: a hover hull's cliff
    /// limit. Default [`MoveClass::HOVER`]'s `cliff` (0.78), the grid's hover class, so the mesh and
    /// the grid agree on where a hull can stand.
    pub hover_cliff: f32,
}

impl Default for TunnelNavMeshBake {
    fn default() -> Self {
        TunnelNavMeshBake {
            hover_cliff: MoveClass::HOVER.cliff,
        }
    }
}

impl TunnelNavMeshBake {
    /// The bake inputs: one corridor per sampled passage (its floor points, half its width, its
    /// ceiling), the ground raster and the map width. `None` without a terrain (no surface to link to).
    pub fn parts(&self, sampled: &SampledLayout, terrain: Option<&TerrainSampler>) -> Option<BakeParts> {
        let terrain = terrain?;
        let corridors = sampled
            .passages
            .iter()
            .map(|p| Corridor {
                points: p.samples.iter().map(|s| s.p).collect(),
                half_width_m: p.width_m * 0.5,
                ceiling_m: p.ceiling_m,
            })
            .collect();
        Some(BakeParts {
            ground: Some(navmesh_ground(terrain, self.hover_cliff)),
            corridors,
            extent_m: terrain.width() as f32 * terrain.spacing,
            ..BakeParts::default()
        })
    }
}

impl TunnelNavBake for TunnelNavMeshBake {
    type Mesh = NavMesh;

    /// Bakes the navmesh from the sampled centrelines (tube, gallery and ramp corridors) plus a
    /// walkable ground raster from the terrain. `None` without a terrain (no surface to link to) or
    /// when [`NavMesh::bake`] refuses the parts. Deterministic: a pure function of the map data.
    fn bake(&self, sampled: &SampledLayout, terrain: Option<&TerrainSampler>) -> Option<Arc<NavMesh>> {
        let _span = tracing::trace_span!("tunnel_navmesh_bake").entered();
        let parts = self.parts(sampled, terrain)?;
        NavMesh::bake(&parts).ok().map(Arc::new)
    }
}

/// The surface navmesh raster from the terrain: one cell per terrain sample, walkable where a hover
/// hull can stand (the central-difference slope at or under `hover_cliff`, as the grid's hover cost
/// reads it). The base heightfield is used, not the baked ground patches: a mouth funnel lowers the
/// drawn ground into the open cut for the floor query, but the navmesh walks the ramp corridors there,
/// so the surface stays the flat ground the ramp tops link to.
pub fn navmesh_ground(terrain: &TerrainSampler, hover_cliff: f32) -> GroundRaster {
    let (w, h) = (terrain.width(), terrain.height());
    let cell_m = terrain.spacing;
    let (origin_x, origin_z) = (-terrain.half_x, -terrain.half_z);
    let mut height = vec![0.0f32; w * h];
    for z in 0..h {
        for x in 0..w {
            height[z * w + x] = terrain.height_at_index(x, z);
        }
    }
    let cliff = hover_cliff;
    let mut walk = vec![false; w * h];
    for z in 0..h {
        for x in 0..w {
            let (xa, xb) = (x.saturating_sub(1), (x + 1).min(w - 1));
            let (za, zb) = (z.saturating_sub(1), (z + 1).min(h - 1));
            // det-scan: allow(std_float_fn): `(xb - xa)` is a usize span, `Ord::max`, not a float order method.
            let gx = (height[z * w + xb] - height[z * w + xa]) / (cell_m * (xb - xa).max(1) as f32);
            // det-scan: allow(std_float_fn): `(zb - za)` is a usize span, `Ord::max`, not a float order method.
            let gz = (height[zb * w + x] - height[za * w + x]) / (cell_m * (zb - za).max(1) as f32);
            let steep = math::sqrt(gx * gx + gz * gz);
            walk[z * w + x] = steep <= cliff;
        }
    }
    GroundRaster {
        cols: w,
        rows: h,
        step_m: cell_m,
        origin_x,
        origin_z,
        walk,
        height,
    }
}

/// The compat terrain as the grid's ground: its samples (centre height subtracted), its half extents,
/// and its baked patches (a patch only lowers the ground; the grid flags the cells it lowers).
impl GridGround for TerrainSampler {
    fn cols(&self) -> usize {
        self.width()
    }
    fn rows(&self) -> usize {
        self.height()
    }
    fn spacing(&self) -> f32 {
        self.spacing
    }
    fn half_x(&self) -> f32 {
        self.half_x
    }
    fn half_z(&self) -> f32 {
        self.half_z
    }
    fn center_height(&self) -> f32 {
        self.center_height
    }
    fn height_at_index(&self, x: usize, z: usize) -> f32 {
        TerrainSampler::height_at_index(self, x, z)
    }
    fn height_at_world(&self, x: f32, z: f32) -> f32 {
        TerrainSampler::height_at_world(self, x, z)
    }
    fn has_patches(&self) -> bool {
        !self.patches.is_empty()
    }
    fn patch_signature(&self) -> u64 {
        patch_signature(
            self.patches
                .iter()
                .map(|p| (p.cols, p.rows, p.heights.as_slice())),
        )
    }
}
