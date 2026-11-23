// SPDX-License-Identifier: MPL-2.0
//! Lathe (surface of revolution) of a 2D profile around the +Y axis.
//!
//! Method: the textbook surface of revolution `(r cos φ, y, −r sin φ)` sampled at `segments`
//! equal angles from the engine's platform-independent `sin_cos`; the seam column reuses the
//! first column's bits so the surface closes exactly.

use regia_core::math::det;

use crate::build::{Builder, normalize};
use crate::{CpuMesh, GenError, MAX_RING_POINTS, MAX_SEGMENTS, check_vertices, finite};

/// Options of [`lathe`].
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LatheOptions {
    /// Facets around the axis, `3..=MAX_SEGMENTS`.
    pub segments: u32,
}

/// Revolves `profile` (points `[r, y]`, `r ≥ 0`, ordered from the bottom up along the outside)
/// around +Y into a surface whose triangles face away from the axis.
///
/// Each profile segment gets its own two vertex rings (hard edges at profile corners, smooth
/// around the axis), so the vertex count is `2 (m − 1)(segments + 1)` for `m` profile points;
/// triangles that collapse on the axis are dropped. UVs: u = angle / 360°, v = distance along the
/// profile divided by its length. The surface is watertight when the profile starts and ends on
/// the axis (`r = 0`); otherwise it is open at the ends.
pub fn lathe(profile: &[[f32; 2]], options: &LatheOptions) -> Result<CpuMesh, GenError> {
    let _span = tracing::trace_span!("regia_gen::lathe").entered();
    let n = options.segments;
    if !(3..=MAX_SEGMENTS).contains(&n) {
        return Err(GenError::InvalidInput(format!(
            "lathe segments must be in 3..={MAX_SEGMENTS}"
        )));
    }
    if profile.len() < 2 {
        return Err(GenError::InvalidInput(
            "a lathe profile needs at least 2 points".into(),
        ));
    }
    if profile.len() > MAX_RING_POINTS {
        return Err(GenError::TooLarge {
            what: "profile points",
            value: profile.len(),
            limit: MAX_RING_POINTS,
        });
    }
    finite("profile points", profile.as_flattened())?;
    if profile.iter().any(|p| p[0] < 0.0) {
        return Err(GenError::InvalidInput("lathe radii must be ≥ 0".into()));
    }
    let columns = n as usize + 1;
    check_vertices(2 * (profile.len() - 1) * columns)?;
    // Angles: column i at 360°·i/n; the seam column reuses column 0.
    let angles: Vec<(f32, f32)> = (0..n)
        .map(|i| {
            if i == 0 {
                (0.0, 1.0)
            } else {
                det::sin_cos(std::f32::consts::TAU * (i as f32 / n as f32))
            }
        })
        .collect();
    let at = |i: usize| angles[i % n as usize];
    let mut lengths = vec![0.0f32; profile.len()];
    for k in 1..profile.len() {
        let (a, b) = (profile[k - 1], profile[k]);
        let (dr, dy) = (b[0] - a[0], b[1] - a[1]);
        lengths[k] = lengths[k - 1] + (dr * dr + dy * dy).sqrt();
    }
    let total = lengths[profile.len() - 1];
    if total <= 0.0 {
        return Err(GenError::InvalidInput("the lathe profile has zero length".into()));
    }
    let mut b = Builder::default();
    for k in 0..profile.len() - 1 {
        let (p0, p1) = (profile[k], profile[k + 1]);
        let (dr, dy) = (p1[0] - p0[0], p1[1] - p0[1]);
        // Outward normal of the profile segment in the (r, y) plane.
        let n2 = normalize([dy, -dr, 0.0]);
        let ring = |b: &mut Builder, p: [f32; 2], v: f32| -> u32 {
            let first = b.mesh.positions.len() as u32;
            for i in 0..columns {
                let (s, c) = at(i);
                let pos = [p[0] * c, p[1], -(p[0] * s)];
                let nrm = [n2[0] * c, n2[1], -(n2[0] * s)];
                b.vertex(pos, nrm, [i as f32 / n as f32, v]);
            }
            first
        };
        let r0 = ring(&mut b, p0, lengths[k] / total);
        let r1 = ring(&mut b, p1, lengths[k + 1] / total);
        for i in 0..n {
            let (a0, a1) = (r0 + i, r0 + i + 1);
            let (b0, b1) = (r1 + i, r1 + i + 1);
            b.tri(a0, a1, b1);
            b.tri(a0, b1, b0);
        }
    }
    Ok(b.finish())
}
