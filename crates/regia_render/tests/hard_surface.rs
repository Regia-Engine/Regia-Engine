// SPDX-License-Identifier: MPL-2.0
//! Lavapipe goldens for the hard-surface material response.
//!
//! Each feature is rendered on and off at 960×540, EV 14.5, with a sphere row in
//! front of a panelled hull. A black or crushed frame fails: mean luma must sit in
//! 60..180, and the feature's own pixels must move (a coat glint present only when
//! the lobe is on, a cavity darker when specular occlusion is on, a hull close-up
//! that shows those glints). Bless with `REGIA_BLESS=1` on lavapipe only.
//! `REGIA_PREVIEW=1` writes `<temp>/hard-surface-preview` and skips the golden compare.
//!
//! SSIM is the global luma form (Wang, Bovik, Sheikh, Simoncelli, IEEE TIP 2004).

use std::path::{Path, PathBuf};

use regia_asset::{DetailMapParams, EdgeWearParams, HardSurfaceParams, MaterialAssetRon, ParallaxParams};
use regia_render::hard_surface::{
    AuthoredSurfaceMaps, DEMO_EV, DEMO_HEIGHT, DEMO_WIDTH, HardSurfaceFeature, feature_demo_quality,
    feature_demo_scene, hull_closeup_quality, hull_closeup_scene, hull_closeup_uncoated,
};
use regia_render::{FrameQuality, FrameRenderer, LabMaterial, LabScene, LabTexture, RenderDevice};

const LUMA_LO: f64 = 60.0;
const LUMA_HI: f64 = 180.0;
const SSIM_MIN: f64 = 0.98;

fn evidence_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/evidence/hard-surface")
}

fn render(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    scene: &LabScene,
    quality: FrameQuality,
) -> Vec<u8> {
    let frame = scene.as_frame_scene_with_quality(quality);
    renderer
        .render_offscreen(device, &frame)
        .expect("offscreen frame")
        .pixels
}

fn luma_at(pixels: &[u8], index: usize) -> f64 {
    let px = index * 4;
    0.2126 * f64::from(pixels[px]) + 0.7152 * f64::from(pixels[px + 1]) + 0.0722 * f64::from(pixels[px + 2])
}

fn mean_luma(pixels: &[u8]) -> f64 {
    let n = pixels.len() / 4;
    let mut sum = 0.0;
    for i in 0..n {
        sum += luma_at(pixels, i);
    }
    sum / n as f64
}

fn mean_abs(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mut sum = 0.0;
    for (left, right) in a.iter().zip(b) {
        sum += f64::from(left.abs_diff(*right));
    }
    sum / a.len() as f64
}

/// Global luma SSIM (one window the size of the frame).
fn luma_ssim(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let n = (a.len() / 4) as f64;
    let mut sum_a = 0.0;
    let mut sum_b = 0.0;
    let mut sum_a2 = 0.0;
    let mut sum_b2 = 0.0;
    let mut sum_ab = 0.0;
    for i in 0..a.len() / 4 {
        let la = luma_at(a, i);
        let lb = luma_at(b, i);
        sum_a += la;
        sum_b += lb;
        sum_a2 += la * la;
        sum_b2 += lb * lb;
        sum_ab += la * lb;
    }
    let mean_a = sum_a / n;
    let mean_b = sum_b / n;
    let var_a = sum_a2 / n - mean_a * mean_a;
    let var_b = sum_b2 / n - mean_b * mean_b;
    let cov = sum_ab / n - mean_a * mean_b;
    let c1 = (0.01 * 255.0_f64).powi(2);
    let c2 = (0.03 * 255.0_f64).powi(2);
    ((2.0 * mean_a * mean_b + c1) * (2.0 * cov + c2))
        / ((mean_a * mean_a + mean_b * mean_b + c1) * (var_a + var_b + c2))
}

fn write_png(path: &Path, width: u32, height: u32, pixels: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("evidence dir");
    }
    let file = std::fs::File::create(path).expect("create golden");
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(pixels).expect("png pixels");
    writer.finish().expect("png finish");
}

fn read_png(path: &Path, width: u32, height: u32) -> Vec<u8> {
    let file = std::fs::File::open(path).unwrap_or_else(|err| panic!("open {}: {err}", path.display()));
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().expect("png info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    buf.truncate(info.buffer_size());
    assert_eq!(info.width, width, "{}", path.display());
    assert_eq!(info.height, height, "{}", path.display());
    assert_eq!(buf.len(), (width as usize) * (height as usize) * 4);
    buf
}

fn assert_exposure(name: &str, pixels: &[u8]) {
    let luma = mean_luma(pixels);
    eprintln!("hard-surface {name}: mean luma {luma:.1}");
    assert!(
        (LUMA_LO..=LUMA_HI).contains(&luma),
        "{name} mean luma {luma:.1} is outside {LUMA_LO}..={LUMA_HI} at EV {DEMO_EV}"
    );
}

/// Pixels where the on frame is a bright glint the off frame does not have.
fn glint_count(on: &[u8], off: &[u8]) -> usize {
    let mut count = 0usize;
    for i in 0..on.len() / 4 {
        let a = luma_at(on, i);
        let b = luma_at(off, i);
        if a >= 185.0 && a > b + 30.0 {
            count += 1;
        }
    }
    count
}

/// Pixels the on frame darkens, and their mean luma on each side.
fn darkened(on: &[u8], off: &[u8]) -> (usize, f64, f64) {
    let mut count = 0usize;
    let mut sum_on = 0.0;
    let mut sum_off = 0.0;
    for i in 0..on.len() / 4 {
        let a = luma_at(on, i);
        let b = luma_at(off, i);
        if b > a + 8.0 {
            count += 1;
            sum_on += a;
            sum_off += b;
        }
    }
    if count == 0 {
        return (0, 0.0, 0.0);
    }
    (count, sum_on / count as f64, sum_off / count as f64)
}

fn changed_count(on: &[u8], off: &[u8], margin: f64) -> (usize, usize) {
    let mut up = 0usize;
    let mut down = 0usize;
    for i in 0..on.len() / 4 {
        let a = luma_at(on, i);
        let b = luma_at(off, i);
        if a > b + margin {
            up += 1;
        } else if b > a + margin {
            down += 1;
        }
    }
    (up, down)
}

/// Strong horizontal luma jumps on the row that has the most of them.
fn strongest_row_edges(pixels: &[u8], width: u32, height: u32) -> usize {
    let y0 = height * 30 / 100;
    let y1 = height * 78 / 100;
    let mut best = 0usize;
    for y in y0..y1 {
        let mut jumps = 0usize;
        let mut prev = luma_at(pixels, (y * width) as usize);
        for x in 1..width {
            let sample = luma_at(pixels, (y * width + x) as usize);
            if (sample - prev).abs() > 20.0 {
                jumps += 1;
                prev = sample;
            }
        }
        best = best.max(jumps);
    }
    best
}

fn assert_sphere_row(name: &str, pixels: &[u8]) {
    let edges = strongest_row_edges(pixels, DEMO_WIDTH, DEMO_HEIGHT);
    eprintln!("{name} strongest row edges {edges}");
    assert!(
        edges >= 4,
        "{name} does not frame a sphere row (strongest row has {edges} edges)"
    );
}

fn assert_glints(name: &str, on: &[u8], off: &[u8]) {
    let glints = glint_count(on, off);
    eprintln!("{name} coat glint pixels {glints}");
    // A 0.04-rough coat is a tight sun highlight (~30 pixels at α 0.0016 → the 0.002025 floor);
    // the old floor (α ≥ 0.045, perceptual 0.21) spread it over ~40+. Without the coat lobe the
    // count is 0.
    assert!(
        glints >= 20,
        "{name} coat highlight missing (pixels with on luma ≥ 185 and 30 above off: {glints})"
    );
}

/// Preview pair (best-effort, see `write_preview`).
fn save_pair(dir: &Path, name: &str, width: u32, height: u32, on: &[u8], off: &[u8]) {
    write_preview(&dir.join(format!("{name}.png")), width, height, on);
    write_preview(&dir.join(format!("{name}_off.png")), width, height, off);
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn hard_surface_features_match_lavapipe_goldens() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless device");
    let mut renderer = FrameRenderer::new();
    let bless = regia_core::brand::env_var("REGIA_BLESS").ok().as_deref() == Some("1");
    let preview = regia_core::brand::env_var("REGIA_PREVIEW").ok().as_deref() == Some("1");
    let dir = evidence_dir();
    let preview_dir = std::env::temp_dir().join("hard-surface-preview");
    let mut parallax_on = Vec::new();

    for feature in HardSurfaceFeature::ALL {
        let quality = feature_demo_quality(feature);
        let on_scene = feature_demo_scene(feature, true);
        assert_eq!(on_scene.width, DEMO_WIDTH);
        assert_eq!(on_scene.height, DEMO_HEIGHT);
        assert!((on_scene.exposure_ev - DEMO_EV).abs() < 1.0e-4);
        assert!(on_scene.meshes.len() >= 4, "{}", feature.name());
        let on = render(&mut renderer, &device, &on_scene, quality);
        let off = render(
            &mut renderer,
            &device,
            &feature_demo_scene(feature, false),
            quality,
        );
        let (up, down) = changed_count(&on, &off, 6.0);
        eprintln!("{} brighter {up} darker {down}", feature.name());
        save_pair(&preview_dir, feature.name(), DEMO_WIDTH, DEMO_HEIGHT, &on, &off);
        assert_exposure(feature.name(), &on);
        assert_sphere_row(feature.name(), &on);
        match feature {
            HardSurfaceFeature::Clearcoat => assert_glints(feature.name(), &on, &off),
            HardSurfaceFeature::SpecularOcclusion => {
                let (count, mean_on, mean_off) = darkened(&on, &off);
                eprintln!("specular occlusion cavity pixels {count} on {mean_on:.1} off {mean_off:.1}");
                // The authored form replaces the always-on one (applied once, see
                // `authored_specular_occlusion_is_applied_once` for the exact ratio), so it
                // deepens the cavity by the difference of the two forms: ~8 luma over the
                // cavity's core. (The 80-pixel / 10-luma bar held only while both multiplied.)
                let frame = (DEMO_WIDTH * DEMO_HEIGHT) as usize;
                assert!(
                    (30..frame / 5).contains(&count),
                    "cavity should darken a small region, got {count} pixels"
                );
                assert!(
                    mean_on + 6.0 < mean_off,
                    "cavity on {mean_on:.1} is not darker than off {mean_off:.1}"
                );
            }
            HardSurfaceFeature::MultiScatter => {
                assert!(
                    up > 400,
                    "multi-scatter did not brighten the rough metal ({up} pixels)"
                );
            }
            HardSurfaceFeature::Anisotropy => {
                assert!(
                    up > 200 && down > 200,
                    "anisotropy streak missing (up {up} down {down})"
                );
            }
            HardSurfaceFeature::Detail | HardSurfaceFeature::Parallax | HardSurfaceFeature::EdgeWear => {
                assert!(
                    up + down > 1500,
                    "{} barely changed the frame (up {up} down {down})",
                    feature.name()
                );
            }
        }
        let path = dir.join(format!("{}.png", feature.name()));
        if feature == HardSurfaceFeature::Parallax {
            parallax_on = on.clone();
        }
        if bless {
            write_png(&path, DEMO_WIDTH, DEMO_HEIGHT, &on);
            continue;
        }
        if preview {
            continue;
        }
        let golden = read_png(&path, DEMO_WIDTH, DEMO_HEIGHT);
        let score = luma_ssim(&on, &golden);
        eprintln!("{} SSIM {score:.4}", feature.name());
        assert!(
            score >= SSIM_MIN,
            "{} SSIM {score:.4} is below {SSIM_MIN}",
            feature.name()
        );
        let (g_up, g_down) = changed_count(&golden, &off, 6.0);
        assert!(
            g_up + g_down > 400,
            "{} golden matches the off frame ({g_up} brighter, {g_down} darker)",
            feature.name()
        );
    }

    let parallax = HardSurfaceFeature::Parallax;
    let mut gated_quality = feature_demo_quality(parallax);
    gated_quality.parallax_occlusion = false;
    let gated = render(
        &mut renderer,
        &device,
        &feature_demo_scene(parallax, true),
        gated_quality,
    );
    let off = render(
        &mut renderer,
        &device,
        &feature_demo_scene(parallax, false),
        feature_demo_quality(parallax),
    );
    let gate_off = mean_abs(&gated, &off);
    let (gate_up, gate_down) = changed_count(&gated, &parallax_on, 6.0);
    eprintln!(
        "parallax gate-off MAD {gate_off:.3}, vs on changed {}",
        gate_up + gate_down
    );
    assert!(
        gate_off < 0.5,
        "parallax with the profile gate off drifted from the feature-off frame (MAD {gate_off:.3})"
    );
    assert!(
        gate_up + gate_down > 1500,
        "parallax profile gate did not change the frame ({} pixels)",
        gate_up + gate_down
    );

    let bytes = include_bytes!("../../../docs/formats/material.example.ron");
    let asset = MaterialAssetRon::from_ron(bytes).expect("v1 example migrates to v2");
    let mut painted_scene = feature_demo_scene(HardSurfaceFeature::Clearcoat, true);
    painted_scene.materials[0] = LabMaterial::from_authored(&asset);
    let quality = feature_demo_quality(HardSurfaceFeature::Clearcoat);
    let painted = render(&mut renderer, &device, &painted_scene, quality);
    let mut flat_scene = painted_scene.clone();
    flat_scene.materials[0].hard_surface.clearcoat = 0.0;
    let flat = render(&mut renderer, &device, &flat_scene, quality);
    let (coat_up, _) = changed_count(&painted, &flat, 6.0);
    eprintln!("authored clearcoat 0.35 brighter pixels {coat_up}");
    assert!(
        coat_up > 80,
        "migrated v1 clearcoat 0.35 did not light a highlight ({coat_up} pixels)"
    );
    assert_exposure("authored clearcoat", &painted);

    let hull = hull_closeup_scene();
    assert_eq!(hull.width, DEMO_WIDTH);
    assert_eq!(hull.height, DEMO_HEIGHT);
    assert!(hull.meshes.len() >= 4);
    let hull_px = render(&mut renderer, &device, &hull, hull_closeup_quality());
    let bare = render(
        &mut renderer,
        &device,
        &hull_closeup_uncoated(),
        hull_closeup_quality(),
    );
    save_pair(
        &preview_dir,
        "hull_closeup",
        hull.width,
        hull.height,
        &hull_px,
        &bare,
    );
    assert_exposure("hull_closeup", &hull_px);
    assert_sphere_row("hull_closeup", &hull_px);
    assert_glints("hull_closeup", &hull_px, &bare);
    let hull_path = dir.join("hull_closeup.png");
    if bless {
        write_png(&hull_path, hull.width, hull.height, &hull_px);
    } else if !preview {
        let golden = read_png(&hull_path, hull.width, hull.height);
        let score = luma_ssim(&hull_px, &golden);
        eprintln!("hull_closeup SSIM {score:.4}");
        assert!(
            score >= SSIM_MIN,
            "hull close-up SSIM {score:.4} is below {SSIM_MIN}"
        );
    }
}

#[test]
fn mesh_bind_uploads_detail_height_and_curvature() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut asset = MaterialAssetRon::default_standard();
    asset.base.base_color = [0.55, 0.58, 0.62, 1.0];
    asset.base.roughness = 0.45;
    asset.base.metallic = 0.0;
    asset.hard_surface = Some(HardSurfaceParams {
        detail: Some(DetailMapParams {
            albedo_strength: 1.0,
            uv_scale: [5.0, 5.0],
            ..DetailMapParams::default()
        }),
        parallax: Some(ParallaxParams {
            scale: 0.18,
            min_steps: 4,
            max_steps: 16,
            ..ParallaxParams::default()
        }),
        edge_wear: Some(EdgeWearParams {
            intensity: 1.0,
            metallic_bias: 1.0,
            roughness_bias: -0.3,
            tint: [0.85, 0.86, 0.88],
            contrast: 6.0,
            ..EdgeWearParams::default()
        }),
        ..HardSurfaceParams::default()
    });
    let checker = checker_tex(32);
    let height = groove_height(32);
    let curve = edge_curve(32);
    let mapped = LabMaterial::from_authored_with_maps(
        &asset,
        AuthoredSurfaceMaps {
            detail_albedo: Some(checker),
            height_map: Some(height),
            curvature_map: Some(curve),
            ..AuthoredSurfaceMaps::default()
        },
    );
    let scalars = LabMaterial::from_authored(&asset);
    assert!(scalars.detail_albedo.is_none());
    assert!(scalars.height_map.is_none());
    assert!(scalars.curvature_map.is_none());

    let scene_of = |material: LabMaterial| {
        let mut scene = feature_demo_scene(HardSurfaceFeature::Detail, false);
        scene.materials = vec![material];
        scene
    };
    let device = RenderDevice::headless().expect("headless device");
    let mut renderer = FrameRenderer::new();
    let quality = hull_closeup_quality();
    let with_maps = render(&mut renderer, &device, &scene_of(mapped), quality);
    let without = render(&mut renderer, &device, &scene_of(scalars), quality);
    let (up, down) = changed_count(&with_maps, &without, 6.0);
    eprintln!("mesh bind maps vs scalars brighter {up} darker {down}");
    assert!(
        up + down > 2000,
        "binding detail, height, and curvature did not change the mesh (up {up} down {down})"
    );
}

fn solid(size: u32, pixel: impl Fn(f32, f32) -> [u8; 4]) -> LabTexture {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let u = (x as f32 + 0.5) / size as f32;
            let v = (y as f32 + 0.5) / size as f32;
            rgba.extend_from_slice(&pixel(u, v));
        }
    }
    LabTexture {
        width: size,
        height: size,
        rgba,
    }
}

fn checker_tex(size: u32) -> LabTexture {
    solid(size, |u, v| {
        let on = ((u * 8.0) as u32 + (v * 8.0) as u32).is_multiple_of(2);
        let g = if on { 220 } else { 40 };
        [g, g, g, 255]
    })
}

fn groove_height(size: u32) -> LabTexture {
    solid(size, |u, v| {
        let groove = |t: f32| {
            let f = (t * 3.0).fract();
            f < 0.14 || f > 0.86
        };
        let h = if groove(u) || groove(v) { 30 } else { 255 };
        [h, h, h, 255]
    })
}

fn edge_curve(size: u32) -> LabTexture {
    solid(size, |u, v| {
        let edge = u.min(1.0 - u).min(v.min(1.0 - v));
        let c = if edge < 0.22 { 240 } else { 60 };
        [c, c, c, 255]
    })
}

/// Authored specular occlusion is applied once, in place of the always-on term (it squared the
/// occlusion when it multiplied on top of it). A facing rough-0.3 metal plane under the IBL with
/// an occlusion texture of 0.5: authored on / off must be the ratio of the two forms, not the
/// authored form alone.
#[test]
fn authored_specular_occlusion_is_applied_once() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use regia_core::glam::{Mat4, Vec3};
    use regia_render::hard_surface::indirect_specular_occlusion;
    use regia_render::{DirectionalLightUniform, HardSurfaceFactors, LabMesh, RenderSettings, box_mesh};

    let device = RenderDevice::headless().expect("headless device");
    let settings = RenderSettings::default();
    let mut post = settings.resolve_post();
    post.taa = false;
    post.auto_exposure = false;
    post.exposure_ev = 12.0;
    let quality = settings.quality();
    let orm = solid(8, |_, _| [128, 255, 255, 255]);
    let ao = 128.0 / 255.0;
    let scene_of = |authored: bool| {
        let material = LabMaterial {
            base_color: [0.9, 0.9, 0.9, 1.0],
            metallic: 1.0,
            roughness: 0.3,
            occlusion_strength: 1.0,
            occlusion_map: Some(orm.clone()),
            orm_packed: true,
            hard_surface: HardSurfaceFactors {
                specular_occlusion: if authored { 1.0 } else { 0.0 },
                ..HardSurfaceFactors::OFF
            },
            ..LabMaterial::EMPTY
        };
        let (vertices, indices) = box_mesh([4.0, 0.1, 4.0]);
        LabScene {
            width: 64,
            height: 64,
            eye: [0.0, 3.0, 0.01],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.3,
            near: 0.05,
            far: 50.0,
            depth_prepass: settings.profile.depth_prepass,
            directional_shadows: false,
            ibl: true,
            exposure_ev: post.exposure_ev,
            ambient_lux: 0.0,
            double_sided: true,
            post: Some(post.clone()),
            directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
            local_lights: Vec::new(),
            materials: vec![material],
            meshes: vec![LabMesh {
                vertices,
                indices,
                material: 0,
                model: Mat4::from_translation(Vec3::new(0.0, -0.05, 0.0)).to_cols_array_2d(),
                joints: Vec::new(),
            }],
        }
    };
    let centre = |authored: bool| {
        let mut renderer = FrameRenderer::new();
        let scene = scene_of(authored);
        let frame = scene.as_frame_scene_with_quality(quality);
        for i in 0..3 {
            if i == 2 {
                renderer.request_hdr_capture();
            }
            renderer
                .render_offscreen(&device, &frame)
                .expect("offscreen frame");
        }
        let hdr = renderer
            .take_hdr_capture(&device)
            .expect("readback")
            .expect("hdr capture");
        let c = hdr.rgb[(hdr.height / 2 * hdr.width + hdr.width / 2) as usize];
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    };
    let (on, off) = (centre(true), centre(false));
    assert!(off > 0.0, "the plane reflects nothing");
    let expected =
        indirect_specular_occlusion(1.0, ao, 0.3, true) / indirect_specular_occlusion(1.0, ao, 0.3, false);
    let squared = indirect_specular_occlusion(1.0, ao, 0.3, true);
    eprintln!(
        "authored on {on:.3} off {off:.3} ratio {:.3} (once {expected:.3}, squared {squared:.3})",
        on / off
    );
    assert!(
        (on / off - expected).abs() < 0.06,
        "authored/default {:.3}, expected {expected:.3} (one application)",
        on / off
    );
}

/// A preview image for a person to look at: written best-effort under the OS temp directory. A
/// preview is not asserted on, so a folder another user owns (a shared `/tmp`) or a read-only temp
/// volume prints a note instead of failing a correct render (audit-v035-flaky M6).
fn write_preview(path: &Path, width: u32, height: u32, pixels: &[u8]) {
    let written = (|| -> Result<(), Box<dyn std::error::Error>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::File::create(path)?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(pixels)?;
        writer.finish()?;
        Ok(())
    })();
    if let Err(err) = written {
        eprintln!("preview {} not written: {err}", path.display());
    }
}
