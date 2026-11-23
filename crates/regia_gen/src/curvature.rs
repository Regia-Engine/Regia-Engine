// SPDX-License-Identifier: MPL-2.0
//! Mesh-derived masks baked into UV space: an edge (convexity) mask and a cavity (concavity) mask
//! from the dihedral angles of a [`CpuMesh`]'s edges.
//!
//! Every edge shared by exactly two triangles (after welding equal positions) gets a strength
//! from the bend between its faces, `1 − n₁·n₂` (no `acos`), ramped from `min_angle` to
//! `full_angle`, and a side: convex when the far corner of the second face lies below the first
//! face's plane, concave when above. Each texel a triangle covers in UV space takes the triangle's
//! world position there and, over the feature edges that touch the triangle's corners, the
//! largest `strength · smoothstep(1 − distance / width)`. Overlapping texels keep the maximum, so
//! the result does not depend on triangle order. A few rings of uncovered texels around each UV
//! island are filled from their neighbours (`dilate`) so filtering at seams does not pull in
//! black.
//!
//! The UV rasterization is winding-agnostic: barycentrics divide the signed edge functions by
//! the signed triangle area, so CCW and CW UV windings (mirrored islands in symmetric unwraps)
//! cover the same texels with the same weights
//! (`cw_wound_uv_triangles_bake_full_coverage` in `tests/texture_edges.rs`).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::build::{dot, face_normal, sub};
use crate::{CpuMesh, FloatImage, GenError, weld_key};

/// The most dilation rings.
pub const MAX_DILATE: u32 = 64;

/// Options of [`curvature_masks`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CurvatureOptions {
    /// How far from an edge the mask reaches, in mesh units (metres), positive (default 0.05).
    pub width: f32,
    /// The bend (degrees between face normals) below which an edge does not count (default 10).
    pub min_angle: f32,
    /// The bend at which an edge counts fully, above `min_angle`, at most 180 (default 60).
    pub full_angle: f32,
    /// Rings of texels filled outward from each UV island (default 2, at most 64).
    pub dilate: u32,
}

impl Default for CurvatureOptions {
    fn default() -> Self {
        Self {
            width: 0.05,
            min_angle: 10.0,
            full_angle: 60.0,
            dilate: 2,
        }
    }
}

/// The masks [`curvature_masks`] bakes, all the requested size.
#[derive(Clone, Debug, PartialEq)]
pub struct CurvatureMasks {
    /// Convex edges (outer corners, where paint chips): 1 on a sharp edge, falling to 0 at
    /// `width` from it.
    pub edge: FloatImage,
    /// Concave edges (inner corners, where dirt gathers), the same way.
    pub cavity: FloatImage,
    /// 1 where a triangle covers the texel (before dilation), else 0.
    pub coverage: FloatImage,
}

struct Tri {
    p: [[f32; 3]; 3],
    uv: [[f32; 2]; 3],
    w: [u32; 3],
    n: [f32; 3],
}

struct FeatureEdge {
    a: [f64; 3],
    b: [f64; 3],
    convex: f64,
    concave: f64,
}

fn degrees_cos(deg: f32) -> f32 {
    regia_core::math::det::cos(deg * (std::f32::consts::PI / 180.0))
}

/// Bakes the edge and cavity masks of `mesh` into a `width × height` UV-space image. The mesh
/// needs one UV per vertex; texels outside `[0, 1]²` are dropped (no wrapping).
pub fn curvature_masks(
    mesh: &CpuMesh,
    width: u32,
    height: u32,
    options: &CurvatureOptions,
) -> Result<CurvatureMasks, GenError> {
    let _span = tracing::trace_span!("regia_gen::curvature_masks").entered();
    let o = options;
    crate::finite("curvature options", &[o.width, o.min_angle, o.full_angle])?;
    if o.width <= 0.0 {
        return Err(GenError::InvalidInput("curvature width must be positive".into()));
    }
    if !(o.min_angle >= 0.0 && o.min_angle < o.full_angle && o.full_angle <= 180.0) {
        return Err(GenError::InvalidInput(
            "curvature angles need 0 <= min_angle < full_angle <= 180".into(),
        ));
    }
    if o.dilate > MAX_DILATE {
        return Err(GenError::TooLarge {
            what: "dilate rings",
            value: o.dilate as usize,
            limit: MAX_DILATE as usize,
        });
    }
    if mesh.uvs.is_empty() || mesh.uvs.len() != mesh.positions.len() {
        return Err(GenError::InvalidInput(format!(
            "the mesh needs one UV per vertex ({} UVs for {} vertices)",
            mesh.uvs.len(),
            mesh.positions.len()
        )));
    }
    crate::finite("mesh UVs", mesh.uvs.as_flattened())?;
    crate::finite("mesh positions", mesh.positions.as_flattened())?;
    let n = crate::image::check_size(width, height)?;

    // Weld positions, in vertex order.
    let mut ids: HashMap<[u32; 3], u32> = HashMap::new();
    let mut welded_pos: Vec<[f32; 3]> = Vec::new();
    let welded: Vec<u32> = mesh
        .positions
        .iter()
        .map(|p| {
            let next = welded_pos.len() as u32;
            *ids.entry(weld_key(*p)).or_insert_with(|| {
                welded_pos.push(*p);
                next
            })
        })
        .collect();

    let mut tris = Vec::with_capacity(mesh.indices.len() / 3);
    let (chunks, rest) = mesh.indices.as_chunks::<3>();
    if !rest.is_empty() {
        return Err(GenError::InvalidInput(
            "index count is not a multiple of 3".into(),
        ));
    }
    for t in chunks {
        let Some(p) = crate::build::corners(&mesh.positions, t) else {
            return Err(GenError::InvalidInput("an index is out of range".into()));
        };
        let w = t.map(|i| welded[i as usize]);
        let normal = face_normal(p[0], p[1], p[2]);
        if w[0] == w[1] || w[1] == w[2] || w[0] == w[2] || normal == [0.0; 3] {
            continue;
        }
        tris.push(Tri {
            p,
            uv: t.map(|i| mesh.uvs[i as usize]),
            w,
            n: normal,
        });
    }

    // Undirected welded edge -> (triangle, corner opposite the edge), in triangle order.
    let mut edge_tris: HashMap<(u32, u32), Vec<(usize, usize)>> = HashMap::new();
    for (ti, t) in tris.iter().enumerate() {
        for k in 0..3 {
            let (a, b) = (t.w[k], t.w[(k + 1) % 3]);
            edge_tris
                .entry((a.min(b), a.max(b)))
                .or_default()
                .push((ti, (k + 2) % 3));
        }
    }
    let bend_min = 1.0 - degrees_cos(o.min_angle);
    let bend_full = 1.0 - degrees_cos(o.full_angle);
    let mut features: Vec<FeatureEdge> = Vec::new();
    let mut vertex_edges: Vec<Vec<u32>> = vec![Vec::new(); welded_pos.len()];
    for (ti, t) in tris.iter().enumerate() {
        for k in 0..3 {
            let (a, b) = (t.w[k], t.w[(k + 1) % 3]);
            let Some(list) = edge_tris.get(&(a.min(b), a.max(b))) else {
                continue;
            };
            if list.len() != 2 || list[0].0 != ti {
                continue;
            }
            let other = &tris[list[1].0];
            let bend = 1.0 - dot(t.n, other.n);
            let strength = ((bend - bend_min) / (bend_full - bend_min)).clamp(0.0, 1.0);
            if strength <= 0.0 {
                continue;
            }
            let side = dot(t.n, sub(other.p[list[1].1], t.p[k]));
            let s = f64::from(strength);
            let (convex, concave) = if side < 0.0 { (s, 0.0) } else { (0.0, s) };
            let id = features.len() as u32;
            features.push(FeatureEdge {
                a: welded_pos[a as usize].map(f64::from),
                b: welded_pos[b as usize].map(f64::from),
                convex,
                concave,
            });
            vertex_edges[a as usize].push(id);
            vertex_edges[b as usize].push(id);
        }
    }

    let mut edge = vec![0.0f32; n];
    let mut cavity = vec![0.0f32; n];
    let mut covered = vec![false; n];
    let (wf, hf) = (f64::from(width), f64::from(height));
    let reach = f64::from(o.width);
    for t in &tris {
        let mut cand: Vec<u32> =
            t.w.iter()
                .flat_map(|&v| vertex_edges[v as usize].iter().copied())
                .collect();
        cand.sort_unstable();
        cand.dedup();
        let q = t.uv.map(|uv| [f64::from(uv[0]) * wf, f64::from(uv[1]) * hf]);
        let area = (q[1][0] - q[0][0]) * (q[2][1] - q[0][1]) - (q[1][1] - q[0][1]) * (q[2][0] - q[0][0]);
        if area == 0.0 {
            continue;
        }
        let lo = |k: usize| q.iter().map(|v| v[k]).fold(f64::INFINITY, f64::min);
        let hi = |k: usize| q.iter().map(|v| v[k]).fold(f64::NEG_INFINITY, f64::max);
        let x0 = (lo(0) - 0.5).ceil().max(0.0) as i64;
        let x1 = (hi(0) - 0.5).floor().min(wf - 1.0) as i64;
        let y0 = (lo(1) - 0.5).ceil().max(0.0) as i64;
        let y1 = (hi(1) - 0.5).floor().min(hf - 1.0) as i64;
        let pos = t.p.map(|p| p.map(f64::from));
        for y in y0..=y1 {
            for x in x0..=x1 {
                let c = [x as f64 + 0.5, y as f64 + 0.5];
                let e = |i: usize, j: usize| {
                    ((q[j][0] - q[i][0]) * (c[1] - q[i][1]) - (q[j][1] - q[i][1]) * (c[0] - q[i][0])) / area
                };
                let b = [e(1, 2), e(2, 0), e(0, 1)];
                if b.iter().any(|&v| v < 0.0) {
                    continue;
                }
                let world: [f64; 3] =
                    std::array::from_fn(|k| b[0] * pos[0][k] + b[1] * pos[1][k] + b[2] * pos[2][k]);
                let (mut ev, mut cv) = (0.0f64, 0.0f64);
                for &id in &cand {
                    let f = &features[id as usize];
                    let u = 1.0 - segment_distance(world, f.a, f.b) / reach;
                    if u <= 0.0 {
                        continue;
                    }
                    let fall = u * u * (3.0 - 2.0 * u);
                    ev = ev.max(f.convex * fall);
                    cv = cv.max(f.concave * fall);
                }
                let i = (y * i64::from(width) + x) as usize;
                edge[i] = edge[i].max(ev as f32);
                cavity[i] = cavity[i].max(cv as f32);
                covered[i] = true;
            }
        }
    }
    let coverage: Vec<f32> = covered.iter().map(|&c| if c { 1.0 } else { 0.0 }).collect();
    dilate(
        &mut [&mut edge, &mut cavity],
        &mut covered,
        width,
        height,
        o.dilate,
    );
    let img = |pixels| FloatImage {
        width,
        height,
        pixels,
    };
    Ok(CurvatureMasks {
        edge: img(edge),
        cavity: img(cavity),
        coverage: img(coverage),
    })
}

/// Distance from `p` to segment `a b`.
fn segment_distance(p: [f64; 3], a: [f64; 3], b: [f64; 3]) -> f64 {
    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let ap = [p[0] - a[0], p[1] - a[1], p[2] - a[2]];
    let len2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
    let t = if len2 > 0.0 {
        ((ap[0] * ab[0] + ap[1] * ab[1] + ap[2] * ab[2]) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let d = [ap[0] - ab[0] * t, ap[1] - ab[1] * t, ap[2] - ab[2] * t];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

/// Fills `rings` rings of uncovered texels from their covered 4-neighbours (the maximum, per
/// layer), each ring from the previous ring's state.
fn dilate(layers: &mut [&mut Vec<f32>], covered: &mut [bool], width: u32, height: u32, rings: u32) {
    let (w, h) = (width as i64, height as i64);
    for _ in 0..rings {
        let before = covered.to_vec();
        let snapshot: Vec<Vec<f32>> = layers.iter().map(|l| l.to_vec()).collect();
        let mut changed = false;
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) as usize;
                if before[i] {
                    continue;
                }
                let mut any = false;
                for (dx, dy) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)] {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= w || ny >= h {
                        continue;
                    }
                    let j = (ny * w + nx) as usize;
                    if !before[j] {
                        continue;
                    }
                    for (layer, snap) in layers.iter_mut().zip(&snapshot) {
                        layer[i] = if any { layer[i].max(snap[j]) } else { snap[j] };
                    }
                    any = true;
                }
                if any {
                    covered[i] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
}
