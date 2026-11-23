// SPDX-License-Identifier: MPL-2.0
//! Procedural stand-in rocks: seeded, faceted boulders with three LODs that share one
//! silhouette, for tests, evidence and maps without authored rock meshes. Authored glTF rocks
//! replace them through the `.scatter.ron` mesh list without engine changes.
//!
//! Shape: an icosphere (Loop-free midpoint subdivision of the icosahedron) whose radius per
//! direction is the distance to the nearest of a few random cutting planes — the intersection of
//! half-spaces, i.e. a convex polyhedron, which reads as a fractured block — softened by a
//! sphere, roughened by 3D value noise, squashed vertically, and with a flattened base. The pivot
//! is the centre of the base (y = 0 at ground contact), +Y up, nominal size about one metre.
//!
//! Vertex colour carries a baked cavity term (concave and lower regions darker) and a mottled
//! mineral variation, so the stand-in reads as stone under the standard lit material.

use regia_core::math::Rng;
use regia_render::{LabMaterial, LabVertex};

use super::field::{ScatterMeshAsset, ScatterMeshLod};

/// Shape family of a procedural rock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RockStyle {
    /// Number of cutting planes (more = more facets).
    pub cuts: u32,
    /// Plane distance range (closer = deeper cuts, more angular).
    pub cut_depth: [f32; 2],
    /// Height / width ratio.
    pub flatness: f32,
    /// 3D noise amplitude (fraction of the radius).
    pub roughness: f32,
    /// Linear base colour of the material.
    pub color: [f32; 3],
    /// Perceptual roughness of the material.
    pub material_roughness: f32,
    /// Strength of horizontal bedding bands in the vertex colour (sandstone; 0 = none).
    pub banding: f32,
}

impl RockStyle {
    /// Fractured sandstone / basalt blocks (Mars).
    #[must_use]
    pub fn angular(color: [f32; 3]) -> Self {
        Self {
            cuts: 12,
            cut_depth: [0.38, 0.8],
            flatness: 0.62,
            roughness: 0.05,
            color,
            material_roughness: 0.88,
            banding: 0.0,
        }
    }

    /// Weathered, rounder boulders (ice and frosted rock on Europa).
    #[must_use]
    pub fn rounded(color: [f32; 3]) -> Self {
        Self {
            cuts: 9,
            cut_depth: [0.5, 0.88],
            flatness: 0.66,
            roughness: 0.09,
            color,
            material_roughness: 0.7,
            banding: 0.0,
        }
    }
}

/// Stand-in rock palettes: what a map's rocks look like before its authored glTF rocks
/// exist. Colours are linear base colour (albedo); the vertex cavity/mottle term varies them
/// by about ±20 %.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RockPalette {
    /// Mars: brown sandstone (red:green ≈ 1.5) at about half the dust's albedo (0.065–0.095 in
    /// red; Mars dust is ~0.25), so even sun-facing faces print darker than the ground; some
    /// near-black basalt, a few flat slabs.
    Mars,
    /// Europa: frosted ice blocks and dark basalt chunks.
    Europa,
}

impl RockPalette {
    /// The style of the `index`-th distinct mesh of a set (cycled).
    #[must_use]
    pub fn style(self, index: usize) -> RockStyle {
        match self {
            Self::Mars => {
                let style = match index % 5 {
                    0 => RockStyle {
                        banding: 0.12,
                        ..RockStyle::angular([0.08, 0.052, 0.034])
                    },
                    1 => RockStyle::angular([0.065, 0.044, 0.031]),
                    2 => RockStyle {
                        flatness: 0.42,
                        banding: 0.1,
                        ..RockStyle::angular([0.095, 0.064, 0.042])
                    },
                    3 => RockStyle::angular([0.035, 0.032, 0.03]),
                    _ => RockStyle::rounded([0.075, 0.05, 0.034]),
                };
                // Dusty, matte stone: no sheen against a low sun.
                RockStyle {
                    material_roughness: 0.95,
                    ..style
                }
            }
            Self::Europa => match index % 3 {
                0 => RockStyle::angular([0.46, 0.49, 0.53]),
                1 => RockStyle::rounded([0.50, 0.52, 0.56]),
                _ => RockStyle::angular([0.07, 0.07, 0.08]),
            },
        }
    }
}

/// Procedural stand-ins for every mesh path `set` names, styled by `palette` (paths in
/// first-use order; the same set always gives the same rocks).
#[must_use]
pub fn standin_library(set: &super::ScatterSet, palette: RockPalette) -> super::field::ScatterLibrary {
    let mut library = super::field::ScatterLibrary::new();
    for (index, path) in set.mesh_paths().into_iter().enumerate() {
        let seed = 0x0057_0a4d_0000_0000 ^ (index as u64 + 1).wrapping_mul(0x9e37_79b9);
        library.insert(path.to_string(), procedural_rock(seed, palette.style(index)));
    }
    library
}

/// Icosphere subdivisions of each LOD (1280, 320, 80 and 20 triangles).
pub const ROCK_LOD_SUBDIVISIONS: [u32; 4] = [3, 2, 1, 0];

/// Builds one seeded rock with four LODs, 1 m across at scale 1 (the `.scatter.ron` mesh
/// convention: pivot at the base centre, +Y up, ~1 m nominal; the class scale is the size in
/// metres).
#[must_use]
pub fn procedural_rock(seed: u64, style: RockStyle) -> ScatterMeshAsset {
    let mut rng = Rng::new(seed ^ 0x0052_4f43_4b5f_5631);
    let planes: Vec<([f32; 3], f32)> = (0..style.cuts)
        .map(|_| {
            let n = rng.unit_vector();
            let d = style.cut_depth[0] + (style.cut_depth[1] - style.cut_depth[0]) * rng.f32();
            ([n.x, n.y, n.z], d)
        })
        .collect();
    let stretch = [0.85 + 0.3 * rng.f32(), 0.85 + 0.3 * rng.f32()];
    let noise_seed = rng.next_u32();
    let mottle_seed = rng.next_u32();
    let shape = Shape {
        planes,
        stretch,
        flatness: style.flatness,
        roughness: style.roughness,
        noise_seed,
    };
    // One scale for every LOD: the widest horizontal extent of the shape is 1 m.
    let (dirs, _) = icosphere(3);
    let widest = dirs
        .iter()
        .map(|d| {
            let p = shape.point(*d);
            (p[0] * p[0] + p[2] * p[2]).sqrt()
        })
        .fold(1e-3f32, f32::max);
    let unit = 0.5 / widest;
    let lods = ROCK_LOD_SUBDIVISIONS
        .iter()
        .enumerate()
        .map(|(lod, &subdivisions)| build_lod(&shape, subdivisions, mottle_seed, style.banding, lod, unit))
        .collect();
    ScatterMeshAsset {
        lods,
        material: LabMaterial {
            base_color: [style.color[0], style.color[1], style.color[2], 1.0],
            roughness: style.material_roughness,
            metallic: 0.0,
            ..LabMaterial::default()
        },
        parts: Vec::new(),
        shadow_lod: 0,
    }
}

struct Shape {
    planes: Vec<([f32; 3], f32)>,
    stretch: [f32; 2],
    flatness: f32,
    roughness: f32,
    noise_seed: u32,
}

impl Shape {
    /// Surface point for unit direction `d` (before the base flattening).
    fn point(&self, d: [f32; 3]) -> [f32; 3] {
        let mut r = 1.0f32;
        for (n, dist) in &self.planes {
            let c = d[0] * n[0] + d[1] * n[1] + d[2] * n[2];
            if c > 1e-3 {
                r = r.min(dist / c);
            }
        }
        // Soften the corners slightly toward a sphere so edges are bevelled, not razor sharp.
        let r = 0.93 * r + 0.07 * r.sqrt();
        let p = [d[0] * 2.3, d[1] * 2.3, d[2] * 2.3];
        let n = crate::noise::value_noise3(p[0], p[1], p[2], self.noise_seed) * 0.65
            + crate::noise::value_noise3(p[0] * 2.7, p[1] * 2.7, p[2] * 2.7, self.noise_seed ^ 0x9e37) * 0.35;
        let r = r * (1.0 + (n - 0.5) * 2.0 * self.roughness);
        [
            d[0] * r * self.stretch[0],
            d[1] * r * self.flatness,
            d[2] * r * self.stretch[1],
        ]
    }
}

/// Normals are shared across edges flatter than this (cosine of 38°); sharper edges split.
const CREASE_COS: f32 = 0.788;

fn build_lod(
    shape: &Shape,
    subdivisions: u32,
    mottle_seed: u32,
    banding: f32,
    lod: usize,
    unit: f32,
) -> ScatterMeshLod {
    let (sphere_dirs, sphere_indices) = icosphere(subdivisions);
    let mut sphere_positions: Vec<[f32; 3]> = sphere_dirs.iter().map(|d| shape.point(*d)).collect();
    // Flatten the base: everything below 30% of the lower half sits on one plane, then move
    // the pivot to the centre of that base.
    let min_y = sphere_positions
        .iter()
        .map(|p| p[1])
        .fold(f32::INFINITY, f32::min);
    let base = min_y * 0.7;
    for p in &mut sphere_positions {
        p[1] = p[1].max(base) - base;
        *p = p.map(|c| c * unit);
    }
    let (source, normals, indices) = crease_split(&sphere_positions, &sphere_indices, CREASE_COS);
    let positions: Vec<[f32; 3]> = source.iter().map(|&v| sphere_positions[v as usize]).collect();
    let dirs: Vec<[f32; 3]> = source.iter().map(|&v| sphere_dirs[v as usize]).collect();
    let height = positions.iter().map(|p| p[1]).fold(0.0f32, f32::max).max(1e-3);
    let vertices = positions
        .iter()
        .zip(&normals)
        .zip(&dirs)
        .map(|((p, n), d)| {
            // Cavity: the shape point is inside the softened sphere where it was cut deepest;
            // concave noise dips are darker. Lower regions get less sky.
            let radial = (p[0] * p[0] + (p[1] - height * 0.5) * (p[1] - height * 0.5) + p[2] * p[2]).sqrt();
            let cavity = (radial / (0.9 * unit)).clamp(0.6, 1.0);
            let sky = 0.72 + 0.28 * (p[1] / height).clamp(0.0, 1.0);
            // Mineral mottling at two scales, and bedding bands for sandstone.
            let m = crate::noise::value_noise3(d[0] * 3.1, d[1] * 3.1, d[2] * 3.1, mottle_seed) * 0.6
                + crate::noise::value_noise3(d[0] * 9.0, d[1] * 9.0, d[2] * 9.0, mottle_seed ^ 0x51ed) * 0.4;
            let mottle = 0.8 + 0.34 * m;
            let band = 1.0 - banding * (0.5 + 0.5 * (p[1] / unit * 31.0 + m * 2.0).sin());
            let k = (cavity * sky * mottle * band).clamp(0.3, 1.1);
            // Slightly warmer in the cavities (dust), cooler on exposed faces.
            let colour = [k * 1.02, k, k * 0.97, 1.0];
            let t = tangent_for(*n);
            let mut v = LabVertex::from_pos_normal_uv(*p, *n, [d[0] * 0.5 + 0.5, d[2] * 0.5 + 0.5]);
            v.tangent = t;
            v.color = colour;
            v
        })
        .collect();
    ScatterMeshLod {
        vertices,
        indices,
        screen_size: [0.06, 0.018, 0.003, 0.0][lod.min(3)],
    }
}

fn tangent_for(n: [f32; 3]) -> [f32; 4] {
    // Any unit vector orthogonal to n (the stand-in has no normal map).
    let a = if n[1].abs() < 0.9 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let t = [
        a[1] * n[2] - a[2] * n[1],
        a[2] * n[0] - a[0] * n[2],
        a[0] * n[1] - a[1] * n[0],
    ];
    let len = (t[0] * t[0] + t[1] * t[1] + t[2] * t[2]).sqrt().max(1e-6);
    [t[0] / len, t[1] / len, t[2] / len, 1.0]
}

/// Crease-aware normals: each triangle corner averages the (area-weighted) normals of the
/// faces around its vertex that are within the crease angle of the triangle itself, and corners
/// with different normals become separate vertices. Planar cut faces shade flat with crisp
/// edges between them; noisy regions stay smooth.
///
/// Returns (source vertex of each output vertex, output normals, output indices).
fn crease_split(
    positions: &[[f32; 3]],
    indices: &[u32],
    crease_cos: f32,
) -> (Vec<u32>, Vec<[f32; 3]>, Vec<u32>) {
    let tris = indices.as_chunks::<3>().0;
    let face: Vec<[f32; 3]> = tris
        .iter()
        .map(|tri| {
            let [pa, pb, pc] = tri.map(|i| positions[i as usize]);
            let e1 = [pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]];
            let e2 = [pc[0] - pa[0], pc[1] - pa[1], pc[2] - pa[2]];
            [
                e1[1] * e2[2] - e1[2] * e2[1],
                e1[2] * e2[0] - e1[0] * e2[2],
                e1[0] * e2[1] - e1[1] * e2[0],
            ]
        })
        .collect();
    let unit = |n: [f32; 3]| {
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len > 1e-12 {
            [n[0] / len, n[1] / len, n[2] / len]
        } else {
            [0.0, 1.0, 0.0]
        }
    };
    let mut around: Vec<Vec<usize>> = vec![Vec::new(); positions.len()];
    for (t, tri) in tris.iter().enumerate() {
        for &v in tri {
            around[v as usize].push(t);
        }
    }
    let mut source = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut out = Vec::with_capacity(indices.len());
    let mut seen: std::collections::HashMap<(u32, [i16; 3]), u32> = std::collections::HashMap::new();
    for (t, tri) in tris.iter().enumerate() {
        let nt = unit(face[t]);
        for &v in tri {
            let mut sum = [0.0f32; 3];
            for &f in &around[v as usize] {
                let nf = unit(face[f]);
                if nf[0] * nt[0] + nf[1] * nt[1] + nf[2] * nt[2] >= crease_cos {
                    for k in 0..3 {
                        sum[k] += face[f][k];
                    }
                }
            }
            let n = unit(sum);
            let key = (v, n.map(|c| (c * 4096.0).round() as i16));
            let index = *seen.entry(key).or_insert_with(|| {
                source.push(v);
                normals.push(n);
                (normals.len() - 1) as u32
            });
            out.push(index);
        }
    }
    (source, normals, out)
}

/// Unit icosphere: vertex directions and counter-clockwise (outward) triangle indices.
fn icosphere(subdivisions: u32) -> (Vec<[f32; 3]>, Vec<u32>) {
    let t = (1.0 + 5.0f32.sqrt()) * 0.5;
    let mut verts: Vec<[f32; 3]> = [
        [-1.0, t, 0.0],
        [1.0, t, 0.0],
        [-1.0, -t, 0.0],
        [1.0, -t, 0.0],
        [0.0, -1.0, t],
        [0.0, 1.0, t],
        [0.0, -1.0, -t],
        [0.0, 1.0, -t],
        [t, 0.0, -1.0],
        [t, 0.0, 1.0],
        [-t, 0.0, -1.0],
        [-t, 0.0, 1.0],
    ]
    .iter()
    .map(|v| normalised(*v))
    .collect();
    let mut faces: Vec<[u32; 3]> = vec![
        [0, 11, 5],
        [0, 5, 1],
        [0, 1, 7],
        [0, 7, 10],
        [0, 10, 11],
        [1, 5, 9],
        [5, 11, 4],
        [11, 10, 2],
        [10, 7, 6],
        [7, 1, 8],
        [3, 9, 4],
        [3, 4, 2],
        [3, 2, 6],
        [3, 6, 8],
        [3, 8, 9],
        [4, 9, 5],
        [2, 4, 11],
        [6, 2, 10],
        [8, 6, 7],
        [9, 8, 1],
    ];
    for _ in 0..subdivisions {
        let mut midpoints = std::collections::HashMap::new();
        let mut next = Vec::with_capacity(faces.len() * 4);
        let mut mid = |a: u32, b: u32, verts: &mut Vec<[f32; 3]>| -> u32 {
            let key = (a.min(b), a.max(b));
            *midpoints.entry(key).or_insert_with(|| {
                let (pa, pb) = (verts[a as usize], verts[b as usize]);
                verts.push(normalised([
                    (pa[0] + pb[0]) * 0.5,
                    (pa[1] + pb[1]) * 0.5,
                    (pa[2] + pb[2]) * 0.5,
                ]));
                (verts.len() - 1) as u32
            })
        };
        for [a, b, c] in faces {
            let ab = mid(a, b, &mut verts);
            let bc = mid(b, c, &mut verts);
            let ca = mid(c, a, &mut verts);
            next.extend([[a, ab, ca], [b, bc, ab], [c, ca, bc], [ab, bc, ca]]);
        }
        faces = next;
    }
    (verts, faces.into_iter().flatten().collect())
}

fn normalised(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    [v[0] / len, v[1] / len, v[2] / len]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lods_shrink_and_share_the_silhouette() {
        let rock = procedural_rock(7, RockStyle::angular([0.4, 0.3, 0.25]));
        assert_eq!(rock.lods.len(), 4);
        let tris: Vec<usize> = rock.lods.iter().map(|l| l.indices.len() / 3).collect();
        assert_eq!(tris, vec![1280, 320, 80, 20]);
        let extent = |lod: &ScatterMeshLod| {
            let mut max = [0.0f32; 3];
            for v in &lod.vertices {
                for (m, p) in max.iter_mut().zip(v.position) {
                    *m = m.max(p.abs());
                }
            }
            max
        };
        let (a, c) = (extent(&rock.lods[0]), extent(&rock.lods[2]));
        for k in 0..3 {
            assert!((a[k] - c[k]).abs() < 0.25 * a[k].max(0.1), "{a:?} vs {c:?}");
        }
    }

    #[test]
    fn pivot_is_the_base_and_normals_face_out() {
        for seed in 0..6 {
            let rock = procedural_rock(seed, RockStyle::rounded([0.7, 0.72, 0.75]));
            let lod = &rock.lods[0];
            let min_y = lod
                .vertices
                .iter()
                .map(|v| v.position[1])
                .fold(f32::INFINITY, f32::min);
            assert!(min_y.abs() < 1e-5, "base at {min_y}");
            let centre_y =
                lod.vertices.iter().map(|v| v.position[1]).sum::<f32>() / lod.vertices.len() as f32;
            let outward = lod
                .vertices
                .iter()
                .filter(|v| {
                    let p = v.position;
                    let d = [p[0], p[1] - centre_y, p[2]];
                    d[0] * v.normal[0] + d[1] * v.normal[1] + d[2] * v.normal[2] > 0.0
                })
                .count();
            assert!(
                outward * 100 > lod.vertices.len() * 95,
                "{outward} of {}",
                lod.vertices.len()
            );
            // Winding is counter-clockwise seen from outside: the face normal agrees with the vertex normals.
            let tri = &lod.indices[..3];
            let p: Vec<[f32; 3]> = tri.iter().map(|&i| lod.vertices[i as usize].position).collect();
            let e1 = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
            let e2 = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
            let n = [
                e1[1] * e2[2] - e1[2] * e2[1],
                e1[2] * e2[0] - e1[0] * e2[2],
                e1[0] * e2[1] - e1[1] * e2[0],
            ];
            let vn = lod.vertices[tri[0] as usize].normal;
            assert!(n[0] * vn[0] + n[1] * vn[1] + n[2] * vn[2] > 0.0);
        }
    }

    #[test]
    fn rocks_are_one_metre_across() {
        for seed in 0..8 {
            let rock = procedural_rock(seed, RockStyle::angular([0.5; 3]));
            for (i, lod) in rock.lods.iter().enumerate() {
                let widest = lod
                    .vertices
                    .iter()
                    .map(|v| (v.position[0].powi(2) + v.position[2].powi(2)).sqrt())
                    .fold(0.0f32, f32::max);
                // LOD 0 reaches the 0.5 m radius; the 20-triangle LOD 3 cuts corners.
                let band = if i == 0 { 0.45..=0.52 } else { 0.28..=0.52 };
                assert!(band.contains(&widest), "seed {seed} lod {i}: radius {widest}");
            }
        }
    }

    #[test]
    fn standin_library_binds_every_path() {
        let set = crate::scatter::ScatterSet::from_ron(
            r#"ScatterSet(version: 2, seed: 1, classes: [
                ScatterClass(name: "a", meshes: [ScatterMeshRef(mesh: "x.glb"), ScatterMeshRef(mesh: "y.glb")]),
                ScatterClass(name: "b", meshes: [ScatterMeshRef(mesh: "y.glb"), ScatterMeshRef(mesh: "z.glb")]),
            ])"#,
            None,
        )
        .expect("set");
        let lib = standin_library(&set, RockPalette::Mars);
        assert_eq!(lib.len(), 3);
        let colour = |p: &str| lib[p].material.base_color;
        // Mars rocks are dark: red albedo at most a quarter.
        assert!(["x.glb", "y.glb", "z.glb"].iter().all(|p| colour(p)[0] <= 0.25));
    }

    #[test]
    fn seeds_differ_and_repeat() {
        let style = RockStyle::angular([0.5; 3]);
        let a = procedural_rock(1, style);
        let b = procedural_rock(1, style);
        let c = procedural_rock(2, style);
        assert_eq!(a.lods[1].vertices[5].position, b.lods[1].vertices[5].position);
        assert_ne!(a.lods[1].vertices[5].position, c.lods[1].vertices[5].position);
    }
}
