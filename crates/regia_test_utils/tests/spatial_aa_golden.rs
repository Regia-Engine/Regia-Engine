// SPDX-License-Identifier: MPL-2.0
//! SMAA 1× and the SGSR spatial upscale (mobile-aa lane): what each does to a frame, and
//! goldens of the Mobile High path (85 %, SMAA at the internal size, SGSR to the output).

use regia_render::{
    FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SpatialUpscaler, TonemapOperator,
};
use regia_test_utils::golden::{DEFAULT_SSIM_THRESHOLD, compare_rgba8_to_png, golden_path};

const W: u32 = 128;
const H: u32 = 96;

fn vert(position: [f32; 3]) -> LabVertex {
    LabVertex {
        position,
        normal: [0.0, 0.0, 1.0],
        uv: [0.0, 0.0],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    }
}

/// A quad from four corners (counter-clockwise), at `z`.
fn quad(c: [[f32; 2]; 4], z: f32, material: usize) -> LabMesh {
    LabMesh {
        vertices: c.iter().map(|p| vert([p[0], p[1], z])).collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// A dark wedge, a few thin leaning bars and a bright tilted square over a mid backdrop:
/// near-horizontal, near-vertical and diagonal edges plus 1–2 px features.
fn scene(width: u32, height: u32) -> LabScene {
    let mut meshes = vec![quad(
        [[-4.0, -3.0], [4.0, -3.0], [4.0, 3.0], [-4.0, 3.0]],
        -0.5,
        0,
    )];
    meshes.push(quad(
        [[-1.9, -1.3], [-0.2, -1.1], [-0.2, -0.9], [-1.9, -1.2]],
        0.0,
        1,
    ));
    for k in 0..6 {
        let x = 0.2 + 0.22 * k as f32;
        let lean = 0.04 + 0.03 * k as f32;
        let w = 0.025;
        meshes.push(quad(
            [
                [x - w, -1.3],
                [x + w, -1.3],
                [x + w + lean, 1.3],
                [x - w + lean, 1.3],
            ],
            0.0,
            1,
        ));
    }
    meshes.push(quad(
        [[-1.6, 0.0], [-0.6, 0.35], [-0.95, 1.3], [-1.95, 0.95]],
        0.0,
        2,
    ));
    let material = |c: f32| LabMaterial {
        base_color: [c, c, c, 1.0],
        unlit: true,
        ..LabMaterial::default()
    };
    LabScene {
        width,
        height,
        eye: [0.0, 0.0, 3.2],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.0,
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        directional: regia_render::DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![material(0.35), material(0.02), material(0.9)],
        meshes,
    }
}

fn post(fxaa: bool) -> PostSettings {
    PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        exposure_ev: 0.0,
        tonemap: TonemapOperator::AgX,
        fxaa,
        taa: false,
        film_grain: 0.0,
        vignette: 0.0,
        ..PostSettings::default()
    }
}

fn quality(scale: f32, smaa: bool, upscaler: SpatialUpscaler) -> FrameQuality {
    let mut q = FrameQuality::LAB;
    q.render_scale_permille = (scale * 1000.0).round() as u16;
    q.smaa = smaa;
    q.spatial_upscaler = upscaler;
    q
}

fn render(device: &RenderDevice, q: FrameQuality, fxaa: bool) -> Vec<u8> {
    let scene = scene(W, H);
    let settings = post(fxaa);
    let mut fs = scene.as_frame_scene_with_quality(q);
    fs.post = Some(&settings);
    let out = FrameRenderer::new().render_offscreen(device, &fs).expect("frame");
    assert_eq!((out.width, out.height), (W, H), "output size");
    out.pixels
}

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

fn rgb(pixels: &[u8]) -> Vec<[f32; 3]> {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            [
                f32::from(p[0]) / 255.0,
                f32::from(p[1]) / 255.0,
                f32::from(p[2]) / 255.0,
            ]
        })
        .collect()
}

/// The scene at 4× in each axis without AA, box-filtered down in linear light.
fn reference(device: &RenderDevice) -> Vec<[f32; 3]> {
    let s = 4;
    let scene = scene(W * s, H * s);
    let settings = post(false);
    let mut fs = scene.as_frame_scene_with_quality(FrameQuality::LAB);
    fs.post = Some(&settings);
    let big = FrameRenderer::new()
        .render_offscreen(device, &fs)
        .expect("reference");
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        for x in 0..W {
            let mut acc = [0.0f32; 3];
            for sy in 0..s {
                for sx in 0..s {
                    let i = (((y * s + sy) * big.width + x * s + sx) * 4) as usize;
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += srgb_to_linear(f32::from(big.pixels[i + c]) / 255.0);
                    }
                }
            }
            out.push(acc.map(|a| linear_to_srgb(a / (s * s) as f32)));
        }
    }
    out
}

/// The three most common green values of a frame: the flat levels of the three materials.
fn flat_levels(pixels: &[u8]) -> [u8; 3] {
    let mut counts = [0usize; 256];
    for p in pixels.as_chunks::<4>().0.iter() {
        counts[usize::from(p[1])] += 1;
    }
    let mut order: Vec<usize> = (0..256).collect();
    order.sort_by_key(|&v| std::cmp::Reverse(counts[v]));
    [order[0] as u8, order[1] as u8, order[2] as u8]
}

/// Pixels more than 2 levels away from every flat level (anti-aliased edge texels).
fn intermediate(pixels: &[u8], levels: [u8; 3]) -> usize {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| levels.iter().all(|l| p[1].abs_diff(*l) > 2))
        .count()
}

#[test]
fn smaa_and_sgsr_shape_the_frame_and_match_goldens() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip smaa_and_sgsr_shape_the_frame_and_match_goldens: {err}");
            return;
        }
    };

    // SMAA at native size: more in-between texels along the edges, flat regions untouched.
    let none = render(&device, quality(1.0, false, SpatialUpscaler::Bilinear), false);
    let smaa = render(&device, quality(1.0, true, SpatialUpscaler::Bilinear), false);
    let levels = flat_levels(&none);
    let (n_none, n_smaa) = (intermediate(&none, levels), intermediate(&smaa, levels));
    assert!(
        n_smaa > n_none + n_none / 4,
        "SMAA must blend the staircases: {n_none} → {n_smaa} intermediate texels"
    );
    // A pixel whose 5×5 neighbourhood is flat in the no-AA frame keeps its value.
    let mut flat_changed = 0;
    for y in 2..H - 2 {
        for x in 2..W - 2 {
            let c = none[((y * W + x) * 4) as usize];
            let flat = (0..25).all(|k| {
                let (dx, dy) = (k % 5, k / 5);
                none[(((y + dy - 2) * W + x + dx - 2) * 4) as usize] == c
            });
            if flat && smaa[((y * W + x) * 4) as usize] != c {
                flat_changed += 1;
            }
        }
    }
    assert_eq!(
        flat_changed, 0,
        "SMAA changed {flat_changed} pixels away from any edge"
    );

    // At 75 %, SGSR beats the bilinear upsample against a 4×4 supersampled reference.
    let truth = reference(&device);
    let ssim = |pixels: &[u8]| regia_render::compare::ssim(&rgb(pixels), &truth, W, H);
    let bilinear = render(&device, quality(0.75, false, SpatialUpscaler::Bilinear), false);
    let sgsr = render(&device, quality(0.75, false, SpatialUpscaler::Sgsr), false);
    let (s_bilinear, s_sgsr) = (ssim(&bilinear), ssim(&sgsr));
    eprintln!(
        "ssim vs 4x4 reference: native {:.4}, native smaa {:.4}, 0.75 bilinear {s_bilinear:.4}, 0.75 sgsr {s_sgsr:.4}",
        ssim(&none),
        ssim(&smaa)
    );
    assert!(
        s_sgsr > s_bilinear,
        "SGSR must be closer to the reference than the bilinear upsample: {s_sgsr:.4} vs {s_bilinear:.4}"
    );

    // The Mobile High preset takes exactly this path.
    let mobile = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::MobileHigh));
    assert!(mobile.smaa && mobile.spatial_upscaler == SpatialUpscaler::Sgsr && !mobile.taau);
    assert_eq!(mobile.render_scale_permille, 850);
    let mobile_px = render(&device, quality(0.85, true, SpatialUpscaler::Sgsr), false);

    for (name, pixels) in [
        ("spatial_aa_smaa_native", &smaa),
        ("spatial_aa_mobile_high", &mobile_px),
    ] {
        let path = golden_path(name);
        compare_rgba8_to_png(pixels, W, H, &path, DEFAULT_SSIM_THRESHOLD)
            .unwrap_or_else(|err| panic!("{name}: {err:#}"));
    }
}
