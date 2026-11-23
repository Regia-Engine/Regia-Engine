// SPDX-License-Identifier: MPL-2.0
//! Planar and box (tri-planar per face) UV projection.
//!
//! Method: the textbook planar map `uv = (p·a, p·b) · scale + offset` along a world axis; box
//! projection picks each triangle's dominant face-normal axis (the "cube map" face selection of
//! the public graphics literature) and splits vertices whose triangles pick different axes.

use std::collections::HashMap;

use crate::build::{corners, cross, sub};
use crate::{CpuMesh, GenError, finite};

/// A world axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    /// +X.
    X,
    /// +Y (up).
    Y,
    /// +Z.
    Z,
}

impl Axis {
    /// The two in-plane coordinates of `p` seen along this axis: X → (z, y), Y → (x, z),
    /// Z → (x, y).
    fn project(self, p: [f32; 3]) -> [f32; 2] {
        match self {
            Axis::X => [p[2], p[1]],
            Axis::Y => [p[0], p[2]],
            Axis::Z => [p[0], p[1]],
        }
    }
}

fn map(a: [f32; 2], scale: f32, offset: [f32; 2]) -> [f32; 2] {
    [a[0] * scale + offset[0], a[1] * scale + offset[1]]
}

fn check(scale: f32, offset: [f32; 2]) -> Result<(), GenError> {
    finite("uv scale and offset", &[scale, offset[0], offset[1]])
}

/// Sets every UV to the vertex position projected along `axis`, times `scale`, plus `offset`.
/// Positions, normals and indices are unchanged.
pub fn uv_planar(mesh: &CpuMesh, axis: Axis, scale: f32, offset: [f32; 2]) -> Result<CpuMesh, GenError> {
    check(scale, offset)?;
    let mut out = mesh.clone();
    out.uvs = out
        .positions
        .iter()
        .map(|p| map(axis.project(*p), scale, offset))
        .collect();
    Ok(out)
}

/// Box projection: each triangle is projected along the axis its face normal is most aligned with
/// (ties: X before Y before Z), times `scale`, plus `offset`. Vertices shared by triangles that
/// pick different axes are split (positions and normals copied), so the vertex count can grow;
/// the triangle order is unchanged. A cube of side `1 / scale` centered at `-offset / scale`
/// maps every face onto exactly `[0, 1]²`.
pub fn uv_box(mesh: &CpuMesh, scale: f32, offset: [f32; 2]) -> Result<CpuMesh, GenError> {
    check(scale, offset)?;
    let mut out = CpuMesh::default();
    let mut ids: HashMap<(u32, Axis), u32> = HashMap::new();
    for t in mesh.indices.as_chunks::<3>().0 {
        let Some(p) = corners(&mesh.positions, t) else {
            return Err(GenError::InvalidInput("an index is out of range".into()));
        };
        let n = cross(sub(p[1], p[0]), sub(p[2], p[0])).map(f32::abs);
        let axis = if n[0] >= n[1] && n[0] >= n[2] {
            Axis::X
        } else if n[1] >= n[2] {
            Axis::Y
        } else {
            Axis::Z
        };
        for &i in t {
            let id = *ids.entry((i, axis)).or_insert_with(|| {
                let pos = mesh.positions[i as usize];
                out.positions.push(pos);
                out.normals
                    .push(mesh.normals.get(i as usize).copied().unwrap_or([0.0; 3]));
                out.uvs.push(map(axis.project(pos), scale, offset));
                (out.positions.len() - 1) as u32
            });
            out.indices.push(id);
        }
    }
    Ok(out)
}
