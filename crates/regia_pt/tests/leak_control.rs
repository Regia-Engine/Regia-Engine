// SPDX-License-Identifier: MPL-2.0
//! Baked GI leak control on the CPU (ADR-366), with synthetic rooms and no GPU:
//!
//! - The wall test between neighbor probes ([`NeighborWalls`]) seals a closed box, opens at a
//!   door gap, walls off a probe inside a solid block, sees a 10 cm partition, and records where
//!   each wall crosses a face segment.
//! - The runtime lookup's CPU mirror ([`sample_static_gi`]) with those wall bits keeps the light
//!   of the far side of a wall out where the probes' distance moments miss the wall (here the
//!   moments are all open on purpose, as a 4×4 octahedral map averages a thin wall away), and
//!   lets it in through the door. Each case also checks the same volume without the codes leaks,
//!   so the test proves the codes are what stops it. The walls sit off the middle of the cells
//!   on purpose: a receiver nearer the far side's probes still reads its own side.
//! - The bake filter ([`filter_probes_with`]) does not average across a wall.

use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedGiVolume, DetailBrick, GI_COMPONENTS, ProbeData, ProbeFlags, ProbeMeta,
    SH_Y00, ShRgb, TerrainLayer, VolumeLayout,
};
use regia_core::glam::{Mat4, Vec3};
use regia_pt::leak::{LeakGeometry, NeighborWalls};
use regia_pt::probe::{detail_brick, filter_probes, filter_probes_with};
use regia_pt::{PtCamera, PtMaterial, PtScene, scenes};
use regia_render::{BakedGiSettings, sample_static_gi};

const BRIGHT: f32 = 1.0;
const DARK: f32 = 0.05;
const UP: [f32; 3] = [0.0, 1.0, 0.0];
/// The room: x 1.72..5.83, y 0..3.8, z 2.3..5.9. The walls sit off the probe planes (every half
/// meter) at different fractions of their cells, so no segment between neighbors leaves the room
/// exactly through an edge of it.
const ROOM_MIN: [f32; 3] = [1.72, 0.0, 2.3];
const ROOM_MAX: [f32; 3] = [5.83, 3.8, 5.9];

/// `got` is `want` up to the volume's RGB9E5 encoding.
fn near(got: f32, want: f32) -> bool {
    (got - want).abs() <= 0.005 + 0.005 * want
}

/// An axis-aligned rectangle in the plane `axis = at`, spanning `u` and `v` on the next two
/// axes (cyclic order), as a quad's four corners.
fn rect(axis: usize, at: f32, u: (f32, f32), v: (f32, f32)) -> [[f32; 3]; 4] {
    let (a, b) = ((axis + 1) % 3, (axis + 2) % 3);
    let corner = |pu: f32, pv: f32| {
        let mut p = [0.0f32; 3];
        p[axis] = at;
        p[a] = pu;
        p[b] = pv;
        p
    };
    [
        corner(u.0, v.0),
        corner(u.1, v.0),
        corner(u.1, v.1),
        corner(u.0, v.1),
    ]
}

fn triangles(rects: &[[[f32; 3]; 4]]) -> Vec<[Vec3; 3]> {
    rects
        .iter()
        .flat_map(|q| {
            let p = q.map(Vec3::from_array);
            [[p[0], p[1], p[2]], [p[0], p[2], p[3]]]
        })
        .collect()
}

/// The walls, floor and ceiling of the room (thin quads). With `door`, the x = 1.72 wall has a
/// doorway at z 3..4, y 0..2.
fn room_rects(door: bool) -> Vec<[[f32; 3]; 4]> {
    let ([x0, y0, z0], [x1, y1, z1]) = (ROOM_MIN, ROOM_MAX);
    let mut r = vec![
        rect(0, x1, (y0, y1), (z0, z1)),
        rect(1, y0, (z0, z1), (x0, x1)),
        rect(1, y1, (z0, z1), (x0, x1)),
        rect(2, z0, (x0, x1), (y0, y1)),
        rect(2, z1, (x0, x1), (y0, y1)),
    ];
    if door {
        r.push(rect(0, x0, (y0, y1), (z0, 3.0)));
        r.push(rect(0, x0, (y0, y1), (4.0, z1)));
        r.push(rect(0, x0, (2.0, y1), (3.0, 4.0)));
    } else {
        r.push(rect(0, x0, (y0, y1), (z0, z1)));
    }
    r
}

fn in_room(p: [f32; 3]) -> bool {
    (0..3).all(|a| p[a] > ROOM_MIN[a] && p[a] < ROOM_MAX[a])
}

/// One brick of probes every meter from (0.5, -0.5, 0.5), around and through the room: the
/// bottom plane sits under the floor, so the floor (y = 0) is inside the volume (a lookup
/// outside every brick's probe hull has no coverage).
fn room_layout() -> VolumeLayout {
    let mut l = VolumeLayout {
        terrain: None,
        bricks: vec![detail_brick([0.5, -0.5, 0.5], [7.5, 4.5, 7.5], 1.0)],
        moment_resolution: 4,
        max_distance: 16.0,
    };
    l.assign_brick_offsets();
    assert_eq!(l.bricks[0].counts, [8, 6, 8]);
    l
}

/// Radiance SH of a uniform sky radiance `l`: irradiance / π reads `l` on any surface.
fn uniform(l: f32) -> ShRgb {
    let k = 4.0 * std::f32::consts::PI * SH_Y00 * l;
    [[k, 0.0, 0.0, 0.0]; 3]
}

/// Valid probes whose sky light is `radiance(position)`, with distance moments that see
/// nothing (the case a coarse moment map gets wrong) and `walls`' bits when given.
fn probes(
    layout: &VolumeLayout,
    walls: Option<&NeighborWalls>,
    radiance: impl Fn([f32; 3]) -> f32,
) -> Vec<ProbeData> {
    let texels = (layout.moment_resolution * layout.moment_resolution) as usize;
    let far = layout.max_distance;
    layout
        .nominal_positions()
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let mut sh = [[[0.0f32; 4]; 3]; GI_COMPONENTS];
            sh[1] = uniform(radiance(p));
            ProbeData {
                sh,
                moments: vec![[far, far * far]; texels],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID | walls.map_or(0, |w| w.probe_flags(i)),
                },
            }
        })
        .collect()
}

fn volume(
    layout: &VolumeLayout,
    walls: Option<&NeighborWalls>,
    radiance: impl Fn([f32; 3]) -> f32,
) -> BakedGiVolume {
    BakedGiVolume::from_probes(
        layout,
        BakeLights {
            sky_scale: 1.0,
            ..BakeLights::default()
        },
        BakeInfo::default(),
        &probes(layout, walls, radiance),
    )
    .expect("volume")
}

const SKY: [[f32; 3]; 4] = [[0.0; 3], [1.0; 3], [0.0; 3], [0.0; 3]];

/// Irradiance / π (red) at `p` facing `n`, seen from along `n`.
fn read(v: &BakedGiVolume, p: [f32; 3], n: [f32; 3]) -> f32 {
    sample_static_gi(v, &BakedGiSettings::default(), &SKY, p, n, n).irradiance_over_pi[0]
}

/// Probes reachable from `start` over face-neighbor segments that cross no geometry.
fn reachable(layout: &VolumeLayout, walls: &NeighborWalls, start: usize) -> Vec<bool> {
    let mut seen = vec![false; layout.probe_count()];
    let mut queue = vec![start];
    seen[start] = true;
    while let Some(i) = queue.pop() {
        for (j, step) in layout.neighbors(i) {
            let face = step.iter().map(|s| s.abs()).sum::<i32>() == 1;
            if face && !seen[j] && !walls.blocked(i, step) {
                seen[j] = true;
                queue.push(j);
            }
        }
    }
    seen
}

fn index_at(layout: &VolumeLayout, p: [f32; 3]) -> usize {
    layout
        .nominal_positions()
        .iter()
        .position(|q| (0..3).all(|a| (q[a] - p[a]).abs() < 1e-4))
        .expect("a probe there")
}

/// A closed box seals its probes: no path of open face segments leads out, every segment
/// through a wall is blocked from both ends, and the wall test runs on the scene's meshes as
/// placed (`LeakGeometry::from_scene`).
#[test]
fn a_closed_box_seals_its_probes() {
    let mut scene = PtScene::new(PtCamera::pinhole([0.0; 3], [0.0, 0.0, -1.0], 1.0, 4, 4));
    let material = scene.add_material(PtMaterial::diffuse([0.5; 3]));
    // The room built 10 m off and moved back by its instance transform.
    let mut mesh = regia_pt::PtMesh::default();
    for q in room_rects(false) {
        let moved = q.map(|p| [p[0] + 10.0, p[1], p[2]]);
        scenes::merge(&mut mesh, &scenes::quad(moved));
    }
    let id = scene.add_mesh(mesh);
    scene.add_instance(id, material, Mat4::from_translation(Vec3::new(-10.0, 0.0, 0.0)));
    let geometry = LeakGeometry::from_scene(&scene);
    assert_eq!(geometry.triangle_count(), 12);

    let layout = room_layout();
    let positions = layout.nominal_positions();
    let walls = NeighborWalls::scan(&layout, &positions, &geometry);
    assert_eq!(
        walls,
        NeighborWalls::scan(&layout, &positions, &geometry),
        "deterministic"
    );
    let inside = reachable(&layout, &walls, index_at(&layout, [3.5, 1.5, 3.5]));
    for (i, p) in positions.iter().enumerate() {
        assert_eq!(inside[i], in_room(*p), "probe {i} at {p:?}");
    }
    // Every segment between a room probe and an outside one is blocked, from both ends; none
    // between two room probes is.
    for (i, p) in positions.iter().enumerate() {
        for (j, step) in layout.neighbors(i) {
            match (in_room(*p), in_room(positions[j])) {
                (true, true) => assert!(!walls.blocked(i, step), "{p:?} → {:?}", positions[j]),
                (true, false) | (false, true) => {
                    assert!(walls.blocked(i, step), "{p:?} → {:?}", positions[j]);
                }
                (false, false) => {}
            }
        }
    }
    assert!(walls.blocked_pairs() > 0 && walls.blocked_pairs() < walls.tested_pairs());
    // The low end of a face segment carries where its wall is.
    let at = |p: [f32; 3], axis: usize| ProbeFlags::wall_at(walls.probe_flags(index_at(&layout, p)), axis);
    let close = |got: Option<f32>, want: f32| got.is_some_and(|t| (t - want).abs() <= 1.0 / 30.0 + 1e-5);
    assert!(
        close(at([1.5, 1.5, 3.5], 0), 0.22),
        "{:?}",
        at([1.5, 1.5, 3.5], 0)
    );
    assert_eq!((at([1.5, 1.5, 3.5], 1), at([1.5, 1.5, 3.5], 2)), (None, None));
    assert_eq!(
        walls.probe_flags(index_at(&layout, [2.5, 1.5, 3.5])),
        0,
        "a room probe"
    );
    assert!(close(at([2.5, 3.5, 2.5], 1), 0.3), "the ceiling");
    assert!(close(at([5.5, 1.5, 3.5], 0), 0.33), "the far wall");
    assert!(close(at([3.5, 1.5, 1.5], 2), 0.8), "the near z wall");
    assert!(walls.walled_probes() > 0);
}

/// Inside the closed box the floor next to a wall reads the room's light; the same volume
/// without the wall bits lets the bright outdoor probes behind the wall in.
#[test]
fn the_floor_inside_a_closed_box_reads_no_outdoor_light() {
    let layout = room_layout();
    let geometry = LeakGeometry::from_triangles(triangles(&room_rects(false)), None);
    let walls = NeighborWalls::scan(&layout, &layout.nominal_positions(), &geometry);
    let light = |p: [f32; 3]| if in_room(p) { DARK } else { BRIGHT };
    let walled = volume(&layout, Some(&walls), light);
    let bare = volume(&layout, None, light);
    assert_eq!(walled.wall_probe_count(), walls.walled_probes());
    assert_eq!(bare.wall_probe_count(), 0);

    // The floor 8 cm inside the wall: nearer the outdoor probes (x = 1.5) than its own (2.5).
    let floor = [1.8, 0.0, 4.3];
    let leak = read(&bare, floor, UP);
    assert!(leak > 0.2, "the control should leak: {leak}");
    let sealed = read(&walled, floor, UP);
    assert!(near(sealed, DARK), "{sealed}");
    // The inner face of the wall, and a point in the middle of the room.
    let face = read(&walled, [1.72, 2.2, 4.3], [1.0, 0.0, 0.0]);
    assert!(near(face, DARK), "{face}");
    assert!(near(read(&walled, [4.0, 2.0, 4.0], UP), DARK));
    // Outside, the outer face of the same wall reads the outdoor light.
    let outer = read(&walled, [1.72, 2.2, 4.3], [-1.0, 0.0, 0.0]);
    assert!(near(outer, BRIGHT), "{outer}");
    // The ground outside next to the wall is not darkened by the room behind it.
    let apron = [1.65, 0.0, 4.3];
    let shade = read(&bare, apron, UP);
    assert!(shade < 0.95, "the control should darken: {shade}");
    let lit = read(&walled, apron, UP);
    assert!(near(lit, BRIGHT), "{lit}");
}

/// A doorway keeps the probes on both sides joined: light comes in at the door, and not along
/// the solid part of the same wall.
#[test]
fn a_door_gap_lets_the_outdoor_light_in_at_the_door_only() {
    let layout = room_layout();
    let geometry = LeakGeometry::from_triangles(triangles(&room_rects(true)), None);
    let positions = layout.nominal_positions();
    let walls = NeighborWalls::scan(&layout, &positions, &geometry);
    let inside = reachable(&layout, &walls, index_at(&layout, [3.5, 1.5, 3.5]));
    assert!(inside[index_at(&layout, [1.5, 0.5, 3.5])], "out through the door");
    assert!(inside.iter().all(|&r| r), "one open space");
    assert!(
        !walls.blocked(index_at(&layout, [1.5, 1.5, 3.5]), [1, 0, 0]),
        "the doorway"
    );
    assert!(
        walls.blocked(index_at(&layout, [1.5, 2.5, 3.5]), [1, 0, 0]),
        "above the door"
    );
    assert!(
        walls.blocked(index_at(&layout, [1.5, 0.5, 4.5]), [1, 0, 0]),
        "beside it"
    );

    let walled = volume(&layout, Some(&walls), |p| if in_room(p) { DARK } else { BRIGHT });
    let at_door = read(&walled, [1.8, 0.0, 3.5], UP);
    assert!(at_door > 0.2, "the doorway gets outdoor light: {at_door}");
    let along_wall = read(&walled, [1.8, 0.0, 5.3], UP);
    assert!(
        near(along_wall, DARK),
        "the solid wall keeps it out: {along_wall}"
    );
}

/// A 10 cm partition between two probe planes walls them off from each other: each side of it
/// reads its own light, and the filter does not average across it.
#[test]
fn a_thin_wall_between_two_probes_holds() {
    let slab = [
        rect(0, 1.45, (-1.0, 3.0), (-1.0, 3.0)),
        rect(0, 1.55, (-1.0, 3.0), (-1.0, 3.0)),
    ];
    let geometry = LeakGeometry::from_triangles(triangles(&slab), None);
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: vec![detail_brick([0.0; 3], [3.0, 1.0, 1.0], 1.0)],
        moment_resolution: 4,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    let positions = layout.nominal_positions();
    let walls = NeighborWalls::scan(&layout, &positions, &geometry);
    // Every segment between x = 1 and x = 2 crosses (4 straight, 4 + 4 diagonal in y or z,
    // 4 diagonal in both).
    assert_eq!(walls.blocked_pairs(), 16);
    let light = |p: [f32; 3]| if p[0] > 1.5 { DARK } else { BRIGHT };
    let walled = volume(&layout, Some(&walls), light);
    let bare = volume(&layout, None, light);
    let mid = [1.7, 0.5, 0.5];
    assert!(read(&bare, mid, UP) > 0.2, "the control should leak");
    assert!(near(read(&walled, mid, UP), DARK));
    assert!(near(read(&walled, [1.55, 0.5, 0.5], [1.0, 0.0, 0.0]), DARK));
    assert!(near(read(&walled, [1.45, 0.5, 0.5], [-1.0, 0.0, 0.0]), BRIGHT));
    // The partition's middle: half way along the segment from x = 1.
    let wall = ProbeFlags::wall_at(walls.probe_flags(index_at(&layout, [1.0, 0.0, 0.0])), 0);
    assert!(wall.is_some_and(|t| (t - 0.5).abs() <= 1.0 / 30.0), "{wall:?}");

    // The filter: the dark side stays dark with the walls, and picks up the bright side without.
    let raw = probes(&layout, Some(&walls), light);
    let dark = index_at(&layout, [2.0, 0.0, 0.0]);
    let l0 = |p: &ProbeData| p.sh[1][0][0];
    let kept = filter_probes_with(&layout, &positions, &raw, |i, step| walls.blocked(i, step));
    assert!((l0(&kept[dark]) - l0(&raw[dark])).abs() < 1e-6);
    let smeared = filter_probes(&layout, &positions, &raw);
    assert!(l0(&smeared[dark]) > 2.0 * l0(&raw[dark]));
}

/// A probe inside a solid block is cut off from all 26 neighbors (and they from it), and from
/// nothing else.
#[test]
fn a_probe_inside_a_solid_block_is_walled_off_from_every_neighbor() {
    // Off-center, so no segment passes exactly through one of its edges.
    let (lo, hi) = ([0.55, 0.62, 0.58], [1.35, 1.42, 1.38]);
    let mut cube = Vec::new();
    for axis in 0..3 {
        let (a, b) = ((axis + 1) % 3, (axis + 2) % 3);
        for at in [lo[axis], hi[axis]] {
            cube.push(rect(axis, at, (lo[a], hi[a]), (lo[b], hi[b])));
        }
    }
    let geometry = LeakGeometry::from_triangles(triangles(&cube), None);
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: vec![detail_brick([0.0; 3], [2.0; 3], 1.0)],
        moment_resolution: 4,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    let positions = layout.nominal_positions();
    let walls = NeighborWalls::scan(&layout, &positions, &geometry);
    let center = index_at(&layout, [1.0; 3]);
    assert_eq!(walls.blocked_pairs(), 26);
    for (j, step) in layout.neighbors(center) {
        assert!(walls.blocked(center, step));
        assert!(walls.blocked(j, [-step[0], -step[1], -step[2]]));
    }
    // The center carries its three forward walls (where it leaves the block); the three
    // neighbors below it along each axis carry theirs (where they enter it).
    let near_t = |flags: u16, axis: usize, t: f32| {
        ProbeFlags::wall_at(flags, axis).is_some_and(|w| (w - t).abs() <= 1.0 / 30.0 + 1e-5)
    };
    let c = walls.probe_flags(center);
    assert!(
        near_t(c, 0, 0.35) && near_t(c, 1, 0.42) && near_t(c, 2, 0.38),
        "{c:#x}"
    );
    let below = |p: [f32; 3]| walls.probe_flags(index_at(&layout, p));
    assert!(near_t(below([0.0, 1.0, 1.0]), 0, 0.55));
    assert!(near_t(below([1.0, 0.0, 1.0]), 1, 0.62));
    assert!(near_t(below([1.0, 1.0, 0.0]), 2, 0.58));
    assert_eq!(below([2.0, 1.0, 1.0]), 0);
    assert_eq!(walls.walled_probes(), 4);
}

/// Terrain-layer probes standing inside a building do not darken the ground outside its walls
/// (the terrain layer's flags use the level as the Y axis).
#[test]
fn terrain_probes_inside_a_room_do_not_darken_the_ground_outside() {
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [0.0, 0.0],
            spacing: [2.0, 2.0],
            counts: [6, 6],
            levels: vec![1.0, 2.0, 4.0],
            ground: vec![10.0; 36],
        }),
        bricks: Vec::new(),
        moment_resolution: 4,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    // A hut 3..7 × 10..13 × 3..7: columns x, z ∈ {4, 6}, levels 11 and 12 are inside.
    let hut = |p: [f32; 3]| p[0] > 3.0 && p[0] < 7.0 && p[1] < 13.0 && p[2] > 3.0 && p[2] < 7.0;
    let rects = [
        rect(0, 3.0, (10.0, 13.0), (3.0, 7.0)),
        rect(0, 7.0, (10.0, 13.0), (3.0, 7.0)),
        rect(2, 3.0, (3.0, 7.0), (10.0, 13.0)),
        rect(2, 7.0, (3.0, 7.0), (10.0, 13.0)),
        rect(1, 13.0, (3.0, 7.0), (3.0, 7.0)),
    ];
    let geometry = LeakGeometry::from_triangles(triangles(&rects), None);
    let positions = layout.nominal_positions();
    let walls = NeighborWalls::scan(&layout, &positions, &geometry);
    let terrain = layout.terrain.as_ref().expect("terrain");
    // The roof sits between levels 12 and 14 of an inside column: a Y (level) wall.
    let under_roof = terrain.index(2, 2, 1);
    assert_eq!(positions[under_roof], [4.0, 12.0, 4.0]);
    let roof = ProbeFlags::wall_at(walls.probe_flags(under_roof), 1);
    assert!(roof.is_some_and(|t| (t - 0.5).abs() <= 1.0 / 30.0), "{roof:?}");
    assert!(walls.blocked(terrain.index(2, 2, 2), [0, -1, 0]));
    assert_eq!(walls.probe_flags(terrain.index(1, 2, 2)), 0, "above the eaves");
    // The wall at x = 3 sits half way between the columns at x = 2 and 4 (the X code of the
    // low column, at the levels below the eaves).
    let side = ProbeFlags::wall_at(walls.probe_flags(terrain.index(1, 2, 0)), 0);
    assert!(side.is_some_and(|t| (t - 0.5).abs() <= 1.0 / 30.0), "{side:?}");

    let light = |p: [f32; 3]| if hut(p) { DARK } else { BRIGHT };
    let walled = volume(&layout, Some(&walls), light);
    let bare = volume(&layout, None, light);
    let apron = [2.8, 10.0, 5.0];
    assert!(read(&bare, apron, UP) < 0.8, "the control should darken");
    assert!(near(read(&walled, apron, UP), BRIGHT));
    // And inside, the floor next to the wall reads the hut's light.
    let floor = [3.2, 10.0, 5.0];
    assert!(read(&bare, floor, UP) > 0.2, "the control should leak");
    assert!(near(read(&walled, floor, UP), DARK));
}

/// A ridge of the terrain blocks the segment between two probes on either side of it.
#[test]
fn a_terrain_ridge_walls_off_the_far_valley() {
    let hf = regia_pt::PtHeightfield {
        width: 5,
        depth: 2,
        origin: [0.0, 0.0],
        spacing: [1.0, 1.0],
        heights: vec![0.0, 0.0, 3.0, 0.0, 0.0, 0.0, 0.0, 3.0, 0.0, 0.0],
        material: 0,
    };
    let geometry = LeakGeometry::from_triangles(Vec::new(), Some(&hf));
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: vec![DetailBrick {
            origin: [0.0, 1.0, 0.0],
            spacing: [4.0, 4.0, 1.0],
            counts: [2, 2, 2],
            first_probe: 0,
            flags: 0,
        }],
        moment_resolution: 4,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    let walls = NeighborWalls::scan(&layout, &layout.nominal_positions(), &geometry);
    // Low probes (1 m) across the 3 m ridge: blocked; high ones (5 m): open.
    assert!(walls.blocked(index_at(&layout, [0.0, 1.0, 0.0]), [1, 0, 0]));
    assert!(!walls.blocked(index_at(&layout, [0.0, 5.0, 0.0]), [1, 0, 0]));
}

/// K translated copies of one wall share that wall's triangles. Each copy still blocks a
/// segment through it, and a segment that misses every copy stays open.
#[test]
fn copied_walls_share_one_mesh() {
    let copies = 6usize;
    let mut scene = PtScene::new(PtCamera::pinhole([0.0; 3], [0.0, 0.0, -1.0], 1.0, 4, 4));
    let material = scene.add_material(PtMaterial::diffuse([0.5; 3]));
    // A quad in the plane x = 0.
    let wall = scenes::quad([
        [0.0, -1.0, -1.0],
        [0.0, -1.0, 1.0],
        [0.0, 1.0, 1.0],
        [0.0, 1.0, -1.0],
    ]);
    let id = scene.add_mesh(wall);
    for k in 0..copies {
        let x = k as f32 * 3.0;
        scene.add_instance(id, material, Mat4::from_translation(Vec3::new(x, 0.0, 0.0)));
    }
    // One copy turned 90° about Y and parked on z = 10, so the ray transform is not a pure
    // translation. The quad's plane was x = 0; after the turn it is z = 0, then moved to z = 10.
    scene.add_instance(
        id,
        material,
        Mat4::from_translation(Vec3::new(0.0, 0.0, 10.0))
            * Mat4::from_rotation_y(std::f32::consts::FRAC_PI_2),
    );
    let geometry = LeakGeometry::from_scene(&scene);
    assert_eq!(
        geometry.triangle_count(),
        2,
        "stored triangles grew with the instance count"
    );
    for k in 0..copies {
        let x = k as f32 * 3.0;
        assert!(
            geometry.segment_blocked([x - 0.5, 0.0, 0.0], [x + 0.5, 0.0, 0.0]),
            "copy {k}"
        );
    }
    assert!(
        !geometry.segment_blocked([0.2, 0.0, 0.0], [2.8, 0.0, 0.0]),
        "the gap between the first two copies"
    );
    assert!(
        geometry.segment_blocked([0.0, 0.0, 9.5], [0.0, 0.0, 10.5]),
        "the rotated copy"
    );
    assert!(
        !geometry.segment_blocked([0.0, 0.0, 9.5], [0.0, 3.0, 10.5]),
        "over the rotated copy"
    );
}
