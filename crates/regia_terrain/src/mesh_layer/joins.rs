// SPDX-License-Identifier: MPL-2.0
//! Joins of the mesh layers into the engine's other consumers of terrain geometry (spec 26
//! § 2a.6 "Pick", § 2a.8; ADR-8537): the open mouths' bands as pick surfaces, and the canonical
//! triangles cut into per-chunk collider pieces.
//!
//! Nothing here stores a second geometry: bands are the mouths' own LOD-0 positions, and the
//! collider pieces are [`regia_world_geom::WorldGeom::triangles`] (the canonical set, i32 mm read
//! through `mm_to_m`), never the render LODs or the band.

use std::collections::BTreeMap;

use regia_world_geom::cooked::TriKind;

use super::{LayerMouth, TerrainMeshLayers};

/// A ray hit on an open mouth's band or throat (LOD-0 positions).
#[derive(Clone, Debug, PartialEq)]
pub struct BandHit {
    /// The hole the mouth fills.
    pub hole: String,
    /// Distance along the unit ray, metres.
    pub distance: f32,
    /// Hit point, world metres.
    pub point: [f32; 3],
    /// Unit normal of the hit triangle, facing the ray's origin.
    pub normal: [f32; 3],
}

/// One collider piece: canonical triangles of one layer whose XZ centroid lies in one chunk.
#[derive(Clone, Debug, PartialEq)]
pub struct LayerColliderPiece {
    /// Layer index in the canonical geometry (`TriRef::layer`).
    pub layer: u16,
    /// Chunk column and row on the presentation grid, or `None` without a presentation section
    /// (the whole layer is one piece).
    pub chunk: Option<(u32, u32)>,
    /// Chunk rectangle `[min_x, min_z, size]`, world metres (`None` with `chunk`).
    pub rect: Option<[f32; 3]>,
    /// Triangles, world metres, in canonical (`TriRef`) order.
    pub triangles: Vec<[[f32; 3]; 3]>,
}

impl LayerMouth {
    /// Band triangles at their LOD-0 positions plus the throat copies when the mouth is open,
    /// otherwise nothing: what picks, colliders and the reference exports read (the drawn band may
    /// morph; LOD 0 is the reference).
    #[must_use]
    pub fn band_lod0(&self) -> Vec<[[f32; 3]; 3]> {
        if !self.open {
            return Vec::new();
        }
        let v = &self.band.vertices;
        let mut out: Vec<[[f32; 3]; 3]> = self
            .band
            .triangles
            .iter()
            .filter_map(|ix| {
                Some([
                    v.get(ix[0] as usize)?.lod0,
                    v.get(ix[1] as usize)?.lod0,
                    v.get(ix[2] as usize)?.lod0,
                ])
            })
            .collect();
        out.extend(self.throat.iter().map(|t| t.v));
        out
    }
}

impl TerrainMeshLayers {
    /// The nearest hit of the ray `origin + t · dir` (`dir` need not be unit) with an open mouth's
    /// band or throat ([`LayerMouth::band_lod0`]) within `max_distance` metres. Both faces count.
    /// Equal distances keep the earlier mouth, then the earlier triangle. `None` without an open
    /// mouth, so a terrain without layers is unchanged.
    #[must_use]
    pub fn raycast_bands(&self, origin: [f32; 3], dir: [f32; 3], max_distance: f32) -> Option<BandHit> {
        let len = dot(dir, dir).sqrt();
        if !(len.is_finite() && len > 0.0) || !origin.iter().all(|v| v.is_finite()) || max_distance.is_nan() {
            return None;
        }
        let d = [dir[0] / len, dir[1] / len, dir[2] / len];
        let mut best: Option<(f32, &LayerMouth, [[f32; 3]; 3])> = None;
        for mouth in self.mouths().iter().filter(|m| m.open) {
            for tri in mouth.band_lod0() {
                if let Some(t) = ray_triangle(origin, d, &tri)
                    && best.as_ref().map_or(t <= max_distance, |b| t < b.0)
                {
                    best = Some((t, mouth, tri));
                }
            }
        }
        let (t, mouth, tri) = best?;
        let mut n = cross(sub(tri[1], tri[0]), sub(tri[2], tri[0]));
        let nl = dot(n, n).sqrt();
        if nl > 0.0 {
            n = [n[0] / nl, n[1] / nl, n[2] / nl];
        }
        if dot(n, d) > 0.0 {
            n = [-n[0], -n[1], -n[2]];
        }
        Some(BandHit {
            hole: mouth.hole.clone(),
            distance: t,
            point: [origin[0] + d[0] * t, origin[1] + d[1] * t, origin[2] + d[2] * t],
            normal: n,
        })
    }

    /// The canonical triangles cut into collider pieces (spec 26 § 2a.8, ADR-8537): one piece per
    /// layer and presentation chunk holding its XZ centroids (the cook's chunk rule), or one per
    /// layer without a presentation section. Blocker plugs are left out (they are the sim's,
    /// through `GeomState`). Pieces are sorted by `(layer, z, x)`; empty without cooked geometry.
    #[must_use]
    pub fn collider_pieces(&self) -> Vec<LayerColliderPiece> {
        let Some(geom) = self.geom() else {
            return Vec::new();
        };
        let grid = self.presentation().map(|p| p.grid);
        let mut pieces: BTreeMap<(u16, u32, u32), LayerColliderPiece> = BTreeMap::new();
        for tri in geom.triangles() {
            if tri.kind == TriKind::Blocker {
                continue;
            }
            let c = [
                (tri.v[0][0] + tri.v[1][0] + tri.v[2][0]) / 3.0,
                (tri.v[0][2] + tri.v[1][2] + tri.v[2][2]) / 3.0,
            ];
            let chunk = grid.as_ref().and_then(|g| g.chunk_at(c));
            let key = chunk.map_or((tri.tri.layer, 0, 0), |(x, z)| (tri.tri.layer, z, x));
            let piece = pieces.entry(key).or_insert_with(|| LayerColliderPiece {
                layer: tri.tri.layer,
                chunk,
                rect: chunk.and_then(|(x, z)| {
                    grid.as_ref().map(|g| {
                        let (min_x, min_z, size) = g.chunk_rect(x, z);
                        [min_x, min_z, size]
                    })
                }),
                triangles: Vec::new(),
            });
            piece.triangles.push(tri.v);
        }
        pieces.into_values().collect()
    }

    /// [`Self::collider_pieces`] of the layers whose cooked `collider` flag is set
    /// ([`super::CookedLayer::collider`], spec 26 § 2a.8, ADR-8872): what a host that does not
    /// lockstep puts in `regia_physics::TerrainLayerColliders` when it follows the document. Empty
    /// without a presentation section (the flag is not known) or when no layer sets it.
    #[must_use]
    pub fn cooked_collider_pieces(&self) -> Vec<LayerColliderPiece> {
        let Some(presentation) = self.presentation() else {
            return Vec::new();
        };
        let flagged = |layer: u16| {
            presentation
                .layers
                .get(usize::from(layer))
                .is_some_and(|l| l.collider)
        };
        if !presentation.layers.iter().any(|l| l.collider) {
            return Vec::new();
        }
        self.collider_pieces()
            .into_iter()
            .filter(|p| flagged(p.layer))
            .collect()
    }
}

/// Mesh and material keys of [`LayerReferencePiece`]s in a [`regia_render::FrameExport`]:
/// `REFERENCE_PIECE_TAG | mouth index << 1 | collar`. Outside the renderer's ephemeral and shadow
/// LOD ranges (`0xFE..` and `0xFF..` in the top byte).
pub const REFERENCE_PIECE_TAG: u64 = 0xFD4D_4C00_0000_0000;

/// One reference piece of an open mouth (spec 26 § 2a.6 "Path tracer"): its band and throat at
/// LOD 0, or its collar (the LOD-0 lattice triangles of removed cells outside the lattice cut,
/// which the reference heightfield drops with every cell touching a hole).
#[derive(Clone, Debug, PartialEq)]
pub struct LayerReferencePiece {
    /// Mesh key in the export ([`REFERENCE_PIECE_TAG`]).
    pub key: u64,
    /// The hole the mouth fills.
    pub hole: String,
    /// `true` for the collar, `false` for the band.
    pub collar: bool,
    /// Triangles, world metres.
    pub triangles: Vec<[[f32; 3]; 3]>,
}

impl TerrainMeshLayers {
    /// The open mouths' bands and collars as reference pieces, in mouth order, band first; empty
    /// pieces are left out. A closed mouth has none, so a terrain without an open mouth adds
    /// nothing to an export.
    #[must_use]
    pub fn reference_pieces(&self) -> Vec<LayerReferencePiece> {
        let mut out = Vec::new();
        for (i, mouth) in self.mouths().iter().enumerate().filter(|(_, m)| m.open) {
            for (collar, triangles) in [(false, mouth.band_lod0()), (true, mouth.collar.clone())] {
                if triangles.is_empty() {
                    continue;
                }
                out.push(LayerReferencePiece {
                    key: REFERENCE_PIECE_TAG | ((i as u64) << 1) | u64::from(collar),
                    hole: mouth.hole.clone(),
                    collar,
                    triangles,
                });
            }
        }
        out
    }

    /// Adds [`Self::reference_pieces`] to `export` as mesh instances beside the reference
    /// heightfield (spec 26 § 2a.6): one mesh per piece (flat normals), the layer chunks' rock
    /// material, placed at `-origin_offset` (the export's local frame, like the heightfield).
    /// Returns how many instances were added.
    pub fn add_reference_pieces(
        &self,
        export: &mut regia_render::FrameExport,
        origin_offset: [f32; 3],
    ) -> usize {
        let pieces = self.reference_pieces();
        if pieces.is_empty() {
            return 0;
        }
        let _span =
            tracing::info_span!("terrain.mesh_layer_reference_pieces", pieces = pieces.len()).entered();
        export
            .materials
            .entry(REFERENCE_PIECE_TAG)
            .or_insert_with(super::stream::layer_chunk_material);
        let model = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [-origin_offset[0], -origin_offset[1], -origin_offset[2], 1.0],
        ];
        for piece in &pieces {
            let mut vertices = Vec::with_capacity(piece.triangles.len() * 3);
            for t in &piece.triangles {
                let mut n = cross(sub(t[1], t[0]), sub(t[2], t[0]));
                let l = dot(n, n).sqrt();
                if l > 0.0 {
                    n = [n[0] / l, n[1] / l, n[2] / l];
                }
                for p in t {
                    vertices.push(regia_render::LabVertex::from_pos_normal_uv(*p, n, [0.0, 0.0]));
                }
            }
            let indices = (0..vertices.len() as u32).collect();
            export.meshes.insert(
                piece.key,
                std::sync::Arc::new(regia_render::CpuMesh { vertices, indices }),
            );
            export.instances.push(regia_render::ExportedInstance {
                mesh_key: piece.key,
                material_key: REFERENCE_PIECE_TAG,
                object_key: piece.key,
                model,
                joints: Vec::new(),
            });
        }
        pieces.len()
    }
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Two-sided ray/triangle distance (Möller and Trumbore, "Fast, Minimum Storage Ray-Triangle
/// Intersection", JGT 1997), `d` unit; `None` behind the origin or off the triangle.
fn ray_triangle(o: [f32; 3], d: [f32; 3], tri: &[[f32; 3]; 3]) -> Option<f32> {
    let e1 = sub(tri[1], tri[0]);
    let e2 = sub(tri[2], tri[0]);
    let p = cross(d, e2);
    let det = dot(e1, p);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    let s = sub(o, tri[0]);
    let u = dot(s, p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = cross(s, e1);
    let v = dot(d, q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = dot(e2, q) * inv;
    (t >= 0.0 && t.is_finite()).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh_layer::{BandMesh, BandVertex};

    fn quad_mouth(open: bool) -> LayerMouth {
        let v = |x: f32, y: f32, z: f32| BandVertex {
            position: [x, y + 5.0, z],
            lod0: [x, y, z],
            ring: true,
        };
        LayerMouth {
            hole: "m".into(),
            rings: Vec::new(),
            band: BandMesh {
                vertices: vec![
                    v(-1.0, -1.0, -1.0),
                    v(1.0, -1.0, -1.0),
                    v(1.0, -1.0, 1.0),
                    v(-1.0, -1.0, 1.0),
                ],
                triangles: vec![[0, 1, 2], [0, 2, 3]],
            },
            collar: Vec::new(),
            throat: Vec::new(),
            body: Vec::new(),
            open,
        }
    }

    #[test]
    fn bands_hit_at_lod0_only_when_open() {
        let mut layers = TerrainMeshLayers::new();
        layers.push_mouth(quad_mouth(true));
        let hit = layers.raycast_bands([0.2, 10.0, 0.1], [0.0, -2.0, 0.0], 100.0);
        let hit = hit.expect("the open band is hit");
        assert_eq!(hit.hole, "m");
        assert!(
            (hit.distance - 11.0).abs() < 1e-5,
            "LOD-0 y, not the drawn y: {}",
            hit.distance
        );
        assert_eq!(hit.normal, [0.0, 1.0, 0.0]);
        assert!(
            layers
                .raycast_bands([0.2, 10.0, 0.1], [0.0, -1.0, 0.0], 10.0)
                .is_none()
        );
        layers.set_open("m", false);
        assert!(
            layers
                .raycast_bands([0.2, 10.0, 0.1], [0.0, -1.0, 0.0], 100.0)
                .is_none()
        );
        assert!(TerrainMeshLayers::new().collider_pieces().is_empty());
    }
}
