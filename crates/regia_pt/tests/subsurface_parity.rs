// SPDX-License-Identifier: MPL-2.0
//! Screen-space subsurface scattering against the path tracer (ADR-2049).
//!
//! The tracer computes the light that lands on the plate (its `direct` AOV, with shadow rays and no
//! screen). The reference is that light under the full radial profile, integrated over the surface
//! in world space, which is what a diffusion BSSRDF gives (Jensen et al. 2001, d'Eon & Luebke 2007):
//!
//! ```text
//! L(x) = Σ_y direct(y) R(|p(x) - p(y)|) dA(y) + (1 - Σ_y R dA) direct(x)
//! ```
//!
//! The last term is the light that would come from surface the camera does not see, which the
//! frame's blur also keeps at the center. The frame runs the two 1D passes on the same scene.
//!
//! The tracer's own Burley BSSRDF (ADR-2725) stays off here (`ExtractOptions::subsurface` is false):
//! this holds the frame's blur to the profile over traced light. The two skin plates are a hard light
//! edge on a flat plate, and a sphere of a face's curvature across its terminator.
//!
//! `SUBSURFACE_PLATES=<dir>` writes the by-eye plates.

use std::path::Path;

use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtSettings, PtSky};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, SubsurfaceFactors, SubsurfaceProfile, uv_sphere,
};

const SIZE: u32 = 256;
const FOV_Y: f32 = 0.5;
const EXTENT: f32 = 2.0;

fn material(subsurface: SubsurfaceFactors) -> LabMaterial {
    LabMaterial {
        base_color: [0.8, 0.6, 0.5, 1.0],
        roughness: 1.0,
        ior: 1.001,
        subsurface,
        ..LabMaterial::default()
    }
}

fn base_scene(eye_z: f32, materials: Vec<LabMaterial>, meshes: Vec<LabMesh>) -> LabScene {
    LabScene {
        width: SIZE,
        height: SIZE,
        eye: [0.0, 0.0, eye_z],
        target: [0.0, 0.0, 0.0],
        fov_y: FOV_Y,
        near: 0.01,
        far: 10.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            taa: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0, 0.98, 0.95], 20_000.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

fn quad(x0: f32, x1: f32, normal: [f32; 3]) -> LabMesh {
    let corner = |x: f32, y: f32| LabVertex::from_pos_normal_uv([x, y, 0.0], normal, [0.5, 0.5]);
    LabMesh {
        vertices: vec![
            corner(x0, -EXTENT),
            corner(x1, -EXTENT),
            corner(x1, EXTENT),
            corner(x0, EXTENT),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 0,
        ..LabMesh::unskinned()
    }
}

fn quality(scatter: bool) -> FrameQuality {
    FrameQuality {
        subsurface_scattering: scatter,
        specular_aa: false,
        ..FrameQuality::LAB
    }
}

/// World position, surface normal and whether the pixel's ray hits the surface.
struct Surface {
    position: Vec<[f32; 3]>,
    area: Vec<f32>,
}

/// The pixels of a plate on z = 0 or a sphere of `radius` at the origin, from the camera's rays.
fn surface(eye_z: f32, radius: Option<f32>) -> Surface {
    let n = SIZE as usize;
    let tan = (FOV_Y * 0.5).tan();
    let mut position = vec![[0.0; 3]; n * n];
    let mut area = vec![0.0; n * n];
    for py in 0..n {
        for px in 0..n {
            let ray = [
                ((px as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0) * tan,
                (1.0 - (py as f32 + 0.5) / SIZE as f32 * 2.0) * tan,
                -1.0f32,
            ];
            let len2 = ray[0] * ray[0] + ray[1] * ray[1] + 1.0;
            let (t, normal) = match radius {
                None => (eye_z, [0.0, 0.0, 1.0]),
                Some(r) => {
                    let b = -eye_z; // o·d with d = ray, o = (0, 0, eye_z)
                    let c = eye_z * eye_z - r * r;
                    let disc = b * b - len2 * c;
                    if disc <= 0.0 {
                        continue;
                    }
                    let t = (-b - disc.sqrt()) / len2;
                    let p = [ray[0] * t, ray[1] * t, eye_z - t];
                    (t, [p[0] / r, p[1] / r, p[2] / r])
                }
            };
            position[py * n + px] = [ray[0] * t, ray[1] * t, eye_z - t];
            // dA of the pixel on the surface, along a ray of direction `ray` at parameter t.
            let pixel = 2.0 * tan / SIZE as f32;
            let cos = -(normal[0] * ray[0] + normal[1] * ray[1] - normal[2]) / len2.sqrt();
            // The pixel's solid angle at its distance, over the cosine.
            area[py * n + px] = pixel * pixel * t * t / (len2.sqrt() * cos.max(0.05));
        }
    }
    Surface { position, area }
}

/// The exact radial profile over the traced light, at pixel `(px, py)`. Gaussian by Gaussian, each
/// normalized over the pixels it covers, so one narrower than a pixel gives the pixel's own light and
/// not a point sample's overshoot. What falls off the visible surface stays at the center.
fn reference(
    direct: &[[f32; 3]],
    surface: &Surface,
    px: usize,
    py: usize,
    reach_mm: f32,
    window: i32,
) -> [f32; 3] {
    let n = SIZE as usize;
    let at = py * n + px;
    let p = surface.position[at];
    let profile = SubsurfaceProfile::Skin;
    let scale = reach_mm / profile.reach_mm();
    let total = [0, 1, 2].map(|ch| profile.gaussians().iter().map(|(_, w)| w[ch]).sum::<f32>());
    let mut out = [0.0f32; 3];
    for (variance, weights) in profile.gaussians() {
        let v = variance * scale * scale;
        let (mut den, mut num) = (0.0f32, [0.0f32; 3]);
        for dy in -window..=window {
            for dx in -window..=window {
                let (qx, qy) = (px as i32 + dx, py as i32 + dy);
                if qx < 0 || qy < 0 || qx >= n as i32 || qy >= n as i32 {
                    continue;
                }
                let q = qy as usize * n + qx as usize;
                if surface.area[q] <= 0.0 {
                    continue;
                }
                let s = surface.position[q];
                let r2_mm = ((p[0] - s[0]).powi(2) + (p[1] - s[1]).powi(2) + (p[2] - s[2]).powi(2)) * 1.0e6;
                let g = (-0.5 * r2_mm / v).exp() / (std::f32::consts::TAU * v) * surface.area[q] * 1.0e6;
                den += g;
                for ch in 0..3 {
                    num[ch] += g * direct[q][ch];
                }
            }
        }
        for ch in 0..3 {
            let held = den.max(1.0);
            let on_surface = num[ch] / held;
            let off_surface = (1.0 - den / held) * direct[at][ch];
            out[ch] += weights[ch] / total[ch] * (on_surface + off_surface);
        }
    }
    out
}

struct Plate {
    lab: LabScene,
    eye_z: f32,
    radius: Option<f32>,
}

fn flat_plate() -> Plate {
    let eye_z = 0.1;
    Plate {
        lab: base_scene(
            eye_z,
            vec![material(SubsurfaceFactors::new(SubsurfaceProfile::Skin))],
            // The left half faces the sun; the right half's normal is square to it and still faces
            // the camera, so both renderers shade it black and neither flips it.
            vec![
                quad(-EXTENT, 0.0, [0.0, 0.0, 1.0]),
                quad(0.0, EXTENT, [-1.0, 0.0, 0.0]),
            ],
        ),
        eye_z,
        radius: None,
    }
}

fn sphere_plate() -> Plate {
    // A tight curve (an ear, a nose): at a whole head's radius the terminator's scattering is
    // under 1 % of the peak.
    let radius = 0.02;
    let eye_z = 0.1;
    let (vertices, indices) = uv_sphere(radius, 256, 128);
    let mut lab = base_scene(
        eye_z,
        vec![material(SubsurfaceFactors::new(SubsurfaceProfile::Skin))],
        vec![LabMesh {
            vertices,
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    );
    // From the side: the terminator runs down the middle of what the camera sees.
    lab.directional = DirectionalLightUniform::new([-1.0, 0.0, -0.3], [1.0, 0.98, 0.95], 20_000.0);
    Plate {
        lab,
        eye_z,
        radius: Some(radius),
    }
}

fn forward(device: &RenderDevice, lab: &LabScene, scatter: bool) -> Vec<[f32; 3]> {
    let scene = lab.as_frame_scene_with_quality(quality(scatter));
    let mut renderer = FrameRenderer::new();
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, &scene).expect("frame");
    if scatter {
        assert_eq!(renderer.subsurface_stats().reason, None);
    }
    renderer
        .take_hdr_capture(device)
        .expect("readback")
        .expect("hdr copy")
        .rgb
}

fn trace(device: &RenderDevice, lab: &LabScene) -> Vec<[f32; 3]> {
    let scene = lab.as_frame_scene_with_quality(quality(false));
    let pt_scene = from_frame(
        &scene,
        &ExtractOptions {
            sky: Some(PtSky::None),
            ..ExtractOptions::default()
        },
    );
    let tracer = PathTracer::new(device).expect("compile");
    let settings = PtSettings {
        spp: 1,
        max_bounces: 1,
        jitter: false,
        ..PtSettings::default()
    };
    tracer.render(&pt_scene, &settings).expect("trace").direct
}

/// How the frames compare with the reference on `rows`, relative to the brightest lit pixel per
/// channel.
struct Comparison {
    /// Mean and worst error with the blur.
    mean: f32,
    worst: f32,
    /// Worst error of the frame without the blur, on the same pixels: what the blur has to fix.
    off_worst: f32,
    on: Vec<[f32; 3]>,
    reference: Vec<[f32; 3]>,
}

fn compare(plate: &Plate, device: &RenderDevice, rows: &[usize]) -> Comparison {
    let direct = trace(device, &plate.lab);
    let on = forward(device, &plate.lab, true);
    let off = forward(device, &plate.lab, false);
    let peak = [0, 1, 2].map(|ch| direct.iter().map(|c| c[ch]).fold(0.0f32, f32::max));
    assert!(peak.iter().all(|&p| p > 1.0), "{peak:?}");
    let same = surface(plate.eye_z, plate.radius);
    let n = SIZE as usize;
    // The tracer and the frame light the plate alike without the blur.
    for (i, a) in same.area.iter().enumerate() {
        if *a > 0.0 {
            for ch in 0..3 {
                let error = (direct[i][ch] - off[i][ch]).abs() / peak[ch];
                assert!(error < 2e-3, "pixel {i} ch {ch}: frame against tracer {error}");
            }
        }
    }
    let reach = SubsurfaceProfile::Skin.reach_mm();
    let px_per_mm = (SIZE as f32 * 0.5) / ((FOV_Y * 0.5).tan() * plate.eye_z) / 1000.0;
    let window = (reach * px_per_mm).ceil() as i32 + 2;
    let (mut sum, mut count, mut worst, mut off_worst) = (0.0f64, 0usize, 0.0f32, 0.0f32);
    let mut reference_image = vec![[0.0; 3]; n * n];
    for &py in rows {
        for px in 0..n {
            if same.area[py * n + px] <= 0.0 {
                continue;
            }
            // Only pixels whose whole kernel is on the surface: the silhouette is a screen-space
            // limit, listed in the book page.
            let inside = (-window..=window).step_by(window as usize).all(|dx| {
                (-window..=window).step_by(window as usize).all(|dy| {
                    let (qx, qy) = (px as i32 + dx, py as i32 + dy);
                    qx >= 0
                        && qy >= 0
                        && qx < n as i32
                        && qy < n as i32
                        && same.area[qy as usize * n + qx as usize] > 0.0
                })
            });
            if !inside {
                continue;
            }
            let expected = reference(&direct, &same, px, py, reach, window);
            reference_image[py * n + px] = expected;
            for ch in 0..3 {
                let error = (on[py * n + px][ch] - expected[ch]).abs() / peak[ch];
                sum += f64::from(error);
                worst = worst.max(error);
                off_worst = off_worst.max((off[py * n + px][ch] - expected[ch]).abs() / peak[ch]);
                count += 1;
            }
        }
    }
    assert!(count > 300, "{count} channels compared");
    Comparison {
        mean: (sum / count as f64) as f32,
        worst,
        off_worst,
        on,
        reference: reference_image,
    }
}

#[test]
fn the_edge_plate_matches_the_profile_over_traced_light() {
    let device = RenderDevice::headless().expect("device");
    let plate = flat_plate();
    let n = SIZE as usize;
    let c = compare(&plate, &device, &[n / 2, n / 2 + 13, n / 2 - 13]);
    eprintln!(
        "edge plate against the reference: mean {}, worst {}; without the blur, worst {}",
        c.mean, c.worst, c.off_worst
    );
    assert!(c.mean < 0.005, "mean {}", c.mean);
    // Per pixel: the taps' noise on a hard edge (the column average is tighter, `subsurface_frame`).
    assert!(c.worst < 0.1, "worst {}", c.worst);
    assert!(
        c.off_worst > 5.0 * c.worst,
        "the plate must tell the blur from none: {}",
        c.off_worst
    );
}

#[test]
fn the_sphere_plate_matches_the_profile_across_the_terminator() {
    let device = RenderDevice::headless().expect("device");
    let plate = sphere_plate();
    let n = SIZE as usize;
    let c = compare(&plate, &device, &[n / 2, n / 2 + 20, n / 2 - 20]);
    eprintln!(
        "sphere plate against the reference: mean {}, worst {}; without the blur, worst {}",
        c.mean, c.worst, c.off_worst
    );
    assert!(c.mean < 0.002, "mean {}", c.mean);
    assert!(c.worst < 0.006, "worst {}", c.worst);
    assert!(
        c.off_worst > 3.0 * c.worst,
        "the plate must tell the blur from none: {}",
        c.off_worst
    );
}

fn write_plate(dir: &Path, name: &str, rgb: &[[f32; 3]], scale: f32) {
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
    let mut encoder = png::Encoder::new(file, SIZE, SIZE);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&bytes).expect("png pixels");
    writer.finish().expect("png finish");
}

/// By-eye plates for the lane report. Runs only with `SUBSURFACE_PLATES` set.
#[test]
fn write_the_plates() {
    let Ok(dir) = std::env::var("SUBSURFACE_PLATES") else {
        return;
    };
    let dir = Path::new(&dir);
    let device = RenderDevice::headless().expect("device");
    for (name, plate) in [("edge", flat_plate()), ("sphere", sphere_plate())] {
        let n = SIZE as usize;
        let rows: Vec<usize> = (0..n).step_by(4).collect();
        let c = compare(&plate, &device, &rows);
        eprintln!(
            "{name}: mean {}, worst {}, without the blur {}",
            c.mean, c.worst, c.off_worst
        );
        let scale = 1.0 / 6000.0;
        write_plate(dir, &format!("{name}_blur.png"), &c.on, scale);
        write_plate(dir, &format!("{name}_reference_rows.png"), &c.reference, scale);
        write_plate(
            dir,
            &format!("{name}_off.png"),
            &forward(&device, &plate.lab, false),
            scale,
        );
    }
}
