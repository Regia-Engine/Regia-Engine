// SPDX-License-Identifier: MPL-2.0
//! Specular anti-aliasing acceptance (spec-aa): a shimmer metric on a slowly panning,
//! normal-mapped, glossy black plane under a low sun.
//!
//! The plane's diffuse is black, so every pixel's luminance is specular. The camera slides
//! sideways a fraction of a pixel per frame with temporal AA off, so any frame-to-frame
//! change is the highlight aliasing on minified normal detail. The metric is the mean squared
//! luma change between consecutive frames. "Before" is the renderer with
//! `FrameQuality::specular_aa` off; "after" turns it on (the geometric term on the shading
//! normal's derivatives plus the Toksvig term on the filtered normal map's length). Same
//! material both ways. Acceptance: the metric drops by at least 40 %.
//!
//! `REGIA_PREVIEW=1` writes the first frame of each variant to `/tmp/specular-aa-preview`.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabTexture,
    LabVertex, RenderDevice,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const FRAMES: u32 = 16;
/// Sideways eye motion per frame, metres (≈ ⅓ px on the near ground).
const PAN_PER_FRAME: f32 = 0.006;
const NORMAL_SIZE: u32 = 256;

fn hash(x: u32, y: u32, seed: u32) -> f32 {
    let mut h = x.wrapping_mul(0x8da6_b343) ^ y.wrapping_mul(0xd816_3841) ^ seed.wrapping_mul(0xcb1a_b31f);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    (h & 0x00ff_ffff) as f32 / 16_777_216.0
}

/// Per-texel random bumps: tilt up to ~0.45 rad in a random direction (hammered steel, ice
/// crust at a few cm per texel).
fn bumpy_normal_map() -> LabTexture {
    let mut rgba = Vec::with_capacity((NORMAL_SIZE * NORMAL_SIZE * 4) as usize);
    for y in 0..NORMAL_SIZE {
        for x in 0..NORMAL_SIZE {
            let tilt = 0.45 * hash(x, y, 1).sqrt();
            let phi = std::f32::consts::TAU * hash(x, y, 2);
            let n = [tilt.sin() * phi.cos(), tilt.sin() * phi.sin(), tilt.cos()];
            rgba.extend(n.iter().map(|c| ((c * 0.5 + 0.5) * 255.0 + 0.5) as u8));
            rgba.push(255);
        }
    }
    LabTexture {
        width: NORMAL_SIZE,
        height: NORMAL_SIZE,
        rgba,
    }
}

fn plane() -> LabMesh {
    // 24 × 24 m ground at y = 0, UV repeating 6× (4 m per tile, 1.6 cm per texel).
    let h = 12.0;
    let t = 6.0;
    let up = [0.0, 1.0, 0.0];
    let vertices = vec![
        LabVertex::from_pos_normal_uv([-h, 0.0, -h], up, [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([h, 0.0, -h], up, [t, 0.0]),
        LabVertex::from_pos_normal_uv([h, 0.0, h], up, [t, t]),
        LabVertex::from_pos_normal_uv([-h, 0.0, h], up, [0.0, t]),
    ];
    LabMesh {
        vertices,
        indices: vec![0, 2, 1, 0, 3, 2],
        material: 0,
        ..LabMesh::unskinned()
    }
}

fn material() -> LabMaterial {
    LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        metallic: 0.0,
        roughness: 0.2,
        normal_scale: 1.0,
        normal_map: Some(bumpy_normal_map()),
        ..LabMaterial::EMPTY
    }
}

fn scene(frame: u32) -> LabScene {
    let x = PAN_PER_FRAME * frame as f32;
    LabScene {
        width: WIDTH,
        height: HEIGHT,
        eye: [x, 1.4, 5.0],
        target: [x, 0.0, -4.0],
        fov_y: 0.8,
        near: 0.05,
        far: 60.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 15.5,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        // A low sun ahead of the camera: its mirror glint lies on the plane in view.
        directional: DirectionalLightUniform::new([0.0, -0.32, 1.0], [1.0, 0.98, 0.95], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![material()],
        meshes: vec![plane()],
    }
}

fn luma(pixels: &[u8]) -> Vec<f64> {
    pixels
        .chunks(4)
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .collect()
}

/// Global luma SSIM (one window the size of the frame; Wang et al. 2004).
fn luma_ssim(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let (mut va, mut vb, mut cov) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        va += (x - ma) * (x - ma);
        vb += (y - mb) * (y - mb);
        cov += (x - ma) * (y - mb);
    }
    let (va, vb, cov) = (va / n, vb / n, cov / n);
    let (c1, c2) = ((0.01f64 * 255.0).powi(2), (0.03f64 * 255.0).powi(2));
    ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))
}

struct Run {
    shimmer: f64,
    mean_luma: f64,
    first: Vec<u8>,
}

fn run(device: &RenderDevice, specular_aa: bool) -> Run {
    let mut renderer = FrameRenderer::new();
    let quality = FrameQuality {
        specular_aa,
        ..FrameQuality::LAB
    };
    let mut prev: Option<Vec<f64>> = None;
    let mut sum = 0.0;
    let mut pairs = 0usize;
    let mut first = Vec::new();
    let mut mean_luma = 0.0;
    for f in 0..FRAMES {
        let s = scene(f);
        let pixels = renderer
            .render_offscreen(device, &s.as_frame_scene_with_quality(quality))
            .expect("offscreen frame")
            .pixels;
        let l = luma(&pixels);
        mean_luma += l.iter().sum::<f64>() / l.len() as f64;
        if let Some(p) = &prev {
            sum += l.iter().zip(p).map(|(a, b)| (a - b) * (a - b)).sum::<f64>() / l.len() as f64;
            pairs += 1;
        } else {
            first = pixels;
        }
        prev = Some(l);
    }
    Run {
        shimmer: sum / pairs as f64,
        mean_luma: mean_luma / f64::from(FRAMES),
        first,
    }
}

fn preview(name: &str, pixels: &[u8]) {
    if regia_core::brand::env_var_os("REGIA_PREVIEW").is_none() {
        return;
    }
    let dir = std::path::Path::new("/tmp/specular-aa-preview");
    std::fs::create_dir_all(dir).expect("preview dir");
    let file = std::fs::File::create(dir.join(format!("{name}.png"))).expect("preview file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), WIDTH, HEIGHT);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(pixels).expect("png pixels");
    writer.finish().expect("png finish");
}

#[test]
fn specular_aa_cuts_shimmer_on_a_panning_normal_mapped_plane() {
    let device = RenderDevice::headless().expect("headless device");
    let before = run(&device, false);
    let after = run(&device, true);
    for (name, r) in [("before", &before), ("after", &after)] {
        eprintln!(
            "specular-aa {name}: shimmer {:.2} (luma² per frame), mean luma {:.1}, drop {:.1} %",
            r.shimmer,
            r.mean_luma,
            100.0 * (1.0 - r.shimmer / before.shimmer)
        );
    }
    let still = luma_ssim(&luma(&before.first), &luma(&after.first));
    eprintln!("specular-aa still SSIM before vs after: {still:.4}");
    preview("before", &before.first);
    preview("after", &after.first);

    // The frame must show a highlight, not black or a blown-out plate.
    assert!(
        (4.0..200.0).contains(&before.mean_luma),
        "before mean luma {:.1}",
        before.mean_luma
    );
    assert!(
        before.shimmer > 1.0,
        "the baseline must shimmer: {:.3}",
        before.shimmer
    );
    assert!(
        after.shimmer <= 0.6 * before.shimmer,
        "shimmer {:.2} → {:.2} is a {:.1} % drop; acceptance is ≥ 40 %",
        before.shimmer,
        after.shimmer,
        100.0 * (1.0 - after.shimmer / before.shimmer)
    );
}
