// SPDX-License-Identifier: MPL-2.0
//! Analytical GPU checks for projected decals.
//!
//! These fail when the shader ignores the receiver normal (angle fade), paints a masked
//! layer (including a rigid mesh stamped as a character), draws a rut as a straight chord,
//! or softens a hard pad marking. The default-look set pieces lock what the decals add
//! inside their footprints (rendered with and without decals) against blessed numbers in
//! `docs/evidence/decals/setpiece_expectations.ron`; the whole frame is not compared.
//! `REGIA_WRITE_DECAL_EVIDENCE=1` re-blesses those numbers and rewrites the evidence
//! PNGs (lavapipe is the blessed GPU).

use std::path::PathBuf;

use std::sync::{Arc, Mutex};

use regia_core::math::Vec3;
use regia_render::{
    AtmosphereParameters, DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer,
    FrameScene, JointMatrix, LabMaterial, LabVertex, OutdoorParams, PostSettings, RenderDevice,
    SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerStyle, MaterialParams, TERRAIN_LAYERS_FORMAT_VERSION, TerrainLayers,
    TerrainMap, TerrainMaterial, TerrainSharedDraw,
};
use regia_vfx::{
    CHANNEL_ALBEDO, CHANNEL_EMISSIVE, CHANNEL_NORMAL, CHANNEL_ROUGHNESS, Decal, DecalFrame, DecalScene,
    DecalStrip, DecalSurface, LAYER_TERRAIN, LAYER_WORLD, VfxCamera, VfxGpu, VfxProfile, align_to_normal,
    builtin_prefab, measure_decal_pass, project_pixel, sample_quadratic, translucent_layer,
};

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn profile() -> VfxProfile {
    VfxProfile {
        particle_capacity: 64,
        decal_capacity: 128,
        sort: false,
        light_budget: 0,
        screen_floor_px: 0.0,
        smoke_budget: 0,
        soft_particle_res: regia_vfx::gpu::SoftParticleRes::Full,
        event_capacity: 16,
        child_capacity: 16,
    }
}

fn luma(px: [u8; 4]) -> f32 {
    (0.2126 * f32::from(px[0]) + 0.7152 * f32::from(px[1]) + 0.0722 * f32::from(px[2])) / 255.0
}

fn at(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

fn window_mean(pixels: &[u8], width: u32, height: u32, cx: u32, cy: u32, radius: u32) -> f32 {
    let mut sum = 0.0f32;
    let mut n = 0.0f32;
    let x0 = cx.saturating_sub(radius);
    let y0 = cy.saturating_sub(radius);
    let x1 = (cx + radius).min(width - 1);
    let y1 = (cy + radius).min(height - 1);
    for y in y0..=y1 {
        for x in x0..=x1 {
            sum += luma(at(pixels, width, x, y));
            n += 1.0;
        }
    }
    sum / n.max(1.0)
}

fn window_variance(pixels: &[u8], width: u32, height: u32, cx: u32, cy: u32, radius: u32) -> f32 {
    let mean = window_mean(pixels, width, height, cx, cy, radius);
    let mut acc = 0.0f32;
    let mut n = 0.0f32;
    let x0 = cx.saturating_sub(radius);
    let y0 = cy.saturating_sub(radius);
    let x1 = (cx + radius).min(width - 1);
    let y1 = (cy + radius).min(height - 1);
    for y in y0..=y1 {
        for x in x0..=x1 {
            let d = luma(at(pixels, width, x, y)) - mean;
            acc += d * d;
            n += 1.0;
        }
    }
    acc / n.max(1.0)
}

/// Fraction of window pixels whose luma moved, but not by a full paint.
fn partial_fraction(base: &[u8], img: &[u8], width: u32, height: u32, cx: u32, cy: u32, radius: u32) -> f32 {
    let mut partial = 0.0f32;
    let mut n = 0.0f32;
    let x0 = cx.saturating_sub(radius);
    let y0 = cy.saturating_sub(radius);
    let x1 = (cx + radius).min(width - 1);
    let y1 = (cy + radius).min(height - 1);
    for y in y0..=y1 {
        for x in x0..=x1 {
            let d = (luma(at(img, width, x, y)) - luma(at(base, width, x, y))).abs();
            if d > 0.03 && d < 0.28 {
                partial += 1.0;
            }
            n += 1.0;
        }
    }
    partial / n.max(1.0)
}

fn must_project(camera: &VfxCamera, width: u32, height: u32, world: Vec3) -> (u32, u32) {
    project_pixel(camera, width, height, world)
        .unwrap_or_else(|| panic!("world {world:?} is off screen at {width}x{height}"))
}

fn solid(position: Vec3, half: Vec3, albedo: [f32; 3], mask: u32) -> (Decal, DecalSurface) {
    let decal = Decal {
        position,
        rotation: align_to_normal(Vec3::Y, 0.0),
        half_size: half,
        color: [albedo[0], albedo[1], albedo[2], 1.0],
        age: 0.0,
        lifetime: None,
    };
    let mut surface = DecalSurface::from_decal(&decal);
    surface.albedo = albedo;
    surface.opacity = 1.0;
    surface.atlas_layer = -1;
    surface.edge_softness = 0.0;
    surface.channels = CHANNEL_ALBEDO;
    surface.receiver_mask = mask;
    surface.angle_fade = [0.0, 0.0];
    surface.blend = 0.0;
    (decal, surface)
}

/// Writes an evidence frame to `docs/evidence/decals/` (only while blessing).
fn write_evidence_png(name: &str, pixels: &[u8], width: u32, height: u32) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/evidence/decals")
        .join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("evidence dir");
    }
    let file = std::fs::File::create(&path).expect("png create");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(pixels).expect("png data");
}

/// CPU adapters (lavapipe) may report a zero timestamp. A GPU that can time passes must not.
fn assert_decal_milliseconds(device: &RenderDevice, ms: f32) {
    let info = device.adapter.get_info();
    if info.device_type == wgpu::DeviceType::Cpu || !device.capabilities.timestamp_query {
        // No GPU timer on this adapter (Metal has no in-encoder timestamps, ADR-275): the pass cannot be timed.
        return;
    }
    assert!(
        ms > 0.0,
        "non-CPU adapter {} ({:?}) must time the decal pass, got {ms}",
        info.name,
        info.device_type
    );
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn decals_respect_normals_masks_curves_and_hard_edges() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut scene = DecalScene::new(&device, profile()).expect("decal scene");
    let width = 320u32;
    let height = 180u32;

    // --- layer mask: a character quad under the same decal must stay clean ---
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 6.0, 8.0), Vec3::ZERO);
    let mut frame = DecalFrame {
        width,
        height,
        camera,
        ground: false,
        split_quads: true,
        ..DecalFrame::default()
    };
    let baseline = scene.render(&device, &frame).expect("mask baseline");
    let (decal, surface) = solid(
        Vec3::new(0.0, 0.05, 0.0),
        Vec3::new(2.4, 1.1, 0.45),
        [0.02, 0.01, 0.0],
        LAYER_WORLD,
    );
    frame.decals = vec![(decal, surface)];
    let masked = scene.render(&device, &frame).expect("mask render");
    let terrain = must_project(&camera, width, height, Vec3::new(-0.8, 0.05, 0.0));
    let character = must_project(&camera, width, height, Vec3::new(0.85, 0.05, 0.0));
    let terrain_delta = (window_mean(&masked, width, height, terrain.0, terrain.1, 6)
        - window_mean(&baseline, width, height, terrain.0, terrain.1, 6))
    .abs();
    let character_delta = (window_mean(&masked, width, height, character.0, character.1, 6)
        - window_mean(&baseline, width, height, character.0, character.1, 6))
    .abs();
    assert!(
        terrain_delta > 0.06,
        "terrain should take the decal, delta {terrain_delta} at {terrain:?}"
    );
    assert!(
        character_delta < 0.02,
        "character layer must reject the decal, delta {character_delta} at {character:?}"
    );

    // --- angle fade: a forced +X receiver normal hides the decal; +Y shows it ---
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 5.0, 7.0), Vec3::ZERO);
    let mut frame = DecalFrame {
        width,
        height,
        camera,
        ground: false,
        single_quad: true,
        ..DecalFrame::default()
    };
    let bare = scene.render(&device, &frame).expect("angle baseline");
    let (mut decal, mut surface) = solid(
        Vec3::new(0.0, 0.05, 0.0),
        Vec3::new(1.1, 1.1, 0.4),
        [0.04, 0.02, 0.01],
        LAYER_WORLD,
    );
    surface.angle_fade = [0.3, 0.6];
    frame.decals = vec![(decal, surface)];
    frame.force_normal = [1.0, 0.0, 0.0, 1.0];
    let grazing = scene.render(&device, &frame).expect("grazing");
    frame.force_normal = [0.0, 1.0, 0.0, 1.0];
    decal.position = Vec3::new(0.0, 0.05, 0.0);
    frame.decals = vec![(decal, surface)];
    let facing = scene.render(&device, &frame).expect("facing");
    let center = must_project(&camera, width, height, Vec3::new(0.0, 0.05, 0.0));
    let grazing_delta = (window_mean(&grazing, width, height, center.0, center.1, 8)
        - window_mean(&bare, width, height, center.0, center.1, 8))
    .abs();
    let facing_delta = (window_mean(&facing, width, height, center.0, center.1, 8)
        - window_mean(&bare, width, height, center.0, center.1, 8))
    .abs();
    assert!(
        grazing_delta < 0.025,
        "angle fade must hide a perpendicular receiver, delta {grazing_delta}"
    );
    assert!(
        facing_delta > 0.06,
        "angle fade must keep a facing receiver, delta {facing_delta}"
    );
    let (clip_w, clip_h) = scene.clip_extent();
    assert!(
        clip_w > 0 && clip_h > 0,
        "a visible decal must upload a tile quad"
    );
    assert!(
        clip_w < width && clip_h < height,
        "one decal's tile union must not fill the viewport ({clip_w}x{clip_h} vs {width}x{height})"
    );

    // --- receiver-normal relight: a rippled normal varies; a flat one does not ---
    let camera = VfxCamera::looking_at(Vec3::new(6.0, 5.0, 7.0), Vec3::new(2.2, 0.6, 0.0));
    let (scorch, mut scorch_s) = solid(
        Vec3::new(2.2, 1.02, 0.0),
        Vec3::new(0.45, 0.45, 0.25),
        [0.55, 0.48, 0.4],
        LAYER_WORLD,
    );
    scorch_s.channels = CHANNEL_ALBEDO | CHANNEL_NORMAL;
    scorch_s.normal_strength = 1.0;
    scorch_s.angle_fade = [0.0, 0.0];
    let mut frame = DecalFrame {
        width,
        height,
        camera,
        ground: false,
        mesh: true,
        mesh_ripple: 0.0,
        decals: vec![(scorch, scorch_s)],
        ..DecalFrame::default()
    };
    let flat = scene.render(&device, &frame).expect("flat normal");
    frame.mesh_ripple = 0.9;
    let rippled = scene.render(&device, &frame).expect("rippled normal");
    let top = must_project(&camera, width, height, Vec3::new(2.2, 1.0, 0.0));
    let flat_var = window_variance(&flat, width, height, top.0, top.1, 5);
    let rippled_var = window_variance(&rippled, width, height, top.0, top.1, 5);
    assert!(
        rippled_var > flat_var * 1.8 && rippled_var > 1.0e-4,
        "relight should follow the receiver normal (flat {flat_var}, ripple {rippled_var}) at {top:?}"
    );
    // --- rut strip follows the bow, not the chord ---
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 9.0, 11.0), Vec3::ZERO);
    let a = Vec3::new(-4.0, 0.02, 2.0);
    let bow = Vec3::new(0.0, 0.02, -2.0);
    let c = Vec3::new(4.0, 0.02, 2.0);
    let prefab = builtin_prefab("rut").expect("rut prefab");
    let (_, surface) = prefab
        .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
        .expect("rut instance");
    let strip = DecalStrip {
        points: sample_quadratic(a, bow, c, 16),
        normal: Vec3::Y,
        half_width: 0.22,
        half_depth: 0.15,
        spacing: 0.4,
        surface,
        lifetime: None,
    };
    let mut frame = DecalFrame {
        width,
        height,
        camera,
        ground: true,
        ..DecalFrame::default()
    };
    let ground = scene.render(&device, &frame).expect("rut baseline");
    frame.decals = strip.stamps();
    assert!(frame.decals.len() >= 8, "curve should emit several stamps");
    let ruts = scene.render(&device, &frame).expect("ruts");
    // The control point is not on the curve. t = 0.5 sits at z = 0, between the bow and the chord.
    let apex = Vec3::new(0.0, 0.02, 0.0);
    let bow_px = must_project(&camera, width, height, apex);
    let chord = Vec3::new(0.0, 0.02, 2.0);
    let chord_px = must_project(&camera, width, height, chord);
    let bow_l = window_mean(&ruts, width, height, bow_px.0, bow_px.1, 3);
    let chord_l = window_mean(&ruts, width, height, chord_px.0, chord_px.1, 3);
    let ground_bow = window_mean(&ground, width, height, bow_px.0, bow_px.1, 3);
    assert!(
        bow_l + 0.04 < chord_l && bow_l + 0.03 < ground_bow,
        "rut should darken the apex ({bow_l} at {bow_px:?}) more than the chord ({chord_l} at {chord_px:?}); ground apex {ground_bow}"
    );

    // --- a pad marking stays crisper than a soft scorch ---
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 8.0, 9.0), Vec3::ZERO);
    let mut frame = DecalFrame {
        width,
        height,
        camera,
        ground: true,
        checker: true,
        ..DecalFrame::default()
    };
    let base = scene.render(&device, &frame).expect("pad baseline");
    let pad = builtin_prefab("pad_marking").expect("pad");
    let (pad_d, pad_s) = pad
        .instantiate(Vec3::new(-1.6, 0.02, 0.2), Vec3::Y, 0.0, None)
        .expect("pad place");
    let scorch = builtin_prefab("scorch").expect("scorch");
    let (scorch_d, scorch_s) = scorch
        .instantiate(Vec3::new(1.8, 0.02, 0.2), Vec3::Y, 0.0, None)
        .expect("scorch place");
    frame.decals = vec![(pad_d, pad_s), (scorch_d, scorch_s)];
    let painted = scene.render(&device, &frame).expect("painted");
    let pad_px = must_project(&camera, width, height, Vec3::new(-1.6, 0.02, 0.2));
    let scorch_px = must_project(&camera, width, height, Vec3::new(1.8, 0.02, 0.2));
    let pad_partial = partial_fraction(&base, &painted, width, height, pad_px.0, pad_px.1, 28);
    let scorch_partial = partial_fraction(&base, &painted, width, height, scorch_px.0, scorch_px.1, 28);
    let pad_delta = (window_mean(&painted, width, height, pad_px.0, pad_px.1, 4)
        - window_mean(&base, width, height, pad_px.0, pad_px.1, 4))
    .abs();
    let scorch_delta = (window_mean(&painted, width, height, scorch_px.0, scorch_px.1, 4)
        - window_mean(&base, width, height, scorch_px.0, scorch_px.1, 4))
    .abs();
    // Both centres must move. Comparing only the partial fractions passes when the
    // second decal never draws (its fraction is zero, which is "crisper").
    assert!(
        pad_delta > 0.04 && scorch_delta > 0.04,
        "pad and scorch must both paint (pad {pad_delta} at {pad_px:?}, scorch {scorch_delta} at {scorch_px:?})"
    );
    assert!(
        pad_partial < scorch_partial,
        "pad edge should be crisper than scorch (pad {pad_partial}, scorch {scorch_partial})"
    );

    // --- emissive adds light the albedo-only decal does not ---
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 4.0, 6.0), Vec3::ZERO);
    let mut frame = DecalFrame {
        width,
        height,
        camera,
        ground: false,
        single_quad: true,
        ..DecalFrame::default()
    };
    let (glow, mut glow_s) = solid(Vec3::ZERO, Vec3::new(0.8, 0.8, 0.3), [0.0, 0.0, 0.0], LAYER_WORLD);
    glow_s.channels = CHANNEL_EMISSIVE;
    glow_s.emissive = [1.2, 0.15, 0.05];
    glow_s.angle_fade = [0.0, 0.0];
    frame.decals = vec![(glow, glow_s)];
    let lit = scene.render(&device, &frame).expect("emissive");
    glow_s.channels = CHANNEL_ALBEDO;
    glow_s.emissive = [0.0; 3];
    glow_s.albedo = [0.0; 3];
    frame.decals = vec![(glow, glow_s)];
    let dark = scene.render(&device, &frame).expect("no emissive");
    let px = must_project(&camera, width, height, Vec3::new(0.0, 0.05, 0.0));
    let glow_l = window_mean(&lit, width, height, px.0, px.1, 4);
    let dark_l = window_mean(&dark, width, height, px.0, px.1, 4);
    assert!(
        glow_l > dark_l + 0.05,
        "emissive should brighten the quad ({glow_l} vs {dark_l})"
    );

    // --- set piece: scorch on terrain and mesh, pad, ruts. Evidence when asked. ---
    let camera = VfxCamera::looking_at(Vec3::new(8.0, 6.5, 10.0), Vec3::new(1.0, 0.4, 0.4));
    let ew = 640u32;
    let eh = 360u32;
    let mut piece = DecalFrame {
        width: ew,
        height: eh,
        camera,
        ground: true,
        mesh: true,
        mesh_ripple: 0.75,
        checker: true,
        ..DecalFrame::default()
    };
    let empty = scene.render(&device, &piece).expect("set piece baseline");
    let (ground_scorch, gs) = builtin_prefab("scorch")
        .expect("scorch")
        .instantiate(Vec3::new(0.2, 0.02, 0.4), Vec3::Y, 0.4, None)
        .expect("ground scorch");
    let (mesh_scorch, ms) = builtin_prefab("scorch")
        .expect("scorch")
        .instantiate(Vec3::new(2.2, 1.02, 0.0), Vec3::Y, 0.0, None)
        .expect("mesh scorch");
    let (pad_d, pad_s) = builtin_prefab("pad_marking")
        .expect("pad")
        .instantiate(Vec3::new(0.4, 0.02, 2.4), Vec3::Y, 0.2, None)
        .expect("pad");
    let (_, rut_s) = builtin_prefab("rut")
        .expect("rut")
        .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
        .expect("rut");
    let rut = DecalStrip {
        points: sample_quadratic(
            Vec3::new(-3.0, 0.02, 3.2),
            Vec3::new(0.2, 0.02, 1.2),
            Vec3::new(3.4, 0.02, 3.0),
            14,
        ),
        normal: Vec3::Y,
        half_width: 0.2,
        half_depth: 0.14,
        spacing: 0.45,
        surface: rut_s,
        lifetime: None,
    };
    piece.decals = vec![(ground_scorch, gs), (mesh_scorch, ms), (pad_d, pad_s)];
    piece.decals.extend(rut.stamps());
    let shot = scene.render(&device, &piece).expect("set piece");
    let gpx = must_project(&camera, ew, eh, Vec3::new(0.2, 0.02, 0.4));
    let mpx = must_project(&camera, ew, eh, Vec3::new(2.2, 1.0, 0.0));
    let ppx = must_project(&camera, ew, eh, Vec3::new(0.4, 0.02, 2.4));
    let gd =
        (window_mean(&shot, ew, eh, gpx.0, gpx.1, 4) - window_mean(&empty, ew, eh, gpx.0, gpx.1, 4)).abs();
    let md =
        (window_mean(&shot, ew, eh, mpx.0, mpx.1, 3) - window_mean(&empty, ew, eh, mpx.0, mpx.1, 3)).abs();
    let pd =
        (window_mean(&shot, ew, eh, ppx.0, ppx.1, 4) - window_mean(&empty, ew, eh, ppx.0, ppx.1, 4)).abs();
    assert!(gd > 0.04, "scorch should mark the terrain ({gd} at {gpx:?})");
    assert!(md > 0.04, "scorch should mark the mesh top ({md} at {mpx:?})");
    assert!(
        pd > 0.04,
        "pad marking should read on the ground ({pd} at {ppx:?})"
    );

    let ms = measure_decal_pass(&device, 64, 320, 180).expect("budget frame");
    assert_decal_milliseconds(&device, ms);
}

/// Real-GPU timing. Lavapipe goldens do not use this number.
///
/// `flock /tmp/regia-gpu.lock flock /tmp/ferrite-gpu.lock env VK_LOADER_DRIVERS_SELECT='*nvidia*' cargo test -p regia_vfx --release --test decals decal_pass_budget -- --ignored --nocapture`
#[test]
#[ignore = "RTX timing; run under flock with the NVIDIA driver selected"]
fn decal_pass_budget_records_gpu_time() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let info = device.adapter.get_info();
    let ms = measure_decal_pass(&device, 256, 1920, 1080).expect("perf frame");
    if info.device_type == wgpu::DeviceType::Cpu {
        eprintln!(
            "skipping perf.md: CPU adapter {} reports {ms} ms; that is not the discrete-GPU number",
            info.name
        );
        return;
    }
    assert!(
        ms > 0.0,
        "non-CPU adapter {} must time the decal pass, got {ms}",
        info.name
    );
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/evidence/decals");
    std::fs::create_dir_all(&dir).expect("evidence dir");
    let body = format!(
        "# Decal pass\n\nAdapter: {} ({:?}, device type {:?}).\n\n256 projected decals at 1920×1080. One timestamp query around the decal pass (`measure_decal_pass`). Goldens are lavapipe frames, not this number.\n\nGPU time: {ms:.3} ms\n",
        info.name, info.backend, info.device_type
    );
    std::fs::write(dir.join("perf.md"), body).expect("write perf");
}

const HOOK_INDICES: [u32; 6] = [0, 2, 1, 0, 3, 2];

fn hook_quad(origin: [f32; 3], normal: [f32; 3], skinned: bool) -> Vec<LabVertex> {
    hook_quad_sized(origin, normal, 0.7, skinned)
}

/// A planar slope `y = rise * x` over `±half` in X and Z.
///
/// `up_normals` writes world +Y into every vertex. The positions stay the slope, so
/// depth derivatives and the receiver GBA disagree. `false` writes the slope normal.
fn slope_grid(half: f32, cells: u32, rise: f32, up_normals: bool) -> (Vec<LabVertex>, Vec<u32>) {
    let cells = cells.max(1);
    let n = cells + 1;
    let slope = regia_core::glam::Vec3::new(-rise, 1.0, 0.0).normalize();
    let normal = if up_normals {
        [0.0, 1.0, 0.0]
    } else {
        [slope.x, slope.y, slope.z]
    };
    let mut verts = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = -half + col as f32 / cells as f32 * (2.0 * half);
            let z = -half + row as f32 / cells as f32 * (2.0 * half);
            verts.push(LabVertex {
                position: [x, rise * x, z],
                normal,
                uv: [0.0, 0.0],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
    }
    let mut indices = Vec::new();
    for row in 0..cells {
        for col in 0..cells {
            let i00 = row * n + col;
            let i10 = i00 + 1;
            let i01 = i00 + n;
            let i11 = i01 + 1;
            // Same winding as [`hook_quad`]: triangle (0, 2, 1) faces +Y on a flat grid.
            indices.extend([i00, i11, i10, i00, i01, i11]);
        }
    }
    (verts, indices)
}

fn slope_frame<'a>(
    key: u64,
    vertices: &'a [LabVertex],
    indices: &'a [u32],
    material: &'a LabMaterial,
) -> FrameMesh<'a> {
    FrameMesh {
        is_static: false,
        mesh_key: key,
        material_key: 7,
        object_key: key,
        vertices,
        indices,
        material,
        model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: regia_render::ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    }
}

fn hook_quad_sized(origin: [f32; 3], normal: [f32; 3], half: f32, skinned: bool) -> Vec<LabVertex> {
    let corners = [
        [origin[0] - half, origin[1], origin[2] - half],
        [origin[0] + half, origin[1], origin[2] - half],
        [origin[0] + half, origin[1], origin[2] + half],
        [origin[0] - half, origin[1], origin[2] + half],
    ];
    let weights = if skinned { [1.0, 0.0, 0.0, 0.0] } else { [0.0; 4] };
    corners
        .into_iter()
        .map(|position| LabVertex {
            position,
            normal,
            uv: [0.0, 0.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights,
        })
        .collect()
}

/// The frame-hook scenes' sun: travel direction (from the sun toward the scene).
const HOOK_SUN_TRAVEL: [f32; 3] = [0.3, -1.0, 0.2];

#[allow(clippy::too_many_arguments)]
fn hook_render(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    gpu: &Arc<Mutex<VfxGpu>>,
    eye: [f32; 3],
    look: [f32; 3],
    fov: f32,
    meshes: Vec<FrameMesh<'_>>,
    decals: &[Decal],
    surfaces: &[DecalSurface],
) -> Vec<u8> {
    let sun = DirectionalLightUniform::new(HOOK_SUN_TRAVEL, [1.0, 1.0, 1.0], 20_000.0);
    hook_render_lit(
        device, renderer, gpu, eye, look, fov, meshes, decals, surfaces, sun,
    )
}

/// [`hook_render`] under a chosen sun.
#[allow(clippy::too_many_arguments)]
fn hook_render_lit(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    gpu: &Arc<Mutex<VfxGpu>>,
    eye: [f32; 3],
    look: [f32; 3],
    fov: f32,
    meshes: Vec<FrameMesh<'_>>,
    decals: &[Decal],
    surfaces: &[DecalSurface],
    sun: DirectionalLightUniform,
) -> Vec<u8> {
    {
        let mut locked = gpu.lock().expect("vfx gpu");
        locked.write_decals_with(decals, surfaces);
    }
    let scene = FrameScene {
        width: 160,
        height: 120,
        eye,
        target: look,
        fov_y: fov,
        near: 0.1,
        far: 40.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 300.0,
        double_sided: false,
        post: None,
        fog: FogSettings::default(),
        directional: sun,
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes,
    };
    renderer
        .render_offscreen(device, &scene)
        .expect("frame hook")
        .pixels
}

fn mesh<'a>(
    key: u64,
    vertices: &'a [LabVertex],
    joints: &'a [JointMatrix],
    material: &'a LabMaterial,
) -> FrameMesh<'a> {
    FrameMesh {
        is_static: false,
        mesh_key: key,
        material_key: 7,
        object_key: key,
        vertices,
        indices: &HOOK_INDICES,
        material,
        model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
        joints,
        receiver_layer: 0,
        shadow: regia_render::ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    }
}

/// The runtime frame hook, not [`DecalScene`]: a character layer stays clean (skinned or a
/// rigid mesh stamped as a character), a terrain-only mask rejects a static mesh, a grazing
/// receiver normal hides the decal, and roughness changes the shaded pixel.
#[test]
fn frame_hook_masks_characters_fades_grazing_and_roughness() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let gpu = Arc::new(Mutex::new(VfxGpu::new(&device, profile()).expect("vfx gpu")));
    let mut renderer = FrameRenderer::new();
    renderer.set_translucent_layers(vec![translucent_layer(Arc::clone(&gpu))]);
    let material = LabMaterial {
        base_color: [0.55, 0.55, 0.55, 1.0],
        unlit: true,
        ..LabMaterial::EMPTY
    };
    let static_v = hook_quad([-1.15, 0.0, 0.0], [0.0, 1.0, 0.0], false);
    let char_v = hook_quad([1.15, 0.0, 0.0], [0.0, 1.0, 0.0], true);
    let char_j = [JointMatrix::identity()];
    let flat_v = hook_quad([0.0, 0.0, 0.0], [0.0, 1.0, 0.0], false);
    let eye = [0.0, 4.2, 6.2];
    let look = [0.0, 0.0, 0.0];
    let fov = 0.9f32;
    let camera = VfxCamera {
        eye: Vec3::from_array(eye),
        target: Vec3::from_array(look),
        fov_y: fov,
        near: 0.1,
        far: 40.0,
    };
    let width = 160u32;
    let height = 120u32;
    let bare = hook_render(
        &device,
        &mut renderer,
        &gpu,
        eye,
        look,
        fov,
        vec![
            mesh(11, &static_v, &[], &material),
            mesh(12, &char_v, &char_j, &material),
        ],
        &[],
        &[],
    );
    let (decal, mut surface) = solid(
        Vec3::new(0.0, 0.04, 0.0),
        Vec3::new(2.4, 1.2, 0.5),
        [0.02, 0.01, 0.0],
        LAYER_WORLD,
    );
    surface.blend = 0.0;
    let painted = hook_render(
        &device,
        &mut renderer,
        &gpu,
        eye,
        look,
        fov,
        vec![
            mesh(11, &static_v, &[], &material),
            mesh(12, &char_v, &char_j, &material),
        ],
        &[decal],
        &[surface],
    );
    let static_px = must_project(&camera, width, height, Vec3::new(-1.15, 0.0, 0.0));
    let char_px = must_project(&camera, width, height, Vec3::new(1.15, 0.0, 0.0));
    let static_delta = (window_mean(&painted, width, height, static_px.0, static_px.1, 3)
        - window_mean(&bare, width, height, static_px.0, static_px.1, 3))
    .abs();
    let char_delta = (window_mean(&painted, width, height, char_px.0, char_px.1, 3)
        - window_mean(&bare, width, height, char_px.0, char_px.1, 3))
    .abs();
    assert!(
        static_delta > 0.05,
        "static mesh should take the decal, delta {static_delta} at {static_px:?}"
    );
    assert!(
        char_delta < 0.02,
        "character layer must stay clean, delta {char_delta} at {char_px:?}"
    );

    // Same heightfield slope. Vertex normals are either the slope or world +Y.
    // Depth derivatives see the slope either way, so a shader that ignores receiver
    // GBA hides both. Angle fade [0.3, 0.6] hides ny ≈ 0.196 and keeps +Y.
    let (slope_pos, slope_idx) = slope_grid(0.9, 4, 5.0, false);
    let (facing_pos, facing_idx) = slope_grid(0.9, 4, 5.0, true);
    let bare_graze = hook_render(
        &device,
        &mut renderer,
        &gpu,
        eye,
        look,
        fov,
        vec![slope_frame(13, &slope_pos, &slope_idx, &material)],
        &[],
        &[],
    );
    let (graze_decal, mut graze_surface) = solid(
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(1.4, 1.4, 3.0),
        [0.02, 0.01, 0.0],
        LAYER_WORLD,
    );
    graze_surface.angle_fade = [0.3, 0.6];
    let hidden = hook_render(
        &device,
        &mut renderer,
        &gpu,
        eye,
        look,
        fov,
        vec![slope_frame(13, &slope_pos, &slope_idx, &material)],
        &[graze_decal],
        &[graze_surface],
    );
    let shown = hook_render(
        &device,
        &mut renderer,
        &gpu,
        eye,
        look,
        fov,
        vec![slope_frame(14, &facing_pos, &facing_idx, &material)],
        &[graze_decal],
        &[graze_surface],
    );
    let center = must_project(&camera, width, height, Vec3::ZERO);
    let hidden_delta = (window_mean(&hidden, width, height, center.0, center.1, 2)
        - window_mean(&bare_graze, width, height, center.0, center.1, 2))
    .abs();
    let bare_facing = hook_render(
        &device,
        &mut renderer,
        &gpu,
        eye,
        look,
        fov,
        vec![slope_frame(14, &facing_pos, &facing_idx, &material)],
        &[],
        &[],
    );
    let shown_delta = (window_mean(&shown, width, height, center.0, center.1, 2)
        - window_mean(&bare_facing, width, height, center.0, center.1, 2))
    .abs();
    assert!(
        hidden_delta < 0.02,
        "grazing receiver normal must hide the decal, delta {hidden_delta}"
    );
    assert!(
        shown_delta > 0.05,
        "facing receiver normal must show the decal, delta {shown_delta} (ignoring GBA hides both)"
    );

    // The glint follows the frame's sun: stand in its mirror direction.
    let sun = -Vec3::from_array(HOOK_SUN_TRAVEL).normalize();
    let incident = -sun;
    let refl = (incident - Vec3::Y * (2.0 * incident.dot(Vec3::Y))).normalize();
    let spec_eye = (refl * 6.0).to_array();
    let spec_camera = VfxCamera {
        eye: Vec3::from_array(spec_eye),
        target: Vec3::ZERO,
        fov_y: 0.7,
        near: 0.1,
        far: 40.0,
    };
    let (spec_decal, mut spec_surface) = solid(
        Vec3::new(0.0, 0.02, 0.0),
        Vec3::new(0.6, 0.6, 0.35),
        [1.0, 1.0, 1.0],
        LAYER_WORLD,
    );
    spec_surface.channels = CHANNEL_ROUGHNESS;
    spec_surface.roughness = 0.0;
    spec_surface.angle_fade = [0.0, 0.0];
    let smooth = hook_render(
        &device,
        &mut renderer,
        &gpu,
        spec_eye,
        [0.0, 0.0, 0.0],
        0.7,
        vec![mesh(15, &flat_v, &[], &material)],
        &[spec_decal],
        &[spec_surface],
    );
    spec_surface.roughness = 1.0;
    let rough = hook_render(
        &device,
        &mut renderer,
        &gpu,
        spec_eye,
        [0.0, 0.0, 0.0],
        0.7,
        vec![mesh(15, &flat_v, &[], &material)],
        &[spec_decal],
        &[spec_surface],
    );
    let spec_px = must_project(&spec_camera, width, height, Vec3::ZERO);
    let smooth_l = window_mean(&smooth, width, height, spec_px.0, spec_px.1, 1);
    let rough_l = window_mean(&rough, width, height, spec_px.0, spec_px.1, 1);
    assert!(
        (smooth_l - rough_l).abs() > 0.04,
        "roughness must change the shaded pixel ({smooth_l} vs {rough_l} at {spec_px:?})"
    );

    // A rigid quad with an explicit character layer must stay clean. No joints.
    let rigid_char_v = hook_quad_sized([0.0, 0.0, 0.0], [0.0, 1.0, 0.0], 0.32, false);
    let mut rigid_char = mesh(16, &rigid_char_v, &[], &material);
    rigid_char.receiver_layer = 2;
    let bare_rigid = hook_render(
        &device,
        &mut renderer,
        &gpu,
        eye,
        look,
        fov,
        vec![
            mesh(11, &static_v, &[], &material),
            rigid_char,
            mesh(12, &char_v, &char_j, &material),
        ],
        &[],
        &[],
    );
    rigid_char.receiver_layer = 2;
    let painted_rigid = hook_render(
        &device,
        &mut renderer,
        &gpu,
        eye,
        look,
        fov,
        vec![
            mesh(11, &static_v, &[], &material),
            rigid_char,
            mesh(12, &char_v, &char_j, &material),
        ],
        &[decal],
        &[surface],
    );
    let rigid_px = must_project(&camera, width, height, Vec3::ZERO);
    let rigid_delta = (window_mean(&painted_rigid, width, height, rigid_px.0, rigid_px.1, 2)
        - window_mean(&bare_rigid, width, height, rigid_px.0, rigid_px.1, 2))
    .abs();
    assert!(
        rigid_delta < 0.02,
        "rigid character layer must stay clean, delta {rigid_delta} at {rigid_px:?}"
    );

    // Terrain-only mask rejects the rigid static mesh (it is layer 1, not the clear).
    let bare_static = hook_render(
        &device,
        &mut renderer,
        &gpu,
        eye,
        look,
        fov,
        vec![mesh(11, &static_v, &[], &material)],
        &[],
        &[],
    );
    let (terrain_decal, mut terrain_surface) = solid(
        Vec3::new(-1.15, 0.04, 0.0),
        Vec3::new(0.9, 0.9, 0.4),
        [0.02, 0.01, 0.0],
        LAYER_TERRAIN,
    );
    terrain_surface.blend = 0.0;
    let rejected = hook_render(
        &device,
        &mut renderer,
        &gpu,
        eye,
        look,
        fov,
        vec![mesh(11, &static_v, &[], &material)],
        &[terrain_decal],
        &[terrain_surface],
    );
    let rejected_delta = (window_mean(&rejected, width, height, static_px.0, static_px.1, 3)
        - window_mean(&bare_static, width, height, static_px.0, static_px.1, 3))
    .abs();
    assert!(
        rejected_delta < 0.02,
        "terrain-only mask must not paint a static mesh, delta {rejected_delta} at {static_px:?}"
    );
}

fn window_rgb(pixels: &[u8], width: u32, height: u32, cx: u32, cy: u32, radius: u32) -> [f32; 3] {
    let mut sum = [0.0f32; 3];
    let mut n = 0.0f32;
    let x0 = cx.saturating_sub(radius);
    let y0 = cy.saturating_sub(radius);
    let x1 = (cx + radius).min(width - 1);
    let y1 = (cy + radius).min(height - 1);
    for y in y0..=y1 {
        for x in x0..=x1 {
            let px = at(pixels, width, x, y);
            sum[0] += f32::from(px[0]) / 255.0;
            sum[1] += f32::from(px[1]) / 255.0;
            sum[2] += f32::from(px[2]) / 255.0;
            n += 1.0;
        }
    }
    let n = n.max(1.0);
    [sum[0] / n, sum[1] / n, sum[2] / n]
}

/// Outward faces. Winding matches [`hook_quad`] (triangle 0,2,1 faces `normal`).
fn push_face(
    verts: &mut Vec<LabVertex>,
    indices: &mut Vec<u32>,
    center: [f32; 3],
    axis_u: [f32; 3],
    axis_v: [f32; 3],
    normal: [f32; 3],
) {
    let base = u32::try_from(verts.len()).unwrap_or(0);
    let corner = |su: f32, sv: f32| {
        [
            center[0] + axis_u[0] * su + axis_v[0] * sv,
            center[1] + axis_u[1] * su + axis_v[1] * sv,
            center[2] + axis_u[2] * su + axis_v[2] * sv,
        ]
    };
    for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
        verts.push(LabVertex::from_pos_normal_uv(corner(su, sv), normal, [0.0, 0.0]));
    }
    indices.extend([base, base + 2, base + 1, base, base + 3, base + 2]);
}

fn crate_mesh(center: [f32; 3], half: [f32; 3]) -> (Vec<LabVertex>, Vec<u32>) {
    let mut verts = Vec::new();
    let mut indices = Vec::new();
    let (hx, hy, hz) = (half[0], half[1], half[2]);
    push_face(
        &mut verts,
        &mut indices,
        [center[0], center[1] + hy, center[2]],
        [hx, 0.0, 0.0],
        [0.0, 0.0, hz],
        [0.0, 1.0, 0.0],
    );
    push_face(
        &mut verts,
        &mut indices,
        [center[0] + hx, center[1], center[2]],
        [0.0, hy, 0.0],
        [0.0, 0.0, -hz],
        [1.0, 0.0, 0.0],
    );
    push_face(
        &mut verts,
        &mut indices,
        [center[0] - hx, center[1], center[2]],
        [0.0, hy, 0.0],
        [0.0, 0.0, hz],
        [-1.0, 0.0, 0.0],
    );
    // +Z and -Z: `cross(axis_v, axis_u)` is the geometric normal for this winding.
    push_face(
        &mut verts,
        &mut indices,
        [center[0], center[1], center[2] + hz],
        [hx, 0.0, 0.0],
        [0.0, -hy, 0.0],
        [0.0, 0.0, 1.0],
    );
    push_face(
        &mut verts,
        &mut indices,
        [center[0], center[1], center[2] - hz],
        [hx, 0.0, 0.0],
        [0.0, hy, 0.0],
        [0.0, 0.0, -1.0],
    );
    (verts, indices)
}

fn offset_path(points: &[Vec3], normal: Vec3, distance: f32) -> Vec<Vec3> {
    points
        .iter()
        .enumerate()
        .map(|(i, point)| {
            let prev = if i == 0 { *point } else { points[i - 1] };
            let next = if i + 1 == points.len() {
                *point
            } else {
                points[i + 1]
            };
            let tangent = (next - prev).normalize_or_zero();
            let mut side = normal.cross(tangent).normalize_or_zero();
            if side.length_squared() < 1e-8 {
                side = Vec3::X;
            }
            *point + side * distance
        })
        .collect()
}

struct SetPiece {
    decals: Vec<Decal>,
    surfaces: Vec<DecalSurface>,
    eye: [f32; 3],
    look: [f32; 3],
    crate_center: [f32; 3],
    crate_half: [f32; 3],
    clear: Vec3,
    /// Index of the first rut stamp in `decals` (scorch, crate scorch and pad come first).
    rut_first: usize,
}

fn on_ground(map: &TerrainMap, x: f32, z: f32, lift: f32) -> Vec3 {
    Vec3::new(x, map.sample_bilinear(x, z) + lift, z)
}

fn set_piece_decals(map: &TerrainMap) -> SetPiece {
    let scorch_at = on_ground(map, -0.95, 1.65, 0.03);
    let (mut ground_scorch, mut gs) = builtin_prefab("scorch")
        .expect("scorch")
        .instantiate(scorch_at, Vec3::Y, 0.3, None)
        .expect("ground scorch");
    // Prefab channels stay albedo+normal+roughness. A non-zero normal_xy is the
    // dent; roughness below the snow's sheen is the char. Do not strip channels.
    gs.albedo = [0.22, 0.08, 0.035];
    // A modest authored tilt. The bowl itself comes from the atlas luminance slope.
    gs.normal_xy = [0.28, 0.10];
    gs.roughness = 0.72;
    ground_scorch.color = [gs.albedo[0], gs.albedo[1], gs.albedo[2], gs.opacity];
    ground_scorch.half_size = Vec3::new(0.72, 0.72, 0.4);
    let crate_half = [0.7, 0.28, 0.7];
    let crate_xz = on_ground(map, 1.25, 2.05, crate_half[1]);
    let (mut mesh_scorch, mut ms) = builtin_prefab("scorch")
        .expect("scorch")
        .instantiate(
            Vec3::new(crate_xz.x, crate_xz.y + crate_half[1] + 0.02, crate_xz.z),
            Vec3::Y,
            0.0,
            None,
        )
        .expect("mesh scorch");
    ms.albedo = gs.albedo;
    ms.normal_xy = gs.normal_xy;
    ms.roughness = gs.roughness;
    mesh_scorch.color = ground_scorch.color;
    mesh_scorch.half_size = Vec3::new(0.42, 0.42, 0.18);
    let pad_at = on_ground(map, 0.0, 3.7, 0.02);
    let (mut pad, mut pad_s) = builtin_prefab("pad_marking")
        .expect("pad")
        .instantiate(pad_at, Vec3::Y, 0.0, None)
        .expect("pad");
    // Near-white prefab paint disappears on snow. A saturated yellow stays a stripe.
    // The prefab is albedo-only replace with a hard edge: that is the crisp paint.
    pad_s.albedo = [0.86, 0.55, 0.04];
    pad.color = [pad_s.albedo[0], pad_s.albedo[1], pad_s.albedo[2], pad_s.opacity];
    pad.half_size = Vec3::new(2.1, 0.32, 0.28);
    pad_s.edge_softness = 0.0;
    let (_, mut rut_s) = builtin_prefab("rut")
        .expect("rut")
        .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
        .expect("rut");
    rut_s.albedo = [0.10, 0.065, 0.04];
    rut_s.normal_xy = [0.32, 0.0];
    rut_s.normal_strength = 1.0;
    rut_s.channels |= CHANNEL_ROUGHNESS;
    rut_s.roughness = 1.0;
    rut_s.edge_softness = 0.0;
    let lift = |p: Vec3| on_ground(map, p.x, p.z, 0.05);
    let curve = sample_quadratic(
        Vec3::new(-1.7, 0.0, 0.05),
        Vec3::new(-0.15, 0.0, 0.55),
        Vec3::new(0.75, 0.0, 1.25),
        18,
    )
    .into_iter()
    .map(lift)
    .collect::<Vec<_>>();
    let mut decals = vec![ground_scorch, mesh_scorch, pad];
    let mut surfaces = vec![gs, ms, pad_s];
    let rut_first = decals.len();
    for side in [-0.16f32, 0.16] {
        let points = offset_path(&curve, Vec3::Y, side);
        let strip = DecalStrip {
            points,
            normal: Vec3::Y,
            half_width: 0.16,
            half_depth: 0.45,
            spacing: 0.22,
            surface: rut_s,
            lifetime: None,
        };
        for (decal, surface) in strip.stamps() {
            decals.push(decal);
            surfaces.push(surface);
        }
    }
    let eye_xz = on_ground(map, 0.15, 6.3, 1.7);
    let look_xz = on_ground(map, 0.1, 0.15, 0.1);
    SetPiece {
        decals,
        surfaces,
        eye: eye_xz.to_array(),
        look: look_xz.to_array(),
        crate_center: crate_xz.to_array(),
        crate_half,
        clear: on_ground(map, 2.35, 2.65, 0.0),
        rut_first,
    }
}

struct ShotLight {
    crate_color: [f32; 3],
    sun_color: [f32; 3],
    sun_lux: f32,
    ambient_lux: f32,
}

/// Camera, frame size and decal list of one set-piece render.
struct LookShot<'a> {
    eye: [f32; 3],
    target: [f32; 3],
    width: u32,
    height: u32,
    decals: &'a [Decal],
    surfaces: &'a [DecalSurface],
}

fn look_render(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    gpu: &Arc<Mutex<VfxGpu>>,
    terrain: TerrainSharedDraw,
    light: ShotLight,
    piece: &SetPiece,
    shot: LookShot<'_>,
) -> Vec<u8> {
    {
        let mut locked = gpu.lock().expect("vfx gpu");
        locked.write_decals_with(shot.decals, shot.surfaces);
    }
    renderer.set_shared_draw(Some(Box::new(terrain) as Box<dyn SharedPassDraw + Send + Sync>));
    let (crate_v, crate_i) = crate_mesh(piece.crate_center, piece.crate_half);
    let crate_mat = LabMaterial {
        base_color: [
            light.crate_color[0],
            light.crate_color[1],
            light.crate_color[2],
            1.0,
        ],
        roughness: 0.35,
        metallic: 0.06,
        ..LabMaterial::EMPTY
    };
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 14.5;
    let travel = Vec3::from_array(SETPIECE_SUN_TRAVEL).normalize();
    let scene = FrameScene {
        width: shot.width,
        height: shot.height,
        eye: shot.eye,
        target: shot.target,
        fov_y: 0.8,
        near: 0.08,
        far: 180.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: light.ambient_lux,
        double_sided: false,
        post: Some(&post),
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new(travel.to_array(), light.sun_color, light.sun_lux),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 22,
            material_key: 22,
            object_key: 22,
            vertices: &crate_v,
            indices: &crate_i,
            material: &crate_mat,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    };
    renderer
        .render_offscreen(device, &scene)
        .expect("set piece")
        .pixels
}

fn piece_map() -> TerrainMap {
    let n = 64u32;
    let world = 40.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
            let z = row as f32 / (n - 1) as f32 * world - world * 0.5;
            // A shallow valley the camera looks down, plus drifts beside it, so the
            // ground is displaced and the decals still sit on slopes gentler than
            // the prefab angle fade.
            let valley = 0.4 * (1.0 - (-(x * x) / 36.0).exp());
            let roll = 0.22 * (z * 0.2).sin() + 0.12 * (x * 0.18).cos();
            let drift = 0.75 * (-((x + 3.6) * (x + 3.6) / 5.0 + (z - 0.1) * (z - 0.1) / 12.0)).exp();
            let lumps = 0.05 * (x * 2.2).sin() * (z * 1.7).cos();
            // A far dune, not a box wall, so the finite patch meets the sky
            // without a black gap or a corner pillar.
            let far = ((-z) - (world * 0.5 - 9.0)).max(0.0);
            let rim = 2.8 * (far / 9.0).clamp(0.0, 1.0);
            heights.push(valley + roll + drift + lumps + rim);
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 1.0, 0.0).expect("piece map")
}

fn wide_layer(name: &str, albedo: [f32; 3], tiling: f32, detail: f32) -> LayerDesc {
    LayerDesc {
        name: name.into(),
        albedo,
        roughness: 0.82,
        tiling,
        slope_min: 0.0,
        slope_max: 1.2,
        altitude_min: -40.0,
        altitude_max: 40.0,
        blend_sharpness: 0.18,
        triplanar: false,
        detail_normal_strength: detail,
        height_blend: 0.6,
        ..LayerDesc::ground()
    }
}

fn piece_material(styles: &[(LayerStyle, u32)]) -> TerrainMaterial {
    TerrainMaterial {
        layers: styles
            .iter()
            .map(|(style, seed)| Some(style.bake(128, *seed)))
            .collect(),
        macro_set: None,
        params: MaterialParams {
            macro_world_size: 0.0,
            macro_strength: 0.0,
            macro_normal_strength: 0.0,
            anti_tiling_cell: 1.5,
            detail_fade_start: 5.0,
            detail_fade_end: 22.0,
            triplanar_auto_slope: 1.2,
        },
    }
}

fn piece_terrain(
    device: &RenderDevice,
    layers: &TerrainLayers,
    material: &TerrainMaterial,
) -> TerrainSharedDraw {
    let mut draw =
        TerrainSharedDraw::new(device, piece_map(), layers, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(material, &[]).expect("material");
    draw
}

/// The set piece's sun, as travel (from the sun toward the scene). The decal shader relights
/// with the frame's sun; the relief metric splits each footprint along its ground direction.
const SETPIECE_SUN_TRAVEL: [f32; 3] = [0.58, -0.40, -0.30];

/// The blessed decal-only expectations (`REGIA_WRITE_DECAL_EVIDENCE=1` rewrites them).
const SETPIECE_EXPECTATIONS: &str = "setpiece_expectations.ron";

/// Which ground the set piece stands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PieceGround {
    Snow,
    Mars,
}

/// One decal-space sample of a feature: the pixel it lands on and its signed offset
/// along the decal light's ground direction (`> 0` toward the light).
#[derive(Clone, Copy)]
struct FootprintSample {
    pixel: (u32, u32),
    toward_light: f32,
}

/// What a decal feature contributes inside its footprint, from a with/without pair.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct FeatureResponse {
    /// Σ with / Σ without over the footprint, linear RGB per channel. Below 1 darkens;
    /// a spread between channels is the paint colour.
    transfer: [f32; 3],
    /// Mean log luma ratio on the light-facing half minus the far half: the shading the
    /// normal channel adds (the atlas bowl or groove and the authored tilt). An albedo
    /// pattern that is symmetric about the decal centre leaves it near zero.
    relief: f32,
}

/// The blessed responses of one ground.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct SetPieceResponse {
    /// Scorch on the terrain.
    scorch: FeatureResponse,
    /// Scorch on the rigid crate's top face.
    crate_scorch: FeatureResponse,
    /// Twin ruts along the curve.
    rut: FeatureResponse,
    /// The yellow pad stripe.
    pad: FeatureResponse,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct SetPieceExpectations {
    snow: SetPieceResponse,
    mars: SetPieceResponse,
}

fn srgb_to_linear(byte: u8) -> f32 {
    let c = f32::from(byte) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_rgb(pixels: &[u8], width: u32, pixel: (u32, u32)) -> [f32; 3] {
    let px = at(pixels, width, pixel.0, pixel.1);
    [
        srgb_to_linear(px[0]),
        srgb_to_linear(px[1]),
        srgb_to_linear(px[2]),
    ]
}

fn decal_light_ground_dir() -> Vec3 {
    Vec3::new(-SETPIECE_SUN_TRAVEL[0], 0.0, -SETPIECE_SUN_TRAVEL[2]).normalize()
}

/// Samples the inner `inner` fraction of each decal box on its receiver (`surface_y`
/// gives the receiver height under a world xz) at decal-space spacing `step` metres.
fn footprint(
    camera: &VfxCamera,
    width: u32,
    height: u32,
    decals: &[Decal],
    inner: f32,
    step: f32,
    surface_y: &dyn Fn(f32, f32) -> f32,
) -> Vec<FootprintSample> {
    let light = decal_light_ground_dir();
    let mut out = Vec::new();
    for decal in decals {
        let right = decal.rotation * Vec3::X;
        let up = decal.rotation * Vec3::Y;
        let hx = decal.half_size.x * inner;
        let hy = decal.half_size.y * inner;
        let nx = ((2.0 * hx / step).ceil() as i32).max(2);
        let ny = ((2.0 * hy / step).ceil() as i32).max(2);
        for j in 0..=ny {
            for i in 0..=nx {
                let u = -hx + 2.0 * hx * i as f32 / nx as f32;
                let v = -hy + 2.0 * hy * j as f32 / ny as f32;
                let offset = right * u + up * v;
                let flat = decal.position + offset;
                let world = Vec3::new(flat.x, surface_y(flat.x, flat.z), flat.z);
                if let Some(pixel) = project_pixel(camera, width, height, world) {
                    out.push(FootprintSample {
                        pixel,
                        toward_light: Vec3::new(offset.x, 0.0, offset.z).dot(light),
                    });
                }
            }
        }
    }
    out
}

fn feature_response(with: &[u8], without: &[u8], width: u32, samples: &[FootprintSample]) -> FeatureResponse {
    let mut sum_with = [0.0f64; 3];
    let mut sum_without = [0.0f64; 3];
    let (mut near, mut near_n, mut far, mut far_n) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let luma_lin = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    for sample in samples {
        let w = linear_rgb(with, width, sample.pixel);
        let o = linear_rgb(without, width, sample.pixel);
        for c in 0..3 {
            sum_with[c] += f64::from(w[c]);
            sum_without[c] += f64::from(o[c]);
        }
        let log_ratio = f64::from(((luma_lin(w) + 1e-3) / (luma_lin(o) + 1e-3)).ln());
        if sample.toward_light > 0.0 {
            near += log_ratio;
            near_n += 1.0;
        } else if sample.toward_light < 0.0 {
            far += log_ratio;
            far_n += 1.0;
        }
    }
    let transfer = [0, 1, 2].map(|c| (sum_with[c] / sum_without[c].max(1e-9)) as f32);
    let relief = (near / near_n.max(1.0) - far / far_n.max(1.0)) as f32;
    FeatureResponse { transfer, relief }
}

/// Everything one set piece needs to render either ground from either camera.
struct PieceRig {
    device: RenderDevice,
    gpu: Arc<Mutex<VfxGpu>>,
    renderer: FrameRenderer,
    map: TerrainMap,
    piece: SetPiece,
}

impl PieceRig {
    fn new() -> Self {
        let device = device();
        let gpu = Arc::new(Mutex::new(
            VfxGpu::new(
                &device,
                VfxProfile {
                    particle_capacity: 64,
                    decal_capacity: 256,
                    sort: false,
                    light_budget: 0,
                    screen_floor_px: 0.0,
                    smoke_budget: 0,
                    soft_particle_res: regia_vfx::gpu::SoftParticleRes::Full,
                    event_capacity: 16,
                    child_capacity: 16,
                },
            )
            .expect("vfx gpu"),
        ));
        let mut renderer = FrameRenderer::new();
        renderer.set_translucent_layers(vec![translucent_layer(Arc::clone(&gpu))]);
        let map = piece_map();
        let piece = set_piece_decals(&map);
        Self {
            device,
            gpu,
            renderer,
            map,
            piece,
        }
    }

    /// The evidence camera and its frame size.
    fn camera(&self) -> (VfxCamera, u32, u32) {
        (
            VfxCamera {
                eye: Vec3::from_array(self.piece.eye),
                target: Vec3::from_array(self.piece.look),
                fov_y: 0.8,
                near: 0.08,
                far: 180.0,
            },
            960,
            540,
        )
    }

    /// Renders `ground` with `surfaces` (`None`: no decals at all).
    fn render(&mut self, ground: PieceGround, surfaces: Option<&[DecalSurface]>) -> Vec<u8> {
        let (camera, width, height) = self.camera();
        let (decals, surfaces): (&[Decal], &[DecalSurface]) = match surfaces {
            Some(surfaces) => (&self.piece.decals, surfaces),
            None => (&[], &[]),
        };
        let (light, terrain) = match ground {
            PieceGround::Snow => {
                // Airless medium (black sky). The preset sun is 4.4 klx, which at EV 14.5
                // crushes snow. 110 klx climbs AgX's shoulder and flattens the dunes, so the
                // shot keeps the medium and uses 75 klx: daylight, with the bake visible.
                let snow_lux = 75_000.0;
                let mut airless = AtmosphereParameters::airless();
                airless.sun_illuminance = snow_lux;
                self.renderer.set_outdoor(OutdoorParams {
                    planet: airless,
                    ..OutdoorParams::default()
                });
                let layers = TerrainLayers {
                    version: TERRAIN_LAYERS_FORMAT_VERSION,
                    wetness_default: 0.0,
                    layers: vec![
                        wide_layer("snow", [0.98, 0.99, 1.0], 2.4, 0.85),
                        wide_layer("drift", [0.90, 0.94, 0.99], 4.8, 0.4),
                    ],
                    puddles: true,
                };
                (
                    ShotLight {
                        crate_color: [0.62, 0.64, 0.68],
                        sun_color: [0.95, 0.97, 1.0],
                        sun_lux: snow_lux,
                        ambient_lux: 2_500.0,
                    },
                    piece_terrain(
                        &self.device,
                        &layers,
                        &piece_material(&[(LayerStyle::Snow, 3), (LayerStyle::Snow, 0x51f3)]),
                    ),
                )
            }
            PieceGround::Mars => {
                let planet = AtmosphereParameters::mars();
                let lux = planet.sun_illuminance;
                self.renderer.set_outdoor(OutdoorParams {
                    planet,
                    ..OutdoorParams::default()
                });
                let layers = TerrainLayers {
                    version: TERRAIN_LAYERS_FORMAT_VERSION,
                    wetness_default: 0.0,
                    layers: vec![
                        wide_layer("dust", [1.0, 0.94, 0.82], 2.8, 0.7),
                        wide_layer("sand", [1.0, 0.90, 0.72], 5.2, 0.35),
                    ],
                    puddles: true,
                };
                (
                    ShotLight {
                        crate_color: [0.42, 0.24, 0.12],
                        sun_color: [1.0, 0.84, 0.64],
                        sun_lux: lux,
                        ambient_lux: 4_000.0,
                    },
                    piece_terrain(
                        &self.device,
                        &layers,
                        &piece_material(&[(LayerStyle::Dirt, 9), (LayerStyle::Sand, 21)]),
                    ),
                )
            }
        };
        look_render(
            &self.device,
            &mut self.renderer,
            &self.gpu,
            terrain,
            light,
            &self.piece,
            LookShot {
                eye: camera.eye.to_array(),
                target: camera.target.to_array(),
                width,
                height,
                decals,
                surfaces,
            },
        )
    }

    fn ground_y(&self) -> impl Fn(f32, f32) -> f32 + '_ {
        |x, z| self.map.sample_bilinear(x, z)
    }

    /// Decal-space samples of each feature.
    fn features(&self) -> [Vec<FootprintSample>; 4] {
        let (camera, width, height) = self.camera();
        let ground = self.ground_y();
        let crate_top = self.piece.crate_center[1] + self.piece.crate_half[1];
        let on_crate = move |_: f32, _: f32| crate_top;
        let decals = &self.piece.decals;
        // Inner 60 %: the measurement stays clear of the feathered rim and of any
        // registration error between the flat decal box and the displaced ground.
        let scorch = footprint(&camera, width, height, &decals[0..1], 0.6, 0.01, &ground);
        let crate_scorch = footprint(&camera, width, height, &decals[1..2], 0.6, 0.01, &on_crate);
        let pad = footprint(&camera, width, height, &decals[2..3], 0.6, 0.01, &ground);
        let rut = footprint(
            &camera,
            width,
            height,
            &decals[self.piece.rut_first..],
            0.6,
            0.01,
            &ground,
        );
        [scorch, crate_scorch, rut, pad]
    }

    /// Decal contribution of `surfaces` on `ground` (renders with and without decals).
    /// Also returns the pair for the evidence and sanity checks.
    fn response(
        &mut self,
        ground: PieceGround,
        surfaces: &[DecalSurface],
    ) -> (SetPieceResponse, Vec<u8>, Vec<u8>) {
        let with = self.render(ground, Some(surfaces));
        let without = self.render(ground, None);
        let (_, width, _) = self.camera();
        let [scorch, crate_scorch, rut, pad] = self.features();
        let response = SetPieceResponse {
            scorch: feature_response(&with, &without, width, &scorch),
            crate_scorch: feature_response(&with, &without, width, &crate_scorch),
            rut: feature_response(&with, &without, width, &rut),
            pad: feature_response(&with, &without, width, &pad),
        };
        (response, with, without)
    }
}

fn near(got: f32, want: f32, abs: f32, rel: f32) -> bool {
    (got - want).abs() <= abs + rel * want.abs()
}

/// Compares measured decal responses with the blessed ones. Tolerances are loose enough
/// for terrain and lighting lanes to move the ground under the decals (the transfer is a
/// ratio to that ground) and tight enough that a missing channel fails.
fn check_response(name: &str, got: &SetPieceResponse, want: &SetPieceResponse) -> Result<(), Vec<String>> {
    let mut misses = Vec::new();
    let features = [
        ("scorch", got.scorch, want.scorch),
        ("crate scorch", got.crate_scorch, want.crate_scorch),
        ("rut", got.rut, want.rut),
        ("pad", got.pad, want.pad),
    ];
    for (feature, g, w) in features {
        for c in 0..3 {
            if !near(g.transfer[c], w.transfer[c], 0.03, 0.12) {
                misses.push(format!(
                    "{name} {feature} transfer[{c}] {} vs blessed {}",
                    g.transfer[c], w.transfer[c]
                ));
            }
        }
        if !near(g.relief, w.relief, 0.02, 0.3) {
            misses.push(format!(
                "{name} {feature} relief {} vs blessed {}",
                g.relief, w.relief
            ));
        }
    }
    if misses.is_empty() { Ok(()) } else { Err(misses) }
}

fn expectations_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/evidence/decals")
        .join(SETPIECE_EXPECTATIONS)
}

fn blessed_expectations() -> SetPieceExpectations {
    let path = expectations_path();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    ron::from_str(&text).unwrap_or_else(|err| panic!("parse {}: {err}", path.display()))
}

fn blessing() -> bool {
    regia_core::brand::env_var("REGIA_WRITE_DECAL_EVIDENCE")
        .ok()
        .as_deref()
        == Some("1")
}

/// The set piece through the default-look path (AgX, EV 14.5, planet sky): scorch on
/// photoreal snow or dust and on a rigid crate, a hard pad stripe, twin ruts. Each ground
/// renders with and without decals; the test locks what the decals add inside their
/// footprints (`setpiece_expectations.ron`), not the whole frame, so a terrain or
/// lighting change does not re-bless it. The PNGs are evidence only.
#[test]
fn default_look_setpiece_snow_and_mars() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rig = PieceRig::new();
    let surfaces = rig.piece.surfaces.clone();
    let (snow, snow_with, snow_without) = rig.response(PieceGround::Snow, &surfaces);
    let (mars, mars_with, mars_without) = rig.response(PieceGround::Mars, &surfaces);
    eprintln!("snow decal response: {snow:?}");
    eprintln!("mars decal response: {mars:?}");
    let (camera, width, height) = rig.camera();

    // Loose whole-frame sanity: the grounds are what the shot is about.
    let clear = must_project(&camera, width, height, rig.piece.clear);
    let snow_clear = window_rgb(&snow_with, width, height, clear.0, clear.1, 4);
    let mars_clear = window_rgb(&mars_with, width, height, clear.0, clear.1, 4);
    assert!(
        luma_rgb(snow_clear) > 0.3,
        "snow ground should be bright daylight, got {snow_clear:?} at {clear:?}"
    );
    assert!(
        mars_clear[0] > mars_clear[2] + 0.02 && luma_rgb(mars_clear) > 0.08 && luma_rgb(mars_clear) < 0.8,
        "mars ground should read as dust, got {mars_clear:?} at {clear:?}"
    );

    // Decals stay inside their boxes: away from every (generously grown) footprint the
    // with and without frames match.
    for (name, with, without) in [
        ("snow", &snow_with, &snow_without),
        ("mars", &mars_with, &mars_without),
    ] {
        let grown = {
            let ground = rig.ground_y();
            let mut grown = vec![false; (width * height) as usize];
            let crate_top = rig.piece.crate_center[1] + rig.piece.crate_half[1];
            let on_crate = move |_: f32, _: f32| crate_top;
            for (index, decal) in rig.piece.decals.iter().enumerate() {
                let surface: &dyn Fn(f32, f32) -> f32 = if index == 1 { &on_crate } else { &ground };
                for sample in footprint(
                    &camera,
                    width,
                    height,
                    std::slice::from_ref(decal),
                    1.6,
                    0.01,
                    surface,
                ) {
                    let (x, y) = sample.pixel;
                    for gy in y.saturating_sub(6)..=(y + 6).min(height - 1) {
                        for gx in x.saturating_sub(6)..=(x + 6).min(width - 1) {
                            grown[(gy * width + gx) as usize] = true;
                        }
                    }
                }
            }
            grown
        };
        let (mut outside, mut moved) = (0u32, 0u32);
        for y in 0..height {
            for x in 0..width {
                if grown[(y * width + x) as usize] {
                    continue;
                }
                outside += 1;
                let a = at(with, width, x, y);
                let b = at(without, width, x, y);
                if (0..3).any(|c| a[c].abs_diff(b[c]) > 2) {
                    moved += 1;
                }
            }
        }
        assert!(
            moved * 1000 <= outside,
            "{name}: decals changed {moved} of {outside} pixels outside their footprints"
        );
    }

    // What each decal is for, whatever the blessed numbers say.
    for (name, r) in [("snow", &snow), ("mars", &mars)] {
        assert!(
            luma_rgb(r.scorch.transfer) < 0.8 && luma_rgb(r.crate_scorch.transfer) < 0.8,
            "{name} scorch should darken its receivers: {r:?}"
        );
        assert!(
            luma_rgb(r.rut.transfer) < 0.92,
            "{name} ruts should darken the curve: {r:?}"
        );
        let pad = r.pad.transfer;
        assert!(
            pad[0] > pad[2] * 1.5 && pad[1] > pad[2] * 1.2,
            "{name} pad stripe should push the ground toward yellow: {pad:?}"
        );
    }

    if blessing() {
        let text = ron::ser::to_string_pretty(&SetPieceExpectations { snow, mars }, regia_core::ron_pretty())
            .expect("ron");
        std::fs::write(expectations_path(), text + "\n").expect("write expectations");
        write_evidence_png("snow_setpiece.png", &snow_with, width, height);
        write_evidence_png("mars_setpiece.png", &mars_with, width, height);
    }
    if regia_test_utils::golden::goldens_skipped() {
        // The farm gate's real-GPU pass (ADR-072): the numbers are blessed on lavapipe.
        eprintln!("REGIA_SKIP_GOLDENS: blessed decal responses not compared on this adapter");
        return;
    }
    let blessed = blessed_expectations();
    let mut misses = Vec::new();
    for (name, got, want) in [("snow", &snow, &blessed.snow), ("mars", &mars, &blessed.mars)] {
        if let Err(more) = check_response(name, got, want) {
            misses.extend(more);
        }
    }
    assert!(
        misses.is_empty(),
        "decal contribution moved (re-bless with REGIA_WRITE_DECAL_EVIDENCE=1 only if intended):\n{}",
        misses.join("\n")
    );
}

/// The set-piece check is only worth its name if the channels it locks are load-bearing:
/// strip one from every decal and the blessed check must fail on the metric it drives.
///
/// Roughness is not among them: the set piece's char (0.72) and ruts (1.0) are rough, and
/// the decal shader's roughness term is a glint toward the sun, which these shots (sun
/// behind the camera) never face; stripping it moves the footprint transfer by < 0.3 %.
/// `frame_hook_masks_characters_fades_grazing_and_roughness` locks that channel from the
/// mirror direction.
#[test]
fn setpiece_check_fails_without_each_decal_channel() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if regia_test_utils::golden::goldens_skipped() {
        eprintln!("REGIA_SKIP_GOLDENS: blessed decal responses not compared on this adapter");
        return;
    }
    let mut rig = PieceRig::new();
    let blessed = blessed_expectations();
    let full = rig.piece.surfaces.clone();
    let (snow, _, _) = rig.response(PieceGround::Snow, &full);
    assert!(
        check_response("snow", &snow, &blessed.snow).is_ok(),
        "the full set piece must pass before stripping channels: {snow:?}"
    );
    // The normal channel relights the whole footprint (the authored tilt) as well as
    // its bowl or groove, so either its transfer or its relief may be what moves.
    let normal_metrics: &[&str] = &["relief", "transfer"];
    for (channel, label, metrics) in [
        (CHANNEL_ALBEDO, "albedo", &["transfer"][..]),
        (CHANNEL_NORMAL, "normal", normal_metrics),
    ] {
        let stripped: Vec<DecalSurface> = full
            .iter()
            .map(|surface| {
                let mut surface = *surface;
                surface.channels &= !channel;
                surface
            })
            .collect();
        let (response, _, _) = rig.response(PieceGround::Snow, &stripped);
        eprintln!("without {label}: {response:?}");
        match check_response("snow", &response, &blessed.snow) {
            Ok(()) => panic!("stripping the {label} channel still passes the set-piece check: {response:?}"),
            Err(misses) => assert!(
                misses
                    .iter()
                    .any(|miss| metrics.iter().any(|metric| miss.contains(metric))),
                "stripping {label} should fail a {metrics:?} metric, failed: {misses:?}"
            ),
        }
    }
}

fn luma_rgb(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

fn ramp_map(rise: f32) -> TerrainMap {
    let n = 32u32;
    let world = 8.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for _row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
            heights.push(rise * x);
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 1.0, 0.0).expect("ramp")
}

fn ramp_layers() -> TerrainLayers {
    TerrainLayers {
        version: TERRAIN_LAYERS_FORMAT_VERSION,
        wetness_default: 0.0,
        layers: vec![LayerDesc {
            name: "ramp".into(),
            albedo: [0.72, 0.68, 0.48],
            slope_max: 1.55,
            ..LayerDesc::ground()
        }],
        puddles: true,
    }
}

fn attach_ramp(device: &RenderDevice, renderer: &mut FrameRenderer, rise: f32) {
    let draw = TerrainSharedDraw::new(device, ramp_map(rise), &ramp_layers(), CdlodConfig::default(), 3)
        .expect("ramp terrain");
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
}

/// A world-down decal on a heightfield. The steep ramp's normal is below the angle-fade
/// start, so the decal hides only when the receiver GBA is the heightfield normal. The
/// +Y clear would show it. The flat map shows it. Derivatives of a plane match that
/// normal, so this test does not replace the mismatched-vertex-normal case above.
#[test]
fn frame_hook_terrain_slope_writes_receiver_normal() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let gpu = Arc::new(Mutex::new(VfxGpu::new(&device, profile()).expect("vfx gpu")));
    let mut renderer = FrameRenderer::new();
    renderer.set_translucent_layers(vec![translucent_layer(Arc::clone(&gpu))]);
    let eye = [0.0, 3.2, 5.5];
    let look = [0.0, 0.0, 0.0];
    let fov = 0.9f32;
    let width = 160u32;
    let height = 120u32;
    let camera = VfxCamera {
        eye: Vec3::from_array(eye),
        target: Vec3::from_array(look),
        fov_y: fov,
        near: 0.1,
        far: 80.0,
    };
    let (decal, mut surface) = solid(
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(2.0, 2.0, 3.0),
        [0.02, 0.01, 0.0],
        LAYER_TERRAIN,
    );
    surface.angle_fade = [0.3, 0.6];
    attach_ramp(&device, &mut renderer, 5.0);
    let bare_steep = hook_render(&device, &mut renderer, &gpu, eye, look, fov, vec![], &[], &[]);
    let steep = hook_render(
        &device,
        &mut renderer,
        &gpu,
        eye,
        look,
        fov,
        vec![],
        &[decal],
        &[surface],
    );
    attach_ramp(&device, &mut renderer, 0.0);
    let bare_flat = hook_render(&device, &mut renderer, &gpu, eye, look, fov, vec![], &[], &[]);
    let flat = hook_render(
        &device,
        &mut renderer,
        &gpu,
        eye,
        look,
        fov,
        vec![],
        &[decal],
        &[surface],
    );
    let center = must_project(&camera, width, height, Vec3::ZERO);
    let steep_delta = (window_mean(&steep, width, height, center.0, center.1, 2)
        - window_mean(&bare_steep, width, height, center.0, center.1, 2))
    .abs();
    let flat_delta = (window_mean(&flat, width, height, center.0, center.1, 2)
        - window_mean(&bare_flat, width, height, center.0, center.1, 2))
    .abs();
    assert!(
        steep_delta < 0.02,
        "a heightfield slope must fade a world-down decal, delta {steep_delta} (the +Y clear would show it)"
    );
    assert!(
        flat_delta > 0.05,
        "the same decal must show on a flat heightfield, delta {flat_delta}"
    );
}

/// Decal relight and glint use the frame's sun: rotating it moves a smooth decal's glint
/// across a flat receiver to the predicted mirror point, and the glint takes its colour.
#[test]
fn decal_glint_follows_the_frame_sun() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let gpu = Arc::new(Mutex::new(VfxGpu::new(&device, profile()).expect("vfx gpu")));
    let mut renderer = FrameRenderer::new();
    renderer.set_translucent_layers(vec![translucent_layer(Arc::clone(&gpu))]);
    let material = LabMaterial {
        base_color: [0.3, 0.3, 0.3, 1.0],
        unlit: true,
        ..LabMaterial::EMPTY
    };
    let flat_v = hook_quad_sized([0.0, 0.0, 0.0], [0.0, 1.0, 0.0], 1.8, false);
    let (decal, mut surface) = solid(
        Vec3::new(0.0, 0.02, 0.0),
        Vec3::new(1.7, 1.7, 0.35),
        [1.0, 1.0, 1.0],
        LAYER_WORLD,
    );
    surface.channels = CHANNEL_ROUGHNESS;
    surface.roughness = 0.0;
    surface.angle_fade = [0.0, 0.0];
    let eye = [0.0f32, 4.0, 0.01];
    let fov = 0.9f32;
    let (width, height) = (160u32, 120u32);
    let camera = VfxCamera {
        eye: Vec3::from_array(eye),
        target: Vec3::ZERO,
        fov_y: fov,
        near: 0.1,
        far: 40.0,
    };
    // Toward-sun (±0.25, 1, 0): the mirror point on the plane is x = 4 · sx / sy = ±1 m.
    let render = |renderer: &mut FrameRenderer, toward: Vec3, color: [f32; 3]| {
        let travel = (-toward).normalize().to_array();
        hook_render_lit(
            &device,
            renderer,
            &gpu,
            eye,
            [0.0, 0.0, 0.0],
            fov,
            vec![mesh(31, &flat_v, &[], &material)],
            &[decal],
            &[surface],
            // 100 klx: the glint's colour scale is 1 (illuminance / 100 klx, clamped).
            DirectionalLightUniform::new(travel, color, 100_000.0),
        )
    };
    let west = render(&mut renderer, Vec3::new(-0.25, 1.0, 0.0), [1.0, 1.0, 1.0]);
    let east = render(&mut renderer, Vec3::new(0.25, 1.0, 0.0), [1.0, 0.25, 0.25]);
    let west_px = must_project(&camera, width, height, Vec3::new(-1.0, 0.0, 0.0));
    let east_px = must_project(&camera, width, height, Vec3::new(1.0, 0.0, 0.0));
    let at_west = |img: &[u8]| window_mean(img, width, height, west_px.0, west_px.1, 2);
    let at_east = |img: &[u8]| window_mean(img, width, height, east_px.0, east_px.1, 2);
    eprintln!(
        "west sun: {:.3} at west, {:.3} at east; east sun: {:.3} at west, {:.3} at east",
        at_west(&west),
        at_east(&west),
        at_west(&east),
        at_east(&east)
    );
    assert!(
        at_west(&west) > at_east(&west) + 0.08,
        "a sun in the west must put the glint at the west mirror point"
    );
    assert!(
        at_east(&east) > at_west(&east) + 0.04,
        "rotating the sun east must move the glint east"
    );
    // The brightest decal column follows the sun across the receiver.
    let brightest_x = |img: &[u8]| {
        let mut best = (0.0f32, 0u32);
        for x in 10..width - 10 {
            let l = window_mean(img, width, height, x, height / 2, 1);
            if l > best.0 {
                best = (l, x);
            }
        }
        best.1
    };
    let (bw, be) = (brightest_x(&west), brightest_x(&east));
    assert!(
        (bw as i32 - west_px.0 as i32).abs() <= 6 && (be as i32 - east_px.0 as i32).abs() <= 6,
        "glint columns {bw} / {be} vs mirror points {} / {}",
        west_px.0,
        east_px.0
    );
    // A red sun glints red.
    let rgb = window_rgb(&east, width, height, east_px.0, east_px.1, 1);
    assert!(rgb[0] > rgb[1] + 0.1, "the glint takes the sun's color: {rgb:?}");
}
