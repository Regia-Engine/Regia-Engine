// SPDX-License-Identifier: MPL-2.0
//! Cooked opt-in eight skin influences on the GPU (ADR-1622, DO27 request DO-6).
//!
//! A cooked skin with two influence sets goes through `CpuMesh::from_cooked` (the vertex-shader
//! path's four strongest) and `SkinningOptions::for_cooked_skin`. With the opt-in off there are
//! no options; with it on, the compute kernel (`skin_compute.wgsl`) skins all eight influences
//! and matches `skin_vertex_reference` with the eight, and differs from the four strongest.

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_render::{
    CpuMesh, JointMatrix, RenderDevice, SkinningMethod, SkinningOptions, skin_on_gpu, skin_vertex_reference,
};

const VERTICES: usize = 48;

/// Eight joints, each a different rotation about Y plus a translation, so every influence moves
/// a vertex somewhere else.
fn palette() -> Vec<JointMatrix> {
    (0..8)
        .map(|j| {
            let angle = (j as f32 * 23.0).to_radians();
            let offset = Vec3::new(0.1 * j as f32, 0.05 * (j % 3) as f32, -0.07 * j as f32);
            JointMatrix::from_mat4(Mat4::from_rotation_translation(
                Quat::from_rotation_y(angle),
                offset,
            ))
        })
        .collect()
}

/// A strip of vertices bound to all eight joints; the heavier weights sit in the second set,
/// so the four strongest are not the first four.
fn cooked(eight_influences: bool) -> regia_asset::CpuMesh {
    let positions: Vec<[f32; 3]> = (0..VERTICES)
        .map(|i| [0.3 + 0.01 * i as f32, 0.05 * i as f32, 0.2])
        .collect();
    let mut weights = Vec::new();
    let mut weights1 = Vec::new();
    for i in 0..VERTICES {
        let shift = (i % 4) as u16 * 500;
        let w0 = [3000 + shift, 4000, 5000, 6000];
        let w1 = [20000 - shift, 12000, 10000];
        let used: u16 = w0.iter().chain(&w1).sum();
        weights.push(w0);
        weights1.push([w1[0], w1[1], w1[2], 65535 - used]);
    }
    regia_asset::CpuMesh {
        normals: vec![[0.0, 0.0, 1.0]; VERTICES],
        uvs: vec![[0.0; 2]; VERTICES],
        indices: (0..(VERTICES as u32 - 2))
            .flat_map(|i| [i, i + 1, i + 2])
            .collect(),
        skin: Some(regia_asset::CpuSkin {
            joint_names: (0..8).map(|j| format!("j{j}")).collect(),
            joints: vec![[0, 1, 2, 3]; VERTICES],
            weights,
            joints1: vec![[4, 5, 6, 7]; VERTICES],
            weights1,
            eight_influences,
        }),
        positions,
        ..Default::default()
    }
}

#[test]
fn the_cooked_opt_in_skins_eight_influences_on_the_gpu() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let joints = palette();

    // Off: no options at all, so the mesh stays on the vertex-shader path as before.
    let off = cooked(false);
    assert!(
        off.skin
            .as_ref()
            .and_then(SkinningOptions::for_cooked_skin)
            .is_none()
    );

    let on = cooked(true);
    let skin = on.skin.as_ref().expect("skin");
    let options = SkinningOptions::for_cooked_skin(skin).expect("opted in");
    let render_mesh = CpuMesh::from_cooked(&on).expect("render mesh");
    assert_eq!(
        bytemuck::cast_slice::<_, u8>(&render_mesh.vertices),
        bytemuck::cast_slice::<_, u8>(&CpuMesh::from_cooked(&off).expect("off").vertices),
        "the vertex buffer is the same with the opt-in on or off"
    );
    let eight = skin_on_gpu(&device, &render_mesh.vertices, &joints, &options).expect("eight on the GPU");
    let four = skin_on_gpu(
        &device,
        &render_mesh.vertices,
        &joints,
        &SkinningOptions::compute(),
    )
    .expect("four on the GPU");
    let table = options.influences8.as_ref().expect("table");
    let mut apart = 0.0f32;
    for (v, vertex) in render_mesh.vertices.iter().enumerate() {
        let influences: Vec<(u32, f32)> = table[v].joints.iter().copied().zip(table[v].weights).collect();
        let (want, _, _) = skin_vertex_reference(
            (vertex.position, vertex.normal, [1.0, 0.0, 0.0]),
            &influences,
            &joints,
            SkinningMethod::Linear,
        );
        let got = Vec3::from(eight[v].position);
        assert!(
            got.distance(Vec3::from(want)) < 1e-4,
            "vertex {v}: kernel {got} vs reference {want:?}"
        );
        apart = apart.max(got.distance(Vec3::from(four[v].position)));
    }
    assert!(
        apart > 1e-2,
        "eight influences drew like the four strongest ({apart})"
    );
}
