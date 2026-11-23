// SPDX-License-Identifier: MPL-2.0
//! The thin film against the path tracer (ADR-1977): both renderers build their direct term from
//! `thin_film.wgsl`, so the forward pass's `THIN_FILM` variant equals the tracer's `direct` AOV on
//! a sphere plate under the sun, per channel. The CPU mirror (`cpu_brdf`) agrees with both.
//!
//! `THIN_FILM_PLATES=<dir>` writes the by-eye plates: the plain material, the film, the forward
//! and tracer views of the same grid (`plate_*.png`).

use std::path::Path;

use regia_core::glam::{Mat4, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtSettings, PtSky};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    HardSurfaceFactors, LabMaterial, LabTexture, PostSettings, RenderDevice, ShadowCast, ThinFilmFactors,
    uv_sphere,
};

const COLS: usize = 6;
const ROWS: usize = 4;

/// A grid of filmed spheres: thickness across, base down (dark dielectric, gold, a clearcoat over
/// the film, multi-scatter energy on a rough base).
fn plate(film: bool) -> Vec<LabMaterial> {
    let mut materials = Vec::new();
    for row in 0..ROWS {
        for col in 0..COLS {
            let thickness_max = 120.0 + 80.0 * col as f32;
            let mut m = LabMaterial {
                roughness: 0.3,
                ..LabMaterial::EMPTY
            };
            let mut ior = 1.33;
            match row {
                0 => m.base_color = [0.02, 0.03, 0.05, 1.0],
                1 => {
                    m.base_color = [1.0, 0.78, 0.34, 1.0];
                    m.metallic = 1.0;
                    ior = 1.8;
                }
                2 => {
                    m.base_color = [0.1, 0.25, 0.6, 1.0];
                    m.hard_surface = HardSurfaceFactors {
                        clearcoat: 1.0,
                        clearcoat_roughness: 0.2,
                        ..HardSurfaceFactors::OFF
                    };
                }
                _ => {
                    m.base_color = [0.5, 0.5, 0.52, 1.0];
                    m.metallic = 1.0;
                    m.roughness = 0.6;
                    m.hard_surface = HardSurfaceFactors {
                        multi_scatter: 1.0,
                        ..HardSurfaceFactors::OFF
                    };
                }
            }
            if film {
                m.thin_film = ThinFilmFactors {
                    factor: 1.0,
                    ior,
                    thickness_min: 100.0,
                    thickness_max,
                };
            }
            materials.push(m);
        }
    }
    materials
}

fn frame_scene<'a>(
    materials: &'a [LabMaterial],
    verts: &'a [regia_render::LabVertex],
    idx: &'a [u32],
    post: &'a PostSettings,
    (width, height): (u32, u32),
    ibl: bool,
) -> FrameScene<'a> {
    let meshes: Vec<FrameMesh<'_>> = materials
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let (row, col) = ((i / COLS) as f32, (i % COLS) as f32);
            FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 100 + i as u64,
                object_key: 200 + i as u64,
                vertices: verts,
                indices: idx,
                material: m,
                model: Mat4::from_translation(Vec3::new(
                    (col - (COLS as f32 - 1.0) / 2.0) * 2.5,
                    ((ROWS as f32 - 1.0) / 2.0 - row) * 2.5,
                    0.0,
                ))
                .to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
        })
        .collect();
    FrameScene {
        width,
        height,
        eye: [0.0, 0.0, 19.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.62,
        near: 0.1,
        far: 100.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([-0.35, -0.5, -0.8], [1.0, 0.98, 0.95], 20_000.0),
        local_lights: &[],
        // Specular AA widens the forward lobe (a screen-space filter the 1-spp trace lacks).
        quality: FrameQuality {
            specular_aa: false,
            ..FrameQuality::LAB
        },
        meshes,
    }
}

fn post() -> PostSettings {
    PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        taa: false,
        ..PostSettings::default()
    }
}

/// `sRGB` bytes of linear scene colours, exposed to a mid-grey the eye can compare.
fn write_plate(dir: &Path, name: &str, (w, h): (u32, u32), rgb: &[[f32; 3]], scale: f32) {
    let encode = |v: f32| {
        let v = (v * scale).clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0 + 0.5) as u8
    };
    let bytes: Vec<u8> = rgb
        .iter()
        .flat_map(|c| [encode(c[0]), encode(c[1]), encode(c[2]), 255])
        .collect();
    std::fs::create_dir_all(dir).expect("plate dir");
    let file = std::fs::File::create(dir.join(name)).expect("plate file");
    let mut encoder = png::Encoder::new(file, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&bytes).expect("png pixels");
    writer.finish().expect("png finish");
}

#[test]
fn forward_direct_equals_pt_direct_on_the_film_plate() {
    let device = RenderDevice::headless().expect("headless device");
    let (verts, idx) = uv_sphere(1.0, 64, 32);
    let materials = plate(true);
    let post = post();
    let scene = frame_scene(&materials, &verts, &idx, &post, (240, 160), false);
    let mut renderer = FrameRenderer::new();
    renderer.request_hdr_capture();
    renderer.render_offscreen(&device, &scene).expect("forward frame");
    assert_eq!(renderer.thin_film_stats().drawn, COLS * ROWS);
    let hdr = renderer
        .take_hdr_capture(&device)
        .expect("hdr readback")
        .expect("the post stack ran, so the HDR copy exists");
    let pt_scene = from_frame(
        &scene,
        &ExtractOptions {
            sky: Some(PtSky::None),
            ..ExtractOptions::default()
        },
    );
    let tracer = PathTracer::new(&device).expect("compile");
    let settings = PtSettings {
        spp: 1,
        max_bounces: 1,
        jitter: false,
        ..PtSettings::default()
    };
    let img = tracer.render(&pt_scene, &settings).expect("trace");
    let (w, h) = (scene.width as usize, scene.height as usize);
    let peak = |c: [f32; 3]| c[0].max(c[1]).max(c[2]);
    let lit: Vec<f32> = img.direct.iter().map(|c| peak(*c)).filter(|v| *v > 0.0).collect();
    let floor = 0.01 * lit.iter().sum::<f32>() / lit.len().max(1) as f32;
    let (mut rel_sum, mut worst_mean, mut n) = (0.0f64, 0.0f64, 0usize);
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            let inside = [i, i - 1, i + 1, i - w, i + w]
                .iter()
                .all(|&j| img.depth[j] > 0.0);
            if !inside || img.shadow[i] < 0.5 || peak(img.direct[i]) < floor {
                continue;
            }
            for k in 0..3 {
                let (f, p) = (f64::from(hdr.rgb[i][k]), f64::from(img.direct[i][k]));
                if p < f64::from(floor) {
                    continue;
                }
                let e = (f - p).abs() / p;
                rel_sum += e;
                worst_mean = worst_mean.max(e);
                n += 1;
            }
        }
    }
    assert!(n > 3000, "{n} compared channels");
    let mre = rel_sum / n as f64;
    eprintln!("film forward vs PT direct: {n} channels, mean relative error {mre:.5}, worst {worst_mean:.4}");
    assert!(
        mre < 0.012,
        "forward film vs PT direct: mean relative error {mre}"
    );
}

/// The tracer's `direct` of a film differs from the same material's without one, in colour as well
/// as in level: the film is really on in the kernel.
#[test]
fn the_tracer_shows_the_film_and_no_film_is_the_old_material() {
    let device = RenderDevice::headless().expect("headless device");
    let (verts, idx) = uv_sphere(1.0, 32, 16);
    let post = post();
    let tracer = PathTracer::new(&device).expect("compile");
    let settings = PtSettings {
        spp: 1,
        max_bounces: 1,
        jitter: false,
        ..PtSettings::default()
    };
    let trace = |materials: &[LabMaterial]| {
        let scene = frame_scene(materials, &verts, &idx, &post, (120, 80), false);
        let pt_scene = from_frame(
            &scene,
            &ExtractOptions {
                sky: Some(PtSky::None),
                ..ExtractOptions::default()
            },
        );
        tracer.render(&pt_scene, &settings).expect("trace").direct
    };
    let (plain, film) = (trace(&plate(false)), trace(&plate(true)));
    let moved = plain
        .iter()
        .zip(&film)
        .filter(|(a, b)| (0..3).any(|k| (a[k] - b[k]).abs() > 0.02 * a[k].abs().max(0.01)))
        .count();
    assert!(moved > 500, "{moved} pixels moved");
    // A factor of 0 packs no film: the tracer draws the plain material exactly.
    let mut off = plate(true);
    for m in &mut off {
        m.thin_film.factor = 0.0;
    }
    assert_eq!(trace(&off), plain);
}

/// By-eye plates for the lane report. Runs only with `THIN_FILM_PLATES` set.
#[test]
fn write_the_plates() {
    let Ok(dir) = std::env::var("THIN_FILM_PLATES") else {
        return;
    };
    let dir = Path::new(&dir);
    let device = RenderDevice::headless().expect("headless device");
    let (verts, idx) = uv_sphere(1.0, 64, 32);
    let post = post();
    let size = (960, 640);
    for (name, film) in [("before", false), ("after", true)] {
        let materials = plate(film);
        let scene = frame_scene(&materials, &verts, &idx, &post, size, true);
        let mut renderer = FrameRenderer::new();
        renderer.request_hdr_capture();
        renderer.render_offscreen(&device, &scene).expect("frame");
        let hdr = renderer
            .take_hdr_capture(&device)
            .expect("hdr readback")
            .expect("hdr");
        write_plate(
            dir,
            &format!("plate_forward_{name}.png"),
            size,
            &hdr.rgb,
            1.0 / 9000.0,
        );
    }
    // Forward and tracer on the same sun-only grid.
    let materials = plate(true);
    let scene = frame_scene(&materials, &verts, &idx, &post, size, false);
    let mut renderer = FrameRenderer::new();
    renderer.request_hdr_capture();
    renderer.render_offscreen(&device, &scene).expect("frame");
    let hdr = renderer
        .take_hdr_capture(&device)
        .expect("hdr readback")
        .expect("hdr");
    write_plate(dir, "plate_sun_forward.png", size, &hdr.rgb, 1.0 / 9000.0);
    let pt_scene = from_frame(
        &scene,
        &ExtractOptions {
            sky: Some(PtSky::None),
            ..ExtractOptions::default()
        },
    );
    let tracer = PathTracer::new(&device).expect("compile");
    let settings = PtSettings {
        spp: 1,
        max_bounces: 1,
        jitter: false,
        ..PtSettings::default()
    };
    let img = tracer.render(&pt_scene, &settings).expect("trace");
    write_plate(dir, "plate_sun_pt.png", size, &img.direct, 1.0 / 9000.0);
    // The maps: no map, a thickness ramp along v, coverage stripes along u.
    let film = LabMaterial {
        base_color: [0.015, 0.02, 0.03, 1.0],
        roughness: 0.28,
        thin_film: ThinFilmFactors {
            factor: 1.0,
            ior: 1.56,
            thickness_min: 150.0,
            thickness_max: 700.0,
        },
        ..LabMaterial::EMPTY
    };
    let image = |pixel: &dyn Fn(f32, f32) -> [u8; 4]| {
        let mut rgba = Vec::with_capacity(256 * 256 * 4);
        for y in 0..256 {
            for x in 0..256 {
                rgba.extend_from_slice(&pixel(x as f32 / 255.0, y as f32 / 255.0));
            }
        }
        LabTexture {
            width: 256,
            height: 256,
            rgba,
        }
    };
    let materials = vec![
        film.clone(),
        LabMaterial {
            thin_film_map: Some(image(&|_, v| [255, (v * 255.0) as u8, 0, 255])),
            ..film.clone()
        },
        LabMaterial {
            thin_film_map: Some(image(&|u, _| {
                [
                    if ((u * 12.0) as u32).is_multiple_of(2) {
                        255
                    } else {
                        0
                    },
                    255,
                    0,
                    255,
                ]
            })),
            ..film.clone()
        },
    ];
    let scene = frame_scene(&materials, &verts, &idx, &post, size, true);
    let mut renderer = FrameRenderer::new();
    renderer.request_hdr_capture();
    renderer.render_offscreen(&device, &scene).expect("frame");
    let hdr = renderer
        .take_hdr_capture(&device)
        .expect("hdr readback")
        .expect("hdr");
    write_plate(dir, "plate_maps.png", size, &hdr.rgb, 1.0 / 9000.0);
    // The forward pass's time on the software rasterizer, filmed against plain (a ratio, printed
    // for the report; nothing is asserted on a wall clock).
    for (name, film) in [("plain", false), ("filmed", true)] {
        let materials = plate(film);
        let scene = frame_scene(&materials, &verts, &idx, &post, size, true);
        let mut renderer = FrameRenderer::new();
        renderer.render_offscreen(&device, &scene).expect("first frame");
        let start = std::time::Instant::now();
        for _ in 0..4 {
            renderer.render_offscreen(&device, &scene).expect("frame");
        }
        eprintln!(
            "{name}: {:.1} ms per frame",
            start.elapsed().as_secs_f64() * 250.0
        );
    }
}
