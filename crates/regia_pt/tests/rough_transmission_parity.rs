// SPDX-License-Identifier: MPL-2.0
//! Rough transmission against the path tracer on a glass plate (ADR-2075): the forward pass's
//! refraction pass averages the lobe the tracer samples (`rough_transmission.wgsl`, shared), so a
//! frosted plate in front of a checkered emissive wall shows the same picture in both, within a
//! tolerance. The wall is unlit and the frame has no other light, so the plate's transmission is the
//! only term in either image.
//!
//! `ROUGH_TRANSMISSION_PLATES=<dir>` writes the by-eye plates: each case's forward and tracer image.

use std::path::Path;

use regia_core::glam::{Mat4, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtSettings, PtSky};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, PostSettings, RenderDevice, ShadowCast,
};

const SIZE: (u32, u32) = (200, 150);
const WALL_Z: f32 = -4.0;
const CELL: f32 = 1.0;
const BRIGHT: f32 = 400.0;
const DARK: f32 = 40.0;
/// The checker's half width, m. Past it the wall holds the checker's mean, out to [`FAR_WALL`], so a
/// wide lobe's rays that leave the checker still see the same mean light.
const CHECKER: i32 = 16;
const FAR_WALL: f32 = 2000.0;

/// Quads square to +z, as one mesh: `(cx, cy, half width, half height, z)`.
fn quads(rects: &[(f32, f32, f32, f32, f32)]) -> (Vec<LabVertex>, Vec<u32>) {
    let (mut verts, mut idx) = (Vec::new(), Vec::new());
    for &(cx, cy, hw, hh, z) in rects {
        let base = verts.len() as u32;
        for (x, y, u, v) in [
            (cx - hw, cy - hh, 0.0, 1.0),
            (cx + hw, cy - hh, 1.0, 1.0),
            (cx + hw, cy + hh, 1.0, 0.0),
            (cx - hw, cy + hh, 0.0, 0.0),
        ] {
            verts.push(LabVertex::from_pos_normal_uv([x, y, z], [0.0, 0.0, 1.0], [u, v]));
        }
        idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (verts, idx)
}

/// The wall's cells of one color of the checker, covering far past the view.
fn checker(even: bool) -> (Vec<LabVertex>, Vec<u32>) {
    let mut rects = Vec::new();
    for j in -CHECKER..CHECKER {
        for i in -CHECKER..CHECKER {
            if ((i + j).rem_euclid(2) == 0) == even {
                let (cx, cy) = ((i as f32 + 0.5) * CELL, (j as f32 + 0.5) * CELL);
                rects.push((cx, cy, CELL * 0.5, CELL * 0.5, WALL_Z));
            }
        }
    }
    quads(&rects)
}

/// Four quads around the checker.
fn surround() -> (Vec<LabVertex>, Vec<u32>) {
    let c = CHECKER as f32 * CELL;
    let mid = (FAR_WALL + c) * 0.5;
    let half = (FAR_WALL - c) * 0.5;
    quads(&[
        (-mid, 0.0, half, FAR_WALL, WALL_Z),
        (mid, 0.0, half, FAR_WALL, WALL_Z),
        (0.0, -mid, c, half, WALL_Z),
        (0.0, mid, c, half, WALL_Z),
    ])
}

fn emitter(level: f32) -> LabMaterial {
    LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        roughness: 1.0,
        emissive: [1.0, 1.0, 1.0],
        emissive_intensity: level,
        ..LabMaterial::EMPTY
    }
}

fn pane(rough: f32, ior: f32) -> LabMaterial {
    LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        roughness: 0.9,
        transmission: 1.0,
        ior,
        transmission_roughness: rough,
        ..LabMaterial::EMPTY
    }
}

struct Rig {
    wall_a: (Vec<LabVertex>, Vec<u32>),
    wall_b: (Vec<LabVertex>, Vec<u32>),
    wall_far: (Vec<LabVertex>, Vec<u32>),
    plate: (Vec<LabVertex>, Vec<u32>),
    bright: LabMaterial,
    dark: LabMaterial,
    mean: LabMaterial,
    post: PostSettings,
}

impl Rig {
    fn new() -> Self {
        Self {
            wall_a: checker(true),
            wall_b: checker(false),
            wall_far: surround(),
            plate: quads(&[(0.0, 0.0, 5.0, 5.0, 0.0)]),
            bright: emitter(BRIGHT),
            dark: emitter(DARK),
            mean: emitter((BRIGHT + DARK) * 0.5),
            post: PostSettings {
                enabled: true,
                bloom: false,
                auto_exposure: false,
                taa: false,
                ..PostSettings::default()
            },
        }
    }

    fn scene<'a>(&'a self, glass: Option<&'a LabMaterial>, tilt: f32, refraction: bool) -> FrameScene<'a> {
        let mesh = |key: u64,
                    mesh: &'a (Vec<LabVertex>, Vec<u32>),
                    material: &'a LabMaterial,
                    model: Mat4| FrameMesh {
            is_static: false,
            mesh_key: key,
            material_key: 100 + key,
            object_key: 200 + key,
            vertices: &mesh.0,
            indices: &mesh.1,
            material,
            model: model.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        };
        let mut meshes = vec![
            mesh(1, &self.wall_a, &self.bright, Mat4::IDENTITY),
            mesh(2, &self.wall_b, &self.dark, Mat4::IDENTITY),
            mesh(4, &self.wall_far, &self.mean, Mat4::IDENTITY),
        ];
        if let Some(glass) = glass {
            meshes.push(mesh(3, &self.plate, glass, Mat4::from_rotation_y(tilt)));
        }
        FrameScene {
            width: SIZE.0,
            height: SIZE.1,
            eye: [0.0, 0.0, 6.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.62,
            near: 0.1,
            far: 4000.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 9.0,
            ambient_lux: 0.0,
            double_sided: false,
            post: Some(&self.post),
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new(
                Vec3::new(0.0, -1.0, 0.0).to_array(),
                [1.0, 1.0, 1.0],
                0.0,
            ),
            local_lights: &[],
            quality: FrameQuality {
                specular_aa: false,
                screen_space_refraction: refraction,
                ..FrameQuality::LAB
            },
            meshes,
        }
    }
}

fn forward(device: &RenderDevice, scene: &FrameScene<'_>) -> Vec<[f32; 3]> {
    let mut renderer = FrameRenderer::new();
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, scene).expect("forward frame");
    renderer
        .take_hdr_capture(device)
        .expect("hdr readback")
        .expect("the post stack ran, so the HDR copy exists")
        .rgb
}

fn trace(tracer: &PathTracer, scene: &FrameScene<'_>, spp: u32) -> Vec<[f32; 3]> {
    let pt_scene = from_frame(
        scene,
        &ExtractOptions {
            sky: Some(PtSky::None),
            ..ExtractOptions::default()
        },
    );
    let settings = PtSettings {
        spp,
        max_bounces: 2,
        jitter: true,
        ..PtSettings::default()
    };
    tracer.render(&pt_scene, &settings).expect("trace").beauty
}

/// Green channel box-filtered over a `2 r + 1` square (a 16-tap forward average is still noisy per
/// pixel; the temporal resolve removes that in a real frame).
fn smooth(img: &[[f32; 3]], r: i32) -> Vec<f32> {
    let (w, h) = (SIZE.0 as i32, SIZE.1 as i32);
    let mut out = vec![0.0; img.len()];
    for y in 0..h {
        for x in 0..w {
            let (mut sum, mut n) = (0.0f32, 0.0f32);
            for dy in -r..=r {
                for dx in -r..=r {
                    let (px, py) = (x + dx, y + dy);
                    if px >= 0 && py >= 0 && px < w && py < h {
                        sum += img[(py * w + px) as usize][1];
                        n += 1.0;
                    }
                }
            }
            out[(y * w + x) as usize] = sum / n;
        }
    }
    out
}

/// The pixels of the central window: the plate covers them, and the lobe's taps stay on screen.
fn window(img: &[f32]) -> Vec<f32> {
    let (w, h) = (SIZE.0 as usize, SIZE.1 as usize);
    let mut out = Vec::new();
    for y in h / 4..3 * h / 4 {
        for x in w / 4..3 * w / 4 {
            out.push(img[y * w + x]);
        }
    }
    out
}

fn mean(v: &[f32]) -> f32 {
    v.iter().sum::<f32>() / v.len().max(1) as f32
}

/// `(p95 − p5) / mean`: how much of the checker survives the plate.
fn contrast(v: &[f32]) -> f32 {
    let mut sorted = v.to_vec();
    sorted.sort_by(f32::total_cmp);
    let at = |q: f32| sorted[((sorted.len() - 1) as f32 * q) as usize];
    (at(0.95) - at(0.05)) / mean(v).max(1e-6)
}

fn write_plate(dir: &Path, name: &str, img: &[[f32; 3]]) {
    let encode = |v: f32| {
        let v = (v / BRIGHT).clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0 + 0.5) as u8
    };
    let bytes: Vec<u8> = img
        .iter()
        .flat_map(|c| [encode(c[0]), encode(c[1]), encode(c[2]), 255])
        .collect();
    std::fs::create_dir_all(dir).expect("plate dir");
    let file = std::fs::File::create(dir.join(name)).expect("plate file");
    let mut encoder = png::Encoder::new(file, SIZE.0, SIZE.1);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&bytes).expect("png pixels");
    writer.finish().expect("png finish");
}

#[test]
fn a_frosted_plate_matches_the_tracer() {
    let device = RenderDevice::headless().expect("headless device");
    let tracer = PathTracer::new(&device).expect("compile");
    let rig = Rig::new();
    let plates = std::env::var("ROUGH_TRANSMISSION_PLATES").ok();
    let mut last_contrast = f32::MAX;
    for (rough, ior, tilt) in [
        (0.15f32, 1.5f32, 0.0f32),
        (0.3, 1.5, 0.0),
        (0.5, 1.5, 0.0),
        (0.8, 1.5, 0.0),
        (0.5, 1.8, 0.0),
        (0.4, 1.5, 0.45),
    ] {
        let glass = pane(rough, ior);
        let scene = rig.scene(Some(&glass), tilt, true);
        let f_rgb = forward(&device, &scene);
        let p_rgb = trace(&tracer, &scene, 128);
        if let Some(dir) = &plates {
            let name = format!(
                "r{}_ior{}_tilt{}",
                (rough * 100.0) as u32,
                ior,
                (tilt * 100.0) as u32
            );
            write_plate(Path::new(dir), &format!("{name}_forward.png"), &f_rgb);
            write_plate(Path::new(dir), &format!("{name}_pt.png"), &p_rgb);
        }
        let (f, p) = (window(&smooth(&f_rgb, 3)), window(&smooth(&p_rgb, 3)));
        let mre = f
            .iter()
            .zip(&p)
            .map(|(a, b)| f64::from((a - b).abs() / b.max(1.0)))
            .sum::<f64>()
            / f.len() as f64;
        let bias = mean(&f) / mean(&p) - 1.0;
        let (cf, cp) = (contrast(&f), contrast(&p));
        eprintln!(
            "rough {rough} ior {ior} tilt {tilt}: mean relative error {mre:.4}, bias {bias:+.4}, contrast forward {cf:.3} tracer {cp:.3}"
        );
        assert!(
            mre < 0.06,
            "rough {rough} ior {ior} tilt {tilt}: mean relative error {mre}"
        );
        assert!(
            bias.abs() < 0.01,
            "rough {rough} ior {ior} tilt {tilt}: bias {bias}"
        );
        assert!(
            (cf - cp).abs() < 0.1,
            "rough {rough} ior {ior} tilt {tilt}: contrast {cf} vs {cp}"
        );
        if ior == 1.5 && tilt == 0.0 {
            assert!(
                cp < last_contrast,
                "more roughness keeps less of the checker: {cp} after {last_contrast}"
            );
            last_contrast = cp;
        }
    }
}

/// The tracer's lobe keeps the light (only its direction spreads), and roughness 0 is the old pane
/// exactly: same pixels from the same random numbers.
#[test]
fn the_tracer_keeps_the_energy_and_zero_is_the_clear_pane() {
    let device = RenderDevice::headless().expect("headless device");
    let tracer = PathTracer::new(&device).expect("compile");
    let rig = Rig::new();
    let clear = pane(0.0, 1.5);
    let frosted = pane(0.6, 1.5);
    let clear_a = trace(&tracer, &rig.scene(Some(&clear), 0.0, true), 64);
    let clear_b = trace(&tracer, &rig.scene(Some(&clear), 0.0, true), 64);
    assert_eq!(clear_a, clear_b, "the tracer is deterministic");
    let rough = trace(&tracer, &rig.scene(Some(&frosted), 0.0, true), 64);
    let (mc, mr) = (window(&smooth(&clear_a, 0)), window(&smooth(&rough, 0)));
    let ratio = mean(&mr) / mean(&mc);
    eprintln!(
        "frosted / clear mean over the window: {ratio:.4}; contrast {:.3} -> {:.3}",
        contrast(&mc),
        contrast(&mr)
    );
    assert!((ratio - 1.0).abs() < 0.01, "{ratio}");
    assert!(contrast(&mr) < 0.6 * contrast(&mc));
}

/// Roughness 0 and a frame that does not refract draw the clear pane, byte for byte: the feature
/// costs nothing where it is off.
#[test]
fn forward_off_paths_are_the_clear_pane() {
    let device = RenderDevice::headless().expect("headless device");
    let rig = Rig::new();
    let clear = pane(0.0, 1.5);
    let frosted = pane(0.5, 1.5);
    let base = forward(&device, &rig.scene(Some(&clear), 0.0, false));
    assert_eq!(
        forward(&device, &rig.scene(Some(&clear), 0.0, true)),
        base,
        "no refraction pass for a thin clear pane"
    );
    assert_eq!(
        forward(&device, &rig.scene(Some(&frosted), 0.0, false)),
        base,
        "a frame that does not refract"
    );
    let on = forward(&device, &rig.scene(Some(&frosted), 0.0, true));
    assert_ne!(on, base);
    assert_eq!(
        forward(&device, &rig.scene(Some(&frosted), 0.0, true)),
        on,
        "deterministic"
    );
}
