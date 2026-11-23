// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/tunnels.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 24-424 and 465-1120 (the tests), mark: as-is; `camera_floor`, `mesh_spans` and `camera_clip`
// (287-329) adapt (Stage A: unchanged; W-26 moves them out). Lines 161-186 (`bake_navmesh`) and 426-463
// (`navmesh_ground`) are not copied: they are the `TunnelNavBake` seam (spec 121 § 4.2, the nav crate).
// Edits: module paths, doc comments (DO27 file, plan and engine names dropped, missing docs added); the
// spec 121 § 4.2 seams (`TunnelsState<N = ()>` with `TunnelNavBake`, `for_layout` for `for_map`,
// `rock_radar_lit_m`, `UNIT_COLLISION_RADIUS_M`, `PILOT_STEP_M`, `regia_detsim::math`); float
// min/max/clamp through `regia_detsim::float::det_*` (ADR-8166). The free functions `sample_place_floor`
// and `nav_floor_y` are ir-sim/src/state.rs 10103-10113 at the same tag (as-is). In the tests a dead
// `prev` variable is dropped from one walk (it was never read; no assert touches it).

//! The tunnel arm of a lockstep sim: the config flag, the one surface query a body asks
//! ([`TunnelsState::support`], [`TunnelsState::move_floor`]), the walls and lid that confine it, the rock
//! between two points, and the nav overlay.
//!
//! Mesh collision and the column query live in [`crate::compat::mesh`] and [`crate::compat::surface`].
//! This module owns the config flag (default off) and the helpers that bind the hull disc radius
//! [`UNIT_COLLISION_RADIUS_M`].

use std::sync::Arc;

use regia_detsim::float::{det_clamp, det_max, det_min};
use regia_detsim::math;

use crate::compat::ground::{GroundSupport, HULL_FIT_M, query_ground};
use crate::compat::layout::{CrawlwayVolume, SampledLayout, TunnelLayout};
use crate::compat::mesh::{
    TunnelCollisionMesh, TunnelNavOverlay, clamp_altitude_under_ceiling, separate_disc_from_walls,
    separate_disc_from_walls_band,
};
use crate::compat::pack::TunnelsFile;
use crate::compat::sampler::{FloorSample, FloorSurface, TerrainSampler, sample_floor};
pub use crate::compat::surface::BODY_REACH_M;
use crate::compat::surface::column;

/// The host's unit collision radius, metres: the hull disc the walls push and the lid caps
/// (spec 121 § 4.2 seam for the source's `UNIT_COLLISION_RADIUS`; the same value).
pub const UNIT_COLLISION_RADIUS_M: f32 = 2.5;

/// The step a body on foot walks up, metres (the source's on-foot `PILOT_STEP_M`, read by
/// [`floor_with_skylight_prop`]; the on-foot module of lane W-03 uses the same value).
pub const PILOT_STEP_M: f32 = 0.55;

/// Config flag `tunnels`, default off: heightfield only until a host arms a map.
pub const TUNNELS_DEFAULT: bool = false;

/// A body whose feet are this far under the outdoor ground line is below it (a classification
/// for the camera and the audit in an open cut, not a floor choice).
pub const UNDERGROUND_M: f32 = 1.0;

/// True when the config flag is on **and** a collision mesh is present.
/// Flag-off or missing mesh keeps floor sampling on the heightfield.
pub fn tunnels_arm_active(enabled: bool, mesh: Option<&TunnelCollisionMesh>) -> bool {
    enabled && mesh.is_some()
}

/// Sim-side tunnel arm state. The mesh is stored beside the heightfield.
///
/// `N` is the host's tunnel navmesh type (spec 121 § 4.2, the [`TunnelNavBake`] seam); `()` for a host
/// that links no navmesh (a headless server without the nav crate).
#[derive(Debug)]
pub struct TunnelsState<N = ()> {
    /// When false (default), `sample_floor` never claims [`FloorSurface::TunnelFloor`].
    pub enabled: bool,
    /// The collision mesh (every mesh layer), when the map has one.
    pub mesh: Option<TunnelCollisionMesh>,
    /// The nav overlay baked from the mesh.
    pub nav: Option<TunnelNavOverlay>,
    /// The spline description the mesh was built from ([`crate::compat::layout`]), sampled.
    /// The pathing layer reads its centrelines and portals. `None` for hand-built meshes.
    pub layout: Option<Arc<MapTunnels>>,
    /// Pilot-only crawlways. Empty unless the layout lists some, or a surface
    /// map loaded a standalone volume list. Not part of the collision mesh.
    pub crawlways: Vec<CrawlwayVolume>,
    /// The host's navmesh with tunnel corridors (built from the same sampled centrelines by
    /// [`TunnelNavBake::bake`]), linked to the surface navmesh at each mouth. `None` when the map
    /// has no terrain, the bake failed, or the host bakes none.
    pub navmesh: Option<Arc<N>>,
}

impl<N> Clone for TunnelsState<N> {
    fn clone(&self) -> Self {
        Self {
            enabled: self.enabled,
            mesh: self.mesh.clone(),
            nav: self.nav.clone(),
            layout: self.layout.clone(),
            crawlways: self.crawlways.clone(),
            navmesh: self.navmesh.clone(),
        }
    }
}

impl<N> Default for TunnelsState<N> {
    fn default() -> Self {
        Self {
            enabled: false,
            mesh: None,
            nav: None,
            layout: None,
            crawlways: Vec::new(),
            navmesh: None,
        }
    }
}

/// Bakes the host's tunnel navmesh beside the collision mesh (spec 121 § 4.2 and § 4.3; implemented by
/// the nav crate). The collision mesh never depends on it.
pub trait TunnelNavBake {
    /// The navmesh type.
    type Mesh;
    /// Bakes it, or `None` without a terrain (no surface to link to) or when the bake fails.
    fn bake(&self, sampled: &SampledLayout, terrain: Option<&TerrainSampler>) -> Option<Arc<Self::Mesh>>;
}

/// The [`TunnelNavBake`] of a host that links no navmesh: it bakes nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoNavBake;

impl TunnelNavBake for NoNavBake {
    type Mesh = ();
    fn bake(&self, _sampled: &SampledLayout, _terrain: Option<&TerrainSampler>) -> Option<Arc<()>> {
        None
    }
}

/// A map's tunnel description and its sampled centrelines.
#[derive(Debug, Clone, PartialEq)]
pub struct MapTunnels {
    /// The spline description.
    pub layout: TunnelLayout,
    /// Its sampled centrelines.
    pub sampled: SampledLayout,
}

/// The drawn rubble under a skylight (a `*_skylight` ground patch) as an on-foot prop: its top at
/// `(x, z)`, for the walker's step rule (`pilot_collide::accept_support`), like a deck or a crate
/// on the set-piece grid. It is not part of the surface query: hulls ride the tunnel floor under
/// it. `None` outside every skylight patch. The host's `pilot_floor_y` and the shell's foot
/// planter both read it, so they stand a walker on the same rock.
pub fn skylight_prop_top(terrain: Option<&TerrainSampler>, x: f32, z: f32) -> Option<f32> {
    let terrain = terrain?;
    terrain.shaft_at(x, z).then(|| terrain.height_at_world(x, z))
}

/// A walker's floor with the skylight rubble prop under it: `floor` raised to the prop's top when
/// that top is no more than one step over the feet. There is no step-down limit: a walker who
/// steps off a higher rock lands on the rubble under it, not on the tunnel floor under that (as
/// before T3, when the rubble was a floor candidate of the old query).
pub fn floor_with_skylight_prop(
    terrain: Option<&TerrainSampler>,
    x: f32,
    z: f32,
    feet_y: f32,
    floor: f32,
) -> f32 {
    match skylight_prop_top(terrain, x, z) {
        Some(top) if top <= feet_y + PILOT_STEP_M + 0.02 && top > floor => top,
        _ => floor,
    }
}

/// How far under a camera a mesh floor still counts as the floor it keeps clear of (a camera
/// constant, not a floor choice: the body's own floor is [`TunnelsState::move_floor`]).
pub const CAMERA_FLOOR_REACH_M: f32 = 12.0;

fn span_floor_in_camera_reach(floor_y: f32, y: f32) -> bool {
    floor_y <= y + 0.5 && floor_y >= y - CAMERA_FLOOR_REACH_M
}

/// Radar through rock: a unit underground and one on the
/// surface see each other on radar only inside this range.
pub const ROCK_RADAR_M: f32 = 150.0;

impl<N> TunnelsState<N> {
    /// The tunnels off: no mesh, the heightfield only.
    pub fn disabled() -> Self {
        Self {
            enabled: TUNNELS_DEFAULT,
            mesh: None,
            nav: None,
            layout: None,
            crawlways: Vec::new(),
            navmesh: None,
        }
    }

    /// A hand-built mesh (no layout), its nav overlay baked.
    pub fn with_mesh(enabled: bool, mesh: TunnelCollisionMesh) -> Self {
        let nav = TunnelNavOverlay::bake(&mesh);
        Self {
            enabled,
            mesh: Some(mesh),
            nav: Some(nav),
            layout: None,
            crawlways: Vec::new(),
            navmesh: None,
        }
    }

    /// The map's tunnels, armed from its layout (spec 121 § 4.2 seam for the source's `for_map`: the map
    /// registry is the host's, so the host looks the layout up and passes it here). No navmesh is baked;
    /// see [`Self::for_layout_with_nav`].
    pub fn for_layout(layout: TunnelLayout, terrain: Option<&TerrainSampler>) -> Self {
        Self::from_layout(layout, terrain)
    }

    /// [`Self::for_layout`] with the host's navmesh bake.
    pub fn for_layout_with_nav(
        layout: TunnelLayout,
        terrain: Option<&TerrainSampler>,
        nav: &impl TunnelNavBake<Mesh = N>,
    ) -> Self {
        Self::from_layout_with_nav(layout, terrain, nav)
    }

    /// The tunnels armed from a spline description, sampled over `terrain`. No navmesh is baked; see
    /// [`Self::from_layout_with_nav`].
    pub fn from_layout(layout: TunnelLayout, terrain: Option<&TerrainSampler>) -> Self {
        Self::from_layout_baking(layout, terrain, |_, _| None)
    }

    /// [`Self::from_layout`] with the host's navmesh bake.
    pub fn from_layout_with_nav(
        layout: TunnelLayout,
        terrain: Option<&TerrainSampler>,
        nav: &impl TunnelNavBake<Mesh = N>,
    ) -> Self {
        Self::from_layout_baking(layout, terrain, |sampled, terrain| nav.bake(sampled, terrain))
    }

    fn from_layout_baking(
        layout: TunnelLayout,
        terrain: Option<&TerrainSampler>,
        bake_navmesh: impl FnOnce(&SampledLayout, Option<&TerrainSampler>) -> Option<Arc<N>>,
    ) -> Self {
        let crawlways = layout.crawlways.clone();
        let sampled = layout.sample(terrain);
        // T3: the support floors are welded to the terrain once, here.
        let mesh = TunnelCollisionMesh::new(sampled.triangles(&layout.openings, terrain)).welded(terrain);
        let mut state = Self::with_mesh(true, mesh);
        state.navmesh = bake_navmesh(&sampled, terrain);
        state.layout = Some(Arc::new(MapTunnels { layout, sampled }));
        state.crawlways = crawlways;
        state
    }

    /// The map's tunnels armed from the one baked file (`tunnels.json`,
    /// `do27-tunnels/1`): its `sampled` block builds the collision mesh
    /// without re-sampling the splines, and its `layout` is the description
    /// the pathing layer reads. Byte-identical to [`Self::from_layout`] when
    /// the file is the current export (the one-bake guarantee, proven by
    /// `switchbacks_baked_file_builds_the_byte_identical_sim`). No navmesh is baked; see
    /// [`Self::from_baked_with_nav`].
    pub fn from_baked(file: &TunnelsFile, terrain: &TerrainSampler) -> Self {
        Self::from_baked_baking(file, terrain, |_, _| None)
    }

    /// [`Self::from_baked`] with the host's navmesh bake.
    pub fn from_baked_with_nav(
        file: &TunnelsFile,
        terrain: &TerrainSampler,
        nav: &impl TunnelNavBake<Mesh = N>,
    ) -> Self {
        Self::from_baked_baking(file, terrain, |sampled, terrain| nav.bake(sampled, terrain))
    }

    fn from_baked_baking(
        file: &TunnelsFile,
        terrain: &TerrainSampler,
        bake_navmesh: impl FnOnce(&SampledLayout, Option<&TerrainSampler>) -> Option<Arc<N>>,
    ) -> Self {
        let mesh = file.collision_mesh(terrain).welded(Some(terrain));
        let mut state = Self::with_mesh(true, mesh);
        state.navmesh = bake_navmesh(&file.sampled, Some(terrain));
        state.layout = Some(Arc::new(MapTunnels {
            layout: file.layout.clone(),
            sampled: file.sampled.clone(),
        }));
        state.crawlways = file.layout.crawlways.clone();
        state
    }

    fn armed_mesh(&self) -> Option<&TunnelCollisionMesh> {
        if self.enabled { self.mesh.as_ref() } else { None }
    }

    /// T3, the one query: the surface under the point
    /// `(x, y, z)` and the ceiling over it, from the column of the terrain and every mesh layer
    /// ([`crate::compat::surface`]). Disarmed (flag off or no mesh), the floor is the outdoor
    /// heightfield and there is no ceiling. A pure function of the point: no history.
    pub fn support(&self, terrain: Option<&TerrainSampler>, x: f32, z: f32, y: f32) -> GroundSupport {
        query_ground(terrain, self.armed_mesh(), x, z, y)
    }

    /// [`Self::support`] for a body whose origin (a hull's hover point, a pilot's feet) is at
    /// `body_y`: asked at its waist, [`BODY_REACH_M`] over the origin.
    pub fn body_support(
        &self,
        terrain: Option<&TerrainSampler>,
        x: f32,
        z: f32,
        body_y: f32,
    ) -> GroundSupport {
        self.support(terrain, x, z, body_y + BODY_REACH_M)
    }

    /// The floor under a body whose origin is at `body_y`: [`Self::body_support`]'s floor. Hover
    /// hulls, pilots on foot, shells and the AI all stand on this one answer.
    pub fn move_floor(&self, terrain: Option<&TerrainSampler>, x: f32, z: f32, body_y: f32) -> FloorSample {
        self.body_support(terrain, x, z, body_y).floor
    }

    /// The floor a body is placed on at `(x, z)` with no height of its own yet (a nav beacon, a
    /// landing, a build site): the highest mesh span a hull fits in, else the outdoor ground.
    pub fn place_floor(&self, terrain: Option<&TerrainSampler>, x: f32, z: f32) -> FloorSample {
        let outdoor = sample_floor(terrain, x, z, 0.0, FloorSurface::Heightfield);
        let Some(mesh) = self.armed_mesh() else {
            return outdoor;
        };
        column(terrain, Some(mesh), x, z)
            .highest_mesh_span(HULL_FIT_M)
            .map_or(outdoor, |span| span.floor)
    }

    /// The sim floor nearest height `y` at `(x, z)`, over every layer: the terrain or a mesh floor.
    /// The set-piece grid uses it to tell a drawn floor that is the sim's own from a deck over it.
    pub fn nearest_floor(&self, terrain: Option<&TerrainSampler>, x: f32, z: f32, y: f32) -> f32 {
        let col = column(terrain, self.armed_mesh(), x, z);
        let mut best = col.spans()[0].floor.point[1];
        for span in col.spans() {
            let fy = span.floor.point[1];
            if (fy - y).abs() < (best - y).abs() {
                best = fy;
            }
        }
        best
    }

    /// Walls and lid for a body at `pos` standing on `floor_y`, with a disc of `radius`: in a mesh
    /// span (a covered gallery or an open cut) the walls at its own height push its disc out, and
    /// the lid of its span caps its centre [`UNIT_COLLISION_RADIUS_M`] under the rock. `None` when
    /// the body is on open ground or the tunnels are off. Host and client both run it, so the
    /// predicted hull meets the same rock as the host's.
    pub fn confine(
        &self,
        terrain: Option<&TerrainSampler>,
        pos: [f32; 3],
        floor_y: f32,
        radius: f32,
    ) -> Option<[f32; 3]> {
        let mesh = self.armed_mesh()?;
        let span = self.body_support(terrain, pos[0], pos[2], pos[1]);
        if span.floor.surface != FloorSurface::TunnelFloor && span.ceiling.is_none() {
            return None;
        }
        let radius = det_clamp(radius, 0.3, 6.0);
        let [x, z] = separate_disc_from_walls_band(
            mesh,
            pos[0],
            pos[2],
            radius,
            floor_y + 0.5,
            det_max(pos[1], floor_y) + 2.0,
        );
        // The lid of the span the body is in where the walls left it. A body over the lid (falling
        // in through a skylight shaft, where there is no ground over the tube) is not pulled under it.
        let y = match self.body_support(terrain, x, z, pos[1]).ceiling {
            Some(lid) if pos[1] <= lid => det_max(det_min(pos[1], lid - UNIT_COLLISION_RADIUS_M), floor_y),
            _ => pos[1],
        };
        Some([x, y, z])
    }

    /// True when the body at `pos` is in a covered gallery: in a mesh span with rock over it.
    pub fn underground(&self, pos: [f32; 3]) -> bool {
        let Some(mesh) = self.armed_mesh() else {
            return false;
        };
        let waist = pos[1] + BODY_REACH_M;
        column(None, Some(mesh), pos[0], pos[2])
            .spans()
            .iter()
            .any(|span| {
                span.floor.surface == FloorSurface::TunnelFloor
                    && span.floor.point[1] <= waist
                    && span.ceiling.is_some_and(|lid| lid > pos[1])
            })
    }

    /// A body whose feet are at `feet` is in the tunnel system below the outdoor ground: in a covered
    /// gallery, or in an open cut under the ground line. The outdoor surface over it is not its floor
    /// and not the camera's.
    pub fn below_ground_at(&self, terrain: Option<&TerrainSampler>, feet: [f32; 3]) -> bool {
        if self.armed_mesh().is_none() {
            return false;
        }
        let span = self.body_support(terrain, feet[0], feet[2], feet[1]);
        if span.ceiling.is_some() {
            return true;
        }
        let ground = sample_floor(terrain, feet[0], feet[2], feet[1], FloorSurface::Heightfield).point[1];
        span.floor.surface == FloorSurface::TunnelFloor && feet[1] < ground - UNDERGROUND_M
    }

    /// The mesh floor a camera at `(x, y, z)` keeps clear of: the highest one at or under it (half a
    /// metre of grace for an eye already sunk into it) and no more than [`CAMERA_FLOOR_REACH_M`]
    /// below it, so a camera on the plain is not held by the tube 30 m under it. `None` when there
    /// is none there, or the tunnels are off.
    pub fn camera_floor(&self, x: f32, z: f32, y: f32) -> Option<f32> {
        let mesh = self.armed_mesh()?;
        column(None, Some(mesh), x, z)
            .spans()
            .iter()
            .rev()
            .filter(|span| span.floor.surface == FloorSurface::TunnelFloor)
            .map(|span| span.floor.point[1])
            .find(|fy| span_floor_in_camera_reach(*fy, y))
    }

    /// The mesh spans at `(x, z)`, bottom up, as `(floor, lid)`: what the camera tests an eye
    /// against. Empty off the mesh layers or with the tunnels off. Allocates (camera only, never a
    /// tick path).
    pub fn mesh_spans(&self, x: f32, z: f32) -> Vec<(f32, Option<f32>)> {
        let Some(mesh) = self.armed_mesh() else {
            return Vec::new();
        };
        column(None, Some(mesh), x, z)
            .spans()
            .iter()
            .filter(|span| span.floor.surface == FloorSurface::TunnelFloor)
            .map(|span| (span.floor.point[1], span.ceiling))
            .collect()
    }

    /// Pull a camera eye in along `pivot -> eye` until it is `margin` clear of the walls and lids
    /// between them. Floors are not tested here. Unchanged when the tunnels are off or the way is clear.
    pub fn camera_clip(&self, pivot: [f32; 3], eye: [f32; 3], margin: f32) -> [f32; 3] {
        let Some(mesh) = self.armed_mesh() else {
            return eye;
        };
        let Some(t) = mesh.segment_blocker(pivot, eye) else {
            return eye;
        };
        let d = [eye[0] - pivot[0], eye[1] - pivot[1], eye[2] - pivot[2]];
        let len = math::sqrt(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]);
        let keep = det_max(t * len - margin, 0.0) / det_max(len, 1e-6);
        [
            pivot[0] + d[0] * keep,
            pivot[1] + d[1] * keep,
            pivot[2] + d[2] * keep,
        ]
    }

    /// Fraction (0..1) along `from -> to` where the segment first crosses covered rock: a tunnel lid, or a wall
    /// with a lid over the hit (rock between a body on the canyon floor and one in the tube under it). The walls
    /// of an open cut do not count: the drawn trench is 2-4x wider than the sim's, so a shot out of a mouth
    /// trench must not stop on them. `None` when the way is clear or the tunnels are off.
    pub fn rock_between(&self, from: [f32; 3], to: [f32; 3]) -> Option<f32> {
        let mesh = self.armed_mesh()?;
        let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        let mut start = 0.0f32;
        for _ in 0..4 {
            let a = [
                from[0] + d[0] * start,
                from[1] + d[1] * start,
                from[2] + d[2] * start,
            ];
            let t = start + (1.0 - start) * mesh.segment_blocker(a, to)?;
            let hit = [from[0] + d[0] * t, from[1] + d[1] * t, from[2] + d[2] * t];
            if mesh.ceiling_above(hit[0], hit[2], hit[1] - 0.1).is_some() {
                return Some(t);
            }
            start = det_min(t + 1.0e-3, 1.0);
        }
        None
    }

    /// Rock between two bodies hides one from the other's radar: exactly one of them is in a
    /// covered gallery and they are more than [`ROCK_RADAR_M`] apart. Allies are never hidden
    /// (the caller checks the teams).
    pub fn rock_hidden(&self, viewer: [f32; 3], target: [f32; 3]) -> bool {
        self.rock_hidden_lit(viewer, target, false, ROCK_RADAR_M)
    }

    /// [`Self::rock_hidden`], honoring a lit target (lamps, a flare, a live
    /// ping). Light shows through rock at `rock_radar_lit_m` (the host's lit range; spec 121 § 4.2 seam)
    /// instead of [`ROCK_RADAR_M`]. The lit bit comes from the host's light rules.
    pub fn rock_hidden_lit(
        &self,
        viewer: [f32; 3],
        target: [f32; 3],
        target_lit: bool,
        rock_radar_lit_m: f32,
    ) -> bool {
        if self.armed_mesh().is_none() {
            return false;
        }
        if self.underground(viewer) == self.underground(target) {
            return false;
        }
        let range = if target_lit {
            rock_radar_lit_m
        } else {
            ROCK_RADAR_M
        };
        let dx = viewer[0] - target[0];
        let dy = viewer[1] - target[1];
        let dz = viewer[2] - target[2];
        dx * dx + dy * dy + dz * dz > range * range
    }

    /// Push the hull disc out of gallery walls. Returns the XZ center.
    /// Disabled tunnels leave the center where it is.
    pub fn separate_from_walls(&self, x: f32, z: f32) -> [f32; 2] {
        if !self.enabled {
            return [x, z];
        }
        let Some(mesh) = self.mesh.as_ref() else {
            return [x, z];
        };
        separate_disc_from_walls(mesh, x, z, UNIT_COLLISION_RADIUS_M)
    }

    /// Keep the hull disc under the ceiling of the volume it is inside.
    pub fn clamp_altitude(&self, x: f32, y: f32, z: f32) -> f32 {
        if !self.enabled {
            return y;
        }
        clamp_altitude_under_ceiling(self.mesh.as_ref(), x, y, z, UNIT_COLLISION_RADIUS_M)
    }

    /// Next nav waypoint, or `None` when the overlay has no route.
    pub fn next_waypoint(&self, from: [f32; 3], goal: [f32; 3]) -> Option<[f32; 3]> {
        self.next_waypoint_for(from, goal, true)
    }

    /// `pilot` false skips building interiors. Tunnel decks stay available.
    pub fn next_waypoint_for(&self, from: [f32; 3], goal: [f32; 3], pilot: bool) -> Option<[f32; 3]> {
        if !self.enabled {
            return None;
        }
        self.nav.as_ref()?.next_waypoint_for(from, goal, pilot)
    }

    /// Deck under `pos` (0 = highest, 3 = fourth deck), when standing on one.
    pub fn depth_at(&self, pos: [f32; 3]) -> Option<u8> {
        if !self.enabled {
            return None;
        }
        self.nav.as_ref()?.depth_at(pos)
    }
}

/// Floor for place, slope and nav drops at `(x, z)`, a point with no height of its own: the
/// heightfield, or on a map with mesh layers the highest mesh span a hull fits in (T3,
/// [`TunnelsState::place_floor`]).
pub fn sample_place_floor<N>(
    tunnels: &TunnelsState<N>,
    terrain: Option<&TerrainSampler>,
    x: f32,
    z: f32,
) -> FloorSample {
    tunnels.place_floor(terrain, x, z)
}

/// Floor y for beacon drops and landings: [`sample_place_floor`]'s height.
pub fn nav_floor_y<N>(tunnels: &TunnelsState<N>, terrain: Option<&TerrainSampler>, x: f32, z: f32) -> f32 {
    sample_place_floor(tunnels, terrain, x, z).point[1]
}

/// Test fixtures for the T3 ground query: the real maps carry baked mouth patches
/// that sink the outdoor ground into each open cut, because the
/// stateless query takes the highest surface at or under the body. A fixture that
/// builds a bare heightfield over a tunnel layout has to carve the same trench.
#[cfg(test)]
pub(crate) mod test_support {
    #![expect(
        clippy::disallowed_methods,
        reason = "test fixture arithmetic (DO27's helper, copied as-is); never a tick path (ADR-8167)"
    )]
    use super::TunnelsState;
    use crate::compat::sampler::TerrainSampler;
    use crate::compat::terrain_patch::TerrainPatch;

    /// `terrain` with each listed ramp's open cut (the segments with no rock over them)
    /// carved to the ramp floor, on 0.5 m cells, the way a baked mouth patch is.
    pub(crate) fn carve_open_cuts<N>(
        terrain: TerrainSampler,
        tunnels: &TunnelsState<N>,
        ramps: &[&str],
    ) -> TerrainSampler {
        let map = tunnels.layout.as_ref().expect("a layout");
        let mut patches = Vec::new();
        for id in ramps {
            let passage = map.sampled.passage(id).unwrap_or_else(|| panic!("{id} passage"));
            let half = passage.width_m * 0.5;
            let cuts: Vec<usize> = (0..passage.samples.len().saturating_sub(1))
                .filter(|i| !passage.covered.get(*i).copied().unwrap_or(true))
                .collect();
            if cuts.is_empty() {
                continue;
            }
            let pts: Vec<[f32; 3]> = cuts
                .iter()
                .flat_map(|i| [passage.samples[*i].p, passage.samples[*i + 1].p])
                .collect();
            let (mut x0, mut x1, mut z0, mut z1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
            for p in &pts {
                x0 = x0.min(p[0]);
                x1 = x1.max(p[0]);
                z0 = z0.min(p[2]);
                z1 = z1.max(p[2]);
            }
            let spacing = 0.5f32;
            let origin = [x0 - half - 1.0, z0 - half - 1.0];
            let cols = ((x1 + half + 1.0 - origin[0]) / spacing).ceil() as usize + 1;
            let rows = ((z1 + half + 1.0 - origin[1]) / spacing).ceil() as usize + 1;
            let mut heights = Vec::with_capacity(cols * rows);
            for r in 0..rows {
                for c in 0..cols {
                    let (x, z) = (origin[0] + c as f32 * spacing, origin[1] + r as f32 * spacing);
                    // The nearest ramp segment decides: only a cell nearest an open-cut segment is carved,
                    // so the covered ramp keeps its rock (and its lid) over it.
                    let mut best: Option<(f32, f32, bool)> = None;
                    for i in 0..passage.samples.len() - 1 {
                        let (a, b) = (passage.samples[i].p, passage.samples[i + 1].p);
                        let (dx, dz) = (b[0] - a[0], b[2] - a[2]);
                        let len2 = (dx * dx + dz * dz).max(1e-6);
                        let t = (((x - a[0]) * dx + (z - a[2]) * dz) / len2).clamp(0.0, 1.0);
                        let (px, pz) = (a[0] + dx * t, a[2] + dz * t);
                        let d = ((x - px).powi(2) + (z - pz).powi(2)).sqrt();
                        if d <= half && best.is_none_or(|(bd, _, _)| d < bd) {
                            best = Some((
                                d,
                                a[1] + (b[1] - a[1]) * t,
                                !passage.covered.get(i).copied().unwrap_or(true),
                            ));
                        }
                    }
                    let base = terrain.height_at_world(x, z);
                    heights.push(match best {
                        Some((_, y, true)) => y.min(base),
                        _ => base,
                    });
                }
            }
            patches.push(TerrainPatch {
                id: format!("{id}_cut"),
                origin,
                spacing,
                cols,
                rows,
                heights,
            });
        }
        terrain.with_patches(patches)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![expect(
        clippy::disallowed_methods,
        reason = "test probe arithmetic; DO27's asserts are kept as copied (ADR-8167)"
    )]
    use super::*;
    use crate::compat::mesh::synthetic_gallery;
    use crate::compat::sampler::HeightGrid;

    /// The tests run without a navmesh (spec 121 § 4.2: the bake is the nav crate's).
    type TunnelsState = super::TunnelsState<()>;

    /// The host's lit radar range the source passes (`ROCK_RADAR_LIT_M`, 300 m).
    const ROCK_RADAR_LIT_M: f32 = 300.0;

    /// The source's `TunnelsState::for_map`: the map registry is the host's (spec 121 § 4.2), so the
    /// tests look the layout up in the fixtures.
    fn for_map(map_id: &str, terrain: Option<&TerrainSampler>) -> Option<TunnelsState> {
        crate::compat::fixtures::for_map(map_id).map(|layout| TunnelsState::for_layout(layout, terrain))
    }

    #[test]
    #[expect(
        clippy::assertions_on_constants,
        reason = "the source's assert, kept as copied (ADR-8167)"
    )]
    fn tunnels_flag_defaults_off() {
        assert!(!TUNNELS_DEFAULT);
        let state = TunnelsState::disabled();
        assert!(!state.enabled);
        let terrain = TerrainSampler::new(HeightGrid::square(2, vec![0, 0, 0, 0]).expect("grid"), 20.0);
        let mesh = synthetic_gallery(5.0, 5.0, -3.0, 1.0, true);
        let state_with_mesh = TunnelsState {
            enabled: false,
            mesh: Some(mesh),
            nav: None,
            layout: None,
            crawlways: Vec::new(),
            navmesh: None,
        };
        let sample = state_with_mesh.move_floor(Some(&terrain), 0.0, 0.0, -1.0);
        assert_eq!(sample.surface, FloorSurface::Heightfield);
    }

    #[test]
    fn tunnels_arm_active_requires_flag_and_mesh() {
        let mesh = synthetic_gallery(5.0, 5.0, 0.0, 4.0, false);
        assert!(!tunnels_arm_active(false, None));
        assert!(!tunnels_arm_active(false, Some(&mesh)));
        assert!(!tunnels_arm_active(true, None));
        assert!(tunnels_arm_active(true, Some(&mesh)));
        assert!(!tunnels_arm_active(TUNNELS_DEFAULT, Some(&mesh)));
    }

    /// The sim floor is the one content query, asked at the body's waist.
    #[test]
    fn move_floor_is_the_one_content_ground_query() {
        let mesh = synthetic_gallery(20.0, 20.0, 0.0, 5.0, false);
        let state = TunnelsState::with_mesh(true, mesh);
        let direct = crate::compat::ground::query_ground(None, state.mesh.as_ref(), 0.0, 0.0, 1.0);
        assert_eq!(state.support(None, 0.0, 0.0, 1.0), direct);
        let waist =
            crate::compat::ground::query_ground(None, state.mesh.as_ref(), 0.0, 0.0, 1.0 + BODY_REACH_M);
        assert_eq!(state.body_support(None, 0.0, 0.0, 1.0), waist);
        assert_eq!(state.move_floor(None, 0.0, 0.0, 1.0), waist.floor);
        assert_eq!(
            crate::compat::surface::surface_under(None, state.mesh.as_ref(), [0.0, 1.0, 0.0]),
            direct.floor,
            "surface_under is the same query"
        );
        assert_eq!(
            crate::compat::surface::ceiling_over(None, state.mesh.as_ref(), [0.0, 1.0, 0.0]),
            direct.ceiling
        );
    }

    #[test]
    fn lit_targets_show_through_rock_twice_as_far() {
        let mesh = synthetic_gallery(5.0, 5.0, 0.0, 4.0, false);
        let state = TunnelsState::with_mesh(true, mesh);
        let viewer = [0.0, 1.0, 0.0];
        assert!(state.underground(viewer), "the viewer is in the gallery");
        let target = [200.0, 1.0, 0.0];
        assert!(!state.underground(target), "the target is on the surface");
        assert!(state.rock_hidden(viewer, target), "200 m past the 150 m rule");
        assert!(
            state.rock_hidden_lit(viewer, target, false, ROCK_RADAR_LIT_M),
            "unlit matches the old rule"
        );
        assert!(
            !state.rock_hidden_lit(viewer, target, true, ROCK_RADAR_LIT_M),
            "lit shows through rock at 200 m"
        );
        let far = [350.0, 1.0, 0.0];
        assert!(
            state.rock_hidden_lit(viewer, far, true, ROCK_RADAR_LIT_M),
            "past even the lit rule"
        );
    }

    #[test]
    fn pathing_no_portal_holds() {
        let mesh = synthetic_gallery(5.0, 5.0, 0.0, 4.0, false);
        let state = TunnelsState::with_mesh(true, mesh);
        assert!(state.next_waypoint([0.0, 0.0, 0.0], [10.0, 0.0, 10.0]).is_none());
    }

    /// T3: the floor is a pure function of the body position. Asking the same
    /// point again, in any order and after any other query, answers bit for bit
    /// the same — host, client predictor and replay can never disagree.
    #[test]
    fn support_is_a_pure_function_of_the_point() {
        let state = for_map("switchbacks", None).expect("switchbacks layout");
        let spots = [
            (-170.0, -256.0, 130.0), // west mouth lip, open cut
            (-175.0, -270.0, 111.0), // west ramp, under the lid
            (-300.0, -284.0, 150.0), // tube floor
            (300.0, -256.0, 300.0),  // plain, no tunnel anywhere near
        ];
        let first: Vec<_> = spots
            .iter()
            .map(|(x, y, z)| state.body_support(None, *x, *z, *y))
            .collect();
        for _ in 0..3 {
            for (i, (x, y, z)) in spots.iter().enumerate().rev() {
                let again = state.body_support(None, *x, *z, *y);
                assert_eq!(
                    again.floor.point[1].to_bits(),
                    first[i].floor.point[1].to_bits(),
                    "({x}, {y}, {z})"
                );
                assert_eq!(again, first[i], "({x}, {y}, {z})");
            }
        }
    }

    /// T3: the query adds no step of its own. Walking every mouth ramp from
    /// the top into the tube in fine spatial steps, the floor it answers is
    /// the ramp mesh's own wherever the mesh has one floor there. A discrete terrain/tunnel
    /// switch — the old 0.55-0.91 m mouth hops — reads far over it.
    #[test]
    fn ramp_query_adds_no_step_of_its_own() {
        let state = for_map("switchbacks", None).expect("switchbacks layout");
        let map = state.layout.as_ref().expect("layout");
        assert_eq!(map.layout.openings.len(), 4, "four mouths");
        for opening in &map.layout.openings {
            let ramp = map
                .sampled
                .passage(&opening.ramp)
                .unwrap_or_else(|| panic!("{} ramp", opening.id));
            // Fine spatial steps along the centreline, top into the tube.
            let mut pts: Vec<[f32; 2]> = Vec::new();
            let rev: Vec<[f32; 3]> = ramp.samples.iter().rev().map(|s| s.p).collect();
            for w in rev.windows(2) {
                let (a, b) = (w[0], w[1]);
                let (dx, dz) = (b[0] - a[0], b[2] - a[2]);
                let dist = (dx * dx + dz * dz).sqrt();
                let n = (dist / 0.5).ceil().max(1.0) as usize;
                for k in 0..n {
                    let f = k as f32 / n as f32;
                    pts.push([a[0] + dx * f, a[2] + dz * f]);
                }
            }
            let last = rev.last().expect("ramp foot");
            pts.push([last[0], last[2]]);
            let mut body_y = opening.top[1];
            let mut px = pts[0][0];
            let mut pz = pts[0][1];
            let mut worst = 0.0f32;
            for [x, z] in pts {
                let floor = state.move_floor(None, x, z, body_y).point[1];
                let step = (floor - body_y).abs();
                let travel = ((x - px) * (x - px) + (z - pz) * (z - pz)).sqrt();
                // Where the ramp mesh is the only floor, the query's answer IS the mesh floor: a
                // terrain/tunnel switch or a history rule would show as a difference. (The step per
                // 0.5 m is the ramp's own slope, which a curved switchback ramp makes steeper than
                // its centreline chords; magnitude alone cannot tell it from a hop.)
                let mesh_floors = state.mesh.as_ref().expect("armed mesh").floors_at(x, z);
                if let [only] = mesh_floors.as_slice() {
                    assert!(
                        (floor - only.point[1]).abs() < 1e-3,
                        "{}: the query answered {floor:.3} where the ramp mesh is {:.3} ({travel:.3} m of travel; the query adds its own step)",
                        opening.id,
                        only.point[1]
                    );
                }
                worst = worst.max(step);
                body_y = floor;
                px = x;
                pz = z;
            }
            assert!(
                (body_y - (-286.0)).abs() < 3.0,
                "{} ramp ends at {body_y}, not the tube floor",
                opening.id
            );
            let _ = worst;
        }
    }

    /// T3 done-criterion at the mouths: crossing the lip (the ramp cut onto
    /// the plain) answers one continuous surface — worst step under 0.2 m
    /// per 0.25 m stride (the ramp's own slope is 0.13 m of that).
    #[test]
    fn mouth_lip_step_under_two_decimetres() {
        let terrain = canyon();
        let state = for_map("switchbacks", Some(&terrain)).expect("switchbacks layout");
        let map = state.layout.as_ref().expect("layout");
        for opening in &map.layout.openings {
            let ramp = map.sampled.passage(&opening.ramp).expect("ramp");
            let n = ramp.samples.len();
            let (top, back) = (ramp.samples[n - 1].p, ramp.samples[n - 2].p);
            let mut d = [top[0] - back[0], top[2] - back[2]];
            let len = (d[0] * d[0] + d[1] * d[1]).sqrt().max(1e-6);
            d = [d[0] / len, d[1] / len];
            // Start on the floor 6 m down the cut (the open ramp floor cuts the ground over it), then
            // walk up over the lip onto the plain.
            let mut body_y = state
                .move_floor(
                    Some(&terrain),
                    top[0] - d[0] * 6.0,
                    top[2] - d[1] * 6.0,
                    opening.top[1],
                )
                .point[1];
            assert!(
                body_y < opening.top[1] - 1.0,
                "{}: 6 m down the cut is in the cut ({body_y})",
                opening.id
            );
            let mut worst = 0.0f32;
            let mut k = -24;
            while k <= 24 {
                let x = top[0] + d[0] * k as f32 * 0.25;
                let z = top[2] + d[1] * k as f32 * 0.25;
                let floor = state.move_floor(Some(&terrain), x, z, body_y).point[1];
                worst = worst.max((floor - body_y).abs());
                body_y = floor;
                k += 1;
            }
            assert!(
                worst < 0.2,
                "{} lip: worst step {worst:.3} m over the mouth (T3: under 0.2 m)",
                opening.id
            );
        }
    }

    /// Hop in and out on foot and in a hull: a hop-out ring round a
    /// body in the gallery stays on the gallery floor (the property the
    /// host's same-floor check needs), with the lid paired over it.
    #[test]
    fn hop_out_ring_round_gallery_body_stays_on_gallery_floor() {
        let state = for_map("switchbacks", None).expect("switchbacks layout");
        // A flat stretch of the tube (floor -286, lid -274).
        let (cx, cz, gallery) = (-300.0f32, 150.0f32, -286.0f32);
        for radius in [0.35f32, 2.5, 4.5] {
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                let (x, z) = (cx + radius * a.cos(), cz + radius * a.sin());
                let support = state.body_support(None, x, z, gallery);
                assert_eq!(
                    support.floor.surface,
                    FloorSurface::TunnelFloor,
                    "H stand at r={radius} k={k} left the gallery: {:?}",
                    support.floor
                );
                assert!(
                    (support.floor.point[1] - gallery).abs() < 2.0,
                    "H stand at r={radius} k={k}: {:?}",
                    support.floor
                );
            }
        }
        let lid = state
            .body_support(None, cx, cz, gallery)
            .ceiling
            .expect("lid over the tube");
        assert!((lid - (-274.0)).abs() < 1.5, "lid {lid}");
    }

    /// T3, projectiles: a shell inside the tube reads the tube floor (the
    /// mortar ground burst compares against this, not the canyon floor
    /// 30 m over it).
    #[test]
    fn shell_inside_the_tube_reads_the_tube_floor() {
        let terrain = canyon();
        let state = for_map("switchbacks", Some(&terrain)).expect("switchbacks layout");
        let floor = state.move_floor(Some(&terrain), -300.0, 150.0, -280.0);
        assert_eq!(floor.surface, FloorSurface::TunnelFloor);
        assert!(
            (floor.point[1] - (-286.0)).abs() < 0.5,
            "tube floor, got {floor:?}"
        );
    }

    #[test]
    fn altitude_clamp_uses_unit_collision_radius() {
        let mesh = synthetic_gallery(5.0, 5.0, 0.0, 6.0, false);
        let state = TunnelsState::with_mesh(true, mesh);
        let y = state.clamp_altitude(0.0, 20.0, 0.0);
        assert!((y - (6.0 - UNIT_COLLISION_RADIUS_M)).abs() < 1e-5);
    }

    /// A hull or pilot walking the centreline of every Switchbacks ramp descends
    /// from the surface opening (-256 m) into the tube floor (-286 m). This is the
    /// authoritative floor (`move_floor`) that units stand on; the open-cut ramp is
    /// a real trench to the sim even where the visual terrain used to be flat.
    /// Proves the physics half of "drive/walk down each ramp into the tube".
    #[test]
    fn a_body_walks_down_each_ramp_into_the_tube() {
        let state = for_map("switchbacks", None).expect("switchbacks layout");
        let map = state.layout.as_ref().expect("layout");
        assert_eq!(map.layout.openings.len(), 4, "four mouths");
        for opening in &map.layout.openings {
            let ramp = map
                .sampled
                .passage(&opening.ramp)
                .unwrap_or_else(|| panic!("{} ramp", opening.id));
            let mut body_y = opening.top[1]; // surface at the mouth
            let mut floors = Vec::with_capacity(ramp.samples.len());
            // Walk the centreline from the top sample back into the tube.
            for s in ramp.samples.iter().rev() {
                let f = state.move_floor(None, s.p[0], s.p[2], body_y).point[1];
                floors.push(f);
                body_y = f;
            }
            let top = *floors.first().expect("top");
            let bottom = *floors.last().expect("bottom");
            assert!(
                (top - opening.top[1]).abs() < 2.0,
                "{} ramp top {top} vs surface {}",
                opening.id,
                opening.top[1]
            );
            assert!(
                (bottom - (-286.0)).abs() < 3.0,
                "{} ramp ends at {bottom}, not the tube floor",
                opening.id
            );
            // A monotonic descent: the floor never climbs back toward the surface.
            for w in floors.windows(2) {
                assert!(w[1] <= w[0] + 0.75, "{} ramp climbed {:?}", opening.id, w);
            }
        }
    }

    /// The canyon floor, flat at -256 m, as the sim heightfield sees it.
    fn canyon() -> TerrainSampler {
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).expect("grid"), 1280.0)
            .with_zero_counts(16384)
    }

    /// A pilot on the west ramp or in the tube is below the outdoor ground, and the camera behind
    /// them is held by the tube's walls: the chase eye never leaves the rock to sit on the plain.
    #[test]
    fn a_pilot_in_the_ramp_is_below_ground_and_the_camera_is_clipped_by_the_walls() {
        let terrain = canyon();
        let state = for_map("switchbacks", Some(&terrain)).expect("switchbacks layout");
        // The baked mouth patch sinks the ground into the open cut (T3 takes the highest surface).
        let terrain = test_support::carve_open_cuts(terrain, &state, &["ramp_west"]);
        let map = state.layout.as_ref().expect("layout");
        let ramp = map.sampled.passage("ramp_west").expect("west ramp");
        let mut prev: Option<f32> = None;
        let mut deep = 0;
        for s in ramp.samples.iter().rev() {
            let f = state.move_floor(Some(&terrain), s.p[0], s.p[2], prev.unwrap_or(-256.0));
            prev = Some(f.point[1]);
            let feet = [s.p[0], f.point[1], s.p[2]];
            let below = state.below_ground_at(Some(&terrain), feet);
            if f.point[1] < -258.0 {
                // T3: the open cut is the carved ground itself (the baked mouth patch), so there
                // the feet stand on the outdoor surface; under a lid they are below ground.
                let lidded = state
                    .mesh
                    .as_ref()
                    .and_then(|m| m.ceiling_above(feet[0], feet[2], feet[1]))
                    .is_some();
                if lidded {
                    assert!(below, "feet {feet:?} are in the covered ramp, not on the plain");
                }
                deep += 1;
            }
            // Straight up and 9 m back along the surface: never leaves the rock unclipped inside the
            // covered part of the tube (lid over the body), and the clipped eye is always nearer.
            let pivot = [feet[0], feet[1] + 1.5, feet[2]];
            let up = [pivot[0], pivot[1] + 40.0, pivot[2]];
            let eye = state.camera_clip(pivot, up, 0.3);
            let covered = state
                .mesh
                .as_ref()
                .and_then(|m| m.ceiling_above(feet[0], feet[2], feet[1]))
                .is_some();
            if covered {
                assert!(
                    eye[1] < pivot[1] + 30.0,
                    "the lid over {feet:?} holds the camera, got {eye:?}"
                );
            }
            let dist = |a: [f32; 3], b: [f32; 3]| {
                ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
            };
            assert!(dist(pivot, eye) <= dist(pivot, up) + 1e-3);
        }
        assert!(deep > 6, "the west ramp has a deep part ({deep})");
        // On the plain beside the system there is no tunnel floor under a body.
        assert!(!state.below_ground_at(Some(&terrain), [-300.0, -256.0, 130.0]));
    }

    /// T2: a body walks over the west mouth rim, from the plain across the cap
    /// and down the portal floors into the cut, with no vertical jump: the
    /// modeled lip joins the trench to the open ground (no seam, no gap).
    /// The hole mask is on the sampler, the way the loader bakes it.
    #[test]
    fn t2_a_body_walks_over_the_mouth_rim_without_a_jump() {
        let plain = canyon();
        let layout = crate::compat::fixtures::switchbacks();
        let holes = layout.sample(Some(&plain)).mouth_holes(&layout.openings);
        assert!(!holes.is_empty(), "the mouths cut holes");
        let terrain = plain.with_holes(holes);
        let state = TunnelsState::from_layout(layout, Some(&terrain));
        let map = state.layout.as_ref().expect("layout");
        let ramp = map.sampled.passage("ramp_west").expect("west ramp");
        let n = ramp.samples.len();
        let (top, below) = (ramp.samples[n - 1].p, ramp.samples[n - 2].p);
        assert!(terrain.hole_at(top[0], top[2]), "the mouth top is holed");
        let (dx, dz) = (top[0] - below[0], top[2] - below[2]);
        let dl = (dx * dx + dz * dz).sqrt().max(1e-3);
        // 1 m strides from the plain 15 m past the mouth down the ramp.
        let mut coarse = Vec::new();
        let mut lead = [top[0] + dx / dl * 15.0, top[2] + dz / dl * 15.0];
        while (lead[0] - top[0]).powi(2) + (lead[1] - top[2]).powi(2) > 1.0 {
            coarse.push(lead);
            lead = [lead[0] - dx / dl, lead[1] - dz / dl];
        }
        // The first 8 ramp samples: the rim, the lip and the open cut. Past them the
        // ramp bends inside covered ground, which is not this test's subject.
        for s in ramp.samples.iter().rev().take(8) {
            coarse.push([s.p[0], s.p[2]]);
        }
        let mut route: Vec<[f32; 2]> = Vec::new();
        for w in coarse.windows(2) {
            let (a, b) = (w[0], w[1]);
            let seg = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            let steps = (seg / 1.0).ceil().max(1.0) as usize;
            for k in 0..steps {
                let f = k as f32 / steps as f32;
                route.push([a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f]);
            }
        }
        route.push(*coarse.last().expect("route"));
        let mut body_y = -256.0f32;
        let mut prev: Option<f32> = None;
        let mut worst = 0.0f32;
        for (i, xz) in route.iter().enumerate() {
            let f = state.move_floor(Some(&terrain), xz[0], xz[1], body_y);
            let dy = prev.map(|p| (f.point[1] - p).abs()).unwrap_or(0.0);
            worst = worst.max(dy);
            assert!(
                dy <= 1.0,
                "step {i} at ({:.1}, {:.1}) jumps {dy:.2} m",
                xz[0],
                xz[1]
            );
            body_y = f.point[1];
            prev = Some(f.point[1]);
        }
        let end = prev.expect("walked");
        assert!(end < -262.0, "ends down in the cut, not on the plain: {end}");
        assert!(worst <= 1.0);
    }

    /// A camera whose way to the eye is clear is not moved.
    #[test]
    fn a_clear_camera_ray_is_untouched() {
        let state = for_map("switchbacks", Some(&canyon())).expect("switchbacks layout");
        let pivot = [300.0, -254.0, 300.0];
        let eye = [305.0, -253.0, 300.0];
        assert_eq!(state.camera_clip(pivot, eye, 0.3), eye);
        assert_eq!(TunnelsState::disabled().camera_clip(pivot, eye, 0.3), eye);
    }

    /// A 1.8 m drawn cone, 6 m across, on a flat stretch of the tube (floor -286, lid -274), as a
    /// baked 0.5 m patch named `id`, over the flat canyon floor.
    fn canyon_with_cone(id: &str) -> (TerrainSampler, f32, f32) {
        let (cx, cz) = (-300.0f32, 150.0f32);
        let (cols, rows) = (25usize, 25usize);
        let origin = [cx - 6.0, cz - 6.0];
        let heights = (0..rows * cols)
            .map(|i| {
                let (c, r) = ((i % cols) as f32, (i / cols) as f32);
                let d = ((origin[0] + c * 0.5 - cx).powi(2) + (origin[1] + r * 0.5 - cz).powi(2)).sqrt();
                -286.0 + 1.8 * (1.0 - (d / 6.0).min(1.0))
            })
            .collect();
        let patch = crate::compat::terrain_patch::TerrainPatch {
            id: id.into(),
            origin,
            spacing: 0.5,
            cols,
            rows,
            heights,
        };
        (canyon().with_patches(vec![patch]), cx, cz)
    }

    /// T3: a drawn patch that dips into a covered span (a mouth funnel under the sim lid) is not a
    /// surface. The tube is carved space: its floor is the floor, the lid still caps it, and the
    /// canyon floor over it is intact.
    #[test]
    fn a_drawn_patch_inside_the_tube_does_not_move_its_floor() {
        let (terrain, cx, cz) = canyon_with_cone("west");
        let tunnels = for_map("switchbacks", Some(&terrain)).expect("layout");
        let mut floor = -286.0f32;
        for k in 0..=24 {
            let x = cx - 6.0 + k as f32 * 0.5;
            let f = tunnels.move_floor(Some(&terrain), x, cz, floor);
            assert_eq!(f.surface, FloorSurface::TunnelFloor, "step {k}: {f:?}");
            assert!((f.point[1] + 286.0).abs() < 0.05, "step {k}: {f:?}");
            floor = f.point[1];
        }
        let f = tunnels.move_floor(Some(&terrain), cx, cz, -281.0);
        assert!(
            (f.point[1] + 286.0).abs() < 0.05,
            "from 5 m up, the tube floor: {f:?}"
        );
        // A body risen to just under the lid (-274) is held a hull radius under it.
        let capped = tunnels
            .confine(Some(&terrain), [cx, -274.5, cz], floor, 0.8)
            .expect("confined in the tube");
        assert!(capped[1] < -276.0, "under the lid: {capped:?}");
        // The canyon floor over the tube is its own span: a body on it stays on it, unconfined.
        let over = tunnels.move_floor(Some(&terrain), cx, cz, -256.0);
        assert_eq!(over.surface, FloorSurface::Heightfield);
        assert!(
            (over.point[1] + 256.0).abs() < 0.05,
            "the grid's ground, not the patch: {over:?}"
        );
        assert!(
            tunnels
                .confine(Some(&terrain), [cx, -255.0, cz], -256.0, 2.5)
                .is_none()
        );
        assert!(tunnels.underground([cx, -285.0, cz]) && !tunnels.underground([cx, -255.0, cz]));
    }

    /// T3: a `*_skylight` patch that dips into the tube is a shaft: there is no ground in that column.
    /// A body in the tube under it rides the tube floor under the tube's lid (the drawn rubble cone is
    /// dressing, no step in the lane); a body on the plain over the drawn pit has no ground, falls in,
    /// and is not snapped under the lid on the way.
    #[test]
    fn a_skylight_shaft_leaves_no_ground_over_the_tube() {
        let (terrain, cx, cz) = canyon_with_cone("west_skylight");
        let tunnels = for_map("switchbacks", Some(&terrain)).expect("layout");
        let mut floor = -286.0f32;
        for k in 0..=28 {
            let x = cx - 7.0 + k as f32 * 0.5;
            let f = tunnels.move_floor(Some(&terrain), x, cz, floor);
            assert!((f.point[1] - floor).abs() < 0.05, "step {k}: {f:?} from {floor}");
            floor = f.point[1];
        }
        assert!((floor + 286.0).abs() < 0.05);
        // In the shaft and beside it: the tube floor under the tube's lid.
        for x in [cx, cx - 9.0] {
            let inside = tunnels.body_support(Some(&terrain), x, cz, -286.0);
            assert_eq!(inside.floor.surface, FloorSurface::TunnelFloor);
            assert!(
                inside.ceiling.is_some_and(|lid| (lid + 274.0).abs() < 0.05),
                "{inside:?}"
            );
        }
        let capped = tunnels
            .confine(Some(&terrain), [cx, -274.5, cz], -286.0, 0.8)
            .expect("confined in the tube");
        assert!(capped[1] < -276.0, "under the lid: {capped:?}");
        // From the plain over the shaft: no ground, the tube floor far below, and no snap under the lid.
        let over = tunnels.body_support(Some(&terrain), cx, cz, -256.0);
        assert_eq!(over.floor.surface, FloorSurface::TunnelFloor);
        assert!((over.floor.point[1] + 286.0).abs() < 0.05, "{over:?}");
        let falling = tunnels
            .confine(Some(&terrain), [cx, -260.0, cz], -286.0, 0.8)
            .expect("in the shaft");
        assert!(
            (falling[1] + 260.0).abs() < 1e-3,
            "a body over the lid keeps its height: {falling:?}"
        );
        // Beside the shaft the plain is the plain.
        assert_eq!(
            tunnels.move_floor(Some(&terrain), cx - 9.0, cz, -256.0).surface,
            FloorSurface::Heightfield
        );
    }

    /// T3: every level of a column is its own span. A body in the tube under the canyon floor stays in
    /// the tube at any height under its lid, and one on the canyon floor stays on it; a shell reads the
    /// floor of the layer it is in.
    #[test]
    fn a_body_stays_on_the_layer_it_is_in_where_the_tube_runs_under_the_canyon() {
        let terrain = canyon();
        let state = for_map("switchbacks", Some(&terrain)).expect("switchbacks layout");
        let (x, z) = (-300.0f32, 150.0f32);
        let col = crate::compat::surface::column(Some(&terrain), state.mesh.as_ref(), x, z);
        assert_eq!(col.spans().len(), 2, "tube and canyon: {:?}", col.spans());
        let mut y = -286.0f32;
        while y < -276.6 {
            let f = state.move_floor(Some(&terrain), x, z, y);
            assert_eq!(f.surface, FloorSurface::TunnelFloor, "body at {y}");
            assert!((f.point[1] + 286.0).abs() < 0.05, "body at {y}: {f:?}");
            y += 0.37;
        }
        let mut y = -256.0f32;
        while y < -200.0 {
            let f = state.move_floor(Some(&terrain), x, z, y);
            assert_eq!(f.surface, FloorSurface::Heightfield, "body at {y}");
            assert!((f.point[1] + 256.0).abs() < 0.05, "body at {y}: {f:?}");
            y += 3.1;
        }
        // A shell: in the tube it reads the tube floor, over the canyon the canyon floor, and one
        // that has sunk just under the canyon floor still reads the canyon floor (it bursts there).
        assert!((state.support(Some(&terrain), x, z, -280.0).floor.point[1] + 286.0).abs() < 0.05);
        assert!((state.support(Some(&terrain), x, z, -240.0).floor.point[1] + 256.0).abs() < 0.05);
        assert!((state.support(Some(&terrain), x, z, -256.4).floor.point[1] + 256.0).abs() < 0.05);
        assert_eq!(
            state
                .support(Some(&terrain), x, z, -280.0)
                .ceiling
                .map(|c| (c + 274.0).abs() < 0.05),
            Some(true)
        );
        assert_eq!(state.support(Some(&terrain), x, z, -240.0).ceiling, None);
    }

    /// T3: the mesh floors that carry bodies. The four threshold sills (a 0.3 m slab where each ramp
    /// leaves the rock) are trim, and the lips and caps drawn 0.10 m proud of the ground are welded
    /// onto it; the drawn triangles are untouched.
    #[test]
    fn switchbacks_support_floors_are_trimmed_and_welded() {
        let terrain = canyon();
        let state = for_map("switchbacks", Some(&terrain)).expect("switchbacks layout");
        let mesh = state.mesh.as_ref().expect("mesh");
        let floors = mesh
            .triangles
            .iter()
            .filter(|t| t.kind == crate::compat::mesh::TunnelFaceKind::Floor)
            .count();
        assert_eq!(
            floors - mesh.support_floors().len(),
            8,
            "two triangles per sill, four sills"
        );
        let proud = |tris: &[crate::compat::mesh::TunnelTriangle]| {
            tris.iter()
                .filter(|t| t.kind == crate::compat::mesh::TunnelFaceKind::Floor)
                .flat_map(|t| [t.a, t.b, t.c])
                .filter(|v| {
                    let d = v[1] - terrain.height_at_world(v[0], v[2]);
                    d > 1e-4 && d <= crate::compat::surface::WELD_M
                })
                .count()
        };
        assert!(
            proud(&mesh.triangles) > 50,
            "the drawn lips are proud of the ground"
        );
        assert_eq!(proud(mesh.support_floors()), 0, "no support vertex is left proud");
    }

    /// A shot from the canyon floor at the tube under it crosses the tube's lid; a shot down the tube
    /// crosses nothing.
    #[test]
    fn rock_between_the_plain_and_the_tube_stops_a_shot() {
        let counts = ((512.0f32 - 256.0) * 32.0) as u16;
        let terrain = TerrainSampler::new(HeightGrid::square(4, vec![counts; 16]).expect("grid"), 1280.0)
            .with_zero_counts(16384);
        let tunnels = for_map("switchbacks", Some(&terrain)).expect("layout");
        let plain = [-300.0, -255.0, 150.0];
        let tube = [-300.0, -284.5, 150.0];
        let t = tunnels
            .rock_between(plain, tube)
            .expect("the lid is between them");
        assert!(t > 0.0 && t < 1.0);
        assert!(
            tunnels
                .rock_between([-310.0, -284.0, 145.0], [-295.0, -284.0, 153.0])
                .is_none(),
            "along the tube"
        );
        assert!(TunnelsState::disabled().rock_between(plain, tube).is_none());
        // Out of the west ramp's open cut (top segment, no lid) sideways over the rim: the sim's 10 m trench wall
        // is not rock to a shot (the drawn funnel is far wider).
        let layout = tunnels.layout.as_ref().unwrap();
        let ramp = layout.sampled.passage("ramp_west").unwrap();
        let k = ramp.covered.iter().rposition(|c| !*c).unwrap();
        let q = ramp.samples[k];
        let from = [q.p[0], q.p[1] + 1.5, q.p[2]];
        let to = [q.p[0] + q.left[0] * 30.0, q.p[1] + 6.0, q.p[2] + q.left[1] * 30.0];
        assert!(
            tunnels.rock_between(from, to).is_none(),
            "open-cut wall stopped a shot"
        );
    }
}
