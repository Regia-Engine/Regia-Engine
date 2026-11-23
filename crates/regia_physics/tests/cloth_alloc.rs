// SPDX-License-Identifier: MPL-2.0
//! Spec 55 test 18: stepping cloth and soft bodies allocates nothing after warm-up.
//!
//! Its own test binary: it installs a counting global allocator (the pattern of
//! `regia_framework/tests/feedback_cost.rs`; ADR-1672 names it).

use regia_core::math::{Isometry, Vec3};
use regia_physics::cloth::{
    ClothCollider, ClothConfig, ClothGrid, ClothShape, ClothSolver, ClothTuning, Gust, PoseMaps, PoseTarget,
    SelfCollision, SnapshotKind, SnapshotShape, SoftModel, SoftParams, SoftSolver, StrainCap, TearParams,
    WorldSnapshot,
};

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

fn allocations() -> usize {
    alloc_counter::thread_allocs() as usize
}

fn loaded_cloth() -> ClothSolver {
    let mut s = ClothSolver::new(ClothConfig::default()).expect("config");
    let h = s
        .add_grid(&ClothGrid {
            origin: Vec3::new(0.0, 2.0, 0.0),
            u_axis: Vec3::X,
            v_axis: Vec3::Z,
            columns: 12,
            rows: 12,
            spacing: 0.05,
            particle_mass: 0.05,
            stretch_compliance: 0.0,
            shear_compliance: 1e-4,
            bend_compliance: 1e-2,
        })
        .expect("grid");
    s.add_collider(ClothCollider {
        shape: ClothShape::Sphere {
            center: Vec3::new(0.3, 1.6, 0.3),
            radius: 0.2,
        },
        friction: 0.3,
    })
    .expect("sphere");
    s.set_tuning(ClothTuning {
        strain_cap: Some(StrainCap { max: 1.1, min: 0.5 }),
        tether: Some(1.1),
        self_collision: SelfCollision::Spheres { radius: 0.01 },
        tear: Some(TearParams {
            strain: 5.0,
            max_per_step: 2,
        }),
        ..ClothTuning::default()
    })
    .expect("tuning");
    let n = s.particles().len();
    let mut reach = vec![0.3; n];
    for c in 0..12 {
        reach[h.index(c, 0).expect("i")] = 0.0;
    }
    s.set_pose_maps(PoseMaps {
        reach,
        pose_pull: vec![0.2; n],
        ..PoseMaps::default()
    })
    .expect("maps");
    let targets: Vec<PoseTarget> = s
        .particles()
        .iter()
        .map(|p| PoseTarget {
            position: p.position,
            normal: Vec3::Y,
        })
        .collect();
    s.set_pose_targets(&targets, &targets).expect("targets");
    s.set_wind(Vec3::new(0.0, 0.0, 6.0)).expect("wind");
    s.set_gust(Some(Gust {
        amplitude: 0.3,
        period_steps: 10,
        seed: 1,
    }))
    .expect("gust");
    s
}

#[test]
fn stepping_allocates_nothing_after_warm_up() {
    let mut cloth = loaded_cloth();
    let mut out = Vec::new();
    for _ in 0..10 {
        cloth.step(1.0 / 60.0).expect("step");
        cloth.output(&mut out);
    }
    let before = allocations();
    for _ in 0..50 {
        cloth.step(1.0 / 60.0).expect("step");
        cloth.output(&mut out);
    }
    assert_eq!(allocations() - before, 0, "cloth step allocated");

    let positions = [Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z, Vec3::new(1.0, 1.0, 1.0)];
    let mut soft = SoftSolver::new(
        &positions,
        SoftModel::Tetra {
            tets: vec![[0, 1, 2, 3], [1, 4, 2, 3]],
            volume_compliance: 0.0,
        },
        SoftParams::default(),
    )
    .expect("tets");
    let mut shape = SoftSolver::new(
        &positions,
        SoftModel::ShapeMatch {
            clusters: vec![vec![0, 1, 2, 3, 4]],
            stiffness: 0.5,
            beta: 0.2,
        },
        SoftParams::default(),
    )
    .expect("cluster");
    for _ in 0..10 {
        soft.step(1.0 / 60.0).expect("step");
        shape.step(1.0 / 60.0).expect("step");
    }
    let before = allocations();
    for _ in 0..50 {
        soft.step(1.0 / 60.0).expect("step");
        shape.step(1.0 / 60.0).expect("step");
    }
    assert_eq!(allocations() - before, 0, "soft step allocated");

    vertex_triangle_and_layers_allocate_nothing_after_warm_up();
    stiff_projection_and_guard_allocate_nothing_after_warm_up();
    pressure_and_snapshots_allocate_nothing_after_warm_up();
}

/// Spec 55 test 18 for the `Pressure` soft model and world snapshots (box, sphere and a
/// triangle floor under the hanging sheet), which the other cases do not step. Called from the one test of this binary.
fn pressure_and_snapshots_allocate_nothing_after_warm_up() {
    // An octahedron, counter-clockwise seen from outside.
    let positions = [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z];
    let mut ball = SoftSolver::new(
        &positions,
        SoftModel::Pressure {
            triangles: vec![
                [0, 2, 4],
                [2, 1, 4],
                [1, 3, 4],
                [3, 0, 4],
                [2, 0, 5],
                [1, 2, 5],
                [3, 1, 5],
                [0, 3, 5],
            ],
            pressure: 1.5,
            volume_compliance: 0.0,
        },
        SoftParams::default(),
    )
    .expect("closed surface");
    for _ in 0..10 {
        ball.step(1.0 / 60.0).expect("step");
    }
    let before = allocations();
    for _ in 0..50 {
        ball.step(1.0 / 60.0).expect("step");
    }
    assert_eq!(allocations() - before, 0, "pressure step allocated");

    let mut cloth = loaded_cloth();
    let at = |key: u64, kind: SnapshotKind, center: Vec3| SnapshotShape {
        key: (key, 0, 0),
        kind,
        prev: Isometry::from_translation(center),
        cur: Isometry::from_translation(center),
        friction: 0.4,
    };
    let snapshot = WorldSnapshot::new(vec![
        at(
            1,
            SnapshotKind::Box {
                half_extents: Vec3::new(0.15, 0.15, 0.15),
            },
            Vec3::new(0.3, 1.65, 0.0),
        ),
        at(2, SnapshotKind::Sphere { radius: 0.1 }, Vec3::new(0.1, 1.8, 0.05)),
        at(
            3,
            SnapshotKind::Triangles {
                triangles: vec![
                    [
                        Vec3::new(-2.0, 1.6, -2.0),
                        Vec3::new(-2.0, 1.6, 2.0),
                        Vec3::new(2.0, 1.6, 2.0),
                    ],
                    [
                        Vec3::new(-2.0, 1.6, -2.0),
                        Vec3::new(2.0, 1.6, 2.0),
                        Vec3::new(2.0, 1.6, -2.0),
                    ],
                ],
            },
            Vec3::ZERO,
        ),
    ])
    .expect("snapshot");
    cloth.set_snapshot(snapshot);
    let mut out = Vec::new();
    for _ in 0..120 {
        cloth.step(1.0 / 60.0).expect("step");
        cloth.output(&mut out);
    }
    let before = allocations();
    for _ in 0..50 {
        cloth.step(1.0 / 60.0).expect("step");
        cloth.output(&mut out);
    }
    assert_eq!(allocations() - before, 0, "snapshot collision allocated");
}

/// Spec 55 test 18 for the ADR-2425 paths: vertex-triangle self-collision with garment layers
/// allocates nothing per step once the contact set has settled. Called from the one test of
/// this binary (the allocation counter is process-wide, so tests must not run side by side).
fn vertex_triangle_and_layers_allocate_nothing_after_warm_up() {
    let mut cloth = loaded_cloth();
    let tuning = ClothTuning {
        self_collision: SelfCollision::VertexTriangle { radius: 0.01 },
        ..*cloth.tuning()
    };
    cloth.set_tuning(tuning).expect("tuning");
    let half = cloth.particles().len() / 2;
    cloth
        .set_layer_range(half, cloth.particles().len() - half, 1)
        .expect("layers");
    let mut out = Vec::new();
    for _ in 0..300 {
        cloth.step(1.0 / 60.0).expect("step");
        cloth.output(&mut out);
    }
    let before = allocations();
    for _ in 0..50 {
        cloth.step(1.0 / 60.0).expect("step");
        cloth.output(&mut out);
    }
    assert_eq!(allocations() - before, 0, "vertex-triangle step allocated");
}

/// ADR-3273: the spec 55 § 10 sheet (32 x 32 at 0.03 m, 4 substeps, 1 iteration), where the stiff
/// projection runs on most substeps and the guard copies the particles every step, allocates
/// nothing per step after warm-up. Called from the one test of this binary.
fn stiff_projection_and_guard_allocate_nothing_after_warm_up() {
    let mut s = ClothSolver::new(ClothConfig {
        substeps: 4,
        iterations: 1,
        ..ClothConfig::default()
    })
    .expect("config");
    let h = s
        .add_grid(&ClothGrid {
            origin: Vec3::new(0.0, 1.5, 0.0),
            u_axis: Vec3::X,
            v_axis: Vec3::Z,
            columns: 32,
            rows: 32,
            spacing: 0.03,
            particle_mass: 0.05,
            stretch_compliance: 0.0,
            shear_compliance: 1e-4,
            bend_compliance: 1e-2,
        })
        .expect("grid");
    for c in 0..32 {
        s.pin(h.index(c, 0).expect("row 0")).expect("pin");
    }
    for _ in 0..10 {
        s.step(1.0 / 60.0).expect("step");
    }
    let projections = s.health().stiff_projections;
    let before = allocations();
    for _ in 0..50 {
        s.step(1.0 / 60.0).expect("step");
    }
    assert_eq!(allocations() - before, 0, "stiff projection or guard allocated");
    assert!(s.health().stiff_projections > projections, "the projection ran");
}
