// SPDX-License-Identifier: MPL-2.0
//! XPBD cloth and rope solver (`regia_physics::cloth`): behavior, collision, determinism and
//! input validation, through the public API only.
//!
//! Every test runs a fixed number of fixed steps from fixed inputs: no wall clock, no GPU, no
//! files. Tolerances are stated next to each assertion.

use regia_core::math::Vec3;
use regia_physics::cloth::{
    ClothCollider, ClothConfig, ClothError, ClothGrid, ClothRope, ClothShape, ClothSolver,
};

/// One fixed step at the default rate.
const DT: f32 = 1.0 / 60.0;

fn solver(config: ClothConfig) -> ClothSolver {
    ClothSolver::new(config).expect("valid config")
}

/// Runs `steps` frames of `DT` and returns how many fixed steps ran.
fn run(solver: &mut ClothSolver, steps: usize) -> u32 {
    let mut total = 0;
    for _ in 0..steps {
        total += solver.step(DT).expect("finite dt");
    }
    total
}

/// Sum of the distances between consecutive particles in `first..first + count`.
fn polyline_length(solver: &ClothSolver, first: usize, count: usize) -> f32 {
    let p = &solver.particles()[first..first + count];
    p.windows(2)
        .map(|w| (w[1].position - w[0].position).length())
        .sum()
}

fn max_speed(solver: &ClothSolver) -> f32 {
    solver
        .particles()
        .iter()
        .map(|p| p.velocity.length())
        .fold(0.0, f32::max)
}

fn rope_desc(segments: usize, stretch_compliance: f32) -> ClothRope {
    ClothRope {
        start: Vec3::new(0.0, 3.0, 0.0),
        end: Vec3::new(2.0, 3.0, 0.0),
        segments,
        particle_mass: 0.05,
        stretch_compliance,
        bend_compliance: 1.0,
    }
}

fn grid_desc(columns: usize, rows: usize, origin: Vec3) -> ClothGrid {
    ClothGrid {
        origin,
        u_axis: Vec3::X,
        v_axis: Vec3::Z,
        columns,
        rows,
        spacing: 0.1,
        particle_mass: 0.01,
        stretch_compliance: 0.0,
        shear_compliance: 1e-4,
        bend_compliance: 1e-2,
    }
}

// ---------------------------------------------------------------------------
// Ropes
// ---------------------------------------------------------------------------

#[test]
fn pinned_rope_hangs_within_one_percent_of_rest_length() {
    // Damping 2/s: a 20-link chain released horizontally swings chaotically for a while; this
    // settles it to well under a millimeter within the 10 s run.
    let mut s = solver(ClothConfig {
        damping: 2.0,
        ..ClothConfig::default()
    });
    let segments = 20;
    let rope = s.add_rope(&rope_desc(segments, 0.0)).expect("rope");
    assert_eq!(rope.count, segments + 1);
    s.pin(rope.first).expect("pin the top");

    let rest = polyline_length(&s, rope.first, rope.count);
    assert!((rest - 2.0).abs() < 1e-5, "rest length {rest}");

    run(&mut s, 600); // 10 s

    let hung = polyline_length(&s, rope.first, rope.count);
    assert!(
        (hung - rest).abs() <= 0.01 * rest,
        "hanging length {hung} vs rest {rest}"
    );
    // It really hangs: the free end is (almost) straight below the pin.
    let top = s.particles()[rope.first].position;
    let bottom = s.particles()[rope.first + segments].position;
    assert_eq!(top, Vec3::new(0.0, 3.0, 0.0), "the pinned end never moves");
    assert!(top.y - bottom.y > 0.98 * rest, "drop {}", top.y - bottom.y);
    assert!((bottom.x - top.x).abs() < 0.01, "sideways {}", bottom.x - top.x);
}

#[test]
fn stiff_rope_stretches_less_than_compliant_rope() {
    let hang = |compliance: f32| {
        let mut s = solver(ClothConfig {
            damping: 0.5,
            ..ClothConfig::default()
        });
        let rope = s.add_rope(&rope_desc(10, compliance)).expect("rope");
        s.pin(rope.first).expect("pin");
        run(&mut s, 600);
        polyline_length(&s, rope.first, rope.count)
    };
    let stiff = hang(0.0);
    let soft = hang(0.05);
    assert!((stiff - 2.0).abs() < 0.01 * 2.0, "stiff rope {stiff}");
    // Compliance is inverse stiffness (m/N): the soft rope must sag measurably longer.
    assert!(soft > stiff * 1.02, "soft {soft} vs stiff {stiff}");
}

#[test]
fn rope_builder_counts_particles_and_constraints() {
    let mut s = solver(ClothConfig::default());
    let rope = s.add_rope(&rope_desc(5, 0.0)).expect("rope");
    assert_eq!((rope.first, rope.count), (0, 6));
    let stats = s.stats();
    assert_eq!(stats.particles, 6);
    // 5 stretch links plus 4 skip-one bend links.
    assert_eq!(stats.distance_constraints, 9);
    assert_eq!(stats.bend_constraints, 0);
    // Evenly spaced along the segment.
    for (i, p) in s.particles().iter().enumerate() {
        let expected = Vec3::new(0.4 * i as f32, 3.0, 0.0);
        assert!((p.position - expected).length() < 1e-6, "particle {i}");
        assert_eq!(p.inv_mass, 20.0);
    }
}

// ---------------------------------------------------------------------------
// Cloth
// ---------------------------------------------------------------------------

#[test]
fn grid_builder_counts_particles_and_constraints() {
    let mut s = solver(ClothConfig::default());
    let grid = s.add_grid(&grid_desc(3, 3, Vec3::ZERO)).expect("grid");
    assert_eq!(grid.first, 0);
    assert_eq!(grid.index(2, 1), Some(5));
    assert_eq!(grid.index(3, 0), None);
    let stats = s.stats();
    assert_eq!(stats.particles, 9);
    // 6 horizontal + 6 vertical + 2 diagonals in each of the 4 quads.
    assert_eq!(stats.distance_constraints, 20);
    // Interior edges of the 8-triangle mesh.
    assert_eq!(stats.bend_constraints, 8);
    assert_eq!(
        s.particles()[grid.index(2, 1).expect("in range")].position,
        Vec3::new(0.2, 0.0, 0.1)
    );
}

#[test]
fn pinned_corner_cloth_comes_to_rest() {
    let mut s = solver(ClothConfig {
        damping: 1.0,
        ..ClothConfig::default()
    });
    let grid = s
        .add_grid(&grid_desc(10, 10, Vec3::new(0.0, 2.0, 0.0)))
        .expect("grid");
    let a = grid.index(0, 0).expect("corner");
    let b = grid.index(9, 0).expect("corner");
    s.pin(a).expect("pin");
    s.pin(b).expect("pin");
    let (pin_a, pin_b) = (s.particles()[a].position, s.particles()[b].position);

    run(&mut s, 60);
    assert!(max_speed(&s) > 0.1, "the cloth must be falling early on");

    run(&mut s, 1200); // 20 s more
    let before: Vec<Vec3> = s.particles().iter().map(|p| p.position).collect();
    run(&mut s, 60);
    let drift = s
        .particles()
        .iter()
        .zip(&before)
        .map(|(p, q)| (p.position - *q).length())
        .fold(0.0, f32::max);
    assert!(max_speed(&s) < 1e-2, "max speed {}", max_speed(&s));
    assert!(drift < 1e-2, "drift over the last second {drift}");
    // The pins held and the cloth hangs below them.
    assert_eq!(s.particles()[a].position, pin_a);
    assert_eq!(s.particles()[b].position, pin_b);
    assert_eq!(pin_a, Vec3::new(0.0, 2.0, 0.0));
    let lowest = s
        .particles()
        .iter()
        .map(|p| p.position.y)
        .fold(f32::MAX, f32::min);
    assert!(lowest < 1.4, "lowest point {lowest}");
    // Stretch compliance 0: no link is longer than 2 % over rest when settled.
    assert!(
        s.stats().max_stretch < 1.02,
        "max stretch {}",
        s.stats().max_stretch
    );
}

#[test]
fn stiff_bending_sags_less_than_soft_bending() {
    // A strip clamped along one short edge (two pinned rows) sticks out horizontally.
    let sag = |bend_compliance: f32| {
        let mut s = solver(ClothConfig {
            damping: 2.0,
            ..ClothConfig::default()
        });
        let grid = s
            .add_grid(&ClothGrid {
                bend_compliance,
                ..grid_desc(12, 3, Vec3::new(0.0, 1.0, 0.0))
            })
            .expect("grid");
        for row in 0..3 {
            for column in 0..2 {
                s.pin(grid.index(column, row).expect("in range")).expect("pin");
            }
        }
        run(&mut s, 300);
        let tip = s.particles()[grid.index(11, 1).expect("tip")].position;
        1.0 - tip.y
    };
    let stiff = sag(1e-6);
    let soft = sag(1.0);
    assert!(stiff >= 0.0 && soft > 0.0, "stiff {stiff} soft {soft}");
    assert!(soft > stiff + 0.1, "soft sag {soft} vs stiff sag {stiff}");
}

#[test]
fn cloth_dropped_on_sphere_does_not_penetrate() {
    let config = ClothConfig::default();
    let thickness = config.thickness;
    let mut s = solver(config);
    let center = Vec3::new(0.55, 0.0, 0.55);
    let radius = 0.5;
    s.add_collider(ClothCollider {
        shape: ClothShape::Sphere { center, radius },
        friction: 0.8,
    })
    .expect("sphere");
    // Soft shear and bending let the cloth fold around the ball; a cloth stiff in every
    // direction balances on a few points and tips off.
    s.add_grid(&ClothGrid {
        shear_compliance: 1e-2,
        bend_compliance: 1.0,
        ..grid_desc(12, 12, Vec3::new(0.0, 0.8, 0.0))
    })
    .expect("grid");

    let eps = 1e-4;
    let mut touched = false;
    for step in 0..240 {
        s.step(DT).expect("step");
        for (i, p) in s.particles().iter().enumerate() {
            let d = (p.position - center).length();
            assert!(
                d >= radius + thickness - eps,
                "step {step}: particle {i} is {} inside the sphere",
                radius + thickness - d
            );
            touched |= d < radius + thickness + 1e-3;
        }
    }
    assert!(touched, "the cloth must reach the sphere");
    // After 4 s it still rests on the ball, centered, with its edges hanging down the sides.
    assert!(s.stats().contacts > 0, "still in contact at the end");
    let n = s.particles().len() as f32;
    let centroid = s.particles().iter().map(|p| p.position).sum::<Vec3>() / n;
    assert!(
        (centroid.x - center.x).abs() < 0.15 && (centroid.z - center.z).abs() < 0.15,
        "the cloth stayed on the ball: centroid {centroid}"
    );
    let lowest = s
        .particles()
        .iter()
        .map(|p| p.position.y)
        .fold(f32::MAX, f32::min);
    assert!((0.0..0.35).contains(&lowest), "lowest point {lowest}");
}

#[test]
fn particles_rest_on_a_capsule_and_a_plane() {
    let config = ClothConfig::default();
    let thickness = config.thickness;
    let mut s = solver(config);
    s.add_collider(ClothCollider {
        shape: ClothShape::Capsule {
            a: Vec3::new(-1.0, 0.5, 0.0),
            b: Vec3::new(1.0, 0.5, 0.0),
            radius: 0.2,
        },
        friction: 1.0,
    })
    .expect("capsule");
    // A floor through y = 0, given with a non-unit normal (it is normalized).
    s.add_collider(ClothCollider {
        shape: ClothShape::Plane {
            normal: Vec3::new(0.0, 2.0, 0.0),
            offset: 0.0,
        },
        friction: 1.0,
    })
    .expect("plane");
    let on_capsule = s.add_particle(Vec3::new(0.3, 1.5, 0.0), 1.0).expect("particle");
    let on_floor = s.add_particle(Vec3::new(0.0, 1.0, 2.0), 1.0).expect("particle");

    run(&mut s, 180);
    let p = s.particles()[on_capsule].position;
    assert!(
        (p.y - (0.5 + 0.2 + thickness)).abs() < 1e-3,
        "on the capsule at {p}"
    );
    assert!(
        (p.x - 0.3).abs() < 1e-3 && p.z.abs() < 1e-3,
        "no sideways drift: {p}"
    );
    let q = s.particles()[on_floor].position;
    assert!((q.y - thickness).abs() < 1e-3, "on the floor at {q}");
}

#[test]
fn friction_stops_a_sliding_particle() {
    let slide = |friction: f32| {
        let mut s = solver(ClothConfig {
            damping: 0.0,
            ..ClothConfig::default()
        });
        s.add_collider(ClothCollider {
            shape: ClothShape::Plane {
                normal: Vec3::Y,
                offset: 0.0,
            },
            friction,
        })
        .expect("plane");
        let thickness = s.config().thickness;
        let i = s
            .add_particle(Vec3::new(0.0, thickness, 0.0), 1.0)
            .expect("particle");
        s.set_velocity(i, Vec3::new(2.0, 0.0, 0.0)).expect("velocity");
        run(&mut s, 60); // 1 s
        s.particles()[i]
    };
    let ice = slide(0.0);
    assert!(ice.position.x > 1.9, "frictionless slide {}", ice.position.x);
    assert!(
        (ice.velocity.x - 2.0).abs() < 0.02,
        "keeps its speed {}",
        ice.velocity.x
    );
    let rubber = slide(1.0);
    // Coulomb: decelerates at about mu * g, so it stops after about 0.2 m.
    assert!(rubber.position.x < 0.3, "rough slide {}", rubber.position.x);
    assert!(rubber.velocity.length() < 1e-3, "stopped {}", rubber.velocity);
}

#[test]
fn moving_a_pinned_particle_drags_the_rope() {
    let mut s = solver(ClothConfig {
        damping: 0.5,
        ..ClothConfig::default()
    });
    let rope = s.add_rope(&rope_desc(10, 0.0)).expect("rope");
    s.pin(rope.first).expect("pin");
    run(&mut s, 300);
    s.set_position(rope.first, Vec3::new(5.0, 3.0, 0.0))
        .expect("move pin");
    // 25 s: the jump sets the rope swinging, and at damping 0.5 it still swings about 0.3 m
    // either side after 10 s (ADR-3273), so a check at 10 s only hit the swing's phase.
    run(&mut s, 1500);
    let bottom = s.particles()[rope.first + 10].position;
    assert!((bottom.x - 5.0).abs() < 0.05, "the rope followed: {bottom}");
    assert_eq!(s.particles()[rope.first].position, Vec3::new(5.0, 3.0, 0.0));
}

// ---------------------------------------------------------------------------
// Fixed timestep
// ---------------------------------------------------------------------------

#[test]
fn step_runs_whole_fixed_steps_and_keeps_the_remainder() {
    let mut s = solver(ClothConfig::default());
    s.add_particle(Vec3::ZERO, 1.0).expect("particle");
    assert_eq!(s.step(0.0).expect("zero dt"), 0);
    assert_eq!(s.step(DT / 2.0).expect("half"), 0);
    assert_eq!(s.particles()[0].position, Vec3::ZERO, "no step ran yet");
    assert_eq!(s.step(DT / 2.0).expect("half"), 1);
    assert!(s.particles()[0].position.y < 0.0, "gravity applied");
    // 3.5 steps: three run, half a step stays in the accumulator.
    assert_eq!(s.step(3.5 * DT).expect("three and a half"), 3);
    assert_eq!(s.stats().steps, 4);
    assert_eq!(s.stats().dropped_steps, 0);
}

#[test]
fn long_frames_are_capped_and_the_excess_dropped() {
    let mut s = solver(ClothConfig {
        max_steps_per_call: 4,
        ..ClothConfig::default()
    });
    s.add_particle(Vec3::ZERO, 1.0).expect("particle");
    assert_eq!(s.step(10.0 * DT + DT / 4.0).expect("long frame"), 4);
    let stats = s.stats();
    assert_eq!(stats.steps, 4);
    assert_eq!(stats.dropped_steps, 6);
    // The quarter-step remainder survives: another quarter does not run a step…
    assert_eq!(s.step(DT / 4.0).expect("quarter"), 0);
    // …and 0.6 more completes one only because of it (0.25 + 0.25 + 0.6 = 1.1 steps).
    assert_eq!(s.step(DT * 0.6).expect("more"), 1);
}

#[test]
fn huge_frames_are_capped_without_overflow() {
    let mut s = solver(ClothConfig::default());
    s.add_particle(Vec3::ZERO, 1.0).expect("particle");
    let cap = s.config().max_steps_per_call;
    // f32::MAX twice would overflow the accumulator to infinity if it were kept.
    assert_eq!(s.step(f32::MAX).expect("finite"), cap);
    assert_eq!(s.step(f32::MAX).expect("finite"), cap);
    let stats = s.stats();
    assert_eq!(stats.steps, 2 * u64::from(cap));
    assert!(stats.dropped_steps > 1_000_000, "dropped {}", stats.dropped_steps);
    assert!(s.particles()[0].position.is_finite());
    // The accumulator is sane afterwards: one ordinary frame runs one step.
    assert_eq!(s.step(DT).expect("normal frame"), 1);
}

#[test]
fn moving_a_collider_pushes_resting_particles() {
    let mut s = solver(ClothConfig {
        gravity: Vec3::ZERO,
        ..ClothConfig::default()
    });
    let ball = s
        .add_collider(ClothCollider {
            shape: ClothShape::Sphere {
                center: Vec3::new(0.0, -5.0, 0.0),
                radius: 0.5,
            },
            friction: 0.0,
        })
        .expect("sphere");
    let i = s.add_particle(Vec3::ZERO, 1.0).expect("particle");
    run(&mut s, 10);
    assert_eq!(s.particles()[i].position, Vec3::ZERO, "out of reach: untouched");
    // Teleport the ball so the particle is 0.1 m inside it, below its center.
    s.set_collider(
        ball,
        ClothCollider {
            shape: ClothShape::Sphere {
                center: Vec3::new(0.0, 0.4, 0.0),
                radius: 0.5,
            },
            friction: 0.0,
        },
    )
    .expect("move");
    assert_eq!(
        s.colliders()[ball].shape,
        ClothShape::Sphere {
            center: Vec3::new(0.0, 0.4, 0.0),
            radius: 0.5
        }
    );
    s.step(DT).expect("step");
    let p = s.particles()[i].position;
    let thickness = s.config().thickness;
    assert!(
        (p.y - (0.4 - 0.5 - thickness)).abs() < 1e-4,
        "pushed out of the bottom of the ball: {p}"
    );
}

// ---------------------------------------------------------------------------
// Determinism and degenerate input
// ---------------------------------------------------------------------------

fn scripted_scene() -> ClothSolver {
    let mut s = solver(ClothConfig {
        damping: 0.2,
        ..ClothConfig::default()
    });
    let grid = s
        .add_grid(&grid_desc(8, 8, Vec3::new(-0.2, 1.2, -0.2)))
        .expect("grid");
    s.pin(grid.index(0, 0).expect("corner")).expect("pin");
    s.add_collider(ClothCollider {
        shape: ClothShape::Sphere {
            center: Vec3::new(0.3, 0.6, 0.3),
            radius: 0.25,
        },
        friction: 0.3,
    })
    .expect("sphere");
    s.add_collider(ClothCollider {
        shape: ClothShape::Capsule {
            a: Vec3::new(-1.0, 0.2, 0.5),
            b: Vec3::new(1.0, 0.3, 0.4),
            radius: 0.1,
        },
        friction: 0.6,
    })
    .expect("capsule");
    s.add_collider(ClothCollider {
        shape: ClothShape::Plane {
            normal: Vec3::new(0.1, 1.0, 0.0),
            offset: -0.1,
        },
        friction: 0.8,
    })
    .expect("plane");
    let rope = s
        .add_rope(&ClothRope {
            start: Vec3::new(1.0, 1.5, 0.0),
            end: Vec3::new(1.0, 1.5, 1.0),
            segments: 8,
            particle_mass: 0.02,
            stretch_compliance: 1e-4,
            bend_compliance: 0.1,
        })
        .expect("rope");
    s.pin(rope.first).expect("pin");
    s
}

#[test]
fn identical_runs_are_bit_identical() {
    let mut a = scripted_scene();
    let mut b = scripted_scene();
    // Uneven frame times exercise the accumulator too.
    let frames = [DT, DT * 0.5, DT * 1.7, DT * 0.3, DT * 2.5];
    for i in 0..200 {
        let dt = frames[i % frames.len()];
        assert_eq!(a.step(dt).expect("a"), b.step(dt).expect("b"));
        assert_eq!(a.state_digest(), b.state_digest(), "diverged at frame {i}");
    }
    for (p, q) in a.particles().iter().zip(b.particles()) {
        for (x, y) in p.position.to_array().into_iter().zip(q.position.to_array()) {
            assert_eq!(x.to_bits(), y.to_bits());
        }
        for (x, y) in p.velocity.to_array().into_iter().zip(q.velocity.to_array()) {
            assert_eq!(x.to_bits(), y.to_bits());
        }
    }
    // The digest is sensitive: one nudged particle changes it.
    let before = b.state_digest();
    let last = b.particles().len() - 1;
    let nudged = b.particles()[last].position + Vec3::new(0.0, 1e-6, 0.0);
    b.set_position(last, nudged).expect("nudge");
    assert_ne!(b.state_digest(), before);
}

#[test]
fn zero_mass_everywhere_stays_put() {
    let mut s = solver(ClothConfig::default());
    let grid = s
        .add_grid(&ClothGrid {
            particle_mass: 0.0,
            ..grid_desc(5, 5, Vec3::new(0.0, 0.1, 0.0))
        })
        .expect("a grid of pinned particles");
    // A sphere and a floor overlapping the cloth, and a stretched extra link.
    s.add_collider(ClothCollider {
        shape: ClothShape::Sphere {
            center: Vec3::new(0.2, 0.1, 0.2),
            radius: 0.3,
        },
        friction: 0.5,
    })
    .expect("sphere");
    s.add_collider(ClothCollider {
        shape: ClothShape::Plane {
            normal: Vec3::Y,
            offset: 1.0,
        },
        friction: 0.5,
    })
    .expect("plane");
    let far = s.add_particle(Vec3::new(3.0, 0.1, 0.0), 0.0).expect("particle");
    s.add_distance(grid.first, far, 0.0).expect("link");
    let before: Vec<_> = s.particles().to_vec();
    assert_eq!(s.stats().pinned, before.len());

    run(&mut s, 120);
    assert_eq!(s.particles(), &before[..], "nothing moves");
    for p in s.particles() {
        assert!(p.position.is_finite() && p.velocity == Vec3::ZERO);
    }
}

#[test]
fn non_finite_input_is_rejected_with_a_typed_error() {
    let mut s = solver(ClothConfig::default());
    let a = s.add_particle(Vec3::ZERO, 1.0).expect("a");
    let b = s.add_particle(Vec3::X, 1.0).expect("b");
    let c = s.add_particle(Vec3::new(0.0, 0.0, 1.0), 1.0).expect("c");
    let d = s.add_particle(Vec3::new(1.0, 0.0, -1.0), 1.0).expect("d");

    let non_finite = |r: Result<usize, ClothError>| matches!(r, Err(ClothError::NonFinite { .. }));
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(non_finite(s.add_particle(Vec3::new(bad, 0.0, 0.0), 1.0)));
        assert!(non_finite(s.add_particle(Vec3::ZERO, bad)));
        assert!(non_finite(s.add_distance(a, b, bad)));
        assert!(non_finite(s.add_bend(a, b, c, d, bad)));
        assert!(non_finite(s.add_collider(ClothCollider {
            shape: ClothShape::Sphere {
                center: Vec3::new(0.0, bad, 0.0),
                radius: 1.0,
            },
            friction: 0.0,
        })));
        assert!(non_finite(s.add_collider(ClothCollider {
            shape: ClothShape::Capsule {
                a: Vec3::ZERO,
                b: Vec3::X,
                radius: bad,
            },
            friction: 0.0,
        })));
        assert!(non_finite(s.add_collider(ClothCollider {
            shape: ClothShape::Plane {
                normal: Vec3::Y,
                offset: 0.0,
            },
            friction: bad,
        })));
        assert!(matches!(s.step(bad), Err(ClothError::NonFinite { what: "dt" })));
        assert!(matches!(
            s.set_position(a, Vec3::splat(bad)),
            Err(ClothError::NonFinite { .. })
        ));
        assert!(matches!(
            s.set_velocity(a, Vec3::new(0.0, 0.0, bad)),
            Err(ClothError::NonFinite { .. })
        ));
        assert!(matches!(
            ClothSolver::new(ClothConfig {
                gravity: Vec3::new(0.0, bad, 0.0),
                ..ClothConfig::default()
            }),
            Err(ClothError::NonFinite { what: "gravity" })
        ));
        assert!(matches!(
            s.add_grid(&ClothGrid {
                spacing: bad,
                ..grid_desc(2, 2, Vec3::ZERO)
            }),
            Err(ClothError::NonFinite { .. })
        ));
        assert!(matches!(
            s.add_rope(&ClothRope {
                end: Vec3::splat(bad),
                ..rope_desc(2, 0.0)
            }),
            Err(ClothError::NonFinite { .. })
        ));
    }
    // Rejected input leaves the solver untouched.
    let stats = s.stats();
    assert_eq!(
        (
            stats.particles,
            stats.distance_constraints,
            stats.bend_constraints,
            stats.colliders
        ),
        (4, 0, 0, 0)
    );
    assert_eq!(stats.steps, 0);
    assert_eq!(s.particles()[a].position, Vec3::ZERO);
}

#[test]
fn invalid_input_is_rejected_with_a_typed_error() {
    let mut s = solver(ClothConfig::default());
    let a = s.add_particle(Vec3::ZERO, 1.0).expect("a");
    let b = s.add_particle(Vec3::X, 1.0).expect("b");

    assert_eq!(
        s.add_particle(Vec3::ZERO, -1.0),
        Err(ClothError::Negative { what: "inverse mass" })
    );
    assert_eq!(
        s.add_distance(a, b, -1.0),
        Err(ClothError::Negative { what: "compliance" })
    );
    assert_eq!(s.step(-DT), Err(ClothError::Negative { what: "dt" }));
    assert_eq!(
        s.add_distance(a, 7, 0.0),
        Err(ClothError::ParticleOutOfRange { index: 7, len: 2 })
    );
    assert_eq!(
        s.add_distance(a, a, 0.0),
        Err(ClothError::RepeatedParticle { index: a })
    );
    assert_eq!(
        s.add_bend(a, b, a, b, 0.0),
        Err(ClothError::RepeatedParticle { index: a })
    );
    assert_eq!(s.pin(9), Err(ClothError::ParticleOutOfRange { index: 9, len: 2 }));
    assert_eq!(
        s.add_collider(ClothCollider {
            shape: ClothShape::Plane {
                normal: Vec3::ZERO,
                offset: 0.0,
            },
            friction: 0.0,
        }),
        Err(ClothError::Degenerate { what: "plane normal" })
    );
    assert_eq!(
        s.add_collider(ClothCollider {
            shape: ClothShape::Sphere {
                center: Vec3::ZERO,
                radius: -0.1,
            },
            friction: 0.0,
        }),
        Err(ClothError::Negative { what: "radius" })
    );
    assert_eq!(
        s.set_collider(
            3,
            ClothCollider {
                shape: ClothShape::Sphere {
                    center: Vec3::ZERO,
                    radius: 0.1,
                },
                friction: 0.0,
            }
        ),
        Err(ClothError::ColliderOutOfRange { index: 3, len: 0 })
    );
    assert_eq!(
        s.add_grid(&grid_desc(1, 4, Vec3::ZERO)),
        Err(ClothError::Degenerate { what: "grid size" })
    );
    assert_eq!(
        s.add_grid(&ClothGrid {
            u_axis: Vec3::ZERO,
            ..grid_desc(2, 2, Vec3::ZERO)
        }),
        Err(ClothError::Degenerate { what: "grid axis" })
    );
    assert_eq!(
        s.add_rope(&rope_desc(0, 0.0)),
        Err(ClothError::Degenerate {
            what: "rope segments"
        })
    );
    assert_eq!(
        s.add_rope(&ClothRope {
            end: Vec3::new(0.0, 3.0, 0.0),
            ..rope_desc(4, 0.0)
        }),
        Err(ClothError::Degenerate { what: "rope length" })
    );
    for (config, what) in [
        (
            ClothConfig {
                substeps: 0,
                ..ClothConfig::default()
            },
            "substeps",
        ),
        (
            ClothConfig {
                iterations: 0,
                ..ClothConfig::default()
            },
            "iterations",
        ),
        (
            ClothConfig {
                time_step: 0.0,
                ..ClothConfig::default()
            },
            "time step",
        ),
        (
            ClothConfig {
                max_steps_per_call: 0,
                ..ClothConfig::default()
            },
            "max steps per call",
        ),
    ] {
        assert_eq!(
            ClothSolver::new(config).err(),
            Some(ClothError::InvalidConfig { what })
        );
    }
    assert_eq!(
        ClothSolver::new(ClothConfig {
            damping: -1.0,
            ..ClothConfig::default()
        })
        .err(),
        Some(ClothError::Negative { what: "damping" })
    );
    // Nothing was added by the failed calls.
    let stats = s.stats();
    assert_eq!(
        (stats.particles, stats.distance_constraints, stats.colliders),
        (2, 0, 0)
    );
}

#[test]
fn error_codes_are_protocol_safe() {
    let errors = [
        ClothError::NonFinite { what: "dt" },
        ClothError::Negative { what: "dt" },
        ClothError::ParticleOutOfRange { index: 1, len: 0 },
        ClothError::ColliderOutOfRange { index: 1, len: 0 },
        ClothError::RepeatedParticle { index: 0 },
        ClothError::Degenerate { what: "grid size" },
        ClothError::InvalidConfig { what: "substeps" },
    ];
    let mut codes: Vec<&str> = errors.iter().map(ClothError::code).collect();
    for code in &codes {
        assert!(code.starts_with("cloth_"), "{code}");
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{code}"
        );
    }
    codes.sort_unstable();
    codes.dedup();
    assert_eq!(codes.len(), errors.len(), "codes are distinct");
    assert!(errors[2].to_string().contains("particle 1"));
}
