// SPDX-License-Identifier: MPL-2.0
//! GPU checks for the crash effects (`regia_vfx::crash`, ADR-1398, ADR-1414) on the offscreen
//! harness, at the showcase scale (0.25 kt): the same crash renders byte-identical frames on two
//! runs, the fireball covers the screen extent its size predicts from 1.2 km without clipping to
//! a white disc and its fire is gone by 6 s, and from shot 4's camera (3.3 km) the cap stands
//! 800–1500 m high after 60 s.
//!
//! Lavapipe (or any Vulkan adapter). Set `REGIA_CRASH_SHEETS=<dir>` to also write the frames
//! as PNGs (evidence sheets).

use std::path::PathBuf;
use std::sync::OnceLock;

use regia_core::math::Vec3;
use regia_vfx::crash::{CrashBurst, CrashScale, CrashTier, GroundClass, ensure_crash_effects, spawn_crash};
use regia_vfx::offscreen::VfxHarness;
use regia_vfx::scene::VfxLibrary;
use regia_vfx::sim::LodInput;
use regia_vfx::{VfxCamera, VfxProfile, VfxScene};

fn device() -> &'static regia_render::RenderDevice {
    static DEVICE: OnceLock<regia_render::RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    })
}

fn profile(tier: CrashTier) -> VfxProfile {
    let mut p = match tier {
        CrashTier::Full => VfxProfile::desktop_high(),
        CrashTier::Lite => VfxProfile::mobile_high(),
    };
    // Small pools keep lavapipe fast; the crash needs well under these.
    p.particle_capacity = p.particle_capacity.min(16_384);
    // The hash contract is the render-out one (desktop profiles sort alpha). Mobile's unsorted
    // alpha list follows the GPU's atomic append order, which is not stable between runs.
    p.sort = true;
    p.soft_particle_res = regia_vfx::gpu::SoftParticleRes::Full;
    p
}

const W: u32 = 320;
const H: u32 = 180;
const SCALE: CrashScale = CrashScale::SHOWCASE;
/// Shot 4's camera relative to the impact site (3.3 km south-west, 248 m above it).
const SHOT4: Vec3 = Vec3::new(-2452.0, 247.5, -2208.0);

/// Shot 4's camera, aimed 450 m above the crash.
fn shot4() -> VfxCamera {
    VfxCamera {
        eye: SHOT4,
        target: Vec3::new(0.0, 450.0, 0.0),
        fov_y: 26.0f32.to_radians(),
        near: 1.0,
        far: 20_000.0,
    }
}

/// A level camera 1.2 km away at 100 m, aimed at the fireball.
fn near() -> VfxCamera {
    VfxCamera {
        eye: Vec3::new(0.0, 100.0, 1_200.0),
        target: Vec3::new(0.0, 100.0, 0.0),
        fov_y: 30.0f32.to_radians(),
        near: 1.0,
        far: 20_000.0,
    }
}

/// Which layers a test fires.
#[derive(Clone, Copy)]
enum Layers {
    /// Every layer of the showcase crash on snow.
    All,
    /// The fireball and its flash only.
    FireballOnly,
}

/// Plays the showcase crash (snow) at the origin with `dt` steps in a 2 m/s wind and returns the
/// frame at each of `times` (seconds, increasing); `fire` `None` renders the backdrop only.
fn play(tier: CrashTier, fire: Option<Layers>, cam: &VfxCamera, times: &[f32], dt: f32) -> Vec<Vec<u8>> {
    let mut scene = VfxScene::new(VfxLibrary::new());
    ensure_crash_effects(&mut scene, SCALE, tier, Some(Vec3::new(2.0, 0.0, 0.0)));
    if let Some(layers) = fire {
        let crash = match layers {
            Layers::All => CrashBurst {
                ground: GroundClass::Snow,
                scale: SCALE,
                ..CrashBurst::default()
            },
            Layers::FireballOnly => CrashBurst {
                scale: SCALE,
                dust: false,
                ejecta: false,
                column: false,
                fires: false,
                ..CrashBurst::default()
            },
        };
        let fired = spawn_crash(&mut scene, &crash);
        assert!(!fired.is_empty(), "{fired:?}");
    }
    let mut harness = VfxHarness::new(device(), profile(tier)).expect("harness");
    let mut out = Vec::new();
    let mut t = 0.0f32;
    for &want in times {
        while t + 1e-4 < want {
            t += dt;
            scene.update(dt, &LodInput { camera_pos: cam.eye }, 16_384);
            scene.upload(harness.gpu());
            // Every frame at the test size: the harness reallocates its targets on a resize.
            let frame = harness.render(device(), cam, W, H, dt).expect("frame");
            if t + 1e-4 >= want {
                out.push(frame.pixels);
            }
        }
    }
    out
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
    })
}

/// Rows and columns of the pixels that differ from the backdrop: `(top, bottom, left, right,
/// count)`.
fn changed_box(with: &[u8], without: &[u8]) -> Option<(u32, u32, u32, u32, u32)> {
    let mut b: Option<(u32, u32, u32, u32, u32)> = None;
    for (i, (a, z)) in with
        .as_chunks::<4>()
        .0
        .iter()
        .zip(without.as_chunks::<4>().0)
        .enumerate()
    {
        if !(0..3).any(|c| a[c].abs_diff(z[c]) > 8) {
            continue;
        }
        let (x, y) = (i as u32 % W, i as u32 / W);
        b = Some(match b {
            None => (y, y, x, x, 1),
            Some((t, bo, l, r, n)) => (t.min(y), bo.max(y), l.min(x), r.max(x), n + 1),
        });
    }
    b
}

/// World height where the view ray through pixel row `row` (centre column) meets the vertical
/// plane through the crash that faces the camera.
fn row_height(cam: &VfxCamera, row: u32) -> f32 {
    let f = (cam.target - cam.eye).normalize();
    let r = f.cross(Vec3::Y).normalize();
    let u = r.cross(f);
    let ndc_y = 1.0 - 2.0 * (row as f32 + 0.5) / H as f32;
    let dir = f + u * (ndc_y * (cam.fov_y * 0.5).tan());
    let n = Vec3::new(cam.eye.x, 0.0, cam.eye.z).normalize();
    let t = -cam.eye.dot(n) / dir.dot(n);
    cam.eye.y + t * dir.y
}

fn write_sheet(name: &str, pixels: &[u8]) {
    let Some(dir) = regia_core::brand::env_var_os("REGIA_CRASH_SHEETS").map(PathBuf::from) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{name}.png"));
    let tmp = dir.join(format!("{name}.png.{}.tmp", std::process::id()));
    let Ok(file) = std::fs::File::create(&tmp) else {
        return;
    };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let ok = enc
        .write_header()
        .and_then(|mut w| w.write_image_data(pixels))
        .is_ok();
    if ok {
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Two runs of the same crash give byte-identical frames (the render-out hash contract), on
/// both tiers, from shot 4's camera.
#[test]
fn crash_frames_are_hash_stable_across_two_runs() {
    let times = [0.6, 3.0, 8.0];
    for tier in CrashTier::ALL {
        let a = play(tier, Some(Layers::All), &shot4(), &times, 1.0 / 60.0);
        let b = play(tier, Some(Layers::All), &shot4(), &times, 1.0 / 60.0);
        assert_eq!(a.len(), times.len());
        for ((fa, fb), t) in a.iter().zip(&b).zip(times) {
            write_sheet(&format!("crash_{}_shot4_{t:.1}s", tier.name()), fa);
            assert_eq!(
                fnv(fa),
                fnv(fb),
                "{} crash at {t} s differs between two runs",
                tier.name()
            );
        }
        println!(
            "crash {} hashes: {:?}",
            tier.name(),
            a.iter().map(|f| format!("{:016x}", fnv(f))).collect::<Vec<_>>()
        );
    }
}

/// From 1.2 km at 1.2 s the fireball alone is about 150–250 m across (with its haze and soot)
/// and does not clip into a white disc; by 6 s no fire-orange is left.
#[test]
fn fireball_extent_matches_its_size_and_the_fire_is_gone_by_6_s() {
    let cam = near();
    let mpp = 2.0 * 1_200.0 * (cam.fov_y * 0.5).tan() / H as f32;
    for tier in CrashTier::ALL {
        let times = [1.2, 6.0];
        let with = play(tier, Some(Layers::FireballOnly), &cam, &times, 1.0 / 60.0);
        let without = play(tier, None, &cam, &times, 1.0 / 60.0);
        write_sheet(&format!("crash_{}_near_1.2s", tier.name()), &with[0]);
        write_sheet(&format!("crash_{}_near_6.0s", tier.name()), &with[1]);
        let (top, bottom, left, right, n) = changed_box(&with[0], &without[0]).expect("the crash shows");
        let half_width = (right - left + 1) as f32 * mpp * 0.5;
        assert!(
            (60.0..=160.0).contains(&half_width),
            "{}: fireball {half_width} m half-width ({left}..{right}, {top}..{bottom}, {n} px)",
            tier.name()
        );
        let clipped = with[0]
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] >= 254 && p[1] >= 254 && p[2] >= 254)
            .count();
        assert!(
            clipped * 50 < n as usize,
            "{}: {clipped} of {n} changed pixels clip to white",
            tier.name()
        );
        let orange = with[1]
            .as_chunks::<4>()
            .0
            .iter()
            .zip(without[1].as_chunks::<4>().0)
            .filter(|(a, z)| {
                let hot = a[0] > 200 && a[1] > 90 && a[2] < 90;
                hot && a[0].abs_diff(z[0]) > 30
            })
            .count();
        assert!(orange < 40, "{}: {orange} fire-orange pixels at 6 s", tier.name());
    }
}

/// Fire orange on a display pixel: HSV hue 8–50°, saturation 0.35 and up, value 0.3 and up
/// (the mask the showcase frames are measured with, ADR-1498).
fn is_orange(p: &[u8; 4]) -> bool {
    let [r, g, b] = [p[0], p[1], p[2]].map(|c| f32::from(c) / 255.0);
    let hi = r.max(g).max(b);
    let lo = r.min(g).min(b);
    // Red the largest channel (hue within ±60° of red), bright and saturated enough.
    if r < g || r < b || hi < 0.3 || (hi - lo) / hi < 0.35 {
        return false;
    }
    let hue = 60.0 * (g - b) / (hi - lo);
    (8.0..=50.0).contains(&hue)
}

/// ADR-1498, what is seen: from 1.2 km at 1.25 s the fireball's orange covers the area of a disc
/// 150–250 m across (equivalent diameter) on both tiers, not the 60–90 m flat sprite the
/// shot-4 frames showed before.
#[test]
fn the_seen_fireball_from_1_2_km_is_150_to_250_m_across() {
    let cam = near();
    let mpp = 2.0 * 1_200.0 * (cam.fov_y * 0.5).tan() / H as f32;
    for tier in CrashTier::ALL {
        let with = play(tier, Some(Layers::FireballOnly), &cam, &[1.25], 1.0 / 60.0);
        let without = play(tier, None, &cam, &[1.25], 1.0 / 60.0);
        write_sheet(&format!("crash_{}_near_1.25s", tier.name()), &with[0]);
        let orange = with[0]
            .as_chunks::<4>()
            .0
            .iter()
            .zip(without[0].as_chunks::<4>().0)
            .filter(|(a, z)| is_orange(a) && !is_orange(z))
            .count();
        let diameter = 2.0 * mpp * (orange as f32 / std::f32::consts::PI).sqrt();
        println!(
            "{}: {orange} orange px, {diameter:.0} m equivalent diameter",
            tier.name()
        );
        assert!(
            (150.0..=250.0).contains(&diameter),
            "{}: {orange} orange px = {diameter} m",
            tier.name()
        );
    }
}

/// The distance test: from shot 4's camera (3.3 km) the cap's top row converts to 800–1500 m
/// after 60 s, on both tiers.
#[test]
fn from_shot_4_the_cap_stands_800_to_1500_m_after_60_s() {
    let cam = shot4();
    for tier in CrashTier::ALL {
        let dt = 1.0 / 20.0;
        let with = play(tier, Some(Layers::All), &cam, &[60.0], dt);
        let without = play(tier, None, &cam, &[60.0], dt);
        write_sheet(&format!("crash_{}_shot4_60s", tier.name()), &with[0]);
        let (top, _, left, right, n) = changed_box(&with[0], &without[0]).expect("the column shows");
        let height = row_height(&cam, top);
        assert!(
            (800.0..=1_500.0).contains(&height),
            "{}: cap top at {height} m (row {top}, columns {left}..{right}, {n} px)",
            tier.name()
        );
    }
}
