// SPDX-License-Identifier: MIT-0
//! The lab's kit: a wall and a tower, cut with the fracture tool from primitives when a test or the
//! example starts (no binary assets). Shared by `#[path]`: the library may not link the pipeline.

use std::sync::Arc;

use destruction_lab::{GROUND_Y, LabKit, LabLook, LabStructure, TOWER_X, WALL_X};
use regia::framework::destruction_draw::{DestructionChunkMeshes, DestructionLodMeshes, DestructionMeshSet};
use regia::physics::destruction::{DestructibleAsset, DestructiblePose};
use regia_asset::CpuMesh as CookMesh;
use regia_asset_pipeline::destructible_cook::{CookSettings, cook_destructible};
use regia_asset_pipeline::fracture::{FractureChunk, FractureSet, FractureSettings, box_mesh, fracture_mesh};
use regia_render::{CpuMaterial, CpuMesh};

/// Half extents of the wall (meters).
pub const WALL_HALF: [f32; 3] = [3.0, 1.5, 0.2];
/// Half extents of the tower (meters).
pub const TOWER_HALF: [f32; 3] = [1.0, 2.5, 1.0];

/// Part of a chunk's render mesh, with its vertices compacted.
fn part(chunk: &FractureChunk, triangles: std::ops::Range<usize>) -> Option<CookMesh> {
    let render = &chunk.render;
    let mut map = vec![u32::MAX; render.positions.len()];
    let mut mesh = CookMesh::default();
    for t in triangles {
        for &i in &render.indices[t * 3..t * 3 + 3] {
            let at = i as usize;
            if map[at] == u32::MAX {
                map[at] = mesh.positions.len() as u32;
                mesh.positions.push(render.positions[at]);
                mesh.normals.push(render.normals[at]);
                mesh.uvs.push(render.uvs[at]);
                mesh.tangents.push(render.tangents[at]);
            }
            mesh.indices.push(map[at]);
        }
    }
    (!mesh.indices.is_empty()).then_some(mesh)
}

fn lod(chunk: &FractureChunk) -> DestructionLodMeshes {
    let split = chunk.render.interior_start as usize;
    let all = chunk.render.indices.len() / 3;
    DestructionLodMeshes::from_cpu_parts(part(chunk, 0..split).as_ref(), part(chunk, split..all).as_ref())
}

/// Every chunk's surface in one mesh: the structure as it stands before it breaks.
fn intact_mesh(set: &DestructionMeshSet) -> CpuMesh {
    let mut whole = CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    for surface in set
        .chunks
        .iter()
        .filter_map(|c| c.lods.first())
        .filter_map(|l| l.surface.as_ref())
    {
        let base = whole.vertices.len() as u32;
        whole.vertices.extend_from_slice(&surface.vertices);
        whole.indices.extend(surface.indices.iter().map(|i| i + base));
    }
    whole
}

/// One primitive cut into `chunks` and placed standing on the ground at `x`.
fn structure(
    name: &str,
    half: [f32; 3],
    chunks: usize,
    fracture_seed: u64,
    x: f32,
    color: [f32; 3],
) -> Result<LabStructure, String> {
    let (positions, indices) = box_mesh(half).map_err(|e| format!("{name}: {e}"))?;
    let settings = FractureSettings::random(chunks, fracture_seed);
    let set: FractureSet =
        fracture_mesh(&positions, &indices, &settings).map_err(|e| format!("{name}: {e}"))?;
    let cook = CookSettings {
        density: 1800.0,
        bond_seconds: 0.002,
        fracture_seed: Some(fracture_seed),
        ..CookSettings::new(name)
    };
    let asset: DestructibleAsset = cook_destructible(&set, &cook).map_err(|e| format!("{name}: {e}"))?;
    let mesh_set = DestructionMeshSet {
        chunks: set
            .chunks
            .iter()
            .map(|c| DestructionChunkMeshes { lods: vec![lod(c)] })
            .collect(),
        surface_material: Arc::new(CpuMaterial {
            base_color: [color[0], color[1], color[2], 1.0],
            ..CpuMaterial::default()
        }),
        cut_material: Arc::new(CpuMaterial {
            base_color: [0.72, 0.68, 0.6, 1.0],
            ..CpuMaterial::default()
        }),
    };
    let intact = Arc::new(intact_mesh(&mesh_set));
    let material = mesh_set.surface_material.clone();
    Ok(LabStructure {
        asset: Arc::new(asset),
        pose: DestructiblePose::at([x, GROUND_Y + half[1], 0.0]),
        seed: fracture_seed,
        look: Some(LabLook {
            set: mesh_set,
            intact,
            material,
        }),
    })
}

/// Cuts the kit. The same seeds give the same chunks.
pub fn fracture_kit() -> Result<LabKit, String> {
    Ok(LabKit {
        structures: vec![
            structure("lab_wall", WALL_HALF, 48, 11, WALL_X, [0.55, 0.27, 0.2])?,
            structure("lab_tower", TOWER_HALF, 40, 23, TOWER_X, [0.42, 0.45, 0.5])?,
        ],
    })
}
