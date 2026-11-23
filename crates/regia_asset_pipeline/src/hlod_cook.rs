// SPDX-License-Identifier: MPL-2.0
//! HLOD proxy meshes (`docs/specs/78-world-streaming.md` § 11.1): the static meshes of a cell merged
//! into one mesh and simplified with the meshoptimizer simplifier, and parents built from their
//! children's proxies the same way. Proxies are ordinary cooked meshes with no collision and no nav.
//!
//! Simplification follows Garland and Heckbert's quadric error metric through meshoptimizer
//! (A. Kapoulkine, MIT). A cell of many small disjoint props cannot reach the target by edge
//! collapses inside each prop, so the sloppy simplifier finishes the job, which merges across gaps.

use std::collections::HashMap;

use meshopt::{SimplifyOptions, simplify_decoder, simplify_sloppy_decoder};
use regia_asset::{
    AssetId, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CpuMesh, encode_cooked_file,
    pack_importer_version,
};
use regia_core::math::{Affine3A, Vec3, Vec3A};

use crate::mesh_cook::{cook_mesh, pack_cooked_mesh};

/// The base color a proxy takes for triangles whose source named no color (a `MeshSource` whose
/// material the cook does not read): the default material's.
pub const UNKNOWN_PROXY_COLOR: [f32; 4] = [0.8, 0.8, 0.8, 1.0];

/// Triangles in world or object space: positions, normals and optional linear base colors.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MeshSoup {
    /// Vertex positions.
    pub positions: Vec<[f32; 3]>,
    /// Vertex normals, one per position.
    pub normals: Vec<[f32; 3]>,
    /// Linear RGBA base color per vertex, or empty when no source named one.
    pub colors: Vec<[f32; 4]>,
    /// Triangle list.
    pub indices: Vec<u32>,
    /// Texture coordinates, one per position, or empty when unknown. The atlas bake samples the
    /// source material with them; a baked proxy carries its atlas UVs here (ADR-2483). Merging,
    /// welding and simplification drop them.
    pub uvs: Vec<[f32; 2]>,
    /// The source material the atlas bake samples, or `None` (vertex colors only). Merging drops it.
    pub material: Option<std::sync::Arc<crate::hlod_atlas::BakeMaterial>>,
}

impl MeshSoup {
    /// A built-in `MeshSource` mesh, or `None` for a name this does not know.
    pub fn builtin(name: &str) -> Option<Self> {
        use regia_render::primitives::{box_mesh, uv_sphere};
        let (vertices, indices) = match name {
            regia_render::MeshSource::BUILTIN_CUBE => box_mesh([0.5; 3]),
            regia_render::MeshSource::BUILTIN_SPHERE => uv_sphere(0.5, 16, 8),
            regia_render::MeshSource::BUILTIN_PLANE => {
                return Some(Self {
                    positions: vec![
                        [-0.5, 0.0, -0.5],
                        [-0.5, 0.0, 0.5],
                        [0.5, 0.0, 0.5],
                        [0.5, 0.0, -0.5],
                    ],
                    normals: vec![[0.0, 1.0, 0.0]; 4],
                    colors: Vec::new(),
                    indices: vec![0, 1, 2, 0, 2, 3],
                    uvs: vec![[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]],
                    material: None,
                });
            }
            _ => return None,
        };
        Some(Self {
            positions: vertices.iter().map(|v| v.position).collect(),
            normals: vertices.iter().map(|v| v.normal).collect(),
            colors: Vec::new(),
            uvs: vertices.iter().map(|v| v.uv).collect(),
            indices,
            material: None,
        })
    }

    /// A soup of a built-in primitive mesh.
    pub fn from_primitive(mesh: &regia_render::CpuMesh) -> Self {
        Self {
            positions: mesh.vertices.iter().map(|v| v.position).collect(),
            normals: mesh.vertices.iter().map(|v| v.normal).collect(),
            colors: Vec::new(),
            indices: mesh.indices.clone(),
            uvs: mesh.vertices.iter().map(|v| v.uv).collect(),
            material: None,
        }
    }

    /// The same triangles drawn with `material` by the atlas bake (a `MeshSource`'s material).
    #[must_use]
    pub fn with_material(
        mut self,
        material: Option<std::sync::Arc<crate::hlod_atlas::BakeMaterial>>,
    ) -> Self {
        self.material = material;
        self
    }

    /// The same triangles with every vertex in `color` (a `Primitive`'s base color).
    #[must_use]
    pub fn tinted(mut self, color: [f32; 4]) -> Self {
        self.colors = vec![color; self.positions.len()];
        self
    }

    /// The area-weighted mean base color of the triangles, or `None` when no vertex has a color.
    /// A triangle's color is the mean of its vertices'. Deterministic: summed in triangle order.
    pub fn mean_color(&self) -> Option<[f32; 4]> {
        if self.colors.len() != self.positions.len() || self.colors.is_empty() {
            return None;
        }
        let mut sum = [0.0f64; 4];
        let mut total = 0.0f64;
        for [a, b, c] in self.indices.as_chunks::<3>().0 {
            let [pa, pb, pc] = [*a, *b, *c].map(|i| Vec3::from_array(self.positions[i as usize]));
            let area = f64::from((pb - pa).cross(pc - pa).length() * 0.5);
            if area <= 0.0 {
                continue;
            }
            for (channel, slot) in sum.iter_mut().enumerate() {
                let mean = [*a, *b, *c]
                    .iter()
                    .map(|i| f64::from(self.colors[*i as usize][channel]))
                    .sum::<f64>()
                    / 3.0;
                *slot += mean * area;
            }
            total += area;
        }
        (total > 0.0).then(|| sum.map(|v| (v / total) as f32))
    }

    /// A soup of a cooked mesh's LOD 0.
    pub fn from_cpu(mesh: &CpuMesh) -> Self {
        Self {
            positions: mesh.positions.clone(),
            normals: mesh.normals.clone(),
            colors: Vec::new(),
            indices: mesh.indices.clone(),
            uvs: if mesh.uvs.len() == mesh.positions.len() {
                mesh.uvs.clone()
            } else {
                Vec::new()
            },
            material: None,
        }
    }

    /// Triangle count.
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }

    /// Whether it holds no triangle.
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// The bounds `[min, max]`, or `None` with no vertex.
    pub fn bounds(&self) -> Option<[[f32; 3]; 2]> {
        let first = Vec3::from_array(*self.positions.first()?);
        let (min, max) = self.positions.iter().fold((first, first), |(lo, hi), p| {
            let p = Vec3::from_array(*p);
            (lo.min(p), hi.max(p))
        });
        Some([min.to_array(), max.to_array()])
    }

    /// Appends `other`, moved by `world`. The merged soup keeps no UVs and no material: proxies are
    /// merged geometry, and the atlas bake reads the sources one by one.
    pub fn append(&mut self, other: &MeshSoup, world: &Affine3A) {
        self.uvs.clear();
        self.material = None;
        let base = self.positions.len() as u32;
        // Colors stay one per vertex: once either side has some, the other side's vertices take
        // the unknown color.
        let other_colored = other.colors.len() == other.positions.len() && !other.colors.is_empty();
        if other_colored || !self.colors.is_empty() {
            self.colors.resize(self.positions.len(), UNKNOWN_PROXY_COLOR);
            if other_colored {
                self.colors.extend_from_slice(&other.colors);
            } else {
                self.colors
                    .extend(std::iter::repeat_n(UNKNOWN_PROXY_COLOR, other.positions.len()));
            }
        }
        let rotate = world.matrix3.inverse().transpose();
        self.positions.extend(
            other
                .positions
                .iter()
                .map(|p| Vec3::from(world.transform_point3a(Vec3A::from(Vec3::from_array(*p)))).to_array()),
        );
        self.normals.extend(other.normals.iter().map(|n| {
            Vec3::from((rotate * Vec3A::from(Vec3::from_array(*n))).normalize_or_zero()).to_array()
        }));
        self.indices.extend(other.indices.iter().map(|i| i + base));
    }

    /// The same triangles moved by `-offset`.
    pub fn relative_to(&self, offset: [f64; 3]) -> Self {
        let mut out = self.clone();
        for p in &mut out.positions {
            *p = std::array::from_fn(|a| (f64::from(p[a]) - offset[a]) as f32);
        }
        out
    }

    /// Welds vertices at the same position, so simplification sees connected surfaces, and drops
    /// degenerate triangles. Normals average over the merged vertices.
    fn welded(&self) -> Self {
        let mut remap: HashMap<[u32; 3], u32> = HashMap::with_capacity(self.positions.len());
        let mut out = Self::default();
        let mut sums: Vec<Vec3> = Vec::new();
        let colored = self.colors.len() == self.positions.len() && !self.colors.is_empty();
        let mut color_sums: Vec<([f32; 4], u32)> = Vec::new();
        let mut map = Vec::with_capacity(self.positions.len());
        for (vertex, (position, normal)) in self.positions.iter().zip(&self.normals).enumerate() {
            let key = position.map(f32::to_bits);
            let index = *remap.entry(key).or_insert_with(|| {
                out.positions.push(*position);
                sums.push(Vec3::ZERO);
                color_sums.push(([0.0; 4], 0));
                out.positions.len() as u32 - 1
            });
            sums[index as usize] += Vec3::from_array(*normal);
            if colored {
                let (sum, count) = &mut color_sums[index as usize];
                for (slot, value) in sum.iter_mut().zip(self.colors[vertex]) {
                    *slot += value;
                }
                *count += 1;
            }
            map.push(index);
        }
        out.normals = sums.iter().map(|n| n.normalize_or_zero().to_array()).collect();
        if colored {
            out.colors = color_sums
                .iter()
                .map(|(sum, count)| sum.map(|v| v / (*count).max(1) as f32))
                .collect();
        }
        for [a, b, c] in self.indices.as_chunks::<3>().0 {
            let (a, b, c) = (map[*a as usize], map[*b as usize], map[*c as usize]);
            if a != b && b != c && a != c {
                out.indices.extend_from_slice(&[a, b, c]);
            }
        }
        out
    }

    /// Simplifies to about `ratio` of the triangles and at least `min_triangles`, with unused
    /// vertices dropped. Deterministic.
    pub fn simplified(&self, ratio: f32, min_triangles: u32) -> Self {
        let welded = self.welded();
        let triangles = welded.triangles();
        let target = (((triangles as f32) * ratio.clamp(0.0, 1.0)).ceil() as usize)
            .max(min_triangles as usize)
            .min(triangles);
        if target >= triangles || welded.positions.is_empty() {
            return welded;
        }
        let mut indices = simplify_decoder(
            &welded.indices,
            &welded.positions,
            target * 3,
            f32::MAX,
            SimplifyOptions::None,
            None,
        );
        if indices.len() > target * 3 {
            indices = simplify_sloppy_decoder(&welded.indices, &welded.positions, target * 3, f32::MAX, None);
        }
        welded.with_indices(indices)
    }

    /// The vertices `indices` use, renumbered.
    fn with_indices(&self, indices: Vec<u32>) -> Self {
        let mut remap = vec![u32::MAX; self.positions.len()];
        let mut out = Self::default();
        for index in indices {
            let slot = &mut remap[index as usize];
            if *slot == u32::MAX {
                *slot = out.positions.len() as u32;
                out.positions.push(self.positions[index as usize]);
                out.normals.push(self.normals[index as usize]);
                if let Some(color) = self.colors.get(index as usize) {
                    out.colors.push(*color);
                }
            }
            out.indices.push(*slot);
        }
        out
    }

    /// The cooked mesh file of this soup, as asset `id`.
    pub fn to_cooked_file(&self, id: AssetId) -> Result<Vec<u8>, String> {
        let cpu = CpuMesh {
            positions: self.positions.clone(),
            normals: self.normals.clone(),
            // A baked proxy writes its atlas UVs; a flat one writes zeros as it always did.
            uvs: if self.uvs.len() == self.positions.len() {
                self.uvs.clone()
            } else {
                vec![[0.0; 2]; self.positions.len()]
            },
            indices: self.indices.clone(),
            ..CpuMesh::default()
        };
        let cooked = pack_cooked_mesh(&cook_mesh(cpu), None).map_err(|e| e.to_string())?;
        let payload = cooked.to_postcard().map_err(|e| e.to_string())?;
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh().0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        encode_cooked_file(&header, &payload).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::primitives::{Primitive, PrimitiveShape};

    fn cube() -> MeshSoup {
        let primitive = Primitive::new(PrimitiveShape::default_of("cuboid").expect("a known shape"));
        MeshSoup::from_primitive(&primitive.shape.mesh().expect("mesh"))
    }

    fn grid_of_cubes(n: u32) -> MeshSoup {
        let unit = cube();
        let mut out = MeshSoup::default();
        for x in 0..n {
            for z in 0..n {
                let at = Affine3A::from_translation(Vec3::new(x as f32 * 3.0, 0.0, z as f32 * 3.0));
                out.append(&unit, &at);
            }
        }
        out
    }

    #[test]
    fn append_moves_positions_and_keeps_indices_in_range() {
        let unit = cube();
        let mut soup = MeshSoup::default();
        soup.append(&unit, &Affine3A::from_translation(Vec3::new(10.0, 0.0, 0.0)));
        soup.append(&unit, &Affine3A::IDENTITY);
        assert_eq!(soup.triangles(), unit.triangles() * 2);
        assert!(soup.indices.iter().all(|i| (*i as usize) < soup.positions.len()));
        let [min, max] = soup.bounds().expect("bounds");
        let [_, unit_max] = unit.bounds().expect("bounds");
        assert_eq!(max[0], 10.0 + unit_max[0]);
        assert!(min[0] < 0.0);
    }

    #[test]
    fn simplification_of_disjoint_props_reaches_the_target_and_keeps_the_extent() {
        let soup = grid_of_cubes(14);
        let total = soup.triangles();
        let proxy = soup.simplified(0.05, 64);
        assert!(
            proxy.triangles() <= (total / 20).max(64) + 12,
            "{} of {total}",
            proxy.triangles()
        );
        assert!(proxy.triangles() >= 1);
        assert!(
            proxy
                .indices
                .iter()
                .all(|i| (*i as usize) < proxy.positions.len())
        );
        let [lo, hi] = soup.bounds().expect("bounds");
        let [plo, phi] = proxy.bounds().expect("bounds");
        for a in 0..3 {
            assert!(plo[a] >= lo[a] && phi[a] <= hi[a], "inside the source");
        }
    }

    #[test]
    fn a_soup_below_the_minimum_stays_whole() {
        let soup = cube();
        assert_eq!(soup.simplified(0.05, 64).triangles(), soup.triangles());
    }

    #[test]
    fn the_same_input_simplifies_to_the_same_output() {
        let soup = grid_of_cubes(8);
        assert_eq!(soup.simplified(0.1, 16), soup.simplified(0.1, 16));
    }

    #[test]
    fn colors_survive_merge_and_simplification_and_average_by_area() {
        let red = cube().tinted([1.0, 0.0, 0.0, 1.0]);
        // A plain cube three times as wide: nine times the area of the red one, in the unknown color.
        let mut soup = MeshSoup::default();
        soup.append(&red, &Affine3A::IDENTITY);
        soup.append(
            &cube(),
            &Affine3A::from_scale_rotation_translation(
                Vec3::splat(3.0),
                regia_core::math::Quat::IDENTITY,
                Vec3::new(10.0, 0.0, 0.0),
            ),
        );
        assert_eq!(soup.colors.len(), soup.positions.len());
        let mean = soup.mean_color().expect("colored");
        let expect_r = (1.0 + 9.0 * UNKNOWN_PROXY_COLOR[0]) / 10.0;
        assert!((mean[0] - expect_r).abs() < 1e-4, "{mean:?}");
        assert!(
            (mean[1] - 9.0 * UNKNOWN_PROXY_COLOR[1] / 10.0).abs() < 1e-4,
            "{mean:?}"
        );
        // Welding and simplification keep one color per vertex.
        let simplified = grid_of_cubes(6).tinted([0.2, 0.4, 0.6, 1.0]).simplified(0.1, 8);
        assert_eq!(simplified.colors.len(), simplified.positions.len());
        let mean = simplified.mean_color().expect("colored");
        for (got, want) in mean.iter().zip([0.2, 0.4, 0.6, 1.0]) {
            assert!((got - want).abs() < 1e-5, "{mean:?}");
        }
        // No source color, no proxy color.
        assert_eq!(cube().mean_color(), None);
        assert_eq!(MeshSoup::default().mean_color(), None);
    }

    #[test]
    fn a_proxy_cooks_to_a_mesh_the_loader_reads() {
        let id = AssetId::new_random();
        let soup = grid_of_cubes(2).relative_to([1.0, 0.0, 0.0]);
        let bytes = soup.to_cooked_file(id).expect("cook");
        let loaded = regia_asset::load_mesh_bytes(&bytes).expect("load");
        assert_eq!(loaded.mesh.indices.len(), soup.indices.len());
    }
}
