// SPDX-License-Identifier: MPL-2.0
//! Extrusion of a 2D profile (with holes) along +Z, with an optional rounded bevel on both caps.
//!
//! Method: a prism sweep (side quads between rings, ear-clipped caps); the bevel insets each ring by
//! the miter offset `(n₁ + n₂) / (1 + n₁·n₂)` of its two edge normals (the standard polygon offset
//! of e.g. the public "straight skeleton" literature, here without topology events: an edge that
//! would turn around is an error) along a quarter circle `(w(1 − cos θ), w sin θ)`.

use regia_core::math::det;

use crate::build::Builder;
use crate::triangulate::{signed_area2, triangulate};
use crate::{CpuMesh, GenError, MAX_SEGMENTS, check_vertices, finite};

/// A closed 2D outline in the XY plane with optional holes. Any winding is accepted.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Profile {
    /// The outer ring.
    pub outer: Vec<[f32; 2]>,
    /// Holes, each strictly inside the outer ring and apart from the others.
    #[serde(default)]
    pub holes: Vec<Vec<[f32; 2]>>,
}

impl Profile {
    /// A profile with no holes.
    pub fn new(outer: Vec<[f32; 2]>) -> Self {
        Self {
            outer,
            holes: Vec::new(),
        }
    }

    /// Adds a hole.
    pub fn with_hole(mut self, hole: Vec<[f32; 2]>) -> Self {
        self.holes.push(hole);
        self
    }
}

/// A rounded bevel on an extrusion's cap edges.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Bevel {
    /// Inset and height of the bevel, in profile units. `0 < width ≤ depth / 2`.
    pub width: f32,
    /// Facets along the quarter circle, `1..=MAX_SEGMENTS` (1 = a 45° chamfer).
    pub segments: u32,
}

/// Options of [`extrude`].
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExtrudeOptions {
    /// Extrusion length along +Z (`> 0`): the back cap sits at `z = 0`, the front at `z = depth`.
    pub depth: f32,
    /// Optional bevel on the edges of both caps.
    #[serde(default)]
    pub bevel: Option<Bevel>,
}

/// One ring level of the sweep: per-ring inset points at height `z`.
struct Level {
    z: f32,
    rings: Vec<Vec<[f32; 2]>>,
}

/// Extrudes `profile` along +Z into a closed solid.
///
/// Vertices: each side facet owns 4 vertices (flat normals, hard edges), each cap owns one vertex
/// per profile point. Without a bevel a profile with `n` points in all rings gives `6n` vertices
/// and `2n + 2(n − 2 + 2h)` triangles (`h` holes). UVs: caps use the XY position, sides use
/// (distance along the ring, z). The result is watertight for a simple profile whose holes lie
/// inside it.
pub fn extrude(profile: &Profile, options: &ExtrudeOptions) -> Result<CpuMesh, GenError> {
    let _span = tracing::trace_span!("regia_gen::extrude").entered();
    finite("depth", &[options.depth])?;
    if options.depth <= 0.0 {
        return Err(GenError::InvalidInput("depth must be > 0".into()));
    }
    // Reorient: outer counter-clockwise, holes clockwise, so material is left of every edge.
    let mut rings: Vec<Vec<[f32; 2]>> = Vec::with_capacity(1 + profile.holes.len());
    for (r, ring) in std::iter::once(&profile.outer).chain(&profile.holes).enumerate() {
        if ring.len() < 3 {
            return Err(GenError::InvalidInput(format!(
                "ring {r} has fewer than 3 points"
            )));
        }
        finite("profile points", ring.as_flattened())?;
        let mut ring = ring.clone();
        let area = signed_area2(&ring);
        if area == 0.0 {
            return Err(GenError::InvalidInput(format!("ring {r} has zero area")));
        }
        if (r == 0) != (area > 0.0) {
            ring.reverse();
        }
        rings.push(ring);
    }
    let total: usize = rings.iter().map(Vec::len).sum();
    let levels = levels(&rings, options)?;
    let strips = levels.len() - 1;
    check_vertices(total.saturating_mul(4 * strips + 2))?;

    let mut b = Builder::default();
    // Caps first (back, then front), then the side strips from back to front.
    let back = &levels[0];
    let front = &levels[levels.len() - 1];
    cap(&mut b, back, false)?;
    cap(&mut b, front, true)?;
    for pair in levels.windows(2) {
        let (lo, hi) = (&pair[0], &pair[1]);
        for (r, (ra, rb)) in lo.rings.iter().zip(&hi.rings).enumerate() {
            let n = ra.len();
            let mut along = 0.0f32;
            for k in 0..n {
                let k1 = (k + 1) % n;
                let (a0, a1, b0, b1) = (ra[k], ra[k1], rb[k], rb[k1]);
                let dx = rings[r][k1][0] - rings[r][k][0];
                let dy = rings[r][k1][1] - rings[r][k][1];
                let len = (dx * dx + dy * dy).sqrt();
                let (u0, u1) = (along, along + len);
                along = u1;
                b.quad(
                    [
                        [a0[0], a0[1], lo.z],
                        [a1[0], a1[1], lo.z],
                        [b1[0], b1[1], hi.z],
                        [b0[0], b0[1], hi.z],
                    ],
                    [[u0, lo.z], [u1, lo.z], [u1, hi.z], [u0, hi.z]],
                );
            }
        }
    }
    Ok(b.finish())
}

/// The ring levels from the back cap to the front cap.
fn levels(rings: &[Vec<[f32; 2]>], options: &ExtrudeOptions) -> Result<Vec<Level>, GenError> {
    let depth = options.depth;
    let Some(bevel) = options.bevel else {
        return Ok(vec![
            Level {
                z: 0.0,
                rings: rings.to_vec(),
            },
            Level {
                z: depth,
                rings: rings.to_vec(),
            },
        ]);
    };
    finite("bevel width", &[bevel.width])?;
    if bevel.width <= 0.0 || bevel.width * 2.0 > depth {
        return Err(GenError::InvalidInput(
            "bevel width must be in (0, depth / 2]".into(),
        ));
    }
    if bevel.segments == 0 || bevel.segments > MAX_SEGMENTS {
        return Err(GenError::InvalidInput(format!(
            "bevel segments must be in 1..={MAX_SEGMENTS}"
        )));
    }
    let w = bevel.width;
    let k = bevel.segments;
    // Quarter-circle steps: (inset, rise) for j = 0..=k, exact at both ends.
    let steps: Vec<(f32, f32)> = (0..=k)
        .map(|j| {
            if j == 0 {
                (0.0, 0.0)
            } else if j == k {
                (w, w)
            } else {
                let theta = std::f32::consts::FRAC_PI_2 * (j as f32 / k as f32);
                let (s, c) = det::sin_cos(theta);
                (w * (1.0 - c), w * s)
            }
        })
        .collect();
    let mut inset_rings = Vec::with_capacity(steps.len());
    for &(inset, _) in &steps {
        inset_rings.push(inset_all(rings, inset, w)?);
    }
    let mut out = Vec::with_capacity(2 * steps.len());
    for j in (0..=k as usize).rev() {
        out.push(Level {
            z: w - steps[j].1,
            rings: inset_rings[j].clone(),
        });
    }
    let wall_top = depth - w;
    if wall_top == w {
        // No straight wall: the two j = 0 levels coincide.
        out.pop();
    }
    for (j, (_, rise)) in steps.iter().enumerate() {
        out.push(Level {
            z: if j == k as usize { depth } else { wall_top + rise },
            rings: inset_rings[j].clone(),
        });
    }
    Ok(out)
}

/// Every ring moved `inset` toward the material (left of each edge).
fn inset_all(rings: &[Vec<[f32; 2]>], inset: f32, width: f32) -> Result<Vec<Vec<[f32; 2]>>, GenError> {
    if inset == 0.0 {
        return Ok(rings.to_vec());
    }
    let mut out = Vec::with_capacity(rings.len());
    for (r, ring) in rings.iter().enumerate() {
        let n = ring.len();
        // Outward (right-hand) unit normals of each edge k → k+1.
        let normals: Vec<[f32; 2]> = (0..n)
            .map(|k| {
                let a = ring[k];
                let b = ring[(k + 1) % n];
                let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
                let len = (dx * dx + dy * dy).sqrt();
                if len > 0.0 {
                    [dy / len, -dx / len]
                } else {
                    [0.0, 0.0]
                }
            })
            .collect();
        let mut moved = Vec::with_capacity(n);
        for k in 0..n {
            let n1 = normals[(k + n - 1) % n];
            let n2 = normals[k];
            let denom = 1.0 + n1[0] * n2[0] + n1[1] * n2[1];
            if denom <= 1e-4 {
                return Err(GenError::BevelTooWide {
                    width,
                    ring: r,
                    edge: k,
                });
            }
            let m = [(n1[0] + n2[0]) / denom, (n1[1] + n2[1]) / denom];
            moved.push([ring[k][0] - inset * m[0], ring[k][1] - inset * m[1]]);
        }
        // No edge may turn around.
        for k in 0..n {
            let k1 = (k + 1) % n;
            let e0 = [ring[k1][0] - ring[k][0], ring[k1][1] - ring[k][1]];
            let e1 = [moved[k1][0] - moved[k][0], moved[k1][1] - moved[k][1]];
            if e0[0] * e1[0] + e0[1] * e1[1] <= 0.0 {
                return Err(GenError::BevelTooWide {
                    width,
                    ring: r,
                    edge: k,
                });
            }
        }
        out.push(moved);
    }
    Ok(out)
}

fn cap(b: &mut Builder, level: &Level, front: bool) -> Result<(), GenError> {
    let outer = &level.rings[0];
    let holes = &level.rings[1..];
    let tris = triangulate(outer, holes)?;
    let normal = if front { [0.0, 0.0, 1.0] } else { [0.0, 0.0, -1.0] };
    let base = b.mesh.positions.len() as u32;
    for p in level.rings.iter().flatten() {
        b.vertex([p[0], p[1], level.z], normal, *p);
    }
    for [i0, i1, i2] in tris {
        if front {
            b.tri(base + i0, base + i1, base + i2);
        } else {
            b.tri(base + i0, base + i2, base + i1);
        }
    }
    Ok(())
}

/// A rectangle `size` centered on the origin with rounded corners of `radius` (`0` = sharp),
/// each corner `segments` facets, counter-clockwise. Pair it with a bevel narrower than `radius`
/// to round every edge of a box.
pub fn rounded_rect(size: [f32; 2], radius: f32, segments: u32) -> Result<Vec<[f32; 2]>, GenError> {
    finite("rectangle", &[size[0], size[1], radius])?;
    let (hx, hy) = (size[0] / 2.0, size[1] / 2.0);
    if hx <= 0.0 || hy <= 0.0 {
        return Err(GenError::InvalidInput("rectangle size must be > 0".into()));
    }
    if radius < 0.0 || radius > hx.min(hy) {
        return Err(GenError::InvalidInput(
            "corner radius must be in [0, min(size) / 2]".into(),
        ));
    }
    if radius == 0.0 {
        return Ok(vec![[-hx, -hy], [hx, -hy], [hx, hy], [-hx, hy]]);
    }
    if segments == 0 || segments > MAX_SEGMENTS {
        return Err(GenError::InvalidInput(format!(
            "corner segments must be in 1..={MAX_SEGMENTS}"
        )));
    }
    let centers = [
        [hx - radius, -hy + radius],
        [hx - radius, hy - radius],
        [-hx + radius, hy - radius],
        [-hx + radius, -hy + radius],
    ];
    // Corner c sweeps from angle (c − 1)·90° to c·90°, starting at −90°.
    let unit: Vec<(f32, f32)> = (0..=segments)
        .map(|j| {
            if j == 0 {
                (0.0, 1.0)
            } else if j == segments {
                (1.0, 0.0)
            } else {
                det::sin_cos(std::f32::consts::FRAC_PI_2 * (j as f32 / segments as f32))
            }
        })
        .collect();
    let mut out = Vec::with_capacity(4 * (segments as usize + 1));
    for (c, center) in centers.iter().enumerate() {
        for &(s, co) in &unit {
            // Rotate (cos, sin) of the local angle by the corner's start angle (−90° + c·90°).
            let (x, y) = match c {
                0 => (s, -co),
                1 => (co, s),
                2 => (-s, co),
                _ => (-co, -s),
            };
            let p = [center[0] + radius * x, center[1] + radius * y];
            if out.last() != Some(&p) {
                out.push(p);
            }
        }
    }
    if out.first() == out.last() {
        out.pop();
    }
    Ok(out)
}
