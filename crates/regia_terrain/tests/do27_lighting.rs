// SPDX-License-Identifier: MPL-2.0
//! DO27 lighting gaps on the terrain path: distance fog on a heightfield (a uniform
//! tint fails), one sun cascade received by flat ground under a scatter caster, and
//! the noon+fog evidence frame as a lavapipe golden.

use std::path::PathBuf;

use regia_asset::Heightfield;
use regia_render::{FogSettings, RenderDevice};
use regia_terrain::gpu::{TerrainCamera, TerrainFrame, TerrainGpu, TimeOfDay, render_offscreen};
use regia_terrain::{
    CdlodConfig, GapShot, GapSun, LayerDesc, MinMaxPyramid, ScatterInstance, TerrainLayers, TerrainMap,
    render_gap_shot, select_nodes,
};
use regia_test_utils::golden::{
    GoldenIcd, HARDWARE_SANITY_SSIM, PixelTolerance, check_pixel_golden, golden_icd, goldens_skipped,
    ssim_rgba8,
};

fn device() -> RenderDevice {
    RenderDevice::headless().expect("lavapipe headless device")
}

fn grass_layers() -> TerrainLayers {
    TerrainLayers {
        layers: vec![LayerDesc {
            name: "grass".into(),
            albedo: [0.28, 0.46, 0.22],
            roughness: 0.9,
            tiling: 4.0,
            slope_min: 0.0,
            slope_max: 2.0,
            altitude_min: -50.0,
            altitude_max: 80.0,
            blend_sharpness: 0.2,
            triplanar: false,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    }
}

fn map_from_heights(n: u32, world: f32, heights_m: &[f32]) -> TerrainMap {
    assert_eq!(heights_m.len(), (n * n) as usize);
    let scale = 40.0;
    let offset = -8.0;
    let samples = heights_m
        .iter()
        .map(|&h| ((h - offset) / scale * 65535.0).round().clamp(0.0, 65535.0) as u16)
        .collect();
    TerrainMap::new(
        &Heightfield {
            width: n,
            height: n,
            samples,
        },
        world,
        scale,
        offset,
    )
    .expect("map")
}

fn project(camera: &TerrainCamera, world: [f32; 3], width: u32, height: u32) -> Option<(u32, u32)> {
    let m = camera.view_projection();
    let v = [world[0], world[1], world[2], 1.0];
    let mut clip = [0.0f32; 4];
    for col in 0..4 {
        for row in 0..4 {
            clip[row] += m[col][row] * v[col];
        }
    }
    if clip[3].abs() < 1.0e-5 {
        return None;
    }
    let ndc_x = clip[0] / clip[3];
    let ndc_y = clip[1] / clip[3];
    if !(-1.0..=1.0).contains(&ndc_x) || !(-1.0..=1.0).contains(&ndc_y) {
        return None;
    }
    let x = (ndc_x * 0.5 + 0.5) * width as f32;
    let y = (0.5 - ndc_y * 0.5) * height as f32;
    Some((x as u32, y as u32))
}

fn luma_at(pixels: &[u8], width: u32, x: u32, y: u32) -> f64 {
    let i = ((y * width + x) * 4) as usize;
    let p = &pixels[i..i + 3];
    0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64
}

fn patch_luma(pixels: &[u8], width: u32, x: u32, y: u32) -> f64 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for dy in 0..3 {
        for dx in 0..3 {
            sum += luma_at(pixels, width, x + dx, y + dy);
            n += 1.0;
        }
    }
    sum / n
}

fn rgb_at(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * width + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

struct Shot<'a> {
    device: &'a RenderDevice,
    map: &'a TerrainMap,
    camera: &'a TerrainCamera,
    tod: &'a TimeOfDay,
    fog: FogSettings,
    shadow_focus: [f32; 3],
    shadow_radius: f32,
    scatter: &'a [ScatterInstance],
    width: u32,
    height: u32,
}

fn render_map(shot: &Shot<'_>) -> Vec<u8> {
    let layers = grass_layers();
    let config = CdlodConfig::with_grid_size(8);
    let root_level = 3;
    let mut gpu =
        TerrainGpu::new(shot.device, shot.map, &layers, &config, root_level, None, None).expect("gpu");
    if !shot.scatter.is_empty() {
        gpu.set_scatter(shot.scatter, 500.0, 800.0, 0.0);
    }
    let mut frame = TerrainFrame::from_camera(shot.camera, shot.tod, 0.0, 0.0);
    frame.fog = shot.fog;
    frame.shadow_focus = shot.shadow_focus;
    frame.shadow_radius = shot.shadow_radius;
    let pyramid = MinMaxPyramid::build(shot.map, root_level).expect("pyramid");
    let selection = select_nodes(
        shot.map,
        &pyramid,
        &config,
        root_level,
        [shot.camera.position[0], shot.camera.position[2]],
        None,
    );
    render_offscreen(
        &mut gpu,
        shot.device,
        &frame,
        &selection,
        shot.width,
        shot.height,
        [0.02, 0.02, 0.05, 1.0],
    )
    .expect("offscreen")
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn heightfield_fog_is_stronger_in_the_distance() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let n = 33u32;
    let world = 120.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for _col in 0..n {
            let z = row as f32 / (n - 1) as f32 * world - world * 0.5;
            // Rising away from the camera (camera sits at +Z): a heightfield, not a quad.
            heights.push((world * 0.5 - z) / world * 10.0);
        }
    }
    let map = map_from_heights(n, world, &heights);
    let width = 160u32;
    let height = 90u32;
    let camera = TerrainCamera {
        position: [0.0, 22.0, 52.0],
        target: [0.0, 4.0, -8.0],
        fov_y_radians: 0.9,
        aspect: width as f32 / height as f32,
        near: 0.3,
        far: 400.0,
    };
    let tod = TimeOfDay {
        azimuth_radians: 0.4,
        elevation_radians: 0.8,
        illuminance_lux: 80_000.0,
        ambient: [0.15, 0.17, 0.2],
        ..TimeOfDay::default()
    };
    let mut shot = Shot {
        device: &device,
        map: &map,
        camera: &camera,
        tod: &tod,
        fog: FogSettings::default(),
        shadow_focus: camera.target,
        shadow_radius: 80.0,
        scatter: &[],
        width,
        height,
    };
    let clear = render_map(&shot);
    shot.fog = FogSettings::outdoor_haze();
    let fogged = render_map(&shot);
    assert_ne!(clear, fogged, "fog must change the heightfield");

    // Near ground is close to the camera (+Z); far ground is down the ramp (−Z).
    let near = project(&camera, [0.0, 2.0, 28.0], width, height).expect("near on screen");
    let far = project(&camera, [0.0, 8.0, -30.0], width, height).expect("far on screen");
    let near_shift = patch_delta(&clear, &fogged, width, near);
    let far_shift = patch_delta(&clear, &fogged, width, far);
    let near_blue = patch_blue(&clear, &fogged, width, near);
    let far_blue = patch_blue(&clear, &fogged, width, far);
    assert!(
        far_shift > near_shift * 1.35,
        "far heightfield must move more than near (far {far_shift:.2} near {near_shift:.2}); a uniform tint fails"
    );
    assert!(
        far_blue > near_blue + 1.0,
        "far texels must shift toward the blue haze more (far {far_blue:.2} near {near_blue:.2})"
    );
    let mean = mean_luma(&fogged);
    assert!(
        (8.0..220.0).contains(&mean),
        "fogged frame luma {mean:.1} is black or blown out"
    );
}

#[test]
fn scatter_casts_a_sun_shadow_on_flat_ground() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let n = 33u32;
    let world = 80.0;
    let heights = vec![0.0f32; (n * n) as usize];
    let map = map_from_heights(n, world, &heights);
    let width = 160u32;
    let height = 90u32;
    // Sun sits toward +Z (azimuth 0) at a low elevation, so the shadow falls toward −Z.
    let tod = TimeOfDay {
        azimuth_radians: 0.0,
        elevation_radians: 0.38,
        sun_color: [1.1, 1.02, 0.92],
        illuminance_lux: 110_000.0,
        ambient: [0.05, 0.055, 0.06],
    };
    let camera = TerrainCamera {
        position: [22.0, 26.0, 24.0],
        target: [0.0, 0.0, -4.0],
        fov_y_radians: 0.85,
        aspect: width as f32 / height as f32,
        near: 0.2,
        far: 200.0,
    };
    let rock = ScatterInstance {
        position: [0.0, 0.0, 0.0],
        yaw: 0.2,
        scale: 9.0,
        normal: [0.0, 1.0, 0.0],
        tint: [0.72, 0.28, 0.16],
    };
    let pixels = render_map(&Shot {
        device: &device,
        map: &map,
        camera: &camera,
        tod: &tod,
        fog: FogSettings::default(),
        shadow_focus: [0.0, 0.0, 0.0],
        shadow_radius: 36.0,
        scatter: &[rock],
        width,
        height,
    });
    // Ground samples sit outside the rock's footprint (~4.5 m). +Z is the lit side.
    let lit = project(&camera, [0.0, 0.15, 14.0], width, height).expect("lit sample on screen");
    let shadowed = project(&camera, [0.0, 0.15, -14.0], width, height).expect("shadow sample on screen");
    let lit_luma = patch_luma(&pixels, width, lit.0.saturating_sub(1), lit.1.saturating_sub(1));
    let shadow_luma = patch_luma(
        &pixels,
        width,
        shadowed.0.saturating_sub(1),
        shadowed.1.saturating_sub(1),
    );
    assert!(
        shadow_luma + 8.0 < lit_luma,
        "shadowed ground ({shadow_luma:.1} at {shadowed:?}) must be darker than lit ground ({lit_luma:.1} at {lit:?})"
    );
    assert!(lit_luma > 18.0, "lit ground is black ({lit_luma:.1})");
    assert!(
        shadow_luma < 200.0,
        "shadowed ground is blown out ({shadow_luma:.1})"
    );
}

#[test]
fn noon_fog_frame_matches_lavapipe_golden() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let shot = GapShot {
        sun: GapSun::Noon,
        fog: true,
        ..GapShot::default()
    };
    let pixels = render_gap_shot(&device, &shot, 800, 450).expect("evidence frame");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/evidence/do27-gaps/terrain_fog_on_noon.png");
    if regia_core::brand::env_var("REGIA_BLESS").ok().as_deref() == Some("1") {
        write_png(&path, 800, 450, &pixels);
        return;
    }
    let golden = read_png(&path);
    // Lavapipe builds differ by an LSB in rounding (llvmpipe 23 vs 26 seen: 1.2% of bytes off by 1), so the
    // reference compare allows rasterizer rounding only: every channel within ±2. Other lavapipe LLVM
    // majors and hardware GPUs follow the per-ICD table (`regia_test_utils::golden::pixel_golden_for`).
    let reference = PixelTolerance {
        max_channel_diff: 2,
        outlier_ppm: 0,
    };
    let info = device.adapter.get_info();
    if golden_icd(&info) == GoldenIcd::Hardware && !goldens_skipped() {
        // The golden is lavapipe-blessed (ADR-025); a hardware GPU rasterizes the ridge and the
        // fog differently (RTX 5090 SSIM 0.9655). Hold it to the sanity floor and say so.
        let score = ssim_rgba8(&pixels, &golden, 800, 450).expect("ssim");
        eprintln!(
            "terrain_fog_on_noon on {} (hardware): SSIM {score:.4} vs lavapipe golden, sanity floor {HARDWARE_SANITY_SSIM}",
            info.name
        );
        assert!(
            score >= HARDWARE_SANITY_SSIM,
            "noon fog frame on hardware is not even close to the lavapipe golden {}: SSIM {score:.4}",
            path.display()
        );
        return;
    }
    match check_pixel_golden(
        &info,
        "terrain_fog_on_noon",
        &pixels,
        &golden,
        800,
        450,
        reference,
    ) {
        Ok(verdict) => eprintln!("{verdict}"),
        Err(verdict) => panic!(
            "noon fog frame drifted from the lavapipe golden {}: {verdict}",
            path.display()
        ),
    }
}

fn patch_delta(clear: &[u8], fogged: &[u8], width: u32, at: (u32, u32)) -> f64 {
    let x = at.0.saturating_sub(1);
    let y = at.1.saturating_sub(1);
    let mut sum = 0.0;
    let mut n = 0.0;
    for dy in 0..3 {
        for dx in 0..3 {
            let a = rgb_at(clear, width, x + dx, y + dy);
            let b = rgb_at(fogged, width, x + dx, y + dy);
            let dr = a[0] as f64 - b[0] as f64;
            let dg = a[1] as f64 - b[1] as f64;
            let db = a[2] as f64 - b[2] as f64;
            sum += (dr * dr + dg * dg + db * db).sqrt();
            n += 1.0;
        }
    }
    sum / n
}

fn patch_blue(clear: &[u8], fogged: &[u8], width: u32, at: (u32, u32)) -> f64 {
    let x = at.0.saturating_sub(1);
    let y = at.1.saturating_sub(1);
    let mut sum = 0.0;
    let mut n = 0.0;
    for dy in 0..3 {
        for dx in 0..3 {
            let a = rgb_at(clear, width, x + dx, y + dy);
            let b = rgb_at(fogged, width, x + dx, y + dy);
            let before = a[2] as f64 - a[0] as f64;
            let after = b[2] as f64 - b[0] as f64;
            sum += after - before;
            n += 1.0;
        }
    }
    sum / n
}

fn mean_luma(pixels: &[u8]) -> f64 {
    let n = pixels.len() / 4;
    let mut sum = 0.0;
    for i in 0..n {
        sum += 0.2126 * pixels[i * 4] as f64
            + 0.7152 * pixels[i * 4 + 1] as f64
            + 0.0722 * pixels[i * 4 + 2] as f64;
    }
    sum / n as f64
}

fn read_png(path: &std::path::Path) -> Vec<u8> {
    let file = std::fs::File::open(path).unwrap_or_else(|err| panic!("open {}: {err}", path.display()));
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().expect("png");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("png output size")];
    let info = reader.next_frame(&mut buf).expect("frame");
    buf.truncate(info.buffer_size());
    assert_eq!(info.width, 800);
    assert_eq!(info.height, 450);
    buf
}

fn write_png(path: &std::path::Path, width: u32, height: u32, pixels: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("evidence dir");
    }
    let file = std::fs::File::create(path).expect("create golden");
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("header");
    writer.write_image_data(pixels).expect("pixels");
}
