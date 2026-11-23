// SPDX-License-Identifier: MPL-2.0
//! Domain-origin differential regression for the CPU reference solver (ADR-2441).
//! Fixed inputs and steps; coordinates are translated by exact cell-size multiples.

use glam::Vec3;
use regia_fluid::sim::{Emitter, EmitterKind, FluidParams, FluidSim, Shape};
use regia_fluid::solid::{Collider, MovingSolid, SolidShape};

// A power-of-two cell size makes each shifted coordinate exactly representable.
const DX: f32 = 0.0625;
const SHIFT: Vec3 = Vec3::new(-8.0 * DX, 12.0 * DX, -4.0 * DX);

fn translated(point: Vec3, shift: Vec3) -> Vec3 {
    point + shift
}

fn shape_box(center: Vec3, half_extents: Vec3, shift: Vec3) -> Shape {
    Shape::Box {
        center: translated(center, shift),
        half_extents,
    }
}

fn params(shift: Vec3) -> FluidParams {
    FluidParams {
        bounds_min: shift,
        bounds_max: shift + Vec3::new(0.5, 0.5, 0.25),
        cell_size: DX,
        dt: 1.0 / 60.0,
        seed: 0x5eed,
        particle_cap: 2_000,
        ..FluidParams::default()
    }
}

fn compare_each_step(mut a: FluidSim, mut b: FluidSim, steps: usize, shift: Vec3) {
    assert_eq!(a.dims().nx, b.dims().nx);
    assert_eq!(a.dims().ny, b.dims().ny);
    assert_eq!(a.dims().nz, b.dims().nz);
    assert_eq!(a.dims().dx, b.dims().dx);
    let position_tolerance = DX * 0.01;
    let velocity_tolerance = DX / a.params().dt * 0.002;
    let pressure_tolerance = f64::from(a.params().density * 9.81 * DX * 0.001);
    let volume_tolerance = DX.powi(3) * 0.02;

    for step in 0..steps {
        a.step();
        b.step();
        assert_eq!(
            a.positions().len(),
            b.positions().len(),
            "particle count at {step}"
        );
        assert_eq!(
            a.velocities().len(),
            b.velocities().len(),
            "velocity count at {step}"
        );
        for (index, (pa, pb)) in a.positions().iter().zip(b.positions()).enumerate() {
            assert!(
                (*pa - (*pb - shift)).abs().max_element() <= position_tolerance,
                "position {index} at step {step}: {pa:?} vs {:?}",
                *pb - shift
            );
            assert_eq!(
                a.dims().cell_of(*pa),
                b.dims().cell_of(*pb),
                "liquid cell for particle {index} at step {step}"
            );
        }
        for (index, (va, vb)) in a.velocities().iter().zip(b.velocities()).enumerate() {
            assert!(
                (*va - *vb).abs().max_element() <= velocity_tolerance,
                "velocity {index} at step {step}: {va:?} vs {vb:?}"
            );
        }
        for k in 0..a.dims().nz {
            for j in 0..a.dims().ny {
                for i in 0..a.dims().nx {
                    let (pa, pb) = (a.pressure(i, j, k), b.pressure(i, j, k));
                    assert!(
                        (pa - pb).abs() <= pressure_tolerance,
                        "pressure ({i},{j},{k}) at step {step}: {pa} vs {pb}"
                    );
                    assert_eq!(
                        a.is_liquid(i, j, k),
                        b.is_liquid(i, j, k),
                        "projected liquid mask ({i},{j},{k}) at step {step}"
                    );
                }
            }
        }
        assert!(
            (a.liquid_volume() - b.liquid_volume()).abs() <= volume_tolerance,
            "volume at step {step}: {} vs {}",
            a.liquid_volume(),
            b.liquid_volume()
        );
        assert_eq!(a.solid_impulses().len(), b.solid_impulses().len());
        for (index, (ia, ib)) in a.solid_impulses().iter().zip(b.solid_impulses()).enumerate() {
            assert!(
                (ia.linear - ib.linear).abs().max_element() <= velocity_tolerance * 10.0,
                "solid linear impulse {index} at step {step}: {:?} vs {:?}",
                ia.linear,
                ib.linear
            );
            assert!(
                (ia.angular - ib.angular).abs().max_element() <= velocity_tolerance * 10.0,
                "solid angular impulse {index} at step {step}: {:?} vs {:?}",
                ia.angular,
                ib.angular
            );
        }
        assert_eq!(a.moving_solids().len(), b.moving_solids().len());
        for (index, (sa, sb)) in a.moving_solids().iter().zip(b.moving_solids()).enumerate() {
            assert!(
                (sa.position - (sb.position - shift)).abs().max_element() <= position_tolerance,
                "moving-solid position {index} at step {step}: {:?} vs {:?}",
                sa.position,
                sb.position - shift
            );
        }
    }
}

/// A passive fixed-seed cloud retains translation-equivalent particle and pressure state.
#[test]
fn passive_cloud_is_invariant_to_domain_origin() {
    let mut a = FluidSim::new(params(Vec3::ZERO)).expect("origin simulation");
    let mut b = FluidSim::new(params(SHIFT)).expect("translated simulation");
    let positions: Vec<Vec3> = (0..4)
        .flat_map(|i| {
            (0..4).flat_map(move |j| {
                (0..2).map(move |k| {
                    Vec3::new(
                        0.075 + i as f32 * DX,
                        0.075 + (j / 2) as f32 * DX + (j % 2) as f32 * DX * 0.4,
                        0.075 + k as f32 * DX,
                    )
                })
            })
        })
        .collect();
    let velocities: Vec<Vec3> = positions
        .iter()
        .enumerate()
        .map(|(index, _)| Vec3::new(0.15 + index as f32 * 0.001, 0.0, -0.1))
        .collect();
    a.set_particles(positions.clone(), velocities.clone());
    b.set_particles(positions.iter().map(|p| *p + SHIFT).collect(), velocities);
    compare_each_step(a, b, 18, SHIFT);
}

/// A sloshing fill around a translated static box and moving box agrees, including impulses.
#[test]
fn bounded_slosh_around_box_is_invariant_to_domain_origin() {
    let make = |shift: Vec3| {
        let mut p = params(shift);
        p.external_acceleration = Vec3::new(2.0, 0.0, 0.0);
        let collider = Collider::Box {
            center: translated(Vec3::new(0.1875, 0.125, 0.125), shift),
            half_extents: Vec3::splat(DX),
        };
        let mut sim = FluidSim::with_colliders(p, vec![collider]).expect("box simulation");
        sim.add_emitter(Emitter {
            shape: shape_box(Vec3::new(0.1875, 0.125, 0.125), Vec3::new(0.125, DX, DX), shift),
            velocity: Vec3::ZERO,
            kind: EmitterKind::Fill,
            done: false,
        })
        .expect("fill");
        let mut moving = MovingSolid::kinematic(
            SolidShape::Box {
                half_extents: Vec3::new(0.025, 0.025, 0.025),
            },
            translated(Vec3::new(0.125, 0.125, 0.125), shift),
        );
        moving.linear_velocity = Vec3::new(0.1, 0.0, 0.0);
        sim.set_moving_solids(vec![moving]).expect("moving box");
        sim
    };
    compare_each_step(make(Vec3::ZERO), make(SHIFT), 14, SHIFT);
}

/// A continuous source and sink retain their translated shape interactions over fixed steps.
#[test]
fn emitter_and_sink_are_invariant_to_domain_origin() {
    let make = |shift| {
        let mut sim = FluidSim::new(params(shift)).expect("source simulation");
        sim.add_emitter(Emitter {
            shape: shape_box(Vec3::new(0.1, 0.2, 0.1), Vec3::new(0.075, 0.15, 0.075), shift),
            velocity: Vec3::new(0.0, 0.1, 0.0),
            kind: EmitterKind::Source,
            done: false,
        })
        .expect("source");
        sim.add_sink(shape_box(
            Vec3::new(0.3, 0.1, 0.1),
            Vec3::new(0.05, 0.075, 0.075),
            shift,
        ))
        .expect("sink");
        sim
    };
    compare_each_step(make(Vec3::ZERO), make(SHIFT), 20, SHIFT);
}
