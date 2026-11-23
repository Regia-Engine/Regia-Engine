// SPDX-License-Identifier: MPL-2.0
//! The highlight peak of the `specs/27` § 2.2 direct row (ADR-3190): the forward's sun specular on
//! smooth spheres against the path tracer, on the row's own scene (`spec27_calibration.rs`'s
//! material grid: dielectric, metal and clearcoat rows × roughness 0.1 … 1.0, sun only, pixel
//! centres, specular AA off) and its own metric (`regia_test_utils::gi_error::measure`'s
//! `rel_rmse`, target ≤ 1 %).
//!
//! The tracer samples the sun as the 0.27° disc it is; the forward lit it as a point. A 0.1-rough
//! lobe (α = 0.01) is about twice as wide as the sun's half-angle, so on its core the two differ by
//! more than the row's tolerance. `DirectionalLightUniform::with_specular_disc` lights the forward's
//! specular from the disc (`pbr_lighting.wgsl` `disc_lobe_scale`). These tests compare against a
//! converged trace of the disc. The calibration suite's 1-spp trace takes one point on the disc per
//! pixel, so its own noise on those cores is larger than the target: the third test pins that.
//!
//! Hand-run on a device (lavapipe is one; minutes of tracing there, seconds on a GPU):
//! `cargo test -p regia_pt --test specular_peak -- --ignored --test-threads=1 --nocapture`.

use std::sync::{Mutex, OnceLock, PoisonError};

use regia_core::glam::{Mat4, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtImage, PtSettings, PtSky};
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer,
    HardSurfaceFactors, LabMaterial, LabMesh, LabScene, OutdoorParams, PostSettings, RenderDevice,
};
use regia_test_utils::gi_error::{GiError, GiErrorInput, luminance, measure};

/// The sun's angular radius, as the calibration suite traces it (`SUN_RADIUS` there).
const SUN_RADIUS: f32 = 0.004_675;
/// Samples of the converged reference: its noise on the grid is about 9 % / √spp of the row.
const CONVERGED_SPP: u32 = 1024;
/// The row's target: mean rel. RMSE ≤ 1 %.
const ROW_TARGET: f32 = 0.01;

static SERIAL_GPU: Mutex<()> = Mutex::new(());

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("a GPU device; lavapipe is one"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

/// `spec27_calibration.rs`'s `material_grid`, with the sun's specular disc on or off.
fn material_grid(disc: bool) -> LabScene {
    let (verts, idx) = regia_render::uv_sphere(1.0, 64, 32);
    let mut materials = Vec::new();
    let mut meshes = Vec::new();
    for r in 0..3 {
        for col in 0..5 {
            let roughness = [0.1, 0.3, 0.5, 0.7, 1.0][col];
            let mut m = LabMaterial {
                roughness,
                ..LabMaterial::EMPTY
            };
            match r {
                0 => m.base_color = [0.8, 0.2, 0.15, 1.0],
                1 => {
                    m.base_color = [1.0, 0.78, 0.34, 1.0];
                    m.metallic = 1.0;
                }
                _ => {
                    m.base_color = [0.1, 0.25, 0.6, 1.0];
                    m.hard_surface = HardSurfaceFactors {
                        clearcoat: 1.0,
                        clearcoat_roughness: 0.2,
                        ..HardSurfaceFactors::OFF
                    };
                }
            }
            meshes.push(LabMesh {
                vertices: verts.clone(),
                indices: idx.clone(),
                material: materials.len(),
                model: Mat4::from_translation(Vec3::new(
                    (col as f32 - 2.0) * 2.6,
                    (1.0 - r as f32) * 2.6,
                    0.0,
                ))
                .to_cols_array_2d(),
                ..LabMesh::unskinned()
            });
            materials.push(m);
        }
    }
    let sun = DirectionalLightUniform::new([-0.4, -0.5, -0.77], [1.0, 0.98, 0.95], 20_000.0);
    LabScene {
        width: 320,
        height: 192,
        eye: [0.0, 0.0, 16.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.62,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 13.0,
            use_physical_camera: false,
            exposure_compensation: 0.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: if disc {
            sun.with_specular_disc(SUN_RADIUS)
        } else {
            sun
        },
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

/// The row's frame quality: LAB, specular AA off (a screen-space filter the trace does not apply).
fn quality() -> FrameQuality {
    FrameQuality {
        specular_aa: false,
        ..FrameQuality::LAB
    }
}

/// The forward frame's linear HDR, as the calibration suite renders it (airless, no DDGI).
fn raster(disc: bool) -> Vec<[f32; 3]> {
    let lab = material_grid(disc);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.request_hdr_capture();
    renderer
        .render_offscreen(device(), &lab.as_frame_scene_with_quality(quality()))
        .expect("forward frame");
    renderer
        .take_hdr_capture(device())
        .expect("read back")
        .expect("the post stack ran, so the HDR copy exists")
        .rgb
}

/// The tracer's direct AOV at pixel centres, the sun a disc of [`SUN_RADIUS`].
fn trace(spp: u32) -> PtImage {
    let lab = material_grid(false);
    let frame = lab.as_frame_scene_with_quality(quality());
    let pt = from_frame(
        &frame,
        &ExtractOptions {
            sky: Some(PtSky::None),
            sun_angular_radius: SUN_RADIUS,
            ..ExtractOptions::default()
        },
    );
    tracer()
        .render(
            &pt,
            &PtSettings {
                spp,
                max_bounces: 1,
                seed: 27,
                jitter: false,
                samples_per_submit: 8,
                ..PtSettings::default()
            },
        )
        .expect("trace")
}

fn converged() -> &'static PtImage {
    static C: OnceLock<PtImage> = OnceLock::new();
    C.get_or_init(|| trace(CONVERGED_SPP))
}

/// The row's pixels: interior sphere pixels (all five neighbours hit) with the sun visible.
fn surface(pt: &PtImage) -> Vec<bool> {
    let (w, h) = (pt.width as usize, pt.height as usize);
    (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            x > 0
                && y > 0
                && x + 1 < w
                && y + 1 < h
                && [i, i - 1, i + 1, i - w, i + w].iter().all(|&j| pt.depth[j] > 0.0)
                && pt.shadow[i] >= 0.5
        })
        .collect()
}

/// The row's metric of `test` against `reference` over `surface`.
fn score(reference: &[[f32; 3]], test: &[[f32; 3]], surface: &[bool], pt: &PtImage) -> GiError {
    let none = vec![false; surface.len()];
    measure(&GiErrorInput {
        reference,
        test,
        surface,
        shadow: &none,
        width: pt.width,
        height: pt.height,
        ev100: 13.0,
        scale: None,
    })
}

/// Relative error at the reference's brightest pixel of the 0.1-rough metal sphere (column 0,
/// middle row): the highlight peak itself.
fn metal_peak_error(
    reference: &[[f32; 3]],
    test: &[[f32; 3]],
    surface: &[bool],
    width: u32,
) -> (f32, f32, f32) {
    let w = width as usize;
    // The sphere at (−5.2, 0) spans the left eighth of the frame at mid height.
    let (i, p) = (0..reference.len())
        .filter(|&i| surface[i] && i % w < w / 4)
        .filter(|&i| (i / w).abs_diff(reference.len() / w / 2) < reference.len() / w / 6)
        .map(|i| (i, luminance(reference[i])))
        .fold((0usize, 0.0f32), |best, c| if c.1 > best.1 { c } else { best });
    let f = luminance(test[i]);
    ((f - p).abs() / p.max(1e-6), p, f)
}

/// `REGIA_SPECULAR_PEAK_DUMP=<dir>` writes `specular_peak.png`: the converged trace, the point sun
/// and the disc sun side by side (same frame, the row's EV 13, clamped, sRGB), and a second row
/// at 4 EV less so the 0.1-rough cores are not clipped.
fn dump(images: &[&[[f32; 3]]], width: u32, height: u32) {
    let Some(dir) = regia_core::brand::env_var_os("REGIA_SPECULAR_PEAK_DUMP") else {
        return;
    };
    let encode = |v: f32, exposure: f32| {
        let v = (v * exposure).clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0 + 0.5) as u8
    };
    let out_w = width * images.len() as u32;
    let mut rgb = Vec::with_capacity((out_w * height * 6) as usize);
    for ev in [13.0f32, 17.0] {
        let exposure = 1.0 / (1.2 * 2f32.powf(ev));
        for y in 0..height {
            for img in images {
                for x in 0..width {
                    let c = img[(y * width + x) as usize];
                    rgb.extend(c.iter().map(|v| encode(*v, exposure)));
                }
            }
        }
    }
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("dump dir");
    let file = std::fs::File::create(dir.join("specular_peak.png")).expect("dump file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), out_w, height * 2);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(&rgb))
        .expect("dump png");
}

#[test]
#[ignore = "hand-run on a device: a 1024-spp trace of the spec 27 grid (ADR-3190)"]
fn sun_disc_highlights_meet_the_direct_row_against_a_converged_trace() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let pt = converged();
    let mask = surface(pt);
    assert!(mask.iter().filter(|m| **m).count() > 10_000, "row pixels");
    let hdr = raster(true);
    dump(&[&pt.direct, &raster(false), &hdr], pt.width, pt.height);
    let e = score(&pt.direct, &hdr, &mask, pt);
    let (peak, p, f) = metal_peak_error(&pt.direct, &hdr, &mask, pt.width);
    eprintln!(
        "disc sun vs {CONVERGED_SPP}-spp trace: rel RMSE {:.3} % ({} px), FLIP {:.4}; metal 0.1 peak {p:.0} vs {f:.0} cd/m² ({:.2} %)",
        100.0 * e.rel_rmse,
        e.pixels,
        e.flip_mean,
        100.0 * peak
    );
    assert!(
        e.rel_rmse <= ROW_TARGET,
        "direct rel. RMSE {:.3} % against the row's 1 %",
        100.0 * e.rel_rmse
    );
    assert!(peak <= ROW_TARGET, "metal 0.1 peak {p} vs {f}");
}

#[test]
#[ignore = "hand-run on a device: a 1024-spp trace of the spec 27 grid (ADR-3190)"]
fn point_sun_misses_the_converged_disc_trace_on_the_highlight_cores() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let pt = converged();
    let mask = surface(pt);
    let point = raster(false);
    let e = score(&pt.direct, &point, &mask, pt);
    let (peak, p, f) = metal_peak_error(&pt.direct, &point, &mask, pt.width);
    let disc = score(&pt.direct, &raster(true), &mask, pt);
    eprintln!(
        "point sun vs {CONVERGED_SPP}-spp disc trace: rel RMSE {:.3} % ({} px; disc sun {:.3} %); metal 0.1 peak {p:.0} vs {f:.0} cd/m² ({:.2} %)",
        100.0 * e.rel_rmse,
        e.pixels,
        100.0 * disc.rel_rmse,
        100.0 * peak
    );
    // The cause the disc removes: the point sun's lobe is the disc's at its centre. A 0.1-rough
    // lobe (α = 0.01) is narrower than one pixel's spread of normals here, so each pixel centre
    // samples it at its own offset, too bright near the peak and too dark around it; the grid's
    // error is that pattern. Against the same converged trace the disc must take at least a third
    // of it away (measured on Metal: 1.061 % → 0.449 %). The default frame keeps the point sun
    // (ADR-3190).
    assert!(
        e.rel_rmse > 1.5 * disc.rel_rmse,
        "the point sun ({:.3} %) is now within 1.5x of the disc sun ({:.3} %): re-check ADR-3190",
        100.0 * e.rel_rmse,
        100.0 * disc.rel_rmse
    );
}

#[test]
#[ignore = "hand-run on a device: a 1024-spp trace of the spec 27 grid (ADR-3190)"]
fn a_one_sample_disc_trace_is_noisier_than_the_row_allows() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let pt = converged();
    let one = trace(1);
    let mask = surface(pt);
    let noise = score(&pt.direct, &one.direct, &mask, pt);
    eprintln!(
        "1-spp disc trace vs {CONVERGED_SPP}-spp: rel RMSE {:.3} % ({} px)",
        100.0 * noise.rel_rmse,
        noise.pixels
    );
    // `spec27_calibration.rs` scores the row against one sample of the disc per pixel: on the
    // 0.1- and 0.3-rough cores that one sample is itself several times the target away from the
    // disc's mean, so no forward can meet the row against it. Its trace needs about
    // (noise / 0.3 %)² samples (ADR-3190).
    assert!(
        noise.rel_rmse > 3.0 * ROW_TARGET,
        "the 1-spp reference is now within {:.3} %: re-check ADR-3190's harness finding",
        100.0 * noise.rel_rmse
    );
}
