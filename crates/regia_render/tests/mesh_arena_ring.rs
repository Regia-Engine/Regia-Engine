// SPDX-License-Identifier: MPL-2.0
//! A mesh uploaded through the mapped staging ring lands in its own mesh-arena range.
//!
//! Inside a frame a mesh upload goes through the staging belt when the belt's chunks have room
//! (ADR-218); with the mapped upload ring (ADR-285) those bytes are written into a mapped chunk
//! that the frame before had unmapped for its submit, and the copy into the arena range is
//! recorded from there. Renderer `ring` reserves a large belt chunk (`prewarm`), draws one quad,
//! then adds a dense grid mesh: the grid's vertices start past the quad's in the vertex arena
//! chunk and travel through the reused ring chunk. Renderer `queue` does the same with a small
//! belt, so the grid goes through `queue.write_buffer`. Both second frames must match pixel for
//! pixel, and only `ring` must have carried the grid's bytes in the belt.
//!
//! This file holds one test on purpose: the upload counters are process-wide.

use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameRenderer, FrameScene, LabMaterial, LabMesh, LabScene, LabVertex,
    RenderDevice, upload_split,
};

/// Grid side in vertices: 4,096 vertices (384 KiB) plus 23,814 indices (93 KiB), far more than
/// the frame's own belt data in this scene.
const GRID: u32 = 64;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn quad(x: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 0.0, 1.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([x - 0.3, -0.3, 0.0], n, [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.3, -0.3, 0.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.3, 0.3, 0.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x - 0.3, 0.3, 0.0], n, [0.0, 0.0]),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

/// A `GRID` × `GRID` vertex sheet over `x ∈ [0.1, 0.8]`, `y ∈ [-0.4, 0.4]`, gently bent in z so
/// its shading varies across the sheet (a vertex in the wrong place shows).
fn grid() -> (Vec<LabVertex>, Vec<u32>) {
    let last = (GRID - 1) as f32;
    let mut vertices = Vec::with_capacity((GRID * GRID) as usize);
    for j in 0..GRID {
        for i in 0..GRID {
            let (u, v) = (i as f32 / last, j as f32 / last);
            let z = 0.15 * (u * std::f32::consts::PI).sin();
            let normal = [
                -0.15 * std::f32::consts::PI * (u * std::f32::consts::PI).cos(),
                0.0,
                1.0,
            ];
            let len = (normal[0] * normal[0] + 1.0).sqrt();
            vertices.push(LabVertex::from_pos_normal_uv(
                [0.1 + 0.7 * u, -0.4 + 0.8 * v, z],
                [normal[0] / len, 0.0, 1.0 / len],
                [u, v],
            ));
        }
    }
    let mut indices = Vec::with_capacity(((GRID - 1) * (GRID - 1) * 6) as usize);
    for j in 0..GRID - 1 {
        for i in 0..GRID - 1 {
            let a = j * GRID + i;
            let (b, c, d) = (a + 1, a + GRID + 1, a + GRID);
            indices.extend_from_slice(&[a, b, c, a, c, d]);
        }
    }
    (vertices, indices)
}

fn lab() -> LabScene {
    let (vertices, indices) = quad(0.0);
    LabScene {
        width: 48,
        height: 32,
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
        directional: DirectionalLightUniform::new([0.3, -0.3, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![LabMesh {
            vertices,
            indices,
            ..LabMesh::unskinned()
        }],
    }
}

fn scene<'a>(base: &FrameScene<'a>, meshes: Vec<FrameMesh<'a>>) -> FrameScene<'a> {
    FrameScene {
        meshes,
        ..base.clone()
    }
}

#[test]
fn a_mesh_through_the_upload_ring_lands_in_its_arena_range() {
    let device = device();
    let lab = lab();
    let base = lab.as_frame_scene();
    let template = base.meshes[0];
    let (quad_v, quad_i) = quad(-0.5);
    let (grid_v, grid_i) = grid();
    let grid_bytes =
        (std::mem::size_of_val(grid_v.as_slice()) + std::mem::size_of_val(grid_i.as_slice())) as u64;
    let first = FrameMesh {
        mesh_key: 0x0A12_0001 << 3,
        object_key: 0x0B1E_C711,
        vertices: &quad_v,
        indices: &quad_i,
        ..template
    };
    let second = FrameMesh {
        mesh_key: 0x0A12_0002 << 3,
        object_key: 0x0B1E_C712,
        vertices: &grid_v,
        indices: &grid_i,
        ..template
    };
    let one = scene(&base, vec![first]);
    let both = scene(&base, vec![first, second]);

    // Two frames on one renderer; returns the second frame and the belt bytes it wrote.
    let run = |max_instances: u32| {
        let mut renderer = FrameRenderer::new();
        renderer.prewarm(&device, &[], max_instances).expect("prewarm");
        let alone = renderer.render_offscreen(&device, &one).expect("first frame");
        let before = upload_split().belt_bytes;
        let frame = renderer.render_offscreen(&device, &both).expect("second frame");
        (alone, frame, upload_split().belt_bytes - before)
    };
    // 16,384 instances reserve a 16 MiB belt chunk: the grid plus the 1 MiB headroom fits.
    let (_, ring, ring_belt) = run(16_384);
    // One instance reserves the smallest chunk: the grid does not fit and is written directly.
    let (alone, queue, queue_belt) = run(1);

    let differ = |a: &[u8], b: &[u8]| a.chunks(4).zip(b.chunks(4)).filter(|(a, b)| a != b).count();
    assert!(differ(&alone.pixels, &queue.pixels) > 0, "the grid is in view");
    assert!(
        ring_belt >= queue_belt + grid_bytes,
        "the grid's {grid_bytes} bytes went through the belt (belt bytes: ring {ring_belt}, queue {queue_belt})"
    );
    assert_eq!(
        differ(&ring.pixels, &queue.pixels),
        0,
        "a mesh copied from the upload ring into its arena range draws like one written directly"
    );
}
