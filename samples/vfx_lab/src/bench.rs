// SPDX-License-Identifier: MIT-0
//! `vfx_lab --bench`: the `specs/26` § 6 VFX perf budget with GPU timestamps.
//!
//! Fills the pool to ~100k live particles (desktop High) and measures the VFX
//! compute pass (emit + update + bitonic sort + indirect prep) and the VFX draw
//! pass (additive billboards, sorted alpha billboards, mesh particles) with
//! pass-level GPU timestamps over the harness backdrop at the profile's
//! resolution. Reports mean / p99 / max milliseconds.
//!
//! The particle mass is alpha-blended smoke on purpose: that is the worst case —
//! every particle passes through the alpha sort.
//!
//! `vfx_lab --sort-bench` ([`run_shot`]) compares the alpha sorts ([`SortAlgorithm`]) on the
//! smoke stress and on a DO27 barrage (the showcase explosion and dust trail, re-fired).

use regia_core::math::Vec3;
use regia_render::RenderDevice;
use regia_vfx::gpu::{SortAlgorithm, VfxError, VfxProfile};
use regia_vfx::offscreen::VfxHarness;
use regia_vfx::scene::{VfxLibrary, VfxScene};
use regia_vfx::sim::LodInput;
use regia_vfx::timing::GpuTimer;

/// One benchmark result row.
#[derive(Debug, Clone, Copy)]
pub struct BenchRow {
    /// Profile label.
    pub label: &'static str,
    /// Live particles reported by the GPU in the steady state.
    pub alive: u32,
    /// Mean VFX compute (simulate) milliseconds.
    pub sim_mean_ms: f32,
    /// p99 VFX compute milliseconds.
    pub sim_p99_ms: f32,
    /// Mean VFX draw milliseconds.
    pub draw_mean_ms: f32,
    /// p99 VFX draw milliseconds.
    pub draw_p99_ms: f32,
    /// Mean total (simulate + draw) milliseconds.
    pub total_mean_ms: f32,
}

/// What the benchmark fills the pool with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchShot {
    /// One alpha smoke column sized to keep the pool at its cap.
    SmokeStress,
    /// DO27's showcase explosion (fireball, smoke, debris) re-fired across a 30 m strip every
    /// 0.4 s, plus four dust trails: a convoy under fire.
    Do27Barrage,
    /// Three showcase smoke columns (lit puffs) filling the frame, after 7 s: the lit-puff
    /// shading cost (`vfx_lab --smoke-bench`).
    ShowcaseSmoke,
    /// A DO27 firefight on Europa (airless): six hover washes on ice with two duct plumes each,
    /// two jetpack pilots, a chaincannon and two rifles firing with muzzles and tracers, and 30
    /// hits a second on metal, concrete and glass (`vfx_lab --combat-bench`).
    CombatAirless,
    /// The same firefight on Mars (dust washes that linger, thin smoke).
    CombatMars,
}

/// Runs the particle-budget benchmark for one profile (smoke stress, the default sort).
///
/// # Errors
///
/// Returns [`VfxError`] when the GPU setup or a frame fails.
pub fn run(
    device: &RenderDevice,
    profile: VfxProfile,
    label: &'static str,
    width: u32,
    height: u32,
) -> Result<BenchRow, VfxError> {
    run_shot(
        device,
        profile,
        label,
        width,
        height,
        BenchShot::SmokeStress,
        SortAlgorithm::default(),
    )
}

/// Runs one benchmark shot with the chosen alpha sort.
///
/// # Errors
///
/// Returns [`VfxError`] when the GPU setup or a frame fails.
pub fn run_shot(
    device: &RenderDevice,
    profile: VfxProfile,
    label: &'static str,
    width: u32,
    height: u32,
    shot: BenchShot,
    sort: SortAlgorithm,
) -> Result<BenchRow, VfxError> {
    let mut library = VfxLibrary::new();
    let (mut scene, target_alive) = match shot {
        BenchShot::SmokeStress => {
            // The stress effect: a smoke column sized to saturate the pool budget.
            library
                .add(smoke_stress(profile.particle_capacity))
                .map_err(|e| VfxError::Shader(format!("stress: {e}")))?;
            let mut scene = VfxScene::new(library);
            scene
                .spawn("bench_stress", Vec3::new(0.0, 0.4, 0.0), Vec3::Z, 7)
                .ok_or_else(|| VfxError::Shader("spawn failed".into()))?;
            (scene, profile.particle_capacity as f32 * 0.995)
        }
        BenchShot::Do27Barrage => {
            for def in [
                regia_vfx::showcase::explosion(),
                regia_vfx::showcase::dust_trail(),
            ] {
                library
                    .add(def)
                    .map_err(|e| VfxError::Shader(format!("showcase: {e}")))?;
            }
            let mut scene = VfxScene::new(library);
            for i in 0..4u64 {
                scene
                    .spawn(
                        "dust_trail",
                        Vec3::new(-9.0 + i as f32 * 6.0, 0.1, 2.0),
                        Vec3::Z,
                        40 + i,
                    )
                    .ok_or_else(|| VfxError::Shader("dust spawn failed".into()))?;
            }
            // The barrage reaches a steady state within the 4 s warm-up.
            (scene, f32::INFINITY)
        }
        BenchShot::ShowcaseSmoke => {
            library
                .add(regia_vfx::showcase::smoke())
                .map_err(|e| VfxError::Shader(format!("showcase: {e}")))?;
            let mut scene = VfxScene::new(library);
            for (i, x) in [-5.0f32, 0.0, 5.0].into_iter().enumerate() {
                scene
                    .spawn("smoke", Vec3::new(x, 0.0, 0.0), Vec3::Y, 60 + i as u64)
                    .ok_or_else(|| VfxError::Shader("smoke spawn failed".into()))?;
            }
            // The columns are full grown after the 7 s warm-up.
            (scene, f32::INFINITY)
        }
        BenchShot::CombatAirless | BenchShot::CombatMars => {
            let mut scene = VfxScene::new(library);
            scene.set_medium(if shot == BenchShot::CombatMars {
                regia_vfx::VfxMedium::mars()
            } else {
                regia_vfx::VfxMedium::airless(regia_vfx::medium::EUROPA_GRAVITY)
            });
            let surface = if shot == BenchShot::CombatMars {
                regia_vfx::HoverSurface::Dust
            } else {
                regia_vfx::HoverSurface::Ice
            };
            let wash = surface.effect().unwrap_or("hover_wash_dust");
            for base in [
                wash,
                "muzzle_cannon",
                "muzzle_flash",
                "tracer",
                "tracer_heavy",
                "hit_metal",
                "hit_concrete",
                "hit_glass",
                "plume_duct",
                "plume_jetpack",
            ] {
                if !regia_vfx::atmos_presets::ensure_builtin(&mut scene, base) {
                    return Err(VfxError::Shader(format!("no built-in {base}")));
                }
            }
            for i in 0..6u64 {
                let x = -12.5 + i as f32 * 5.0;
                scene
                    .spawn(
                        wash,
                        Vec3::new(x, 0.0, -2.0 + (i % 2) as f32 * 3.0),
                        Vec3::Y,
                        80 + i,
                    )
                    .ok_or_else(|| VfxError::Shader("wash spawn failed".into()))?;
            }
            // Each hull's two lift-duct plumes, and two jetpack pilots.
            for i in 0..6u64 {
                let x = -12.5 + i as f32 * 5.0;
                let z = -2.0 + (i % 2) as f32 * 3.0;
                for side in [-1.5f32, 1.5] {
                    scene
                        .spawn("plume_duct", Vec3::new(x + side, 0.75, z), -Vec3::Y, 120 + i)
                        .ok_or_else(|| VfxError::Shader("plume spawn failed".into()))?;
                }
            }
            for (i, x) in [-3.0f32, 3.0].into_iter().enumerate() {
                scene
                    .spawn("plume_jetpack", Vec3::new(x, 2.5, 0.0), -Vec3::Y, 140 + i as u64)
                    .ok_or_else(|| VfxError::Shader("jetpack spawn failed".into()))?;
            }
            // Three guns firing across the frame (forward = -fire direction).
            for (i, (tracer, x)) in [("tracer_heavy", -10.0f32), ("tracer", -6.0), ("tracer", 8.0)]
                .into_iter()
                .enumerate()
            {
                let fire_dir = if x < 0.0 { Vec3::X } else { -Vec3::X };
                scene
                    .spawn(
                        tracer,
                        Vec3::new(x, 1.5, 1.0 + i as f32),
                        -fire_dir,
                        90 + i as u64,
                    )
                    .ok_or_else(|| VfxError::Shader("tracer spawn failed".into()))?;
            }
            (scene, f32::INFINITY)
        }
    };
    let mut fired = 0u64;
    let mut fire = |scene: &mut VfxScene, tick: u32| {
        if shot == BenchShot::Do27Barrage && tick.is_multiple_of(24) {
            let x = -12.0 + (fired % 7) as f32 * 4.0;
            let z = -4.0 + (fired % 3) as f32 * 3.0;
            let _ = scene.spawn("explosion", Vec3::new(x, 0.2, z), Vec3::Y, 100 + fired);
            fired += 1;
        }
        if matches!(shot, BenchShot::CombatAirless | BenchShot::CombatMars) {
            // Chaincannon at 400 rpm, rifles at 600 rpm.
            if tick.is_multiple_of(9) {
                let _ = scene.spawn("muzzle_cannon", Vec3::new(-10.0, 1.5, 1.0), -Vec3::X, 200 + fired);
            }
            if tick.is_multiple_of(6) {
                let _ = scene.spawn("muzzle_flash", Vec3::new(-6.0, 1.5, 2.0), -Vec3::X, 300 + fired);
                let _ = scene.spawn("muzzle_flash", Vec3::new(8.0, 1.5, 3.0), Vec3::X, 400 + fired);
            }
            // Hits: one every other frame, 30 a second, over hull, wall and canopy.
            if tick.is_multiple_of(2) {
                let k = fired % 3;
                let base = ["hit_metal", "hit_concrete", "hit_glass"][k as usize];
                let x = -9.0 + (fired % 11) as f32 * 1.8;
                let at = Vec3::new(x, 0.5 + (fired % 5) as f32 * 0.4, -1.0);
                let _ = scene.spawn(base, at, Vec3::Z, 500 + fired);
            }
            fired += 1;
        }
    };

    let (eye, target) = match shot {
        // The three columns fill the frame.
        BenchShot::ShowcaseSmoke => (Vec3::new(0.0, 6.0, 16.0), Vec3::new(0.0, 6.0, 0.0)),
        _ => (Vec3::new(0.0, 6.0, 14.0), Vec3::new(0.0, 3.0, 0.0)),
    };
    let camera = regia_vfx::VfxCamera {
        eye,
        target,
        fov_y: 55.0f32.to_radians(),
        near: 0.05,
        far: 200.0,
    };
    let dt = 1.0f32 / 60.0;
    let mut harness = VfxHarness::new(device, profile)?;
    harness.gpu().set_sort_algorithm(sort);
    let timer = GpuTimer::new(&device.device, 4)
        .ok_or_else(|| VfxError::Shader("GPU timestamps unsupported".into()))?;

    // Warm up until the pool is saturated (or 12 s of sim time; the barrage runs 4 s).
    let warmup_ticks = match shot {
        BenchShot::SmokeStress => (12.0 / dt) as u32,
        BenchShot::Do27Barrage => (4.0 / dt) as u32,
        BenchShot::ShowcaseSmoke => (7.0 / dt) as u32,
        // Mars dust washes linger 3.5–6 s: reach the steady state.
        BenchShot::CombatAirless | BenchShot::CombatMars => (7.0 / dt) as u32,
    };
    let mut counters = [0u32; 4];
    let mut tick = 0u32;
    for _ in 0..warmup_ticks {
        fire(&mut scene, tick);
        tick += 1;
        scene.update(
            dt,
            &LodInput {
                camera_pos: camera.eye,
            },
            profile.particle_capacity,
        );
        scene.upload(harness.gpu());
        counters = harness.render_timed(device, &camera, width, height, dt, &timer)?;
        if counters[3] as f32 >= target_alive {
            break;
        }
    }

    // Measure.
    let frames = 120u32;
    let mut sims = Vec::with_capacity(frames as usize);
    let mut draws = Vec::with_capacity(frames as usize);
    let mut alpha = 0u64;
    for _ in 0..frames {
        fire(&mut scene, tick);
        tick += 1;
        scene.update(
            dt,
            &LodInput {
                camera_pos: camera.eye,
            },
            profile.particle_capacity,
        );
        scene.upload(harness.gpu());
        counters = harness.render_timed(device, &camera, width, height, dt, &timer)?;
        let ms = timer
            .read_ms(&device.device, &device.queue)
            .ok_or_else(|| VfxError::Shader("timestamp readback".into()))?;
        // Pairs: [sim, draw].
        let sim = ms.first().copied().unwrap_or(0.0);
        let draw = ms.get(1).copied().unwrap_or(0.0);
        sims.push(sim);
        draws.push(draw);
        alpha += u64::from(counters[1]);
    }
    let alpha_mean = alpha / u64::from(frames);
    let p99 = |v: &mut Vec<f32>| -> f32 {
        v.sort_by(|a, b| a.total_cmp(b));
        v[(v.len() as f32 * 0.99) as usize % v.len().saturating_sub(1)]
    };
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
    let alive = counters[3];
    let (mut s, mut d) = (sims.clone(), draws.clone());
    let row = BenchRow {
        label,
        alive,
        sim_mean_ms: mean(&sims),
        sim_p99_ms: p99(&mut s),
        draw_mean_ms: mean(&draws),
        draw_p99_ms: p99(&mut d),
        total_mean_ms: mean(&sims) + mean(&draws),
    };
    println!(
        "{} [{sort:?}]: {} alive ({alpha_mean} alpha-sorted) @ {}x{} — sim {:.3} ms (p99 {:.3}), draw {:.3} ms (p99 {:.3}), total {:.3} ms",
        row.label,
        row.alive,
        width,
        height,
        row.sim_mean_ms,
        row.sim_p99_ms,
        row.draw_mean_ms,
        row.draw_p99_ms,
        row.total_mean_ms,
    );
    Ok(row)
}

/// The stress system: saturates `budget` live alpha-blended particles.
fn smoke_stress(budget: u32) -> regia_vfx::VfxSystemDef {
    use regia_vfx::format::{
        BlendMode, ColorKey, EmitterDef, FloatKey, InitModule, RangeDef, RenderMode, RenderModule, ShapeDef,
        SpawnMode, SpawnModule, SpriteId, SystemModule, UpdateModule, VelocityDef, VelocityMode,
    };
    let r = |min: f32, max: f32| RangeDef { min, max };
    // rate × mean lifetime ≈ budget: keep the pool pinned at its cap.
    let lifetime = 4.5f32;
    let rate = budget as f32 / lifetime * 1.10;
    regia_vfx::VfxSystemDef {
        version: 1,
        name: "bench_stress".into(),
        system: SystemModule {
            bounds: None,
            budget,
            lod: None,
        },
        emitters: vec![EmitterDef {
            name: "smoke".into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode: SpawnMode::Rate(rate),
                lifetime: r(lifetime * 0.96, lifetime * 1.04),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Sphere {
                    radius: r(0.0, 0.5),
                    surface: false,
                },
                velocity: VelocityDef {
                    mode: VelocityMode::Directional {
                        dir: (0.0, 1.0, 0.0),
                        spread: 22.0,
                        speed: r(1.0, 4.0),
                    },
                    inherit: 0.0,
                },
                size: r(0.3, 0.8),
                rotation: r(0.0, 0.0),
                spin: r(-0.4, 0.4),
                color: vec![ColorKey {
                    t: 0.0,
                    color: "#777777b8".into(),
                }],
            },
            update: UpdateModule {
                gravity: (0.0, 0.4, 0.0),
                drag: 0.6,
                curl: Some((1.2, 0.4, 0.25)),
                size_over_life: vec![FloatKey { t: 0.0, v: 0.4 }, FloatKey { t: 1.0, v: 1.8 }],
                alpha_over_life: vec![
                    FloatKey { t: 0.0, v: 0.55 },
                    FloatKey { t: 0.7, v: 0.35 },
                    FloatKey { t: 1.0, v: 0.0 },
                ],
                ..Default::default()
            },
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::Puff,
                blend: BlendMode::Alpha,
                softness: 0.4,
                ..Default::default()
            },
        }],
    }
}
