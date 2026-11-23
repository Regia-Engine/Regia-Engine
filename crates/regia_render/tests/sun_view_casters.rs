// SPDX-License-Identifier: MPL-2.0
//! What reaches the occlusion tile aimed along the sun (`TranslucentLayer::sun_occlusion_request`,
//! ADR-3024), on the headless device (lavapipe in CI): a canopy drawn each way a forest can be
//! drawn. Forward draws, the GPU-driven instanced path and visibility-buffer clusters must all
//! shade the tile, on the High profile's paged shadow frame, or motes under trees would glint as
//! if in open sun.

use std::sync::Arc;

use regia_render::visbuffer::{MeshletBuilder, MeshletMesh};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, IndirectStrategy, LabMaterial, LabMesh, LabScene,
    LabVertex, LayerCamera, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    SunOcclusionRequest, TranslucentLayer, box_mesh,
};

/// The way the sun's light goes (about 65° up).
const SUN_TRAVEL: [f32; 3] = [0.3, -1.0, -0.4];
/// The crowns' centers' height (m).
const CROWN_Y: f32 = 12.0;
/// The height the tile is read at (m), where a mote floats.
const MOTE_Y: f32 = 1.5;
/// One mesh for all the crowns, so the instanced path batches them.
const CROWN_MESH: u64 = 0xC207_4E00;

/// Asks for the sun's tile and draws nothing: only the tile is under test.
struct WantsSunView;

impl TranslucentLayer for WantsSunView {
    fn simulate(&self, _: &wgpu::TextureView, _: &LayerCamera, _: &mut wgpu::CommandEncoder) {}

    fn draw(&self, _: &mut wgpu::RenderPass<'_>, _: wgpu::TextureFormat) {}

    fn sun_occlusion_request(&self) -> Option<SunOcclusionRequest> {
        Some(SunOcclusionRequest {
            texels: 512,
            extent_m: 64.0,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Path {
    Forward,
    Instanced,
    Clusters,
}

fn translate(at: [f32; 3]) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [at[0], at[1], at[2], 1.0],
    ]
}

/// The crowns' centers: a stand over x -14..-2, open sky east of it.
fn crowns() -> Vec<[f32; 3]> {
    let mut out = Vec::new();
    for x in 0..4 {
        for z in 0..8 {
            out.push([-14.0 + x as f32 * 4.0, CROWN_Y, -14.0 + z as f32 * 4.0]);
        }
    }
    out
}

fn scene(eye: [f32; 3]) -> LabScene {
    let (ground_v, ground_i) = box_mesh([80.0, 0.05, 80.0]);
    let (crown_v, crown_i) = box_mesh([1.8, 0.6, 1.8]);
    let mut meshes = vec![LabMesh {
        vertices: ground_v,
        indices: ground_i,
        material: 0,
        model: translate([0.0, -0.05, 0.0]),
        ..LabMesh::unskinned()
    }];
    meshes.extend(crowns().into_iter().map(|at| LabMesh {
        vertices: crown_v.clone(),
        indices: crown_i.clone(),
        material: 0,
        model: translate(at),
        ..LabMesh::unskinned()
    }));
    LabScene {
        width: 160,
        height: 96,
        eye,
        target: [eye[0], eye[1], eye[2] - 8.0],
        fov_y: 1.0,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(PostSettings::golden_deterministic()),
        directional: DirectionalLightUniform::new(SUN_TRAVEL, [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes,
    }
}

fn quality(path: Path) -> FrameQuality {
    FrameQuality {
        paged_sun_shadows: true,
        taau: false,
        msaa_samples: 1,
        gpu_driven: path == Path::Instanced,
        visibility_buffer: path == Path::Clusters,
        ..FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh))
    }
}

fn single_level_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        Some(MeshletMesh::single_level(&positions, indices))
    })
}

fn frame(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene, path: Path) {
    let mut scene = lab.as_frame_scene_with_quality(quality(path));
    for mesh in scene.meshes.iter_mut().skip(1) {
        mesh.mesh_key = CROWN_MESH;
    }
    if let Err(err) = renderer.render_offscreen(device, &scene) {
        panic!("{path:?} frame: {err}");
    }
}

/// Where the sun's line through `crown` comes down to mote height.
fn under(crown: [f32; 3]) -> [f32; 3] {
    let steps = (crown[1] - MOTE_Y) / -SUN_TRAVEL[1];
    [
        crown[0] + SUN_TRAVEL[0] * steps,
        MOTE_Y,
        crown[2] + SUN_TRAVEL[2] * steps,
    ]
}

/// Every crown shades the air down-sun of it, and the air past the stand stays in the sun.
fn check_tile(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene, path: Path) {
    renderer.request_precip_occlusion_readback();
    frame(renderer, device, lab, path);
    assert!(
        renderer.sun_occlusion().is_some() && renderer.precipitation_occlusion().is_none(),
        "{path:?}: the tile looks along the sun"
    );
    let map = renderer.take_precip_occlusion_readback().expect("copied");
    let mut uncovered = Vec::new();
    for crown in crowns() {
        let point = under(crown);
        let (open, cover) = map.exposure_at(point);
        if open > 0.0 || cover.is_none() {
            uncovered.push((crown, open, cover));
        }
    }
    assert!(
        uncovered.is_empty(),
        "{path:?}: {} of {} crowns are missing from the tile: {uncovered:?}",
        uncovered.len(),
        crowns().len()
    );
    for z in [-12.0, 0.0, 12.0] {
        let (open, cover) = map.exposure_at([16.0, MOTE_Y, z]);
        assert_eq!((open, cover), (1.0, None), "{path:?}: open sky east of the stand");
    }
}

fn run(path: Path) {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let gpu_objects = IndirectStrategy::select(&device.capabilities).uses_gpu_objects();
    if path != Path::Forward && !gpu_objects {
        eprintln!("skip {path:?}: the device draws no GPU objects");
        return;
    }
    let mut renderer = FrameRenderer::new();
    renderer.set_translucent_layers(vec![Arc::new(WantsSunView)]);
    if path == Path::Clusters {
        renderer.set_meshlet_builder(Some(single_level_builder()));
        let mut settings = renderer.visbuffer_settings();
        settings.cluster_shadows = true;
        renderer.set_visbuffer_settings(settings);
    }
    let lab = scene([0.0, 2.0, 0.0]);
    // Past the first frames: the two-phase shadow cull has history, the clusters are resident.
    for _ in 0..4 {
        frame(&mut renderer, &device, &lab, path);
    }
    assert_eq!(
        renderer.visbuffer_active(),
        path == Path::Clusters,
        "{path:?} is the path under test"
    );
    check_tile(&mut renderer, &device, &lab, path);
    // The eye walks on, past the view's snap: the redrawn tile still holds every crown.
    let redraws = renderer.sun_occlusion().expect("tile").redraws;
    let walked = scene([9.0, 2.0, -5.0]);
    frame(&mut renderer, &device, &walked, path);
    assert!(
        renderer.sun_occlusion().expect("tile").redraws > redraws,
        "{path:?}: the walk moved the tile"
    );
    check_tile(&mut renderer, &device, &walked, path);
}

#[test]
fn forward_trees_reach_the_sun_tile() {
    run(Path::Forward);
}

#[test]
fn instanced_trees_reach_the_sun_tile() {
    run(Path::Instanced);
}

#[test]
fn cluster_trees_reach_the_sun_tile() {
    run(Path::Clusters);
}
