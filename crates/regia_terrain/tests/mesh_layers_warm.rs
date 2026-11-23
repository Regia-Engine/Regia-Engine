// SPDX-License-Identifier: MPL-2.0
//! Plan row W-16, amendment A2 (DO27 handover finding 8, the 252 s terrain pipeline stall): the
//! mesh-layer chunks join the warm. After the terrain warm job and the frame warm, chunks that
//! stream in and out (appearing, changing LOD, leaving) create no pipeline on the thread that
//! draws them. Its own test binary: the pipeline counter is per process. Lavapipe is enough.

use std::sync::Arc;

use regia_asset::AssetId;
use regia_render::pipeline_cache::pipelines_created;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMesh, LabScene, LabVertex, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, CookedChunkLod, CookedLayer, CookedLayerChunk, LayerChunkGrid, TerrainLayerStream,
    TerrainLayers, TerrainMap, TerrainMeshLayersCooked, TerrainSharedDraw, warm_terrain_pipelines,
};

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    }
}

fn map(n: u32, world: f32) -> TerrainMap {
    let heights = (0..n * n)
        .map(|i| 2.0 * ((i % n) as f32 * 0.19).sin() * ((i / n) as f32 * 0.13).cos())
        .collect();
    TerrainMap::from_heights(n, n, heights, world, 20.0, -10.0).expect("map")
}

/// A box of `size` metres at `at`, `subdiv`-dependent so each LOD is a different mesh.
fn chunk_box(at: [f32; 2], size: f32, lod: u8) -> regia_render::CpuMesh {
    let h = size * 0.5;
    let y = 1.0 + f32::from(lod);
    let c = [
        [at[0] - h, 0.0, at[1] - h],
        [at[0] + h, 0.0, at[1] - h],
        [at[0] + h, y, at[1] - h],
        [at[0] - h, y, at[1] - h],
        [at[0] - h, 0.0, at[1] + h],
        [at[0] + h, 0.0, at[1] + h],
        [at[0] + h, y, at[1] + h],
        [at[0] - h, y, at[1] + h],
    ];
    let vertices = c
        .iter()
        .map(|p| LabVertex::from_pos_normal_uv(*p, [0.0, 1.0, 0.0], [p[0] * 0.25, p[2] * 0.25]))
        .collect();
    let indices = vec![
        0, 2, 1, 0, 3, 2, 4, 5, 6, 4, 6, 7, 0, 1, 5, 0, 5, 4, 3, 7, 6, 3, 6, 2, 0, 4, 7, 0, 7, 3, 1, 2, 6, 1,
        6, 5,
    ];
    regia_render::CpuMesh { vertices, indices }
}

/// One layer over a 4 x 4 chunk grid of a 256 m map (64 m chunks), three LODs each.
fn layers() -> TerrainMeshLayersCooked {
    let config = CdlodConfig::default();
    let grid = LayerChunkGrid::new(256.0, 2, config.grid_size, config.range_factor, [0.0, 0.0], 64.0);
    let id = |x: u32, z: u32, lod: u8| AssetId::sub_asset(AssetId::default(), &format!("warm/{x}_{z}/{lod}"));
    TerrainMeshLayersCooked {
        version: regia_terrain::mesh_layer::PRESENTATION_VERSION,
        chunk_importer: 0,
        grid,
        layers: vec![CookedLayer {
            id: "rock".into(),
            map_layer: 0,
            cast_shadows: true,
            collider: false,
            material: String::new(),
            material_id: None,
            chunks: (0..4)
                .flat_map(|z| (0..4).map(move |x| (x, z)))
                .map(|(x, z)| CookedLayerChunk {
                    x,
                    z,
                    bounds: [[0.0; 3], [1.0; 3]],
                    lods: (0..3u8)
                        .map(|lod| CookedChunkLod {
                            mesh: id(x, z, lod),
                            triangles: 12,
                            bytes: 1000,
                            error_m: 0.0,
                        })
                        .collect(),
                })
                .collect(),
        }],
    }
}

fn frame(renderer: &mut FrameRenderer, device: &RenderDevice, meshes: Vec<LabMesh>, warm: bool) {
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: 160,
        height: 90,
        eye: [0.0, 60.0, -150.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.5,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.3, -0.8, 0.4], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![
            regia_terrain::mesh_layer::stream::layer_chunk_material()
                .as_ref()
                .clone(),
        ],
        meshes,
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut scene = scene.as_frame_scene_with_quality(quality);
    scene.post = Some(&post);
    if warm {
        renderer
            .warm_scene(device, &scene, wgpu::TextureFormat::Rgba8UnormSrgb, true)
            .expect("warm");
    } else {
        renderer.render_offscreen(device, &scene).expect("frame");
    }
}

fn lab(mesh: &regia_render::CpuMesh) -> LabMesh {
    LabMesh {
        vertices: mesh.vertices.clone(),
        indices: mesh.indices.clone(),
        ..LabMesh::unskinned()
    }
}

/// A2 fail-first (`no_pipeline_created_on_update_after_warm`): a counter of pipeline creations
/// stays 0 after the warm while layer chunks stream in and out.
#[test]
fn no_pipeline_created_on_update_after_warm() {
    let device = device();
    // The terrain warm job's sets (the v0.5.1 warm queue runs this on a worker).
    let built = warm_terrain_pipelines(&device, &[wgpu::TextureFormat::Rgba16Float]).expect("terrain warm");
    assert!(built > 0);
    let draw = TerrainSharedDraw::new(
        &device,
        map(65, 256.0),
        &TerrainLayers::default(),
        CdlodConfig::default(),
        2,
    )
    .expect("terrain");
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    // The frame warm, with one layer chunk in the warm scene: the layer pipelines join the warm.
    let warm_chunk = lab(&chunk_box([0.0, 0.0], 8.0, 0));
    frame(&mut renderer, &device, vec![warm_chunk], true);
    frame(&mut renderer, &device, Vec::new(), false);

    let cooked = layers();
    let mut stream = TerrainLayerStream::new(70.0, 4 * 1000);
    let mut fetch = |id: AssetId| -> Result<Arc<regia_render::CpuMesh>, String> {
        let c = cooked.layers[0]
            .chunks
            .iter()
            .find_map(|c| c.lods.iter().position(|l| l.mesh == id).map(|lod| (c, lod as u8)))
            .ok_or("unknown chunk")?;
        let (mx, mz, size) = cooked.grid.chunk_rect(c.0.x, c.0.z);
        Ok(Arc::new(chunk_box(
            [mx + size * 0.5, mz + size * 0.5],
            size * 0.5,
            c.1,
        )))
    };
    let before = pipelines_created();
    let mut drawn_counts = Vec::new();
    for step in 0..12 {
        // Across the map and past its edge: chunks appear, change LOD and leave.
        let t = step as f32 / 11.0;
        let p = [-200.0 + 400.0 * t, -150.0 + 300.0 * t];
        let draws = stream.step(&cooked, &[p], &mut fetch);
        let meshes: Vec<LabMesh> = draws
            .iter()
            .filter_map(|d| d.draw.as_ref().map(|(_, m)| lab(m)))
            .collect();
        drawn_counts.push(meshes.len());
        frame(&mut renderer, &device, meshes, false);
    }
    let created = pipelines_created() - before;
    println!("chunks drawn per step {drawn_counts:?}; pipelines created after the warm: {created}");
    assert!(drawn_counts.contains(&0), "chunks left the view at some step");
    assert!(drawn_counts.iter().any(|&n| n > 1), "chunks streamed in");
    assert_eq!(created, 0);
}
