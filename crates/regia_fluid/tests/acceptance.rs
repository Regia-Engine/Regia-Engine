// SPDX-License-Identifier: MPL-2.0
//! Acceptance tests of the CPU reference solver (ADR-2441): fixed steps, deterministic inputs, no
//! goldens and no wall clock.

use glam::Vec3;
use regia_fluid::presets;
use regia_fluid::sim::{Emitter, EmitterKind, FluidError, FluidParams, FluidSim, Shape};
use regia_fluid::solid::{Collider, MovingSolid, SolidShape};
use regia_fluid::{FluidCache, PointKind};

/// A column of water at rest: 4 x 44 x 4 cells of 2.5 cm, filled 1 m deep.
fn column(steps_whitewater: bool) -> FluidSim {
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::new(0.1, 1.1, 0.1),
        cell_size: 0.025,
        ..FluidParams::default()
    };
    params.whitewater.enabled = steps_whitewater;
    let mut sim = FluidSim::new(params).expect("column");
    sim.add_emitter(Emitter {
        shape: Shape::Box {
            center: Vec3::new(0.05, 0.5, 0.05),
            half_extents: Vec3::new(0.05, 0.5, 0.05),
        },
        velocity: Vec3::ZERO,
        kind: EmitterKind::Fill,
        done: false,
    })
    .expect("emitter");
    sim
}

fn dam_break(params: impl FnOnce(&mut FluidParams)) -> FluidSim {
    let (lo, hi, dx) = presets::default_bounds("dam_break").expect("bounds");
    let mut p = FluidParams {
        bounds_min: lo,
        bounds_max: hi,
        cell_size: dx,
        ..FluidParams::default()
    };
    params(&mut p);
    presets::build("dam_break", p).expect("dam break")
}

fn range(values: &[f32]) -> f32 {
    let lo = values.iter().copied().fold(f32::MAX, f32::min);
    let hi = values.iter().copied().fold(f32::MIN, f32::max);
    hi - lo
}

/// A column at rest keeps its volume within 2 % over 600 steps and its surface flat within a cell.
#[test]
fn column_at_rest_keeps_volume_and_a_flat_surface() {
    let mut sim = column(false);
    sim.step();
    let v0 = sim.liquid_volume();
    let dx = sim.dims().dx;
    let mut worst_drift = 0.0f32;
    let mut worst_flatness = 0.0f32;
    for step in 1..600 {
        sim.step();
        if step % 50 == 0 || step == 599 {
            let v = sim.liquid_volume();
            worst_drift = worst_drift.max(((v - v0) / v0).abs());
            worst_flatness = worst_flatness.max(range(&sim.column_heights()));
        }
    }
    let stats = sim.stats();
    eprintln!(
        "column: v0 {v0}, v {}, worst drift {worst_drift}, worst flatness {worst_flatness} (dx {dx}), \
         particles {}, pcg {} it, divergence {}",
        stats.volume, stats.particles, stats.pcg_iterations, stats.divergence_max
    );
    assert!(worst_drift < 0.02, "volume drift {worst_drift}");
    assert!(worst_flatness <= dx, "surface range {worst_flatness} > dx {dx}");
    assert!(stats.pcg_converged, "{stats:?}");
}

/// Hydrostatic pressure: p = rho g h within 5 % in the column (h below the fill height).
#[test]
fn hydrostatic_pressure_is_rho_g_h() {
    let mut sim = column(false);
    for _ in 0..30 {
        sim.step();
    }
    let heights = sim.column_heights();
    let surface = heights.iter().sum::<f32>() / heights.len() as f32;
    let rho_g = f64::from(sim.params().density) * 9.81;
    let dx = f64::from(sim.dims().dx);
    for j in [0usize, 10, 20] {
        let depth = f64::from(surface) - (j as f64 + 0.5) * dx;
        let expected = rho_g * depth;
        let p = sim.pressure(1, j, 1);
        let error = (p - expected).abs() / expected;
        eprintln!("hydrostatic j {j}: p {p:.1} Pa, rho g h {expected:.1} Pa, error {error:.4}");
        assert!(error < 0.05, "cell {j}: {p} vs {expected}");
    }
}

/// Dam break: once the start-up transient is over, the front runs at the shallow-water (Ritter)
/// dry-bed front speed `2 sqrt(g h0)` within 15 %. A 0.4 m wide, 0.2 m deep column and a 3.2 m
/// runout give the front room to settle (it reaches the Ritter speed about 0.35 s after release).
#[test]
fn dam_break_front_matches_shallow_water_speed() {
    let params = FluidParams {
        bounds_max: Vec3::new(3.2, 0.3, 0.1),
        cell_size: 0.025,
        ..FluidParams::default()
    };
    let mut sim = FluidSim::new(params).expect("dam");
    sim.add_emitter(Emitter {
        shape: Shape::Box {
            center: Vec3::new(0.2, 0.1, 0.05),
            half_extents: Vec3::new(0.2, 0.1, 0.05),
        },
        velocity: Vec3::ZERO,
        kind: EmitterKind::Fill,
        done: false,
    })
    .expect("fill");
    sim.step();
    let h0 = 0.2f32;
    let ritter = 2.0 * (9.81 * h0).sqrt();
    let dt = sim.params().dt;
    let front = |sim: &FluidSim| sim.positions().iter().fold(0.0f32, |m, p| m.max(p.x));
    let mut fronts = vec![(0usize, front(&sim))];
    for step in 1..=48 {
        sim.step();
        fronts.push((step, front(&sim)));
    }
    // Front speed between steps 24 and 48 (0.4 s to 0.8 s after release).
    let (a, b) = (fronts[24], fronts[48]);
    let speed = (b.1 - a.1) / ((b.0 - a.0) as f32 * dt);
    let error = (speed - ritter).abs() / ritter;
    eprintln!("dam break: front speed {speed} m/s, ritter {ritter} m/s, error {error}; fronts {fronts:?}");
    assert!(error < 0.15, "front speed {speed} vs {ritter}");
    assert!(
        b.1 < 3.1,
        "the front reached the far wall: the window is too long"
    );
}

/// A closed tank shaken sideways then left to slosh loses under 3 % of its volume.
#[test]
fn closed_box_slosh_volume_drift_is_small() {
    let params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::new(0.6, 0.4, 0.1),
        cell_size: 0.025,
        external_acceleration: Vec3::new(4.0, 0.0, 0.0),
        ..FluidParams::default()
    };
    let mut sim = FluidSim::new(params).expect("tank");
    sim.add_emitter(Emitter {
        shape: Shape::Box {
            center: Vec3::new(0.3, 0.075, 0.05),
            half_extents: Vec3::new(0.3, 0.075, 0.05),
        },
        velocity: Vec3::ZERO,
        kind: EmitterKind::Fill,
        done: false,
    })
    .expect("fill");
    sim.step();
    let v0 = sim.liquid_volume();
    for _ in 0..40 {
        sim.step();
    }
    // Stop shaking: a fresh simulation with the same particles and no external acceleration.
    let mut calm = FluidSim::new(FluidParams {
        external_acceleration: Vec3::ZERO,
        ..sim.params().clone()
    })
    .expect("calm");
    calm.set_particles(sim.positions().to_vec(), sim.velocities().to_vec());
    let mut worst = 0.0f32;
    for step in 0..300 {
        calm.step();
        if step % 25 == 0 || step == 299 {
            worst = worst.max(((calm.liquid_volume() - v0) / v0).abs());
        }
    }
    eprintln!("slosh: v0 {v0}, worst drift {worst}");
    assert!(worst < 0.03, "volume drift {worst}");
}

/// The same inputs twice give the same digest every step; another seed gives another.
#[test]
fn same_seed_gives_equal_digests() {
    let mut a = dam_break(|_| {});
    let mut b = dam_break(|_| {});
    for _ in 0..40 {
        a.step();
        b.step();
        assert_eq!(a.state_digest(), b.state_digest());
    }
    let mut c = dam_break(|p| p.seed = 7);
    for _ in 0..40 {
        c.step();
    }
    assert_ne!(a.state_digest(), c.state_digest());
}

/// Whitewater: none from a calm pool, some from a dam break, and foam never above its cap.
#[test]
fn whitewater_generation_and_caps() {
    let mut pool = column(true);
    let mut calm_total = 0;
    for _ in 0..120 {
        pool.step();
        calm_total += pool.stats().whitewater_generated;
    }
    assert_eq!(calm_total, 0, "a calm pool made whitewater");
    assert_eq!(pool.whitewater().particles().len(), 0);

    let mut dam = dam_break(|p| p.whitewater.max_foam = 40);
    let mut generated = 0;
    for _ in 0..60 {
        dam.step();
        let stats = dam.stats();
        generated += stats.whitewater_generated;
        assert!(stats.foam <= 40, "foam {} over its cap", stats.foam);
    }
    eprintln!(
        "dam break whitewater: generated {generated}, now {:?}",
        dam.whitewater().counts()
    );
    assert!(generated > 0, "a dam break made no whitewater");
    // The frame carries the type tags and ages.
    let frame = dam.frame();
    assert_eq!(frame.count(PointKind::Liquid), dam.positions().len());
    assert_eq!(
        frame.len(),
        dam.positions().len() + dam.whitewater().particles().len()
    );
}

/// A recorded run replays from the cache without the solver, equal to the live run (quantized).
#[test]
fn cache_round_trip_equals_the_live_run() {
    let mut sim = dam_break(|_| {});
    let mut cache = FluidCache::for_sim(&sim, true);
    let mut expected = Vec::new();
    for _ in 0..45 {
        sim.step();
        cache.record(&sim).expect("record");
        expected.push(cache.quantized_frame(&sim));
    }
    let bytes = cache.to_bytes();
    let back = FluidCache::from_bytes(&bytes).expect("read back");
    assert_eq!(back, cache);
    assert_eq!(back.to_bytes(), bytes);
    assert_eq!(regia_fluid::cache::cook(&bytes).expect("cook"), bytes);
    for (i, want) in expected.iter().enumerate() {
        let got = back.play(i).expect("play");
        assert_eq!(&got, want, "frame {i}");
    }
    // Within one quantum of the live positions.
    let live = sim.frame();
    let last = back.play(44).expect("last");
    for (a, b) in live.positions.iter().zip(&last.positions) {
        for k in 0..3 {
            assert!((a[k] - b[k]).abs() <= cache.quantum, "{a:?} vs {b:?}");
        }
    }
    eprintln!("cache: {} frames, {} bytes", back.frame_count(), bytes.len());
}

/// Truncated or hostile caches are refused without a panic or a big allocation.
#[test]
fn hostile_caches_are_rejected() {
    let mut sim = dam_break(|_| {});
    let mut cache = FluidCache::for_sim(&sim, true);
    for _ in 0..3 {
        sim.step();
        cache.record(&sim).expect("record");
    }
    let bytes = cache.to_bytes();
    for len in 0..bytes.len() {
        assert!(
            FluidCache::from_bytes(&bytes[..len]).is_err(),
            "prefix {len} loaded"
        );
    }
    // A header claiming the maximum frames with no bytes behind it.
    let mut huge = bytes[..52].to_vec();
    huge[16..20].copy_from_slice(&regia_fluid::cache::MAX_FRAMES.to_le_bytes());
    assert_eq!(
        FluidCache::from_bytes(&huge).map_err(|e| e.code()),
        Err("fluidcache_truncated")
    );
    huge[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        FluidCache::from_bytes(&huge).map_err(|e| e.code()),
        Err("fluidcache_too_large")
    );
    // A frame claiming four million points in a few bytes.
    let mut one = bytes[..52].to_vec();
    one[16..20].copy_from_slice(&1u32.to_le_bytes());
    let mut frame = vec![0u8];
    frame.extend_from_slice(&0u64.to_le_bytes());
    frame.extend_from_slice(&4_000_000u32.to_le_bytes());
    frame.extend_from_slice(&[0, 0, 0]);
    one.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    one.extend_from_slice(&frame);
    assert_eq!(
        FluidCache::from_bytes(&one).map_err(|e| e.code()),
        Err("fluidcache_truncated")
    );
    // Wrong magic and version.
    let mut magic = bytes.clone();
    magic[0] = b'X';
    assert_eq!(
        FluidCache::from_bytes(&magic).map_err(|e| e.code()),
        Err("fluidcache_bad_magic")
    );
    let mut version = bytes.clone();
    version[4] = 9;
    assert_eq!(
        FluidCache::from_bytes(&version).map_err(|e| e.code()),
        Err("fluidcache_bad_version")
    );
    assert_eq!(regia_fluid::cache::fluidcache_file_version(&bytes), Ok(Some(1)));
    assert_eq!(
        regia_fluid::cache::fluidcache_file_version(b"not a cache"),
        Ok(None)
    );
}

/// The surface mesh of the released block encloses about the block's volume.
#[test]
fn surface_mesh_encloses_the_liquid() {
    let mut sim = dam_break(|_| {});
    sim.step();
    let mesh = sim.surface_mesh(2);
    let v = mesh.enclosed_volume();
    let block = 0.4 * 0.4 * 0.1;
    eprintln!(
        "surface: {} triangles, volume {v} vs block {block}",
        mesh.triangle_count()
    );
    assert!(mesh.triangle_count() > 0);
    assert!((v - block).abs() / block < 0.2, "{v} vs {block}");
}

/// Every preset builds and steps; the particle cap holds; the stream's sink and terrain work.
#[test]
fn presets_build_step_and_respect_caps() {
    for name in presets::PRESETS {
        let (lo, hi, dx) = presets::default_bounds(name).expect("bounds");
        let params = FluidParams {
            bounds_min: lo,
            bounds_max: hi,
            cell_size: dx,
            particle_cap: 3000,
            ..FluidParams::default()
        };
        let mut sim = presets::build(name, params).expect(name);
        for _ in 0..20 {
            sim.step();
            assert!(sim.positions().len() <= 3000, "{name}: {}", sim.positions().len());
        }
        assert!(!sim.positions().is_empty(), "{name} has no liquid");
        if *name == "stream" {
            let dx = sim.dims().dx;
            for p in sim.positions() {
                assert!(
                    sim.solid().distance(*p) > -0.5 * dx,
                    "{name}: {p:?} inside the terrain"
                );
            }
        }
    }
    assert!(matches!(
        presets::build("nope", FluidParams::default()),
        Err(FluidError::BadParam(_))
    ));
}

/// Oversized grids and bad settings are refused before anything is allocated.
#[test]
fn bad_params_are_refused() {
    let huge = FluidParams {
        bounds_max: Vec3::splat(1.0e6),
        cell_size: 1.0e-3,
        ..FluidParams::default()
    };
    assert_eq!(
        FluidSim::new(huge).map(|_| ()).map_err(|e| e.code()),
        Err("fluid_too_many_cells")
    );
    let nan = FluidParams {
        bounds_max: Vec3::splat(f32::NAN),
        ..FluidParams::default()
    };
    assert_eq!(
        FluidSim::new(nan).map(|_| ()).map_err(|e| e.code()),
        Err("fluid_bad_bounds")
    );
    let flip = FluidParams {
        flip_ratio: 2.0,
        ..FluidParams::default()
    };
    assert_eq!(
        FluidSim::new(flip).map(|_| ()).map_err(|e| e.code()),
        Err("fluid_bad_param")
    );
}

/// A sphere collider holds the liquid off it.
#[test]
fn sphere_collider_holds_liquid() {
    let params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::new(0.5, 0.6, 0.5),
        cell_size: 0.05,
        ..FluidParams::default()
    };
    let ball = Collider::Sphere {
        center: Vec3::new(0.25, 0.15, 0.25),
        radius: 0.12,
    };
    let mut sim = FluidSim::with_colliders(params, vec![ball]).expect("sim");
    sim.add_emitter(Emitter {
        shape: Shape::Box {
            center: Vec3::new(0.25, 0.45, 0.25),
            half_extents: Vec3::new(0.15, 0.08, 0.15),
        },
        velocity: Vec3::ZERO,
        kind: EmitterKind::Fill,
        done: false,
    })
    .expect("fill");
    for _ in 0..60 {
        sim.step();
    }
    let dx = sim.dims().dx;
    let inside = sim
        .positions()
        .iter()
        .filter(|p| sim.solid().distance(**p) < -0.25 * dx)
        .count();
    assert_eq!(inside, 0, "{inside} particles inside the sphere");
}

/// The CFL substep count is decided on squared f64 compares, not `f32::sqrt` (ADR-4753): a
/// particle one ulp of speed around the `n` substeps boundary must classify the same on every
/// platform. With dx 2.5 cm, dt 1/60 s and cfl 1, one substep covers 1.5 m/s, so the n = 2
/// boundary sits at exactly 3 m/s. The upper scene pins its substep count and digest.
#[test]
fn cfl_boundary_substep_count_is_stable() {
    // Half-metre cube: 20^3 = 8000 cells, under the Android cell cap so the test also runs on
    // aarch64 devices.
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::splat(0.5),
        cell_size: 0.025,
        gravity: Vec3::ZERO,
        max_substeps: 4,
        reseed: false,
        separation: 0.0,
        // The mobile cap: keeps the test runnable on Android aarch64.
        particle_cap: 65_536,
        ..FluidParams::default()
    };
    params.whitewater.enabled = false;
    let mut sim = FluidSim::new(params.clone()).expect("sim");
    // Speed squared a hair above 9: strictly more than two substeps' coverage.
    sim.add_particles(&[(Vec3::new(0.25, 0.25, 0.25), Vec3::new(3.0, 3.0e-3, 0.0))]);
    sim.step();
    let stats = sim.stats();
    eprintln!(
        "cfl boundary: substeps {} digest {:#018x}",
        stats.substeps,
        sim.state_digest()
    );
    assert_eq!(stats.substeps, 3, "speed just over 3 m/s needs three substeps");
    assert_eq!(sim.state_digest(), 0x08c9_0391_4fe1_eb24, "digest pin");

    // Exactly 3 m/s also takes three: `dx` and `dt` are f32, so the f64 boundary
    // `(2 * cfl * dx / dt)^2` lands at 8.9999993 — a hair *below* 9 — and 3.0 m/s exceeds it.
    // One ulp of speed below 3 m/s stays at two.
    for (v, want) in [
        (Vec3::new(3.0, 0.0, 0.0), 3),
        (Vec3::new(3.0f32.next_down(), 0.0, 0.0), 2),
    ] {
        let mut sim = FluidSim::new(params.clone()).expect("sim");
        sim.add_particles(&[(Vec3::new(0.25, 0.25, 0.25), v)]);
        sim.step();
        assert_eq!(sim.stats().substeps, want, "{v:?}");
    }
}

/// The wave-crest gate decides whether foam spawns (and how many generator draws the step
/// consumes), so it compares on the squares in f64, never `sqrt` (ADR-4753). The gate's boundary
/// classification is pinned in `whitewater.rs` unit tests; this pins a foam-producing run end
/// to end.
#[test]
fn whitewater_crest_gate_is_stable() {
    let mut dam = dam_break(|p| {
        p.whitewater.max_foam = 40;
        // The mobile cap: keeps the test runnable on Android aarch64.
        p.particle_cap = 65_536;
    });
    let mut foams = Vec::new();
    for _ in 0..30 {
        dam.step();
        foams.push(dam.stats().foam);
    }
    eprintln!(
        "dam break foam over 30 steps: {foams:?}, digest {:#018x}",
        dam.state_digest()
    );
    // The whole 30-step foam trajectory is pinned (reference host: the build server x86_64, lavapipe
    // class CPU; the WGSL-free CPU path is device-independent).
    const FOAM_PIN: [usize; 30] = [
        0, 0, 0, 0, 1, 0, 3, 1, 2, 5, 5, 3, 15, 24, 35, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40,
        40, 40,
    ];
    assert_eq!(
        foams,
        FOAM_PIN.to_vec(),
        "foam trajectory pin (any platform ulp of sqrt forks it)"
    );
    // Two intended changes move this digest, and nothing else does (integ-v048 merge audit,
    // ADR-5795; recorded on the test machine, x86_64). fixrev-09's software sqrt on the solver paths
    // and its digest form (ADR-5495) took the uncapped solver from 0x1e6f_dd38_c15d_1fb4 to
    // 0x3f9b_1eb7_7937_1e34, integ-v048b's pin. The pair cap of 12 (ADR-5313), which alone had
    // moved the old solver to 0x1a4a_d8b6_5646_d251 (v047's pin), gives the value below. The
    // same 30 steps with `SEPARATE_CELL_CAP` lifted to 1_000_000 hash to 0x3f9b_1eb7_7937_1e34
    // exactly, and the foam trajectory above does not change.
    assert_eq!(dam.state_digest(), 0x6ef8_50a0_1b8b_8671, "digest pin");
    assert!(foams.iter().any(|&f| f > 0), "the dam break made no foam");
}

/// Equal digests must mean the next step reads the same inputs (ADR-4753): a pending fill
/// emitter, a different gravity, a different sink or collider, or an advanced generator all
/// change the next step, so they must change the digest even when the particles match.
#[test]
fn digest_differs_when_the_next_step_would() {
    // A pending fill emitter against an empty sim: equal particles (none), different next step.
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::splat(0.2),
        cell_size: 0.025,
        // The mobile cap: keeps the test runnable on Android aarch64.
        particle_cap: 65_536,
        ..FluidParams::default()
    };
    params.whitewater.enabled = false;
    let mut empty = FluidSim::new(params.clone()).expect("empty");
    let mut filling = FluidSim::new(params.clone()).expect("filling");
    filling
        .add_emitter(Emitter {
            shape: Shape::Box {
                center: Vec3::splat(0.1),
                half_extents: Vec3::splat(0.05),
            },
            velocity: Vec3::ZERO,
            kind: EmitterKind::Fill,
            done: false,
        })
        .expect("fill");
    assert_ne!(empty.state_digest(), filling.state_digest());
    // The fill runs: from here on only `done` differs, and that must show too.
    empty.step();
    filling.step();
    assert_ne!(empty.state_digest(), filling.state_digest());

    // Same particles, different gravity.
    let mut weightless_params = params.clone();
    weightless_params.gravity = Vec3::ZERO;
    let mut heavy = FluidSim::new(params.clone()).expect("heavy");
    let mut weightless = FluidSim::new(weightless_params).expect("weightless");
    for sim in [&mut heavy, &mut weightless] {
        sim.add_particles(&[(Vec3::new(0.1, 0.1, 0.1), Vec3::ZERO)]);
    }
    assert_ne!(heavy.state_digest(), weightless.state_digest());

    // A sink and a collider change the step without touching the particles.
    let mut with_sink = FluidSim::new(params.clone()).expect("sink");
    with_sink
        .add_sink(Shape::Sphere {
            center: Vec3::splat(0.19),
            radius: 0.01,
        })
        .expect("sink");
    assert_ne!(heavy.state_digest(), with_sink.state_digest());
    let walled = FluidSim::with_colliders(
        params.clone(),
        vec![Collider::Box {
            center: Vec3::splat(0.19),
            half_extents: Vec3::splat(0.01),
        }],
    )
    .expect("walled");
    assert_ne!(heavy.state_digest(), walled.state_digest());

    // Moving solids before any step: `impulses` is still empty, so a digest that only zipped
    // the two lists would hash nothing but the solid count. A box solid at A against a sphere
    // at B, and a box against a capsule at the same pose, must both show.
    let mut box_at_a = FluidSim::new(params.clone()).expect("box");
    box_at_a
        .set_moving_solids(vec![MovingSolid::kinematic(
            SolidShape::Box {
                half_extents: Vec3::new(0.02, 0.02, 0.02),
            },
            Vec3::new(0.05, 0.05, 0.05),
        )])
        .expect("box solid");
    let mut sphere_at_b = FluidSim::new(params.clone()).expect("sphere");
    sphere_at_b
        .set_moving_solids(vec![MovingSolid::kinematic(
            SolidShape::Sphere { radius: 0.02 },
            Vec3::new(0.15, 0.15, 0.15),
        )])
        .expect("sphere solid");
    assert_ne!(box_at_a.state_digest(), sphere_at_b.state_digest());
    let mut capsule_same_pose = FluidSim::new(params.clone()).expect("capsule");
    capsule_same_pose
        .set_moving_solids(vec![MovingSolid::kinematic(
            SolidShape::Capsule {
                half_height: 0.02,
                radius: 0.02,
            },
            Vec3::new(0.05, 0.05, 0.05),
        )])
        .expect("capsule solid");
    assert_ne!(
        box_at_a.state_digest(),
        capsule_same_pose.state_digest(),
        "a shape-only difference at the same pose"
    );
    // A different half extent is a different solid too.
    let mut wider_box = FluidSim::new(params.clone()).expect("wider box");
    wider_box
        .set_moving_solids(vec![MovingSolid::kinematic(
            SolidShape::Box {
                half_extents: Vec3::new(0.03, 0.02, 0.02),
            },
            Vec3::new(0.05, 0.05, 0.05),
        )])
        .expect("wider box");
    assert_ne!(box_at_a.state_digest(), wider_box.state_digest());
}

/// (Audit G-fluid #6) A collider that pokes through a domain wall: the domain clamp after the push
/// out of the solid must not leave a particle inside it. The push slides along the wall instead.
#[test]
fn solid_through_domain_wall_ejects_particles() {
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::splat(0.4),
        cell_size: 0.1,
        gravity: Vec3::ZERO,
        reseed: false,
        separation: 0.0,
        ..FluidParams::default()
    };
    params.whitewater.enabled = false;
    // The sphere's centre is 2 cm inside the x = 0 wall: its nearest surface for points near the
    // wall is the cap outside the domain.
    let ball = Collider::Sphere {
        center: Vec3::new(0.02, 0.2, 0.2),
        radius: 0.05,
    };
    let mut sim = FluidSim::with_colliders(params, vec![ball.clone()]).expect("sim");
    let seeds: Vec<(Vec3, Vec3)> = [
        (0.01, 0.0, 0.0),
        (0.01, 0.01, 0.0),
        (0.005, -0.012, 0.007),
        (0.015, 0.004, -0.015),
        (0.002, 0.02, 0.02),
    ]
    .iter()
    .map(|&(x, dy, dz)| (Vec3::new(x, 0.2 + dy, 0.2 + dz), Vec3::ZERO))
    .collect();
    assert_eq!(sim.add_particles(&seeds), seeds.len());
    for step in 0..5 {
        sim.step();
        for p in sim.positions() {
            let phi = ball.distance(*p);
            assert!(
                phi >= 0.0,
                "step {step}: particle {p} still inside the sphere (phi {phi})"
            );
        }
    }
}

/// Eight particles in one cell keep today's pairs. Two hundred in that cell only pair the first
/// twelve (the reseed thin line): `12 * 11 / 2 = 66`, not `200 * 199 / 2`.
#[test]
fn separate_caps_pairs_in_an_overfilled_cell() {
    fn packed(n: usize) -> FluidSim {
        let mut params = FluidParams {
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::splat(0.1),
            cell_size: 0.1,
            gravity: Vec3::ZERO,
            reseed: false,
            separation: 0.5,
            max_substeps: 1,
            particle_cap: 65_536,
            ..FluidParams::default()
        };
        params.whitewater.enabled = false;
        let mut sim = FluidSim::new(params).expect("sim");
        let mut pos = Vec::with_capacity(n);
        let vel = vec![Vec3::ZERO; n];
        for i in 0..n {
            let t = (i as f32 + 0.5) / n as f32;
            pos.push(Vec3::new(0.02 + 0.06 * t, 0.05, 0.05));
        }
        sim.set_particles(pos, vel);
        sim
    }
    let mut eight = packed(8);
    eight.step();
    let pairs8 = eight.pair_visits();
    let digest8 = eight.state_digest();
    let mut crowd = packed(200);
    crowd.step();
    let pairs200 = crowd.pair_visits();
    let digest200 = crowd.state_digest();
    eprintln!(
        "eight-per-cell pairs {pairs8} digest {digest8:#018x}; overfilled pairs {pairs200} digest {digest200:#018x}"
    );
    assert_eq!(pairs8, 28, "eight particles are one cell of pairs");
    assert_eq!(
        pairs200, 66,
        "overfilled cell considered {pairs200} pairs (digest {digest200:#018x})"
    );
}
