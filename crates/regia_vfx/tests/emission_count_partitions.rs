// SPDX-License-Identifier: MPL-2.0
//! CPU scheduling counts, independently checked from episode totals, not spawn timestamps.
//!
//! Exact binary inputs keep rounding drift out of this bounded contract: fixed LOD/intensity,
//! no deaths, a drain every step, and nonbinding budgets except in the named budget test.
//! Window tests use explicit end-of-step eligibility tables; they do not assume continuous
//! integration through a delay or a duration boundary. There is no loop field in format v6.

use regia_core::math::Vec3;
use regia_vfx::format::{
    BlendMode, EmitterDef, InitModule, LodDef, RangeDef, RenderModule, ShapeDef, SpawnMode, SpawnModule,
    SystemModule, VFX_FORMAT_VERSION, VfxSystemDef,
};
use regia_vfx::sim::{LodInput, SpawnJob, VfxInstance};

const LOD: LodInput = LodInput {
    camera_pos: Vec3::ZERO,
};
const START: Vec3 = Vec3::new(2.0, 4.0, 8.0);

fn definition(mode: SpawnMode, lod_scale: f32, count_scale: f32) -> VfxSystemDef {
    let def = VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "emission_count_partitions".into(),
        system: SystemModule {
            bounds: None,
            budget: 4096,
            // Every tested origin stays well inside this band; no camera/LOD transition.
            lod: Some(LodDef {
                distances: vec![1024.0],
                rate_scale: vec![lod_scale, 0.0],
            }),
        },
        emitters: vec![EmitterDef {
            name: "source".into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode,
                lifetime: RangeDef { min: 32.0, max: 64.0 },
                count_scale,
            },
            init: InitModule {
                shape: ShapeDef::Box {
                    half_extents: (0.5, 0.25, 0.125),
                    surface: false,
                },
                velocity: Default::default(),
                size: RangeDef { min: 0.25, max: 0.5 },
                rotation: RangeDef { min: -0.5, max: 0.5 },
                spin: RangeDef { min: -2.0, max: 2.0 },
                color: Vec::new(),
            },
            update: Default::default(),
            render: RenderModule {
                blend: BlendMode::Alpha,
                ..Default::default()
            },
        }],
    };
    def.validate().expect("valid bounded fixture");
    def
}

fn valid_job(job: &SpawnJob) {
    assert!(
        job.position
            .iter()
            .chain(&job.velocity)
            .chain(&job.color)
            .chain([&job.lifetime, &job.size, &job.rotation, &job.spin, &job.phase])
            .all(|v| v.is_finite()),
        "nonfinite job: {job:?}"
    );
    assert!((32.0..=64.0).contains(&job.lifetime));
    assert!(job.size > 0.0);
    assert!((0.0..1.0).contains(&job.phase));
    assert!((0.0..=1.0).contains(&job.color[3]));
}

fn step(instance: &mut VfxInstance, def: &VfxSystemDef, dt: f32, headroom: u32, smoke_headroom: u32) -> u32 {
    let age = instance.age();
    let before = instance.live_particles();
    let count = instance.schedule_budgeted(def, dt, &LOD, headroom, smoke_headroom);
    assert_eq!(instance.age(), age + dt);
    assert!(count <= headroom && count <= smoke_headroom);
    assert!(instance.live_particles() <= def.system.budget);
    assert_eq!(instance.live_particles(), before + count, "no episode deaths");
    assert_eq!(instance.pending_jobs(), count);
    let mut jobs = Vec::new();
    instance.drain_jobs(&mut jobs);
    assert_eq!(jobs.len(), count as usize);
    for job in &jobs {
        valid_job(job);
    }
    assert_eq!(instance.pending_jobs(), 0);
    let live = instance.live_particles();
    instance.drain_jobs(&mut jobs);
    assert_eq!(
        jobs.len(),
        count as usize,
        "consecutive drain cannot duplicate jobs"
    );
    assert_eq!(instance.live_particles(), live, "upload retains live budget");
    count
}

fn partitions() -> Vec<Vec<u32>> {
    vec![
        vec![60],
        vec![1; 60],
        vec![4; 15],
        vec![0, 1, 7, 0, 12, 3, 17, 20, 0],
        vec![20, 17, 3, 12, 0, 7, 1, 0],
    ]
}

#[test]
fn stationary_rate_totals_match_accumulated_elapsed_time_across_partitions() {
    for rate in [0.0, 0.5, 2.5, 7.5, 32.0] {
        for (lod_scale, count_scale, intensity) in [(1.0, 1.0, 1.0), (0.5, 1.5, 0.5), (1.0, 0.5, 2.0)] {
            let def = definition(SpawnMode::Rate(rate), lod_scale, count_scale);
            // Independent closed form: floor(rate * elapsed * all fixed multipliers).
            // f64 here never carries or updates a production-style fractional accumulator.
            let expected = |ticks: u32| {
                (f64::from(rate) * f64::from(ticks) / 16.0
                    * f64::from(lod_scale)
                    * f64::from(count_scale)
                    * f64::from(intensity))
                .floor() as u32
            };
            let mut totals = Vec::new();
            for parts in partitions() {
                assert_eq!(parts.iter().sum::<u32>(), 60);
                let mut instance = VfxInstance::new(0, 73, START, Vec3::Z);
                instance.set_intensity(intensity);
                let (mut ticks, mut total) = (0, 0);
                for dt_ticks in &parts {
                    ticks += dt_ticks;
                    total += step(&mut instance, &def, *dt_ticks as f32 / 16.0, 4096, 4096);
                    if *dt_ticks > 0 {
                        assert_eq!(instance.lod_scale(), lod_scale);
                    }
                    assert_eq!(total, expected(ticks), "rate={rate}, parts={parts:?}, t={ticks}");
                }
                totals.push(total);
            }
            assert!(totals.iter().all(|&total| total == expected(60)));
        }
    }
}

#[test]
fn straight_travel_totals_match_reference_lengths_across_partitions() {
    // Analytic geometry: axis stride length 1/8; 3-4-5 stride length 5/32.
    // Expected distances never call the vector length routine used by the scheduler.
    for (stride, length) in [
        (Vec3::new(-0.125, 0.0, 0.0), 0.125_f64),
        (Vec3::new(3.0 / 32.0, 4.0 / 32.0, 0.0), 5.0 / 32.0),
    ] {
        for every in [0.25, 0.5, 2.5] {
            for (lod_scale, count_scale, intensity) in [(1.0, 1.0, 1.0), (0.5, 1.5, 0.5)] {
                let def = definition(SpawnMode::PerDistance { every }, lod_scale, count_scale);
                let expected = |ticks: u32| {
                    (f64::from(ticks)
                        * length
                        * f64::from(lod_scale)
                        * f64::from(count_scale)
                        * f64::from(intensity)
                        / f64::from(every))
                    .floor() as u32
                };
                for parts in partitions() {
                    let mut instance = VfxInstance::new(0, 91, START, Vec3::Z);
                    instance.set_intensity(intensity);
                    let (mut ticks, mut total) = (0, 0);
                    for dt_ticks in &parts {
                        ticks += dt_ticks;
                        let dt = *dt_ticks as f32 / 16.0;
                        instance.set_transform(START + stride * ticks as f32, Vec3::Z, dt);
                        total += step(&mut instance, &def, dt, 4096, 4096);
                        if *dt_ticks > 0 {
                            assert_eq!(instance.lod_scale(), lod_scale);
                        }
                        assert_eq!(
                            total,
                            expected(ticks),
                            "every={every}, stride={stride:?}, parts={parts:?}, t={ticks}"
                        );
                    }
                    assert_eq!(total, expected(60));
                    // Waiting without traveling cannot consume the residual distance again.
                    assert_eq!(step(&mut instance, &def, 0.25, 4096, 4096), 0);
                }
            }
        }
    }
}

#[test]
fn burst_fires_once_despite_zero_dt_and_consecutive_drains() {
    let def = definition(SpawnMode::Burst(7), 1.0, 1.0);
    let mut instance = VfxInstance::new(0, 11, START, Vec3::Z);
    let counts: Vec<_> = [0.0, 0.0, 0.125, 0.0, 0.25, 0.0, 1.0]
        .into_iter()
        .map(|dt| step(&mut instance, &def, dt, 4096, 4096))
        .collect();
    assert_eq!(counts, [0, 0, 7, 0, 0, 0, 0]);
    assert_eq!(instance.live_particles(), 7);
    assert!(!instance.is_active(), "spent burst does not reactivate");
}

#[test]
fn finite_windows_use_inclusive_end_of_step_age_and_whole_step_input() {
    // delay=1/2, duration=1/2, rate=8. Eligible frame endpoints are [1/2, 1].
    // A step spanning the entire window can miss it; an eligible endpoint charges all dt.
    let cases: &[(&[f32], &[u32])] = &[
        (&[0.25, 0.25, 0.25, 0.25, 0.25], &[0, 2, 2, 2, 0]),
        (&[1.25], &[0]),
        (&[1.0, 0.25], &[8, 0]),
        (&[0.5, 0.0, 0.5, 0.25], &[4, 0, 4, 0]),
    ];
    for mode in [SpawnMode::Rate(8.0), SpawnMode::PerDistance { every: 0.125 }] {
        let mut def = definition(mode, 1.0, 1.0);
        def.emitters[0].delay = 0.5;
        def.emitters[0].duration = Some(0.5);
        def.validate().expect("finite window fixture");
        for &(deltas, expected) in cases {
            let mut instance = VfxInstance::new(0, 5, START, Vec3::Z);
            let mut time = 0.0;
            let counts: Vec<_> = deltas
                .iter()
                .map(|&dt| {
                    time += dt;
                    instance.set_transform(START + Vec3::X * time, Vec3::Z, dt);
                    step(&mut instance, &def, dt, 4096, 4096)
                })
                .collect();
            assert_eq!(counts, expected, "mode={mode:?}, deltas={deltas:?}");
            assert!(!instance.is_active());
            assert_eq!(step(&mut instance, &def, 2.0, 4096, 4096), 0, "no implicit loop");
        }
    }
}

#[test]
fn zero_duration_window_can_be_hit_exactly_or_skipped_without_looping() {
    for mode in [SpawnMode::Rate(8.0), SpawnMode::Burst(7)] {
        let mut def = definition(mode, 1.0, 1.0);
        def.emitters[0].delay = 0.5;
        def.emitters[0].duration = Some(0.0);
        for (deltas, expected) in [
            (
                vec![0.25, 0.25, 0.0, 0.25, 1.0],
                vec![
                    0,
                    match mode {
                        SpawnMode::Burst(_) => 7,
                        _ => 2,
                    },
                    0,
                    0,
                    0,
                ],
            ),
            (vec![0.75, 1.0], vec![0, 0]),
        ] {
            let mut instance = VfxInstance::new(0, 19, START, Vec3::Z);
            let counts: Vec<_> = deltas
                .into_iter()
                .map(|dt| step(&mut instance, &def, dt, 4096, 4096))
                .collect();
            assert_eq!(counts, expected, "mode={mode:?}");
            assert!(!instance.is_active());
        }
    }
}

#[test]
fn binding_budgets_bound_counts_but_are_explicitly_partition_dependent() {
    // Same 2 seconds at rate 8. Per-call headroom is replenished by the host each step;
    // system headroom is not: draining jobs leaves all these long-lived particles live.
    for (system_budget, headroom, smoke_headroom, coarse, fine) in
        [(4096, 3, 4096, 3, 6), (4096, 4096, 2, 2, 4), (5, 3, 4096, 3, 5)]
    {
        let mut def = definition(SpawnMode::Rate(8.0), 1.0, 1.0);
        def.system.budget = system_budget;
        let mut totals = Vec::new();
        for (parts, expected) in [(vec![2.0], coarse), (vec![1.0, 1.0], fine)] {
            let mut instance = VfxInstance::new(0, 23, START, Vec3::Z);
            let total: u32 = parts
                .into_iter()
                .map(|dt| step(&mut instance, &def, dt, headroom, smoke_headroom))
                .sum();
            assert_eq!(total, expected);
            assert_eq!(instance.live_particles(), expected);
            totals.push(total);
        }
        assert_ne!(totals[0], totals[1], "no invariance claim with binding caps");
    }
}
