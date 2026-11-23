// SPDX-License-Identifier: MPL-2.0
//! Shared fixture of the host memory budget tests (ADR-7891): a pak of distinct GPU-ready static
//! meshes, mounted through an asset server, and a lab scene that draws them in a grid.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use regia_asset::{
    AssetId, AssetManifest, AssetServer, AssetTypeTag, GpuMeshInfo, GpuMeshLod, ManifestEntry,
};
use regia_asset_pipeline::pak_writer::{PakWriteOptions, write_pak};
use regia_render::{DirectionalLightUniform, LabMaterial, LabScene, LabVertex};

/// Bytes per MiB.
pub const MIB: u64 = 1024 * 1024;

/// Bytes of one [`LabVertex`] (the GPU-ready layout's stride).
pub const VERTEX_BYTES: u64 = size_of::<LabVertex>() as u64;

/// Tiles per grid row.
pub const GRID: u32 = 8;

/// The vertices of mesh `index` of `count`: a quad tile in an 8-wide grid on the z = 0 plane,
/// then `vertices - 4` more vertices at the tile's corners (inside its bound, never indexed),
/// each with its own UV so every mesh's bytes are distinct.
pub fn tile_vertices(index: u32, vertices: usize) -> Vec<LabVertex> {
    let col = (index % GRID) as f32;
    let row = (index / GRID) as f32;
    let size = 2.0 / GRID as f32;
    let x0 = -1.0 + col * size + 0.1 * size;
    let y0 = 1.0 - (row + 1.0) * size + 0.1 * size;
    let s = 0.8 * size;
    let corners = [
        [x0, y0, 0.0],
        [x0 + s, y0, 0.0],
        [x0 + s, y0 + s, 0.0],
        [x0, y0 + s, 0.0],
    ];
    let n = [0.0, 0.0, 1.0];
    let mut out = Vec::with_capacity(vertices);
    for (i, corner) in corners.iter().enumerate() {
        out.push(LabVertex::from_pos_normal_uv(
            *corner,
            n,
            [(i & 1) as f32, (i >> 1) as f32],
        ));
    }
    for i in 4..vertices {
        let mut v = LabVertex::from_pos_normal_uv(corners[i % 4], n, [0.0, 0.0]);
        v.uv = [(i as f32) * 1e-6, index as f32];
        out.push(v);
    }
    out
}

/// The two triangles of a tile.
pub const TILE_INDICES: [u32; 6] = [0, 1, 2, 0, 2, 3];

/// A pak in `dir/content` with `count` GPU-ready meshes of `vertices` vertices each, stored
/// uncompressed (so the mounted pak serves views of its mapping). Returns the project root and
/// the mesh ids in tile order.
pub fn write_mesh_pak(dir: &Path, count: u32, vertices: usize) -> (PathBuf, Vec<AssetId>) {
    let cooked = dir.join("cooked");
    let content = dir.join("content");
    std::fs::create_dir_all(&cooked).expect("cooked dir");
    std::fs::create_dir_all(&content).expect("content dir");
    let mut ids = Vec::new();
    let mut entries = Vec::new();
    let mut manifest = Vec::new();
    for index in 0..count {
        let id = AssetId::new_random();
        let verts = tile_vertices(index, vertices);
        let info = GpuMeshInfo {
            aabb_min: [-1.0, -1.0, 0.0],
            aabb_max: [1.0, 1.0, 0.0],
            bounding_sphere: [0.0, 0.0, 0.0, 1.5],
            lods: vec![GpuMeshLod {
                screen_relative_size: 0.0,
                first_index: 0,
                index_count: TILE_INDICES.len() as u32,
            }],
        };
        let file = regia_asset::encode_gpu_mesh_file(
            id,
            1,
            Vec::new(),
            &info,
            bytemuck::cast_slice(&verts),
            &TILE_INDICES,
        )
        .expect("encode mesh");
        drop(verts);
        std::fs::write(cooked.join(regia_asset::cooked_file_name(id)), file).expect("write cooked");
        entries.push((id, AssetTypeTag::gpu_mesh().0));
        manifest.push(ManifestEntry {
            path: format!("assets/host_budget/tile_{index}.glb"),
            id,
            type_path: regia_asset::GPU_MESH_TYPE_PATH.to_string(),
        });
        ids.push(id);
    }
    let manifest = AssetManifest { entries: manifest };
    write_pak(
        &content.join("chunk_base.fpak"),
        &cooked,
        &entries,
        &manifest,
        &PakWriteOptions {
            compress: false,
            ..PakWriteOptions::default()
        },
    )
    .expect("write pak");
    // The cooked files are not needed once packed (the server reads the pak).
    let _ = std::fs::remove_dir_all(&cooked);
    (dir.to_path_buf(), ids)
}

/// An asset server for `root` that mounts `root/content`.
pub fn open_server(root: &Path) -> AssetServer {
    AssetServer::open(regia_asset::AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: regia_asset::current_platform(),
        content_dir: Some(root.join("content")),
    })
    .expect("open server")
}

/// A lab scene that looks at the grid (no meshes: the tests add the store's).
pub fn grid_scene() -> LabScene {
    LabScene {
        width: 64,
        height: 64,
        eye: [0.0, 0.0, 2.6],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 10.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: Vec::new(),
    }
}

/// Column-major identity.
pub const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];
