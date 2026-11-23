// SPDX-License-Identifier: MPL-2.0
//! Test-gap audit (lane testgaps-fluid-2d-xr): parameter validation, emitters, sinks, particle
//! caps, non-finite state, binning, stats, bounded velocity and determinism of the CPU solver.
//! Fixed steps and counters only: no wall clock, no sleeps.

use glam::Vec3;
use regia_fluid::presets;
use regia_fluid::sim::{
    Emitter, EmitterKind, FluidError, FluidParams, FluidSim, MAX_SHAPES, PARTICLES_PER_CELL, Shape,
    max_cells, max_particles,
};
use regia_fluid::solid::{Collider, MovingSolid, SolidShape};

fn small(dx: f32, n: f32) -> FluidParams {
    FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::splat(n),
        cell_size: dx,
        reseed: false,
        ..FluidParams::default()
    }
}

fn fill_box(center: Vec3, half: Vec3, velocity: Vec3, kind: EmitterKind) -> Emitter {
    Emitter {
        shape: Shape::Box {
            center,
            half_extents: half,
        },
        velocity,
        kind,
        done: false,
    }
}

fn code(p: FluidParams) -> &'static str {
    match FluidSim::new(p) {
        Ok(_) => "ok",
        Err(e) => e.code(),
    }
}

/// Every range check of `validate` rejects just outside its limit and accepts the limit itself.
#[test]
fn validate_boundaries() {
    let base = || small(0.25, 1.0);
    assert_eq!(code(base()), "ok");
    let mut cases: Vec<(&str, FluidParams, &str)> = Vec::new();
    let mut add = |name: &'static str, f: &dyn Fn(&mut FluidParams), want: &'static str| {
        let mut p = base();
        f(&mut p);
        cases.push((name, p, want));
    };
    add("dt 0.1 ok", &|p| p.dt = 0.1, "ok");
    add("dt 0.1001", &|p| p.dt = 0.1001, "fluid_bad_param");
    add("dt 0", &|p| p.dt = 0.0, "fluid_bad_param");
    add("dt nan", &|p| p.dt = f32::NAN, "fluid_bad_param");
    add("dt negative", &|p| p.dt = -0.01, "fluid_bad_param");
    add(
        "gravity nan",
        &|p| p.gravity = Vec3::new(0.0, f32::NAN, 0.0),
        "fluid_bad_param",
    );
    add(
        "gravity 1000 ok",
        &|p| p.gravity = Vec3::new(0.0, -1000.0, 0.0),
        "ok",
    );
    add(
        "gravity 1000.5",
        &|p| p.gravity = Vec3::new(0.0, -1000.5, 0.0),
        "fluid_bad_param",
    );
    add(
        "external accel inf",
        &|p| p.external_acceleration = Vec3::INFINITY,
        "fluid_bad_param",
    );
    add(
        "external accel 1001",
        &|p| p.external_acceleration = Vec3::new(1001.0, 0.0, 0.0),
        "fluid_bad_param",
    );
    add("flip 0 ok", &|p| p.flip_ratio = 0.0, "ok");
    add("flip 1 ok", &|p| p.flip_ratio = 1.0, "ok");
    add("flip -0.01", &|p| p.flip_ratio = -0.01, "fluid_bad_param");
    add("flip nan", &|p| p.flip_ratio = f32::NAN, "fluid_bad_param");
    add("density 0", &|p| p.density = 0.0, "fluid_bad_param");
    add("density inf", &|p| p.density = f32::INFINITY, "fluid_bad_param");
    add(
        "pcg iterations 0",
        &|p| p.pcg_max_iterations = 0,
        "fluid_bad_param",
    );
    add(
        "pcg iterations 10000 ok",
        &|p| p.pcg_max_iterations = 10_000,
        "ok",
    );
    add(
        "pcg iterations 10001",
        &|p| p.pcg_max_iterations = 10_001,
        "fluid_bad_param",
    );
    add("pcg tolerance 0", &|p| p.pcg_tolerance = 0.0, "fluid_bad_param");
    add("pcg tolerance 1", &|p| p.pcg_tolerance = 1.0, "fluid_bad_param");
    add(
        "pcg tolerance nan",
        &|p| p.pcg_tolerance = f64::NAN,
        "fluid_bad_param",
    );
    add("substeps 0", &|p| p.max_substeps = 0, "fluid_bad_param");
    add("substeps 16 ok", &|p| p.max_substeps = 16, "ok");
    add("substeps 17", &|p| p.max_substeps = 17, "fluid_bad_param");
    add("cfl 0", &|p| p.cfl = 0.0, "fluid_bad_param");
    add("cfl 10 ok", &|p| p.cfl = 10.0, "ok");
    add("cfl 10.1", &|p| p.cfl = 10.1, "fluid_bad_param");
    add("separation 1 ok", &|p| p.separation = 1.0, "ok");
    add("separation 1.01", &|p| p.separation = 1.01, "fluid_bad_param");
    add("cap 0", &|p| p.particle_cap = 0, "fluid_bad_param");
    add("cap max ok", &|p| p.particle_cap = max_particles(), "ok");
    add(
        "cap max+1",
        &|p| p.particle_cap = max_particles() + 1,
        "fluid_bad_param",
    );
    add("bounds flat", &|p| p.bounds_max.y = 0.0, "fluid_bad_bounds");
    add("bounds inverted", &|p| p.bounds_min.x = 2.0, "fluid_bad_bounds");
    add(
        "bounds inf",
        &|p| p.bounds_min.z = f32::NEG_INFINITY,
        "fluid_bad_bounds",
    );
    add("cell zero", &|p| p.cell_size = 0.0, "fluid_bad_cell_size");
    add("cell negative", &|p| p.cell_size = -0.1, "fluid_bad_cell_size");
    add("cell nan", &|p| p.cell_size = f32::NAN, "fluid_bad_cell_size");
    for (name, p, want) in cases {
        assert_eq!(code(p), want, "{name}");
    }
}

/// The cell cap is exact: the cap's own cube builds, one more cell along an axis does not, and
/// the error reports the cell count it refused.
#[test]
fn cell_cap_is_exact() {
    let cube = (max_cells() as f64).cbrt().round() as f32;
    assert_eq!(
        cube * cube * cube,
        max_cells() as f32,
        "the cap is a cube on this platform"
    );
    let ok = FluidParams {
        bounds_max: Vec3::splat(cube),
        cell_size: 1.0,
        ..FluidParams::default()
    };
    let dims = ok.grid().expect("the cap's cube");
    assert_eq!(dims.cells(), max_cells());
    let over = FluidParams {
        bounds_max: Vec3::new(cube + 1.0, cube, cube),
        cell_size: 1.0,
        ..FluidParams::default()
    };
    match over.grid() {
        Err(FluidError::TooManyCells { cells, max }) => {
            assert_eq!(max, max_cells());
            assert_eq!(cells, (cube as usize + 1) * (cube as usize) * (cube as usize));
        }
        other => panic!("{other:?}"),
    }
    // A domain thinner than a cell still gets one cell.
    let thin = FluidParams {
        bounds_max: Vec3::new(0.01, 1.0, 1.0),
        cell_size: 0.5,
        ..FluidParams::default()
    };
    let d = thin.grid().expect("thin");
    assert_eq!((d.nx, d.ny, d.nz), (1, 2, 2));
    assert_eq!(d.origin, Vec3::ZERO);
}

/// The 64-shape cap holds for emitters, sinks and colliders, and bad shapes are refused.
#[test]
fn shape_caps_and_bad_shapes() {
    let mut sim = FluidSim::new(small(0.25, 1.0)).expect("sim");
    let e = fill_box(Vec3::splat(0.5), Vec3::splat(0.1), Vec3::ZERO, EmitterKind::Fill);
    for i in 0..MAX_SHAPES {
        assert_eq!(sim.add_emitter(e), Ok(i));
    }
    assert_eq!(sim.add_emitter(e), Err(FluidError::TooManyShapes));
    let s = Shape::Sphere {
        center: Vec3::splat(0.5),
        radius: 0.1,
    };
    for i in 0..MAX_SHAPES {
        assert_eq!(sim.add_sink(s), Ok(i));
    }
    assert_eq!(
        sim.add_sink(s).map_err(|e| e.code()),
        Err("fluid_too_many_shapes")
    );
    assert_eq!(sim.emitters().len(), MAX_SHAPES);
    assert_eq!(sim.sinks().len(), MAX_SHAPES);
    let ball = Collider::Sphere {
        center: Vec3::splat(0.5),
        radius: 0.1,
    };
    assert!(FluidSim::with_colliders(small(0.25, 1.0), vec![ball.clone(); MAX_SHAPES]).is_ok());
    assert_eq!(
        FluidSim::with_colliders(small(0.25, 1.0), vec![ball; MAX_SHAPES + 1]).map(|_| ()),
        Err(FluidError::TooManyShapes)
    );

    let mut sim = FluidSim::new(small(0.25, 1.0)).expect("sim");
    let bad = [
        fill_box(
            Vec3::splat(f32::NAN),
            Vec3::splat(0.1),
            Vec3::ZERO,
            EmitterKind::Fill,
        ),
        fill_box(
            Vec3::splat(0.5),
            Vec3::new(-0.1, 0.1, 0.1),
            Vec3::ZERO,
            EmitterKind::Fill,
        ),
        fill_box(
            Vec3::splat(0.5),
            Vec3::splat(0.1),
            Vec3::new(f32::NAN, 0.0, 0.0),
            EmitterKind::Source,
        ),
        fill_box(
            Vec3::splat(0.5),
            Vec3::splat(0.1),
            Vec3::new(1001.0, 0.0, 0.0),
            EmitterKind::Source,
        ),
    ];
    for b in bad {
        assert_eq!(
            sim.add_emitter(b).map_err(|e| e.code()),
            Err("fluid_bad_param"),
            "{b:?}"
        );
    }
    let bad_shapes = [
        Shape::Sphere {
            center: Vec3::ZERO,
            radius: -1.0,
        },
        Shape::Sphere {
            center: Vec3::ZERO,
            radius: f32::NAN,
        },
        Shape::Box {
            center: Vec3::splat(f32::INFINITY),
            half_extents: Vec3::ONE,
        },
    ];
    for s in bad_shapes {
        assert_eq!(
            sim.add_sink(s).map_err(|e| e.code()),
            Err("fluid_bad_param"),
            "{s:?}"
        );
    }
    assert!(
        sim.emitters().is_empty() && sim.sinks().is_empty(),
        "refused shapes are not stored"
    );
    // A zero-size shape is legal.
    assert!(
        sim.add_sink(Shape::Sphere {
            center: Vec3::splat(0.5),
            radius: 0.0
        })
        .is_ok()
    );
}

/// A fill that exactly covers whole cells seeds eight particles per cell, once, at the emitter's
/// velocity, every one inside both the shape and the domain.
#[test]
fn fill_emits_eight_per_cell_once() {
    let mut sim = FluidSim::new(small(0.25, 1.0)).expect("sim");
    let v = Vec3::new(0.5, 0.0, -0.25);
    sim.add_emitter(fill_box(
        Vec3::new(0.5, 0.25, 0.5),
        Vec3::new(0.5, 0.25, 0.25),
        v,
        EmitterKind::Fill,
    ))
    .expect("emitter");
    sim.step();
    // 4 x 2 x 2 cells.
    assert_eq!(sim.positions().len(), 16 * PARTICLES_PER_CELL);
    assert!(sim.emitters()[0].done);
    // The step moved them under gravity; the emitter velocity was the starting point, so the mean
    // x-velocity is still near 0.5 (no source resets it).
    let mean_x = sim.velocities().iter().map(|v| v.x).sum::<f32>() / sim.velocities().len() as f32;
    assert!((mean_x - 0.5).abs() < 0.15, "mean x velocity {mean_x}");
    let before = sim.positions().len();
    for _ in 0..5 {
        sim.step();
    }
    assert_eq!(
        sim.positions().len(),
        before,
        "a fill emitter runs once; nothing is reseeded or lost"
    );
    let s = sim.stats();
    assert_eq!(s.particles, before);
    assert_eq!(s.steps, 6);
    for p in sim.positions() {
        assert!(p.cmpge(Vec3::ZERO).all() && p.cmple(Vec3::ONE).all(), "{p:?}");
    }
}

/// A fill whose velocity is huge is not clamped away at emission, but the CFL substep count never
/// exceeds `max_substeps`.
#[test]
fn substeps_follow_cfl_and_cap() {
    let mut p = small(0.25, 1.0);
    p.max_substeps = 3;
    let mut sim = FluidSim::new(p).expect("sim");
    sim.add_emitter(fill_box(
        Vec3::splat(0.5),
        Vec3::splat(0.25),
        Vec3::new(50.0, 0.0, 0.0),
        EmitterKind::Fill,
    ))
    .expect("emitter");
    sim.step();
    assert_eq!(
        sim.stats().substeps,
        3,
        "50 m/s needs more than 3 substeps; the cap holds"
    );
    // At rest the substep count is 1.
    let mut calm = FluidSim::new(small(0.25, 1.0)).expect("sim");
    calm.add_emitter(fill_box(
        Vec3::splat(0.5),
        Vec3::splat(0.25),
        Vec3::ZERO,
        EmitterKind::Fill,
    ))
    .expect("emitter");
    calm.step();
    assert_eq!(calm.stats().substeps, 1);
}

/// A source tops cells up to eight and resets the velocity of liquid inside; the cap stops emission
/// at exactly the cap.
#[test]
fn source_tops_up_and_the_particle_cap_is_exact() {
    let mut p = small(0.25, 1.0);
    p.particle_cap = 100;
    let mut sim = FluidSim::new(p).expect("sim");
    sim.add_emitter(fill_box(
        Vec3::splat(0.5),
        Vec3::splat(0.5),
        Vec3::new(1.0, 0.0, 0.0),
        EmitterKind::Source,
    ))
    .expect("emitter");
    for _ in 0..4 {
        sim.step();
        assert!(sim.positions().len() <= 100);
        assert_eq!(sim.positions().len(), sim.velocities().len());
    }
    assert_eq!(
        sim.positions().len(),
        100,
        "the domain holds 512 particles' worth; the cap stops at 100"
    );

    // Uncapped: a one-cell source reaches exactly 8 and stays at 8 while the liquid stays in the cell.
    let mut sim = FluidSim::new(FluidParams {
        gravity: Vec3::ZERO,
        ..small(0.25, 1.0)
    })
    .expect("sim");
    sim.add_emitter(fill_box(
        Vec3::splat(0.375),
        Vec3::splat(0.125),
        Vec3::ZERO,
        EmitterKind::Source,
    ))
    .expect("emitter");
    sim.step();
    assert_eq!(sim.positions().len(), PARTICLES_PER_CELL);
    sim.step();
    assert_eq!(
        sim.positions().len(),
        PARTICLES_PER_CELL,
        "an eight-full cell is not topped up again"
    );
}

/// Sinks remove what is inside every step; `add_particles` clamps, skips non-finite input and
/// stops at the cap.
#[test]
fn sinks_and_add_particles() {
    let mut sim = FluidSim::new(small(0.25, 1.0)).expect("sim");
    sim.add_sink(Shape::Box {
        center: Vec3::splat(0.5),
        half_extents: Vec3::splat(0.6),
    })
    .expect("sink");
    let added = sim.add_particles(&[
        (Vec3::splat(0.5), Vec3::ZERO),
        (Vec3::new(f32::NAN, 0.5, 0.5), Vec3::ZERO),
        (Vec3::splat(0.5), Vec3::new(0.0, f32::INFINITY, 0.0)),
        (Vec3::splat(-9.0), Vec3::ZERO),
        (Vec3::splat(9.0), Vec3::ZERO),
    ]);
    assert_eq!(added, 3, "NaN position and infinite velocity are skipped");
    let eps = sim.dims().dx * 1e-3;
    assert!(
        (sim.positions()[1] - Vec3::splat(eps)).length() < 1e-6,
        "clamped to the minimum corner"
    );
    assert!(
        (sim.positions()[2] - Vec3::splat(1.0 - eps)).length() < 1e-6,
        "clamped to the maximum corner"
    );
    sim.step();
    assert!(sim.positions().is_empty(), "the sink covers the whole domain");
    assert_eq!(sim.velocities().len(), 0);

    let mut sim = FluidSim::new(FluidParams {
        particle_cap: 2,
        ..small(0.25, 1.0)
    })
    .expect("sim");
    let many = vec![(Vec3::splat(0.5), Vec3::ZERO); 10];
    assert_eq!(sim.add_particles(&many), 2);
    assert_eq!(sim.add_particles(&many), 0);
}

/// `set_particles` truncates to the shorter of the two arrays and to the cap.
#[test]
fn set_particles_truncates() {
    let mut sim = FluidSim::new(FluidParams {
        particle_cap: 5,
        ..small(0.25, 1.0)
    })
    .expect("sim");
    sim.set_particles(vec![Vec3::splat(0.5); 4], vec![Vec3::ZERO; 3]);
    assert_eq!((sim.positions().len(), sim.velocities().len()), (3, 3));
    sim.set_particles(vec![Vec3::splat(0.5); 9], vec![Vec3::ZERO; 9]);
    assert_eq!((sim.positions().len(), sim.velocities().len()), (5, 5));
}

/// A non-finite position is binned into cell 0. While that cell is interior, separation used to
/// treat the NaN distance as "close" and write a NaN correction; the clamp then pinned every
/// partner to the cell corner (or dropped it). The bad particle is ignored until it is dropped.
#[test]
fn non_finite_particle_does_not_shove_its_cell() {
    let mut params = small(0.5, 1.0);
    params.gravity = Vec3::ZERO;
    params.separation = 0.5;
    params.reseed = false;
    params.whitewater.enabled = false;
    let mut clean = FluidSim::new(params).expect("sim");
    let mut pos = Vec::new();
    for k in 0..2 {
        for j in 0..2 {
            for i in 0..2 {
                pos.push(Vec3::new(
                    (i as f32 + 0.4) * 0.5,
                    (j as f32 + 0.4) * 0.5,
                    (k as f32 + 0.4) * 0.5,
                ));
            }
        }
    }
    // Clearly inside cell 0, and not within the separation radius of the other cells' particles.
    pos[0] = Vec3::new(0.2, 0.2, 0.2);
    clean.set_particles(pos.clone(), vec![Vec3::ZERO; pos.len()]);
    let mut dirty = clean.clone();
    pos.push(Vec3::new(f32::NAN, 0.2, 0.2));
    dirty.set_particles(pos, vec![Vec3::ZERO; dirty.positions().len() + 1]);
    clean.step();
    dirty.step();
    assert_eq!(dirty.stats().dropped_non_finite, 1);
    assert_eq!(dirty.positions().len(), clean.positions().len());
    for (good, bad) in clean.positions().iter().zip(dirty.positions()) {
        let shift = (*good - *bad).length();
        assert!(
            shift < 1.0e-3,
            "a NaN particle moved its neighbour from {good:?} to {bad:?} (shift {shift})"
        );
    }
}

/// A lone non-finite particle is not a liquid cell. It used to clamp into cell 0 and mark that
/// cell, so the pressure solve treated empty space at the origin as liquid for the step.
#[test]
fn non_finite_particle_does_not_mark_a_cell() {
    let mut params = small(0.25, 1.0);
    params.gravity = Vec3::ZERO;
    params.separation = 0.0;
    params.reseed = false;
    params.whitewater.enabled = false;
    let mut sim = FluidSim::new(params).expect("sim");
    sim.set_particles(vec![Vec3::new(f32::NAN, f32::INFINITY, 0.0)], vec![Vec3::ZERO]);
    sim.step();
    assert_eq!(sim.positions().len(), 0);
    assert_eq!(
        sim.stats().liquid_cells,
        0,
        "a non-finite particle marked {} liquid cells",
        sim.stats().liquid_cells
    );
}

/// A solid that reaches through the domain floor, with its nearest face outside the domain, used
/// to slide a trapped particle along +x (deeper into the solid) when the in-plane normal vanished.
#[test]
fn wall_slide_leaves_a_solid_that_crosses_the_floor() {
    let mut params = small(0.1, 1.0);
    params.gravity = Vec3::ZERO;
    params.separation = 0.0;
    params.reseed = false;
    params.whitewater.enabled = false;
    params.max_substeps = 1;
    // x in [0.2, 1.2], y in [-0.02, 1.0], z in [0, 1]. The particle at x = 0.25 is closer to the
    // face below the floor than to the open side at x = 0.2, so the push hits the floor clamp.
    let solid = Collider::Box {
        center: Vec3::new(0.7, 0.49, 0.5),
        half_extents: Vec3::new(0.5, 0.51, 0.5),
    };
    let start = Vec3::new(0.25, 0.001, 0.5);
    let mut sim = FluidSim::with_colliders(params, vec![solid]).expect("sim");
    assert!(
        sim.solid().distance(start) < 0.0,
        "the probe particle starts inside the solid"
    );
    sim.set_particles(vec![start], vec![Vec3::ZERO]);
    sim.step();
    assert_eq!(sim.positions().len(), 1);
    let p = sim.positions()[0];
    let phi = sim.solid().distance(p);
    assert!(
        p.is_finite() && phi >= -1.0e-3 && p.x < start.x,
        "particle slid to {p:?} (distance {phi}), still inside or deeper in x"
    );
}

/// `finite3` in `fluid_flip.wgsl` must test the exponent bits, not `v == v`: WGSL lets a
/// fast-math shader compiler fold the equality to true (Metal; the `ddgi_common.wgsl` note,
/// ADR-3287), and a NaN velocity would scatter onto every neighbour face again. The bit test
/// itself must be the shift form that survives NVIDIA's Windows compiler (ADR-3150, ADR-5875).
#[test]
fn finite3_tests_the_exponent_bits() {
    let src = include_str!("../src/fluid_flip.wgsl");
    let start = src.find("fn finite3").expect("finite3 in the shader");
    let body = &src[start..];
    let body = &body[..body.find("\n}\n").expect("finite3 body")];
    assert!(
        body.contains("bitcast<vec3<u32>>"),
        "finite3 must test the exponent bits, not `v == v` (fast math folds it away): {body}"
    );
    assert!(
        !body.contains("== v."),
        "finite3 still uses an equality NaN test a fast-math compiler can fold: {body}"
    );
    // The sign bit is shifted out, not masked: the NVIDIA 610.88 Windows Vulkan compiler folded
    // `(bits & 0x7f800000) == 0x7f800000` to false on a `dot` result, so NaN and ∞ passed; the
    // shift form `(bits << 1) >= 0xff000000` held in every case ADR-3150 probed.
    assert!(
        body.contains("<< vec3<u32>(1u)") && body.contains(">= vec3<u32>(0xff000000u)"),
        "finite3 must use the shift form `(bits << 1) >= 0xff000000` (ADR-3150): {body}"
    );
    assert!(
        !body.contains("0x7f800000u"),
        "finite3 still masks the exponent, which NVIDIA's Windows compiler folds (ADR-3150): {body}"
    );
}

/// A box's CFL bound is `|w|` times the software square root of `half_extents.length_squared()`
/// (ADR-4753). `Vec3::length` is the platform square root; widened back to f64 it sits on the
/// other side of this limit, so the platform path takes one substep and the software path two.
#[test]
fn box_substep_count_uses_the_software_radius() {
    // dx and dt are the f32 values whose CFL limit squared (cfl = 1) lies strictly between
    // `(f32::sqrt(3))²` and `(software_sqrt(3))²`. Angular speed is 1 rad/s, half extents are 1.
    let mut params = small(f32::from_bits(0x3c6c_7b91), 0.2);
    params.dt = f32::from_bits(0x3c08_8889);
    params.cfl = 1.0;
    params.max_substeps = 2;
    params.gravity = Vec3::ZERO;
    params.separation = 0.0;
    params.reseed = false;
    params.whitewater.enabled = false;
    let mut sim = FluidSim::new(params).expect("sim");
    let mut solid = MovingSolid::kinematic(
        SolidShape::Box {
            half_extents: Vec3::ONE,
        },
        Vec3::ZERO,
    );
    solid.angular_velocity = Vec3::X;
    sim.set_moving_solids(vec![solid]).expect("solid");
    sim.step();
    assert_eq!(
        sim.stats().substeps,
        2,
        "platform sqrt chose {} substeps",
        sim.stats().substeps
    );
}

/// A particle whose state went non-finite is dropped, counted, and does not poison the rest.
#[test]
fn non_finite_particles_are_dropped_and_counted() {
    for (bad_pos, bad_vel) in [
        (Vec3::new(f32::NAN, 0.5, 0.5), Vec3::ZERO),
        (Vec3::splat(0.5), Vec3::new(f32::NAN, 0.0, 0.0)),
        (Vec3::splat(0.5), Vec3::new(f32::INFINITY, 0.0, 0.0)),
    ] {
        let mut sim = FluidSim::new(small(0.125, 1.0)).expect("sim");
        sim.add_emitter(fill_box(
            Vec3::new(0.5, 0.25, 0.5),
            Vec3::new(0.5, 0.25, 0.5),
            Vec3::ZERO,
            EmitterKind::Fill,
        ))
        .expect("emitter");
        sim.step();
        let (mut pos, mut vel) = (sim.positions().to_vec(), sim.velocities().to_vec());
        let good = pos.len();
        pos.push(bad_pos);
        vel.push(bad_vel);
        sim.set_particles(pos, vel);
        for _ in 0..3 {
            sim.step();
        }
        let s = sim.stats();
        assert_eq!(s.dropped_non_finite, 1, "{bad_pos:?} {bad_vel:?}");
        assert_eq!(sim.positions().len(), good);
        assert!(
            sim.positions()
                .iter()
                .chain(sim.velocities())
                .all(|v| v.is_finite()),
            "a bad particle poisoned its neighbours ({bad_pos:?}, {bad_vel:?})"
        );
        assert!(s.pcg_residual.is_finite());
    }
}

/// Binning is a stable counting sort: indices ascend inside a cell, cells partition the
/// particles, and strays go to the nearest cell.
#[test]
fn bins_are_stable_and_clamp_strays() {
    let mut sim = FluidSim::new(small(0.5, 1.0)).expect("sim");
    sim.set_particles(
        vec![
            Vec3::new(0.75, 0.25, 0.25), // cell (1,0,0)
            Vec3::new(0.25, 0.25, 0.25), // cell (0,0,0)
            Vec3::new(0.80, 0.30, 0.20), // cell (1,0,0)
            Vec3::new(-5.0, -5.0, -5.0), // clamped to (0,0,0)
            Vec3::new(9.0, 9.0, 9.0),    // clamped to (1,1,1)
            Vec3::new(f32::NAN, 0.0, 0.0),
        ],
        vec![Vec3::ZERO; 6],
    );
    let b = sim.bin();
    let d = *sim.dims();
    assert_eq!(b.start.len(), d.cells() + 1);
    assert_eq!(b.items.len(), 6);
    assert_eq!(b.cell(d.cell(0, 0, 0)), &[1, 3, 5]);
    assert_eq!(b.cell(d.cell(1, 0, 0)), &[0, 2]);
    assert_eq!(b.cell(d.cell(1, 1, 1)), &[4]);
    assert_eq!(b.count(d.cell(0, 1, 0)), 0);
    let total: usize = (0..d.cells()).map(|c| b.count(c)).sum();
    assert_eq!(total, 6);
}

/// Volume, column heights and the liquid-cell count on a hand-built configuration.
#[test]
fn volume_and_column_height_arithmetic() {
    let mut sim = FluidSim::new(small(0.5, 1.0)).expect("sim");
    // Four particles in cell (0,0,0): half a cell of volume.
    let four: Vec<_> = (0..4).map(|_| (Vec3::splat(0.25), Vec3::ZERO)).collect();
    sim.add_particles(&four);
    let dx = 0.5f32;
    assert!(
        (sim.liquid_volume() - 0.5 * dx * dx * dx).abs() < 1e-7,
        "{}",
        sim.liquid_volume()
    );
    let h = sim.column_heights();
    assert_eq!(h.len(), 4);
    assert!((h[0] - 0.5 * dx).abs() < 1e-7, "{h:?}");
    assert!(h[1..].iter().all(|v| *v == 0.0));
    // Fill it to eight and add a second cell above it.
    let more: Vec<_> = (0..4).map(|_| (Vec3::splat(0.25), Vec3::ZERO)).collect();
    sim.add_particles(&more);
    let above: Vec<_> = (0..8)
        .map(|_| (Vec3::new(0.25, 0.75, 0.25), Vec3::ZERO))
        .collect();
    sim.add_particles(&above);
    assert!((sim.liquid_volume() - 2.0 * dx * dx * dx).abs() < 1e-7);
    assert!((sim.column_heights()[0] - 2.0 * dx).abs() < 1e-7);
    // More than eight particles still count one cell, not more.
    sim.add_particles(&vec![(Vec3::splat(0.25), Vec3::ZERO); 20]);
    assert!((sim.liquid_volume() - 2.0 * dx * dx * dx).abs() < 1e-7);
    // The digest sees a one-bit velocity change and the step count.
    let d0 = sim.state_digest();
    let mut other = sim.clone();
    assert_eq!(other.state_digest(), d0);
    let mut vel = other.velocities().to_vec();
    vel[0].x = f32::from_bits(vel[0].x.to_bits() ^ 1);
    let pos = other.positions().to_vec();
    other.set_particles(pos, vel);
    assert_ne!(other.state_digest(), d0);
    let mut stepped = sim.clone();
    stepped.step();
    assert_ne!(stepped.state_digest(), d0);
}

/// `is_liquid` is false outside the grid and before any step; `pressure` is zero outside the grid.
#[test]
fn out_of_range_queries_are_safe() {
    let mut sim = FluidSim::new(small(0.5, 1.0)).expect("sim");
    assert!(!sim.is_liquid(0, 0, 0));
    assert!(!sim.is_liquid(2, 0, 0) && !sim.is_liquid(0, 99, 0) && !sim.is_liquid(0, 0, usize::MAX));
    assert_eq!(sim.pressure(2, 0, 0), 0.0);
    assert_eq!(sim.pressure(usize::MAX, 0, 0), 0.0);
    sim.add_emitter(fill_box(
        Vec3::splat(0.5),
        Vec3::splat(0.5),
        Vec3::ZERO,
        EmitterKind::Fill,
    ))
    .expect("emitter");
    sim.step();
    assert!(sim.is_liquid(0, 0, 0));
    assert_eq!(sim.stats().liquid_cells, 8);
}

/// Two runs from the same seed are bit-identical; a different seed gives a different state; and the
/// result does not depend on how many threads run simulations at once (the solver is sequential
/// and shares no state).
#[test]
fn determinism_across_seeds_and_threads() {
    fn run(seed: u64) -> u64 {
        let (lo, hi, dx) = presets::default_bounds("splash").expect("bounds");
        let params = FluidParams {
            bounds_min: lo,
            bounds_max: hi,
            cell_size: dx * 2.0,
            seed,
            ..FluidParams::default()
        };
        let mut sim = presets::build("splash", params).expect("splash");
        for _ in 0..40 {
            sim.step();
        }
        sim.state_digest()
    }
    let reference = run(5);
    assert_eq!(reference, run(5));
    assert_ne!(reference, run(6), "the seed reaches the jitter");
    for threads in [1usize, 2, 4] {
        let handles: Vec<_> = (0..threads).map(|_| std::thread::spawn(|| run(5))).collect();
        for h in handles {
            assert_eq!(
                h.join().expect("thread"),
                reference,
                "{threads} concurrent simulations"
            );
        }
    }
}

/// Bounded motion on a violent scene: every particle stays inside the domain and below a
/// free-fall-plus-impact speed bound for the whole run; the substep count obeys its cap.
#[test]
fn splash_stays_bounded() {
    let (lo, hi, dx) = presets::default_bounds("splash").expect("bounds");
    let params = FluidParams {
        bounds_min: lo,
        bounds_max: hi,
        cell_size: dx * 2.0,
        ..FluidParams::default()
    };
    let max_sub = params.max_substeps;
    let mut sim = presets::build("splash", params).expect("splash");
    // Free fall from the top of a 1 m box plus the 2 m/s release speed: sqrt(2 g h + v0²).
    let bound = (2.0f32 * 9.81 * 1.0 + 4.0).sqrt() * 1.6;
    let mut peak = 0.0f32;
    for step in 0..150 {
        sim.step();
        let s = sim.stats();
        assert!(
            s.substeps >= 1 && s.substeps <= max_sub,
            "step {step}: substeps {}",
            s.substeps
        );
        assert_eq!(s.dropped_non_finite, 0, "step {step}");
        for (p, v) in sim.positions().iter().zip(sim.velocities()) {
            assert!(
                p.cmpge(lo).all() && p.cmple(hi).all(),
                "step {step}: {p:?} left the domain"
            );
            peak = peak.max(v.length());
        }
    }
    assert!(peak < bound, "peak speed {peak} exceeds {bound}");
    assert!(peak > 1.0, "the scene did move ({peak})");
}

/// With no emitters, sinks or reseeding, a closed box keeps its particle count exactly, and the
/// pressure solve converges on every step of a resting column.
#[test]
fn closed_box_conserves_count_and_solves_every_step() {
    let mut sim = FluidSim::new(small(0.125, 1.0)).expect("sim");
    sim.add_emitter(fill_box(
        Vec3::new(0.5, 0.25, 0.5),
        Vec3::new(0.5, 0.25, 0.5),
        Vec3::ZERO,
        EmitterKind::Fill,
    ))
    .expect("emitter");
    sim.step();
    let n = sim.positions().len();
    assert!(n > 0);
    let mut unconverged = 0;
    for _ in 0..200 {
        sim.step();
        assert_eq!(sim.positions().len(), n);
        let s = sim.stats();
        if !s.pcg_converged {
            unconverged += 1;
        }
        assert!(s.pcg_iterations <= sim.params().pcg_max_iterations);
        assert!(s.pcg_residual.is_finite());
    }
    assert_eq!(
        unconverged, 0,
        "the pressure solve missed its tolerance on {unconverged} of 200 steps"
    );
}

/// Liquid poured on a flat heightfield floor comes to rest on it, never through it.
#[test]
fn liquid_rests_on_a_heightfield_floor() {
    let floor_y = 0.25;
    let floor = Collider::Heightfield {
        origin: [0.0, 0.0],
        spacing: 1.0,
        columns: 2,
        rows: 2,
        heights: vec![floor_y; 4],
    };
    let mut sim = FluidSim::with_colliders(small(0.125, 1.0), vec![floor]).expect("sim");
    sim.add_emitter(fill_box(
        Vec3::new(0.5, 0.6, 0.5),
        Vec3::new(0.4, 0.15, 0.4),
        Vec3::ZERO,
        EmitterKind::Fill,
    ))
    .expect("emitter");
    for step in 0..240 {
        sim.step();
        for p in sim.positions() {
            assert!(
                p.y > floor_y - 0.5 * 0.125,
                "step {step}: {p:?} is below the floor"
            );
        }
    }
    let min_y = sim.positions().iter().map(|p| p.y).fold(f32::MAX, f32::min);
    assert!(min_y < floor_y + 0.125, "the liquid reached the floor ({min_y})");
    let max_speed = sim.velocities().iter().map(|v| v.length()).fold(0.0f32, f32::max);
    assert!(max_speed < 1.5, "the pool is nearly at rest ({max_speed})");
}

/// Every preset fits the cell cap at its default bounds, validates, and emits a count of
/// particles within the documented per-cell budget: at most eight per cell of the domain.
#[test]
fn presets_default_bounds_and_particle_budget() {
    for name in presets::PRESETS {
        let (lo, hi, dx) = presets::default_bounds(name).expect(name);
        let params = FluidParams {
            bounds_min: lo,
            bounds_max: hi,
            cell_size: dx,
            ..FluidParams::default()
        };
        let dims = params.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(dims.cells() <= max_cells(), "{name}");
        let mut sim = presets::build(name, params).expect(name);
        sim.step();
        let n = sim.positions().len();
        assert!(n > 0, "{name} emitted nothing");
        assert!(
            n <= dims.cells() * PARTICLES_PER_CELL,
            "{name}: {n} particles in {} cells",
            dims.cells()
        );
    }
    assert!(presets::default_bounds("nope").is_none());
    let Err(err) = presets::build("nope", FluidParams::default()) else {
        panic!("an unknown preset must be refused");
    };
    assert!(
        err.to_string().contains("pour"),
        "the error lists the presets: {err}"
    );
    // Presets scale with the caller's bounds: halving the box halves the dam's width.
    let (lo, hi, dx) = presets::default_bounds("dam_break").expect("bounds");
    let half = FluidParams {
        bounds_min: lo,
        bounds_max: hi,
        cell_size: dx,
        reseed: false,
        ..FluidParams::default()
    };
    let mut full = presets::build("dam_break", half.clone()).expect("dam");
    full.step();
    let (_, e_hi) = full.emitters()[0].shape.aabb();
    assert!(
        (e_hi.x - 0.25 * (hi.x - lo.x)).abs() < 1e-5,
        "the block spans a quarter of the width"
    );
    assert!((e_hi.y - (2.0 / 3.0) * (hi.y - lo.y)).abs() < 1e-5);
}
