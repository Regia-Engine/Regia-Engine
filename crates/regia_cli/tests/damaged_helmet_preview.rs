// SPDX-License-Identifier: MPL-2.0
//! Writes `target/evidence/m4/damaged-helmet-preview.png` (never `docs/`) when
//! `REGIA_WRITE_EVIDENCE=1` and the Khronos cache is present — through the same
//! frame path games run.

use std::path::PathBuf;
use std::sync::Arc;

use image::ImageEncoder;
use regia_asset_pipeline::{CookOptions, cook_project, host_platform};
use regia_render::{
    CpuMaterial, CpuMesh as RenderMesh, DirectionalLightUniform, FrameMesh, FrameRenderer, FrameScene,
    LabVertex, RenderDevice,
};

#[test]
#[ignore = "needs a GPU, REGIA_WRITE_EVIDENCE=1 and the Khronos cache; run with --ignored"]
fn write_damaged_helmet_preview_png() {
    if regia_core::brand::env_var_os("REGIA_WRITE_EVIDENCE").as_deref() != Some(std::ffi::OsStr::new("1")) {
        eprintln!("skip preview evidence: set REGIA_WRITE_EVIDENCE=1");
        return;
    }
    let cache = match regia_core::brand::env_var("REGIA_KHRONOS_ASSETS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("skip preview evidence: set REGIA_KHRONOS_ASSETS");
            return;
        }
    };
    let helmet = cache.join("DamagedHelmet.glb");
    if !helmet.is_file() {
        eprintln!("skip preview evidence: run cargo xtask fetch-test-assets");
        return;
    }
    let root = std::env::temp_dir().join(format!("regia_preview_{}", uuid::Uuid::new_v4()));
    let assets = root.join("assets").join("khronos");
    std::fs::create_dir_all(&assets).expect("assets");
    std::fs::copy(&helmet, assets.join("DamagedHelmet.glb")).expect("copy");
    cook_project(CookOptions::for_host(root.clone())).expect("cook");

    let platform = host_platform();
    let mut server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
        project_root: root,
        platform: platform.to_string(),
        content_dir: None,
    })
    .expect("server");
    let handle = server
        .load_mesh_path("assets/khronos/DamagedHelmet.glb")
        .expect("load mesh");
    let mesh = server.meshes().get(&handle).cloned().expect("mesh");

    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(_) => {
            eprintln!("skip preview evidence: no GPU");
            return;
        }
    };

    let cpu = Arc::new(RenderMesh {
        vertices: mesh
            .positions
            .iter()
            .zip(mesh.normals.iter())
            .zip(mesh.uvs.iter())
            .enumerate()
            .map(|(index, ((position, normal), uv))| LabVertex {
                position: *position,
                normal: *normal,
                uv: *uv,
                tangent: mesh.tangents.get(index).copied().unwrap_or([1.0, 0.0, 0.0, 1.0]),
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            })
            .collect(),
        indices: mesh.indices.clone(),
    });
    let material = Arc::new(CpuMaterial::default());

    // One frame of the runtime shape: a persistent renderer and mesh + light
    // data exactly as the ECS path extracts them.
    let mut renderer = FrameRenderer::new();
    let scene = FrameScene {
        width: 512,
        height: 512,
        eye: [0.0, 0.2, 2.5],
        target: [0.0, 0.0, 0.0],
        fov_y: std::f32::consts::FRAC_PI_4,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([-0.577, -0.577, -0.577], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: &[],
        quality: Default::default(),
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: Arc::as_ptr(&cpu) as u64,
            material_key: Arc::as_ptr(&material) as u64,
            object_key: 1,
            vertices: &cpu.vertices,
            indices: &cpu.indices,
            material: &material,
            model: regia_core::math::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    };
    let result = renderer.render_offscreen(&device, &scene).expect("render");

    let out =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/evidence/m4/damaged-helmet-preview.png");
    std::fs::create_dir_all(out.parent().expect("parent")).expect("mkdir evidence");
    write_png(&out, result.width, result.height, &result.pixels).expect("write png");
    assert!(out.is_file());
    // A blank (all one value) frame means the render path produced nothing.
    let luma = |px: &[u8; 4]| (u32::from(px[0]) + u32::from(px[1]) + u32::from(px[2])) / 3;
    let (min, max) = result
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(luma)
        .fold((u32::MAX, 0), |(lo, hi), l| (lo.min(l), hi.max(l)));
    assert!(max > min, "preview is a single flat color (luma {min})");
}

fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let enc = image::codecs::png::PngEncoder::new(std::io::BufWriter::new(file));
    enc.write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| std::io::Error::other(e.to_string()))
}
