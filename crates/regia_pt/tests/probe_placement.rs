// SPDX-License-Identifier: MPL-2.0
//! Probe placement (`regia_pt::placement`, baked light phase 3b, ADR-418) on synthetic scenes,
//! CPU only: adaptive bricks are finer near geometry and leave no coverage seam under the
//! runtime's deepest-brick rule, solid and under-terrain blocks are dropped, budgets drop levels,
//! probes inside solids are moved out or culled, offsets never cross a surface, and everything is
//! deterministic.

use regia_asset::baked_gi::{DetailBrick, ProbeFlags, VolumeLayout};
use regia_core::glam::{Mat4, Vec3};
use regia_pt::placement::{
    AdaptiveSettings, PlacementGeometry, PlacementSettings, adaptive_bricks, fibonacci_directions,
    place_probes,
};
use regia_pt::{PtCamera, PtHeightfield, PtMaterial, PtMesh, PtScene, PtSky};

/// The 12 triangles of a closed box `lo..hi`, wound so every face points out.
fn cube_tris(lo: [f32; 3], hi: [f32; 3]) -> Vec<[Vec3; 3]> {
    let c = |x: usize, y: usize, z: usize| Vec3::new([lo[0], hi[0]][x], [lo[1], hi[1]][y], [lo[2], hi[2]][z]);
    let faces = [
        [c(0, 0, 0), c(0, 0, 1), c(0, 1, 1), c(0, 1, 0)],
        [c(1, 0, 0), c(1, 1, 0), c(1, 1, 1), c(1, 0, 1)],
        [c(0, 0, 0), c(1, 0, 0), c(1, 0, 1), c(0, 0, 1)],
        [c(0, 1, 0), c(0, 1, 1), c(1, 1, 1), c(1, 1, 0)],
        [c(0, 0, 0), c(0, 1, 0), c(1, 1, 0), c(1, 0, 0)],
        [c(0, 0, 1), c(1, 0, 1), c(1, 1, 1), c(0, 1, 1)],
    ];
    faces
        .iter()
        .flat_map(|q| [[q[0], q[1], q[2]], [q[0], q[2], q[3]]])
        .collect()
}

/// A traceable scene of closed boxes under a uniform sky.
fn box_scene(boxes: &[([f32; 3], [f32; 3])], terrain: Option<PtHeightfield>) -> PtScene {
    let mut scene = PtScene::new(PtCamera::pinhole([0.0, 2.0, 10.0], [0.0; 3], 1.0, 4, 4));
    scene.sky = PtSky::Uniform([1.0; 3]);
    scene.terrain = terrain;
    let material = scene.add_material(PtMaterial::diffuse([0.5; 3]));
    for (lo, hi) in boxes {
        let tris = cube_tris(*lo, *hi);
        let positions: Vec<[f32; 3]> = tris.iter().flatten().map(|v| v.to_array()).collect();
        let mesh = PtMesh {
            normals: vec![[0.0; 3]; positions.len()],
            indices: (0..positions.len() as u32).collect(),
            positions,
            ..PtMesh::default()
        };
        let mesh = scene.add_mesh(mesh);
        scene.add_instance(mesh, material, Mat4::IDENTITY);
    }
    scene
}

fn flat_terrain(height: f32, origin: [f32; 2], size: u32, spacing: f32) -> PtHeightfield {
    PtHeightfield {
        width: size,
        depth: size,
        origin,
        spacing: [spacing; 2],
        heights: vec![height; (size * size) as usize],
        material: 0,
    }
}

/// The runtime's brick choice (`bgi_brick` in `baked_gi.wgsl`, no interior bricks): the brick
/// holding `p` deepest, in its own cells. Returns its index and that depth.
fn select(bricks: &[DetailBrick], p: [f32; 3]) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for (i, b) in bricks.iter().enumerate() {
        let inset = (0..3)
            .map(|a| {
                let g = (p[a] - b.origin[a]) / b.spacing[a];
                g.min(b.counts[a] as f32 - 1.0 - g)
            })
            .fold(f32::INFINITY, f32::min);
        if inset > best.map_or(0.0, |(_, k)| k) {
            best = Some((i, inset));
        }
    }
    best
}

fn inside_box(p: [f32; 3], lo: [f32; 3], hi: [f32; 3], grow: f32) -> bool {
    (0..3).all(|a| p[a] >= lo[a] - grow && p[a] <= hi[a] + grow)
}

fn lattice(lo: [f32; 3], hi: [f32; 3], step: f32) -> Vec<[f32; 3]> {
    let n: [usize; 3] = std::array::from_fn(|a| ((hi[a] - lo[a]) / step).floor() as usize + 1);
    let mut out = Vec::new();
    for k in 0..n[2] {
        for j in 0..n[1] {
            for i in 0..n[0] {
                out.push([
                    lo[0] + i as f32 * step,
                    lo[1] + j as f32 * step,
                    lo[2] + k as f32 * step,
                ]);
            }
        }
    }
    out
}

const PILLAR: ([f32; 3], [f32; 3]) = ([-1.0, 0.0, -1.0], [1.0, 3.0, 1.0]);

fn pillar_settings() -> AdaptiveSettings {
    AdaptiveSettings {
        levels: 2,
        block_cells: 4,
        margin: 0.25,
        ..AdaptiveSettings::new([-16.0, 0.0, -16.0], [16.0, 8.0, 16.0], 2.0)
    }
}

/// Near the pillar the runtime reads the finest bricks (0.5 m), far from it the coarsest (2 m),
/// and the probe count is far below a uniform fine grid over the region.
#[test]
fn adaptive_bricks_are_finer_near_geometry() {
    let scene = box_scene(&[PILLAR], None);
    let g = PlacementGeometry::from_scene(&scene);
    assert_eq!(g.triangle_count(), 12);
    let placed = adaptive_bricks(&g, &pillar_settings()).expect("bricks");
    let r = &placed.report;
    assert_eq!(r.levels, 2);
    assert_eq!(r.spacings, vec![2.0, 1.0, 0.5]);
    assert!(r.leaf_blocks[2] > 0 && r.leaf_blocks[0] > 0, "{r:?}");
    assert_eq!(placed.bricks.len(), placed.brick_levels.len());
    assert_eq!(
        r.probes,
        placed.bricks.iter().map(DetailBrick::probe_count).sum::<usize>()
    );
    for near in [[1.3, 1.5, 0.0], [0.0, 3.3, 0.2], [-1.2, 0.5, -1.2]] {
        let (b, _) = select(&placed.bricks, near).expect("covered");
        assert_eq!(placed.bricks[b].spacing, [0.5; 3], "near {near:?}");
    }
    for far in [[-14.0, 4.0, -14.0], [13.0, 6.0, 12.0]] {
        let (b, _) = select(&placed.bricks, far).expect("covered");
        assert_eq!(placed.bricks[b].spacing, [2.0; 3], "far {far:?}");
    }
    // A uniform 0.5 m grid over the region holds 65 × 17 × 65 probes.
    assert!(r.probes * 4 < 65 * 17 * 65, "{} probes", r.probes);
}

/// Every point of the region outside the solid is held at least one cell deep by the brick the
/// runtime picks there, so its coverage is 1: the merged, padded bricks leave no seam between
/// levels or between the boxes of one level.
#[test]
fn adaptive_bricks_leave_no_coverage_seam() {
    let scene = box_scene(&[PILLAR], None);
    let g = PlacementGeometry::from_scene(&scene);
    let s = pillar_settings();
    let placed = adaptive_bricks(&g, &s).expect("bricks");
    // An off-grid step so the samples land between probes and on block faces alike.
    let points = lattice(s.min, s.max, 0.37);
    assert!(points.len() > 50_000);
    let mut checked = 0usize;
    for p in points {
        if inside_box(p, PILLAR.0, PILLAR.1, 0.0) {
            continue;
        }
        let (_, depth) = select(&placed.bricks, p).expect("covered");
        assert!(depth >= 1.0 - 1e-4, "{p:?} is held only {depth} cells deep");
        checked += 1;
    }
    assert!(checked > 50_000);
}

/// A big solid: the blocks deep inside it are dropped, no kept brick's own region lies wholly
/// inside it more than a cell from its faces, and turning culling off keeps more probes.
#[test]
fn blocks_inside_a_solid_are_culled() {
    let solid = ([-12.0, -6.0, -12.0], [12.0, 10.0, 12.0]);
    let scene = box_scene(&[solid], None);
    let g = PlacementGeometry::from_scene(&scene);
    let s = AdaptiveSettings {
        levels: 1,
        block_cells: 2,
        max_bricks: 100_000,
        ..AdaptiveSettings::new([-16.0, -8.0, -16.0], [16.0, 16.0, 16.0], 2.0)
    };
    let culled = adaptive_bricks(&g, &s).expect("bricks");
    assert_eq!(culled.report.levels, 1);
    assert!(culled.report.culled_solid > 0, "{:?}", culled.report);
    for (b, &level) in culled.bricks.iter().zip(&culled.brick_levels) {
        let sp = b.spacing[0];
        let lo: [f32; 3] = std::array::from_fn(|a| b.origin[a] + sp);
        let hi: [f32; 3] = std::array::from_fn(|a| b.origin[a] + (b.counts[a] - 2) as f32 * sp);
        let deep = (0..3).all(|a| lo[a] > solid.0[a] + sp && hi[a] < solid.1[a] - sp);
        assert!(
            !deep,
            "level {level} brick {lo:?}..{hi:?} lies deep inside the solid"
        );
    }
    let kept = adaptive_bricks(
        &g,
        &AdaptiveSettings {
            cull_solid: false,
            ..s
        },
    )
    .expect("bricks");
    assert_eq!(kept.report.culled_solid, 0);
    assert!(kept.report.probes > culled.report.probes);
}

/// Blocks wholly under a flat terrain are dropped; nothing above it is.
#[test]
fn blocks_under_the_terrain_are_culled() {
    let scene = box_scene(&[], Some(flat_terrain(9.0, [-20.0, -20.0], 41, 1.0)));
    let g = PlacementGeometry::from_scene(&scene);
    let s = AdaptiveSettings {
        levels: 0,
        block_cells: 2,
        ..AdaptiveSettings::new([-16.0, 0.0, -16.0], [16.0, 16.0, 16.0], 2.0)
    };
    let placed = adaptive_bricks(&g, &s).expect("bricks");
    // Blocks of 4 m: y 0..4 lies wholly under 9 m with its pad; y 4..8 reaches 10 m padded.
    assert_eq!(placed.report.culled_under_terrain, 8 * 8);
    for b in &placed.bricks {
        let top = b.origin[1] + (b.counts[1] - 1) as f32 * b.spacing[1];
        assert!(top > 9.0, "a brick ends under the ground at {top}");
    }
    assert!(!placed.bricks.is_empty());
}

/// Over `max_probes` or `max_bricks` the finest level is dropped until the bricks fit; when even
/// level 0 does not fit the error is `placement_budget`.
#[test]
fn budgets_drop_the_finest_levels() {
    let scene = box_scene(&[PILLAR], None);
    let g = PlacementGeometry::from_scene(&scene);
    let s = pillar_settings();
    let full = adaptive_bricks(&g, &s).expect("full");
    let capped = adaptive_bricks(
        &g,
        &AdaptiveSettings {
            max_probes: full.report.probes - 1,
            ..s
        },
    )
    .expect("capped");
    assert!(capped.report.levels < 2, "{:?}", capped.report);
    assert_eq!(capped.report.requested_levels, 2);
    assert!(capped.report.probes < full.report.probes);
    // Level 0 alone merges the whole region into one brick: one brick is enough without levels.
    let one = adaptive_bricks(&g, &AdaptiveSettings { max_bricks: 1, ..s }).expect("one brick");
    assert_eq!((one.report.levels, one.bricks.len()), (0, 1));
    let err = adaptive_bricks(&g, &AdaptiveSettings { max_probes: 10, ..s }).expect_err("tiny budget");
    assert_eq!(err.code, "placement_budget");
}

#[test]
fn bad_settings_are_placement_invalid() {
    let g = PlacementGeometry::from_triangles(Vec::new(), None);
    let s = pillar_settings();
    for bad in [
        AdaptiveSettings { spacing: 0.0, ..s },
        AdaptiveSettings { levels: 7, ..s },
        AdaptiveSettings { block_cells: 0, ..s },
        AdaptiveSettings { min: s.max, ..s },
        AdaptiveSettings {
            margin: f32::NAN,
            ..s
        },
        AdaptiveSettings {
            inside_fraction: 0.0,
            ..s
        },
        AdaptiveSettings { max_probes: 0, ..s },
    ] {
        assert_eq!(
            adaptive_bricks(&g, &bad).expect_err("invalid").code,
            "placement_invalid"
        );
    }
    // An empty scene is one coarse level, merged into a single brick.
    let empty = adaptive_bricks(&g, &s).expect("empty");
    assert_eq!(empty.bricks.len(), 1);
    assert_eq!(empty.report.leaf_blocks, vec![4 * 4, 0, 0]);
}

/// A region whose level-0 block count overflows a `usize` product is refused, not wrapped.
#[test]
fn adaptive_block_count_overflow_is_invalid() {
    let g = PlacementGeometry::from_triangles(Vec::new(), None);
    // 2^22 blocks per axis: the true product is 2^66.
    let side = 4_194_304.0;
    let s = AdaptiveSettings {
        block_cells: 1,
        ..AdaptiveSettings::new([0.0; 3], [side; 3], 1.0)
    };
    assert_eq!(
        adaptive_bricks(&g, &s).expect_err("too many blocks").code,
        "placement_invalid"
    );
}

/// The same bricks whatever the triangle order.
#[test]
fn adaptive_bricks_do_not_depend_on_triangle_order() {
    let mut tris = cube_tris(PILLAR.0, PILLAR.1);
    tris.extend(cube_tris([5.0, 0.0, 5.0], [6.0, 1.0, 9.0]));
    let a = adaptive_bricks(
        &PlacementGeometry::from_triangles(tris.clone(), None),
        &pillar_settings(),
    )
    .expect("a");
    tris.reverse();
    tris.rotate_left(5);
    let b = adaptive_bricks(&PlacementGeometry::from_triangles(tris, None), &pillar_settings()).expect("b");
    assert_eq!(a, b);
}

fn brick_layout(origin: [f32; 3], counts: [u32; 3], spacing: f32) -> VolumeLayout {
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: vec![DetailBrick {
            origin,
            spacing: [spacing; 3],
            counts,
            first_probe: 0,
            flags: 0,
        }],
        moment_resolution: 4,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    layout
}

fn index_of(layout: &VolumeLayout, p: [f32; 3]) -> usize {
    layout
        .nominal_positions()
        .iter()
        .position(|q| (0..3).all(|a| (q[a] - p[a]).abs() < 1e-4))
        .expect("a probe at that position")
}

/// Probes inside a solid: one a cell deep is culled, one 0.2 m from a face moves out through it;
/// after placement no valid probe sits inside the solid and every offset is within 0.45 cells.
#[test]
fn probes_inside_a_solid_move_out_or_are_culled() {
    let solid = ([0.8, 0.8, 0.8], [3.2, 3.2, 3.2]);
    let scene = box_scene(&[solid], None);
    let g = PlacementGeometry::from_scene(&scene);
    let layout = brick_layout([-1.0; 3], [6, 6, 6], 1.0);
    let settings = PlacementSettings {
        threads: 2,
        ..PlacementSettings::default()
    };
    let placed = place_probes(&layout, &g, &settings);
    let r = &placed.report;
    assert_eq!(r.probes, 216);
    assert_eq!(
        r.kept + r.pushed + r.escaped + r.culled + r.under_terrain,
        r.probes
    );
    let centre = index_of(&layout, [2.0, 2.0, 2.0]);
    assert_eq!(placed.flags[centre], ProbeFlags::INSIDE_GEOMETRY);
    assert_eq!(placed.offsets[centre], [0.0; 3]);
    let near_face = index_of(&layout, [1.0, 2.0, 2.0]);
    assert_eq!(placed.flags[near_face], ProbeFlags::VALID | ProbeFlags::RELOCATED);
    let moved = placed.positions(&layout)[near_face];
    assert!(moved[0] < solid.0[0], "{moved:?}");
    assert!(r.escaped > 0 && r.culled > 0, "{r:?}");
    let dirs = fibonacci_directions(32);
    for (i, p) in placed.positions(&layout).iter().enumerate() {
        for o in placed.offsets[i] {
            assert!(o.abs() <= 0.45 + 1e-5, "probe {i} offset {:?}", placed.offsets[i]);
        }
        if placed.flags[i] & ProbeFlags::VALID != 0 {
            assert!(
                !g.is_inside(*p, &dirs, 16.0, 0.25),
                "valid probe {i} at {p:?} is inside"
            );
        }
    }
}

/// Probes near walls: a probe 3 cm in front of a wall is pushed back to the clearance and stays
/// on its side; a probe inside a thin wall leaves it; a probe in a gap narrower than twice the
/// clearance stays put (moving would bring it closer to the other wall). No probe that started
/// outside crosses a surface on its way to its final position.
#[test]
fn offsets_never_cross_a_wall() {
    let wall_a = ([2.03, -5.0, -5.0], [2.13, 5.0, 5.0]);
    let wall_b = ([4.95, -5.0, -5.0], [5.05, 5.0, 5.0]);
    let gap_left = ([6.85, -5.0, -5.0], [6.95, 5.0, 5.0]);
    let gap_right = ([7.07, -5.0, -5.0], [7.17, 5.0, 5.0]);
    let scene = box_scene(&[wall_a, wall_b, gap_left, gap_right], None);
    let g = PlacementGeometry::from_scene(&scene);
    let layout = brick_layout([0.0, -1.0, -1.0], [10, 3, 3], 1.0);
    let placed = place_probes(&layout, &g, &PlacementSettings::default());
    let positions = placed.positions(&layout);
    let nominal = layout.nominal_positions();
    let dirs = fibonacci_directions(32);
    let front = index_of(&layout, [2.0, 0.0, 0.0]);
    assert_eq!(placed.flags[front], ProbeFlags::VALID | ProbeFlags::RELOCATED);
    // Pushed back along the ray that found the wall, to about the clearance (0.1 m) from it.
    let p = positions[front];
    assert!(p[0] > 1.88 && p[0] < 1.98, "{p:?}");
    assert!(p[1].abs() < 0.06 && p[2].abs() < 0.06, "{p:?}");
    let in_wall = index_of(&layout, [5.0, 0.0, 0.0]);
    assert_eq!(placed.flags[in_wall], ProbeFlags::VALID | ProbeFlags::RELOCATED);
    assert!(!g.is_inside(positions[in_wall], &dirs, 16.0, 0.25));
    let x = positions[in_wall][0];
    assert!(x < wall_b.0[0] || x > wall_b.1[0], "{x}");
    let gap = index_of(&layout, [7.0, 0.0, 0.0]);
    assert_eq!(placed.flags[gap], ProbeFlags::VALID);
    assert_eq!(placed.offsets[gap], [0.0; 3]);
    let open = index_of(&layout, [3.0, 0.0, 0.0]);
    assert_eq!(
        (placed.flags[open], placed.offsets[open]),
        (ProbeFlags::VALID, [0.0; 3])
    );
    for i in 0..nominal.len() {
        if g.is_inside(nominal[i], &dirs, 16.0, 0.25) {
            continue;
        }
        assert!(
            g.segment_hit(nominal[i], positions[i]).is_none(),
            "probe {i} crossed a surface: {:?} -> {:?}",
            nominal[i],
            positions[i]
        );
    }
}

/// Under the terrain a probe is flagged as the bake flags it and not moved.
#[test]
fn probes_under_the_terrain_are_flagged() {
    let scene = box_scene(&[], Some(flat_terrain(1.5, [-4.0, -4.0], 9, 1.0)));
    let g = PlacementGeometry::from_scene(&scene);
    let layout = brick_layout([-2.0, 0.0, -2.0], [5, 4, 5], 1.0);
    let placed = place_probes(&layout, &g, &PlacementSettings::default());
    for (i, p) in layout.nominal_positions().iter().enumerate() {
        if p[1] < 1.5 {
            assert_eq!(placed.flags[i], ProbeFlags::UNDER_TERRAIN, "{p:?}");
            assert_eq!(placed.offsets[i], [0.0; 3]);
        } else {
            assert_ne!(placed.flags[i] & ProbeFlags::VALID, 0, "{p:?}");
        }
    }
    assert_eq!(placed.report.under_terrain, 2 * 25);
    // The probes at 2 m stand 0.5 m above the ground: clear of it, not moved.
    assert_eq!(placed.report.pushed, 0);
}

/// The same offsets and flags whatever the thread count and triangle order.
#[test]
fn placement_is_deterministic() {
    let mut tris = cube_tris([0.8, 0.8, 0.8], [3.2, 3.2, 3.2]);
    tris.extend(cube_tris([4.03, -5.0, -5.0], [4.13, 5.0, 5.0]));
    let layout = brick_layout([-1.0; 3], [7, 6, 6], 1.0);
    let one = place_probes(
        &layout,
        &PlacementGeometry::from_triangles(tris.clone(), None),
        &PlacementSettings {
            threads: 1,
            ..PlacementSettings::default()
        },
    );
    tris.reverse();
    let many = place_probes(
        &layout,
        &PlacementGeometry::from_triangles(tris, None),
        &PlacementSettings {
            threads: 5,
            ..PlacementSettings::default()
        },
    );
    assert_eq!(one, many);
    assert!(one.report.pushed + one.report.escaped > 0);
}

/// Adaptive bricks, then placement, over a floor and a pillar: the layout validates, the
/// placement matches it probe for probe, and no valid probe is inside the pillar.
#[test]
fn adaptive_layout_then_placement() {
    let floor = ([-16.0, -1.0, -16.0], [16.0, 0.0, 16.0]);
    let scene = box_scene(&[floor, PILLAR], None);
    let g = PlacementGeometry::from_scene(&scene);
    let s = AdaptiveSettings {
        levels: 1,
        ..AdaptiveSettings::new([-8.0, 0.0, -8.0], [8.0, 6.0, 8.0], 2.0)
    };
    let placed = adaptive_bricks(&g, &s).expect("bricks");
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: placed.bricks,
        moment_resolution: 4,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    layout.validate().expect("valid layout");
    let probes = place_probes(&layout, &g, &PlacementSettings::default());
    assert_eq!(probes.offsets.len(), layout.probe_count());
    assert_eq!(probes.flags.len(), layout.probe_count());
    let dirs = fibonacci_directions(32);
    for (i, p) in probes.positions(&layout).iter().enumerate() {
        if probes.flags[i] & ProbeFlags::VALID != 0 {
            assert!(!g.is_inside(*p, &dirs, 16.0, 0.25), "probe {i} at {p:?}");
        }
    }
    // The padding layer under the floor's top face is inside the floor slab: culled or moved up.
    assert!(
        probes.report.culled + probes.report.escaped > 0,
        "{:?}",
        probes.report
    );
}
