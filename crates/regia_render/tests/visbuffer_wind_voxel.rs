// SPDX-License-Identifier: MPL-2.0
//! The sway's voxel fade finds a mesh's voxel far levels from the cluster list (ADR-3916): this pins
//! `visbuffer::sway::voxel_switch` against what `VoxelFarProxy::extend_meshlets` really writes, on a
//! canopy cooked as the cook does it. CPU only.

use regia_asset_pipeline::voxel_far_cook::{VoxelFarCookSettings, VoxelizeInput, voxelize};
use regia_asset_pipeline::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::LabVertex;
use regia_render::visbuffer::sway::{SWAY_FADE_END, sway_fade, voxel_switch};
use regia_render::visbuffer::{MeshletMesh, VoxelFarProxy, VoxelFarSettings};

/// `count` small leaf quads scattered over a sphere (fixed LCG), facing out.
fn canopy(count: u32) -> (Vec<LabVertex>, Vec<u32>) {
    let mut state = 0x2345_6789u32;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 8) as f32 / (1u32 << 24) as f32
    };
    let (mut vertices, mut indices) = (Vec::new(), Vec::new());
    for _ in 0..count {
        let (mut c, mut l2) = ([0.0f32; 3], 2.0f32);
        while l2 > 1.0 {
            c = [next() * 2.0 - 1.0, next() * 2.0 - 1.0, next() * 2.0 - 1.0];
            l2 = c[0] * c[0] + c[1] * c[1] + c[2] * c[2];
        }
        let n = c.map(|v| v / l2.sqrt().max(1e-3));
        let t = if n[0].abs() < 0.9 {
            [0.0, -n[2], n[1]]
        } else {
            [n[2], 0.0, -n[0]]
        };
        let length = (t[0] * t[0] + t[1] * t[1] + t[2] * t[2]).sqrt();
        let t = t.map(|v| v / length * 0.04);
        let b = [
            n[1] * t[2] - n[2] * t[1],
            n[2] * t[0] - n[0] * t[2],
            n[0] * t[1] - n[1] * t[0],
        ];
        let base = vertices.len() as u32;
        for (u, v) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let mut vertex = LabVertex::from_pos_normal_uv(
                std::array::from_fn(|k| c[k] * 0.9 + t[k] * u + b[k] * v),
                n,
                [0.5, 0.5],
            );
            vertex.color = [0.18, 0.45, 0.08, 1.0];
            vertices.push(vertex);
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

#[test]
fn the_voxel_switch_is_found_in_the_real_extension_and_only_there() {
    let (vertices, indices) = canopy(3000);
    let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
    let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
    let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
    let colors: Vec<[f32; 4]> = vertices.iter().map(|v| v.color).collect();
    let dag = build_meshlet_mesh_with_attributes(
        &positions,
        Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        }),
        &indices,
        &MeshletSettings::default(),
    )
    .expect("cluster DAG");
    assert_eq!(voxel_switch(&dag), None, "a cooked DAG has no voxel level");
    assert_eq!(
        voxel_switch(&MeshletMesh::single_level(&positions, &indices)),
        None,
        "nor has a single level"
    );
    let proxy = voxelize(
        "canopy",
        &VoxelizeInput {
            positions: &positions,
            normals: &normals,
            uvs: &uvs,
            colors: &colors,
            indices: &indices,
        },
        &VoxelFarCookSettings { resolution: 32 },
    )
    .expect("voxelize");
    let far = proxy
        .extend_meshlets(&dag, &vertices, &VoxelFarSettings::default())
        .expect("extend");
    let first = *far.levels.first().expect("a voxel level cheaper than the roots");
    let found = voxel_switch(&far.meshlets).expect("the extension's levels are found");
    assert_eq!(found.level, first.dag_level);
    assert_eq!(found.error, first.error);
    // Every cluster below it is the DAG's, every cluster from it up a voxel cluster.
    let dag_clusters = far
        .meshlets
        .meshlets
        .iter()
        .filter(|m| m.level < found.level)
        .count();
    assert_eq!(dag_clusters, dag.meshlets.len());
    let sphere = far
        .meshlets
        .meshlets
        .iter()
        .find(|m| m.level == found.level)
        .expect("a voxel cluster")
        .lod;
    assert_eq!(found.sphere, sphere);
    // The fade ends before the distance the proxy itself gives for the switch.
    let (ppu, threshold) = (128.0 / (2.0 * 0.5f32.tan()), 2.0);
    let switch = VoxelFarProxy::switch_distance(first.error, sphere[3], ppu, threshold);
    assert_eq!(sway_fade(SWAY_FADE_END * switch, switch), 0.0);
    assert!(sway_fade(0.5 * switch, switch) == 1.0);
}
