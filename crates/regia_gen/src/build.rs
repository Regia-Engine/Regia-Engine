// SPDX-License-Identifier: MPL-2.0
//! The shared mesh builder, vector helpers, and the whole-mesh transforms.

use crate::{CpuMesh, weld_key};

pub(crate) fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub(crate) fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub(crate) fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// `v / |v|`, or zero for a zero vector. `sqrt` and `/` are exact IEEE operations.
pub(crate) fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = dot(v, v).sqrt();
    if len > 0.0 && len.is_finite() {
        [v[0] / len, v[1] / len, v[2] / len]
    } else {
        [0.0; 3]
    }
}

/// The normal of triangle `a b c` (counter-clockwise front), unit length or zero.
pub(crate) fn face_normal(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    normalize(cross(sub(b, a), sub(c, a)))
}

/// Appends vertices and triangles; skips triangles whose corners weld together.
#[derive(Default)]
pub(crate) struct Builder {
    pub(crate) mesh: CpuMesh,
}

impl Builder {
    pub(crate) fn vertex(&mut self, p: [f32; 3], n: [f32; 3], uv: [f32; 2]) -> u32 {
        self.mesh.positions.push(p);
        self.mesh.normals.push(n);
        self.mesh.uvs.push(uv);
        (self.mesh.positions.len() - 1) as u32
    }

    /// Adds triangle `a b c` unless two corners share a position (zero area by topology).
    pub(crate) fn tri(&mut self, a: u32, b: u32, c: u32) {
        let key = |i: u32| weld_key(self.mesh.positions[i as usize]);
        let (ka, kb, kc) = (key(a), key(b), key(c));
        if ka != kb && kb != kc && ka != kc {
            self.mesh.indices.extend_from_slice(&[a, b, c]);
        }
    }

    /// A planar quad `a b c d` (counter-clockwise front) with its own four vertices and a flat
    /// normal from the first corner's two edges (the second for a collapsed first edge).
    pub(crate) fn quad(&mut self, p: [[f32; 3]; 4], uv: [[f32; 2]; 4]) {
        let mut n = normalize(cross(sub(p[1], p[0]), sub(p[3], p[0])));
        if n == [0.0; 3] {
            n = normalize(cross(sub(p[2], p[1]), sub(p[3], p[1])));
        }
        let i = std::array::from_fn::<u32, 4, _>(|k| self.vertex(p[k], n, uv[k]));
        self.tri(i[0], i[1], i[2]);
        self.tri(i[0], i[2], i[3]);
    }

    pub(crate) fn finish(self) -> CpuMesh {
        self.mesh
    }
}

/// Recomputes every normal as its triangle's face normal, splitting vertices so each triangle
/// owns its three corners (faceted shading). UVs are kept per corner.
pub fn flat_normals(mesh: &CpuMesh) -> CpuMesh {
    let mut b = Builder::default();
    for t in mesh.indices.as_chunks::<3>().0 {
        let Some(p) = corners(&mesh.positions, t) else {
            continue;
        };
        let n = face_normal(p[0], p[1], p[2]);
        let uv = |k: usize| mesh.uvs.get(t[k] as usize).copied().unwrap_or([0.0; 2]);
        let a = b.vertex(p[0], n, uv(0));
        let bb = b.vertex(p[1], n, uv(1));
        let c = b.vertex(p[2], n, uv(2));
        b.tri(a, bb, c);
    }
    b.finish()
}

pub(crate) fn corners(positions: &[[f32; 3]], t: &[u32; 3]) -> Option<[[f32; 3]; 3]> {
    Some([
        *positions.get(t[0] as usize)?,
        *positions.get(t[1] as usize)?,
        *positions.get(t[2] as usize)?,
    ])
}

/// Concatenates meshes into one (a union of solids that do not overlap). Missing normals or UVs
/// of a part become zero so the attribute arrays stay the same length.
pub fn merge(parts: &[&CpuMesh]) -> CpuMesh {
    let mut out = CpuMesh::default();
    for part in parts {
        let base = out.positions.len() as u32;
        let n = part.positions.len();
        out.positions.extend_from_slice(&part.positions);
        out.normals
            .extend((0..n).map(|i| part.normals.get(i).copied().unwrap_or([0.0; 3])));
        out.uvs
            .extend((0..n).map(|i| part.uvs.get(i).copied().unwrap_or([0.0; 2])));
        out.indices.extend(part.indices.iter().map(|i| base + i));
    }
    out
}

/// Moves every position by `by`.
pub fn translate(mesh: &CpuMesh, by: [f32; 3]) -> CpuMesh {
    let mut out = mesh.clone();
    for p in &mut out.positions {
        *p = [p[0] + by[0], p[1] + by[1], p[2] + by[2]];
    }
    out
}

/// Scales every position per axis. Normals are rescaled by the inverse and renormalized; a
/// negative determinant reverses the winding so triangles keep facing outward.
pub fn scale(mesh: &CpuMesh, by: [f32; 3]) -> CpuMesh {
    let mut out = mesh.clone();
    for p in &mut out.positions {
        *p = [p[0] * by[0], p[1] * by[1], p[2] * by[2]];
    }
    for n in &mut out.normals {
        *n = normalize([n[0] * by[1] * by[2], n[1] * by[0] * by[2], n[2] * by[0] * by[1]]);
    }
    if by[0] * by[1] * by[2] < 0.0 {
        for t in out.indices.as_chunks_mut::<3>().0 {
            t.swap(1, 2);
        }
    }
    out
}
