// SPDX-License-Identifier: MPL-2.0
//! GPU checks for the round-7 weapon effects: a tracer reads on a bright, sunlit ground without
//! clipping it and keeps its colour, and the hover washes and hits render in every medium.
//!
//! Lavapipe (or any Vulkan adapter).

use std::sync::OnceLock;

use regia_core::math::Vec3;
use regia_vfx::format::{BlendMode, VfxSystemDef};
use regia_vfx::gpu::VfxLighting;
use regia_vfx::offscreen::{OffscreenFrame, VfxHarness};
use regia_vfx::sim::LodInput;
use regia_vfx::{VfxCamera, VfxMedium, VfxProfile, VfxScene};

fn device() -> &'static regia_render::RenderDevice {
    static DEVICE: OnceLock<regia_render::RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    })
}

fn profile() -> VfxProfile {
    profile_at(regia_vfx::gpu::SoftParticleRes::Full)
}

fn profile_at(res: regia_vfx::gpu::SoftParticleRes) -> VfxProfile {
    VfxProfile {
        particle_capacity: 8_192,
        decal_capacity: 16,
        sort: true,
        light_budget: 4,
        screen_floor_px: 1.5,
        smoke_budget: 8_192,
        soft_particle_res: res,
        event_capacity: 16,
        child_capacity: 16,
    }
}

/// A bright, sunlit ground: Europa's ice at midday is near white on screen.
fn bright(harness: &mut VfxHarness) {
    harness.set_lighting(VfxLighting {
        sun_color: [6.0, 6.0, 6.0],
        ambient: 1.0,
        ..VfxLighting::default()
    });
}

const W: u32 = 320;
const H: u32 = 180;

fn camera() -> VfxCamera {
    VfxCamera {
        eye: Vec3::new(0.0, 7.0, 7.0),
        target: Vec3::new(0.0, 0.0, 0.0),
        fov_y: 60.0_f32.to_radians(),
        near: 0.05,
        far: 250.0,
    }
}

/// Plays `name` (spawned at `at` facing `forward`) for `seconds` and returns the last frame;
/// `None` plays nothing (the backdrop).
fn play(
    defs: &[VfxSystemDef],
    medium: VfxMedium,
    spawn: Option<(&str, Vec3, Vec3)>,
    seconds: f32,
) -> OffscreenFrame {
    play_at(
        defs,
        medium,
        spawn,
        seconds,
        regia_vfx::gpu::SoftParticleRes::Full,
    )
}

fn play_at(
    defs: &[VfxSystemDef],
    medium: VfxMedium,
    spawn: Option<(&str, Vec3, Vec3)>,
    seconds: f32,
    res: regia_vfx::gpu::SoftParticleRes,
) -> OffscreenFrame {
    let mut lib = regia_vfx::scene::VfxLibrary::new();
    for def in defs {
        lib.add(def.clone()).expect("def");
    }
    let mut scene = VfxScene::new(lib);
    scene.set_medium(medium);
    if let Some((name, at, forward)) = spawn {
        scene.spawn(name, at, forward, 7).expect("spawn");
    }
    let mut harness = VfxHarness::new(device(), profile_at(res)).expect("harness");
    bright(&mut harness);
    let cam = camera();
    let dt = 1.0 / 60.0;
    let steps = (seconds / dt).round() as u32;
    let mut frame = None;
    for _ in 0..steps.max(1) {
        scene.update(dt, &LodInput { camera_pos: cam.eye }, 8_192);
        scene.upload(harness.gpu());
        frame = Some(harness.render(device(), &cam, W, H, dt).expect("frame"));
    }
    frame.expect("a frame")
}

/// (covered, clipped, mean chroma change) of `with` against `without`.
fn read(with: &[u8], without: &[u8]) -> (u32, u32, f32) {
    let mut covered = 0u32;
    let mut clipped = 0u32;
    let mut chroma = 0.0f32;
    for (a, b) in with.as_chunks::<4>().0.iter().zip(without.as_chunks::<4>().0) {
        if !(0..3).any(|c| a[c].abs_diff(b[c]) > 6) {
            continue;
        }
        covered += 1;
        let white = |p: &[u8; 4]| p[0] >= 250 && p[1] >= 250 && p[2] >= 250;
        if white(a) && !white(b) {
            clipped += 1;
        }
        let f = |p: &[u8; 4]| {
            [
                f32::from(p[0]) / 255.0,
                f32::from(p[1]) / 255.0,
                f32::from(p[2]) / 255.0,
            ]
        };
        let (pa, pb) = (f(a), f(b));
        let ma = (pa[0] + pa[1] + pa[2]) / 3.0;
        let mb = (pb[0] + pb[1] + pb[2]) / 3.0;
        chroma += (0..3)
            .map(|c| ((pa[c] - ma) - (pb[c] - mb)).powi(2))
            .sum::<f32>()
            .sqrt();
    }
    (covered, clipped, chroma / covered.max(1) as f32)
}

/// The previous tracer: an additive `#ff8a22` core at emissive 4.
fn legacy_tracer() -> VfxSystemDef {
    let mut def = regia_vfx::showcase::tracer();
    def.name = "tracer_legacy".into();
    for e in &mut def.emitters {
        e.render.blend = BlendMode::Additive;
        e.render.emissive = 4.0;
        e.init.color[0].color = "#ff8a22".into();
    }
    def
}

#[test]
fn tracer_reads_on_bright_ground_without_clipping_it() {
    let defs = vec![regia_vfx::showcase::tracer(), legacy_tracer()];
    let earth = VfxMedium::earth();
    let back = play(&defs, earth, None, 0.35);
    // Fired across the frame (+X), a metre off the ground.
    let at = Vec3::new(-6.0, 1.0, 0.0);
    let new = play(&defs, earth, Some(("tracer", at, -Vec3::X)), 0.35);
    let old = play(&defs, earth, Some(("tracer_legacy", at, -Vec3::X)), 0.35);
    let (n_cov, n_clip, n_chroma) = read(&new.pixels, &back.pixels);
    let (o_cov, o_clip, o_chroma) = read(&old.pixels, &back.pixels);
    eprintln!(
        "tracer on bright ground: new covers {n_cov} px, clips {n_clip}, chroma {n_chroma:.3}; \
         old covers {o_cov} px, clips {o_clip}, chroma {o_chroma:.3}"
    );
    assert!(n_cov > 40, "the tracer must be visible: {n_cov} px");
    assert!(
        (n_clip as f32) < 0.1 * n_cov as f32,
        "the tracer clips the ground to white: {n_clip}/{n_cov}"
    );
    // The harness ground is a sunlit green checker, not white ice: neither core clips here, so
    // the before/after comparison lives on DO27's bright ice (`lookdev combat_shots`, the
    // `daylight_read` metrics). Here the tracer must keep a strong colour of its own.
    assert!(
        n_chroma > 0.2,
        "the tracer must put color on the ground: {n_chroma}"
    );
    assert!(
        o_cov > 0 && o_clip <= o_cov && o_chroma >= 0.0,
        "the previous tracer rendered nothing"
    );
}

#[test]
fn hover_washes_and_hits_render_in_every_medium() {
    for medium in [VfxMedium::earth(), VfxMedium::mars(), VfxMedium::airless(1.315)] {
        let mut defs = Vec::new();
        for surface in regia_vfx::HoverSurface::ALL {
            defs.push(regia_vfx::hover::hover_wash(surface));
            defs.push(regia_vfx::hover::hover_wash_in(surface, &medium));
        }
        for base in ["hit_metal", "hit_concrete", "hit_glass"] {
            for m in [VfxMedium::earth(), medium] {
                if let Some(d) = regia_vfx::combat::combat_effect_in(base, &m) {
                    defs.push(d);
                }
            }
        }
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs.dedup_by(|a, b| a.name == b.name);
        let back = play(&defs, medium, None, 0.5);
        let surface = regia_vfx::HoverSurface::Auto.resolve(&medium);
        let wash = surface.effect().expect("wash");
        let washed = play(&defs, medium, Some((wash, Vec3::ZERO, Vec3::Y)), 1.5);
        let (cov, _, _) = read(&washed.pixels, &back.pixels);
        assert!(
            cov > 200,
            "{wash} in {:?} covers only {cov} px",
            medium.atmosphere
        );
        for base in ["hit_metal", "hit_concrete", "hit_glass"] {
            let hit = play(
                &defs,
                medium,
                Some((base, Vec3::new(0.0, 1.0, 0.0), Vec3::Z)),
                0.05,
            );
            let (cov, _, _) = read(&hit.pixels, &back.pixels);
            assert!(cov > 10, "{base} in {:?} covers only {cov} px", medium.atmosphere);
        }
    }
}

/// The low-resolution soft layer: sharp emitters (a tracer) are pixel-identical to full
/// resolution, and a soft wash keeps its coverage and colour within a small error.
#[test]
fn soft_layer_keeps_sharp_emitters_full_res_and_soft_ones_close() {
    use regia_vfx::gpu::SoftParticleRes;
    let earth = VfxMedium::earth();
    let defs = vec![
        regia_vfx::showcase::tracer(),
        regia_vfx::hover::hover_wash(regia_vfx::HoverSurface::Dust),
    ];
    let at = Vec3::new(-6.0, 1.0, 0.0);
    let full = play_at(
        &defs,
        earth,
        Some(("tracer", at, -Vec3::X)),
        0.35,
        SoftParticleRes::Full,
    );
    for res in [SoftParticleRes::Half, SoftParticleRes::Quarter] {
        let low = play_at(&defs, earth, Some(("tracer", at, -Vec3::X)), 0.35, res);
        assert_eq!(
            full.pixels, low.pixels,
            "{res:?}: a tracer is not soft and must not change"
        );
    }
    let back = play(&defs, earth, None, 1.2);
    let wash_full = play_at(
        &defs,
        earth,
        Some(("hover_wash_dust", Vec3::ZERO, Vec3::Y)),
        1.2,
        SoftParticleRes::Full,
    );
    let (cov_full, _, _) = read(&wash_full.pixels, &back.pixels);
    for (res, tol) in [(SoftParticleRes::Half, 0.2f32), (SoftParticleRes::Quarter, 0.35)] {
        let wash = play_at(
            &defs,
            earth,
            Some(("hover_wash_dust", Vec3::ZERO, Vec3::Y)),
            1.2,
            res,
        );
        let (cov, _, _) = read(&wash.pixels, &back.pixels);
        let rel = (cov as f32 - cov_full as f32).abs() / cov_full.max(1) as f32;
        let mean_diff: f32 = wash
            .pixels
            .iter()
            .zip(&wash_full.pixels)
            .map(|(a, b)| f32::from(a.abs_diff(*b)))
            .sum::<f32>()
            / wash.pixels.len() as f32;
        eprintln!("{res:?}: coverage {cov} vs {cov_full} ({rel:.3}), mean byte diff {mean_diff:.2}");
        assert!(rel < tol, "{res:?}: the wash's coverage moved {rel}");
        assert!(
            mean_diff < 3.0,
            "{res:?}: the wash changed too much ({mean_diff})"
        );
    }
}

/// Sorting only the live prefix of the alpha list, and updating only the ring window spawned
/// within the longest lifetime, give the same frame as sorting and updating the whole pool.
#[test]
fn bounded_alpha_sort_draws_the_same_frame_as_the_full_sort() {
    let mars = VfxMedium::mars();
    let defs = vec![
        regia_vfx::hover::hover_wash(regia_vfx::HoverSurface::Dust),
        regia_vfx::hover::hover_wash_in(regia_vfx::HoverSurface::Dust, &mars),
        regia_vfx::combat::hit_metal(&VfxMedium::earth()),
    ];
    let run = |full_sort: bool| {
        let mut lib = regia_vfx::scene::VfxLibrary::new();
        for def in &defs {
            lib.add(def.clone()).expect("def");
        }
        let mut scene = VfxScene::new(lib);
        scene.set_medium(mars);
        scene
            .spawn("hover_wash_dust", Vec3::ZERO, Vec3::Y, 7)
            .expect("spawn");
        let mut harness = VfxHarness::new(device(), profile()).expect("harness");
        bright(&mut harness);
        let cam = camera();
        let dt = 1.0 / 60.0;
        let mut frame = None;
        let mut bound = 0;
        let mut window = (0, 0);
        for f in 0..300u32 {
            // Short-lived hits keep spawning and dying, so the ring window moves.
            if f % 20 == 0 {
                let _ = scene.spawn("hit_metal", Vec3::new(0.0, 1.0, 0.0), Vec3::Z, u64::from(f));
            }
            scene.update(dt, &LodInput { camera_pos: cam.eye }, 8_192);
            scene.upload(harness.gpu());
            if full_sort {
                harness.gpu().set_alpha_bound(u32::MAX);
                harness.gpu().set_max_lifetime(0.0);
            }
            bound = harness.gpu().sort_live_keys();
            frame = Some(harness.render(device(), &cam, W, H, dt).expect("frame"));
            window = harness.gpu().update_window();
        }
        (frame.expect("frame"), bound, window)
    };
    let (bounded, keys, window) = run(false);
    let (full, full_keys, full_window) = run(true);
    eprintln!("bitonic keys: bounded {keys}, full {full_keys}; update window {window:?} vs {full_window:?}");
    assert!(
        window.1 < full_window.1,
        "the update window must shrink ({window:?} vs {full_window:?})"
    );
    assert!(
        keys < full_keys,
        "the bound must shrink the sort ({keys} vs {full_keys})"
    );
    assert_eq!(bounded.pixels, full.pixels, "the bounded sort changed the frame");
}
