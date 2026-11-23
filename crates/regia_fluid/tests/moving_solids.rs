// SPDX-License-Identifier: MPL-2.0
//! Moving solids in the CPU reference (ADR-2577): kinematic solids as moving boundaries of the
//! pressure projection and the pressure impulse they receive. Fixed steps, no wall clock, no goldens.

use glam::Vec3;
use regia_fluid::sim::{Emitter, EmitterKind, FluidParams, FluidSim, Shape};
use regia_fluid::solid::{Collider, MovingSolid, SolidShape};

/// A closed tank `size` with liquid `depth` deep, `dx` cells, no whitewater.
fn tank(size: Vec3, dx: f32, depth: f32, reseed: bool) -> FluidSim {
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: size,
        cell_size: dx,
        dt: 1.0 / 60.0,
        reseed,
        // The mobile cap: keeps the suite runnable on Android aarch64 (the digest pins must be
        // re-checked there). The fills stay far below it.
        particle_cap: 65_536,
        ..FluidParams::default()
    };
    params.whitewater.enabled = false;
    let mut sim = FluidSim::new(params).expect("tank");
    sim.add_emitter(Emitter {
        shape: Shape::Box {
            center: Vec3::new(size.x * 0.5, depth * 0.5, size.z * 0.5),
            half_extents: Vec3::new(size.x * 0.5, depth * 0.5, size.z * 0.5),
        },
        velocity: Vec3::ZERO,
        kind: EmitterKind::Fill,
        done: false,
    })
    .expect("fill");
    sim
}

fn mean_velocity(sim: &FluidSim) -> Vec3 {
    let v = sim.velocities();
    v.iter().fold(Vec3::ZERO, |s, x| s + *x) / v.len().max(1) as f32
}

/// A paddle (a box spanning the tank's depth, open below and above) at `x`, moving at `speed`
/// along x after 10 settling steps; returns the mean liquid velocity and the paddle's x impulse
/// over the moving steps.
fn paddle_run(speed: f32) -> (Vec3, f32, FluidSim) {
    let size = Vec3::new(0.8, 0.4, 0.1);
    let mut sim = tank(size, 0.025, 0.2, true);
    let paddle = MovingSolid::kinematic(
        SolidShape::Box {
            half_extents: Vec3::new(0.03, 0.08, 0.2),
        },
        Vec3::new(0.4, 0.13, 0.05),
    );
    sim.set_moving_solids(vec![paddle]).expect("paddle");
    for _ in 0..10 {
        sim.step();
    }
    let mut moving = sim.moving_solids()[0];
    moving.linear_velocity = Vec3::new(speed, 0.0, 0.0);
    sim.set_moving_solids(vec![moving]).expect("moving paddle");
    let mut impulse = 0.0;
    for _ in 0..20 {
        sim.step();
        impulse += sim.solid_impulses()[0].linear.x;
    }
    (mean_velocity(&sim), impulse, sim)
}

/// A paddle moving through still liquid gives it momentum in its own direction, the liquid pushes
/// back on the paddle, and a paddle at rest leaves the liquid near rest.
#[test]
fn paddle_imparts_momentum_in_its_direction() {
    let (forward, push_back, sim) = paddle_run(0.5);
    let (backward, push_forward, _) = paddle_run(-0.5);
    let (rest, _, _) = paddle_run(0.0);
    eprintln!(
        "mean liquid velocity: paddle +0.5 m/s {forward}, -0.5 m/s {backward}, at rest {rest}; \
         x impulse on the paddle {push_back} / {push_forward} N s"
    );
    // The paddle moved 20 steps at 0.5 m/s.
    assert!((sim.moving_solids()[0].position.x - (0.4 + 0.5 * 20.0 / 60.0)).abs() < 1e-4);
    assert!(forward.x > 0.02, "forward paddle: {forward}");
    assert!(backward.x < -0.02, "backward paddle: {backward}");
    assert!(rest.x.abs() < 0.2 * forward.x, "paddle at rest: {rest}");
    // The liquid resists the paddle: the impulse on it points against its motion.
    assert!(push_back < 0.0, "x impulse on the forward paddle {push_back}");
    assert!(
        push_forward > 0.0,
        "x impulse on the backward paddle {push_forward}"
    );
}

/// A wall spanning the whole cross-section moves through the tank: the liquid on each side stays on
/// its side (no flow through a moving wall), and the side it moves into rises.
#[test]
fn moving_wall_lets_no_liquid_through() {
    let size = Vec3::new(0.8, 0.4, 0.1);
    // No reseeding: particle counts per side are the flux measure.
    let mut sim = tank(size, 0.025, 0.2, false);
    let x0 = 0.4;
    let wall = MovingSolid::kinematic(
        SolidShape::Box {
            half_extents: Vec3::new(0.03, 0.3, 0.2),
        },
        Vec3::new(x0, 0.2, 0.05),
    );
    sim.set_moving_solids(vec![wall]).expect("wall");
    for _ in 0..5 {
        sim.step();
    }
    let side_counts = |sim: &FluidSim| {
        let x = sim.moving_solids()[0].position.x;
        let left = sim.positions().iter().filter(|p| p.x < x).count();
        (left, sim.positions().len() - left)
    };
    let mean_height = |sim: &FluidSim, right: bool| {
        let x = sim.moving_solids()[0].position.x;
        let ys: Vec<f32> = sim
            .positions()
            .iter()
            .filter(|p| (p.x > x) == right)
            .map(|p| p.y)
            .collect();
        ys.iter().sum::<f32>() / ys.len().max(1) as f32
    };
    let (left0, right0) = side_counts(&sim);
    let right_height0 = mean_height(&sim, true);
    let mut moving = sim.moving_solids()[0];
    moving.linear_velocity = Vec3::new(0.2, 0.0, 0.0);
    sim.set_moving_solids(vec![moving]).expect("moving wall");
    let mut worst = 0usize;
    for _ in 0..40 {
        sim.step();
        let (left, _) = side_counts(&sim);
        worst = worst.max(left.abs_diff(left0));
    }
    let (left1, right1) = side_counts(&sim);
    let total = (left0 + right0) as f32;
    let right_height1 = mean_height(&sim, true);
    eprintln!(
        "wall moved to {:.3}: left {left0} -> {left1}, right {right0} -> {right1}, worst change {worst}; \
         right mean height {right_height0:.4} -> {right_height1:.4}",
        sim.moving_solids()[0].position.x
    );
    // Flux bound: at most 0.2 % of the particles ever on the wrong side.
    assert!(
        (worst as f32) <= 0.002 * total,
        "{worst} particles crossed the moving wall (of {total})"
    );
    assert_eq!(left0 + right0, left1 + right1);
    assert!(
        right_height1 > right_height0 + 0.01,
        "the wall pushed the right side up: {right_height0} -> {right_height1}"
    );
}

/// A box held still, 0.4 m deep in liquid at rest, feels the weight of the liquid it displaces: the
/// vertical pressure impulse per step is `rho g V_submerged dt` (Archimedes) within 5 %, with the
/// free surface measured away from the box. (The depth is 40 cells: the first-order free surface
/// can put the pressure up to about two cells of depth high.)
#[test]
fn held_box_feels_archimedes_buoyancy() {
    let size = Vec3::new(0.6, 0.7, 0.04);
    let dx = 0.01;
    let mut sim = tank(size, dx, 0.5, true);
    let (half_w, half_h, bottom) = (0.08f32, 0.25f32, 0.1f32);
    let held = MovingSolid::kinematic(
        SolidShape::Box {
            half_extents: Vec3::new(half_w, half_h, 0.1),
        },
        Vec3::new(0.3, bottom + half_h, 0.02),
    );
    sim.set_moving_solids(vec![held]).expect("box");
    let steps: u32 = 90;
    let mut impulse = 0.0f64;
    let mut height = 0.0f64;
    let window = 30;
    for step in 0..steps {
        sim.step();
        if step.is_multiple_of(15) {
            let s = sim.stats();
            eprintln!(
                "held box step {step}: particles {}, volume {:.5}, drift {:.3}, pcg {} it converged {}, \
                 divergence {:.3}, impulse {}",
                s.particles,
                s.volume,
                s.volume_drift,
                s.pcg_iterations,
                s.pcg_converged,
                s.divergence_max,
                sim.solid_impulses()[0].linear
            );
        }
        if step >= steps - window {
            impulse += f64::from(sim.solid_impulses()[0].linear.y);
            height += f64::from(free_surface(&sim, 0.3 - half_w - 0.04, 0.3 + half_w + 0.04));
        }
    }
    let impulse = impulse / f64::from(window);
    let surface = height / f64::from(window);
    let params = sim.params();
    let submerged = (surface - f64::from(bottom)) * f64::from(2.0 * half_w) * f64::from(size.z);
    let expected = f64::from(params.density) * 9.81 * submerged * f64::from(params.dt);
    let error = (impulse - expected).abs() / expected;
    eprintln!(
        "held box: surface {surface:.4} m, submerged {submerged:.6} m³, impulse {impulse:.6} vs {expected:.6} N s \
         ({:.2} %)",
        error * 100.0
    );
    assert!(
        error < 0.05,
        "buoyancy impulse {impulse} vs Archimedes {expected}"
    );
}

/// Mean free-surface height of the columns outside `[skip_lo, skip_hi]` in x.
fn free_surface(sim: &FluidSim, skip_lo: f32, skip_hi: f32) -> f32 {
    let d = *sim.dims();
    // The free surface of a column: its top particle plus a quarter cell (the 2 x 2 x 2 seeding puts
    // the top particle layer a quarter cell below the liquid's top). A particle count per cell
    // under-reads it, because surface cells settle with fewer than eight particles.
    let mut top = vec![f32::NEG_INFINITY; d.nx * d.nz];
    for p in sim.positions() {
        let Some((i, _, k)) = d.cell_of(*p) else {
            continue;
        };
        let t = &mut top[i + d.nx * k];
        *t = t.max(p.y);
    }
    let mut sum = 0.0;
    let mut n = 0;
    for k in 0..d.nz {
        for i in 0..d.nx {
            let x = d.origin.x + (i as f32 + 0.5) * d.dx;
            let t = top[i + d.nx * k];
            if (x > skip_lo && x < skip_hi) || !t.is_finite() {
                continue;
            }
            sum += t + 0.25 * d.dx;
            n += 1;
        }
    }
    sum / n.max(1) as f32
}

/// The same inputs give the same digest at every step with moving solids and coupling, and a
/// different paddle speed gives a different run.
#[test]
fn moving_solids_are_deterministic() {
    let run = |speed: f32| {
        let mut sim = tank(Vec3::new(0.6, 0.3, 0.1), 0.025, 0.15, true);
        let mut paddle = MovingSolid::kinematic(
            SolidShape::Capsule {
                half_height: 0.05,
                radius: 0.03,
            },
            Vec3::new(0.2, 0.1, 0.05),
        );
        paddle.linear_velocity = Vec3::new(speed, 0.0, 0.0);
        paddle.angular_velocity = Vec3::new(0.0, 0.0, 3.0);
        sim.set_moving_solids(vec![paddle]).expect("paddle");
        let mut digests = Vec::new();
        for _ in 0..25 {
            sim.step();
            digests.push(sim.state_digest());
        }
        (digests, sim.solid_impulses()[0])
    };
    let (a, ia) = run(0.4);
    let (b, ib) = run(0.4);
    assert_eq!(a, b);
    assert_eq!(ia, ib);
    let (c, _) = run(0.3);
    assert_ne!(a.last(), c.last());
}

/// Bad solids are refused with their codes, and clearing the list restores the static field.
#[test]
fn bad_solids_are_refused_and_clearing_restores_the_field() {
    let mut sim = tank(Vec3::new(0.4, 0.3, 0.1), 0.025, 0.1, true);
    let fresh = sim.solid().clone();
    let mut bad = MovingSolid::kinematic(SolidShape::Sphere { radius: 0.05 }, Vec3::splat(0.1));
    bad.linear_velocity = Vec3::new(f32::NAN, 0.0, 0.0);
    let e = sim.set_moving_solids(vec![bad]).expect_err("nan");
    assert_eq!(e.code(), "fluid_bad_solid");
    let many = vec![MovingSolid::kinematic(SolidShape::Sphere { radius: 0.05 }, Vec3::splat(0.1)); 65];
    assert_eq!(
        sim.set_moving_solids(many).expect_err("too many").code(),
        "fluid_too_many_shapes"
    );
    sim.set_moving_solids(vec![MovingSolid::kinematic(
        SolidShape::Sphere { radius: 0.05 },
        Vec3::new(0.2, 0.05, 0.05),
    )])
    .expect("sphere");
    sim.step();
    assert!(
        sim.solid().face_open[1]
            .iter()
            .zip(&fresh.face_open[1])
            .any(|(a, b)| a != b)
    );
    sim.set_moving_solids(Vec::new()).expect("clear");
    sim.step();
    assert_eq!(sim.solid().face_open, fresh.face_open);
    assert_eq!(sim.solid().node_phi, fresh.node_phi);
    assert!(sim.solid_impulses().is_empty());
}

/// A spinning kinematic box integrates its pose with the software sincos and sqrt (ADR-4753), so
/// the digest and the rotation bits are the same on every platform. The pins were captured on
/// the reference host (x86_64, the lane report names it); the same test must be run on Android
/// aarch64 before release.
#[test]
fn rotating_solid_digest_bits_are_stable() {
    let mut sim = tank(Vec3::new(0.6, 0.3, 0.1), 0.025, 0.15, true);
    let mut paddle = MovingSolid::kinematic(
        SolidShape::Box {
            half_extents: Vec3::new(0.03, 0.08, 0.2),
        },
        Vec3::new(0.2, 0.1, 0.05),
    );
    paddle.angular_velocity = Vec3::new(0.0, 1.0, 0.0);
    sim.set_moving_solids(vec![paddle]).expect("paddle");
    let mut digest = 0;
    for _ in 0..8 {
        sim.step();
        digest = sim.state_digest();
    }
    let rot = sim.moving_solids()[0].rotation;
    let bits = rot.to_array().map(f32::to_bits);
    eprintln!("spinning box: digest {digest:#018x}, rotation bits {bits:08x?}");
    assert_eq!(digest, 0x1de2_10d6_11e8_9351, "digest pin");
    assert_eq!(
        bits,
        [0x0000_0000, 0x3d88_6ea7, 0x0000_0000, 0x3f7f_6e6b],
        "rotation bits pin"
    );
}

/// A capsule solid posed by the physics axis-turn quaternion (a bit constant since ADR-4753)
/// hashes the same on every platform: the X-axis turn feeds the SDF, so platform `sin_cos` would
/// fork the run one ulp at a time.
#[test]
fn capsule_axis_turn_digest_bits_are_stable() {
    use regia_physics::ShapeAxis;
    let mut sim = tank(Vec3::new(0.6, 0.3, 0.1), 0.025, 0.15, true);
    let mut capsule = MovingSolid::kinematic(
        SolidShape::Capsule {
            half_height: 0.05,
            radius: 0.03,
        },
        Vec3::new(0.2, 0.1, 0.05),
    );
    // What the physics coupling builds for a capsule collider on ShapeAxis::X.
    capsule.rotation = ShapeAxis::X.from_y_rotation();
    sim.set_moving_solids(vec![capsule]).expect("capsule");
    let mut digest = 0;
    for _ in 0..8 {
        sim.step();
        digest = sim.state_digest();
    }
    let rot = sim.moving_solids()[0].rotation;
    let bits = rot.to_array().map(f32::to_bits);
    eprintln!("x-axis capsule: digest {digest:#018x}, rotation bits {bits:08x?}");
    assert_eq!(digest, 0xc65a_1cc9_993b_e83b, "digest pin");
    // The rotation is exactly the bit constant (+-sin(pi/4)): the platform libm can touch
    // nothing here.
    assert_eq!(
        bits,
        [0x0000_0000, 0x0000_0000, 0xbf35_04f3, 0x3f35_04f3],
        "rotation bits pin"
    );
}

/// (Audit G-fluid #2) A wall thinner than the distance it moves in one step, pushed through liquid
/// at rest (the particle-only CFL would ask for one substep): the substep count includes the
/// solid's speed, so the wall is sampled often enough that no liquid tunnels through it.
#[test]
fn fast_thin_wall_does_not_tunnel() {
    let size = Vec3::new(0.8, 0.4, 0.1);
    let dx = 0.05;
    let mut sim = tank(size, dx, 0.2, false);
    let x0 = 0.2;
    // 6 cm thick, moving 10 cm per step.
    let wall = MovingSolid::kinematic(
        SolidShape::Box {
            half_extents: Vec3::new(0.03, 0.3, 0.2),
        },
        Vec3::new(x0, 0.2, 0.05),
    );
    sim.set_moving_solids(vec![wall]).expect("wall");
    for _ in 0..5 {
        sim.step();
    }
    let left_of = |sim: &FluidSim| {
        let x = sim.moving_solids()[0].position.x;
        sim.positions().iter().filter(|p| p.x < x).count()
    };
    let left0 = left_of(&sim);
    let total = sim.positions().len();
    let mut moving = sim.moving_solids()[0];
    moving.linear_velocity = Vec3::new(6.0, 0.0, 0.0);
    sim.set_moving_solids(vec![moving]).expect("moving wall");
    let mut worst = 0usize;
    for step in 0..3 {
        sim.step();
        let left = left_of(&sim);
        eprintln!(
            "thin wall step {step}: x {:.3}, substeps {}, left {left0} -> {left}",
            sim.moving_solids()[0].position.x,
            sim.stats().substeps
        );
        worst = worst.max(left.abs_diff(left0));
    }
    assert!(
        (worst as f32) <= 0.002 * total as f32,
        "{worst} particles tunnelled through the fast wall (of {total})"
    );
}

/// (Audit G-fluid #11) Replacing the moving solids drops the impulses of the old list: they are
/// aligned with `moving_solids`, so until the next step there are none.
#[test]
fn replacing_solids_clears_impulses() {
    let mut sim = tank(Vec3::new(0.4, 0.3, 0.1), 0.025, 0.1, true);
    let mut a = MovingSolid::kinematic(SolidShape::Sphere { radius: 0.03 }, Vec3::new(0.1, 0.05, 0.05));
    a.tag = 1;
    let mut b = MovingSolid::kinematic(SolidShape::Sphere { radius: 0.03 }, Vec3::new(0.3, 0.05, 0.05));
    b.tag = 2;
    sim.set_moving_solids(vec![a, b]).expect("a and b");
    sim.step();
    assert_eq!(sim.solid_impulses().len(), 2);
    let mut c = MovingSolid::kinematic(SolidShape::Sphere { radius: 0.03 }, Vec3::new(0.2, 0.05, 0.05));
    c.tag = 3;
    sim.set_moving_solids(vec![c]).expect("c");
    assert!(
        sim.solid_impulses().is_empty(),
        "stale impulses {:?}",
        sim.solid_impulses()
    );
    // The digest still covers the new solid while it has no impulse yet.
    let mut moved = c;
    moved.position.x += 0.01;
    let mut other = sim.clone();
    other.set_moving_solids(vec![moved]).expect("moved c");
    assert_ne!(sim.state_digest(), other.state_digest());
    sim.step();
    assert_eq!(sim.solid_impulses().len(), 1);
    assert_eq!(sim.solid_impulses()[0].tag, 3);
}

/// (Audit G-fluid #2, the angular term) A paddle spinning in still liquid: its tips move at
/// `|w| r`, which sets the substep count even though no particle moves yet. With dx 2.5 cm,
/// dt 1/60 s and cfl 1 one substep covers 1.5 m/s; 15 rad/s at the box's bounding radius
/// (about 0.218 m) is about 3.27 m/s, so three substeps.
#[test]
fn spinning_solid_sets_the_substep_count() {
    let mut sim = tank(Vec3::new(0.6, 0.3, 0.1), 0.025, 0.15, false);
    let mut paddle = MovingSolid::kinematic(
        SolidShape::Box {
            half_extents: Vec3::new(0.03, 0.08, 0.2),
        },
        Vec3::new(0.3, 0.1, 0.05),
    );
    paddle.angular_velocity = Vec3::new(0.0, 15.0, 0.0);
    sim.set_moving_solids(vec![paddle]).expect("paddle");
    // The fill happens in this step; its particles start at rest.
    sim.step();
    assert_eq!(sim.stats().substeps, 3);
}

/// Particles far from every collider do not evaluate each collider's analytic distance.
#[test]
fn distant_colliders_are_not_tested_per_particle() {
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::ONE,
        cell_size: 0.25,
        gravity: Vec3::ZERO,
        reseed: false,
        separation: 0.0,
        max_substeps: 1,
        particle_cap: 65_536,
        ..FluidParams::default()
    };
    params.whitewater.enabled = false;
    let colliders: Vec<Collider> = (0..32)
        .map(|i| Collider::Sphere {
            center: Vec3::new(-8.0 - i as f32, -8.0, -8.0),
            radius: 0.25,
        })
        .collect();
    let mut sim = FluidSim::with_colliders(params, colliders).expect("sim");
    let mut pos = Vec::new();
    let mut vel = Vec::new();
    for i in 0..4 {
        pos.push(Vec3::new(0.4, 0.5, 0.3 + 0.1 * i as f32));
        vel.push(Vec3::ZERO);
    }
    sim.set_particles(pos, vel);
    let before = sim.solid().distance_evals();
    sim.step();
    let delta = sim.solid().distance_evals() - before;
    eprintln!("distant collider evals during one step: {delta} (particles 4, colliders 32)");
    assert!(
        delta < 4,
        "evaluated {delta} analytic distances for 4 particles far from 32 spheres"
    );
}
