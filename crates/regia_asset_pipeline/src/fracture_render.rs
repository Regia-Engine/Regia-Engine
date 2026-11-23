// SPDX-License-Identifier: MPL-2.0
//! Render attributes of fracture chunks (ADR-1967): the normals, UVs and tangents the welded chunk
//! meshes of `fracture.rs` lack.
//!
//! A chunk has two kinds of triangle. A *surface piece* is part of a source triangle, so its
//! corners read the source's attributes by barycentric interpolation (Moller and Haines, *Real-Time
//! Rendering*). A source without normals gets them per corner, smoothed across edges flatter than
//! `crease_degrees` and weighted by corner angle (Thurmer and Wuthrich, "Computing vertex normals
//! from polygonal facets", JGT 3(1), 1998); a source without UVs gets a box projection along each
//! triangle's dominant axis. A *cut face* gets its cutting plane's normal and a planar projection
//! at `interior_uv_scale` UV units per metre, on a basis chosen from the plane alone: no stretch,
//! the same texel density on every cut, and the two sides of one cut show the same pattern.
//! Tangents come from MikkTSpace over the finished mesh (Mikkelsen, "Simulation of wrinkled
//! surfaces revisited", 2008), the importer's generator, so cut faces shade like any cooked mesh.
//!
//! Vertices are split where a normal or UV differs, so the render mesh has more vertices than the
//! welded mesh but the same triangles in the same order.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::fracture::{FractureChunk, FractureError, Solid};
use crate::fracture_clip::{P, Tri, add, cross, dot, length, plane_basis, scale, sub};

/// A closed mesh with whatever vertex attributes it has.
#[derive(Clone, Copy, Debug, Default)]
pub struct FractureSource<'a> {
    /// Vertex positions.
    pub positions: &'a [[f32; 3]],
    /// Triangle indices, counter-clockwise from outside.
    pub indices: &'a [u32],
    /// Vertex normals; `None` generates them.
    pub normals: Option<&'a [[f32; 3]]>,
    /// Texture coordinates; `None` generates a box projection.
    pub uvs: Option<&'a [[f32; 2]]>,
}

/// How the chunks' render attributes are made.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderSettings {
    /// UV units per metre on the cut faces (> 0).
    pub interior_uv_scale: f32,
    /// UV units per metre of the box projection a source without UVs gets (> 0).
    pub surface_uv_scale: f32,
    /// A source without normals is smoothed across edges flatter than this, degrees (0 to 180).
    pub crease_degrees: f32,
}

impl Default for RenderSettings {
    fn default() -> Self {
        Self {
            interior_uv_scale: 1.0,
            surface_uv_scale: 1.0,
            crease_degrees: 40.0,
        }
    }
}

impl RenderSettings {
    /// Checks the ranges; the error text names the setting.
    pub(crate) fn check(&self) -> Result<(), String> {
        let positive = |v: f32| v.is_finite() && v > 0.0;
        if !positive(self.interior_uv_scale) {
            return Err("interior_uv_scale must be finite and positive".to_string());
        }
        if !positive(self.surface_uv_scale) {
            return Err("surface_uv_scale must be finite and positive".to_string());
        }
        if !(0.0..=180.0).contains(&self.crease_degrees) {
            return Err("crease_degrees must be 0 to 180".to_string());
        }
        Ok(())
    }
}

/// What a chunk draws. Arrays are per vertex; triangles match the chunk's welded `indices` one to
/// one, so the first `interior_start` are surface pieces (outer material) and the rest cut faces
/// (inner material).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChunkRenderMesh {
    /// Vertex positions.
    pub positions: Vec<[f32; 3]>,
    /// Unit vertex normals.
    pub normals: Vec<[f32; 3]>,
    /// MikkTSpace tangents (xyz) with handedness in w (+1 or -1).
    pub tangents: Vec<[f32; 4]>,
    /// Texture coordinates.
    pub uvs: Vec<[f32; 2]>,
    /// Triangle indices.
    pub indices: Vec<u32>,
    /// Index of the first cut-face triangle (not of its first index).
    pub interior_start: u32,
}

impl ChunkRenderMesh {
    /// Checks the mesh against its chunk's index count and `interior_start`.
    pub(crate) fn check(&self, chunk_indices: usize, chunk_interior_start: u32) -> Result<(), String> {
        let n = self.positions.len();
        if self.normals.len() != n || self.tangents.len() != n || self.uvs.len() != n {
            return Err("attribute arrays differ in length".to_string());
        }
        if self.indices.len() != chunk_indices {
            return Err(format!(
                "{} indices against the chunk's {chunk_indices}",
                self.indices.len()
            ));
        }
        if self.interior_start != chunk_interior_start {
            return Err("interior_start differs from the chunk's".to_string());
        }
        if self.indices.iter().any(|&i| i as usize >= n) {
            return Err("an index is out of range".to_string());
        }
        let finite = self.positions.iter().flatten().all(|c| c.is_finite())
            && self.normals.iter().flatten().all(|c| c.is_finite())
            && self.tangents.iter().flatten().all(|c| c.is_finite())
            && self.uvs.iter().flatten().all(|c| c.is_finite());
        if !finite {
            return Err("an attribute is not finite".to_string());
        }
        Ok(())
    }
}

/// Attributes of one source triangle's three corners.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Corners {
    normals: [P; 3],
    uvs: [[f64; 2]; 3],
}

/// Attributes per source triangle (generated where the source has none).
pub(crate) struct SourceCorners {
    tris: Vec<Corners>,
    /// The source triangles' positions, for the barycentric lookup.
    frames: Vec<[P; 3]>,
}

impl SourceCorners {
    /// Reads or generates every source triangle's corner attributes.
    pub(crate) fn new(
        source: &FractureSource<'_>,
        solid: &Solid,
        settings: &RenderSettings,
    ) -> Result<Self, FractureError> {
        let vertices = source.positions.len();
        if let Some(normals) = source.normals {
            if normals.len() != vertices {
                return Err(FractureError::BadAttributes(format!(
                    "{} normals for {vertices} vertices",
                    normals.len()
                )));
            }
            let bad = |n: &[f32; 3]| {
                !n.iter().all(|c| c.is_finite()) || dot(to_p(*n), to_p(*n)) < f64::MIN_POSITIVE.sqrt()
            };
            if let Some(i) = normals.iter().position(bad) {
                return Err(FractureError::BadAttributes(format!(
                    "normal {i} is zero or not finite"
                )));
            }
        }
        if let Some(uvs) = source.uvs {
            if uvs.len() != vertices {
                return Err(FractureError::BadAttributes(format!(
                    "{} uvs for {vertices} vertices",
                    uvs.len()
                )));
            }
            if let Some(i) = uvs.iter().position(|u| !u.iter().all(|c| c.is_finite())) {
                return Err(FractureError::BadAttributes(format!("uv {i} is not finite")));
            }
        }
        let frames: Vec<[P; 3]> = solid.tris.iter().map(|t| t.v).collect();
        let smooth = match source.normals {
            Some(_) => Vec::new(),
            None => crease_normals(&frames, &solid.welded, settings.crease_degrees),
        };
        let tris = source
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .enumerate()
            .map(|(t, tri)| {
                let normals = match source.normals {
                    Some(n) => tri_map(tri, |i| unit(to_p(n[i]))),
                    None => smooth[t],
                };
                let uvs = match source.uvs {
                    Some(uv) => tri_map(tri, |i| [f64::from(uv[i][0]), f64::from(uv[i][1])]),
                    None => box_uvs(&frames[t], settings.surface_uv_scale),
                };
                Corners { normals, uvs }
            })
            .collect();
        Ok(Self { tris, frames })
    }

    /// The attributes at a corner of a surface piece of source triangle `src`.
    fn at(&self, src: u32, p: P) -> (P, [f64; 2]) {
        let (Some(c), Some(f)) = (self.tris.get(src as usize), self.frames.get(src as usize)) else {
            return ([0.0, 0.0, 1.0], [0.0, 0.0]);
        };
        let w = barycentric(f, p);
        let n = add(
            add(scale(c.normals[0], w[0]), scale(c.normals[1], w[1])),
            scale(c.normals[2], w[2]),
        );
        let uv = [
            w[0] * c.uvs[0][0] + w[1] * c.uvs[1][0] + w[2] * c.uvs[2][0],
            w[0] * c.uvs[0][1] + w[1] * c.uvs[1][1] + w[2] * c.uvs[2][1],
        ];
        (unit(n), uv)
    }
}

fn tri_map<T>(tri: &[u32; 3], f: impl Fn(usize) -> T) -> [T; 3] {
    tri.map(|i| f(i as usize))
}

fn to_p(v: [f32; 3]) -> P {
    [f64::from(v[0]), f64::from(v[1]), f64::from(v[2])]
}

fn unit(v: P) -> P {
    let l = length(v);
    if l > 0.0 {
        scale(v, 1.0 / l)
    } else {
        [0.0, 0.0, 1.0]
    }
}

/// Barycentric coordinates of `p` in a triangle, clamped: `p` lies in the triangle up to rounding.
fn barycentric(t: &[P; 3], p: P) -> [f64; 3] {
    let (e0, e1, e2) = (sub(t[1], t[0]), sub(t[2], t[0]), sub(p, t[0]));
    let (d00, d01, d11) = (dot(e0, e0), dot(e0, e1), dot(e1, e1));
    let (d20, d21) = (dot(e2, e0), dot(e2, e1));
    let den = d00 * d11 - d01 * d01;
    if den <= 0.0 {
        return [1.0, 0.0, 0.0];
    }
    let v = ((d11 * d20 - d01 * d21) / den).clamp(0.0, 1.0);
    let w = ((d00 * d21 - d01 * d20) / den).clamp(0.0, 1.0);
    let sum = v + w;
    if sum > 1.0 {
        return [0.0, v / sum, w / sum];
    }
    [1.0 - sum, v, w]
}

/// Geometric unit normal of a triangle (+z for a degenerate one).
fn face_normal(t: &[P; 3]) -> P {
    unit(cross(sub(t[1], t[0]), sub(t[2], t[0])))
}

/// Box projection: the triangle's dominant axis picks the two axes that become U and V.
fn box_uvs(t: &[P; 3], scale_by: f32) -> [[f64; 2]; 3] {
    let n = face_normal(t);
    let (a, b) = if n[0].abs() >= n[1].abs() && n[0].abs() >= n[2].abs() {
        (1, 2)
    } else if n[1].abs() >= n[2].abs() {
        (0, 2)
    } else {
        (0, 1)
    };
    let s = f64::from(scale_by);
    t.map(|p| [p[a] * s, p[b] * s])
}

/// Normals per triangle corner: around each welded vertex, a corner sums the angle-weighted normals
/// of the faces within `crease_degrees` of its own face.
fn crease_normals(tris: &[[P; 3]], welded: &[[u32; 3]], crease_degrees: f32) -> Vec<[P; 3]> {
    let faces: Vec<P> = tris.iter().map(face_normal).collect();
    let angle_at = |t: &[P; 3], i: usize| {
        let (a, b) = (unit(sub(t[(i + 1) % 3], t[i])), unit(sub(t[(i + 2) % 3], t[i])));
        dot(a, b).clamp(-1.0, 1.0).acos()
    };
    let count = welded.iter().flatten().max().map_or(0, |m| *m as usize + 1);
    let mut around: Vec<Vec<(u32, f64)>> = vec![Vec::new(); count];
    for (t, w) in welded.iter().enumerate() {
        for (i, v) in w.iter().enumerate() {
            around[*v as usize].push((t as u32, angle_at(&tris[t], i)));
        }
    }
    let limit = f64::from(crease_degrees).to_radians().cos();
    welded
        .iter()
        .enumerate()
        .map(|(t, w)| {
            w.map(|v| {
                let mut sum = [0.0; 3];
                for (other, angle) in &around[v as usize] {
                    let n = faces[*other as usize];
                    if dot(n, faces[t]) >= limit {
                        sum = add(sum, scale(n, *angle));
                    }
                }
                if length(sum) > 0.0 { unit(sum) } else { faces[t] }
            })
        })
        .collect()
}

/// The planar UVs of a point on a cut plane. The basis comes from the plane's direction with its
/// sign normalized, so both sides of a cut (opposite normals) share it.
fn cap_uv(n: P, p: P, scale_by: f32) -> [f64; 2] {
    let flip = n.iter().find(|c| c.abs() > 1e-12).is_some_and(|c| *c < 0.0);
    let (u, v) = plane_basis(if flip { scale(n, -1.0) } else { n });
    let s = f64::from(scale_by);
    [dot(p, u) * s, dot(p, v) * s]
}

fn f32s(p: P) -> [f32; 3] {
    p.map(|c| c as f32)
}

fn bits(v: f32) -> u32 {
    if v == 0.0 { 0 } else { v.to_bits() }
}

/// Accumulates a chunk's render triangles, sharing vertices that agree bit for bit.
#[derive(Default)]
pub(crate) struct RenderBuilder {
    ids: HashMap<[u32; 8], u32>,
    mesh: ChunkRenderMesh,
}

impl RenderBuilder {
    fn vertex(&mut self, p: [f32; 3], n: [f32; 3], uv: [f32; 2]) -> u32 {
        let key = [
            bits(p[0]),
            bits(p[1]),
            bits(p[2]),
            bits(n[0]),
            bits(n[1]),
            bits(n[2]),
            bits(uv[0]),
            bits(uv[1]),
        ];
        let mesh = &mut self.mesh;
        *self.ids.entry(key).or_insert_with(|| {
            mesh.positions.push(p);
            mesh.normals.push(n);
            mesh.uvs.push(uv);
            (mesh.positions.len() - 1) as u32
        })
    }

    fn push(&mut self, corners: [([f32; 3], [f32; 3], [f32; 2]); 3]) {
        for (p, n, uv) in corners {
            let id = self.vertex(p, n, uv);
            self.mesh.indices.push(id);
        }
    }

    /// Adds a surface piece: its corners read the source triangle.
    pub(crate) fn push_surface(&mut self, t: &Tri, corners: &SourceCorners) {
        self.push(t.v.map(|p| {
            let (n, uv) = corners.at(t.src, p);
            (f32s(p), f32s(n), [uv[0] as f32, uv[1] as f32])
        }));
    }

    /// Adds a cut-face triangle on the plane with unit normal `n`.
    pub(crate) fn push_cap(&mut self, t: &Tri, n: P, uv_scale: f32) {
        self.push(t.v.map(|p| {
            let uv = cap_uv(n, p, uv_scale);
            (f32s(p), f32s(n), [uv[0] as f32, uv[1] as f32])
        }));
    }

    /// The finished mesh, with tangents.
    pub(crate) fn finish(mut self, interior_start: u32) -> Result<ChunkRenderMesh, crate::ImportError> {
        self.mesh.interior_start = interior_start;
        compute_tangents(&mut self.mesh)?;
        Ok(self.mesh)
    }
}

/// Fills [`ChunkRenderMesh::tangents`] from its normals and UVs. Sliver triangles and zero-area UVs
/// leave the generator a zero or default tangent at their vertices, so every tangent is then made
/// perpendicular to its normal (Gram-Schmidt), with an arbitrary perpendicular where nothing is left.
fn compute_tangents(mesh: &mut ChunkRenderMesh) -> Result<(), crate::ImportError> {
    let cpu = regia_asset::CpuMesh {
        positions: std::mem::take(&mut mesh.positions),
        normals: std::mem::take(&mut mesh.normals),
        uvs: std::mem::take(&mut mesh.uvs),
        indices: std::mem::take(&mut mesh.indices),
        ..Default::default()
    };
    let tangents = crate::gltf_import::generate_mikkt_tangents(&cpu);
    mesh.positions = cpu.positions;
    mesh.normals = cpu.normals;
    mesh.uvs = cpu.uvs;
    mesh.indices = cpu.indices;
    mesh.tangents = tangents?
        .into_iter()
        .zip(&mesh.normals)
        .map(|(t, n)| perpendicular(t, to_p(*n)))
        .collect();
    Ok(())
}

/// `t` made unit and perpendicular to the unit `n`, keeping its handedness.
fn perpendicular(t: [f32; 4], n: P) -> [f32; 4] {
    let v = to_p([t[0], t[1], t[2]]);
    let on_plane = sub(v, scale(n, dot(v, n)));
    let xyz = if length(on_plane) > 1e-4 {
        unit(on_plane)
    } else {
        plane_basis(n).0
    };
    [
        xyz[0] as f32,
        xyz[1] as f32,
        xyz[2] as f32,
        if t[3] < 0.0 { -1.0 } else { 1.0 },
    ]
}

/// Makes chunk `index`'s render mesh from its welded mesh alone (a v1 set: the source mesh is
/// gone). Surface pieces get crease-smoothed normals and box-projected UVs; each cut-face triangle
/// its geometric normal and the planar projection. The chunk comes from a file, so its indices are
/// checked first.
pub(crate) fn derive_render_mesh(
    index: usize,
    chunk: &FractureChunk,
    settings: &RenderSettings,
) -> Result<ChunkRenderMesh, FractureError> {
    let vertices = chunk.positions.len();
    if !chunk.indices.len().is_multiple_of(3) || chunk.indices.iter().any(|&i| i as usize >= vertices) {
        return Err(FractureError::Format(format!(
            "chunk {index} has a bad index list"
        )));
    }
    let (triangles, _) = chunk.indices.as_chunks::<3>();
    let frames: Vec<[P; 3]> = triangles
        .iter()
        .map(|t| t.map(|i| to_p(chunk.positions[i as usize])))
        .collect();
    let surface = (chunk.interior_start as usize).min(frames.len());
    let normals = crease_normals(&frames[..surface], &triangles[..surface], settings.crease_degrees);
    let mut render = RenderBuilder::default();
    for (t, frame) in frames.iter().enumerate() {
        if t < surface {
            let uvs = box_uvs(frame, settings.surface_uv_scale);
            let corner = |i: usize| {
                (
                    f32s(frame[i]),
                    f32s(normals[t][i]),
                    [uvs[i][0] as f32, uvs[i][1] as f32],
                )
            };
            render.push([corner(0), corner(1), corner(2)]);
        } else {
            let tri = Tri {
                v: *frame,
                tag: 0,
                src: 0,
            };
            render.push_cap(&tri, face_normal(frame), settings.interior_uv_scale);
        }
    }
    render
        .finish(surface as u32)
        .map_err(|e| FractureError::Tangents(index, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barycentric_finds_corners_edges_and_rounding_outside() {
        let t = [[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 2.0, 0.0]];
        assert_eq!(barycentric(&t, t[0]), [1.0, 0.0, 0.0]);
        assert_eq!(barycentric(&t, t[1]), [0.0, 1.0, 0.0]);
        assert_eq!(barycentric(&t, t[2]), [0.0, 0.0, 1.0]);
        assert_eq!(barycentric(&t, [1.0, 1.0, 0.0]), [0.0, 0.5, 0.5]);
        let w = barycentric(&t, [-1e-12, -1e-12, 0.0]);
        assert!(w.iter().all(|c| (0.0..=1.0).contains(c)));
        assert!((w.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_degenerate_source_triangle_reads_its_first_corner() {
        let t = [[1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 0.0, 0.0]];
        assert_eq!(barycentric(&t, [1.0, 0.0, 0.0]), [1.0, 0.0, 0.0]);
    }

    #[test]
    fn both_sides_of_a_cut_read_the_same_uv() {
        let n = unit([0.3, -0.5, 0.8]);
        let p = [0.7, 1.1, -0.2];
        assert_eq!(cap_uv(n, p, 2.0), cap_uv(scale(n, -1.0), p, 2.0));
    }

    #[test]
    fn cap_uvs_do_not_stretch() {
        let n = unit([1.0, 2.0, -0.5]);
        let (u, v) = plane_basis(n);
        let a = [0.2, 0.3, 0.4];
        let b = add(add(a, scale(u, 0.6)), scale(v, 0.8));
        let (ua, ub) = (cap_uv(n, a, 1.0), cap_uv(n, b, 1.0));
        let d = ((ua[0] - ub[0]).powi(2) + (ua[1] - ub[1]).powi(2)).sqrt();
        assert!(
            (d - 1.0).abs() < 1e-9,
            "one metre on the plane is one UV unit: {d}"
        );
    }

    #[test]
    fn crease_keeps_a_cube_edge_and_smooths_a_shallow_one() {
        // Two triangles sharing the edge (0, 1), 90 degrees apart, then 10 degrees apart.
        let fold = |angle: f64| -> (Vec<[P; 3]>, Vec<[u32; 3]>) {
            let (s, c) = angle.to_radians().sin_cos();
            let (a, b) = ([0.0, 0.0, 0.0], [1.0, 0.0, 0.0]);
            let up = [0.5, 1.0, 0.0];
            let over = [0.5, -c, s];
            (vec![[a, b, up], [b, a, over]], vec![[0, 1, 2], [1, 0, 3]])
        };
        let (t, w) = fold(90.0);
        let n = crease_normals(&t, &w, 40.0);
        assert_eq!(n[0][0], face_normal(&t[0]), "a hard edge stays hard");
        let (t, w) = fold(10.0);
        let n = crease_normals(&t, &w, 40.0);
        assert!(
            dot(n[0][0], face_normal(&t[0])) < 1.0 - 1e-6,
            "a soft edge blends"
        );
        assert!((length(n[0][0]) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_tangent_is_made_perpendicular_or_replaced() {
        let n = [0.0, 0.0, 1.0];
        assert_eq!(perpendicular([2.0, 0.0, 1.0, -1.0], n), [1.0, 0.0, 0.0, -1.0]);
        for t in [[0.0, 0.0, 1.0, 1.0], [0.0; 4]] {
            let p = perpendicular(t, n);
            assert_eq!(dot(to_p([p[0], p[1], p[2]]), n), 0.0);
            assert!((length(to_p([p[0], p[1], p[2]])) - 1.0).abs() < 1e-6);
            assert_eq!(p[3], 1.0);
        }
    }

    #[test]
    fn box_uvs_follow_the_dominant_axis() {
        let t = [[1.0, 2.0, 3.0], [1.0, 3.0, 3.0], [1.0, 2.0, 4.0]];
        assert_eq!(box_uvs(&t, 2.0)[1], [6.0, 6.0]);
        let t = [[1.0, 2.0, 3.0], [2.0, 2.0, 3.0], [1.0, 3.0, 3.0]];
        assert_eq!(box_uvs(&t, 1.0)[2], [1.0, 3.0]);
    }

    #[test]
    fn settings_out_of_range_name_themselves() {
        for (s, word) in [
            (
                RenderSettings {
                    interior_uv_scale: 0.0,
                    ..RenderSettings::default()
                },
                "interior_uv_scale",
            ),
            (
                RenderSettings {
                    surface_uv_scale: f32::NAN,
                    ..RenderSettings::default()
                },
                "surface_uv_scale",
            ),
            (
                RenderSettings {
                    crease_degrees: 200.0,
                    ..RenderSettings::default()
                },
                "crease_degrees",
            ),
        ] {
            assert!(s.check().is_err_and(|e| e.contains(word)), "{word}");
        }
        assert_eq!(RenderSettings::default().check(), Ok(()));
    }
}
