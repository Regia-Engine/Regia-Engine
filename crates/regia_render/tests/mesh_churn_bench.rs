// SPDX-License-Identifier: MPL-2.0
//! Mesh-churn bench (roadmap 7a; `--ignored`, prints numbers): 40 units of 1,089 vertices,
//! drawn by a host that keeps its mesh `Arc`s (`stable`) or builds new ones every frame with
//! the same content (`churn`, the old ones dropped after the new ones exist). Per mode: CPU
//! time of `render_into` (encode + submit, no readback wait) p50 / p99, and buffers created
//! after warm-up.
//!
//! `cargo test -p regia_render --test mesh_churn_bench -- --ignored --nocapture`

use std::sync::Arc;

use regia_render::{
    CpuMesh, DirectionalLightUniform, FrameMesh, FrameRenderer, FrameScene, LabMaterial, LabMesh, LabScene,
    LabVertex, RenderDevice, frame_path_counters, mesh_key, take_released_mesh_keys, upload_split,
};

const UNITS: usize = 40;
const GRID: usize = 33;
const WARM: usize = 30;
const FRAMES: usize = 240;

fn grid(x: f32) -> CpuMesh {
    let mut vertices = Vec::with_capacity(GRID * GRID);
    for j in 0..GRID {
        for i in 0..GRID {
            let (u, v) = (i as f32 / (GRID - 1) as f32, j as f32 / (GRID - 1) as f32);
            vertices.push(LabVertex::from_pos_normal_uv(
                [x + (u - 0.5) * 0.1, (v - 0.5) * 0.1, 0.0],
                [0.0, 0.0, 1.0],
                [u, v],
            ));
        }
    }
    let mut indices = Vec::new();
    for j in 0..GRID as u32 - 1 {
        for i in 0..GRID as u32 - 1 {
            let a = j * GRID as u32 + i;
            let b = a + GRID as u32;
            indices.extend_from_slice(&[a, a + 1, b + 1, a, b + 1, b]);
        }
    }
    CpuMesh { vertices, indices }
}

fn lab() -> LabScene {
    let mesh = grid(0.0);
    LabScene {
        width: 64,
        height: 64,
        eye: [0.0, 0.0, 3.0],
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
        meshes: vec![LabMesh {
            vertices: mesh.vertices,
            indices: mesh.indices,
            ..LabMesh::unskinned()
        }],
    }
}

fn units() -> Vec<Arc<CpuMesh>> {
    (0..UNITS)
        .map(|i| Arc::new(grid(i as f32 * 0.05 - 1.0)))
        .collect()
}

fn percentile(sorted: &[f32], p: f32) -> f32 {
    sorted[((sorted.len() - 1) as f32 * p).round() as usize]
}

fn run(device: &RenderDevice, base: &FrameScene<'_>, churn: bool) {
    let template = base.meshes[0];
    let mut renderer = FrameRenderer::new();
    let mut meshes = units();
    let mut cpu = Vec::with_capacity(FRAMES);
    let mut warm = None;
    for n in 0..WARM + FRAMES {
        if churn {
            // The new Arcs exist before the old ones drop.
            meshes = units();
        }
        renderer.release_meshes(&take_released_mesh_keys());
        let frame = FrameScene {
            meshes: meshes
                .iter()
                .enumerate()
                .map(|(i, mesh)| FrameMesh {
                    mesh_key: mesh_key(mesh),
                    object_key: 0x0B1E_C700 + i as u64,
                    vertices: &mesh.vertices,
                    indices: &mesh.indices,
                    ..template
                })
                .collect(),
            ..base.clone()
        };
        let _ = renderer.render_offscreen(device, &frame).expect("frame");
        if n == WARM {
            warm = Some((upload_split(), frame_path_counters().buffers));
        }
        if n >= WARM {
            cpu.push(renderer.cpu_timings().total_ms);
        }
    }
    let (split_warm, buffers_warm) = warm.expect("warm");
    let split = upload_split();
    cpu.sort_by(f32::total_cmp);
    println!(
        "{}: cpu render_into p50 {:.3} ms p99 {:.3} ms over {FRAMES} frames; after warm-up: {} mesh buffers, {} buffers, {} KiB mesh uploads",
        if churn { "churn " } else { "stable" },
        percentile(&cpu, 0.5),
        percentile(&cpu, 0.99),
        split.mesh_buffers - split_warm.mesh_buffers,
        frame_path_counters().buffers - buffers_warm,
        (split.mesh_bytes - split_warm.mesh_bytes) / 1024,
    );
}

#[test]
#[ignore = "bench: prints numbers"]
fn mesh_churn_bench() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let lab = lab();
    let base = lab.as_frame_scene();
    run(&device, &base, false);
    run(&device, &base, true);
}
