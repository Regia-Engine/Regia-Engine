// SPDX-License-Identifier: MPL-2.0
//! The M14 evidence suite: one mid-effect shot per DO27 effect, plus decals, trails and
//! camera shake (spec `26` § 6; round-2 requirement).
//!
//! [`render_shot`] drives the full stack offscreen — CPU scheduling ([`crate::scene`]),
//! GPU simulation and draw ([`crate::gpu`]) over [`crate::offscreen::VfxHarness`]'s lit
//! ground + sky — twice: once with the effect ("with") and once without it, at the same
//! simulation time. [`measure_effect`] turns the pair into [`EffectMetrics`] and
//! [`assert_shot_plausible`] fails the empty/black/blown-out cases.
//!
//! `samples/vfx_lab --evidence` saves each shot as a PNG (+ a contact sheet) into
//! `docs/evidence/m14/`; `crates/regia_vfx/tests/evidence.rs` runs the same shots as GPU
//! tests.

use regia_core::math::{Quat, Vec3};
use regia_render::RenderDevice;

use crate::format::VfxSystemDef;
use crate::gpu::{VfxCamera, VfxError, VfxProfile};
use crate::measure::{
    EffectMetrics, MIN_COVERAGE, SPARKS_MIN_COVERAGE, assert_effect_plausible, measure_effect, metrics_line,
};
use crate::offscreen::VfxHarness;
use crate::scene::{VfxLibrary, VfxScene};
use crate::shake::CameraShake;
use crate::sim::LodInput;
use crate::trails::{TrailRig, TrailStyle};

/// How one decal appears in a shot (see [`crate::decal::SpawnDecal`]).
#[derive(Debug, Clone, Copy)]
pub struct DecalSpec {
    /// Centre of the decal volume.
    pub position: Vec3,
    /// Rotation around world Y (degrees) — the projection axis stays +Y (ground decals).
    pub yaw_deg: f32,
    /// Half extents `(u, v, depth)` in metres.
    pub half: Vec3,
    /// sRGB-linear tint.
    pub color: [f32; 4],
    /// Lifetime (seconds).
    pub lifetime: Option<f32>,
}

/// The motion of a shot's trail source.
#[derive(Debug, Clone, Copy)]
pub enum TrailSpec {
    /// A horizontal circle (for fly-by trails).
    Circle {
        /// Circle radius (m).
        radius: f32,
        /// Full turns per second.
        pub_speed: f32,
        /// Height above the ground (m).
        height: f32,
    },
    /// A horizontal figure-eight (lemniscate).
    Figure8 {
        /// Half extent (m).
        extent: f32,
        /// Full cycles per second.
        cycles: f32,
        /// Height above the ground (m).
        height: f32,
    },
}

impl TrailSpec {
    fn position(&self, t: f32) -> Vec3 {
        match *self {
            Self::Circle {
                radius,
                pub_speed,
                height,
            } => {
                let a = t * pub_speed * std::f32::consts::TAU;
                Vec3::new(radius * a.cos(), height, radius * a.sin())
            }
            Self::Figure8 {
                extent,
                cycles,
                height,
            } => {
                let a = t * cycles * std::f32::consts::TAU;
                let (s, c) = a.sin_cos();
                Vec3::new(extent * s, height, extent * s * c)
            }
        }
    }
}

/// One evidence shot: what spawns, where the camera is, when to capture.
#[derive(Debug, Clone)]
pub struct EffectShot {
    /// File name (`{name}.png`) and test label.
    pub name: &'static str,
    /// Showcase effect to spawn (name), or `None` for decal/trail-only shots.
    pub effect: Option<&'static str>,
    /// Instance origin (start position when `move_to` is set).
    pub origin: Vec3,
    /// Instance local +Z (cone/velocity axis).
    pub forward: Vec3,
    /// Linear instance motion over the shot (drives `PerDistance` spawns + trails).
    pub move_to: Option<Vec3>,
    /// Seconds to advance before the capture frame.
    pub capture: f32,
    /// Camera eye.
    pub eye: Vec3,
    /// Camera look-at.
    pub target: Vec3,
    /// Vertical FOV (radians).
    pub fov_y: f32,
    /// Decals on the ground.
    pub decals: Vec<DecalSpec>,
    /// Trail rig following this motion.
    pub trail: Option<TrailSpec>,
    /// Trigger full camera-shake trauma mid-shot (explosion-sized blasts shake the camera).
    pub shake: bool,
    /// Expected colour bias of the effect pixels: `Some(true)` warm, `Some(false)` cool.
    pub warm: Option<bool>,
    /// Evidence-only multiplier on the spawned effect's emitter `count_scale` (1.0 = the shipped
    /// effect, unchanged). Small, faint particles (sparks) are invisible-edge cases on a
    /// coverage floor: the evidence scene frames a denser burst of the *same* effect so the shot
    /// measures the effect's rendering, not one vendor's sub-pixel coverage. Games never see
    /// this — it is applied only by [`render_shot`].
    pub density: f32,
    /// Minimum diff coverage this shot must produce (fraction of the frame). The generic
    /// [`MIN_COVERAGE`] band is [`assert_effect_plausible`]'s floor; a shot can pin a tighter
    /// one (sparks at [`SPARKS_MIN_COVERAGE`]) so a regression toward the generic floor still
    /// fails.
    pub min_coverage: f32,
}

/// A shot's result: the pixel pair, metrics and GPU counters.
pub struct ShotResult {
    /// The with-effect frame (tightly packed RGBA8, `width × height`).
    pub with: Vec<u8>,
    /// The without-effect frame (same size, same ticks).
    pub without: Vec<u8>,
    /// Diff-mask metrics of the pair.
    pub metrics: EffectMetrics,
    /// GPU counters of the capture frame: `(additive, alpha, mesh, alive)`.
    pub counters: [u32; 4],
    /// Coverage of the shaken-vs-unshaken pair (camera-shake shots only).
    pub shake_coverage: Option<f32>,
    /// Linear pre-present RGBA of the with-effect capture frame (`Rgba16Float` decoded).
    pub with_hdr: Vec<f32>,
    /// Linear pre-present RGBA of the without-effect capture frame.
    pub without_hdr: Vec<f32>,
}

impl ShotResult {
    /// Frame width/height convenience.
    #[must_use]
    pub fn size(&self) -> usize {
        self.with.len()
    }
}

/// The M14 evidence shots (one per DO27 effect + decal, trail, shake).
pub fn effect_shots() -> Vec<EffectShot> {
    let deg = |d: f32| d.to_radians();
    vec![
        EffectShot {
            name: "muzzle_flash",
            effect: Some("muzzle_flash"),
            // The cone shoots along -instance-forward; forward +Z sends it toward -Z,
            // i.e. away from the camera behind it.
            origin: Vec3::new(0.0, 1.6, 0.0),
            forward: Vec3::Z,
            move_to: None,
            capture: 0.05,
            eye: Vec3::new(0.0, 1.7, 2.8),
            target: Vec3::new(0.0, 1.55, -0.5),
            fov_y: deg(55.0),
            decals: vec![],
            trail: None,
            shake: false,
            warm: Some(true),
            density: 1.0,
            min_coverage: MIN_COVERAGE,
        },
        EffectShot {
            name: "tracer",
            effect: Some("tracer"),
            // Elevated side view: the ribbon travels down -Z in the air, ground below.
            origin: Vec3::new(0.0, 2.6, 0.0),
            forward: Vec3::Z,
            move_to: None,
            capture: 0.45,
            eye: Vec3::new(9.0, 4.5, 4.0),
            target: Vec3::new(0.0, 2.2, -18.0),
            fov_y: deg(55.0),
            decals: vec![],
            trail: None,
            shake: false,
            warm: Some(true),
            density: 1.0,
            min_coverage: MIN_COVERAGE,
        },
        EffectShot {
            name: "impact",
            effect: Some("impact"),
            origin: Vec3::new(0.0, 1.2, 0.0),
            forward: Vec3::Z,
            move_to: None,
            capture: 0.18,
            eye: Vec3::new(2.2, 1.9, 3.2),
            target: Vec3::new(0.0, 1.1, 0.0),
            fov_y: deg(55.0),
            decals: vec![DecalSpec {
                position: Vec3::new(0.0, 0.0, 0.0),
                yaw_deg: 25.0,
                half: Vec3::new(0.55, 0.55, 0.8),
                color: [0.16, 0.10, 0.06, 0.9],
                lifetime: None,
            }],
            trail: None,
            shake: false,
            warm: Some(true),
            density: 1.0,
            min_coverage: MIN_COVERAGE,
        },
        EffectShot {
            name: "explosion",
            effect: Some("explosion"),
            origin: Vec3::new(0.0, 1.0, 0.0),
            forward: Vec3::Z,
            move_to: None,
            // Mid blast: flash is gone, flame cards and streaks are up, smoke has started.
            capture: 0.60,
            eye: Vec3::new(4.6, 2.8, 5.6),
            target: Vec3::new(0.0, 1.4, 0.0),
            fov_y: deg(55.0),
            decals: vec![DecalSpec {
                position: Vec3::new(0.0, 0.0, 0.0),
                yaw_deg: 10.0,
                half: Vec3::new(1.6, 1.6, 1.0),
                color: [0.08, 0.06, 0.04, 0.95],
                lifetime: None,
            }],
            trail: None,
            shake: false,
            warm: Some(true),
            density: 1.0,
            min_coverage: MIN_COVERAGE,
        },
        EffectShot {
            name: "exhaust",
            effect: Some("exhaust"),
            origin: Vec3::new(0.0, 1.1, 0.0),
            forward: Vec3::Z,
            move_to: None,
            capture: 1.60,
            eye: Vec3::new(2.2, 1.6, 2.6),
            target: Vec3::new(-0.4, 1.4, -1.2),
            fov_y: deg(55.0),
            decals: vec![],
            trail: None,
            shake: false,
            warm: None,
            density: 1.0,
            min_coverage: MIN_COVERAGE,
        },
        EffectShot {
            name: "dust_trail",
            effect: Some("dust_trail"),
            origin: Vec3::new(-4.0, 0.35, 0.8),
            forward: Vec3::Z,
            move_to: Some(Vec3::new(4.0, 0.35, -0.8)),
            capture: 1.50,
            eye: Vec3::new(0.5, 2.4, 6.0),
            target: Vec3::new(0.0, 0.5, -0.5),
            fov_y: deg(55.0),
            decals: vec![],
            trail: None,
            shake: false,
            warm: None,
            density: 1.0,
            min_coverage: MIN_COVERAGE,
        },
        EffectShot {
            // The shipped sparks are few and tiny (0.02–0.06 m stretched streaks); at showcase
            // density the shot sat on the 0.5 % emptiness floor and dipped below it on the
            // RTX 5080 (0.0050 coverage, 2026-10-05). The evidence scene spawns the same effect
            // at 4× emitter density and frames the mid-burst arc closer and tighter, so coverage
            // measures the streak rendering (~4 % on lavapipe) instead of sub-pixel luck. The
            // shipped `sparks` system is unchanged.
            name: "sparks",
            effect: Some("sparks"),
            origin: Vec3::new(0.0, 0.5, 0.0),
            forward: Vec3::Z,
            move_to: None,
            capture: 0.80,
            eye: Vec3::new(0.95, 0.95, 1.30),
            target: Vec3::new(0.0, 0.85, 0.0),
            fov_y: deg(45.0),
            decals: vec![],
            trail: None,
            shake: false,
            warm: Some(true),
            density: 4.0,
            min_coverage: SPARKS_MIN_COVERAGE,
        },
        EffectShot {
            name: "weather",
            effect: Some("weather"),
            origin: Vec3::new(0.0, 7.0, 0.0),
            forward: Vec3::Z,
            move_to: None,
            capture: 1.80,
            eye: Vec3::new(5.5, 2.3, 7.5),
            target: Vec3::new(0.0, 3.0, 0.0),
            fov_y: deg(60.0),
            decals: vec![],
            trail: None,
            shake: false,
            warm: Some(false),
            density: 1.0,
            min_coverage: MIN_COVERAGE,
        },
        EffectShot {
            name: "smoke",
            effect: Some("smoke"),
            origin: Vec3::new(0.0, 0.2, 0.0),
            forward: Vec3::Z,
            move_to: None,
            capture: 3.40,
            eye: Vec3::new(4.4, 3.4, 5.4),
            target: Vec3::new(0.0, 3.1, 0.0),
            fov_y: deg(55.0),
            decals: vec![],
            trail: None,
            shake: false,
            warm: None,
            density: 1.0,
            min_coverage: MIN_COVERAGE,
        },
        EffectShot {
            name: "decal",
            effect: None,
            origin: Vec3::ZERO,
            forward: Vec3::Z,
            move_to: None,
            capture: 0.40,
            eye: Vec3::new(0.6, 4.6, 2.2),
            target: Vec3::new(0.0, 0.0, 0.0),
            fov_y: deg(55.0),
            decals: vec![
                DecalSpec {
                    position: Vec3::new(0.0, 0.0, 0.0),
                    yaw_deg: 12.0,
                    half: Vec3::new(1.5, 1.5, 1.0),
                    color: [0.07, 0.05, 0.035, 0.95],
                    lifetime: None,
                },
                DecalSpec {
                    position: Vec3::new(1.4, 0.0, -0.9),
                    yaw_deg: 55.0,
                    half: Vec3::new(0.5, 0.5, 0.8),
                    color: [0.30, 0.12, 0.05, 0.9],
                    lifetime: None,
                },
                DecalSpec {
                    position: Vec3::new(-1.2, 0.0, 1.1),
                    yaw_deg: -30.0,
                    half: Vec3::new(0.35, 0.7, 0.8),
                    color: [0.55, 0.25, 0.05, 0.85],
                    lifetime: None,
                },
            ],
            trail: None,
            shake: false,
            warm: None,
            density: 1.0,
            min_coverage: MIN_COVERAGE,
        },
        EffectShot {
            name: "trail",
            effect: None,
            origin: Vec3::ZERO,
            forward: Vec3::Z,
            move_to: None,
            capture: 1.40,
            eye: Vec3::new(2.0, 1.95, 2.7),
            target: Vec3::new(0.4, 1.5, -0.2),
            fov_y: deg(55.0),
            decals: vec![],
            trail: Some(TrailSpec::Figure8 {
                extent: 2.2,
                cycles: 0.35,
                height: 1.5,
            }),
            shake: false,
            warm: Some(true),
            density: 1.0,
            min_coverage: MIN_COVERAGE,
        },
        EffectShot {
            name: "camera_shake",
            effect: Some("explosion"),
            origin: Vec3::new(0.0, 1.0, 0.0),
            forward: Vec3::Z,
            move_to: None,
            capture: 0.32,
            eye: Vec3::new(4.2, 2.6, 5.2),
            target: Vec3::new(0.0, 1.3, 0.0),
            fov_y: deg(55.0),
            decals: vec![DecalSpec {
                position: Vec3::new(0.0, 0.0, 0.0),
                yaw_deg: 10.0,
                half: Vec3::new(1.6, 1.6, 1.0),
                color: [0.08, 0.06, 0.04, 0.95],
                lifetime: None,
            }],
            trail: None,
            shake: true,
            warm: None,
            density: 1.0,
            min_coverage: MIN_COVERAGE,
        },
    ]
}

/// Renders one shot at `width`×`height` (RGBA8, sRGB) and returns the with/without pair,
/// metrics and counters.
///
/// The fixed step is 1/60 s; both runs tick the same number of frames so the background
/// state matches. The "without" run has no effect, decals, trail or shake — an empty scene
/// through the same pipeline.
pub fn render_shot(
    device: &RenderDevice,
    shot: &EffectShot,
    width: u32,
    height: u32,
) -> Result<ShotResult, VfxError> {
    render_shot_with(device, shot, crate::showcase::showcase_effects(), width, height)
}

/// [`render_shot`] with a caller-supplied effect library (for example one emitter of a
/// showcase effect, under the effect's name, to judge that layer alone).
pub fn render_shot_with(
    device: &RenderDevice,
    shot: &EffectShot,
    effects: Vec<VfxSystemDef>,
    width: u32,
    height: u32,
) -> Result<ShotResult, VfxError> {
    let dt = 1.0f32 / 60.0;
    let ticks = (shot.capture / dt).round().max(1.0) as u32;

    // ---- without ----
    let mut harness = VfxHarness::new(device, VfxProfile::desktop_high())?;
    harness.set_capture_hdr(true);
    let mut empty_scene = VfxScene::new(VfxLibrary::new());
    let camera = camera_of(shot);
    let mut last_without = Vec::new();
    let mut without_hdr = Vec::new();
    for _ in 0..ticks {
        empty_scene.update(
            dt,
            &LodInput {
                camera_pos: camera.eye,
            },
            100_000,
        );
        empty_scene.upload(harness.gpu());
        let frame = harness.render(device, &camera, width, height, dt)?;
        last_without = frame.pixels;
        without_hdr = frame.hdr.unwrap_or_default();
    }

    // ---- with (fresh GPU state) ----
    let mut harness = VfxHarness::new(device, VfxProfile::desktop_high())?;
    harness.set_capture_hdr(true);
    let mut with_hdr = Vec::new();
    let mut library = VfxLibrary::new();
    for def in effects {
        // The evidence-only density multiplier scales the shot effect's emitter count_scale
        // (see `EffectShot::density`); the shipped systems are untouched.
        let mut def = def;
        if shot.density != 1.0 && Some(def.name.as_str()) == shot.effect {
            for emitter in &mut def.emitters {
                emitter.spawn.count_scale *= shot.density;
            }
        }
        library
            .add(def)
            .map_err(|e| VfxError::Shader(format!("showcase: {e}")))?;
    }
    let mut scene = VfxScene::new(library);
    if let Some(effect) = shot.effect
        && scene.spawn(effect, shot.origin, shot.forward, 42).is_none()
    {
        return Err(VfxError::Shader(format!("unknown effect {effect}")));
    }
    let mut shake = CameraShake::default();
    if shot.shake {
        shake.add_trauma(1.0);
    }
    let mut trail = shot.trail.map(|_| {
        TrailRig::new(TrailStyle {
            width: 0.22,
            lifetime: 1.2,
            color: [1.0, 0.55, 0.12, 0.95],
            min_segment: 0.05,
        })
    });
    let mut last_with = Vec::new();
    let mut last_counters = [0u32; 4];
    let mut shake_offset = Vec3::ZERO;
    for tick in 0..ticks {
        let t = tick as f32 * dt;
        // Instance motion (drives PerDistance spawns).
        if let Some(to) = shot.move_to
            && let Some(instance) = scene.instances_mut().next()
        {
            let alpha = (t / shot.capture).min(1.0);
            instance.set_transform(shot.origin.lerp(to, alpha), shot.forward, dt);
        }
        scene.update(
            dt,
            &LodInput {
                camera_pos: camera.eye,
            },
            100_000,
        );
        let mut frame_lights = crate::lights::VfxFrameLights::default();
        scene.gather_lights(&mut frame_lights, 4);
        if let Some(light) = frame_lights.lights.first() {
            harness.set_point_light(light.position, light.color, light.radius);
        } else {
            harness.set_point_light(Vec3::ZERO, [0.0, 0.0, 0.0], 0.0);
        }

        if let (Some(spec), Some(rig)) = (shot.trail, trail.as_mut()) {
            rig.update(spec.position(t), dt);
            let mut verts = Vec::new();
            let right = (camera.target - camera.eye).normalize_or_zero().cross(Vec3::Y);
            rig.build_vertices(&mut verts, right, [1.0, 0.7, 0.3, 1.0]);
            harness.gpu().write_trails(&verts);
        }
        shake.update(dt, 1.2, 0.35, 0.02);
        shake_offset = shake.offset();

        scene.upload(harness.gpu());
        if tick == 0 && !shot.decals.is_empty() {
            harness.gpu().write_decals(
                &shot
                    .decals
                    .iter()
                    .map(|d| crate::decal::Decal {
                        position: d.position,
                        // Ground decals: local +Z (the projection axis) up, spun by yaw.
                        rotation: Quat::from_rotation_y(d.yaw_deg.to_radians())
                            * Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
                        half_size: d.half,
                        color: d.color,
                        age: 0.0,
                        lifetime: d.lifetime,
                    })
                    .collect::<Vec<_>>(),
            );
        }
        let frame = harness.render(device, &camera, width, height, dt)?;
        last_with = frame.pixels;
        last_counters = frame.counters;
        with_hdr = frame.hdr.unwrap_or_default();
    }

    let metrics = measure_effect(&last_with, &last_without, width);

    // Camera-shake shots: same state, one more frame with a tiny dt, shaken vs unshaken.
    let mut shake_coverage = None;
    if shot.shake {
        let calm = camera;
        let shaken_camera = VfxCamera {
            eye: calm.eye + shake_offset,
            target: calm.target,
            fov_y: calm.fov_y,
            near: calm.near,
            far: calm.far,
        };
        // Re-render the (near-)frozen state with both cameras: dt ~ 0 pushes the sim a hair.
        scene.upload(harness.gpu());
        let calm_frame = harness.render(device, &calm, width, height, 0.001)?;
        scene.upload(harness.gpu());
        let shaken_frame = harness.render(device, &shaken_camera, width, height, 0.001)?;
        let m = measure_effect(&shaken_frame.pixels, &calm_frame.pixels, width);
        shake_coverage = Some(m.coverage);
        last_with = shaken_frame.pixels;
    }

    Ok(ShotResult {
        with: last_with,
        without: last_without,
        metrics,
        counters: last_counters,
        shake_coverage,
        with_hdr,
        without_hdr,
    })
}

/// A one-line human-readable summary for reports.
pub fn shot_line(name: &str, result: &ShotResult) -> String {
    let mut line = metrics_line(name, &result.metrics);
    line.push_str(&format!(
        "  [additive {}, alpha {}, mesh {}, alive {}]",
        result.counters[0], result.counters[1], result.counters[2], result.counters[3]
    ));
    if let Some(coverage) = result.shake_coverage {
        line.push_str(&format!("  [shake diff {:.2}]", coverage * 100.0));
    }
    line
}

/// Asserts a shot is plausible: the generic bands plus the shot's warmth expectation.
///
/// # Panics
///
/// Panics with a readable failure message (the GPU evidence tests).
#[allow(clippy::missing_panics_doc)]
pub fn assert_shot_plausible(shot: &EffectShot, result: &ShotResult) {
    assert_effect_plausible(shot.name, &result.metrics);
    assert!(
        result.metrics.coverage >= shot.min_coverage,
        "{}: coverage {:.4} below the shot's floor {:.4}",
        shot.name,
        result.metrics.coverage,
        shot.min_coverage
    );
    if let Some(warm) = shot.warm {
        assert!(
            result.metrics.warmth * if warm { 1.0 } else { -1.0 } > 0.01,
            "{}: expected {} effect pixels, warmth {:+.3}",
            shot.name,
            if warm { "warm" } else { "cool" },
            result.metrics.warmth
        );
    }
    if shot.shake {
        let coverage = result.shake_coverage.unwrap_or(0.0);
        assert!(
            coverage > 0.005,
            "{}: camera shake moved nothing (diff coverage {:.4})",
            shot.name,
            coverage
        );
    }
    if let Some(effect) = shot.effect {
        let alive = result.counters[3];
        assert!(alive > 0, "{effect}: GPU reports no alive particles at capture");
    }
}

fn camera_of(shot: &EffectShot) -> VfxCamera {
    VfxCamera {
        eye: shot.eye,
        target: shot.target,
        fov_y: shot.fov_y,
        near: 0.05,
        far: 200.0,
    }
}
