// SPDX-License-Identifier: MPL-2.0
//! Atmosphere-aware effects on the GPU: airless debris flies on ballistic arcs under the
//! level's gravity (`regia_vfx::medium`).
//!
//! Lavapipe (or any Vulkan adapter).

use std::sync::OnceLock;

use regia_core::math::Vec3;
use regia_vfx::format::{
    BlendMode, InitModule, RangeDef, ShapeDef, SpawnMode, SpriteId, VelocityDef, VelocityMode,
};
use regia_vfx::medium::{VfxMedium, ballistic_position};
use regia_vfx::offscreen::VfxHarness;
use regia_vfx::sim::LodInput;
use regia_vfx::{VfxCamera, VfxProfile, VfxScene};

fn device() -> &'static regia_render::RenderDevice {
    static DEVICE: OnceLock<regia_render::RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    })
}

fn profile() -> VfxProfile {
    VfxProfile {
        particle_capacity: 2_048,
        decal_capacity: 8,
        sort: true,
        light_budget: 4,
        screen_floor_px: 0.0,
        smoke_budget: 2_048,
        soft_particle_res: regia_vfx::gpu::SoftParticleRes::Full,
        event_capacity: 16,
        child_capacity: 16,
    }
}

/// Centroid (px) of the pixels that differ from the backdrop.
fn centroid(frame: &[u8], back: &[u8], width: u32, height: u32) -> Option<(f32, f32)> {
    let (mut sx, mut sy, mut n) = (0.0f64, 0.0f64, 0u32);
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            let d: i32 = (0..3)
                .map(|c| (i32::from(frame[i + c]) - i32::from(back[i + c])).abs())
                .sum();
            if d > 60 {
                sx += f64::from(x);
                sy += f64::from(y);
                n += 1;
            }
        }
    }
    (n > 0).then(|| ((sx / f64::from(n)) as f32, (sy / f64::from(n)) as f32))
}

fn project(cam: &VfxCamera, p: Vec3, width: u32, height: u32) -> (f32, f32) {
    let (_, _, view_proj) = cam.matrices(width as f32 / height as f32);
    let clip = view_proj * p.extend(1.0);
    let ndc = clip.truncate() / clip.w;
    (
        (ndc.x * 0.5 + 0.5) * width as f32,
        (1.0 - (ndc.y * 0.5 + 0.5)) * height as f32,
    )
}

/// The airless explosion's ejecta emitter, reduced to one bright grain launched at a known
/// velocity: its gravity and drag are the preset's, so the flight tests the preset and the GPU
/// integrator together.
#[test]
fn airless_ejecta_fly_ballistic_arcs_under_the_level_gravity() {
    let g = 1.315;
    let medium = VfxMedium::airless(g);
    let mut def = regia_vfx::showcase::explosion_in(&medium);
    let mut probe = def
        .emitters
        .iter()
        .find(|e| e.name == "ejecta")
        .cloned()
        .expect("ejecta emitter");
    assert_eq!(probe.update.gravity, (0.0, -g, 0.0));
    assert_eq!(probe.update.drag, 0.0);
    let speed = 6.0f32;
    // World launch direction: up and to +X. The emitter's local frame for forward +Z is
    // right = forward × up = −X, so the local direction is (−1, 1, 0).
    let dir = Vec3::new(1.0, 1.0, 0.0).normalize();
    probe.delay = 0.0;
    probe.spawn.mode = SpawnMode::Burst(1);
    probe.spawn.lifetime = RangeDef::from(30.0);
    probe.init = InitModule {
        shape: ShapeDef::Point,
        velocity: VelocityDef {
            mode: VelocityMode::Directional {
                dir: (-dir.x, dir.y, dir.z),
                spread: 0.0,
                speed: RangeDef::from(speed),
            },
            inherit: 0.0,
        },
        size: RangeDef::from(0.5),
        rotation: RangeDef::from(0.0),
        spin: RangeDef::from(0.0),
        color: probe.init.color.clone(),
    };
    probe.update.alpha_over_life = Vec::new();
    probe.update.size_over_life = Vec::new();
    probe.update.depth_collision = None;
    probe.render.sprite = SpriteId::HardDisc;
    probe.render.blend = BlendMode::Additive;
    probe.render.lit = false;
    probe.render.emissive = 4.0;
    probe.render.softness = 0.0;
    def.emitters = vec![probe];
    def.name = "probe".into();

    let mut lib = regia_vfx::scene::VfxLibrary::new();
    lib.add(def).expect("probe def");
    let mut scene = VfxScene::new(lib);
    let origin = Vec3::new(-6.0, 1.0, 0.0);
    scene.spawn("probe", origin, Vec3::Z, 1).expect("spawn");

    const W: u32 = 320;
    const H: u32 = 200;
    // Side view of the arc (apex ≈ 6.8 m above the origin, range ≈ 27 m at landing).
    let cam = VfxCamera {
        eye: Vec3::new(6.0, 5.0, 40.0),
        target: Vec3::new(6.0, 5.0, 0.0),
        fov_y: 50.0_f32.to_radians(),
        near: 0.1,
        far: 200.0,
    };
    let mut back_h = VfxHarness::new(device(), profile()).expect("harness");
    back_h.set_bloom(false);
    let back = back_h.render(device(), &cam, W, H, 1.0 / 60.0).expect("backdrop");
    let mut harness = VfxHarness::new(device(), profile()).expect("harness");
    harness.set_bloom(false);
    let dt = 1.0 / 60.0;
    let lod = LodInput { camera_pos: cam.eye };
    let mut t = 0.0f32;
    let mut worst = 0.0f32;
    let mut checked = 0;
    let earth_gap_min = {
        // At 2 s Earth gravity would be (9.81 − 1.315)·2²/2 = 17 m lower: far off this frame.
        let a = ballistic_position(origin, dir * speed, g, 2.0);
        let b = ballistic_position(origin, dir * speed, 9.81, 2.0);
        (a - b).length()
    };
    assert!(earth_gap_min > 10.0);
    for frame in 0..(4.2 / dt) as u32 {
        scene.update(dt, &lod, 2_048);
        scene.upload(harness.gpu());
        let shot = harness.render(device(), &cam, W, H, dt).expect("frame");
        t += dt;
        if frame % 30 == 29 {
            let Some((cx, cy)) = centroid(&shot.pixels, &back.pixels, W, H) else {
                panic!("probe not visible at {t:.2} s");
            };
            // The particle is spawned this frame and integrated from the next: allow one step.
            let expect = ballistic_position(origin, dir * speed, g, t);
            let expect_prev = ballistic_position(origin, dir * speed, g, t - dt);
            let (ex, ey) = project(&cam, expect, W, H);
            let (px, py) = project(&cam, expect_prev, W, H);
            let err = ((cx - ex).powi(2) + (cy - ey).powi(2))
                .sqrt()
                .min(((cx - px).powi(2) + (cy - py).powi(2)).sqrt());
            // 1 px at this distance is about 9 cm.
            assert!(
                err < 3.0,
                "t {t:.2} s: probe at ({cx:.1}, {cy:.1}) px, ballistic ({ex:.1}, {ey:.1}) px"
            );
            worst = worst.max(err);
            checked += 1;
        }
    }
    assert!(checked >= 7, "{checked} samples");
    println!("ballistic: worst error {worst:.2} px over {checked} samples");
}
