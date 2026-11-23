// SPDX-License-Identifier: MPL-2.0
//! Cross-arch pin for the Stage A mesher and queries (ADR-8050, ADR-8012 A-4, ADR-8166).
//!
//! Every peer of a lockstep match builds its tunnel collision mesh, nav overlay and terrain hole list from the
//! same description at load and then asks the same queries every tick, so none of those bits may depend on the
//! host. This test hashes, per fixture map and terrain:
//!
//! - the mesh (vertex bits, face kind, flags, in mesh order) and nav overlay (waypoint bits, edges, portals,
//!   levels);
//! - the mouth holes (`SampledLayout::mouth_holes`: opening, centre, half sizes and yaw bits);
//! - a fixed query table (review rev-b L5): `query_ground`, `ceiling_above`, `separate_disc_from_walls_band`,
//!   `segment_blocker` and `TerrainSampler::hole_at` at points laid out from the description's own control
//!   points and mouth tops (fixed inputs, independent of the code under test), with a dense grid over each
//!   mouth so hole edges are crossed.
//!
//! The hashes were recorded on x86-64 Linux. The same test run on aarch64 (the Mac, the Android test device, a
//! qemu-user leg) and on Windows must match. A mismatch prints the map, terrain and part that moved. A change
//! that moves a pinned hash must bump `regia_world_geom::QUERY_BITS_VERSION` and `PINNED_QUERY_BITS_VERSION`
//! together (spec 121 § 14 rule 2).

#![allow(clippy::unwrap_used)]

use regia_world_geom::QUERY_BITS_VERSION;
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

/// FNV-1a 64.
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

/// The fixed query inputs: around every control point and mouth top of the description (a coarse star of
/// offsets at three body heights), plus a dense 0.25 m grid over +-12 m of each mouth top for the hole test.
/// Built only from the fixture's own numbers with `+` and `*` on exact steps, so the inputs are the same bits on
/// every host and do not depend on the code under test.
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

/// Segment ends relative to a query point: across a wall, down through a floor, up through a lid.
const SEGMENTS: [[f32; 3]; 3] = [[8.0, 1.0, 0.0], [0.0, -2.0, 8.0], [-6.0, 3.0, -6.0]];

/// Hash of the query table on one map and terrain. `terrain` carries the map's mouth holes.
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
        // One bit per point, packed, so the hash sees every answer.
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

/// The `QUERY_BITS_VERSION` the hashes below were recorded with.
// ADR-8246: version 3 adds native queries. Every Stage A hash below is unchanged.
// Version 6 changes native tie order only. These Stage A hashes were re-checked and did not move.
// Version 7 (W-13) moves answers only while a blocker plug is active; these hashes did not move.
// Version 8 (ADR-8323, numbered at ADR-8531) adds the native capsule push only; the Stage A hashes are unchanged.
const PINNED_QUERY_BITS_VERSION: u32 = 8;

/// (map, terrain, mesh and nav, mouth holes, query table) recorded on x86-64 Linux (the build server, 2026-11-16) at
/// `QUERY_BITS_VERSION` 1; the query column re-recorded at 2 (ADR-8167: `query_ground` is T3's `span_at`; the mesh
/// and hole hashes did not move). The Switchbacks' mouths open at -256 m, far below the rolling terrain (about 0 m), so
/// that terrain adds no lip and the mesh equals the terrain-free one. The mesh hashes are the ones the regia-absorb-1
/// copy recorded. Measured on x86-64 (ADR-8166): with `det_*` and libm `sqrt` but std trig, all 27 hashes equal the
/// regia-absorb-1 copy's; libm trig then moved 7 hole hashes (glibc and libm `atan2f` differ on some yaws) and no
/// mesh or query hash.
const PINNED: [(&str, &str, u64, u64, u64); 9] = [
    (
        "switchbacks",
        "none",
        0xc717_2398_946e_2bb2,
        0x4d25_767f_9dce_13f5,
        0xa11f_0ece_effa_aa44,
    ),
    (
        "switchbacks",
        "canyon",
        0xb249_b8d9_3843_cfc3,
        0xf1e7_88f8_0bc6_1f5a,
        0x4718_1dbd_6540_c23f,
    ),
    (
        "switchbacks",
        "rolling",
        0xc717_2398_946e_2bb2,
        0x4d25_767f_9dce_13f5,
        0x1d86_3566_2efe_e279,
    ),
    (
        "atacama_rail",
        "none",
        0xfab6_00c6_ea31_8aa4,
        0xeb6d_fcb8_048f_9665,
        0xf89e_b90c_1a7a_01b9,
    ),
    (
        "atacama_rail",
        "canyon",
        0xa9b9_4188_39e0_acb7,
        0xeb6d_fcb8_048f_9665,
        0x7865_9834_3e3d_0055,
    ),
    (
        "atacama_rail",
        "rolling",
        0xcdc2_4d97_2efd_b312,
        0xeb6d_fcb8_048f_9665,
        0x38c7_7c7d_9519_df07,
    ),
    (
        "cinder_steps",
        "none",
        0x2d4c_f45a_180e_74f1,
        0x8fa4_d05f_a195_185e,
        0x3f0d_cd2c_b207_7379,
    ),
    (
        "cinder_steps",
        "canyon",
        0x17be_4deb_2b8e_fd55,
        0x8fa4_d05f_a195_185e,
        0x3fd4_e962_cd5b_0811,
    ),
    (
        "cinder_steps",
        "rolling",
        0x624d_cd20_0ddc_5e9b,
        0xfac0_e5d1_f878_9e8d,
        0xeef8_9300_7e85_e3aa,
    ),
];

#[test]
fn fixture_meshes_hash_to_the_pinned_bits_on_every_arch() {
    assert_eq!(
        QUERY_BITS_VERSION, PINNED_QUERY_BITS_VERSION,
        "QUERY_BITS_VERSION moved: re-record the hashes below with the new version"
    );
    let terrains = [
        ("none", None),
        ("canyon", Some(canyon_terrain())),
        ("rolling", Some(rolling_terrain())),
    ];
    let mut moved = Vec::new();
    for (map, terrain_name, want_mesh, want_holes, want_queries) in PINNED {
        let terrain = terrains
            .iter()
            .find(|(n, _)| *n == terrain_name)
            .unwrap()
            .1
            .as_ref();
        let layout = for_map(map).unwrap();
        let sampled = layout.sample(terrain);
        let mesh = sampled.mesh(&layout.openings, terrain);
        let nav = TunnelNavOverlay::bake(&mesh);
        let holes = sampled.mouth_holes(&layout.openings);
        let holed = terrain.map(|t| t.clone().with_holes(holes.clone()));
        let got_mesh = mesh_hash(&mesh, &nav);
        let got_holes = holes_hash(&holes);
        let got_queries = query_hash(&layout, &mesh, holed.as_ref());
        println!(
            "(\"{map}\", \"{terrain_name}\", {got_mesh:#018x}, {got_holes:#018x}, {got_queries:#018x}),"
        );
        for (part, got, want) in [
            ("mesh", got_mesh, want_mesh),
            ("holes", got_holes, want_holes),
            ("queries", got_queries, want_queries),
        ] {
            if got != want {
                moved.push(format!(
                    "{map}/{terrain_name} {part}: {got:#018x}, pinned {want:#018x}"
                ));
            }
        }
    }
    assert!(
        moved.is_empty(),
        "bits moved on {} ({}):\n{}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        moved.join("\n")
    );
}

/// The fixtures of the Stage A bit table: (name, terrain).
const TABLE: [(&str, &str); 5] = [
    ("switchbacks", "none"),
    ("switchbacks", "canyon"),
    ("cinder_steps", "none"),
    ("cinder_steps", "canyon"),
    ("four_decks", "none"),
];

/// The host's cushion-tilt tuning the table asks `ground_tilt_at` with (3.0 m, 0.61 rad, 2.5 m).
const TILT: TiltParams = TiltParams {
    span_m: 3.0,
    max: 0.61,
    ledge_m: 2.5,
};

/// Headings the tilt is asked at (radians; exact f32 literals).
const YAWS: [f32; 4] = [0.0, 0.75, 2.25, -1.5];

/// Query points of the four-deck stack (no layout): a 1 m grid over its plan at every 1 m height.
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

/// Hash of the Stage A table on one fixture: every public query of `compat::state` and
/// `compat::hover_ground` at every point (plan row W-02 acceptance 4, the table lane W-05's
/// `stage_a_bits_are_the_same_with_and_without_stage_b` builds on).
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

/// The Stage A bit table, recorded on x86-64 Linux (the build server, 2026-11-16) at `QUERY_BITS_VERSION` 2 (the
/// regia-absorb-2 copy, DO27's 1.58 query).
const STAGE_A_PINNED: [(&str, &str, u64); 5] = [
    ("switchbacks", "none", 0xd8e3_cef9_8082_1b23),
    ("switchbacks", "canyon", 0x8a02_39cb_c199_18ce),
    ("cinder_steps", "none", 0xbe2c_20d4_34f7_ee84),
    ("cinder_steps", "canyon", 0x5235_4c62_6c2e_a45b),
    ("four_decks", "none", 0xe72e_4ea0_bcf5_c015),
];

#[test]
fn stage_a_bit_table_is_pinned_on_every_arch() {
    assert_eq!(
        QUERY_BITS_VERSION, PINNED_QUERY_BITS_VERSION,
        "QUERY_BITS_VERSION moved: re-record the hashes below with the new version"
    );
    let canyon = canyon_terrain();
    let mut moved = Vec::new();
    for ((map, terrain_name), (pin_map, pin_terrain, want)) in TABLE.into_iter().zip(STAGE_A_PINNED) {
        assert_eq!((map, terrain_name), (pin_map, pin_terrain));
        let (tunnels, terrain, points) = if map == "four_decks" {
            (
                TunnelsState::with_mesh(true, synthetic_four_decks()),
                None,
                deck_points(),
            )
        } else {
            let layout = for_map(map).unwrap();
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
        let got = stage_a_hash(&tunnels, terrain.as_ref(), &points);
        println!("(\"{map}\", \"{terrain_name}\", {got:#018x}),");
        if got != want {
            moved.push(format!("{map}/{terrain_name}: {got:#018x}, pinned {want:#018x}"));
        }
    }
    assert!(
        moved.is_empty(),
        "Stage A bits moved on {} ({}):\n{}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        moved.join("\n")
    );
}

// --- Plan row W-03 acceptance 5: the on-foot rows of the Stage A bit table. ---

/// A host's on-foot solids for the walk: the static grids, two buildings, loose and fixed boxes, no rocks.
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

/// The on-foot world grid of a fixture: every collision-mesh triangle stamped in world space with the T3
/// floor rule (so the mesh floors drop and the walls, lids and lips stay), plus a raised deck, a low step
/// and a wall box laid on the walk at `at` (fixed offsets).
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

/// The walk host at `at`: the grid above, one building grid (a deck and a wall ring piece, local space)
/// placed twice with yaws, a loose step box, a loose forced block and a fixed crate.
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

/// Hash of the grid's cells (floor and span bits, in key order) and its door cuts.
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

/// Headings of the walk (exact f32 steps of about one run tick each).
const WALK: [[f32; 2]; 4] = [[0.125, 0.0], [0.0625, 0.125], [0.125, -0.0625], [-0.125, 0.0625]];

/// The on-foot rows on one fixture: the stamped grid, then a fixed walk of `steps` ticks from `start`
/// (`resolve_feet` on the tunnel floor, a long `slide_move` probe and `pilot_floor_y` every tick).
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
    // How often the walk met a wall (the probe was cut short), stood above the tunnel floor, and climbed.
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

/// The Switchbacks fixture's canyon with a rubble cone (a `*_skylight` patch, 1.8 m, 6 m across) on the
/// mid-east ramp about a third of the way up, the cone heights being the ramp floor plus the cone.
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

/// The on-foot bit table, recorded on x86-64 Linux (the build server, 2026-11-17) at `QUERY_BITS_VERSION` 2: the
/// gallery (floor -5 under flat ground), the Switchbacks tube west of the junction, and the mid-east ramp
/// across the skylight cone.
const ONFOOT_PINNED: [(&str, u64); 3] = [
    ("gallery", 0x0935_9425_42f4_c2a2),
    ("switchbacks_tube", 0x042a_f2f6_62ac_4b9f),
    ("switchbacks_ramp_cone", 0xc9de_38d3_6487_2aa4),
];

#[test]
fn onfoot_bit_table_is_pinned_on_every_arch() {
    assert_eq!(
        QUERY_BITS_VERSION, PINNED_QUERY_BITS_VERSION,
        "QUERY_BITS_VERSION moved: re-record the hashes below with the new version"
    );
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
    let mut moved = Vec::new();
    for ((name, (got, met)), (pin_name, want)) in got.into_iter().zip(ONFOOT_PINNED) {
        assert_eq!(name, pin_name);
        println!("(\"{name}\", {got:#018x}), // walls, above the floor, raised: {met:?}");
        assert!(
            met.iter().all(|n| *n > 0),
            "{name}: the walk must meet a wall, a deck and a raised floor: {met:?}"
        );
        if got != want {
            moved.push(format!("{name}: {got:#018x}, pinned {want:#018x}"));
        }
    }
    assert!(
        moved.is_empty(),
        "on-foot bits moved on {} ({}):\n{}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        moved.join("\n")
    );
}
