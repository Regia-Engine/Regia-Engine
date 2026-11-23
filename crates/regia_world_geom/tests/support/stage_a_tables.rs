// SPDX-License-Identifier: MPL-2.0
//! Live Stage A table rows for `tests/stage_a_bits.rs` (plan row W-05).
//!
//! The hash helpers are the ones in `tests/mesh_bits.rs` (W-01 mesh and query, W-02 state and
//! hover-ground, W-03 on-foot). This file has no `#[test]` items. Integration tests are separate
//! crates, so `tests/stage_a_bits.rs` includes this with `#[path]` and does not include `mesh_bits.rs`.
//! The navmesh ids stay literals in `tests/fixtures/stage_a_bits.ron` (no `regia_nav` dependency).

use regia_world_geom::compat::fixtures::ON_FOOT_PARAMS;
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::ground::query_ground;
use regia_world_geom::compat::hover_ground::{
    CraterBook, NoGroundDelta, TiltParams, ground_tilt_at, sample_move_floor,
};
use regia_world_geom::compat::layout::{MouthHole, TunnelLayout};
use regia_world_geom::compat::mesh::synthetic_four_decks;
use regia_world_geom::compat::mesh::synthetic_gallery;
use regia_world_geom::compat::mesh::{
    TunnelCollisionMesh, TunnelFaceKind, TunnelNavOverlay, separate_disc_from_walls_band,
};
use regia_world_geom::compat::onfoot::{
    Blocker, OnFootSolids, PilotGrid, PilotSolids, building_key, pilot_floor_y, resolve_feet, slide_move,
};
use regia_world_geom::compat::sampler::{FloorSample, FloorSurface, HeightGrid, TerrainSampler};
use regia_world_geom::compat::state::TunnelsState;
use regia_world_geom::compat::terrain_patch::TerrainPatch;

/// One mesh row: map, terrain, mesh+nav, mouth holes, query table.
pub struct MeshRow {
    /// Fixture map id.
    pub map: &'static str,
    /// Terrain name (`none`, `canyon`, `rolling`).
    pub terrain: &'static str,
    /// Mesh and nav overlay hash.
    pub mesh: u64,
    /// Mouth-hole hash.
    pub holes: u64,
    /// Query-table hash.
    pub queries: u64,
}

/// One state and hover-ground row.
pub struct StageRow {
    /// Fixture map id.
    pub map: &'static str,
    /// Terrain name.
    pub terrain: &'static str,
    /// Stage A query hash.
    pub hash: u64,
}

/// One on-foot row.
pub struct OnfootRow {
    /// Row name.
    pub name: &'static str,
    /// On-foot hash.
    pub hash: u64,
    /// Wall hits, steps above the tunnel floor, raised-floor steps.
    pub met: [u32; 3],
}

/// The three live tables, in the order `tests/fixtures/stage_a_bits.ron` pins.
pub struct Live {
    /// W-01 mesh, hole and query triples.
    pub mesh: Vec<MeshRow>,
    /// W-02 state and hover-ground hashes.
    pub stage_a: Vec<StageRow>,
    /// W-03 on-foot hashes.
    pub onfoot: Vec<OnfootRow>,
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 ^= u64::from(x);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }
    fn opt(&mut self, v: Option<f32>) {
        match v {
            Some(v) => {
                self.u32(1);
                self.f32(v);
            }
            None => self.u32(0),
        }
    }
    fn floor(&mut self, f: &FloorSample) {
        for c in f.point.into_iter().chain(f.normal) {
            self.f32(c);
        }
        self.u32(match f.surface {
            FloorSurface::Heightfield => 0,
            FloorSurface::TunnelFloor => 1,
        });
    }
}

fn mesh_hash(mesh: &TunnelCollisionMesh, nav: &TunnelNavOverlay) -> u64 {
    let mut h = Fnv::new();
    h.u32(mesh.triangles.len() as u32);
    for t in &mesh.triangles {
        for v in [t.a, t.b, t.c] {
            for c in v {
                h.f32(c);
            }
        }
        h.u32(match t.kind {
            TunnelFaceKind::Floor => 0,
            TunnelFaceKind::Wall => 1,
            TunnelFaceKind::Ceiling => 2,
        });
        h.u32(u32::from(t.portal) | (u32::from(t.pilot_only) << 1));
    }
    h.u32(nav.waypoints.len() as u32);
    for w in &nav.waypoints {
        for c in w {
            h.f32(*c);
        }
    }
    for (a, b) in &nav.edges {
        h.u32(*a as u32);
        h.u32(*b as u32);
    }
    for p in &nav.portal_indices {
        h.u32(*p as u32);
    }
    h.bytes(&nav.levels);
    h.0
}

fn holes_hash(holes: &[MouthHole]) -> u64 {
    let mut h = Fnv::new();
    h.u32(holes.len() as u32);
    for hole in holes {
        h.u32(hole.opening.len() as u32);
        h.bytes(hole.opening.as_bytes());
        for c in hole.center.into_iter().chain(hole.half) {
            h.f32(c);
        }
        h.f32(hole.yaw);
    }
    h.0
}

fn query_points(layout: &TunnelLayout) -> (Vec<[f32; 3]>, Vec<[f32; 2]>) {
    const OFFSETS: [f32; 9] = [-6.0, -3.0, -1.0, -0.5, 0.0, 0.5, 1.0, 3.0, 6.0];
    const HEIGHTS: [f32; 3] = [-2.0, 1.0, 4.0];
    let mut anchors: Vec<[f32; 3]> = layout
        .passages
        .iter()
        .flat_map(|p| p.points.iter().copied())
        .collect();
    anchors.extend(layout.openings.iter().map(|o| o.top));
    let mut body = Vec::new();
    for a in &anchors {
        for dx in OFFSETS {
            for dz in OFFSETS {
                for dy in HEIGHTS {
                    body.push([a[0] + dx, a[1] + dy, a[2] + dz]);
                }
            }
        }
    }
    let mut plan = Vec::new();
    for o in &layout.openings {
        for i in -48i16..=48 {
            for k in -48i16..=48 {
                plan.push([o.top[0] + f32::from(i) * 0.25, o.top[2] + f32::from(k) * 0.25]);
            }
        }
    }
    (body, plan)
}

const SEGMENTS: [[f32; 3]; 3] = [[8.0, 1.0, 0.0], [0.0, -2.0, 8.0], [-6.0, 3.0, -6.0]];

fn query_hash(layout: &TunnelLayout, mesh: &TunnelCollisionMesh, terrain: Option<&TerrainSampler>) -> u64 {
    let (body, plan) = query_points(layout);
    let mut h = Fnv::new();
    h.u32(body.len() as u32);
    for p in &body {
        let [x, y, z] = *p;
        let support = query_ground(terrain, Some(mesh), x, z, y);
        h.floor(&support.floor);
        h.opt(support.ceiling);
        h.opt(mesh.ceiling_above(x, z, y));
        let [px, pz] = separate_disc_from_walls_band(mesh, x, z, 2.5, y - 1.0, y + 3.0);
        h.f32(px);
        h.f32(pz);
        for d in SEGMENTS {
            h.opt(mesh.segment_blocker(*p, [x + d[0], y + d[1], z + d[2]]));
        }
    }
    h.u32(plan.len() as u32);
    if let Some(terrain) = terrain {
        let mut word = 0u32;
        for (i, [x, z]) in plan.iter().enumerate() {
            word |= u32::from(terrain.hole_at(*x, *z)) << (i % 32);
            if i % 32 == 31 {
                h.u32(word);
                word = 0;
            }
        }
        h.u32(word);
    }
    h.0
}

fn canyon_terrain() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384)
}

fn rolling_terrain() -> TerrainSampler {
    let side = 65usize;
    let heights = (0..side * side)
        .map(|i| {
            let (x, z) = (i % side, i / side);
            (20_000 + (x * 37 + z * 91) % 640) as u16
        })
        .collect();
    TerrainSampler::new(HeightGrid::square(side, heights).unwrap(), 1280.0)
}

const MESH_ORDER: [(&str, &str); 9] = [
    ("switchbacks", "none"),
    ("switchbacks", "canyon"),
    ("switchbacks", "rolling"),
    ("atacama_rail", "none"),
    ("atacama_rail", "canyon"),
    ("atacama_rail", "rolling"),
    ("cinder_steps", "none"),
    ("cinder_steps", "canyon"),
    ("cinder_steps", "rolling"),
];

fn mesh_rows() -> Vec<MeshRow> {
    let terrains = [
        ("none", None),
        ("canyon", Some(canyon_terrain())),
        ("rolling", Some(rolling_terrain())),
    ];
    MESH_ORDER
        .into_iter()
        .map(|(map, terrain_name)| {
            let terrain = terrains
                .iter()
                .find(|(n, _)| *n == terrain_name)
                .unwrap()
                .1
                .as_ref();
            let layout = for_map(map).unwrap_or_else(|| panic!("{map}"));
            let sampled = layout.sample(terrain);
            let mesh = sampled.mesh(&layout.openings, terrain);
            let nav = TunnelNavOverlay::bake(&mesh);
            let holes = sampled.mouth_holes(&layout.openings);
            let holed = terrain.map(|t| t.clone().with_holes(holes.clone()));
            MeshRow {
                map,
                terrain: terrain_name,
                mesh: mesh_hash(&mesh, &nav),
                holes: holes_hash(&holes),
                queries: query_hash(&layout, &mesh, holed.as_ref()),
            }
        })
        .collect()
}

const TABLE: [(&str, &str); 5] = [
    ("switchbacks", "none"),
    ("switchbacks", "canyon"),
    ("cinder_steps", "none"),
    ("cinder_steps", "canyon"),
    ("four_decks", "none"),
];

const TILT: TiltParams = TiltParams {
    span_m: 3.0,
    max: 0.61,
    ledge_m: 2.5,
};

const YAWS: [f32; 4] = [0.0, 0.75, 2.25, -1.5];

fn deck_points() -> Vec<[f32; 3]> {
    let mut out = Vec::new();
    for i in -5i16..=9 {
        for k in -5i16..=5 {
            for j in -13i16..=5 {
                out.push([f32::from(i), f32::from(j), f32::from(k)]);
            }
        }
    }
    out
}

fn stage_a_hash(tunnels: &TunnelsState, terrain: Option<&TerrainSampler>, points: &[[f32; 3]]) -> u64 {
    let mut craters = CraterBook::default();
    if let Some(p) = points.first() {
        craters.crater(p[0], p[2]);
    }
    let mut h = Fnv::new();
    h.u32(points.len() as u32);
    for (i, p) in points.iter().enumerate() {
        let [x, y, z] = *p;
        let support = tunnels.support(terrain, x, z, y);
        h.floor(&support.floor);
        h.opt(support.ceiling);
        let body = tunnels.body_support(terrain, x, z, y);
        h.floor(&body.floor);
        h.opt(body.ceiling);
        let floor = tunnels.move_floor(terrain, x, z, y);
        h.floor(&floor);
        h.floor(&tunnels.place_floor(terrain, x, z));
        h.f32(tunnels.nearest_floor(terrain, x, z, y));
        match tunnels.confine(terrain, *p, floor.point[1], 2.5) {
            Some(c) => {
                h.u32(1);
                for v in c {
                    h.f32(v);
                }
            }
            None => h.u32(0),
        }
        h.u32(u32::from(tunnels.underground(*p)) | (u32::from(tunnels.below_ground_at(terrain, *p)) << 1));
        for d in SEGMENTS {
            h.opt(tunnels.rock_between(*p, [x + d[0], y + d[1], z + d[2]]));
        }
        let [wx, wz] = tunnels.separate_from_walls(x, z);
        h.f32(wx);
        h.f32(wz);
        h.f32(sample_move_floor(terrain, tunnels, &NoGroundDelta, x, z, y));
        h.f32(sample_move_floor(terrain, tunnels, &craters, x, z, y));
        let (pitch, roll) = ground_tilt_at(
            *p,
            YAWS[i % YAWS.len()],
            floor.point[1],
            terrain,
            tunnels,
            &NoGroundDelta,
            &TILT,
        );
        h.f32(pitch);
        h.f32(roll);
    }
    h.0
}

fn stage_rows() -> Vec<StageRow> {
    let canyon = canyon_terrain();
    TABLE
        .into_iter()
        .map(|(map, terrain_name)| {
            let (tunnels, terrain, points) = if map == "four_decks" {
                (
                    TunnelsState::with_mesh(true, synthetic_four_decks()),
                    None,
                    deck_points(),
                )
            } else {
                let layout = for_map(map).unwrap_or_else(|| panic!("{map}"));
                let terrain = (terrain_name == "canyon").then(|| {
                    let holes = layout.sample(Some(&canyon)).mouth_holes(&layout.openings);
                    canyon.clone().with_holes(holes)
                });
                let (points, _) = query_points(&layout);
                (
                    TunnelsState::for_layout(layout, terrain.as_ref()),
                    terrain,
                    points,
                )
            };
            StageRow {
                map,
                terrain: terrain_name,
                hash: stage_a_hash(&tunnels, terrain.as_ref(), &points),
            }
        })
        .collect()
}

struct WalkHost {
    solids: PilotSolids,
    buildings: Vec<(String, [f32; 3], f32)>,
    loose: Vec<Blocker>,
    fixed: Vec<Blocker>,
}

impl OnFootSolids for WalkHost {
    fn pilot_solids(&self) -> &PilotSolids {
        &self.solids
    }
    fn for_each_building(&self, f: &mut dyn FnMut(&str, [f32; 3], f32)) {
        for (key, center, yaw) in &self.buildings {
            f(key, *center, *yaw);
        }
    }
    fn for_each_loose_blocker(&self, f: &mut dyn FnMut(Blocker)) {
        self.loose.iter().copied().for_each(f);
    }
    fn for_each_fixed_blocker(&self, f: &mut dyn FnMut(Blocker)) {
        self.fixed.iter().copied().for_each(f);
    }
    fn rock_push_out(&self, x: f32, z: f32, _feet_y: f32, _radius: f32) -> [f32; 2] {
        [x, z]
    }
}

fn onfoot_world_grid(tunnels: &TunnelsState, terrain: &TerrainSampler, at: [f32; 3]) -> PilotGrid {
    let mut grid = PilotGrid::default();
    let sim_floor = |x: f32, z: f32, y: f32| {
        let ground = terrain.height_at_world(x, z);
        let span = tunnels.nearest_floor(Some(terrain), x, z, y);
        if (span - y).abs() < (ground - y).abs() {
            span
        } else {
            ground
        }
    };
    for t in &tunnels.mesh.as_ref().unwrap().triangles {
        grid.stamp_triangle(t.a, t.b, t.c, Some(&sim_floor));
    }
    let [x, y, z] = at;
    grid.stamp_floor_rect(x + 2.0, x + 4.0, z - 1.0, z + 1.0, y + 0.4);
    grid.stamp_floor_rect(x + 6.0, x + 7.5, z - 2.0, z + 2.0, y + 1.0);
    grid.stamp_wall_box([x + 9.0, y, z + 0.5], [x + 9.5, y + 2.5, z + 3.0]);
    grid
}

fn walk_host(tunnels: &TunnelsState, terrain: &TerrainSampler, at: [f32; 3]) -> WalkHost {
    let mut hall = PilotGrid::default();
    hall.stamp_floor_rect(-2.0, 2.0, -2.0, 2.0, 0.3);
    hall.stamp_wall_box([-3.0, 0.0, 2.5], [3.0, 3.0, 3.0]);
    let mut solids = PilotSolids {
        world: onfoot_world_grid(tunnels, terrain, at),
        ..PilotSolids::default()
    };
    solids.buildings.insert(building_key("hall", false), hall);
    let [x, y, z] = at;
    let b = |min: [f32; 3], max: [f32; 3], force_block: bool| Blocker {
        min,
        max,
        force_block,
    };
    WalkHost {
        solids,
        buildings: vec![
            (building_key("hall", false), [x + 12.0, y, z], 0.5),
            (building_key("hall", false), [x + 3.0, y + 0.1, z - 5.0], -1.25),
            (building_key("hall", true), [x, y, z], 0.0),
        ],
        loose: vec![
            b([x + 5.0, y, z - 0.5], [x + 5.6, y + 0.25, z + 0.5], false),
            b([x + 14.0, y, z - 1.0], [x + 16.0, y + 0.3, z + 1.0], true),
        ],
        fixed: vec![b([x + 1.0, y, z + 2.0], [x + 1.8, y + 0.5, z + 2.8], false)],
    }
}

fn grid_hash(h: &mut Fnv, grid: &PilotGrid) {
    h.u32(grid.cells.len() as u32);
    for ((ix, iz), cell) in &grid.cells {
        h.u32(*ix as u32);
        h.u32(*iz as u32);
        h.u32(u32::from(cell.has_floor));
        h.f32(cell.floor_y);
        h.u32(cell.spans().len() as u32);
        for s in cell.spans() {
            h.f32(s.lo);
            h.f32(s.hi);
        }
    }
    h.u32(grid.span_cap_hits);
}

const WALK: [[f32; 2]; 4] = [[0.125, 0.0], [0.0625, 0.125], [0.125, -0.0625], [-0.125, 0.0625]];

fn onfoot_hash(
    tunnels: &TunnelsState,
    terrain: &TerrainSampler,
    start: [f32; 3],
    steps: usize,
) -> (u64, [u32; 3]) {
    let host = walk_host(tunnels, terrain, start);
    let grid = &host.solids.world;
    let boxes: Vec<Blocker> = host.loose.iter().chain(&host.fixed).copied().collect();
    let mut h = Fnv::new();
    grid_hash(&mut h, grid);
    let mut feet = start;
    let mut met = [0u32; 3];
    for i in 0..steps {
        let d = WALK[(i / 40) % WALK.len()];
        let prev_y = feet[1];
        let floor = |x: f32, z: f32| tunnels.move_floor(Some(terrain), x, z, prev_y).point[1];
        feet = resolve_feet(&ON_FOOT_PARAMS, feet, d, floor, grid, &boxes, i % 17 != 16);
        for c in feet {
            h.f32(c);
        }
        let probe = slide_move(
            &ON_FOOT_PARAMS,
            grid,
            &boxes,
            feet[0],
            feet[2],
            feet[0] + d[0] * 24.0,
            feet[2] + d[1] * 24.0,
            feet[1],
            ON_FOOT_PARAMS.capsule_radius_m,
        );
        h.f32(probe[0]);
        h.f32(probe[1]);
        met[0] += u32::from(probe != [feet[0] + d[0] * 24.0, feet[2] + d[1] * 24.0]);
        let base = floor(feet[0], feet[2]);
        met[1] += u32::from(feet[1] > base + 0.01);
        let on_foot = pilot_floor_y(
            &host,
            Some(terrain),
            &ON_FOOT_PARAMS,
            feet[0],
            feet[2],
            feet[1],
            base,
        );
        met[2] += u32::from(on_foot > base + 0.01);
        h.f32(on_foot);
        h.f32(pilot_floor_y(
            &host,
            Some(terrain),
            &ON_FOOT_PARAMS,
            feet[0],
            feet[2],
            feet[1] + 0.5,
            base,
        ));
    }
    (h.0, met)
}

fn switchbacks_with_cone(canyon: &TerrainSampler) -> (TunnelsState, TerrainSampler) {
    let tunnels = TunnelsState::for_layout(for_map("switchbacks").unwrap(), Some(canyon));
    let (cx, cz) = (16.0f32, -126.0f32);
    let (cols, rows) = (25usize, 25usize);
    let origin = [cx - 6.0, cz - 6.0];
    let ramp_y = tunnels.move_floor(Some(canyon), cx, cz, -270.0).point[1];
    let heights = (0..rows * cols)
        .map(|i| {
            let (x, z) = (
                origin[0] + (i % cols) as f32 * 0.5,
                origin[1] + (i / cols) as f32 * 0.5,
            );
            let floor = tunnels.move_floor(Some(canyon), x, z, ramp_y + 1.0).point[1];
            let d = regia_detsim::math::sqrt((x - cx) * (x - cx) + (z - cz) * (z - cz));
            floor + 1.8 * (1.0 - regia_detsim::float::det_min(d / 6.0, 1.0))
        })
        .collect();
    let patch = TerrainPatch {
        id: "mid_east_skylight".into(),
        origin,
        spacing: 0.5,
        cols,
        rows,
        heights,
    };
    (tunnels, canyon.clone().with_patches(vec![patch]))
}

fn onfoot_rows() -> Vec<OnfootRow> {
    let flat = TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 40.0);
    let gallery = TunnelsState::with_mesh(true, synthetic_gallery(10.0, 10.0, -5.0, 1.0, true));
    let canyon = canyon_terrain();
    let (switchbacks, coned) = switchbacks_with_cone(&canyon);
    let ramp_start = {
        let y = switchbacks.move_floor(Some(&coned), 6.0, -140.0, -280.0).point[1];
        [6.0, y, -140.0]
    };
    let got = [
        ("gallery", onfoot_hash(&gallery, &flat, [-8.0, -5.0, -8.0], 320)),
        (
            "switchbacks_tube",
            onfoot_hash(&switchbacks, &coned, [-300.0, -286.0, 150.0], 320),
        ),
        (
            "switchbacks_ramp_cone",
            onfoot_hash(&switchbacks, &coned, ramp_start, 320),
        ),
    ];
    got.into_iter()
        .map(|(name, (hash, met))| OnfootRow { name, hash, met })
        .collect()
}

/// Recompute the three tables. Order matches `tests/fixtures/stage_a_bits.ron`.
pub fn live() -> Live {
    Live {
        mesh: mesh_rows(),
        stage_a: stage_rows(),
        onfoot: onfoot_rows(),
    }
}
