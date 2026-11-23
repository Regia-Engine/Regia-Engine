// SPDX-License-Identifier: MPL-2.0
//! Cloth extras (spec 55 § 2.4, § 2.6, § 7; ADR-2425): vertex-triangle self-collision, garment
//! layers, reaction torque, swept collider tests and parallel stepping. Fixed steps and counters
//! only; no clock, no GPU, no files.

use regia_core::math::{Isometry, Vec3};
use regia_physics::cloth::{
    ClothCollider, ClothConfig, ClothGrid, ClothShape, ClothSolver, ClothTuning, SelfCollision, SnapshotKind,
    SnapshotShape, WorldSnapshot,
};

const DT: f32 = 1.0 / 60.0;

fn grid(columns: usize, rows: usize, spacing: f32, origin: Vec3, v_axis: Vec3) -> ClothGrid {
    ClothGrid {
        origin,
        u_axis: Vec3::X,
        v_axis,
        columns,
        rows,
        spacing,
        particle_mass: 0.05,
        stretch_compliance: 0.0,
        shear_compliance: 1e-4,
        bend_compliance: 1e-2,
    }
}

fn solver(config: ClothConfig) -> ClothSolver {
    ClothSolver::new(config).expect("config")
}

fn with_self_collision(s: &mut ClothSolver, mode: SelfCollision) {
    s.set_tuning(ClothTuning {
        self_collision: mode,
        ..ClothTuning::default()
    })
    .expect("tuning");
}

/// Two stacked sheets on a floor (the spec 55 test 9 fixture).
fn stacked(mode: SelfCollision, steps: usize) -> ClothSolver {
    let mut s = solver(ClothConfig {
        iterations: 4,
        ..ClothConfig::default()
    });
    let n = 8;
    let spacing = 0.05;
    s.add_grid(&grid(n, n, spacing, Vec3::new(0.0, 0.011, 0.0), Vec3::Z))
        .expect("bottom");
    s.add_grid(&grid(n, n, spacing, Vec3::new(0.0, 0.111, 0.0), Vec3::Z))
        .expect("top");
    s.add_collider(ClothCollider {
        shape: ClothShape::Plane {
            normal: Vec3::Y,
            offset: 0.0,
        },
        friction: 0.5,
    })
    .expect("floor");
    with_self_collision(&mut s, mode);
    for _ in 0..steps {
        s.step(DT).expect("step");
    }
    s
}

/// A slowly moving sphere (per-substep travel well under the thickness) dragging a cloth.
fn slow_sphere(steps: usize) -> ClothSolver {
    let mut s = solver(ClothConfig::default());
    s.add_grid(&grid(8, 8, 0.05, Vec3::new(-0.175, 0.32, -0.175), Vec3::Z))
        .expect("grid");
    let ball = s
        .add_collider(ClothCollider {
            shape: ClothShape::Sphere {
                center: Vec3::ZERO,
                radius: 0.3,
            },
            friction: 0.8,
        })
        .expect("ball");
    let mut center = Vec3::ZERO;
    for _ in 0..steps {
        let prev = center;
        // 0.12 m/s: 0.0005 m per substep at 4 substeps, under the 0.01 m thickness.
        center += Vec3::new(0.002, 0.0, 0.0);
        s.set_collider(
            ball,
            ClothCollider {
                shape: ClothShape::Sphere { center, radius: 0.3 },
                friction: 0.8,
            },
        )
        .expect("move");
        s.set_collider_previous(
            ball,
            ClothShape::Sphere {
                center: prev,
                radius: 0.3,
            },
        )
        .expect("previous");
        s.step(DT).expect("step");
    }
    s
}

/// A cloth dropped onto a static box snapshot with two-way reaction on.
fn on_box(steps: usize) -> ClothSolver {
    let mut s = solver(ClothConfig::default());
    s.add_grid(&grid(10, 10, 0.06, Vec3::new(-0.27, 0.6, -0.27), Vec3::Z))
        .expect("grid");
    s.set_snapshot(
        WorldSnapshot::new(vec![SnapshotShape {
            key: (7, 0, 0),
            kind: SnapshotKind::Box {
                half_extents: Vec3::new(2.0, 0.5, 2.0),
            },
            prev: Isometry::from_translation(Vec3::ZERO),
            cur: Isometry::from_translation(Vec3::ZERO),
            friction: 0.4,
        }])
        .expect("snapshot"),
    );
    s.set_reaction(Some(5.0)).expect("reaction");
    let mut out = Vec::new();
    for _ in 0..steps {
        s.step(DT).expect("step");
        out.clear();
        s.take_reaction(&mut out);
    }
    s
}

/// Digests of the base solver (`origin/integ/v035`, x86_64-linux), measured on the base tree
/// before this lane's changes: `Spheres` self-collision, slow moving colliders and static
/// snapshots must not move by a bit. Re-pinned only with a stated reason.
#[test]
fn spheres_slow_colliders_and_static_snapshots_match_the_base() {
    let spheres = stacked(SelfCollision::Spheres { radius: 0.03 }, 240).state_digest();
    let slow = slow_sphere(120).state_digest();
    let boxed = on_box(240).state_digest();
    eprintln!("base digests: spheres {spheres:#018x}, slow sphere {slow:#018x}, box {boxed:#018x}");
    if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        assert_eq!(spheres, BASE_SPHERES, "Spheres self-collision moved");
        assert_eq!(slow, BASE_SLOW_SPHERE, "a slow moving collider moved");
        assert_eq!(boxed, BASE_BOX, "a static snapshot with reaction moved");
    }
}

const BASE_SPHERES: u64 = 0x3e75_500e_221f_4e43;
const BASE_SLOW_SPHERE: u64 = 0xdf80_106f_674d_c4b4;
const BASE_BOX: u64 = 0xad8b_c282_2bcf_2cb6;

// ---------------------------------------------------------------------------
// Vertex-triangle self-collision
// ---------------------------------------------------------------------------

const FOLD_COLUMNS: usize = 10;
const FOLD_HALF: usize = 8;
const FOLD_SPACING: f32 = 0.05;

/// A sheet folded on itself: rows `0..m` flat at y = 0.3, rows `m..2m` mirrored back over them
/// one spacing higher and thrown down at `speed`. No gravity, no colliders: without
/// self-collision the top half passes through the bottom half.
fn folded(mode: SelfCollision, speed: f32) -> ClothSolver {
    let mut s = solver(ClothConfig {
        gravity: Vec3::ZERO,
        iterations: 4,
        ..ClothConfig::default()
    });
    let mut g = grid(
        FOLD_COLUMNS,
        2 * FOLD_HALF,
        FOLD_SPACING,
        Vec3::new(0.0, 0.3, 0.0),
        Vec3::Z,
    );
    // Nearly no bending stiffness: a flat rest angle would otherwise unfold the 180° fold.
    g.bend_compliance = 1e3;
    let h = s.add_grid(&g).expect("grid");
    for r in FOLD_HALF..2 * FOLD_HALF {
        let mirror = (2 * FOLD_HALF - 1 - r) as f32;
        for c in 0..FOLD_COLUMNS {
            let i = h.index(c, r).expect("index");
            s.set_position(
                i,
                Vec3::new(c as f32 * FOLD_SPACING, 0.3 + FOLD_SPACING, mirror * FOLD_SPACING),
            )
            .expect("fold");
            s.set_velocity(i, Vec3::new(0.0, -speed, 0.0)).expect("throw");
        }
    }
    with_self_collision(&mut s, mode);
    s
}

fn row_of(index: usize) -> usize {
    index / FOLD_COLUMNS
}

/// Barycentric coordinates of the projection of `p` onto triangle `a, b, c`.
fn bary(a: Vec3, b: Vec3, c: Vec3, p: Vec3) -> (f32, f32, f32) {
    let (v0, v1, v2) = (b - a, c - a, p - a);
    let (d00, d01, d11, d20, d21) = (v0.dot(v0), v0.dot(v1), v1.dot(v1), v2.dot(v0), v2.dot(v1));
    let denom = d00 * d11 - d01 * d01;
    let v = (d11 * d20 - d01 * d21) / denom;
    let w = (d00 * d21 - d01 * d20) / denom;
    (1.0 - v - w, v, w)
}

/// Signed distance of `p` to the plane of `t` and whether it projects inside `t`.
fn plane_side(pos: &[Vec3], t: [usize; 3], p: Vec3) -> (f32, bool) {
    let [a, b, c] = t.map(|i| pos[i]);
    let n = (b - a).cross(c - a).normalize();
    let (u, v, w) = bary(a, b, c, p);
    (n.dot(p - a), u >= 0.0 && v >= 0.0 && w >= 0.0)
}

/// Vertex-triangle pairs (`vertex in group A`, `triangle in group B` and the reverse) that went
/// through the triangle between `start` and `now`: inside its prism and within `window` of its
/// plane at both, on opposite sides. (Sliding around a free edge leaves the prism first and is
/// not a penetration.)
fn penetrating_pairs(
    start: &[Vec3],
    now: &[Vec3],
    triangles: &[[usize; 3]],
    in_a: &dyn Fn(usize) -> bool,
    in_b: &dyn Fn(usize) -> bool,
    window: f32,
) -> usize {
    let mut count = 0;
    for &t in triangles {
        let tri_a = t.iter().all(|&i| in_a(i));
        let tri_b = t.iter().all(|&i| in_b(i));
        if !tri_a && !tri_b {
            continue;
        }
        for v in 0..now.len() {
            let other = if tri_a { in_b(v) } else { in_a(v) };
            if !other {
                continue;
            }
            let (d0, inside0) = plane_side(start, t, start[v]);
            let (d1, inside) = plane_side(now, t, now[v]);
            let near = d0.abs() < window && d1.abs() < window;
            if inside && inside0 && near && d0.signum() != d1.signum() {
                count += 1;
            }
        }
    }
    count
}

fn positions(s: &ClothSolver) -> Vec<Vec3> {
    s.particles().iter().map(|p| p.position).collect()
}

fn fold_penetrations(s: &ClothSolver, start: &[Vec3]) -> usize {
    // Rows next to the fold line are within the exclusion rings of each other; leave them out.
    let bottom = |i: usize| row_of(i) + 2 < FOLD_HALF;
    let top = |i: usize| row_of(i) > FOLD_HALF + 1;
    penetrating_pairs(
        start,
        &positions(s),
        s.triangles(),
        &bottom,
        &top,
        4.0 * FOLD_SPACING,
    )
}

/// Spec 55 test 9 (the `VertexTriangle` part): a folded sheet thrown onto itself keeps every
/// particle on its side of the other half's triangles; without self-collision (control) the
/// halves pass through each other. Two runs give the same digest.
#[test]
fn folded_sheet_with_vertex_triangle_has_no_penetrating_pairs() {
    let r = 0.01;
    let start = positions(&folded(SelfCollision::Off, 2.0));

    let mut off = folded(SelfCollision::Off, 2.0);
    let mut vt = folded(SelfCollision::VertexTriangle { radius: r }, 2.0);
    let mut again = folded(SelfCollision::VertexTriangle { radius: r }, 2.0);
    let mut most_contacts = 0;
    let mut control = 0;
    let (mut off_before, mut vt_before) = (start.clone(), start.clone());
    for step in 0..90 {
        off.step(DT).expect("step");
        control += fold_penetrations(&off, &off_before);
        off_before = positions(&off);
        vt.step(DT).expect("step");
        again.step(DT).expect("step");
        let d = vt.diagnostics();
        most_contacts = most_contacts.max(d.self_contacts);
        assert_eq!(
            vt.self_crossings(),
            0,
            "step {step}: a candidate pair ended crossed"
        );
        assert_eq!(
            fold_penetrations(&vt, &vt_before),
            0,
            "step {step}: penetrating pairs"
        );
        vt_before = positions(&vt);
        if step % 30 == 29 {
            assert_eq!(
                vt.state_digest(),
                again.state_digest(),
                "step {step}: deterministic"
            );
        }
    }
    eprintln!("fold: control penetrations {control}, most vertex-triangle contacts {most_contacts}");
    assert!(control > 0, "the control passes through itself");
    assert!(most_contacts > 0, "the halves touched");
    assert_eq!(vt.diagnostics().dropped_self_pairs, 0);

    // The top half rests at least about the radius above the bottom half.
    let now = positions(&vt);
    let mut closest = f32::INFINITY;
    for &t in vt.triangles() {
        if !t.iter().all(|&i| row_of(i) + 2 < FOLD_HALF) {
            continue;
        }
        for v in (0..now.len()).filter(|&v| row_of(v) > FOLD_HALF + 1) {
            let (d, inside) = plane_side(&now, t, now[v]);
            if inside && d.abs() < 4.0 * FOLD_SPACING {
                closest = closest.min(d.abs());
            }
        }
    }
    eprintln!("fold: closest top particle to a bottom triangle {closest}");
    assert!(closest >= 0.5 * r, "kept apart: {closest}");
}

/// `VertexTriangle` is its own algorithm, and distance LOD 1 steps it down to `Spheres`.
#[test]
fn vertex_triangle_differs_from_spheres_and_falls_back_with_lod() {
    let r = 0.01;
    let mut spheres = folded(SelfCollision::Spheres { radius: r }, 2.0);
    let mut vt = folded(SelfCollision::VertexTriangle { radius: r }, 2.0);
    for _ in 0..10 {
        spheres.step(DT).expect("step");
        vt.step(DT).expect("step");
    }
    assert_ne!(spheres.state_digest(), vt.state_digest(), "a different algorithm");

    // Distance LOD 1 runs `VertexTriangle` as `Spheres` and LOD 2 switches it off (spec 55
    // § 2.9): at LOD 2 it moves exactly like `Off`.
    let mut lod_vt = folded(SelfCollision::VertexTriangle { radius: r }, 2.0);
    let mut lod_spheres = folded(SelfCollision::Off, 2.0);
    lod_vt.set_lod_level(2);
    lod_spheres.set_lod_level(2);
    for _ in 0..10 {
        lod_vt.step(DT).expect("step");
        lod_spheres.step(DT).expect("step");
    }
    let bits = |s: &ClothSolver| {
        s.particles()
            .iter()
            .flat_map(|p| p.position.to_array().into_iter().chain(p.velocity.to_array()))
            .map(f32::to_bits)
            .collect::<Vec<_>>()
    };
    assert_eq!(bits(&lod_vt), bits(&lod_spheres), "LOD 2 runs no self-collision");
    assert_eq!(lod_vt.self_crossings(), 0);
}

// ---------------------------------------------------------------------------
// Garment layers
// ---------------------------------------------------------------------------

const DRAPE_N: usize = 8;

/// An inner sheet (particles `0..n*n`) dropped over a ball and, with `outer`, an outer sheet
/// (layer 1 when `layered`) dropped onto it.
fn draped(outer: bool, layered: bool, mode: SelfCollision) -> ClothSolver {
    let mut s = solver(ClothConfig {
        iterations: 2,
        ..ClothConfig::default()
    });
    let half = (DRAPE_N - 1) as f32 * 0.05 / 2.0;
    s.add_grid(&grid(
        DRAPE_N,
        DRAPE_N,
        0.05,
        Vec3::new(-half, 0.33, -half),
        Vec3::Z,
    ))
    .expect("inner");
    if outer {
        s.add_grid(&grid(
            DRAPE_N,
            DRAPE_N,
            0.05,
            Vec3::new(-half, 0.40, -half),
            Vec3::Z,
        ))
        .expect("outer");
    }
    s.add_collider(ClothCollider {
        shape: ClothShape::Sphere {
            center: Vec3::ZERO,
            radius: 0.3,
        },
        friction: 0.6,
    })
    .expect("ball");
    with_self_collision(&mut s, mode);
    if outer && layered {
        s.set_layer_range(DRAPE_N * DRAPE_N, DRAPE_N * DRAPE_N, 1)
            .expect("layer");
    }
    s
}

fn inner_bits(s: &ClothSolver) -> Vec<u32> {
    s.particles()[..DRAPE_N * DRAPE_N]
        .iter()
        .flat_map(|p| p.position.to_array().into_iter().chain(p.velocity.to_array()))
        .map(f32::to_bits)
        .collect()
}

/// Spec 55 § 2.6 garment layers: the outer layer stays outside the inner one, and the inner
/// layer moves bit for bit as if the outer layer were not there (inner pushes outer, never the
/// reverse). Without layers the outer sheet does push the inner one (control).
#[test]
fn two_layers_keep_their_order_and_only_the_inner_pushes() {
    let r = 0.01;
    for mode in [
        SelfCollision::VertexTriangle { radius: r },
        SelfCollision::Spheres { radius: r },
    ] {
        let mut alone = draped(false, false, mode);
        let mut layered = draped(true, true, mode);
        let mut flat = draped(true, false, mode);
        assert_eq!(layered.layers().len(), 2 * DRAPE_N * DRAPE_N);
        assert_eq!(layered.max_layer(), 1);
        assert!(flat.layers().is_empty(), "layer 0 everywhere stores nothing");
        let inner = |i: usize| i < DRAPE_N * DRAPE_N;
        let outer = |i: usize| i >= DRAPE_N * DRAPE_N;
        let mut before = positions(&layered);
        for step in 0..180 {
            alone.step(DT).expect("step");
            layered.step(DT).expect("step");
            flat.step(DT).expect("step");
            assert_eq!(
                inner_bits(&layered),
                inner_bits(&alone),
                "{mode:?} step {step}: inner moved"
            );
            let now = positions(&layered);
            let crossed = penetrating_pairs(&before, &now, layered.triangles(), &inner, &outer, 0.2);
            assert_eq!(crossed, 0, "{mode:?} step {step}: the layers crossed");
            before = now;
        }
        assert_ne!(
            inner_bits(&flat),
            inner_bits(&alone),
            "{mode:?}: without layers both move"
        );
        // On average the outer sheet sits farther from the ball than the inner one.
        let mean = |ps: &[regia_physics::cloth::ClothParticle]| {
            ps.iter().map(|p| p.position.length()).sum::<f32>() / ps.len() as f32
        };
        let (inner_mean, outer_mean) = (
            mean(&layered.particles()[..DRAPE_N * DRAPE_N]),
            mean(&layered.particles()[DRAPE_N * DRAPE_N..]),
        );
        eprintln!("{mode:?}: mean distance from the ball center inner {inner_mean}, outer {outer_mean}");
        assert!(
            outer_mean > inner_mean + r,
            "{mode:?}: the outer sheet rests on the inner one"
        );
    }
}

#[test]
fn layer_input_is_checked_and_digested_only_when_set() {
    let mut s = draped(true, false, SelfCollision::VertexTriangle { radius: 0.01 });
    let plain = s.state_digest();
    assert_eq!(
        s.set_layers(&[1, 2, 3]).map_err(|e| e.code()),
        Err("cloth_pose_size_mismatch")
    );
    assert_eq!(
        s.set_layer_range(DRAPE_N * DRAPE_N, 2 * DRAPE_N * DRAPE_N, 1)
            .map_err(|e| e.code()),
        Err("cloth_particle_out_of_range")
    );
    assert_eq!(
        s.set_layer_gap(Some(-1.0)).map_err(|e| e.code()),
        Err("cloth_negative")
    );
    assert_eq!(s.state_digest(), plain, "rejected input changes nothing");
    s.set_layers(&[0; 2 * DRAPE_N * DRAPE_N]).expect("all zero");
    assert!(s.layers().is_empty());
    assert_eq!(s.state_digest(), plain, "layer 0 everywhere is no layers");
    s.set_layer_range(DRAPE_N * DRAPE_N, DRAPE_N * DRAPE_N, 1)
        .expect("outer");
    assert_ne!(s.state_digest(), plain, "layers are part of the digest");
    s.set_layer_gap(Some(0.03)).expect("gap");
    assert_eq!(s.layer_gap(), Some(0.03));
    s.set_layers(&[]).expect("clear");
    s.set_layer_gap(None).expect("default gap");
    assert_eq!(s.state_digest(), plain, "cleared");
}

// ---------------------------------------------------------------------------
// Reaction torque
// ---------------------------------------------------------------------------

/// Spec 55 § 2.4 two-way reaction with torque: a cloth resting off-center on a box pushes it
/// down with its weight and turns it about the box origin by `r × J`; about the cloth's own
/// center the moment nearly vanishes.
#[test]
fn reaction_torque_is_the_moment_of_the_contact_impulses() {
    let mut s = solver(ClothConfig::default());
    s.add_grid(&grid(8, 8, 0.05, Vec3::new(0.6, 0.6, -0.9), Vec3::Z))
        .expect("grid");
    s.set_snapshot(
        WorldSnapshot::new(vec![SnapshotShape {
            key: (11, 0, 0),
            kind: SnapshotKind::Box {
                half_extents: Vec3::new(2.0, 0.5, 2.0),
            },
            prev: Isometry::from_translation(Vec3::ZERO),
            cur: Isometry::from_translation(Vec3::ZERO),
            friction: 0.4,
        }])
        .expect("snapshot"),
    );
    s.set_reaction(Some(5.0)).expect("reaction");
    let mut out = Vec::new();
    for _ in 0..600 {
        s.step(DT).expect("step");
        out.clear();
        s.take_reaction_wrench(&mut out);
    }
    assert_eq!(out.len(), 1);
    let r = out[0];
    assert_eq!(r.key, (11, 0, 0));
    assert_eq!(r.about, Vec3::ZERO);
    let weight = s.particles().iter().map(|p| 9.81 * DT / p.inv_mass).sum::<f32>();
    let center = s.particles().iter().map(|p| p.position).sum::<Vec3>() / s.particles().len() as f32;
    let expected = center.cross(Vec3::new(0.0, -weight, 0.0));
    eprintln!(
        "reaction: impulse {}, angular {}, expected about {expected}",
        r.impulse, r.angular_impulse
    );
    assert!(
        (-r.impulse.y / weight - 1.0).abs() < 0.05,
        "weight: {}",
        r.impulse
    );
    assert!(
        (r.angular_impulse - expected).length() < 0.05 * expected.length(),
        "moment {}",
        r.angular_impulse
    );
    assert!(
        r.angular_impulse.z < 0.0 && r.angular_impulse.x < 0.0,
        "tips toward +x and -z"
    );
    let about_cloth = r.angular_about(center);
    assert!(
        about_cloth.length() < 0.05 * expected.length(),
        "about the cloth: {about_cloth}"
    );

    // Taken once; `take_reaction` clears the angular part too.
    out.clear();
    s.take_reaction_wrench(&mut out);
    assert!(out.is_empty());
    s.step(DT).expect("step");
    let mut linear = Vec::new();
    s.take_reaction(&mut linear);
    assert_eq!(linear.len(), 1);
    s.take_reaction_wrench(&mut out);
    assert!(out.is_empty(), "take_reaction drained both parts");
}

// ---------------------------------------------------------------------------
// Swept colliders
// ---------------------------------------------------------------------------

/// A vertical sheet hanging from its top row in the z = 0 plane; particle `(4, 4)` sits at
/// `(0, 0.8, 0)`.
fn curtain() -> (ClothSolver, usize) {
    let mut s = solver(ClothConfig {
        substeps: 1,
        iterations: 1,
        ..ClothConfig::default()
    });
    let h = s
        .add_grid(&grid(9, 9, 0.05, Vec3::new(-0.2, 1.0, 0.0), Vec3::NEG_Y))
        .expect("grid");
    for c in 0..9 {
        s.pin(h.index(c, 0).expect("top")).expect("pin");
    }
    (s, h.index(4, 4).expect("middle"))
}

const BALL_RADIUS: f32 = 0.03;

fn outside_ball(s: &ClothSolver, center: Vec3) -> f32 {
    s.particles()
        .iter()
        .map(|p| p.position.distance(center) - BALL_RADIUS)
        .fold(f32::INFINITY, f32::min)
}

/// Spec 55 § 2.4: a collider that moves more than the thickness in one substep is tested swept.
/// A small ball crossing the curtain in one step (0.24 m, never touching it at a substep end)
/// carries the cloth in front of it instead of passing through.
#[test]
fn a_fast_collider_carries_the_cloth_instead_of_tunneling() {
    let (from, to) = (Vec3::new(0.0, 0.8, -0.12), Vec3::new(0.0, 0.8, 0.12));
    let ball = |center| ClothCollider {
        shape: ClothShape::Sphere {
            center,
            radius: BALL_RADIUS,
        },
        friction: 0.0,
    };
    let (mut s, middle) = curtain();
    let index = s.add_collider(ball(to)).expect("ball");
    s.set_collider_previous(
        index,
        ClothShape::Sphere {
            center: from,
            radius: BALL_RADIUS,
        },
    )
    .expect("previous");
    s.step(DT).expect("step");
    let z = s.particles()[middle].position.z;
    eprintln!("swept collider: middle particle z = {z}");
    assert!(z > to.z + BALL_RADIUS, "carried in front of the ball: z = {z}");
    assert!(outside_ball(&s, to) > -1e-4, "nothing inside the ball");
    for _ in 0..30 {
        s.step(DT).expect("step");
        assert!(
            outside_ball(&s, to) > -1e-4,
            "the resting ball stays outside the cloth"
        );
    }

    // The same with a world-snapshot sphere.
    let (mut s, middle) = curtain();
    s.set_snapshot(
        WorldSnapshot::new(vec![SnapshotShape {
            key: (1, 0, 0),
            kind: SnapshotKind::Sphere { radius: BALL_RADIUS },
            prev: Isometry::from_translation(from),
            cur: Isometry::from_translation(to),
            friction: 0.0,
        }])
        .expect("snapshot"),
    );
    s.step(DT).expect("step");
    let z = s.particles()[middle].position.z;
    eprintln!("swept snapshot: middle particle z = {z}");
    assert!(
        z > to.z + BALL_RADIUS,
        "carried in front of the snapshot ball: z = {z}"
    );
    assert!(outside_ball(&s, to) > -1e-4, "nothing inside the snapshot ball");
}

/// A ball moving away from the cloth faster than the thickness does not drag it along.
#[test]
fn a_fast_collider_moving_away_lets_go() {
    let (mut s, middle) = curtain();
    let near = Vec3::new(0.0, 0.8, 0.035);
    let index = s
        .add_collider(ClothCollider {
            shape: ClothShape::Sphere {
                center: near + Vec3::new(0.0, 0.0, 0.3),
                radius: BALL_RADIUS,
            },
            friction: 0.0,
        })
        .expect("ball");
    s.set_collider_previous(
        index,
        ClothShape::Sphere {
            center: near,
            radius: BALL_RADIUS,
        },
    )
    .expect("previous");
    s.step(DT).expect("step");
    let z = s.particles()[middle].position.z;
    assert!(z.abs() < 0.01, "left in place: z = {z}");
}

// ---------------------------------------------------------------------------
// Parallel stepping
// ---------------------------------------------------------------------------

fn mixed_set(parallel: bool) -> regia_physics::cloth::ClothSet {
    let mut set = regia_physics::cloth::ClothSet::new();
    set.insert(
        "stacked_spheres",
        stacked(SelfCollision::Spheres { radius: 0.03 }, 0),
    );
    set.insert(
        "stacked_vt",
        stacked(SelfCollision::VertexTriangle { radius: 0.02 }, 0),
    );
    set.insert(
        "fold",
        folded(SelfCollision::VertexTriangle { radius: 0.01 }, 2.0),
    );
    set.insert(
        "layers",
        draped(true, true, SelfCollision::VertexTriangle { radius: 0.01 }),
    );
    set.insert("box", on_box(0));
    set.insert("curtain", curtain().0);
    set.set_parallel(parallel);
    set
}

fn set_bytes(set: &regia_physics::cloth::ClothSet) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, s) in set.iter() {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&s.state_digest().to_le_bytes());
        for p in s.particles() {
            for v in p.position.to_array().into_iter().chain(p.velocity.to_array()) {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    out
}

/// Spec 55 § 7: stepping the solvers of a set in parallel (one task per solver, results reduced
/// in insertion order) gives the same bytes as stepping them one after another.
#[test]
fn parallel_stepping_gives_the_same_bytes_as_serial() {
    let mut serial = mixed_set(false);
    let mut parallel = mixed_set(true);
    assert!(parallel.parallel() && !serial.parallel());
    for step in 0..120 {
        // A late frame now and then runs two fixed steps.
        let dt = if step % 7 == 3 { 2.0 * DT } else { DT };
        let a = serial.step_all(dt).expect("serial");
        let b = parallel.step_all(dt).expect("parallel");
        assert_eq!(a, b, "step {step}: fixed steps run");
        if step % 20 == 19 {
            assert_eq!(set_bytes(&serial), set_bytes(&parallel), "step {step}");
        }
    }
    assert_eq!(set_bytes(&serial), set_bytes(&parallel));
    assert_eq!(
        parallel.step_all(f32::NAN).map_err(|e| e.code()),
        Err("cloth_non_finite"),
        "bad input is refused before any solver runs"
    );
    assert_eq!(set_bytes(&serial), set_bytes(&parallel), "nothing ran");
}

// ---------------------------------------------------------------------------
// Cooked round trip
// ---------------------------------------------------------------------------

/// A two-layer 4 x 8 sim mesh: rows 0-3 are layer 0, rows 4-7 (a second sheet above) layer 1.
fn layered_asset() -> regia_physics::cloth::ClothAsset {
    use regia_physics::cloth::asset::{ClothSource, SimMeshInput};
    let mut mesh = SimMeshInput::default();
    for sheet in 0..2u32 {
        for r in 0..4u32 {
            for c in 0..4u32 {
                mesh.positions
                    .push(Vec3::new(c as f32 * 0.1, 0.05 * sheet as f32, r as f32 * 0.1));
                mesh.uvs.push([c as f32 / 3.0, r as f32 / 3.0]);
                mesh.joints.push([0, 0, 0, 0]);
                mesh.weights.push([1.0, 0.0, 0.0, 0.0]);
            }
        }
        for r in 0..3u32 {
            for c in 0..3u32 {
                let i = sheet * 16 + r * 4 + c;
                mesh.triangles.push([i, i + 4, i + 1]);
                mesh.triangles.push([i + 1, i + 4, i + 5]);
            }
        }
    }
    let entries: Vec<String> = (16..32).map(|i| format!("({i}, 1.0)")).collect();
    let ron = format!(
        r#"(format_version: 1, name: "coat", sim_mesh: (source: "coat.glb", node: "Coat"),
            params: (self_collision: VertexTriangle(radius: 0.01)),
            maps: {{ "layer": Sparse(default: 0.0, entries: [{}]) }})"#,
        entries.join(", ")
    );
    let source = ClothSource::from_ron_str(&ron).expect("parses");
    regia_physics::cloth::ClothAsset::cook(&source, &mesh, None, &[]).expect("cooks")
}

/// The cooked `.cloth.bin` already carries the `layer` map and the self-collision mode; a
/// round trip builds a solver with the same layers, mode and stepping (no format change).
#[test]
fn a_cooked_round_trip_keeps_layers_and_vertex_triangle() {
    use regia_core::math::Affine3A;
    let asset = layered_asset();
    let bytes = asset.to_bytes().expect("cook");
    let back = regia_physics::cloth::ClothAsset::from_bytes(&bytes).expect("load");
    assert_eq!(back.to_bytes().expect("again"), bytes, "byte-identical");
    let mut a = asset.build_solver(0, DT, Affine3A::IDENTITY).expect("solver");
    let mut b = back.build_solver(0, DT, Affine3A::IDENTITY).expect("solver");
    let expected: Vec<u8> = (0..32).map(|i| u8::from(i >= 16)).collect();
    assert_eq!(a.layers(), expected.as_slice());
    assert_eq!(b.layers(), expected.as_slice());
    assert!(matches!(
        a.tuning().self_collision,
        SelfCollision::VertexTriangle { .. }
    ));
    for _ in 0..30 {
        a.step(DT).expect("step");
        b.step(DT).expect("step");
    }
    assert_eq!(a.state_digest(), b.state_digest());
}

/// Two free sheets, the top one thrown down onto the bottom one with no gravity: vertex-triangle
/// contact is inelastic (both end at half the speed) and conserves momentum; nothing crosses.
#[test]
fn sheets_thrown_together_stick_and_conserve_momentum() {
    let mut s = solver(ClothConfig {
        gravity: Vec3::ZERO,
        damping: 0.0,
        iterations: 4,
        ..ClothConfig::default()
    });
    let n = 8;
    s.add_grid(&grid(n, n, 0.05, Vec3::new(0.0, 0.0, 0.0), Vec3::Z))
        .expect("bottom");
    s.add_grid(&grid(n, n, 0.05, Vec3::new(0.0, 0.04, 0.0), Vec3::Z))
        .expect("top");
    for i in n * n..2 * n * n {
        s.set_velocity(i, Vec3::new(0.0, -2.0, 0.0)).expect("throw");
    }
    with_self_collision(&mut s, SelfCollision::VertexTriangle { radius: 0.01 });
    let lower = |i: usize| i < n * n;
    let upper = |i: usize| i >= n * n;
    let mut before = positions(&s);
    for step in 0..30 {
        s.step(DT).expect("step");
        let now = positions(&s);
        assert_eq!(
            penetrating_pairs(&before, &now, s.triangles(), &lower, &upper, 0.2),
            0,
            "step {step}"
        );
        before = now;
    }
    let mean_v = |range: std::ops::Range<usize>| {
        s.particles()[range.clone()]
            .iter()
            .map(|p| p.velocity.y)
            .sum::<f32>()
            / range.len() as f32
    };
    let (bottom, top) = (mean_v(0..n * n), mean_v(n * n..2 * n * n));
    eprintln!("thrown sheets: bottom v {bottom}, top v {top}");
    assert!((bottom + top + 2.0).abs() < 0.02, "momentum: {bottom} + {top}");
    assert!(
        (bottom + 1.0).abs() < 0.1 && (top + 1.0).abs() < 0.1,
        "inelastic: {bottom}, {top}"
    );
}

/// Hand-run throughput probe for spec 55 § 10 (never part of the gate, prints only): a 32 x 32
/// hanging sheet (0.05 m spacing, damping 0.5, 8 substeps, 1 iteration) with `Off`, `Spheres`
/// and `VertexTriangle`. (A 0.03 m sheet at 4 or 8 substeps diverges on the base solver, so it
/// would time garbage; see the lane report. The probe refuses a stretched sheet.)
/// `cargo test -p regia_physics --release --test cloth_extras -- --ignored --nocapture`.
#[test]
#[ignore = "hand-run timing probe"]
fn self_collision_throughput_probe() {
    let run = |mode: SelfCollision| {
        let mut s = solver(ClothConfig {
            substeps: 8,
            iterations: 1,
            damping: 0.5,
            ..ClothConfig::default()
        });
        let h = s
            .add_grid(&grid(32, 32, 0.05, Vec3::new(0.0, 2.0, 0.0), Vec3::Z))
            .expect("grid");
        for c in 0..32 {
            s.pin(h.index(c, 0).expect("top")).expect("pin");
        }
        with_self_collision(&mut s, mode);
        for _ in 0..60 {
            s.step(DT).expect("warm");
        }
        let steps = 300;
        let start = std::time::Instant::now();
        for _ in 0..steps {
            s.step(DT).expect("step");
        }
        let ms = start.elapsed().as_secs_f64() * 1e3 / f64::from(steps);
        let rate = s.particles().len() as f64 / ms;
        let d = s.diagnostics();
        let bad = s.particles().iter().filter(|p| !p.position.is_finite()).count();
        let (lo, hi) = s
            .particles()
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), p| {
                (lo.min(p.position.y), hi.max(p.position.y))
            });
        eprintln!("{}: non-finite particles {bad}, y range {lo}..{hi}", mode.name());
        assert_eq!(bad, 0, "the probe must time a finite sheet");
        assert!(
            s.stats().max_stretch < 1.5,
            "the probe must time a sheet that did not blow up"
        );
        eprintln!(
            "{}: {ms:.3} ms per fixed step, {rate:.0} particles/ms (contacts {}, dropped pairs {}, max stretch {:.3}, crossings {})",
            mode.name(),
            d.self_contacts,
            d.dropped_self_pairs,
            s.stats().max_stretch,
            s.self_crossings()
        );
        rate
    };
    let off = run(SelfCollision::Off);
    let spheres = run(SelfCollision::Spheres { radius: 0.01 });
    let vt = run(SelfCollision::VertexTriangle { radius: 0.01 });
    eprintln!(
        "ratios: spheres/off {:.2}, vertex_triangle/spheres {:.2}",
        spheres / off,
        vt / spheres
    );
}
