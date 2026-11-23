// SPDX-License-Identifier: MPL-2.0
//! Mesh-surface and skinned-mesh init shapes (`.vfx.ron` v7, [`crate::format::ShapeDef::MeshSurface`]
//! and [`crate::format::ShapeDef::SkinnedMesh`]).
//!
//! A [`VfxMesh`] is built once when the host registers a mesh asset
//! ([`crate::scene::VfxLibrary::add_mesh`]): its triangles, their bind-pose areas in an alias
//! table (Vose, "A Linear Algorithm for Generating Random Numbers with a Given Distribution",
//! IEEE TSE 17(9), 1991) and, for a skinned mesh, the four strongest joint influences per vertex.
//! A spawn then costs O(1): one alias draw picks a triangle with probability proportional to its
//! area, and the square-root barycentric map (Osada, Funkhouser, Chazelle and Dobkin, "Shape
//! Distributions", ACM TOG 21(4), 2002, § 4.2) picks a uniform point on it. A skinned sample
//! blends the triangle's three corners with the current joint palette (linear blend skinning)
//! before the barycentric mix, so the point lies on the deformed triangle.

use regia_core::math::{Mat4, Rng, Vec3};

/// Why a mesh cannot be a spawn shape. The agent protocol reports these as `missing_mesh` and
/// `bad_mesh`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum VfxMeshError {
    /// No mesh was registered under the shape's name.
    #[error("mesh '{0}' is not registered with the vfx library")]
    Missing(String),
    /// The mesh has no triangles.
    #[error("mesh has no triangles")]
    Empty,
    /// Every triangle has zero area (or the positions are not finite).
    #[error("mesh has zero surface area")]
    ZeroArea,
    /// An index points past the vertex list, or the index count is not a multiple of 3.
    #[error(
        "mesh index {index} is out of range for {vertices} vertices (or the index count is not a multiple of 3)"
    )]
    BadIndex {
        /// The offending index (0 for a bad count).
        index: u32,
        /// Vertex count.
        vertices: usize,
    },
    /// A skinned shape names a mesh without joint influences.
    #[error("mesh has no skin")]
    NoSkin,
}

impl VfxMeshError {
    /// The agent-protocol error code: `missing_mesh` for [`Self::Missing`], `bad_mesh` otherwise.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing(_) => "missing_mesh",
            _ => "bad_mesh",
        }
    }
}

/// Vose's alias table: O(n) to build, O(1) per draw.
#[derive(Debug, Clone, PartialEq)]
pub struct AliasTable {
    /// Chance to keep column `i` (else take `alias[i]`).
    prob: Vec<f32>,
    alias: Vec<u32>,
}

impl AliasTable {
    /// Builds the table for `weights` (non-negative). `None` when there is no weight or a weight
    /// is not finite.
    #[must_use]
    pub fn new(weights: &[f64]) -> Option<Self> {
        let n = weights.len();
        let total: f64 = weights.iter().sum();
        if n == 0 || !total.is_finite() || total <= 0.0 || weights.iter().any(|w| !w.is_finite() || *w < 0.0)
        {
            return None;
        }
        let mut scaled: Vec<f64> = weights.iter().map(|w| w * n as f64 / total).collect();
        let mut prob = vec![1.0f32; n];
        let mut alias: Vec<u32> = (0..n as u32).collect();
        let mut small: Vec<usize> = Vec::new();
        let mut large: Vec<usize> = Vec::new();
        for (i, &p) in scaled.iter().enumerate() {
            if p < 1.0 { small.push(i) } else { large.push(i) }
        }
        while let (Some(&s), Some(&l)) = (small.last(), large.last()) {
            small.pop();
            prob[s] = scaled[s] as f32;
            alias[s] = l as u32;
            scaled[l] = (scaled[l] + scaled[s]) - 1.0;
            if scaled[l] < 1.0 {
                large.pop();
                small.push(l);
            }
        }
        // What is left is 1 up to rounding (Vose § III): keep those columns whole.
        for i in large.into_iter().chain(small) {
            prob[i] = 1.0;
        }
        Some(Self { prob, alias })
    }

    /// Columns (the number of weights).
    #[must_use]
    pub fn len(&self) -> usize {
        self.prob.len()
    }

    /// Whether the table is empty (never, for a built table).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prob.is_empty()
    }

    /// One draw: a column, then a coin.
    pub fn sample(&self, rng: &mut Rng) -> usize {
        let n = self.prob.len();
        let column = (rng.next_u32() as usize % n.max(1)).min(n.saturating_sub(1));
        if rng.f32() < self.prob[column] {
            column
        } else {
            self.alias[column] as usize
        }
    }
}

/// The four strongest joint influences of one vertex.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Influence {
    joints: [u32; 4],
    weights: [f32; 4],
}

/// A registered spawn mesh: triangles, an area alias table and (optionally) a skin.
#[derive(Debug, Clone, PartialEq)]
pub struct VfxMesh {
    positions: Vec<Vec3>,
    triangles: Vec<[u32; 3]>,
    table: AliasTable,
    area: f32,
    skin: Option<Vec<Influence>>,
}

/// One surface sample: a point and the surface normal there (object space, or the skinned pose).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceSample {
    /// The point.
    pub position: Vec3,
    /// Unit face normal (`Vec3::Z` for a triangle that skinned flat).
    pub normal: Vec3,
    /// Index of the triangle it lies on.
    pub triangle: u32,
}

impl VfxMesh {
    /// A rigid mesh from positions and a triangle list.
    pub fn from_triangles(positions: &[[f32; 3]], indices: &[u32]) -> Result<Self, VfxMeshError> {
        if indices.is_empty() {
            return Err(VfxMeshError::Empty);
        }
        if !indices.len().is_multiple_of(3) {
            return Err(VfxMeshError::BadIndex {
                index: 0,
                vertices: positions.len(),
            });
        }
        if let Some(&index) = indices.iter().find(|&&i| i as usize >= positions.len()) {
            return Err(VfxMeshError::BadIndex {
                index,
                vertices: positions.len(),
            });
        }
        let positions: Vec<Vec3> = positions.iter().map(|p| Vec3::from_array(*p)).collect();
        let (triangles, areas): (Vec<[u32; 3]>, Vec<f64>) = indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| {
                let [a, b, c] = t.map(|i| positions[i as usize]);
                let area = 0.5 * f64::from((b - a).cross(c - a).length());
                (*t, if area.is_finite() { area } else { 0.0 })
            })
            .unzip();
        let table = AliasTable::new(&areas).ok_or(VfxMeshError::ZeroArea)?;
        let area = areas.iter().sum::<f64>() as f32;
        Ok(Self {
            positions,
            triangles,
            table,
            area,
            skin: None,
        })
    }

    /// A mesh from a loaded mesh asset, with its skin when it has one.
    pub fn from_cpu_mesh(mesh: &regia_asset::CpuMesh) -> Result<Self, VfxMeshError> {
        let mut out = Self::from_triangles(&mesh.positions, &mesh.indices)?;
        if let Some(skin) = &mesh.skin {
            let influences = (0..mesh.positions.len())
                .map(|v| {
                    // A vertex with no weight follows the root joint.
                    let (joints, weights) = skin.top4(v).unwrap_or(([0; 4], [1.0, 0.0, 0.0, 0.0]));
                    Influence { joints, weights }
                })
                .collect();
            out.skin = Some(influences);
        }
        Ok(out)
    }

    /// Adds joint influences (`joints[v]`, `weights[v]` per vertex; weights are normalised) to a
    /// mesh built by [`Self::from_triangles`].
    pub fn with_skin(mut self, joints: &[[u32; 4]], weights: &[[f32; 4]]) -> Result<Self, VfxMeshError> {
        if joints.len() != self.positions.len() || weights.len() != self.positions.len() {
            return Err(VfxMeshError::NoSkin);
        }
        let influences = joints
            .iter()
            .zip(weights)
            .map(|(&joints, &w)| {
                let sum: f32 = w.iter().map(|x| x.max(0.0)).sum();
                let weights = if sum > 0.0 && sum.is_finite() {
                    w.map(|x| x.max(0.0) / sum)
                } else {
                    [1.0, 0.0, 0.0, 0.0]
                };
                Influence { joints, weights }
            })
            .collect();
        self.skin = Some(influences);
        Ok(self)
    }

    /// Triangle count.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.triangles.len()
    }

    /// Total bind-pose surface area, square metres.
    #[must_use]
    pub fn area(&self) -> f32 {
        self.area
    }

    /// Whether the mesh carries joint influences.
    #[must_use]
    pub fn is_skinned(&self) -> bool {
        self.skin.is_some()
    }

    /// Bind-pose vertex positions (object space).
    #[must_use]
    pub fn positions(&self) -> &[Vec3] {
        &self.positions
    }

    /// The triangle list (vertex indices).
    #[must_use]
    pub fn triangles(&self) -> &[[u32; 3]] {
        &self.triangles
    }

    /// A triangle's corners in the bind pose.
    #[must_use]
    pub fn triangle(&self, index: usize) -> Option<[Vec3; 3]> {
        let t = self.triangles.get(index)?;
        Some(t.map(|i| self.positions[i as usize]))
    }

    /// A uniform point on the bind-pose surface.
    pub fn sample(&self, rng: &mut Rng) -> SurfaceSample {
        let triangle = self.table.sample(rng);
        let corners = self.triangles[triangle].map(|i| self.positions[i as usize]);
        barycentric(corners, triangle as u32, rng)
    }

    /// A point on the surface deformed by `palette` (one matrix per joint, bind pose to the
    /// instance frame). A joint past the palette's end, or a mesh without a skin, is the bind
    /// pose. The triangle is picked by bind-pose area.
    pub fn sample_skinned(&self, rng: &mut Rng, palette: &[Mat4]) -> SurfaceSample {
        let triangle = self.table.sample(rng);
        let ids = self.triangles[triangle];
        let corners = ids.map(|i| {
            let p = self.positions[i as usize];
            match &self.skin {
                Some(skin) => skin_point(p, skin[i as usize], palette),
                None => p,
            }
        });
        barycentric(corners, triangle as u32, rng)
    }
}

/// Linear blend skinning of one point.
fn skin_point(p: Vec3, influence: Influence, palette: &[Mat4]) -> Vec3 {
    let mut out = Vec3::ZERO;
    let mut weight = 0.0;
    for (joint, w) in influence.joints.iter().zip(influence.weights) {
        if w <= 0.0 {
            continue;
        }
        let m = palette.get(*joint as usize).copied().unwrap_or(Mat4::IDENTITY);
        out += m.transform_point3(p) * w;
        weight += w;
    }
    if weight > 0.0 { out / weight } else { p }
}

/// A uniform point on a triangle: `(1 - √r1) A + √r1 (1 - r2) B + √r1 r2 C` (Osada et al. 2002).
fn barycentric(corners: [Vec3; 3], triangle: u32, rng: &mut Rng) -> SurfaceSample {
    let [a, b, c] = corners;
    let s = rng.f32().sqrt();
    let r2 = rng.f32();
    let position = a * (1.0 - s) + b * (s * (1.0 - r2)) + c * (s * r2);
    let normal = (b - a).cross(c - a).normalize_or_zero();
    SurfaceSample {
        position,
        normal: if normal == Vec3::ZERO { Vec3::Z } else { normal },
        triangle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_table_reproduces_its_weights() {
        let table = AliasTable::new(&[1.0, 2.0, 0.0, 5.0]).expect("table");
        let mut rng = Rng::new(3);
        let mut counts = [0u32; 4];
        let n = 200_000;
        for _ in 0..n {
            counts[table.sample(&mut rng)] += 1;
        }
        assert_eq!(counts[2], 0, "a zero weight is never drawn");
        for (i, w) in [(0, 1.0), (1, 2.0), (3, 5.0)] {
            let share = counts[i] as f64 / n as f64;
            assert!((share - w / 8.0).abs() < 0.01, "column {i}: {share}");
        }
    }

    #[test]
    fn alias_table_rejects_nothing_to_draw() {
        assert!(AliasTable::new(&[]).is_none());
        assert!(AliasTable::new(&[0.0, 0.0]).is_none());
        assert!(AliasTable::new(&[1.0, f64::NAN]).is_none());
        assert!(AliasTable::new(&[1.0, -1.0]).is_none());
    }

    #[test]
    fn bad_meshes_are_errors_not_panics() {
        assert_eq!(VfxMesh::from_triangles(&[], &[]), Err(VfxMeshError::Empty));
        let flat = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]];
        assert_eq!(
            VfxMesh::from_triangles(&flat, &[0, 1, 2]),
            Err(VfxMeshError::ZeroArea)
        );
        assert!(matches!(
            VfxMesh::from_triangles(&flat, &[0, 1, 7]),
            Err(VfxMeshError::BadIndex { index: 7, .. })
        ));
        assert!(matches!(
            VfxMesh::from_triangles(&flat, &[0, 1]),
            Err(VfxMeshError::BadIndex { .. })
        ));
        assert_eq!(VfxMeshError::Missing("x".into()).code(), "missing_mesh");
        assert_eq!(VfxMeshError::ZeroArea.code(), "bad_mesh");
    }
}
