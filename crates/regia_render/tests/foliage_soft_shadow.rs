// SPDX-License-Identifier: MPL-2.0
//! Soft leaf shadows (ADR-2449): `FrameRenderer::set_foliage_shadow_softness` on the headless
//! device (lavapipe in CI).
//!
//! A thin slab whose vertex alpha rises from 0 to 1 across its width casts a masked shadow with a
//! straight edge where the alpha crosses the cutoff. With the hard cutoff the edge is clean; with
//! a soft band it is written as a fixed per-texel pattern the shadow filter averages into a
//! penumbra. The tests compare frames (no golden image): the shadow edge changes, the mean
//! luminance stays about the same (the band is symmetric), the edge gains variation, the result
//! is identical between two runs, and a softness of zero equals the default frame byte for byte.
//!
//! Set `REGIA_SOFT_SHADOW_IMAGES` to a directory to also write the frames as PNGs.

use regia_core::math::{Mat4, Vec3};
use regia_render::{
    ALPHA_MASK, DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    IndirectStrategy, LabMaterial, LabVertex, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast,
    box_mesh,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const RES: usize = 512;

fn device() -> Option<RenderDevice> {
    let device = RenderDevice::headless().expect("headless device (lavapipe)");
    if !IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
        eprintln!("skip: the device draws no GPU objects");
        return None;
    }
    Some(device)
}

fn quality() -> FrameQuality {
    let base = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    FrameQuality {
        csm_cascade_count: 1,
        csm_resolution: RES as u32,
        paged_sun_shadows: false,
        taau: false,
        ..base
    }
}

fn frame_mesh<'g>(
    key: u64,
    geo: &'g (Vec<LabVertex>, Vec<u32>),
    material: &'g LabMaterial,
    at: Vec3,
) -> FrameMesh<'g> {
    FrameMesh {
        is_static: false,
        mesh_key: key,
        material_key: key,
        object_key: key,
        vertices: geo.0.as_slice(),
        indices: geo.1.as_slice(),
        material,
        model: Mat4::from_translation(at).to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    }
}

struct Geometry {
    ground: (Vec<LabVertex>, Vec<u32>),
    leaf: (Vec<LabVertex>, Vec<u32>),
    ground_material: LabMaterial,
    leaf_material: LabMaterial,
}

impl Geometry {
    fn new() -> Self {
        let (mut vertices, indices) = box_mesh([2.0, 0.02, 2.0]);
        let (lo, hi) = vertices.iter().fold((f32::MAX, f32::MIN), |(lo, hi), v| {
            (lo.min(v.position[0]), hi.max(v.position[0]))
        });
        for v in &mut vertices {
            v.color[3] = (v.position[0] - lo) / (hi - lo);
        }
        Self {
            ground: box_mesh([8.0, 0.05, 8.0]),
            leaf: (vertices, indices),
            ground_material: LabMaterial {
                base_color: [0.6, 0.6, 0.55, 1.0],
                roughness: 0.8,
                ..LabMaterial::default()
            },
            leaf_material: LabMaterial {
                base_color: [0.2, 0.45, 0.15, 1.0],
                alpha_mode: ALPHA_MASK,
                alpha_cutoff: 0.5,
                double_sided: true,
                ..LabMaterial::default()
            },
        }
    }

    fn scene(&self) -> FrameScene<'_> {
        FrameScene {
            width: WIDTH,
            height: HEIGHT,
            eye: [0.0, 3.0, 6.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 40.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 0.0,
            ambient_lux: 0.5,
            double_sided: false,
            post: None,
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([-0.1, -1.0, -0.2], [1.0, 1.0, 1.0], 3.0),
            local_lights: &[],
            quality: quality(),
            meshes: vec![
                frame_mesh(1, &self.ground, &self.ground_material, Vec3::new(0.0, -0.05, 0.0)),
                frame_mesh(2, &self.leaf, &self.leaf_material, Vec3::new(0.0, 1.2, 0.0)),
            ],
        }
    }
}

struct Rendered {
    pixels: Vec<u8>,
}

fn render(device: &RenderDevice, geometry: &Geometry, softness: f32) -> Rendered {
    let mut renderer = FrameRenderer::new();
    renderer.set_alpha_tested_shadows(true);
    renderer.set_foliage_shadow_softness(softness);
    let scene = geometry.scene();
    // Two frames: the first builds pipelines and history; the second is the one measured.
    renderer.render_offscreen(device, &scene).expect("frame");
    let frame = renderer.render_offscreen(device, &scene).expect("frame");
    Rendered { pixels: frame.pixels }
}

fn save(dir: &str, name: &str, pixels: &[u8]) {
    let path = std::path::Path::new(dir);
    std::fs::create_dir_all(path).expect("dir");
    let file = std::fs::File::create(path.join(name)).expect("file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), WIDTH, HEIGHT);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .expect("header")
        .write_image_data(pixels)
        .expect("png");
}

/// Luminance of pixel `i` of an RGBA8 frame.
fn luma(pixels: &[u8], i: usize) -> f64 {
    0.2126 * f64::from(pixels[i * 4])
        + 0.7152 * f64::from(pixels[i * 4 + 1])
        + 0.0722 * f64::from(pixels[i * 4 + 2])
}

/// `(mean luminance, total horizontal variation)` of the ground rows below the leaf: the
/// variation is what a stippled shadow edge adds and a clean one does not.
fn measure(pixels: &[u8]) -> (f64, f64) {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let (mut sum, mut variation, mut count) = (0.0, 0.0, 0.0);
    for y in h / 2..h {
        for x in 0..w - 1 {
            let i = y * w + x;
            sum += luma(pixels, i);
            variation += (luma(pixels, i + 1) - luma(pixels, i)).abs();
            count += 1.0;
        }
    }
    (sum / count, variation)
}

#[test]
fn a_soft_band_turns_the_leaf_edge_into_a_filtered_pattern() {
    let Some(device) = device() else {
        return;
    };
    let geometry = Geometry::new();
    let hard = render(&device, &geometry, 0.0);
    let soft = render(&device, &geometry, 0.3);
    if let Ok(dir) = std::env::var("REGIA_SOFT_SHADOW_IMAGES") {
        save(&dir, "leaf_shadow_hard.png", &hard.pixels);
        save(&dir, "leaf_shadow_soft.png", &soft.pixels);
    }
    let differing = hard
        .pixels
        .chunks(4)
        .zip(soft.pixels.chunks(4))
        .filter(|(a, b)| a != b)
        .count();
    let (hard_mean, hard_variation) = measure(&hard.pixels);
    let (soft_mean, soft_variation) = measure(&soft.pixels);
    eprintln!(
        "differing pixels {differing}; mean {hard_mean:.3} -> {soft_mean:.3}; variation {hard_variation:.0} -> {soft_variation:.0}"
    );
    // The shadow edge changed.
    assert!(
        differing > 100,
        "only {differing} pixels changed: the softness did not reach the shadow"
    );
    // The band is symmetric about the cutoff, so the shadow's weight stays about the same.
    assert!(
        (soft_mean - hard_mean).abs() < 0.03 * hard_mean,
        "mean luminance moved {hard_mean} -> {soft_mean}"
    );
    // The pattern adds variation along the edge.
    assert!(
        soft_variation > hard_variation * 1.05,
        "variation {hard_variation} -> {soft_variation}: the edge shows no pattern"
    );
}

#[test]
fn the_soft_pattern_is_the_same_every_run_and_zero_is_the_hard_cutoff() {
    let Some(device) = device() else {
        return;
    };
    let geometry = Geometry::new();
    let a = render(&device, &geometry, 0.3);
    let b = render(&device, &geometry, 0.3);
    assert_eq!(a.pixels, b.pixels, "the soft shadow changes from run to run");
    // Zero is the default renderer's frame, byte for byte.
    let default = {
        let mut renderer = FrameRenderer::new();
        renderer.set_alpha_tested_shadows(true);
        let scene = geometry.scene();
        renderer.render_offscreen(&device, &scene).expect("frame");
        renderer.render_offscreen(&device, &scene).expect("frame").pixels
    };
    let zero = render(&device, &geometry, 0.0);
    assert_eq!(zero.pixels, default, "softness 0 changed the frame");
    // Turning it on and off again restores the hard frame.
    let mut renderer = FrameRenderer::new();
    renderer.set_alpha_tested_shadows(true);
    renderer.set_foliage_shadow_softness(0.3);
    let scene = geometry.scene();
    renderer.render_offscreen(&device, &scene).expect("frame");
    renderer.set_foliage_shadow_softness(0.0);
    renderer.render_offscreen(&device, &scene).expect("frame");
    let back = renderer.render_offscreen(&device, &scene).expect("frame");
    assert_eq!(
        back.pixels, default,
        "switching the softness off did not restore the hard shadow"
    );
}

#[test]
fn softness_is_clamped_and_non_finite_reads_as_zero() {
    let mut renderer = FrameRenderer::new();
    assert_eq!(renderer.foliage_shadow_softness(), 0.0);
    renderer.set_foliage_shadow_softness(0.25);
    assert_eq!(renderer.foliage_shadow_softness(), 0.25);
    renderer.set_foliage_shadow_softness(9.0);
    assert_eq!(
        renderer.foliage_shadow_softness(),
        FrameRenderer::MAX_FOLIAGE_SHADOW_SOFTNESS
    );
    renderer.set_foliage_shadow_softness(f32::NAN);
    assert_eq!(renderer.foliage_shadow_softness(), 0.0);
    renderer.set_foliage_shadow_softness(-1.0);
    assert_eq!(renderer.foliage_shadow_softness(), 0.0);
}
