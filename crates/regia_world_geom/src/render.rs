// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (tools/tunnels_v2/tunnel_bake.py, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 578-707, mark: adapt (Stage A: unchanged).
// Edits: Python rewritten in Rust (AGENTS rule 2): `build_meshes`' render half (`_across`, `_wall_row`, `_runs`, the `detail` subdivision) subdivides the pinned mesher's canonical faces instead of a second builder; `build_terrain_cut`'s quad in integer mm; DO27 file and engine names dropped; plan row W-07, ADR-8175.

//! The render half of the mesh layers cook (spec 121 § 9.2 item 3, spec 26 § 2a.3; plan row W-07, ADR-8175).
//!
//! **The collision triangles never reach the draw list.** A `Passages` layer's render mesh is cooked from the same
//! conditioned canonical triangles the sim stands on, so it shares their section planes (flat floor, vertical walls,
//! flat ceiling, the lips, risers and headers of the pinned mesher), but every canonical face is subdivided
//! `subdivision` times along each edge into its own render triangles: lights and looks get vertices to work with,
//! and no canonical triangle is drawn as it is (DO27's "flat collision-tube material" known issue). The collision
//! displacement is 0: every render vertex lies on its face's plane up to the millimetre rounding.
//!
//! The subdivision is integer arithmetic on the face's millimetre corners (barycentric grid, rounded half away from
//! zero), so the bytes are the same on every host, and an edge two faces share is split into the same points from
//! either side: the render mesh is as closed as the canonical one.
//!
//! The mouth cut quads (one per opening on a ramp, the T1 baker's terrain cut rect) are written with the render
//! mesh for the terrain's cut (lane W-10 owns the lattice cut that replaces the quads).
//!
//! The presentation section is not hashed into `content_hash` (spec 121 § 9.2): a render-only change never splits a
//! match.

use regia_detsim::float::{det_clamp_f64, det_min_f64};
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::compat::layout::{MAX_HOLE_SHAPES, Opening, PORTAL_LENGTH_M, SampledLayout};
use crate::cook::WorldTriangleMm;
use crate::cooked::{GeomLoadError, TriKind};
use crate::units::m_to_mm;

/// The presentation section's own version (1). Still 1 while the cooked file is unshipped (ADR-8175).
pub const PRESENTATION_VERSION: u32 = 1;

/// The default number of times each canonical face edge is split (the T1 baker's `detail`).
pub const DEFAULT_SUBDIVISION: u8 = 4;
/// The smallest subdivision: 1 would draw the canonical triangles themselves.
pub const MIN_SUBDIVISION: u8 = 2;
/// The largest subdivision (256 render triangles per canonical face).
pub const MAX_SUBDIVISION: u8 = 16;

/// Extra graded lip drawn around every mouth cut, metres (the T1 baker's `MOUTH_LIP_M`).
pub const MOUTH_LIP_M: f32 = 2.0;

/// What a render triangle draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RenderPart {
    /// A floor (the canonical `Floor`).
    Floor,
    /// A wall, riser, header or cap (the canonical `Wall`).
    Wall,
    /// A ceiling (the canonical `Roof`).
    Roof,
}

/// Where a render triangle comes from: the canonical face it subdivides (the layer's canonical index, the
/// `TriRef::triangle` of [`crate::TriRef`]) and which of its `subdivision²` pieces it is. A render triangle is
/// never the canonical triangle itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RenderSource {
    /// The passage the face belongs to (index in the layout; the passage whose sampled centreline is nearest the
    /// face's centroid in XZ, ties to the lower index).
    pub passage: u16,
    /// The canonical face's index in its layer.
    pub face: u32,
    /// The piece, `0..subdivision²`, in grid order.
    pub piece: u16,
}

/// One render triangle: three indices into [`LayerRender::vertices`], wound like its canonical face.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderTriangle {
    /// Vertex indices.
    pub v: [u32; 3],
    /// What it draws.
    pub part: RenderPart,
    /// Its provenance.
    pub source: RenderSource,
}

/// One passage's share of a layer's render mesh: its triangles are `first..first + count` of
/// [`LayerRender::triangles`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassageRender {
    /// The passage id.
    pub id: String,
    /// First triangle.
    pub first: u32,
    /// Triangle count.
    pub count: u32,
}

/// A mouth cut quad: the terrain cut rect around one opening, corners counter-clockwise from the back left, at the
/// opening top's height, world millimetres.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouthQuad {
    /// The opening id.
    pub opening: String,
    /// The corners, world millimetres.
    pub corners: [[i32; 3]; 4],
}

/// One layer's render mesh.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerRender {
    /// The layer's index in the document (the canonical `TriRef::layer`).
    pub layer: u16,
    /// The layer id.
    pub id: String,
    /// The render material asset path (empty: the engine's default rock).
    pub material: String,
    /// Box-projection UV tile, metres, as `f32` bits.
    pub uv_tile_m_bits: u32,
    /// How many times each canonical face edge was split.
    pub subdivision: u8,
    /// Distinct vertex positions, world millimetres, in first-use order.
    pub vertices: Vec<[i32; 3]>,
    /// The draw list, grouped by passage (canonical face order inside a passage, then piece order).
    pub triangles: Vec<RenderTriangle>,
    /// Each passage's range of `triangles`, in layout order.
    pub passages: Vec<PassageRender>,
    /// The mouth cut quads, in opening order.
    pub mouths: Vec<MouthQuad>,
}

/// The presentation section of a cooked `<map>.meshlayers` (spec 121 § 9.2 item 3), version first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PresentationCooked {
    /// [`PRESENTATION_VERSION`].
    pub version: u32,
    /// One entry per layer that has a render mesh, in document order.
    pub layers: Vec<LayerRender>,
}

impl Default for PresentationCooked {
    fn default() -> Self {
        Self {
            version: PRESENTATION_VERSION,
            layers: Vec::new(),
        }
    }
}

impl PresentationCooked {
    /// The section's bytes (postcard).
    ///
    /// # Errors
    ///
    /// [`GeomLoadError::Corrupt`] if postcard refuses the value (it does not for any value).
    pub fn encode(&self) -> Result<Vec<u8>, GeomLoadError> {
        postcard::to_allocvec(self).map_err(|e| GeomLoadError::Corrupt(e.to_string()))
    }

    /// Decodes the section. Empty bytes are an empty section (a file cooked before the render half). Refuses a
    /// newer version before decoding the rest, trailing bytes, and an index out of range.
    ///
    /// # Errors
    ///
    /// [`GeomLoadError::NewerVersion`] (`geom_newer_version`), [`GeomLoadError::Corrupt`] (`geom_corrupt`).
    pub fn decode(bytes: &[u8]) -> Result<Self, GeomLoadError> {
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        let (version, _) = postcard::take_from_bytes::<u32>(bytes)
            .map_err(|e| GeomLoadError::Corrupt(format!("presentation section: {e}")))?;
        if version > PRESENTATION_VERSION {
            return Err(GeomLoadError::NewerVersion {
                found: version,
                supported: PRESENTATION_VERSION,
            });
        }
        let (section, rest) = postcard::take_from_bytes::<Self>(bytes)
            .map_err(|e| GeomLoadError::Corrupt(format!("presentation section: {e}")))?;
        if !rest.is_empty() {
            return Err(GeomLoadError::Corrupt(format!(
                "presentation section: {} trailing bytes",
                rest.len()
            )));
        }
        for layer in &section.layers {
            let n = layer.vertices.len() as u64;
            if layer
                .triangles
                .iter()
                .any(|t| t.v.iter().any(|&i| u64::from(i) >= n))
            {
                return Err(GeomLoadError::Corrupt(format!(
                    "presentation layer `{}`: a vertex index is out of range",
                    layer.id
                )));
            }
            let mut next = 0u64;
            for p in &layer.passages {
                if u64::from(p.first) != next {
                    return Err(GeomLoadError::Corrupt(format!(
                        "presentation layer `{}`: passage `{}` does not start where the previous one ends",
                        layer.id, p.id
                    )));
                }
                next += u64::from(p.count);
            }
            if next != layer.triangles.len() as u64 {
                return Err(GeomLoadError::Corrupt(format!(
                    "presentation layer `{}`: passage ranges cover {next} of {} triangles",
                    layer.id,
                    layer.triangles.len()
                )));
            }
        }
        Ok(section)
    }
}

impl LayerRender {
    /// The draw list as world-millimetre corner triples (for checks and tools).
    #[must_use]
    pub fn triangles_mm(&self) -> Vec<[[i32; 3]; 3]> {
        self.triangles
            .iter()
            .map(|t| t.v.map(|i| self.vertices[i as usize]))
            .collect()
    }
}

/// `n / d` rounded half away from zero (`d > 0`).
fn div_round(n: i64, d: i64) -> i64 {
    if n >= 0 {
        (2 * n + d) / (2 * d)
    } else {
        -((-2 * n + d) / (2 * d))
    }
}

/// The barycentric grid point `(i, j)` of face `[a, b, c]` split `d` times: `(a·(d − i − j) + b·i + c·j) / d`.
fn grid_point(v: &[[i32; 3]; 3], d: i64, i: i64, j: i64) -> [i32; 3] {
    let mut out = [0i32; 3];
    for (k, o) in out.iter_mut().enumerate() {
        let n = i64::from(v[0][k]) * (d - i - j) + i64::from(v[1][k]) * i + i64::from(v[2][k]) * j;
        // |n / d| is within the face's own coordinate range, so it fits an i32.
        *o = div_round(n, d) as i32;
    }
    out
}

/// Splits one face into `d²` triangles of the barycentric grid, wound like the face, in grid order: for each row
/// `j`, the up triangle `(i, j)` then the down triangle `(i, j)` when it exists.
#[must_use]
pub fn subdivide_face(v: &[[i32; 3]; 3], subdivision: u8) -> Vec<[[i32; 3]; 3]> {
    let d = i64::from(subdivision.max(1));
    let mut out = Vec::with_capacity((d * d) as usize);
    for j in 0..d {
        for i in 0..d - j {
            let p = |a: i64, b: i64| grid_point(v, d, a, b);
            out.push([p(i, j), p(i + 1, j), p(i, j + 1)]);
            if i + j + 1 < d {
                out.push([p(i + 1, j), p(i + 1, j + 1), p(i, j + 1)]);
            }
        }
    }
    out
}

fn degenerate(v: &[[i32; 3]; 3]) -> bool {
    let [a, b, c] = v.map(|p| p.map(i64::from));
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let w = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    [
        u[1] * w[2] - u[2] * w[1],
        u[2] * w[0] - u[0] * w[2],
        u[0] * w[1] - u[1] * w[0],
    ] == [0, 0, 0]
}

/// XZ distance squared from `(x, z)` to the segment `a -> b`, metres.
fn seg_dist2_xz(x: f64, z: f64, a: [f32; 3], b: [f32; 3]) -> f64 {
    let (ax, az) = (f64::from(a[0]), f64::from(a[2]));
    let (abx, abz) = (f64::from(b[0]) - ax, f64::from(b[2]) - az);
    let len2 = abx * abx + abz * abz;
    let t = if len2 < 1e-12 {
        0.0
    } else {
        det_clamp_f64(((x - ax) * abx + (z - az) * abz) / len2, 0.0, 1.0)
    };
    let (dx, dz) = (x - (ax + abx * t), z - (az + abz * t));
    dx * dx + dz * dz
}

/// The passage whose sampled centreline is nearest `(x, z)`, metres (ties to the lower index).
fn nearest_passage(sampled: &SampledLayout, x: f64, z: f64) -> u16 {
    let mut best = (f64::INFINITY, 0u16);
    for (i, p) in sampled.passages.iter().enumerate() {
        let d = match p.samples.as_slice() {
            [] => f64::INFINITY,
            [only] => seg_dist2_xz(x, z, only.p, only.p),
            s => s
                .windows(2)
                .map(|w| seg_dist2_xz(x, z, w[0].p, w[1].p))
                .fold(f64::INFINITY, det_min_f64),
        };
        if d < best.0 {
            best = (d, i as u16);
        }
    }
    best.1
}

/// The mouth cut quad of each opening whose ramp is in the layout, in opening order: half the ramp width plus
/// [`MOUTH_LIP_M`] each side, from [`PORTAL_LENGTH_M`] + [`MOUTH_LIP_M`] behind the opening top to [`MOUTH_LIP_M`]
/// past it along the ramp's outward direction (its last two samples), at the top's height. At most
/// [`MAX_HOLE_SHAPES`] quads (the hole mask budget).
#[must_use]
pub fn mouth_quads(sampled: &SampledLayout, openings: &[Opening]) -> Vec<MouthQuad> {
    let mut out = Vec::new();
    for o in openings {
        let Some(ramp) = sampled.passage(&o.ramp) else {
            continue;
        };
        let Some(outward) = outward(ramp.samples.as_slice()) else {
            continue;
        };
        let corners = cut_corners(o.top, outward, ramp.width_m);
        out.push(MouthQuad {
            opening: o.id.clone(),
            corners: corners.map(|c| c.map(|v| m_to_mm(v as f32))),
        });
        if out.len() == MAX_HOLE_SHAPES {
            break;
        }
    }
    out
}

/// The unit XZ direction from a ramp's second-to-last sample to its last, `None` when they share an XZ.
#[must_use]
pub fn outward(samples: &[crate::compat::layout::Sample]) -> Option<[f64; 2]> {
    let [.., a, b] = samples else {
        return None;
    };
    let (dx, dz) = (
        f64::from(b.p[0]) - f64::from(a.p[0]),
        f64::from(b.p[2]) - f64::from(a.p[2]),
    );
    let len = regia_detsim::math::sqrt_f64(dx * dx + dz * dz);
    (len > 1e-9).then(|| [dx / len, dz / len])
}

/// The four cut corners (metres) around `top` for a ramp `width_m` wide leaving along `out`: back left, back
/// right, front right, front left.
#[must_use]
pub fn cut_corners(top: [f32; 3], out: [f64; 2], width_m: f32) -> [[f64; 3]; 4] {
    let half = f64::from(width_m) * 0.5 + f64::from(MOUTH_LIP_M);
    let back = f64::from(PORTAL_LENGTH_M) + f64::from(MOUTH_LIP_M);
    let fwd = f64::from(MOUTH_LIP_M);
    let (cx, cy, cz) = (f64::from(top[0]), f64::from(top[1]), f64::from(top[2]));
    // Lateral: left of outward travel.
    let (px, pz) = (-out[1], out[0]);
    [
        [cx - out[0] * back + px * half, cy, cz - out[1] * back + pz * half],
        [cx - out[0] * back - px * half, cy, cz - out[1] * back - pz * half],
        [cx + out[0] * fwd - px * half, cy, cz + out[1] * fwd - pz * half],
        [cx + out[0] * fwd + px * half, cy, cz + out[1] * fwd + pz * half],
    ]
}

/// What a render cook of one layer needs besides its canonical triangles.
pub struct RenderLayerInputs<'a> {
    /// The layer's index in the document.
    pub layer: u16,
    /// The layer id.
    pub id: &'a str,
    /// The render material.
    pub material: &'a str,
    /// The UV tile, metres.
    pub uv_tile_m: f32,
    /// The subdivision, [`MIN_SUBDIVISION`]..=[`MAX_SUBDIVISION`] (clamped).
    pub subdivision: u8,
    /// The layout as the canonical triangles were meshed from it.
    pub sampled: &'a SampledLayout,
    /// Its openings.
    pub openings: &'a [Opening],
}

/// Cooks one layer's render mesh from its conditioned canonical triangles (source order, the layer's canonical
/// indices). Zero-area canonical faces draw nothing.
#[must_use]
pub fn render_layer(inputs: &RenderLayerInputs<'_>, canonical: &[WorldTriangleMm]) -> LayerRender {
    let _span = tracing::info_span!("meshlayers_render_layer", layer = inputs.id).entered();
    let subdivision = inputs.subdivision.clamp(MIN_SUBDIVISION, MAX_SUBDIVISION);
    // Faces grouped by passage, canonical order inside each.
    let mut by_passage: BTreeMap<u16, Vec<u32>> = BTreeMap::new();
    for (face, t) in canonical.iter().enumerate() {
        if degenerate(&t.v) {
            continue;
        }
        let cx = (f64::from(t.v[0][0]) + f64::from(t.v[1][0]) + f64::from(t.v[2][0])) / 3000.0;
        let cz = (f64::from(t.v[0][2]) + f64::from(t.v[1][2]) + f64::from(t.v[2][2])) / 3000.0;
        by_passage
            .entry(nearest_passage(inputs.sampled, cx, cz))
            .or_default()
            .push(face as u32);
    }
    let mut index: BTreeMap<[i32; 3], u32> = BTreeMap::new();
    let mut vertices: Vec<[i32; 3]> = Vec::new();
    let mut triangles: Vec<RenderTriangle> = Vec::new();
    let mut passages = Vec::with_capacity(inputs.sampled.passages.len());
    for (pi, p) in inputs.sampled.passages.iter().enumerate() {
        let first = triangles.len() as u32;
        for &face in by_passage.get(&(pi as u16)).map_or(&[][..], Vec::as_slice) {
            let t = &canonical[face as usize];
            let part = match t.kind {
                TriKind::Floor => RenderPart::Floor,
                TriKind::Roof => RenderPart::Roof,
                TriKind::Wall | TriKind::Blocker => RenderPart::Wall,
            };
            for (piece, sub) in subdivide_face(&t.v, subdivision).into_iter().enumerate() {
                if degenerate(&sub) {
                    continue;
                }
                let v = sub.map(|q| {
                    *index.entry(q).or_insert_with(|| {
                        vertices.push(q);
                        (vertices.len() - 1) as u32
                    })
                });
                triangles.push(RenderTriangle {
                    v,
                    part,
                    source: RenderSource {
                        passage: pi as u16,
                        face,
                        piece: piece as u16,
                    },
                });
            }
        }
        passages.push(PassageRender {
            id: p.id.clone(),
            first,
            count: triangles.len() as u32 - first,
        });
    }
    let plus = |v: f32| if v == 0.0 { 0.0f32 } else { v };
    LayerRender {
        layer: inputs.layer,
        id: inputs.id.to_string(),
        material: inputs.material.to_string(),
        uv_tile_m_bits: plus(inputs.uv_tile_m).to_bits(),
        subdivision,
        vertices,
        triangles,
        passages,
        mouths: mouth_quads(inputs.sampled, inputs.openings),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_face_splits_into_d_squared_pieces_on_its_plane() {
        let face = [[0, 0, 0], [5000, 0, 0], [0, 0, 5000]];
        for d in [2u8, 3, 4, 7] {
            let pieces = subdivide_face(&face, d);
            assert_eq!(pieces.len(), usize::from(d) * usize::from(d));
            assert!(pieces.iter().all(|t| t.iter().all(|p| p[1] == 0)));
            // Same winding (the face's normal points down here: (b - a) x (c - a) has y < 0).
            for t in &pieces {
                let u = [t[1][0] - t[0][0], t[1][2] - t[0][2]];
                let w = [t[2][0] - t[0][0], t[2][2] - t[0][2]];
                assert!(i64::from(u[1]) * i64::from(w[0]) - i64::from(u[0]) * i64::from(w[1]) < 0);
            }
            // The pieces' area adds up to the face's (within the millimetre rounding).
            let area = |t: &[[i32; 3]; 3]| {
                let (ax, az) = (f64::from(t[0][0]), f64::from(t[0][2]));
                ((f64::from(t[1][0]) - ax) * (f64::from(t[2][2]) - az)
                    - (f64::from(t[2][0]) - ax) * (f64::from(t[1][2]) - az))
                    .abs()
                    * 0.5
            };
            let sum: f64 = pieces.iter().map(area).sum();
            assert!((sum - area(&face)).abs() / area(&face) < 0.01, "d {d}: {sum}");
        }
    }

    #[test]
    fn a_shared_edge_splits_into_the_same_points_from_both_faces() {
        let a = [12_345, -40_000, 7];
        let b = [-9_999, -39_001, 51_003];
        let left = [a, b, [30_000, -40_000, 30_000]];
        let right = [b, a, [-30_000, -38_000, -2_000]];
        for d in [2u8, 3, 4, 5, 16] {
            let dd = i64::from(d);
            let on_left: Vec<_> = (0..=dd).map(|i| grid_point(&left, dd, i, 0)).collect();
            let mut on_right: Vec<_> = (0..=dd).map(|i| grid_point(&right, dd, i, 0)).collect();
            on_right.reverse();
            assert_eq!(on_left, on_right, "d {d}");
        }
    }

    #[test]
    fn rounding_is_half_away_from_zero() {
        assert_eq!(div_round(5, 2), 3);
        assert_eq!(div_round(-5, 2), -3);
        assert_eq!(div_round(4, 3), 1);
        assert_eq!(div_round(-4, 3), -1);
        assert_eq!(div_round(0, 7), 0);
    }

    #[test]
    fn an_empty_section_decodes_and_a_newer_one_is_refused() {
        assert_eq!(PresentationCooked::decode(&[]), Ok(PresentationCooked::default()));
        let newer = PresentationCooked {
            version: PRESENTATION_VERSION + 1,
            layers: Vec::new(),
        };
        let bytes = newer.encode().unwrap_or_default();
        assert_eq!(
            PresentationCooked::decode(&bytes).map_err(|e| e.code()),
            Err("geom_newer_version")
        );
        let mut bad = PresentationCooked::default();
        bad.layers.push(LayerRender {
            layer: 0,
            id: "x".into(),
            material: String::new(),
            uv_tile_m_bits: 4.0f32.to_bits(),
            subdivision: 2,
            vertices: vec![[0, 0, 0]],
            triangles: vec![RenderTriangle {
                v: [0, 0, 1],
                part: RenderPart::Floor,
                source: RenderSource {
                    passage: 0,
                    face: 0,
                    piece: 0,
                },
            }],
            passages: vec![PassageRender {
                id: "p".into(),
                first: 0,
                count: 1,
            }],
            mouths: Vec::new(),
        });
        let bytes = bad.encode().unwrap_or_default();
        assert_eq!(
            PresentationCooked::decode(&bytes).map_err(|e| e.code()),
            Err("geom_corrupt")
        );
        let mut trailing = PresentationCooked::default().encode().unwrap_or_default();
        trailing.push(0);
        assert_eq!(
            PresentationCooked::decode(&trailing).map_err(|e| e.code()),
            Err("geom_corrupt")
        );
    }
}
