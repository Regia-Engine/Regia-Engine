// SPDX-License-Identifier: MPL-2.0
//! Compressed graph and detail textures against the RGBA8 path they replace.
//!
//! A graph slot cooked with [`cook_graph_texture`] draws within BC7's error of the same
//! RGBA8 slot. Packed detail (linear BC7) does the same. Eight materials that clone one
//! image upload one GPU texture; eight private copies upload eight. A 2048² RGBA8 mip
//! chain is about 22.4 MiB and the BC7 chain about 5.6 MiB; this file measures 64² so
//! the run stays small.

use std::sync::Arc;

use regia_asset::TextureCookUsage;
use regia_asset_pipeline::{TextureCookQuality, cook_graph_texture, cook_surface_maps};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, GraphTexture,
    HardSurfaceFactors, LabMaterial, LabTexture, LabVertex, SurfaceGraph,
};

const SURFACE: &str = r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    let tint = material.graph[1].xyz;
    let albedo = textureSample(albedo_tex, tex_samp, in.uv);
    return regia::pbr::surface::Surface(
        albedo.rgb * tint * in.vertex_color.rgb,
        albedo.a,
        0.0,
        1.0,
        in.world_normal,
        1.0,
        vec3<f32>(0.0),
        0.0,
    );
}
";

fn quad() -> Vec<LabVertex> {
    [[-2.0f32, -2.0], [2.0, -2.0], [2.0, 2.0], [-2.0, 2.0]]
        .into_iter()
        .map(|[x, z]| LabVertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [(x + 2.0) / 4.0, (z + 2.0) / 4.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

fn ramp(size: u32, scale: u32, bias: u8) -> LabTexture {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let checker = if ((x / 16) + (y / 16)) % 2 == 0 { 20 } else { 0 };
            rgba.extend_from_slice(&[
                ((x * scale) / size) as u8 + checker,
                ((y * scale) / size) as u8 + bias,
                48,
                255,
            ]);
        }
    }
    LabTexture {
        width: size,
        height: size,
        rgba,
    }
}

fn render(
    renderer: &mut FrameRenderer,
    device: &regia_render::RenderDevice,
    materials: &[LabMaterial],
) -> Vec<u8> {
    let vertices = quad();
    let indices = [0u32, 2, 1, 0, 3, 2];
    let meshes = materials
        .iter()
        .enumerate()
        .map(|(i, material)| FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: (i as u64) + 1,
            object_key: (i as u64) + 1,
            vertices: &vertices,
            indices: &indices,
            material,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        })
        .collect();
    let scene = FrameScene {
        width: 64,
        height: 64,
        eye: [0.0, 3.0, 0.01],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.1,
        near: 0.05,
        far: 50.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 1.0),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes,
    };
    renderer.render_offscreen(device, &scene).expect("frame").pixels
}

fn device() -> regia_render::RenderDevice {
    match regia_terrain::gpu::headless_bcn_device() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device with BC (lavapipe?): {err}"),
    }
}

fn within(a: &[u8], b: &[u8], what: &str) {
    let max = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
    let mean = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(x.abs_diff(*y)))
        .sum::<f64>()
        / a.len() as f64;
    assert!(
        max <= 24 && mean < 3.0,
        "{what} within the encoder's error: max {max}, mean {mean:.2}"
    );
}

fn mean_diff(a: &[u8], b: &[u8]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| f64::from(x.abs_diff(*y)))
        .sum::<f64>()
        / a.len() as f64
}

fn graph_material(texture: GraphTexture) -> LabMaterial {
    let mut graph = SurfaceGraph::new(SURFACE);
    graph.params[1] = [1.0, 1.0, 1.0, 1.0];
    graph.textures[0] = Some(texture);
    LabMaterial {
        unlit: true,
        base_color: [1.0, 1.0, 1.0, 1.0],
        surface_graph: Some(Arc::new(graph)),
        ..LabMaterial::default()
    }
}

#[test]
fn cooked_detail_is_linear_bc7_and_aux_is_bc5() {
    let detail = ramp(64, 80, 12);
    let height = ramp(64, 200, 0);
    let curvature = ramp(64, 100, 40);
    let maps = cook_surface_maps(
        Some(&detail),
        None,
        None,
        None,
        Some(&height),
        Some(&curvature),
        "linux",
        TextureCookQuality::High,
    )
    .expect("cook");
    let detail_map = maps.detail.as_ref().expect("detail");
    assert!(
        format!("{:?}", detail_map.format).contains("Bc7") && !detail_map.format.is_srgb(),
        "packed detail is linear BC7 ({:?})",
        detail_map.format
    );
    let aux = maps.aux.as_ref().expect("aux");
    assert!(
        format!("{:?}", aux.format).contains("Bc5"),
        "packed height and curvature are BC5 ({:?})",
        aux.format
    );
}

#[test]
fn a_compressed_graph_texture_matches_its_rgba8_source() {
    let device = device();
    let image = ramp(128, 180, 16);
    let cooked = cook_graph_texture(
        image.width,
        image.height,
        &image.rgba,
        TextureCookUsage::ColorSrgb,
        true,
        "linux",
        TextureCookQuality::High,
    )
    .expect("cook");
    let chain = cooked
        .shared
        .as_ref()
        .and_then(|image| image.compressed())
        .expect("chain");
    assert!(chain.format.is_srgb() && format!("{:?}", chain.format).starts_with("Bc7"));

    let rgba8 = graph_material(GraphTexture::rgba(image, true));
    let bc7 = graph_material(cooked);
    let plain = graph_material_empty();
    let a = render(&mut FrameRenderer::new(), &device, &[rgba8]);
    let mut bc7_renderer = FrameRenderer::new();
    let b = render(&mut bc7_renderer, &device, &[bc7]);
    within(&a, &b, "BC7 graph slot");
    let white = render(&mut FrameRenderer::new(), &device, &[plain]);
    let off = mean_diff(&b, &white);
    assert!(
        off > 10.0,
        "the compressed graph slot is sampled, not white ({off:.1})"
    );
}

fn graph_material_empty() -> LabMaterial {
    let mut graph = SurfaceGraph::new(SURFACE);
    graph.params[1] = [1.0, 1.0, 1.0, 1.0];
    LabMaterial {
        unlit: true,
        base_color: [1.0, 1.0, 1.0, 1.0],
        surface_graph: Some(Arc::new(graph)),
        ..LabMaterial::default()
    }
}

#[test]
fn a_compressed_detail_map_matches_its_rgba8_pack() {
    let device = device();
    let detail = ramp(64, 80, 12);
    let height = ramp(64, 200, 0);
    let curvature = ramp(64, 100, 40);
    let maps = cook_surface_maps(
        Some(&detail),
        None,
        None,
        None,
        Some(&height),
        Some(&curvature),
        "linux",
        TextureCookQuality::High,
    )
    .expect("cook");
    let factors = HardSurfaceFactors {
        detail_albedo_strength: 1.0,
        ..HardSurfaceFactors::OFF
    };
    let rgba8 = LabMaterial {
        unlit: true,
        base_color: [1.0, 1.0, 1.0, 1.0],
        detail_albedo: Some(detail),
        hard_surface: factors,
        ..LabMaterial::default()
    };
    let bc7 = LabMaterial {
        unlit: true,
        base_color: [1.0, 1.0, 1.0, 1.0],
        hard_surface: factors,
        compressed: Some(Arc::new(maps)),
        ..LabMaterial::default()
    };
    let bare = LabMaterial {
        unlit: true,
        base_color: [1.0, 1.0, 1.0, 1.0],
        hard_surface: factors,
        ..LabMaterial::default()
    };
    let a = render(&mut FrameRenderer::new(), &device, &[rgba8]);
    let mut bc7_renderer = FrameRenderer::new();
    let b = render(&mut bc7_renderer, &device, std::slice::from_ref(&bc7));
    within(&a, &b, "BC7 detail");
    let mid = render(&mut FrameRenderer::new(), &device, &[bare]);
    let off = mean_diff(&b, &mid);
    assert!(off > 10.0, "the compressed detail map is sampled ({off:.1})");
    let report = bc7_renderer.texture_memory();
    let detail_row = report
        .textures
        .iter()
        .find(|row| row.slot == "detail")
        .expect("detail row");
    assert!(detail_row.compressed, "detail is block-compressed");
    assert!(
        detail_row.format.contains("Bc7") && !detail_row.format.contains("Srgb"),
        "detail view is linear BC7 ({})",
        detail_row.format
    );
    let aux_row = report
        .textures
        .iter()
        .find(|row| row.slot == "aux")
        .expect("aux row");
    assert!(
        aux_row.compressed && aux_row.format.contains("Bc5"),
        "aux is BC5 ({})",
        aux_row.format
    );
}

#[test]
fn eight_shared_graph_images_upload_one_texture() {
    let device = device();
    let image = ramp(64, 180, 16);
    let private: Vec<LabMaterial> = (0..8)
        .map(|_| graph_material(GraphTexture::rgba(image.clone(), true)))
        .collect();
    let mut private_renderer = FrameRenderer::new();
    let _ = render(&mut private_renderer, &device, &private);
    let private_report = private_renderer.texture_memory();
    let private_rows: Vec<_> = private_report
        .textures
        .iter()
        .filter(|row| row.slot == "graph0")
        .collect();
    assert_eq!(private_rows.len(), 8, "each private copy uploads its own texture");
    assert!(private_rows.iter().all(|row| !row.compressed));

    let shared =
        regia_render::SharedImage::rgba(image.width, image.height, image.rgba.clone(), true).expect("image");
    let shared_mats: Vec<LabMaterial> = (0..8)
        .map(|_| graph_material(GraphTexture::from_shared(shared.clone())))
        .collect();
    let mut shared_renderer = FrameRenderer::new();
    let _ = render(&mut shared_renderer, &device, &shared_mats);
    let shared_report = shared_renderer.texture_memory();
    let shared_rows: Vec<_> = shared_report
        .textures
        .iter()
        .filter(|row| row.slot == "graph0")
        .collect();
    assert_eq!(shared_rows.len(), 1, "one GPU texture for eight materials");
    assert!(!shared_rows[0].compressed);
    assert_eq!(shared_report.uncompressed, 1);

    let cooked = cook_graph_texture(
        image.width,
        image.height,
        &image.rgba,
        TextureCookUsage::ColorSrgb,
        true,
        "linux",
        TextureCookQuality::High,
    )
    .expect("cook");
    let cooked_mats: Vec<LabMaterial> = (0..8).map(|_| graph_material(cooked.clone())).collect();
    let mut cooked_renderer = FrameRenderer::new();
    let _ = render(&mut cooked_renderer, &device, &cooked_mats);
    let cooked_report = cooked_renderer.texture_memory();
    let cooked_rows: Vec<_> = cooked_report
        .textures
        .iter()
        .filter(|row| row.slot == "graph0")
        .collect();
    assert_eq!(cooked_rows.len(), 1, "one cooked GPU texture");
    assert!(cooked_rows[0].compressed);
    assert_eq!(cooked_report.uncompressed, 0);
    eprintln!(
        "sample scene 8x64² graph0: private count={} bytes={}; shared rgba8 count={} bytes={}; shared bc7 count={} bytes={} uncompressed={}",
        private_rows.len(),
        private_rows.iter().map(|row| row.gpu_bytes).sum::<u64>(),
        shared_rows.len(),
        shared_rows[0].gpu_bytes,
        cooked_rows.len(),
        cooked_rows[0].gpu_bytes,
        cooked_report.uncompressed_bytes,
    );
}
