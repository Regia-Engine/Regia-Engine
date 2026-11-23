// SPDX-License-Identifier: MPL-2.0
//! The runtime frame path at daylight exposure: decals, trails and particles must survive the
//! post stack's exposure multiply.
//!
//! The offscreen VFX harness runs at exposure 1, so it cannot see this. These tests drive the
//! real `FrameRenderer` through the `TranslucentLayer` hook with a Desktop-High-shaped frame
//! (single sample, `Rgba16Float`, post on) at `exposure_from_ev100(15)` ≈ 2.5e-5. A layer that
//! draws into the pre-exposure HDR target without dividing by that exposure prints black.

use std::sync::{Arc, Mutex};

use regia_core::math::Vec3;
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, PostSettings, RenderDevice,
};
use regia_vfx::format::{
    BlendMode, ColorKey, EmitterDef, InitModule, RangeDef, RenderMode, RenderModule, ShapeDef, SpawnMode,
    SpawnModule, SpriteId, SystemModule, UpdateModule, VFX_FORMAT_VERSION, VelocityDef, VfxSystemDef,
};
use regia_vfx::gpu::TrailVertex;
use regia_vfx::sim::LodInput;
use regia_vfx::{
    CHANNEL_ALBEDO, Decal, DecalSurface, VfxCamera, VfxGpu, VfxProfile, VfxScene, align_to_normal,
    project_pixel, translucent_layer,
};

const W: u32 = 160;
const H: u32 = 120;
const EYE: [f32; 3] = [0.0, 3.0, 8.0];
const LOOK: [f32; 3] = [0.0, 0.6, 0.0];
const FOV: f32 = 0.9;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn profile() -> VfxProfile {
    VfxProfile {
        particle_capacity: 256,
        decal_capacity: 16,
        sort: true,
        light_budget: 0,
        screen_floor_px: 0.0,
        smoke_budget: 256,
        soft_particle_res: regia_vfx::gpu::SoftParticleRes::Full,
        event_capacity: 16,
        child_capacity: 16,
    }
}

fn camera() -> VfxCamera {
    VfxCamera {
        eye: Vec3::from_array(EYE),
        target: Vec3::from_array(LOOK),
        fov_y: FOV,
        near: 0.1,
        far: 60.0,
    }
}

fn ground() -> Vec<LabVertex> {
    let half = 30.0;
    [[-half, -half], [half, -half], [half, half], [-half, half]]
        .into_iter()
        .map(|[x, z]| LabVertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

const GROUND_INDICES: [u32; 6] = [0, 2, 1, 0, 3, 2];

/// One soft orange puff at `(0, 1.6, 0)`, alive for the whole test.
fn puff() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "puff".into(),
        system: SystemModule {
            bounds: None,
            budget: 8,
            lod: None,
        },
        emitters: vec![EmitterDef {
            name: "puff".into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode: SpawnMode::Burst(1),
                lifetime: RangeDef::from(30.0),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Point,
                velocity: VelocityDef::default(),
                size: RangeDef::from(1.1),
                rotation: RangeDef::from(0.0),
                spin: RangeDef::from(0.0),
                color: vec![ColorKey {
                    t: 0.0,
                    color: "#ff9a3c".into(),
                }],
            },
            update: UpdateModule::default(),
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::SoftDisc,
                blend: BlendMode::Alpha,
                emissive: 1.5,
                ..RenderModule::default()
            },
        }],
    }
}

/// A yellow paint stripe on the ground at `(-1.6, 0, 0.6)`.
fn paint() -> (Decal, DecalSurface) {
    let decal = Decal {
        position: Vec3::new(-1.6, 0.0, 0.6),
        rotation: align_to_normal(Vec3::Y, 0.0),
        half_size: Vec3::new(0.9, 0.5, 0.9),
        color: [0.95, 0.78, 0.08, 1.0],
        age: 0.0,
        lifetime: None,
    };
    let mut surface = DecalSurface::from_decal(&decal);
    surface.albedo = [0.95, 0.78, 0.08];
    surface.opacity = 1.0;
    surface.atlas_layer = -1;
    surface.edge_softness = 0.0;
    surface.channels = CHANNEL_ALBEDO;
    surface.angle_fade = [0.0, 0.0];
    // Paint keeps the receiver's light and swaps the colour.
    surface.blend = 1.0;
    (decal, surface)
}

/// A 0.3 m orange ribbon from `(0.6, 1.0, 0)` to `(3.0, 1.0, 0)`, camera-facing in +Y.
fn trail() -> Vec<TrailVertex> {
    let v = |x: f32, y: f32, side: f32| TrailVertex {
        position: [x, y, 0.0],
        _pad: 0.0,
        uv: [0.0, side],
        _pad2: [0.0; 2],
        color: [1.0, 0.55, 0.15, 1.0],
    };
    let (a_l, a_r) = (v(0.6, 0.85, -1.0), v(0.6, 1.15, 1.0));
    let (b_l, b_r) = (v(3.0, 0.85, -1.0), v(3.0, 1.15, 1.0));
    vec![a_l, b_l, a_r, a_r, b_l, b_r]
}

struct Shot {
    quality: FrameQuality,
    post: PostSettings,
    /// `FrameScene::exposure_ev`. Auto exposure ignores it.
    scene_ev: f32,
    frames: u32,
    /// `FrameScene::depth_prepass` (Mobile profiles run without one).
    prepass: bool,
    /// Sun illuminance (lux) and flat ambient (lux).
    sun_lux: f32,
    ambient_lux: f32,
    /// Paint decal blend (0 multiply, 1 paint).
    paint_blend: f32,
}

fn desktop_high_like(auto_exposure: bool) -> Shot {
    Shot {
        quality: FrameQuality::LAB,
        post: PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure,
            exposure_ev: if auto_exposure { 0.0 } else { 15.0 },
            fxaa: false,
            taa: false,
            ..PostSettings::default()
        },
        scene_ev: if auto_exposure { 0.0 } else { 15.0 },
        frames: if auto_exposure { 6 } else { 2 },
        prepass: true,
        sun_lux: 100_000.0,
        ambient_lux: 20_000.0,
        paint_blend: 1.0,
    }
}

/// Renders the ground with (or without) the paint decal, the trail and the puff.
fn render(device: &RenderDevice, shot: &Shot, effects: bool) -> Vec<u8> {
    let all = Effects {
        puff: effects,
        paint: effects,
        trail: effects,
        haze: false,
        haze_on: true,
    };
    render_with(device, shot, all, &[])
}

fn render_with_lights(
    device: &RenderDevice,
    shot: &Shot,
    effects: bool,
    local_lights: &[regia_render::LabLocalLight],
) -> Vec<u8> {
    let all = Effects {
        puff: effects,
        paint: effects,
        trail: effects,
        haze: false,
        haze_on: true,
    };
    render_with(device, shot, all, local_lights)
}

/// Which effects a frame draws.
#[derive(Clone, Copy)]
struct Effects {
    puff: bool,
    paint: bool,
    trail: bool,
    haze: bool,
    /// Host look switch (`VfxLook::heat_haze`, agent `vfx.set_look`).
    haze_on: bool,
}

/// One heat-haze card standing over the paint stripe.
fn haze_card() -> VfxSystemDef {
    let mut def = puff();
    def.name = "haze".into();
    let e = &mut def.emitters[0];
    e.name = "haze".into();
    e.init.size = RangeDef::from(2.4);
    e.render.heat_haze = 10.0;
    e.render.blend = BlendMode::Additive;
    def
}

fn render_with(
    device: &RenderDevice,
    shot: &Shot,
    effects: Effects,
    local_lights: &[regia_render::LabLocalLight],
) -> Vec<u8> {
    let gpu = Arc::new(Mutex::new(VfxGpu::new(device, profile()).expect("vfx gpu")));
    let mut renderer = FrameRenderer::new();
    renderer.set_translucent_layers(vec![translucent_layer(Arc::clone(&gpu))]);
    let mut lib = regia_vfx::scene::VfxLibrary::new();
    lib.add(puff()).expect("puff def");
    lib.add(haze_card()).expect("haze def");
    let mut scene = VfxScene::new(lib);
    if effects.puff {
        scene
            .spawn("puff", Vec3::new(0.0, 1.6, 0.0), Vec3::Z, 3)
            .expect("spawn puff");
    }
    if effects.haze {
        scene
            .spawn("haze", Vec3::new(-1.6, 0.6, 0.6), Vec3::Z, 4)
            .expect("spawn haze");
    }
    let vertices = ground();
    let material = LabMaterial {
        base_color: [0.5, 0.5, 0.5, 1.0],
        roughness: 0.9,
        ..LabMaterial::EMPTY
    };
    let mut pixels = Vec::new();
    for _ in 0..shot.frames {
        {
            let mut locked = gpu.lock().expect("vfx gpu");
            scene.update(
                1.0 / 60.0,
                &LodInput {
                    camera_pos: Vec3::from_array(EYE),
                },
                256,
            );
            scene.upload(&mut locked);
            locked.set_haze_look(effects.haze_on, 1.0);
            if effects.paint {
                let (decal, mut surface) = paint();
                surface.blend = shot.paint_blend;
                locked.write_decals_with(&[decal], &[surface]);
            } else {
                locked.write_decals_with(&[], &[]);
            }
            let ribbon = if effects.trail { trail() } else { Vec::new() };
            locked.write_trails(&ribbon);
        }
        let frame = FrameScene {
            width: W,
            height: H,
            eye: EYE,
            target: LOOK,
            fov_y: FOV,
            near: 0.1,
            far: 60.0,
            depth_prepass: shot.prepass,
            directional_shadows: false,
            ibl: false,
            exposure_ev: shot.scene_ev,
            ambient_lux: shot.ambient_lux,
            double_sided: false,
            post: Some(&shot.post),
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0, 1.0, 1.0], shot.sun_lux),
            local_lights,
            quality: shot.quality,
            meshes: vec![FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1,
                object_key: 1,
                vertices: &vertices,
                indices: &GROUND_INDICES,
                material: &material,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: regia_render::ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }],
        };
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    pixels
}

/// Mean display RGB (0..1) of a `(2r+1)²` window.
fn mean_rgb(pixels: &[u8], cx: u32, cy: u32, r: u32) -> [f32; 3] {
    let mut sum = [0.0f32; 3];
    let mut n = 0.0f32;
    for y in cy.saturating_sub(r)..=(cy + r).min(H - 1) {
        for x in cx.saturating_sub(r)..=(cx + r).min(W - 1) {
            let i = ((y * W + x) * 4) as usize;
            for c in 0..3 {
                sum[c] += f32::from(pixels[i + c]) / 255.0;
            }
            n += 1.0;
        }
    }
    [sum[0] / n, sum[1] / n, sum[2] / n]
}

/// `REGIA_HDR_FRAME_DUMP=<dir>` writes the frames as PNGs (evidence; not compared).
fn dump(label: &str, kind: &str, pixels: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_HDR_FRAME_DUMP") else {
        return;
    };
    let name = format!("{}_{kind}.png", label.replace(' ', "_").to_lowercase());
    let path = std::path::Path::new(&dir).join(name);
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::File::create(&path).expect("png create");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(pixels).expect("png data");
}

fn luma(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

/// Orange over the grey ground: red leads blue by 0.3 more than before, and the pixel is
/// neither black (an undivided HDR draw) nor blown white (a divided display draw).
fn is_visible_orange(before: [f32; 3], after: [f32; 3]) -> bool {
    (after[0] - after[2]) > (before[0] - before[2]) + 0.3 && luma(after) > 0.3
}

fn pixel_of(world: Vec3) -> (u32, u32) {
    project_pixel(&camera(), W, H, world).expect("point on screen")
}

/// Decal, trail and particle at Desktop High EV 15 through exposure, AgX and the look.
fn assert_effects_visible(label: &str, shot: &Shot) {
    let device = device();
    let bare = render(&device, shot, false);
    let lit = render(&device, shot, true);
    dump(label, "bare", &bare);
    dump(label, "effects", &lit);

    let ground = mean_rgb(&bare, W / 2, H - 8, 3);
    assert!(
        luma(ground) > 0.15,
        "{label}: the sunlit ground must be exposed, luma {:.3}",
        luma(ground)
    );

    let (dx, dy) = pixel_of(Vec3::new(-1.6, 0.0, 0.6));
    let before = mean_rgb(&bare, dx, dy, 2);
    let after = mean_rgb(&lit, dx, dy, 2);
    assert!(
        luma(after) > 0.5 * luma(before),
        "{label}: the paint decal went dark ({:.3} → {:.3}, {after:?})",
        luma(before),
        luma(after)
    );
    assert!(
        (after[0] - after[2]) > (before[0] - before[2]) + 0.12,
        "{label}: the paint decal is not yellow ({before:?} → {after:?})"
    );

    let (tx, ty) = pixel_of(Vec3::new(1.8, 1.0, 0.0));
    let before = mean_rgb(&bare, tx, ty, 0);
    let after = mean_rgb(&lit, tx, ty, 0);
    assert!(
        is_visible_orange(before, after),
        "{label}: the trail is not a visible orange ribbon ({before:?} → {after:?})"
    );

    let (px, py) = pixel_of(Vec3::new(0.0, 1.6, 0.0));
    let before = mean_rgb(&bare, px, py, 1);
    let after = mean_rgb(&lit, px, py, 1);
    assert!(
        is_visible_orange(before, after),
        "{label}: the particle is not a visible orange puff ({before:?} → {after:?})"
    );
    assert!(
        after[2] < 0.9 * after[0],
        "{label}: the particle blew out to white ({after:?})"
    );
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn decal_trail_and_particle_survive_ev15_exposure() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_effects_visible("manual EV 15", &desktop_high_like(false));
}

/// Auto exposure meters the sunlit ground near EV 15 while `FrameScene::exposure_ev` stays 0.
/// The layer must divide by the exposure the post stack applies, not the scene's EV.
#[test]
fn particles_follow_auto_exposure() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_effects_visible("auto exposure", &desktop_high_like(true));
}

/// MSAA keeps the display pass. Display-referred colours are not divided by exposure there.
#[test]
fn msaa_display_pass_is_not_divided_by_exposure() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut shot = desktop_high_like(false);
    shot.quality.msaa_samples = 4;
    assert_effects_visible("MSAA EV 15", &shot);
}

/// The explosion's fire light reaches the ground through the renderer's local lights at
/// daylight EV 15: the ground under the fireball brightens and warms while the fire burns.
#[test]
fn fireball_light_reaches_the_ground_at_ev15() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut lib = regia_vfx::scene::VfxLibrary::new();
    lib.add(regia_vfx::showcase::explosion()).expect("explosion");
    let mut scene = VfxScene::new(lib);
    scene
        .spawn("explosion", Vec3::new(0.0, 1.0, 0.0), Vec3::Z, 5)
        .expect("spawn");
    let lod = LodInput {
        camera_pos: Vec3::from_array(EYE),
    };
    for _ in 0..18 {
        scene.update(1.0 / 60.0, &lod, 4096);
    }
    let mut lights = regia_vfx::lights::VfxFrameLights::default();
    scene.gather_lights(&mut lights, 4);
    let point = regia_vfx::particle_point_lights(&lights);
    assert!(!point.is_empty(), "no fire light at 0.3 s");
    let shot = desktop_high_like(false);
    let dark = render_with_lights(&device, &shot, false, &[]);
    let lit = render_with_lights(&device, &shot, false, &point);
    dump("fire light", "off", &dark);
    dump("fire light", "on", &lit);
    let (gx, gy) = pixel_of(Vec3::new(0.6, 0.0, 1.2));
    let before = mean_rgb(&dark, gx, gy, 2);
    let after = mean_rgb(&lit, gx, gy, 2);
    assert!(
        luma(after) > luma(before) + 0.05,
        "the fire did not light the ground ({before:?} → {after:?})"
    );
    assert!(
        (after[0] - after[2]) > (before[0] - before[2]) + 0.04,
        "the fire light is not warm on the ground ({before:?} → {after:?})"
    );
}

/// Heat haze on the runtime HDR path bends the scene behind the card (pixels move) without
/// erasing what the decals drew there: the haze samples the copy taken after the decals.
#[test]
fn heat_haze_bends_the_scene_and_keeps_the_decal() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let shot = desktop_high_like(false);
    let base = Effects {
        puff: false,
        paint: true,
        trail: false,
        haze: false,
        haze_on: true,
    };
    let plain = render_with(&device, &shot, base, &[]);
    let switched_off = render_with(
        &device,
        &shot,
        Effects {
            haze: true,
            haze_on: false,
            ..base
        },
        &[],
    );
    assert_eq!(
        plain, switched_off,
        "vfx.set_look heat_haze:false still bent the scene"
    );
    let hazy = render_with(&device, &shot, Effects { haze: true, ..base }, &[]);
    let bare = render_with(&device, &shot, Effects { paint: false, ..base }, &[]);
    dump("haze", "plain", &plain);
    dump("haze", "hazy", &hazy);
    let (dx, dy) = pixel_of(Vec3::new(-1.6, 0.0, 0.6));
    let mut moved = 0u32;
    let mut total = 0u32;
    for y in dy.saturating_sub(10)..(dy + 10).min(H) {
        for x in dx.saturating_sub(14)..(dx + 14).min(W) {
            let a = mean_rgb(&plain, x, y, 0);
            let b = mean_rgb(&hazy, x, y, 0);
            total += 1;
            if (luma(a) - luma(b)).abs() > 0.02 || (a[2] - b[2]).abs() > 0.03 {
                moved += 1;
            }
        }
    }
    assert!(
        moved * 50 >= total,
        "the haze card did not bend the scene ({moved}/{total} pixels moved)"
    );
    let painted = mean_rgb(&plain, dx, dy, 2);
    let through = mean_rgb(&hazy, dx, dy, 2);
    let ground = mean_rgb(&bare, dx, dy, 2);
    assert!(
        (through[0] - through[2]) > 0.6 * (painted[0] - painted[2])
            && (through[0] - through[2]) > (ground[0] - ground[2]) + 0.1,
        "the haze erased the paint under it ({painted:?} → {through:?}, ground {ground:?})"
    );
}

/// Mobile shape: 4× MSAA and no depth prepass. The forward pass writes only the
/// multisampled depth; the translucent pass resolves it, so decals, trails and particles
/// still draw.
#[test]
fn msaa_without_prepass_still_draws_decals() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut shot = desktop_high_like(false);
    shot.quality.msaa_samples = 4;
    shot.prepass = false;
    assert_effects_visible("MSAA no prepass", &shot);
}

/// Trails are emissive: on a Desktop High HDR frame a tracer prints as bright as its
/// authored colour did on the display pass (MSAA path, no tonemap on the effect), within
/// 20 % in luma. Measured: HDR 0.74 (the linear colour through exposure and AgX, the same
/// bytes an sRGB swapchain shows on the display pass) vs 0.62 on this `Rgba8Unorm` display
/// target, which stores the linear colour as bytes. It reads lighter, not dimmer, so no HDR
/// gain is applied; a tracer that should bloom authors a colour above 1.
#[test]
fn hdr_trail_is_as_bright_as_the_display_trail() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let hdr = desktop_high_like(false);
    let mut msaa = desktop_high_like(false);
    msaa.quality.msaa_samples = 4;
    let only_trail = Effects {
        puff: false,
        paint: false,
        trail: true,
        haze: false,
        haze_on: true,
    };
    let on_hdr = render_with(&device, &hdr, only_trail, &[]);
    let on_msaa = render_with(&device, &msaa, only_trail, &[]);
    dump("trail", "hdr", &on_hdr);
    dump("trail", "msaa", &on_msaa);
    let (tx, ty) = pixel_of(Vec3::new(1.8, 1.0, 0.0));
    let a = luma(mean_rgb(&on_hdr, tx, ty, 0));
    let b = luma(mean_rgb(&on_msaa, tx, ty, 0));
    eprintln!(
        "trail luma: HDR {a:.3} {:?}, display pass {b:.3} {:?}",
        mean_rgb(&on_hdr, tx, ty, 0),
        mean_rgb(&on_msaa, tx, ty, 0)
    );
    assert!(
        (a - b).abs() <= 0.2 * b,
        "HDR trail luma {a:.3} vs display-pass {b:.3}"
    );
}

/// A bright-albedo decal at EV 11 (a dim Mars afternoon) on the runtime HDR path is lit by
/// the receiver's light: multiply never prints brighter than the ground, and paint prints
/// at most the ground × the albedo ratio (paint luma over the 0.45 reference ground, capped
/// at 2×), in sun and in shade. A paint composite with a light floor of its own glows in
/// shade (DO27's road read as a glowing ochre ribbon).
#[test]
fn bright_decal_is_never_brighter_than_its_ground_at_ev11() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    // (sun lux, ambient lux): a lit afternoon and a slope in shade.
    for (sun, ambient) in [(6_000.0f32, 1_500.0f32), (0.0, 900.0)] {
        for blend in [0.0f32, 1.0] {
            let mut shot = desktop_high_like(false);
            shot.post.exposure_ev = 11.0;
            shot.scene_ev = 11.0;
            shot.sun_lux = sun;
            shot.ambient_lux = ambient;
            shot.paint_blend = blend;
            let only_paint = Effects {
                puff: false,
                paint: true,
                trail: false,
                haze: false,
                haze_on: true,
            };
            let bare = render_with(
                &device,
                &shot,
                Effects {
                    paint: false,
                    ..only_paint
                },
                &[],
            );
            let painted = render_with(&device, &shot, only_paint, &[]);
            let label = format!("ev11 sun {sun} blend {blend}");
            dump(&label, "bare", &bare);
            dump(&label, "painted", &painted);
            let (dx, dy) = pixel_of(Vec3::new(-1.6, 0.0, 0.6));
            let ground = mean_rgb(&bare, dx, dy, 2);
            let decal = mean_rgb(&painted, dx, dy, 2);
            let lin = |c: f32| {
                if c <= 0.04045 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            let lin_luma = |p: [f32; 3]| 0.2126 * lin(p[0]) + 0.7152 * lin(p[1]) + 0.0722 * lin(p[2]);
            assert!(
                luma(ground) > 0.08,
                "{label}: the ground must be visible ({ground:?})"
            );
            // The paint albedo (0.95, 0.78, 0.08) has linear luma 0.76: ratio 1.69.
            let ratio = if blend > 0.5 {
                (0.76f32 / 0.45).min(2.0)
            } else {
                1.0
            };
            // 30 % for the display transform (AgX toe and the look lift a saturated hue
            // above a grey or blue of the same scene luma). The light-floor composite
            // this guards against printed 18× its ground in the shade case.
            assert!(
                lin_luma(decal) <= lin_luma(ground) * ratio * 1.3,
                "{label}: the decal printed brighter than its ground allows ({ground:?} -> {decal:?})"
            );
            assert!(
                decal[0] - decal[2] > ground[0] - ground[2] + 0.05,
                "{label}: the decal still shows its yellow ({ground:?} -> {decal:?})"
            );
        }
    }
}
