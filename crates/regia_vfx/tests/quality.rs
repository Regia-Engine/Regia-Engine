// SPDX-License-Identifier: MPL-2.0
//! GPU checks that fail on the broken DO27 look: a hard ground cut, an unlit blob, a popping
//! sort, a pink fireball, a frozen sprite, and a sub-pixel spark.
//!
//! Lavapipe (or any Vulkan adapter). The pool is a few thousand particles, not the desktop
//! 100k profile.

use std::sync::OnceLock;

use regia_core::math::Vec3;
use regia_render::{exposure_from_ev100, tonemap_agx};
use regia_vfx::format::{
    BlendMode, ColorKey, EmitterDef, FlipbookDef, FlipbookMode, InitModule, RangeDef, RenderMode,
    RenderModule, ShapeDef, SpawnMode, SpawnModule, SpriteId, SystemModule, UpdateModule, VFX_FORMAT_VERSION,
    VelocityDef, VelocityMode, VfxSystemDef,
};
use regia_vfx::offscreen::VfxHarness;
use regia_vfx::quality::{desktop_high_display, in_fire_family, restore_exposed};
use regia_vfx::sim::LodInput;
use regia_vfx::{VfxCamera, VfxProfile, VfxScene};

fn device() -> &'static regia_render::RenderDevice {
    static DEVICE: OnceLock<regia_render::RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    })
}

fn profile(floor: f32) -> VfxProfile {
    VfxProfile {
        particle_capacity: 2_048,
        decal_capacity: 8,
        sort: true,
        light_budget: 4,
        screen_floor_px: floor,
        smoke_budget: 2_048,
        soft_particle_res: regia_vfx::gpu::SoftParticleRes::Full,
        event_capacity: 16,
        child_capacity: 16,
    }
}

fn range(v: f32) -> RangeDef {
    RangeDef::from(v)
}

/// One burst billboard. `speed` along local +Z (instance forward is world +Z).
#[allow(clippy::too_many_arguments)]
fn burst(
    name: &str,
    color: &str,
    blend: BlendMode,
    sprite: SpriteId,
    softness: f32,
    lit: bool,
    emissive: f32,
    size: f32,
    speed_z: f32,
    flipbook: Option<FlipbookDef>,
    floor_px: f32,
) -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: name.into(),
        system: SystemModule {
            bounds: None,
            budget: 32,
            lod: None,
        },
        emitters: vec![EmitterDef {
            name: name.into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode: SpawnMode::Burst(1),
                lifetime: range(4.0),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Point,
                velocity: VelocityDef {
                    mode: VelocityMode::Directional {
                        dir: (0.0, 0.0, if speed_z < 0.0 { -1.0 } else { 1.0 }),
                        spread: 0.0,
                        speed: range(speed_z.abs()),
                    },
                    inherit: 0.0,
                },
                size: range(size),
                rotation: range(0.0),
                spin: range(0.0),
                color: vec![ColorKey {
                    t: 0.0,
                    color: color.into(),
                }],
            },
            update: UpdateModule::default(),
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite,
                flipbook,
                blend,
                softness,
                emissive,
                lit,
                self_shadow: 0.7,
                screen_floor_px: floor_px,
                ..RenderModule::default()
            },
        }],
    }
}

fn scene_of(defs: Vec<VfxSystemDef>) -> VfxScene {
    let mut lib = regia_vfx::scene::VfxLibrary::new();
    for def in defs {
        lib.add(def).expect("effect def");
    }
    VfxScene::new(lib)
}

fn camera(eye: Vec3, target: Vec3) -> VfxCamera {
    VfxCamera {
        eye,
        target,
        fov_y: 60.0_f32.to_radians(),
        near: 0.05,
        far: 250.0,
    }
}

fn shoot(
    harness: &mut VfxHarness,
    scene: &mut VfxScene,
    cam: &VfxCamera,
    width: u32,
    height: u32,
    dt: f32,
) -> regia_vfx::offscreen::OffscreenFrame {
    let lod = LodInput { camera_pos: cam.eye };
    scene.update(dt, &lod, 2_048);
    scene.upload(harness.gpu());
    harness.render(device(), cam, width, height, dt).expect("frame")
}

fn at(bytes: &[u8], i: usize) -> [u8; 4] {
    [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
}

fn luma_byte(p: [u8; 4]) -> f32 {
    (0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])) / 255.0
}

fn linear_luma(p: [u8; 4]) -> f32 {
    0.2126 * srgb_to_linear(p[0]) + 0.7152 * srgb_to_linear(p[1]) + 0.0722 * srgb_to_linear(p[2])
}

fn srgb_to_linear(b: u8) -> f32 {
    let c = f32::from(b) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Longest run of deltas sitting in the middle of the ramp, along one column.
fn mid_ramp_run(effect: &[u8], backdrop: &[u8], width: u32, height: u32, x: u32) -> usize {
    let mut delta = Vec::with_capacity(height as usize);
    let mut peak = 0.0f32;
    for y in 0..height {
        let i = ((y * width + x) * 4) as usize;
        let d = (luma_byte(at(effect, i)) - luma_byte(at(backdrop, i))).abs();
        peak = peak.max(d);
        delta.push(d);
    }
    if peak < 0.05 {
        return 0;
    }
    let mut best = 0usize;
    let mut run = 0usize;
    for d in delta {
        if d > peak * 0.15 && d < peak * 0.85 {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}

#[test]
fn soft_sprite_crossing_the_ground_has_no_hard_edge() {
    let cam = camera(Vec3::new(0.0, 1.6, 4.2), Vec3::new(0.0, 0.35, 0.0));
    const W: u32 = 160;
    const H: u32 = 120;
    let mut backdrop_h = VfxHarness::new(device(), profile(0.0)).expect("harness");
    let back = backdrop_h
        .render(device(), &cam, W, H, 1.0 / 60.0)
        .expect("backdrop");

    // The flat quad's authored softness (spherical billboards off; the spherical path has
    // its own test below).
    let mut soft_h = VfxHarness::new(device(), profile(0.0)).expect("harness");
    soft_h.gpu().set_volumetric_billboards(false);
    let mut scene = scene_of(vec![burst(
        "soft",
        "#ffffff",
        BlendMode::Alpha,
        SpriteId::HardDisc,
        1.6,
        false,
        1.0,
        2.2,
        0.0,
        None,
        0.0,
    )]);
    scene
        .spawn("soft", Vec3::new(0.0, 0.35, 0.0), Vec3::Z, 1)
        .expect("spawn");
    let soft = shoot(&mut soft_h, &mut scene, &cam, W, H, 1.0 / 60.0);

    // The flat quad (spherical billboards off): softness 0 is the hard cut.
    let mut hard_h = VfxHarness::new(device(), profile(0.0)).expect("harness");
    hard_h.gpu().set_volumetric_billboards(false);
    let mut hard_scene = scene_of(vec![burst(
        "hard",
        "#ffffff",
        BlendMode::Alpha,
        SpriteId::HardDisc,
        0.0,
        false,
        1.0,
        2.2,
        0.0,
        None,
        0.0,
    )]);
    hard_scene
        .spawn("hard", Vec3::new(0.0, 0.35, 0.0), Vec3::Z, 1)
        .expect("spawn");
    let hard = shoot(&mut hard_h, &mut hard_scene, &cam, W, H, 1.0 / 60.0);

    let x = W / 2;
    let soft_run = mid_ramp_run(&soft.pixels, &back.pixels, W, H, x);
    let hard_run = mid_ramp_run(&hard.pixels, &back.pixels, W, H, x);
    assert!(
        soft_run >= 4,
        "soft intersection ramp is {soft_run} px (a hard cut fails this)"
    );
    assert!(
        hard_run < soft_run && hard_run <= 2,
        "softness 0 should be a step (hard {hard_run}, soft {soft_run})"
    );
}

/// Spherical billboards (the default): a camera-facing puff fades into the ground over its
/// own depth even with no authored softness, and a bigger puff fades over more pixels.
#[test]
fn spherical_puff_meets_the_ground_over_its_own_depth() {
    let cam = camera(Vec3::new(0.0, 1.6, 4.2), Vec3::new(0.0, 0.35, 0.0));
    const W: u32 = 160;
    const H: u32 = 120;
    let mut backdrop_h = VfxHarness::new(device(), profile(0.0)).expect("harness");
    let back = backdrop_h
        .render(device(), &cam, W, H, 1.0 / 60.0)
        .expect("backdrop");
    let ramp = |size: f32| {
        let mut h = VfxHarness::new(device(), profile(0.0)).expect("harness");
        assert!(
            h.gpu().volumetric_billboards(),
            "spherical billboards are the default"
        );
        let mut scene = scene_of(vec![burst(
            "puff",
            "#ffffff",
            BlendMode::Alpha,
            SpriteId::HardDisc,
            0.0,
            false,
            1.0,
            size,
            0.0,
            None,
            0.0,
        )]);
        scene
            .spawn("puff", Vec3::new(0.0, 0.35, 0.0), Vec3::Z, 1)
            .expect("spawn");
        let frame = shoot(&mut h, &mut scene, &cam, W, H, 1.0 / 60.0);
        mid_ramp_run(&frame.pixels, &back.pixels, W, H, W / 2)
    };
    let small = ramp(1.1);
    let big = ramp(2.2);
    eprintln!("ground ramp: {small} px at 1.1 m, {big} px at 2.2 m");
    assert!(
        big >= 3,
        "a 2.2 m puff must meet the ground over a ramp, got {big} px"
    );
    assert!(
        big > small,
        "the ramp grows with the puff ({small} px at 1.1 m, {big} px at 2.2 m)"
    );
}

/// Mean linear (frame − backdrop) luma on each half of the sprite. Subtracting the backdrop
/// removes the sky gradient; a radially symmetric sprite then leaves only the shade.
fn side_deltas(frame: &[u8], back: &[u8], width: u32, height: u32) -> (f32, f32, u32, u32) {
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    let mut deltas = Vec::new();
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            let d = linear_luma(at(frame, i)) - linear_luma(at(back, i));
            if d.abs() > 0.015 {
                xs.push(x);
                ys.push(y);
                deltas.push(d);
            }
        }
    }
    assert!(xs.len() > 20, "lit puff covered {} px", xs.len());
    let x_split = (xs.iter().min().expect("bbox") + xs.iter().max().expect("bbox")) / 2;
    let mut left = 0.0f32;
    let mut left_n = 0u32;
    let mut right = 0.0f32;
    let mut right_n = 0u32;
    for ((x, _y), d) in xs.iter().zip(ys.iter()).zip(deltas) {
        if *x <= x_split {
            left += d;
            left_n += 1;
        } else {
            right += d;
            right_n += 1;
        }
    }
    assert!(left_n > 8 && right_n > 8, "sides {left_n}/{right_n}");
    (left / left_n as f32, right / right_n as f32, left_n, right_n)
}

#[test]
fn lit_puff_is_brighter_on_the_sun_side() {
    let cam = camera(Vec3::new(0.0, 1.5, 5.0), Vec3::new(0.0, 1.5, 0.0));
    const W: u32 = 160;
    const H: u32 = 120;
    // SoftDisc is radially symmetric, so a left/right gap is the Lambert term (Harris 2002),
    // not the puff noise. Ambient is low so the sun side is not flattened into the sky.
    let mut backdrop_h = VfxHarness::new(device(), profile(0.0)).expect("harness");
    backdrop_h.set_bloom(false);
    let back = backdrop_h
        .render(device(), &cam, W, H, 1.0 / 60.0)
        .expect("backdrop");
    let mut harness = VfxHarness::new(device(), profile(0.0)).expect("harness");
    harness.set_bloom(false);
    harness.set_lighting(regia_vfx::gpu::VfxLighting {
        sun_direction: [-1.0, 0.2, 0.05],
        sun_color: [1.05, 0.98, 0.85],
        ambient: 0.12,
        exposure: 1.0,
    });
    let mut scene = scene_of(vec![burst(
        "puff",
        "#f2efe8",
        BlendMode::Alpha,
        SpriteId::SoftDisc,
        0.0,
        true,
        1.0,
        2.4,
        0.0,
        None,
        0.0,
    )]);
    scene
        .spawn("puff", Vec3::new(0.0, 1.5, 0.0), Vec3::Z, 3)
        .expect("spawn");
    let frame = shoot(&mut harness, &mut scene, &cam, W, H, 1.0 / 60.0);
    let (left, right, _, _) = side_deltas(&frame.pixels, &back.pixels, W, H);
    assert!(
        left > right * 1.15 && left - right > 0.02,
        "sun on screen-left: lit {left:.3} should beat shadow {right:.3}"
    );

    // The bright side must follow the sun. A texture bias would stay put.
    harness.set_lighting(regia_vfx::gpu::VfxLighting {
        sun_direction: [1.0, 0.2, 0.05],
        sun_color: [1.05, 0.98, 0.85],
        ambient: 0.12,
        exposure: 1.0,
    });
    let flipped = shoot(&mut harness, &mut scene, &cam, W, H, 0.0);
    let (left_f, right_f, _, _) = side_deltas(&flipped.pixels, &back.pixels, W, H);
    assert!(
        right_f > left_f * 1.15 && right_f - left_f > 0.02,
        "sun on screen-right: lit {right_f:.3} should beat shadow {left_f:.3}"
    );
}

/// A rotated sprite is lit from the same screen side as an upright one: the shader turns the
/// sprite-space sphere normal by the sprite's rotation (a puff column with random rotations
/// used to be lit from a random side per puff, which flattened it).
#[test]
fn lit_puff_sun_side_ignores_sprite_rotation() {
    let cam = camera(Vec3::new(0.0, 1.5, 5.0), Vec3::new(0.0, 1.5, 0.0));
    const W: u32 = 160;
    const H: u32 = 120;
    let mut backdrop_h = VfxHarness::new(device(), profile(0.0)).expect("harness");
    backdrop_h.set_bloom(false);
    let back = backdrop_h
        .render(device(), &cam, W, H, 1.0 / 60.0)
        .expect("backdrop");
    for rotation in [std::f32::consts::FRAC_PI_2, 2.4] {
        let mut harness = VfxHarness::new(device(), profile(0.0)).expect("harness");
        harness.set_bloom(false);
        harness.set_lighting(regia_vfx::gpu::VfxLighting {
            sun_direction: [-1.0, 0.2, 0.05],
            sun_color: [1.05, 0.98, 0.85],
            ambient: 0.12,
            exposure: 1.0,
        });
        let mut def = burst(
            "puff",
            "#f2efe8",
            BlendMode::Alpha,
            SpriteId::SoftDisc,
            0.0,
            true,
            1.0,
            2.4,
            0.0,
            None,
            0.0,
        );
        def.emitters[0].init.rotation = range(rotation);
        let mut scene = scene_of(vec![def]);
        scene
            .spawn("puff", Vec3::new(0.0, 1.5, 0.0), Vec3::Z, 3)
            .expect("spawn");
        let frame = shoot(&mut harness, &mut scene, &cam, W, H, 1.0 / 60.0);
        let (left, right, _, _) = side_deltas(&frame.pixels, &back.pixels, W, H);
        assert!(
            left > right * 1.15 && left - right > 0.02,
            "rotation {rotation}: sun on screen-left, lit {left:.3} should beat shadow {right:.3}"
        );
    }
}

#[test]
fn alpha_sort_is_stable_and_crosses_once() {
    let cam = camera(Vec3::new(0.0, 1.0, 6.0), Vec3::new(0.0, 1.0, 0.0));
    const W: u32 = 96;
    const H: u32 = 64;
    let mut harness = VfxHarness::new(device(), profile(0.0)).expect("harness");
    let mut scene = scene_of(vec![
        burst(
            "red",
            "#ff2020",
            BlendMode::Alpha,
            SpriteId::HardDisc,
            0.0,
            false,
            1.0,
            1.4,
            -2.0,
            None,
            0.0,
        ),
        burst(
            "blue",
            "#2020ff",
            BlendMode::Alpha,
            SpriteId::HardDisc,
            0.0,
            false,
            1.0,
            1.4,
            2.0,
            None,
            0.0,
        ),
    ]);
    scene
        .spawn("red", Vec3::new(0.0, 1.0, 1.0), Vec3::Z, 1)
        .expect("red");
    scene
        .spawn("blue", Vec3::new(0.0, 1.0, -1.0), Vec3::Z, 2)
        .expect("blue");

    let mut winner = Vec::new();
    let mut previous = None;
    for _ in 0..16 {
        let frame = shoot(&mut harness, &mut scene, &cam, W, H, 0.05);
        if previous.is_none() {
            let again = shoot(&mut harness, &mut scene, &cam, W, H, 0.0);
            assert_eq!(
                frame.pixels, again.pixels,
                "two frames with no motion must not pop"
            );
        }
        let i = (((H / 2) * W + W / 2) * 4) as usize;
        let r = frame.pixels[i];
        let b = frame.pixels[i + 2];
        let who = if r > b.saturating_add(20) {
            "red"
        } else if b > r.saturating_add(20) {
            "blue"
        } else {
            "tie"
        };
        winner.push(who);
        previous = Some(frame.pixels);
    }
    let decided: Vec<&str> = winner.iter().copied().filter(|w| *w != "tie").collect();
    assert!(
        decided.contains(&"red") && decided.contains(&"blue"),
        "winners {winner:?}"
    );
    let mut flips = 0;
    for pair in decided.windows(2) {
        if pair[0] != pair[1] {
            flips += 1;
        }
    }
    assert_eq!(flips, 1, "crossing emitters flipped {flips} times: {winner:?}");
}

/// `post.wgsl` mode 3 blends toward the blurred chain with this weight (`mix`, not an add).
const RENDER_BLOOM_WEIGHT: f32 = regia_render::DEFAULT_BLOOM_INTENSITY;

fn linear_rgb(px: [u8; 4]) -> [f32; 3] {
    [
        srgb_to_linear(px[0]),
        srgb_to_linear(px[1]),
        srgb_to_linear(px[2]),
    ]
}

fn hdr_rgb(px: [f32; 4]) -> [f32; 3] {
    [px[0], px[1], px[2]]
}

fn channel_max(rgb: [f32; 3]) -> f32 {
    rgb[0].max(rgb[1]).max(rgb[2])
}

fn rec709(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

fn saturation(rgb: [f32; 3]) -> f32 {
    let maxc = channel_max(rgb).max(0.0);
    let minc = rgb[0].min(rgb[1]).min(rgb[2]);
    if maxc <= 1e-4 { 0.0 } else { (maxc - minc) / maxc }
}

/// A bloom pixel bright enough to be the glow, not a sky texel a tap barely warmed.
fn is_glow(rgb: [f32; 3]) -> bool {
    rec709(rgb) > 0.55 || channel_max(rgb) > 0.70
}

/// Added light: negative channels are the backdrop, not the bloom.
fn added_light(on: [f32; 3], off: [f32; 3]) -> [f32; 3] {
    [
        (on[0] - off[0]).max(0.0),
        (on[1] - off[1]).max(0.0),
        (on[2] - off[2]).max(0.0),
    ]
}

/// 3×3 mean. A stand-in for the Desktop High bloom pyramid (`post.wgsl` modes 1–2).
fn blur3(hdr: &[[f32; 4]], width: u32, height: u32, x: u32, y: u32) -> [f32; 3] {
    let mut sum = [0.0f32; 3];
    let mut n = 0.0f32;
    for dy in -1i32..=1 {
        for dx in -1i32..=1 {
            let xx = x as i32 + dx;
            let yy = y as i32 + dy;
            if xx < 0 || yy < 0 || xx >= width as i32 || yy >= height as i32 {
                continue;
            }
            let px = hdr[(yy as u32 * width + xx as u32) as usize];
            sum[0] += px[0];
            sum[1] += px[1];
            sum[2] += px[2];
            n += 1.0;
        }
    }
    [sum[0] / n, sum[1] / n, sum[2] / n]
}

/// Reads the pre-present HDR buffer and the display bytes the present pass wrote.
///
/// A pink core fails here. The present pass is not allowed to repaint hue. Desktop High
/// is the exposure round-trip, `mix(base, bloom, 0.04)`, `tonemap_agx` and
/// `display_look(1.2, 1.4)` — [`desktop_high_display`].
fn assert_fire_look(
    label: &str,
    on: &regia_vfx::offscreen::OffscreenFrame,
    off: &regia_vfx::offscreen::OffscreenFrame,
    width: u32,
    height: u32,
) {
    let hdr = on.hdr.as_ref().expect("hdr capture").as_chunks::<4>().0;
    let exposure = exposure_from_ev100(15.0);
    // Desktop High stores the divided colour in Rgba16Float, multiplies exposure back,
    // then blooms that image. The harness itself runs at exposure 1.
    let mut restored_hdr = vec![[0.0f32; 4]; hdr.len()];
    for (dst, px) in restored_hdr.iter_mut().zip(hdr.iter()) {
        let back = restore_exposed(hdr_rgb(*px), exposure);
        *dst = [back[0], back[1], back[2], px[3]];
    }
    let mut peak = 0.0f32;
    let mut hdr_n = 0u32;
    let mut hdr_bad = 0u32;
    let mut agx_bad = 0u32;
    let mut bloom_bad = 0u32;
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) as usize;
            let rgb = hdr_rgb(hdr[i]);
            peak = peak.max(channel_max(rgb));
            // Pre-present buffer. Bloom has not run. A pixel above 1 is the core
            // (or the sun disc, which is warm white and already in the family).
            if channel_max(rgb) <= 1.0 {
                continue;
            }
            hdr_n += 1;
            if !in_fire_family(rgb) {
                hdr_bad += 1;
            }
            let restored = hdr_rgb(restored_hdr[i]);
            if !in_fire_family(tonemap_agx(restored)) {
                agx_bad += 1;
            }
            let blur = blur3(&restored_hdr, width, height, x, y);
            if !in_fire_family(desktop_high_display(restored, blur)) {
                bloom_bad += 1;
            }
        }
    }
    assert!(peak > 1.0, "{label}: core must be HDR, peak {peak:.3}");
    assert!(hdr_n > 0, "{label}: no HDR core pixels");
    assert_eq!(
        hdr_bad, 0,
        "{label}: {hdr_bad}/{hdr_n} pre-present HDR pixels left the fire family"
    );
    assert_eq!(
        agx_bad, 0,
        "{label}: {agx_bad}/{hdr_n} pixels left the family after AgX"
    );
    assert_eq!(
        bloom_bad, 0,
        "{label}: {bloom_bad}/{hdr_n} pixels left the family after mix(base, bloom, {RENDER_BLOOM_WEIGHT}), AgX and the default look"
    );

    let mut bright = 0u32;
    let mut family = 0u32;
    for px in on.pixels.as_chunks::<4>().0 {
        let rgb = linear_rgb(*px);
        if channel_max(rgb) > 0.85 {
            bright += 1;
            if in_fire_family(rgb) {
                family += 1;
            }
        }
    }
    assert!(bright > 0, "{label}: no bright display pixels");
    assert_eq!(
        family, bright,
        "{label}: {family}/{bright} bright display pixels stayed in the fire family"
    );

    // The bloom adds fire-coloured light on top of the sky. A pale sum (saturation
    // under 0.22) is the sky not yet covered: 8-bit grey with a cool tilt of ~0.19.
    // That is not a pink halo. Where the tint is visible, the sum has to be in the
    // family, and the light that was added has to be in the family on every glow pixel.
    let mut halo_n = 0u32;
    let mut added_bad = 0u32;
    let mut hue_bad = 0u32;
    let mut max_gap = 0.0f32;
    for (on_px, off_px) in on
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(off.pixels.as_chunks::<4>().0)
    {
        let on_rgb = linear_rgb(*on_px);
        let off_rgb = linear_rgb(*off_px);
        let gap = linear_luma(*on_px) - linear_luma(*off_px);
        max_gap = max_gap.max(gap);
        if gap > 0.04 && is_glow(on_rgb) {
            halo_n += 1;
            // The displayed pixel is what the present pass wrote. A saturated one
            // outside the family is a pink or green halo, sun or no sun.
            if saturation(on_rgb) >= 0.22 && !in_fire_family(on_rgb) {
                hue_bad += 1;
            }
            // Added light is the particle's own colour only on a backdrop that is
            // not already near white. Over the sun, or over a premultiplied edge,
            // the red channel rises more than green because the sky was already
            // bright there. That remainder is not a pink halo when the pixel on
            // screen is a saturated fire colour. A pink add that only shows up as
            // a grey sum (saturation under 0.22) still fails here.
            let light = added_light(on_rgb, off_rgb);
            if channel_max(off_rgb) < 0.70
                && saturation(light) >= 0.22
                && !in_fire_family(light)
                && saturation(on_rgb) < 0.22
            {
                added_bad += 1;
            }
        }
    }
    assert!(
        halo_n > 0,
        "{label}: bloom did not move the picture (max linear gap {max_gap:.4})"
    );
    assert_eq!(
        added_bad, 0,
        "{label}: {added_bad}/{halo_n} glow pixels added light outside the fire family"
    );
    assert_eq!(
        hue_bad, 0,
        "{label}: {hue_bad}/{halo_n} visibly tinted glow pixels left the fire family"
    );
}

#[allow(clippy::too_many_arguments)]
fn render_effect(
    def: VfxSystemDef,
    name: &str,
    bloom: bool,
    cam: &VfxCamera,
    width: u32,
    height: u32,
    dt: f32,
) -> regia_vfx::offscreen::OffscreenFrame {
    let mut harness = VfxHarness::new(device(), profile(0.0)).expect("harness");
    harness.set_bloom(bloom);
    harness.set_capture_hdr(true);
    let mut scene = scene_of(vec![def]);
    scene
        .spawn(name, Vec3::new(0.0, 1.2, 0.0), Vec3::Z, 7)
        .expect("spawn");
    shoot(&mut harness, &mut scene, cam, width, height, dt)
}

#[test]
fn hdr_core_blooms_and_stays_in_the_fire_family() {
    let cam = camera(Vec3::new(0.0, 1.2, 4.0), Vec3::new(0.0, 1.2, 0.0));
    const W: u32 = 128;
    const H: u32 = 96;
    let def = burst(
        "core",
        // Linear ~[1, 0.37, 0.07] times emissive 4.2. Green stays under the f16
        // ceiling; red clips. Raw g/r stays above 0.35 so the core is still fire.
        "#ffa449",
        BlendMode::Additive,
        SpriteId::SoftDisc,
        0.0,
        false,
        4.2,
        // ~15 px across at this camera. The HDR core stays inside the 4 px ring
        // (peak scale hides bloom there) and the shoulder sits on the 5 px taps.
        0.72,
        0.0,
        None,
        0.0,
    );
    let on = render_effect(def.clone(), "core", true, &cam, W, H, 1.0 / 60.0);
    let off = render_effect(def, "core", false, &cam, W, H, 1.0 / 60.0);
    assert_fire_look("additive core", &on, &off, W, H);

    let hdr = on.hdr.as_ref().expect("hdr");
    let mut best = (0u32, 0u32, 0.0f32);
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            let v = hdr[i] * 0.2126 + hdr[i + 1] * 0.7152 + hdr[i + 2] * 0.0722;
            if v > best.2 {
                best = (x, y, v);
            }
        }
    }
    let (cx, cy, _) = best;
    let mut on_sum = 0.0f32;
    let mut off_sum = 0.0f32;
    let mut n = 0u32;
    for y in cy.saturating_sub(12)..(cy + 12).min(H) {
        for x in cx.saturating_sub(12)..(cx + 12).min(W) {
            let cheb = x.abs_diff(cx).max(y.abs_diff(cy));
            if !(4..=8).contains(&cheb) {
                continue;
            }
            let i = ((y * W + x) * 4) as usize;
            on_sum += luma_byte(at(&on.pixels, i));
            off_sum += luma_byte(at(&off.pixels, i));
            n += 1;
        }
    }
    assert!(n > 8, "halo sample {n}");
    assert!(
        on_sum > off_sum + 0.02 * n as f32,
        "bloom halo {} vs {}",
        on_sum / n as f32,
        off_sum / n as f32
    );
}

#[test]
fn gallery_fireball_is_premultiplied_and_stays_in_the_fire_family() {
    let cam = camera(Vec3::new(0.0, 1.2, 4.0), Vec3::new(0.0, 1.2, 0.0));
    const W: u32 = 128;
    const H: u32 = 96;
    let boom = regia_vfx::showcase::explosion();
    let fire = boom
        .emitters
        .into_iter()
        .find(|emitter| emitter.name == "fireball")
        .expect("gallery fireball");
    assert_eq!(fire.render.blend, BlendMode::Alpha);
    assert!((fire.render.emissive - 6.0).abs() < 1e-4);
    let def = VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "fireball".into(),
        system: SystemModule {
            bounds: None,
            budget: 64,
            lod: None,
        },
        emitters: vec![fire],
    };
    // Age into the orange of the ramp, not only the white-hot birth frame.
    let on = render_effect(def.clone(), "fireball", true, &cam, W, H, 0.35);
    let off = render_effect(def, "fireball", false, &cam, W, H, 0.35);
    assert_fire_look("gallery fireball", &on, &off, W, H);
}

#[test]
fn a_pink_core_and_its_halo_fail_the_fire_family() {
    let cam = camera(Vec3::new(0.0, 1.2, 4.0), Vec3::new(0.0, 1.2, 0.0));
    const W: u32 = 128;
    const H: u32 = 96;
    let def = burst(
        "pink",
        "#ff3ec8",
        BlendMode::Additive,
        SpriteId::SoftDisc,
        0.0,
        false,
        8.0,
        0.55,
        0.0,
        None,
        0.0,
    );
    let on = render_effect(def.clone(), "pink", true, &cam, W, H, 1.0 / 60.0);
    let off = render_effect(def, "pink", false, &cam, W, H, 1.0 / 60.0);
    let hdr = on.hdr.as_ref().expect("hdr").as_chunks::<4>().0;
    let mut brightest = [0.0f32; 3];
    let mut peak = 0.0f32;
    for px in hdr {
        let rgb = hdr_rgb(*px);
        let m = channel_max(rgb);
        if m > peak {
            peak = m;
            brightest = rgb;
        }
    }
    assert!(peak > 1.0, "pink core should still be HDR, peak {peak:.3}");
    assert!(
        !in_fire_family(brightest),
        "pre-present HDR pink core was rewritten into the fire family: {brightest:?}"
    );
    assert!(
        !in_fire_family(tonemap_agx(brightest)),
        "AgX turned the pink core into the fire family: {brightest:?}"
    );

    let mut display_peak = 0.0f32;
    let mut display_rgb = [0.0f32; 3];
    for px in on.pixels.as_chunks::<4>().0 {
        let rgb = linear_rgb(*px);
        let m = channel_max(rgb);
        if m > display_peak {
            display_peak = m;
            display_rgb = rgb;
        }
    }
    assert!(display_peak > 0.5, "no display core");
    assert!(
        !in_fire_family(display_rgb),
        "display pink core was rewritten: {display_rgb:?}"
    );

    let mut halo_out = 0u32;
    let mut halo_sat = 0.0f32;
    for (on_px, off_px) in on
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(off.pixels.as_chunks::<4>().0)
    {
        let rgb = linear_rgb(*on_px);
        let gap = linear_luma(*on_px) - linear_luma(*off_px);
        if gap > 0.04 && channel_max(rgb) > 0.4 && !in_fire_family(rgb) {
            halo_out += 1;
            halo_sat = halo_sat.max(saturation(rgb));
        }
    }
    assert!(
        halo_out > 0,
        "every pink halo pixel was rewritten into the fire family"
    );
    assert!(
        halo_sat > 0.35,
        "pink halo saturation {halo_sat:.3} is a gray fringe, not a pink hue"
    );
}

#[test]
fn flipbook_frames_change_while_the_particle_stays_put() {
    let cam = camera(Vec3::new(0.0, 1.2, 4.0), Vec3::new(0.0, 1.2, 0.0));
    const W: u32 = 96;
    const H: u32 = 64;
    let mut harness = VfxHarness::new(device(), profile(0.0)).expect("harness");
    let sheet = FlipbookDef {
        cols: 8,
        rows: 2,
        frames: 16,
        mode: FlipbookMode::Fps(8.0),
        random_start: false,
        blend_frames: true,
    };
    let mut scene = scene_of(vec![burst(
        "anim",
        "#ffffff",
        BlendMode::Alpha,
        SpriteId::PuffAnim,
        0.0,
        false,
        1.0,
        1.4,
        0.0,
        Some(sheet),
        0.0,
    )]);
    scene
        .spawn("anim", Vec3::new(0.0, 1.2, 0.0), Vec3::Z, 4)
        .expect("spawn");
    let first = shoot(&mut harness, &mut scene, &cam, W, H, 1.0 / 60.0);
    let later = shoot(&mut harness, &mut scene, &cam, W, H, 0.4);
    let diff = first
        .pixels
        .iter()
        .zip(later.pixels.iter())
        .filter(|(a, b)| a != b)
        .count();
    assert!(diff > 40, "flipbook did not change the picture ({diff} bytes)");
}

#[test]
fn screen_floor_keeps_a_distant_spark_visible() {
    let cam = camera(Vec3::new(0.0, 8.0, 90.0), Vec3::new(0.0, 8.0, 0.0));
    const W: u32 = 320;
    const H: u32 = 180;
    let origin = Vec3::new(0.0, 8.0, 10.0);
    let count = |floor: f32| {
        let mut back_h = VfxHarness::new(device(), profile(floor)).expect("harness");
        let back = back_h.render(device(), &cam, W, H, 1.0 / 60.0).expect("backdrop");
        let mut harness = VfxHarness::new(device(), profile(floor)).expect("harness");
        let mut scene = scene_of(vec![burst(
            "mote",
            "#ffffff",
            BlendMode::Additive,
            SpriteId::HardDisc,
            0.0,
            false,
            4.0,
            0.05,
            0.0,
            None,
            0.0,
        )]);
        scene.spawn("mote", origin, Vec3::Z, 9).expect("spawn");
        let frame = shoot(&mut harness, &mut scene, &cam, W, H, 1.0 / 60.0);
        let (frame_px, _) = frame.pixels.as_chunks::<4>();
        let (back_px, _) = back.pixels.as_chunks::<4>();
        frame_px
            .iter()
            .zip(back_px)
            .filter(|(a, b)| (luma_byte(**a) - luma_byte(**b)).abs() > 0.04)
            .count()
    };
    let bare = count(0.0);
    let floored = count(2.0);
    assert!(
        floored >= 3 && floored > bare,
        "screen floor should lift a 0.05 m spark at ~80 m (bare {bare} px, floor {floored} px)"
    );
}

/// Counts streak pixels over the sky that are visibly tinted and outside the fire family,
/// on the harness display bytes and on the Desktop High composite (exposure round-trip,
/// AgX, default look) of the pre-present HDR buffer.
fn sky_streaks_out_of_family(shot: &regia_vfx::evidence::ShotResult) -> (u32, u32, u32, [f32; 3]) {
    let exposure = exposure_from_ev100(15.0);
    let on_hdr = shot.with_hdr.as_chunks::<4>().0;
    let off_hdr = shot.without_hdr.as_chunks::<4>().0;
    let mut streak = 0u32;
    let mut display_bad = 0u32;
    let mut agx_bad = 0u32;
    let mut worst = [0.0f32; 3];
    for (i, (on_px, off_px)) in shot
        .with
        .as_chunks::<4>()
        .0
        .iter()
        .zip(shot.without.as_chunks::<4>().0)
        .enumerate()
    {
        let sky = linear_rgb(*off_px);
        // Clear sky: blue leads green and red. The ground and the grey horizon haze
        // behind a streak are not this test.
        if !(sky[2] > sky[1] && sky[2] > 1.2 * sky[0] && rec709(sky) > 0.2) {
            continue;
        }
        let shown = linear_rgb(*on_px);
        if linear_luma(*on_px) - linear_luma(*off_px) < 0.03 {
            continue;
        }
        streak += 1;
        // The light the streak adds (pre-present HDR, unclipped) is its own colour: deep
        // red added to a blue sky is the pink streak. Where the streak carries the pixel,
        // the pixel on screen and its Desktop High grade must also be fire. Below ~0.02 the
        // difference is f16/8-bit noise, not a hue.
        let added = added_light(hdr_rgb(on_hdr[i]), hdr_rgb(off_hdr[i]));
        let carries = rec709(added) >= rec709(hdr_rgb(off_hdr[i]));
        let added_bad = rec709(added) >= 0.02 && saturation(added) >= 0.22 && !in_fire_family(added);
        if added_bad || (carries && saturation(shown) >= 0.22 && !in_fire_family(shown)) {
            display_bad += 1;
            worst = shown;
        }
        if carries {
            let restored = restore_exposed(hdr_rgb(on_hdr[i]), exposure);
            let graded = desktop_high_display(restored, restored);
            if saturation(graded) >= 0.22 && !in_fire_family(graded) {
                agx_bad += 1;
            }
        }
    }
    (streak, display_bad, agx_bad, worst)
}

/// Gallery sparks and the explosion's embers over the sky (the evidence shots): no pink or
/// salmon streak. Additive deep red over blue sky prints pink; the family is not widened.
#[test]
fn sky_streaks_stay_in_the_fire_family() {
    const W: u32 = 480;
    const H: u32 = 270;
    let shots = regia_vfx::evidence::effect_shots();
    let pick = |name: &str, capture: f32| {
        let mut shot = shots
            .iter()
            .find(|shot| shot.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name} shot"));
        shot.capture = capture;
        shot
    };
    // The explosion's streak layers alone, under the explosion's name: the smoke column
    // darkens the sky (that is not an added hue) and the fire body has its own test.
    let mut streaks = regia_vfx::showcase::explosion();
    streaks
        .emitters
        .retain(|emitter| matches!(emitter.name.as_str(), "embers" | "sparks" | "streaks"));
    assert!(!streaks.emitters.is_empty(), "the explosion has streak layers");
    for (label, shot, effects) in [
        ("sparks", pick("sparks", 0.8), regia_vfx::showcase_effects()),
        (
            "explosion streaks 0.2 s",
            pick("explosion", 0.2),
            vec![streaks.clone()],
        ),
        (
            "explosion streaks 0.6 s",
            pick("explosion", 0.6),
            vec![streaks.clone()],
        ),
    ] {
        let result = regia_vfx::evidence::render_shot_with(device(), &shot, effects, W, H).expect("shot");
        dump_png(&format!("sky_{}", label.replace(' ', "_")), &result.with, W, H);
        let (streak, display_bad, agx_bad, worst) = sky_streaks_out_of_family(&result);
        assert!(streak >= 20, "{label}: only {streak} streak pixels over the sky");
        assert_eq!(
            display_bad, 0,
            "{label}: {display_bad}/{streak} sky streak pixels left the fire family (e.g. {worst:?})"
        );
        assert_eq!(
            agx_bad, 0,
            "{label}: {agx_bad}/{streak} sky streak pixels left the family through AgX"
        );
    }
}

/// `REGIA_VFX_TEST_DUMP=<dir>` writes test frames as PNGs (inspection only, never compared).
fn dump_png(name: &str, pixels: &[u8], width: u32, height: u32) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_VFX_TEST_DUMP") else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    let file = std::fs::File::create(&path).expect("png create");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(pixels).expect("png data");
}

/// Shape of the fire in an evidence frame (display bytes against the empty backdrop).
#[derive(Debug)]
struct FireShape {
    /// Fire-body pixels (warm, saturated, lit, changed by the effect; the component holding
    /// the brightest fire pixel, so an ember streak or a lit ground square is not the body).
    area: u32,
    /// Bright fire regions (8-connected, at least 3 % of the bright area).
    bright_regions: u32,
    /// Mean linear luma inside 0.35 R of the luma centroid over the fringe (0.75–1.3 R).
    core_over_fringe: f32,
    /// Green/red of the core minus green/red of the fringe (hotter core: yellower).
    core_minus_fringe_gr: f32,
    /// Coefficient of variation of the silhouette radius over 72 rays.
    silhouette_cv: f32,
    /// High-frequency rim roughness: mean |r − moving average of r (9 rays)| over the mean
    /// radius. An ellipse scores ~0; a billowing, licking rim scores high.
    rim_roughness: f32,
    /// Flame tongues on the top edge: local maxima of the silhouette radius over the upper
    /// rays that stand at least 0.08 R above the lowest radius within ±4 rays.
    top_tongues: u32,
}

/// 8-connected components of `mask`, sizes in pixels, as `(labels, sizes)`; label 0 = off.
fn components(mask: &[bool], width: u32, height: u32) -> (Vec<u32>, Vec<usize>) {
    let n = mask.len();
    let mut label = vec![0u32; n];
    let mut sizes = vec![0usize];
    for start in 0..n {
        if !mask[start] || label[start] != 0 {
            continue;
        }
        let id = sizes.len() as u32;
        let mut stack = vec![start];
        label[start] = id;
        let mut size = 0usize;
        while let Some(p) = stack.pop() {
            size += 1;
            let (x, y) = ((p as u32 % width) as i32, (p as u32 / width) as i32);
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= width as i32 || ny >= height as i32 {
                        continue;
                    }
                    let q = (ny as u32 * width + nx as u32) as usize;
                    if mask[q] && label[q] == 0 {
                        label[q] = id;
                        stack.push(q);
                    }
                }
            }
        }
        sizes.push(size);
    }
    (label, sizes)
}

fn fire_shape(with: &[u8], without: &[u8], width: u32, height: u32) -> FireShape {
    let n = (width * height) as usize;
    let mut fire = vec![false; n];
    let mut luma = vec![0.0f32; n];
    let mut gr = vec![0.0f32; n];
    for i in 0..n {
        let on = linear_rgb(at(with, i * 4));
        let off = linear_rgb(at(without, i * 4));
        let l = rec709(on);
        luma[i] = l;
        gr[i] = on[1] / on[0].max(1e-4);
        let changed = (l - rec709(off)).abs() > 0.04 || added_light(on, off).iter().any(|c| *c > 0.08);
        fire[i] = changed && l > 0.2 && saturation(on) >= 0.25 && on[0] >= on[1] && on[1] >= on[2];
    }
    // The body: the fire component that holds the brightest fire pixel. Thin ember streaks
    // over the sky and warm-lit ground squares under the fire light are separate
    // components and are not the body.
    let brightest = (0..n)
        .filter(|i| fire[*i])
        .max_by(|a, b| luma[*a].total_cmp(&luma[*b]));
    let (labels, _) = components(&fire, width, height);
    let body_label = brightest.map(|i| labels[i]).unwrap_or(0);
    let fire: Vec<bool> = labels
        .iter()
        .map(|l| body_label != 0 && *l == body_label)
        .collect();
    let area = fire.iter().filter(|f| **f).count() as u32;
    let mut lumas: Vec<f32> = (0..n).filter(|i| fire[*i]).map(|i| luma[i]).collect();
    lumas.sort_by(f32::total_cmp);
    let p95 = lumas.get(lumas.len() * 95 / 100).copied().unwrap_or(0.0);
    let bright: Vec<bool> = (0..n).map(|i| fire[i] && luma[i] >= 0.75 * p95).collect();
    let bright_area = bright.iter().filter(|b| **b).count();
    let (_, sizes) = components(&bright, width, height);
    let regions = sizes
        .iter()
        .skip(1)
        .filter(|size| **size * 100 >= bright_area * 3)
        .count() as u32;
    // Luma-weighted centroid and equivalent radius of the fire.
    let (mut cx, mut cy, mut w) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..n {
        if fire[i] {
            cx += (i as u32 % width) as f32 * luma[i];
            cy += (i as u32 / width) as f32 * luma[i];
            w += luma[i];
        }
    }
    let (cx, cy) = (cx / w.max(1e-6), cy / w.max(1e-6));
    let radius = (area as f32 / std::f32::consts::PI).sqrt().max(1.0);
    let (mut core_l, mut core_gr, mut core_n) = (0.0f32, 0.0f32, 0.0f32);
    let (mut ring_l, mut ring_gr, mut ring_n) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..n {
        if !fire[i] {
            continue;
        }
        let dx = (i as u32 % width) as f32 - cx;
        let dy = (i as u32 / width) as f32 - cy;
        let r = (dx * dx + dy * dy).sqrt() / radius;
        if r < 0.35 {
            core_l += luma[i];
            core_gr += gr[i];
            core_n += 1.0;
        } else if (0.75..1.3).contains(&r) {
            ring_l += luma[i];
            ring_gr += gr[i];
            ring_n += 1.0;
        }
    }
    let core_mean = core_l / core_n.max(1.0);
    let ring_mean = ring_l / ring_n.max(1.0);
    // Silhouette: the farthest fire pixel along each of 72 rays from the centroid.
    let mut radii = Vec::with_capacity(72);
    for k in 0..72 {
        let angle = k as f32 * std::f32::consts::TAU / 72.0;
        let (sx, sy) = (angle.cos(), angle.sin());
        let mut last = 0.0f32;
        let mut t = 0.0f32;
        while t < 3.0 * radius {
            let x = (cx + sx * t).round() as i32;
            let y = (cy + sy * t).round() as i32;
            if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                break;
            }
            if fire[(y as u32 * width + x as u32) as usize] {
                last = t;
            }
            t += 0.5;
        }
        radii.push(last);
    }
    let mean_r = radii.iter().sum::<f32>() / radii.len() as f32;
    let var = radii.iter().map(|r| (r - mean_r).powi(2)).sum::<f32>() / radii.len() as f32;
    let n_rays = radii.len();
    let ring = |i: isize| radii[i.rem_euclid(n_rays as isize) as usize];
    let mut rough = 0.0f32;
    for i in 0..n_rays as isize {
        let avg = (-4..=4).map(|k| ring(i + k)).sum::<f32>() / 9.0;
        rough += (ring(i) - avg).abs();
    }
    let rim_roughness = rough / n_rays as f32 / mean_r.max(1e-4);
    // Rays point up the screen where sin(angle) < 0 (y grows downward); keep the upper
    // 120° (angles 210°..330°).
    let mut top_tongues = 0u32;
    for i in 0..n_rays as isize {
        let deg = i as f32 * 360.0 / n_rays as f32;
        if !(210.0..=330.0).contains(&deg) {
            continue;
        }
        let r = ring(i);
        let peak = (-2..=2).all(|k| k == 0 || ring(i + k) <= r) && ring(i - 1) < r + 1e-3;
        let low = (-4..=4).map(|k| ring(i + k)).fold(f32::MAX, f32::min);
        if peak && r - low >= 0.08 * radius {
            top_tongues += 1;
        }
    }
    FireShape {
        area,
        bright_regions: regions,
        core_over_fringe: core_mean / ring_mean.max(1e-4),
        core_minus_fringe_gr: core_gr / core_n.max(1.0) - ring_gr / ring_n.max(1.0),
        silhouette_cv: var.sqrt() / mean_r.max(1e-4),
        rim_roughness,
        top_tongues,
    }
}

/// The explosion at 0.6 s reads as one fire body: one bright region, a core hotter
/// (brighter and yellower) than its fringe, and a ragged silhouette, not one soft disc and
/// not a pile of separate cards.
#[test]
fn explosion_reads_as_one_fire_body() {
    const W: u32 = 480;
    const H: u32 = 270;
    let mut shot = regia_vfx::evidence::effect_shots()
        .into_iter()
        .find(|shot| shot.name == "explosion")
        .expect("explosion shot");
    shot.capture = 0.6;
    let result = regia_vfx::evidence::render_shot(device(), &shot, W, H).expect("shot");
    dump_png("fire_body_0.6", &result.with, W, H);
    let shape = fire_shape(&result.with, &result.without, W, H);
    eprintln!("fire shape at 0.6 s: {shape:?}");
    assert!(shape.area > 1500, "no fire body at 0.6 s: {shape:?}");
    assert_eq!(shape.bright_regions, 1, "the fire is not one body: {shape:?}");
    assert!(
        shape.core_over_fringe > 1.2,
        "the core is not brighter than the fringe: {shape:?}"
    );
    assert!(
        shape.core_minus_fringe_gr > 0.04,
        "the core is not hotter (yellower) than the fringe: {shape:?}"
    );
    assert!(
        shape.silhouette_cv > 0.08,
        "the silhouette is a soft disc: {shape:?}"
    );
    // Round 2: not a round ball. Round 1's body measured rim roughness 0.049 and 2 top
    // tongues (body = the component holding the brightest pixel); flame tongues and soot
    // folds must lift both.
    assert!(
        shape.rim_roughness > 0.07,
        "the rim is a smooth ball, not billows and licks: {shape:?}"
    );
    assert!(
        shape.top_tongues >= 3,
        "no flame tongues on the top edge: {shape:?}"
    );

    // The metric itself: a single soft orange disc on the same backdrop is rejected.
    let mut disc = result.without.clone();
    let (cx, cy, radius) = (W as f32 * 0.5, H as f32 * 0.5, 40.0f32);
    for y in 0..H {
        for x in 0..W {
            let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt() / radius;
            if d < 1.0 {
                let k = 1.0 - d * d;
                let i = ((y * W + x) * 4) as usize;
                let warm = [255.0, 140.0 + 90.0 * k, 40.0 + 60.0 * k];
                for c in 0..3 {
                    disc[i + c] = (f32::from(disc[i + c]) * (1.0 - k) + warm[c] * k).round() as u8;
                }
            }
        }
    }
    let disc_shape = fire_shape(&disc, &result.without, W, H);
    assert!(
        disc_shape.silhouette_cv <= 0.08,
        "the disc check would pass a soft disc: {disc_shape:?}"
    );
}

/// Screen coverage of the gallery smoke after 4 s, seen from `distance` metres with the
/// LOD driven by that camera (the runtime's `LodInput`), on the desktop High profile.
fn smoke_coverage_at(distance: f32, label: &str) -> (f32, f32) {
    const W: u32 = 640;
    const H: u32 = 360;
    let cam = VfxCamera {
        eye: Vec3::new(0.0, 2.0, distance),
        target: Vec3::new(0.0, 4.0, 0.0),
        fov_y: 50.0_f32.to_radians(),
        near: 0.1,
        far: 400.0,
    };
    let run = |spawn: bool| {
        let mut harness = VfxHarness::new(device(), regia_vfx::VfxProfile::desktop_high()).expect("harness");
        let mut scene = scene_of(vec![regia_vfx::showcase::smoke()]);
        if spawn {
            scene.spawn("smoke", Vec3::ZERO, Vec3::Z, 21).expect("spawn");
        }
        let mut frame = None;
        for _ in 0..80 {
            let lod = LodInput { camera_pos: cam.eye };
            scene.update_budgets(0.05, &lod, 100_000, 8_192);
            scene.upload(harness.gpu());
            frame = Some(harness.render(device(), &cam, W, H, 0.05).expect("frame"));
        }
        frame.expect("frames")
    };
    let on = run(true);
    let off = run(false);
    dump_png(label, &on.pixels, W, H);
    let mut changed = 0usize;
    let mut ink = 0.0f32;
    for (a, b) in on
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(off.pixels.as_chunks::<4>().0)
    {
        let d = (luma_byte(*a) - luma_byte(*b)).abs();
        if d > 0.03 {
            changed += 1;
        }
        ink += d;
    }
    let px = (W * H) as f32;
    (changed as f32 / px, ink / px)
}

/// The 80 m smoke (DO27's smoke_80m camera distance) keeps the look it has up close: its
/// screen coverage, normalised by distance², stays within reach of the 20 m view. A LOD
/// that only drops spawn rate thins the column until it vanishes at range.
#[test]
fn smoke_at_80m_keeps_its_coverage() {
    let (near, near_ink) = smoke_coverage_at(20.0, "smoke_20m");
    let (far, far_ink) = smoke_coverage_at(80.0, "smoke_80m");
    let k = (80.0f32 / 20.0).powi(2);
    let normalised = far * k / near.max(1e-6);
    let ink = far_ink * k / near_ink.max(1e-6);
    eprintln!(
        "smoke: 20 m coverage {near:.4} ink {near_ink:.5}; 80 m coverage {far:.4} ink {far_ink:.5}; normalized coverage {normalised:.2} ink {ink:.2}"
    );
    assert!(near > 0.02, "no smoke up close ({near})");
    assert!(
        normalised > 0.65,
        "the 80 m smoke lost its body: {far:.4} at 80 m vs {near:.4} at 20 m (normalized {normalised:.2})"
    );
    assert!(
        ink > 0.75,
        "the 80 m smoke is thin: ink {far_ink:.5} at 80 m vs {near_ink:.5} at 20 m (normalized {ink:.2})"
    );
    // A column on a far ridge still reads: the LOD thins the spawn rate and grows the
    // puffs instead of dropping the effect.
    let (_, far2_ink) = smoke_coverage_at(200.0, "smoke_200m");
    let ink2 = far2_ink * (200.0f32 / 20.0).powi(2) / near_ink.max(1e-6);
    eprintln!("smoke: 200 m ink {far2_ink:.6}, normalized {ink2:.2}");
    assert!(
        ink2 > 0.5,
        "the 200 m smoke is gone or thin: ink {far2_ink:.6} (normalized {ink2:.2})"
    );
}
