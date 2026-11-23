// SPDX-License-Identifier: MPL-2.0
//! The visibility-buffer differential on an outdoor frame (ADR-2689, the outdoor row of
//! `regia_test_utils/tests/visbuffer_differential.rs`): rolling terrain, the physical sky, height
//! fog, and rocks scattered over the ground near and far.
//!
//! The rocks are rigid opaque meshes, so they take the visibility path; the terrain draws through
//! its shared pass and the sky and fog are the outdoor passes, which read the depth the path wrote.
//! The same frame through the classic forward path and through the visibility path (one cluster
//! level per mesh, so the rasterizers differ and the LOD cut does not) must agree within the
//! differential's tolerance. No golden image is read or written.
//!
//! The masked row (ADR-3915) adds leaf cards among the rocks, on a texture-table device: they take
//! the path through the alpha-tested raster and the frame must still agree, with terrain, sky and
//! fog seen through their holes.

use std::sync::{Arc, OnceLock};

use regia_render::visbuffer::{MeshletBuilder, MeshletMesh};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabTexture,
    LabVertex, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
    VisibilityPath, VolumetricFogSettings,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};
use regia_test_utils::golden::ssim_rgba8;

const W: u32 = 192;
const H: u32 = 108;
/// Heightfield samples per side.
const N: usize = 65;
/// World size of the heightfield, metres.
const WORLD: f32 = 96.0;

/// The level request is process-wide: only one test at a time may create a device.
static DEVICE_CREATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> Option<&'static RenderDevice> {
    static DEVICE: OnceLock<Option<RenderDevice>> = OnceLock::new();
    DEVICE
        .get_or_init(|| {
            let _guard = DEVICE_CREATE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match RenderDevice::headless() {
                Ok(device) => Some(device),
                Err(err) => {
                    eprintln!("skip: no headless GPU device: {err}");
                    None
                }
            }
        })
        .as_ref()
}

/// Gentle hills: the ground's height at `(x, z)`.
/// A device that carries the texture table, which masked clusters read their alpha through.
fn table_device() -> Option<RenderDevice> {
    let device = {
        let _guard = DEVICE_CREATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        regia_render::texture_table::set_device_level_request(
            regia_render::texture_table::TEXTURE_TABLE_LEVEL,
        );
        let device = RenderDevice::headless();
        regia_render::texture_table::set_device_level_request(0);
        device
    };
    match device {
        Ok(device) => regia_render::texture_table::device_support(&device.device)
            .capacity()
            .is_some()
            .then_some(device),
        Err(err) => {
            eprintln!("skip: no headless GPU device: {err}");
            None
        }
    }
}

fn height(x: f32, z: f32) -> f32 {
    1.6 * (x * 0.11).sin() * (z * 0.07).cos() + 0.8 * (x * 0.23 + z * 0.17).sin()
}

fn single_level_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        Some(MeshletMesh::single_level(&positions, indices))
    })
}

fn terrain(device: &RenderDevice) -> TerrainSharedDraw {
    let heights: Vec<f32> = (0..N * N)
        .map(|i| {
            let x = ((i % N) as f32 / (N - 1) as f32 - 0.5) * WORLD;
            let z = ((i / N) as f32 / (N - 1) as f32 - 0.5) * WORLD;
            height(x, z)
        })
        .collect();
    let map = TerrainMap::from_heights(N as u32, N as u32, heights, WORLD, 40.0, -20.0).expect("heightfield");
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 0.9,
            tiling: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            blend_sharpness: 0.15,
            triplanar: false,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let mut draw = TerrainSharedDraw::new(device, map, &stack, CdlodConfig::default(), 3).expect("terrain");
    let g = regia_terrain::linear_to_srgb8(0.22);
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: Rgba8Image::solid(64, 64, [g, regia_terrain::linear_to_srgb8(0.19), g, 255]),
                normal: Rgba8Image::neutral_normal(64),
                height: Rgba8Image::neutral_height(64),
                orm: Rgba8Image::solid(64, 64, [255, 230, 0, 255]),
                use_orm: false,
            })],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("terrain material");
    draw.set_relief(TerrainRelief::off());
    draw
}

/// Leaf cards in the masked row.
const CARDS: u32 = 12;

/// A leaf: an ellipse across the card, alpha 255 inside and 0 outside.
fn leaf_texture() -> LabTexture {
    let size = 64u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let u = (x as f32 + 0.5) / size as f32 * 2.0 - 1.0;
            let v = (y as f32 + 0.5) / size as f32 * 2.0 - 1.0;
            let a = if u * u / 0.5 + v * v / 0.95 < 1.0 { 255 } else { 0 };
            rgba.extend_from_slice(&[60, 140 + (x % 5) as u8 * 8, 50, a]);
        }
    }
    LabTexture {
        width: size,
        height: size,
        rgba,
    }
}

/// A 1.2 × 1.8 m card of 4 × 4 quads, standing on `y = 0`, facing +Z.
fn card() -> (Vec<LabVertex>, Vec<u32>) {
    let n = 4u32;
    let mut vertices = Vec::new();
    for j in 0..=n {
        for i in 0..=n {
            let (u, v) = (i as f32 / n as f32, j as f32 / n as f32);
            vertices.push(LabVertex::from_pos_normal_uv(
                [(u - 0.5) * 1.2, v * 1.8, 0.0],
                [0.0, 0.0, 1.0],
                [u, 1.0 - v],
            ));
        }
    }
    let mut indices = Vec::new();
    for j in 0..n {
        for i in 0..n {
            let a = j * (n + 1) + i;
            let b = a + n + 1;
            indices.extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
        }
    }
    (vertices, indices)
}

/// Rocks seated on the hills from about 10 m to 40 m from the eye, on two materials; with `masked`,
/// leaf cards between the eye and the first rocks.
fn scene(post: &PostSettings, masked: bool) -> LabScene {
    let (vertices, indices) = regia_render::uv_sphere(1.0, 16, 8);
    let mut meshes = Vec::new();
    for i in 0..24u32 {
        let row = (i / 6) as f32;
        let x = (i % 6) as f32 * 4.5 - 11.0 + row * 1.3;
        let z = -6.0 - row * 10.0;
        let s = 0.6 + 0.25 * (i % 3) as f32;
        let mut model = [[0.0f32; 4]; 4];
        model[0][0] = s * 1.3;
        model[1][1] = s * 0.7;
        model[2][2] = s;
        model[3] = [x, height(x, z) + s * 0.4, z, 1.0];
        meshes.push(LabMesh {
            vertices: vertices.clone(),
            indices: indices.clone(),
            material: (i % 2) as usize,
            model,
            ..LabMesh::unskinned()
        });
    }
    let mut materials = vec![
        LabMaterial {
            base_color: [0.42, 0.38, 0.33, 1.0],
            roughness: 0.85,
            ..LabMaterial::default()
        },
        LabMaterial {
            base_color: [0.3, 0.32, 0.35, 1.0],
            roughness: 0.6,
            ..LabMaterial::default()
        },
    ];
    if masked {
        materials.push(LabMaterial {
            albedo: Some(leaf_texture()),
            alpha_mode: regia_render::ALPHA_MASK,
            alpha_cutoff: 0.5,
            double_sided: true,
            ..LabMaterial::default()
        });
        let (vertices, indices) = card();
        for i in 0..CARDS {
            let (x, z) = ((i % 6) as f32 * 2.1 - 5.3, -1.0 - (i / 6) as f32 * 3.0);
            let mut model = [[0.0f32; 4]; 4];
            for (axis, row) in model.iter_mut().enumerate() {
                row[axis] = 1.0;
            }
            model[3] = [x, height(x, z), z, 1.0];
            meshes.push(LabMesh {
                vertices: vertices.clone(),
                indices: indices.clone(),
                material: 2,
                model,
                ..LabMesh::unskinned()
            });
        }
    }
    LabScene {
        width: W,
        height: H,
        eye: [0.0, height(0.0, 4.0) + 3.0, 4.0],
        target: [0.0, height(0.0, -20.0) + 1.0, -20.0],
        fov_y: 0.9,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: post.exposure_ev,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.5, -0.45, -0.4], [1.0, 0.95, 0.88], 90_000.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

/// The twelfth frame (the probes and the fog history settle the same way on both paths) and the
/// renderer.
fn frame(device: &RenderDevice, vis: bool, masked: bool) -> (Vec<u8>, FrameRenderer) {
    let post = PostSettings {
        exposure_ev: 14.5,
        ..PostSettings::golden_deterministic()
    };
    let lab = scene(&post, masked);
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.msaa_samples = 1;
    quality.visibility_buffer = vis;
    quality.visibility_path = VisibilityPath::DEFAULT;
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    if device.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        // As in the other differentials: llvmpipe's anisotropic taps need the quad derivatives a
        // full-screen resolve cannot give it.
        renderer.set_material_anisotropy_for_tests(1);
    }
    if vis {
        renderer.set_meshlet_builder(Some(single_level_builder()));
    }
    renderer.set_outdoor(OutdoorParams {
        fog: VolumetricFogSettings {
            density: 0.012,
            height_falloff: 0.08,
            base_height: 0.0,
            ..VolumetricFogSettings::default()
        },
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(
        Box::new(terrain(device)) as Box<dyn SharedPassDraw + Send + Sync>
    ));
    for _ in 0..11 {
        renderer.render_offscreen(device, &frame).expect("settling frame");
    }
    let pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    if let Ok(dir) = regia_core::brand::env_var("REGIA_VIS_DIFF_DIR") {
        let path = std::path::Path::new(&dir).join(format!(
            "outdoor{}_{}.png",
            if masked { "_masked" } else { "" },
            if vis { "vis" } else { "classic" }
        ));
        if let Ok(file) = std::fs::File::create(&path) {
            let mut encoder = png::Encoder::new(file, W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = encoder.write_header() {
                let _ = writer.write_image_data(&pixels);
            }
        }
    }
    (pixels, renderer)
}

/// (largest channel difference, fraction of pixels whose channels all differ by at most 2).
fn diff(a: &[u8], b: &[u8]) -> (u8, f64) {
    let mut worst = 0u8;
    let mut close = 0usize;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        worst = worst.max(d);
        close += usize::from(d <= 2);
    }
    (worst, close as f64 / (a.len() / 4).max(1) as f64)
}

/// The differential's floors (`visbuffer_differential.rs` `STRICT`), lowered on Metal as there.
const CLOSE: f64 = 0.985;
const SSIM: f64 = 0.985;

#[test]
fn the_visibility_path_agrees_with_the_classic_path_outdoors() {
    let Some(device) = device() else {
        return;
    };
    let metal = device.capabilities.backend == wgpu::Backend::Metal;
    let floor = |value: f64| if metal { value.min(0.97) } else { value };
    let (classic, _) = frame(device, false, false);
    let (vis, renderer) = frame(device, true, false);
    let stats = renderer.visbuffer_stats();
    assert!(renderer.visbuffer_active(), "the frame took the path");
    assert_eq!(
        stats.instances, 24,
        "every rock is a visibility instance: {stats:?}"
    );
    // A frame the comparison can mean something on: sky, ground and rocks, not one flat colour.
    let texels = classic.as_chunks::<4>().0;
    let clipped = texels.iter().filter(|p| p[..3].iter().any(|c| *c >= 250)).count();
    assert!(
        clipped * 5 < texels.len(),
        "{clipped} of {} pixels clipped",
        texels.len()
    );
    let range = (0..3)
        .map(|c| {
            let (lo, hi) = texels
                .iter()
                .fold((255u8, 0u8), |(lo, hi), p| (lo.min(p[c]), hi.max(p[c])));
            hi - lo
        })
        .max()
        .unwrap_or(0);
    assert!(range >= 32, "the frame is nearly flat: range {range}");
    let (worst, close) = diff(&classic, &vis);
    let ssim = ssim_rgba8(&classic, &vis, W, H).expect("ssim");
    println!(
        "outdoor: {:.3} % within 2 levels (worst {worst}), ssim {ssim:.4}, {} instances",
        close * 100.0,
        stats.instances
    );
    assert!(
        close >= floor(CLOSE) && ssim >= floor(SSIM),
        "outdoor: {:.3} % close (floor {:.1} %), ssim {ssim:.4} (floor {:.3})",
        close * 100.0,
        floor(CLOSE) * 100.0,
        floor(SSIM)
    );
}

#[test]
fn masked_cards_on_the_visibility_path_agree_with_the_classic_path_outdoors() {
    let Some(device) = table_device() else {
        eprintln!("skip: the adapter cannot carry the texture table");
        return;
    };
    let metal = device.capabilities.backend == wgpu::Backend::Metal;
    let floor = |value: f64| if metal { value.min(0.97) } else { value };
    let (classic, _) = frame(&device, false, true);
    let (vis, renderer) = frame(&device, true, true);
    let stats = renderer.visbuffer_stats();
    assert!(renderer.visbuffer_active(), "the frame took the path");
    assert_eq!(stats.masked_instances, CARDS, "every card is masked: {stats:?}");
    assert_eq!(
        stats.instances,
        24 + CARDS,
        "cards and rocks are on the path: {stats:?}"
    );
    assert!(stats.masked_tested, "the cards were alpha-tested");
    let (worst, close) = diff(&classic, &vis);
    let ssim = ssim_rgba8(&classic, &vis, W, H).expect("ssim");
    println!(
        "outdoor masked: {:.3} % within 2 levels (worst {worst}), ssim {ssim:.4}, {} instances, {} masked",
        close * 100.0,
        stats.instances,
        stats.masked_instances
    );
    assert!(
        close >= floor(CLOSE) && ssim >= floor(SSIM),
        "outdoor masked: {:.3} % close (floor {:.1} %), ssim {ssim:.4} (floor {:.3})",
        close * 100.0,
        floor(CLOSE) * 100.0,
        floor(SSIM)
    );
}
