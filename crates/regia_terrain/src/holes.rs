// SPDX-License-Identifier: MPL-2.0
//! Holes punched through the terrain surface — tunnel entrances (spec 26 § 1, ADR-088).
//!
//! A hole is a world-XZ footprint: an ellipse ([`HoleEllipse`]), an oriented rectangle, a
//! polygon, or a coverage raster in terrain space ([`HoleShape`]). Whole nodes inside a hole
//! are dropped at selection time; partially covered nodes get a hole-culled index buffer
//! (triangles whose three vertices are inside are removed) and are drawn with fragment
//! discard against [`HoleWindow`] coverage rasters, so the cut edge is per pixel, the same in
//! every pass (colour, depth, velocity, shadows) and at every LOD.

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::cdlod::QuadNode;

/// Errors returned for invalid holes masks.
#[derive(Clone, Debug, Error)]
pub enum HolesError {
    /// The entity holds no terrain to edit.
    #[error("the entity has no terrain")]
    NoTerrain,
    /// Hole radii must be finite and positive.
    #[error("hole radii must be finite and > 0, got {radii:?}")]
    BadRadii {
        /// The rejected radii `(rx, rz)`.
        radii: [f32; 2],
    },
    /// The hole centre must be finite.
    #[error("hole center must be finite, got {0:?}")]
    BadCentre([f32; 2]),
    /// A polygon needs at least three finite points.
    #[error("a hole polygon needs at least three finite points, got {0}")]
    BadPolygon(usize),
    /// A raster's coverage must hold `width × height` bytes.
    #[error("hole raster {width}×{height} needs {width}×{height} coverage bytes, got {len}")]
    BadRaster {
        /// Texels along local x.
        width: u32,
        /// Texels along local z.
        height: u32,
        /// Bytes given.
        len: usize,
    },
    /// At most [`MAX_HOLE_WINDOWS`] shapes.
    #[error("at most {MAX_HOLE_WINDOWS} hole shapes, got {0}")]
    TooManyShapes(usize),
    /// A shape's rotation must be finite.
    #[error("hole rotation must be finite, got {0}")]
    BadRotation(f32),
    /// A named hole's id is empty, longer than [`MAX_HOLE_ID_LEN`] bytes, or holds a character
    /// other than ASCII letters, digits, `_`, `-` and `.` (ADR-1595).
    #[error("hole id {0:?} must be 1..=64 ASCII letters, digits, `_`, `-` or `.`")]
    BadId(String),
    /// Two named holes share an id.
    #[error("two named holes share the id {0:?}")]
    DuplicateId(String),
    /// No named hole has this id.
    #[error("no named hole {0:?}")]
    UnknownId(String),
    /// A mesh layer fills this hole, so its footprint cannot change (ADR-8291). Opening, closing
    /// and removing still succeed.
    #[error("hole {0} is filled by a mesh layer; its shape is fixed")]
    FilledByLayer(String),
}

/// Longest named-hole id, in bytes ([`NamedHole::id`]).
pub const MAX_HOLE_ID_LEN: usize = 64;

/// `true` when `id` is a valid [`NamedHole::id`].
#[must_use]
pub fn valid_hole_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_HOLE_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// A hole with an id that gameplay opens and closes at runtime (ADR-1595, DO-4: a tunnel blasted
/// open mid-match). Format v2 of `<map>.holes.ron` stores them (`named`); the agent protocol's
/// `terrain.set_hole` adds, replaces, opens, closes and removes them one at a time.
///
/// A closed hole cuts nothing (no window, no collider cut, no pick) but keeps its slot in the
/// [`MAX_HOLE_WINDOWS`] count, so opening it never fails at runtime.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamedHole {
    /// Unique id: 1..=[`MAX_HOLE_ID_LEN`] ASCII letters, digits, `_`, `-` or `.`.
    pub id: String,
    /// Whether the hole is cut. Absent in a file = open.
    #[serde(default = "named_hole_open")]
    pub open: bool,
    /// The footprint.
    pub shape: HoleShape,
}

fn named_hole_open() -> bool {
    true
}

/// `true` when a raster's coverage holds exactly `width × height` bytes (in 64-bit arithmetic,
/// so a huge raster cannot wrap to a small product).
fn raster_len_ok(width: u32, height: u32, len: usize) -> bool {
    width != 0 && height != 0 && u64::from(width) * u64::from(height) == len as u64
}

/// One elliptical hole in world XZ space.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HoleEllipse {
    /// Hole centre `(x, z)`, metres.
    pub center: [f32; 2],
    /// Radii `(rx, rz)`, metres.
    pub radius: [f32; 2],
    /// Rotation of the ellipse axes about Y, radians.
    pub rotation_radians: f32,
}

impl HoleEllipse {
    /// Validates the ellipse.
    ///
    /// # Errors
    ///
    /// See [`HolesError`].
    pub fn validate(&self) -> Result<(), HolesError> {
        if !self.center[0].is_finite() || !self.center[1].is_finite() {
            return Err(HolesError::BadCentre(self.center));
        }
        if !self.radius[0].is_finite()
            || !self.radius[1].is_finite()
            || self.radius[0] <= 0.0
            || self.radius[1] <= 0.0
        {
            return Err(HolesError::BadRadii { radii: self.radius });
        }
        if !self.rotation_radians.is_finite() {
            return Err(HolesError::BadRotation(self.rotation_radians));
        }
        Ok(())
    }

    /// `true` when the world XZ point lies inside the ellipse.
    pub fn contains(&self, x: f32, z: f32) -> bool {
        let (s, c) = self.rotation_radians.sin_cos();
        let dx = x - self.center[0];
        let dz = z - self.center[1];
        let lx = c * dx + s * dz;
        let lz = -s * dx + c * dz;
        let nx = lx / self.radius[0];
        let nz = lz / self.radius[1];
        nx * nx + nz * nz <= 1.0
    }

    /// Conservative world-XZ bounding box of the ellipse.
    pub fn world_bounds(&self) -> ([f32; 2], [f32; 2]) {
        // Rotation-invariant bound: the circumscribed square of the rotated ellipse.
        let (s, c) = self.rotation_radians.sin_cos();
        let ex = (self.radius[0] * c.abs()).hypot(self.radius[1] * s.abs());
        let ez = (self.radius[0] * s.abs()).hypot(self.radius[1] * c.abs());
        (
            [self.center[0] - ex, self.center[1] - ez],
            [self.center[0] + ex, self.center[1] + ez],
        )
    }
}

/// A coverage raster in terrain space: `coverage` (row-major, `width × height`, 0 = ground,
/// 255 = hole) over the oriented rectangle `center ± half_extents` rotated about Y.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HoleRaster {
    /// Rectangle centre `(x, z)`, metres.
    pub center: [f32; 2],
    /// Half sizes `(along local x, along local z)`, metres.
    pub half_extents: [f32; 2],
    /// Rotation of the rectangle about Y, radians.
    pub rotation_radians: f32,
    /// Texels along local x.
    pub width: u32,
    /// Texels along local z.
    pub height: u32,
    /// Coverage, row-major (row = local z), 0..=255.
    pub coverage: Vec<u8>,
}

/// One hole footprint in world XZ. Serialised externally tagged in snake case:
/// `{"rect": {"center": [x, z], "half_extents": [hx, hz], "rotation_radians": r}}`,
/// `{"polygon": {"points": [[x, z], ...]}}`, `{"ellipse": {...}}`, `{"raster": {...}}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoleShape {
    /// An ellipse.
    Ellipse(HoleEllipse),
    /// A rectangle `center ± half_extents`, rotated about Y.
    Rect {
        /// Centre `(x, z)`, metres.
        center: [f32; 2],
        /// Half sizes `(along local x, along local z)`, metres.
        half_extents: [f32; 2],
        /// Rotation about Y, radians.
        rotation_radians: f32,
    },
    /// A simple or self-intersecting polygon (even-odd fill), world `(x, z)` points.
    Polygon {
        /// Vertices, at least three.
        points: Vec<[f32; 2]>,
    },
    /// A coverage raster (inside where coverage ≥ 50 %, bilinear).
    Raster(HoleRaster),
}

/// Rotates world `(dx, dz)` into a frame rotated by `rotation` about Y.
fn to_local(dx: f32, dz: f32, rotation: f32) -> (f32, f32) {
    let (s, c) = rotation.sin_cos();
    (c * dx + s * dz, -s * dx + c * dz)
}

impl HoleRaster {
    /// Bilinear coverage (0..=1) at world `(x, z)`; 0 outside the rectangle.
    #[must_use]
    pub fn coverage_at(&self, x: f32, z: f32) -> f32 {
        bilinear_coverage(
            self.center,
            self.half_extents,
            self.rotation_radians,
            [self.width, self.height],
            &self.coverage,
            x,
            z,
        )
    }
}

/// Bilinear coverage (0..=1) of a `size` raster over the oriented rectangle `center ± half`
/// at world `(x, z)`; 0 outside it (the sampling `terrain.wgsl`'s `in_hole` does).
fn bilinear_coverage(
    center: [f32; 2],
    half: [f32; 2],
    rotation: f32,
    size: [u32; 2],
    coverage: &[u8],
    x: f32,
    z: f32,
) -> f32 {
    let [w, h] = size;
    if !raster_len_ok(w, h, coverage.len()) {
        return 0.0;
    }
    let (lx, lz) = to_local(x - center[0], z - center[1], rotation);
    let u = (lx / half[0].max(1e-6)) * 0.5 + 0.5;
    let v = (lz / half[1].max(1e-6)) * 0.5 + 0.5;
    if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
        return 0.0;
    }
    let fx = (u * w as f32 - 0.5).clamp(0.0, (w - 1) as f32);
    let fz = (v * h as f32 - 0.5).clamp(0.0, (h - 1) as f32);
    let (x0, z0) = (fx.floor() as u32, fz.floor() as u32);
    let (x1, z1) = ((x0 + 1).min(w - 1), (z0 + 1).min(h - 1));
    let (tx, tz) = (fx - x0 as f32, fz - z0 as f32);
    let at = |x: u32, z: u32| f32::from(coverage[z as usize * w as usize + x as usize]) / 255.0;
    let a = at(x0, z0) + (at(x1, z0) - at(x0, z0)) * tx;
    let b = at(x0, z1) + (at(x1, z1) - at(x0, z1)) * tx;
    a + (b - a) * tz
}

impl HoleShape {
    /// The rectangle `width` wide running from `from` to `to` (world XZ): an open-cut ramp
    /// trench along its centreline, such as a tunnel mouth's approach.
    #[must_use]
    pub fn segment(from: [f32; 2], to: [f32; 2], width: f32) -> Self {
        let (dx, dz) = (to[0] - from[0], to[1] - from[1]);
        let len = (dx * dx + dz * dz).sqrt();
        // Local x runs along the segment: (cos, sin) of the rotation is its direction.
        Self::Rect {
            center: [(from[0] + to[0]) * 0.5, (from[1] + to[1]) * 0.5],
            half_extents: [len * 0.5, width * 0.5],
            rotation_radians: dz.atan2(dx),
        }
    }

    /// A mouth cut on the ground lattice as one hole (spec 26 § 2a.4 item 2, ADR-8214): the cut's
    /// ring loops, each a closed list of lattice vertices `(x, z)` (the first not repeated), as one
    /// even-odd polygon. Loops after the first are joined to it by a bridge walked once each way,
    /// which the even-odd fill cancels, so the polygon covers exactly the inside of the loops.
    ///
    /// Every lattice triangle centroid is at least a sixth of a cell from any lattice edge, so
    /// [`Self::contains`] at the centroids reproduces the cut cell for cell. `None` for no loop or a
    /// loop of fewer than three points.
    #[must_use]
    pub fn lattice_rings(loops: &[Vec<[f32; 2]>]) -> Option<Self> {
        let first = loops.first()?;
        if loops.iter().any(|l| l.len() < 3) {
            return None;
        }
        let mut points = first.clone();
        for other in &loops[1..] {
            points.push(first[0]);
            points.extend_from_slice(other);
            points.push(other[0]);
        }
        Some(Self::Polygon { points })
    }

    /// Validates the shape.
    ///
    /// # Errors
    ///
    /// See [`HolesError`].
    pub fn validate(&self) -> Result<(), HolesError> {
        match self {
            Self::Ellipse(e) => e.validate(),
            Self::Rect {
                center,
                half_extents,
                rotation_radians,
            } => {
                if !center.iter().all(|v| v.is_finite()) {
                    return Err(HolesError::BadCentre(*center));
                }
                if !half_extents.iter().all(|v| v.is_finite() && *v > 0.0) {
                    return Err(HolesError::BadRadii { radii: *half_extents });
                }
                if !rotation_radians.is_finite() {
                    return Err(HolesError::BadRotation(*rotation_radians));
                }
                Ok(())
            }
            Self::Polygon { points } => {
                if points.len() < 3
                    || points.len() > MAX_POLYGON_POINTS
                    || !points.iter().flatten().all(|v| v.is_finite())
                {
                    return Err(HolesError::BadPolygon(points.len()));
                }
                // Points all on one axis-aligned line have a zero-width box: the GPU window's
                // 1 / half extent would be infinite.
                let (_, half, _) = self.window();
                if !(half[0] > 0.0 && half[1] > 0.0) {
                    return Err(HolesError::BadPolygon(points.len()));
                }
                Ok(())
            }
            Self::Raster(r) => {
                if !raster_len_ok(r.width, r.height, r.coverage.len()) {
                    return Err(HolesError::BadRaster {
                        width: r.width,
                        height: r.height,
                        len: r.coverage.len(),
                    });
                }
                if !r.center.iter().all(|v| v.is_finite()) {
                    return Err(HolesError::BadCentre(r.center));
                }
                if !r.half_extents.iter().all(|v| v.is_finite() && *v > 0.0) {
                    return Err(HolesError::BadRadii {
                        radii: r.half_extents,
                    });
                }
                if !r.rotation_radians.is_finite() {
                    return Err(HolesError::BadRotation(r.rotation_radians));
                }
                Ok(())
            }
        }
    }

    /// `true` when world `(x, z)` lies inside the shape.
    #[must_use]
    pub fn contains(&self, x: f32, z: f32) -> bool {
        match self {
            Self::Ellipse(e) => e.contains(x, z),
            Self::Rect {
                center,
                half_extents,
                rotation_radians,
            } => {
                let (lx, lz) = to_local(x - center[0], z - center[1], *rotation_radians);
                lx.abs() <= half_extents[0] && lz.abs() <= half_extents[1]
            }
            Self::Polygon { points } => {
                // A degenerate polygon (built without `validate`) contains nothing.
                if points.len() < 3 {
                    return false;
                }
                let mut inside = false;
                let mut j = points.len() - 1;
                for i in 0..points.len() {
                    let (pi, pj) = (points[i], points[j]);
                    if (pi[1] > z) != (pj[1] > z)
                        && x < (pj[0] - pi[0]) * (z - pi[1]) / (pj[1] - pi[1]) + pi[0]
                    {
                        inside = !inside;
                    }
                    j = i;
                }
                inside
            }
            Self::Raster(r) => r.coverage_at(x, z) >= 0.5,
        }
    }

    /// The window a [`HoleWindow`] raster of this shape covers: centre, half extents and
    /// rotation (the shape's own oriented box; polygons take their axis-aligned box).
    #[must_use]
    pub fn window(&self) -> ([f32; 2], [f32; 2], f32) {
        match self {
            Self::Ellipse(e) => (e.center, e.radius, e.rotation_radians),
            Self::Rect {
                center,
                half_extents,
                rotation_radians,
            } => (*center, *half_extents, *rotation_radians),
            Self::Polygon { points } => {
                let (mut lo, mut hi) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
                for p in points {
                    lo = [lo[0].min(p[0]), lo[1].min(p[1])];
                    hi = [hi[0].max(p[0]), hi[1].max(p[1])];
                }
                (
                    [(lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5],
                    [(hi[0] - lo[0]) * 0.5, (hi[1] - lo[1]) * 0.5],
                    0.0,
                )
            }
            Self::Raster(r) => (r.center, r.half_extents, r.rotation_radians),
        }
    }

    /// Conservative world-XZ bounding box.
    #[must_use]
    pub fn world_bounds(&self) -> ([f32; 2], [f32; 2]) {
        let (c, h, rot) = self.window();
        let (s, co) = rot.sin_cos();
        let ex = (h[0] * co).abs() + (h[1] * s).abs();
        let ez = (h[0] * s).abs() + (h[1] * co).abs();
        ([c[0] - ex, c[1] - ez], [c[0] + ex, c[1] + ez])
    }
}

/// Texels per side of a [`HoleWindow`] coverage raster.
pub const HOLE_WINDOW_TEXELS: u32 = 256;

/// Most hole windows a terrain draws (one per shape).
pub const MAX_HOLE_WINDOWS: usize = 64;

/// Most vertices one hole polygon may have (`contains` and the height cut are linear in it).
pub const MAX_POLYGON_POINTS: usize = 4096;

/// One shape's coverage raster for the GPU: [`HOLE_WINDOW_TEXELS`]² texels over the oriented
/// window `center ± half_extents` (the shape's box plus a two-texel margin), each texel the
/// shape's coverage from 4 × 4 samples. The cut is where the bilinear coverage reaches 50 %.
#[derive(Clone, Debug, PartialEq)]
pub struct HoleWindow {
    /// Window centre `(x, z)`, metres.
    pub center: [f32; 2],
    /// Window half sizes, metres.
    pub half_extents: [f32; 2],
    /// Window rotation about Y, radians.
    pub rotation_radians: f32,
    /// Coverage, row-major (row = local z), [`HOLE_WINDOW_TEXELS`]² bytes.
    pub coverage: Vec<u8>,
}

impl HoleWindow {
    /// Rasterises `shape`.
    #[must_use]
    pub fn rasterize(shape: &HoleShape) -> Self {
        let n = HOLE_WINDOW_TEXELS;
        let (center, half, rot) = shape.window();
        // Two texels of margin on every side so the 50 % edge never touches the border.
        let margin = 2.0 / (n as f32 - 4.0);
        let half = [half[0] * (1.0 + 2.0 * margin), half[1] * (1.0 + 2.0 * margin)];
        let (s, c) = rot.sin_cos();
        let mut coverage = Vec::with_capacity((n * n) as usize);
        for j in 0..n {
            for i in 0..n {
                let mut hits = 0u32;
                for sj in 0..4 {
                    for si in 0..4 {
                        let lu = ((i as f32 + (si as f32 + 0.5) / 4.0) / n as f32) * 2.0 - 1.0;
                        let lv = ((j as f32 + (sj as f32 + 0.5) / 4.0) / n as f32) * 2.0 - 1.0;
                        let (lx, lz) = (lu * half[0], lv * half[1]);
                        // Local → world (inverse of `to_local`).
                        let x = center[0] + c * lx - s * lz;
                        let z = center[1] + s * lx + c * lz;
                        if shape.contains(x, z) {
                            hits += 1;
                        }
                    }
                }
                coverage.push(((hits * 255 + 8) / 16) as u8);
            }
        }
        Self {
            center,
            half_extents: half,
            rotation_radians: rot,
            coverage,
        }
    }

    /// Bilinear coverage (0..=1) at world `(x, z)`, as the GPU samples it.
    #[must_use]
    pub fn coverage_at(&self, x: f32, z: f32) -> f32 {
        bilinear_coverage(
            self.center,
            self.half_extents,
            self.rotation_radians,
            [HOLE_WINDOW_TEXELS; 2],
            &self.coverage,
            x,
            z,
        )
    }
}

/// Word 6 of a lattice hole window (a coverage window's word 6 is `0`).
pub const LATTICE_WINDOW_MODE: u32 = 1;

/// Lattice cells per side of one [`LatticeWindow`] (one byte per cell, the coverage block's size).
pub const LATTICE_WINDOW_CELLS: u32 = HOLE_WINDOW_TEXELS;

/// Bit of a [`LatticeWindow`] cell byte: the lower triangle `[v00, v01, v10]` is cut.
pub const LATTICE_CUT_LOWER: u8 = 1;
/// Bit of a [`LatticeWindow`] cell byte: the upper triangle `[v10, v01, v11]` is cut.
pub const LATTICE_CUT_UPPER: u8 = 2;
/// Bit of a [`LatticeWindow`] cell byte: the cell lies within [`LATTICE_RING_FADE_CELLS`] cells
/// of a cut cell, where cliff relief is off so the drawn ring stays on the lattice ring.
pub const LATTICE_NEAR_RING: u8 = 4;
/// How many lattice cells around a cut cell keep no cliff relief.
pub const LATTICE_RING_FADE_CELLS: u32 = 2;

/// One window of a [`LatticeHoleCut`]: up to [`LATTICE_WINDOW_CELLS`]² lattice cells from cell
/// `cell0`, one byte per cell (row-major, row = z): [`LATTICE_CUT_LOWER`], [`LATTICE_CUT_UPPER`],
/// [`LATTICE_NEAR_RING`].
#[derive(Clone, Debug, PartialEq)]
pub struct LatticeWindow {
    /// The first cell `(i, j)`.
    pub cell0: [u32; 2],
    /// Cells covered along x and z (each at most [`LATTICE_WINDOW_CELLS`]).
    pub size: [u32; 2],
    /// World `(x, z)` of the window's corner: `origin + cell0 * step` in `f32`.
    pub corner: [f32; 2],
    /// [`LATTICE_WINDOW_CELLS`]² bytes; cells past `size` are zero.
    pub bytes: Vec<u8>,
}

impl LatticeWindow {
    /// The cell byte under world `(x, z)` and whether the point is in the upper triangle, with the
    /// arithmetic `terrain.wgsl`'s `lattice_byte` runs (`f32`: `(p - corner) * inv_step`, floor, the
    /// diagonal `fu + fv > 1`); `None` outside the window.
    #[must_use]
    pub fn byte_at(&self, x: f32, z: f32, inv_step: f32) -> Option<(u8, bool)> {
        let fx = (x - self.corner[0]) * inv_step;
        let fz = (z - self.corner[1]) * inv_step;
        let (sx, sz) = (self.size[0] as f32, self.size[1] as f32);
        if !(fx >= 0.0 && fz >= 0.0 && fx < sx && fz < sz) {
            return None;
        }
        let (cx, cz) = (fx.floor(), fz.floor());
        let upper = (fx - cx) + (fz - cz) > 1.0;
        let i = cz as usize * LATTICE_WINDOW_CELLS as usize + cx as usize;
        Some((self.bytes.get(i).copied().unwrap_or(0), upper))
    }
}

/// One filled hole's lattice windows.
#[derive(Clone, Debug, PartialEq)]
pub struct LatticeHoleWindows {
    /// The named hole id.
    pub hole: String,
    /// Its windows, covering the cells around its cut.
    pub windows: Vec<LatticeWindow>,
}

/// The ground-lattice cut of the holes mesh layers fill, as the GPU draws it (spec 26 § 2a.4
/// item 2, ADR-8470): the per-pixel test reads, at the interpolated pre-morph grid position, the
/// lattice triangle under it and whether that triangle is cut, so the drawn cut is the cooked cut
/// cell for cell at level 0 and its affine image on every coarser or morphing triangle.
///
/// The lattice is the terrain's sample grid: cell `(i, j)` spans samples `i..=i + 1` and
/// `j..=j + 1`, split by the `(x0, z1)-(x1, z0)` diagonal.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LatticeHoleCut {
    /// World `(x, z)` of sample `(0, 0)`.
    pub origin: [f32; 2],
    /// The sample pitch, metres.
    pub step: f32,
    /// `1 / step` as the GPU reads it.
    pub inv_step: f32,
    /// Cells per side of the lattice.
    pub cells: [u32; 2],
    /// One row per filled hole, sorted by id.
    pub holes: Vec<LatticeHoleWindows>,
}

impl LatticeHoleCut {
    /// An empty cut on a lattice of `cells` cells from `origin` at pitch `step`.
    #[must_use]
    pub fn new(origin: [f32; 2], step: f32, cells: [u32; 2]) -> Self {
        Self {
            origin,
            step,
            inv_step: 1.0 / step.max(1e-9),
            cells,
            holes: Vec::new(),
        }
    }

    /// Adds (or replaces) hole `hole`: the cells `[lo, hi)` (clamped to the lattice) in windows
    /// of [`LATTICE_WINDOW_CELLS`], each triangle cut where `is_cut(i, j, t)` (`t` 0 lower, 1
    /// upper), and [`LATTICE_NEAR_RING`] set on cells within [`LATTICE_RING_FADE_CELLS`] of a cut
    /// cell (the range is grown by that much so the band reaches past the cut).
    pub fn set_hole(
        &mut self,
        hole: &str,
        lo: [u32; 2],
        hi: [u32; 2],
        is_cut: impl Fn(u32, u32, u8) -> bool,
    ) {
        let fade = LATTICE_RING_FADE_CELLS;
        let lo = [lo[0].saturating_sub(fade), lo[1].saturating_sub(fade)];
        let hi = [
            (hi[0] + fade).min(self.cells[0]),
            (hi[1] + fade).min(self.cells[1]),
        ];
        let span = [hi[0].saturating_sub(lo[0]), hi[1].saturating_sub(lo[1])];
        let side = LATTICE_WINDOW_CELLS as usize;
        // The cut cells of the range, so the near-ring bit is one pass.
        let w = span[0] as usize;
        let h = span[1] as usize;
        let mut cut = vec![0u8; w * h];
        for dj in 0..h {
            for di in 0..w {
                let (i, j) = (lo[0] + di as u32, lo[1] + dj as u32);
                let mut b = 0u8;
                if is_cut(i, j, 0) {
                    b |= LATTICE_CUT_LOWER;
                }
                if is_cut(i, j, 1) {
                    b |= LATTICE_CUT_UPPER;
                }
                cut[dj * w + di] = b;
            }
        }
        let f = fade as usize;
        let near = |di: usize, dj: usize| -> bool {
            let (i0, i1) = (di.saturating_sub(f), (di + f).min(w.saturating_sub(1)));
            let (j0, j1) = (dj.saturating_sub(f), (dj + f).min(h.saturating_sub(1)));
            (j0..=j1).any(|j| (i0..=i1).any(|i| cut[j * w + i] != 0))
        };
        let mut windows = Vec::new();
        let mut wj = 0usize;
        while wj < h {
            let mut wi = 0usize;
            while wi < w {
                let size = [(w - wi).min(side) as u32, (h - wj).min(side) as u32];
                let mut bytes = vec![0u8; side * side];
                for dj in 0..size[1] as usize {
                    for di in 0..size[0] as usize {
                        let (gi, gj) = (wi + di, wj + dj);
                        let mut b = cut[gj * w + gi];
                        if near(gi, gj) {
                            b |= LATTICE_NEAR_RING;
                        }
                        bytes[dj * side + di] = b;
                    }
                }
                let cell0 = [lo[0] + wi as u32, lo[1] + wj as u32];
                windows.push(LatticeWindow {
                    cell0,
                    size,
                    corner: [
                        self.origin[0] + cell0[0] as f32 * self.step,
                        self.origin[1] + cell0[1] as f32 * self.step,
                    ],
                    bytes,
                });
                wi += side;
            }
            wj += side;
        }
        let row = LatticeHoleWindows {
            hole: hole.to_owned(),
            windows,
        };
        match self.holes.binary_search_by(|r| r.hole.as_str().cmp(hole)) {
            Ok(k) => self.holes[k] = row,
            Err(k) => self.holes.insert(k, row),
        }
    }

    /// The windows of hole `id`.
    #[must_use]
    pub fn hole(&self, id: &str) -> Option<&LatticeHoleWindows> {
        self.holes
            .binary_search_by(|r| r.hole.as_str().cmp(id))
            .ok()
            .map(|k| &self.holes[k])
    }

    /// Windows of every hole, in hole order.
    pub fn windows(&self) -> impl Iterator<Item = &LatticeWindow> + '_ {
        self.holes.iter().flat_map(|h| h.windows.iter())
    }

    /// `true` where world `(x, z)` (the pre-morph grid position) lies on a cut lattice triangle of
    /// any hole: the CPU mirror of `terrain.wgsl`'s lattice test, bit for bit.
    #[must_use]
    pub fn cuts(&self, x: f32, z: f32) -> bool {
        self.windows().any(|w| {
            w.byte_at(x, z, self.inv_step).is_some_and(|(b, upper)| {
                let bit = if upper {
                    LATTICE_CUT_UPPER
                } else {
                    LATTICE_CUT_LOWER
                };
                b & bit != 0
            })
        })
    }

    /// `true` where world `(x, z)` lies on a cut lattice triangle of hole `id` (the per-hole
    /// [`Self::cuts`]); `false` when the cut does not hold `id`.
    #[must_use]
    pub fn cuts_hole(&self, id: &str, x: f32, z: f32) -> bool {
        self.hole(id).is_some_and(|h| {
            h.windows.iter().any(|w| {
                w.byte_at(x, z, self.inv_step).is_some_and(|(b, upper)| {
                    let bit = if upper {
                        LATTICE_CUT_UPPER
                    } else {
                        LATTICE_CUT_LOWER
                    };
                    b & bit != 0
                })
            })
        })
    }

    /// `true` where world `(x, z)` lies in a cell that keeps no cliff relief
    /// ([`LATTICE_NEAR_RING`]).
    #[must_use]
    pub fn near_ring(&self, x: f32, z: f32) -> bool {
        self.windows().any(|w| {
            w.byte_at(x, z, self.inv_step)
                .is_some_and(|(b, _)| b & LATTICE_NEAR_RING != 0)
        })
    }

    /// `true` when every lattice triangle of every cell the world rectangle touches is cut (a
    /// node triangle over it may be dropped); `false` when any is ground or off the windows.
    #[must_use]
    pub fn covers_rect(&self, min: [f32; 2], max: [f32; 2]) -> bool {
        let cell = |v: f32, o: f32, n: u32| -> i64 {
            (((v - o) * self.inv_step).floor() as i64).clamp(-1, i64::from(n))
        };
        let (i0, i1) = (
            cell(min[0], self.origin[0], self.cells[0]),
            cell(max[0], self.origin[0], self.cells[0]),
        );
        let (j0, j1) = (
            cell(min[1], self.origin[1], self.cells[1]),
            cell(max[1], self.origin[1], self.cells[1]),
        );
        // A rectangle edge on a cell border also touches the neighbour (rounding either way).
        let (i0, i1, j0, j1) = (i0 - 1, i1 + 1, j0 - 1, j1 + 1);
        if i0 < 0 || j0 < 0 || i1 >= i64::from(self.cells[0]) || j1 >= i64::from(self.cells[1]) {
            return false;
        }
        (j0..=j1).all(|j| (i0..=i1).all(|i| self.cell_byte(i as u32, j as u32) & 3 == 3))
    }

    /// `true` when the world rectangle overlaps any window.
    #[must_use]
    pub fn touches_rect(&self, min: [f32; 2], max: [f32; 2]) -> bool {
        self.windows().any(|w| {
            let hi = [
                w.corner[0] + w.size[0] as f32 * self.step,
                w.corner[1] + w.size[1] as f32 * self.step,
            ];
            min[0] <= hi[0] && w.corner[0] <= max[0] && min[1] <= hi[1] && w.corner[1] <= max[1]
        })
    }

    /// The byte of lattice cell `(i, j)` (OR over the windows that hold it; `0` in none).
    #[must_use]
    pub fn cell_byte(&self, i: u32, j: u32) -> u8 {
        self.windows()
            .filter(|w| {
                i >= w.cell0[0] && j >= w.cell0[1] && i - w.cell0[0] < w.size[0] && j - w.cell0[1] < w.size[1]
            })
            .fold(0u8, |acc, w| {
                let k = (j - w.cell0[1]) as usize * LATTICE_WINDOW_CELLS as usize + (i - w.cell0[0]) as usize;
                acc | w.bytes.get(k).copied().unwrap_or(0)
            })
    }
}

/// A set of holes (typically tunnel entrances) for one terrain.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HolesMask {
    /// Elliptical holes (the original form).
    pub holes: Vec<HoleEllipse>,
    /// Further footprints: rectangles, polygons, coverage rasters (ellipses too).
    #[serde(default)]
    pub shapes: Vec<HoleShape>,
    /// Holes with ids that open and close at runtime (format v2, ADR-1595), sorted by id
    /// ([`Self::set_named`] keeps the order; a file is sorted on load), so the GPU windows and
    /// the mask's hash do not depend on the order the holes were opened in. Only open ones cut.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub named: Vec<NamedHole>,
}

impl HolesMask {
    /// Builds a mask from ellipses, validating each.
    ///
    /// # Errors
    ///
    /// See [`HolesError`]; more than [`MAX_HOLE_WINDOWS`] ellipses is
    /// [`HolesError::TooManyShapes`] (the GPU draws one window per shape and would drop the
    /// rest while picks and the collider still cut them).
    pub fn new(holes: Vec<HoleEllipse>) -> Result<Self, HolesError> {
        if holes.len() > MAX_HOLE_WINDOWS {
            return Err(HolesError::TooManyShapes(holes.len()));
        }
        for hole in &holes {
            hole.validate()?;
        }
        Ok(Self {
            holes,
            shapes: Vec::new(),
            named: Vec::new(),
        })
    }

    /// Builds a mask from any footprints, validating each.
    ///
    /// # Errors
    ///
    /// See [`HolesError`]; more than [`MAX_HOLE_WINDOWS`] shapes is
    /// [`HolesError::TooManyShapes`].
    pub fn from_shapes(shapes: Vec<HoleShape>) -> Result<Self, HolesError> {
        if shapes.len() > MAX_HOLE_WINDOWS {
            return Err(HolesError::TooManyShapes(shapes.len()));
        }
        for shape in &shapes {
            shape.validate()?;
        }
        Ok(Self {
            holes: Vec::new(),
            shapes,
            named: Vec::new(),
        })
    }

    /// Every footprint that cuts: ellipses first, then `shapes`, then the open named holes
    /// in id order.
    pub fn all_shapes(&self) -> impl Iterator<Item = HoleShape> + '_ {
        self.holes
            .iter()
            .cloned()
            .map(HoleShape::Ellipse)
            .chain(self.shapes.iter().cloned())
            .chain(self.open_named().map(|n| n.shape.clone()))
    }

    /// Every footprint that cuts, in [`Self::all_shapes`] order, without copying the polygons'
    /// points or the rasters' coverage ([`Self::all_shapes`] clones each shape; the per-node
    /// tests run per frame).
    fn shapes_ref(&self) -> impl Iterator<Item = std::borrow::Cow<'_, HoleShape>> + '_ {
        self.holes
            .iter()
            .map(|e| std::borrow::Cow::Owned(HoleShape::Ellipse(e.clone())))
            .chain(self.shapes.iter().map(std::borrow::Cow::Borrowed))
            .chain(self.open_named().map(|n| std::borrow::Cow::Borrowed(&n.shape)))
    }

    /// The named holes that are open, in id order.
    fn open_named(&self) -> impl Iterator<Item = &NamedHole> + '_ {
        self.named.iter().filter(|n| n.open)
    }

    /// Footprints that cut ([`Self::all_shapes`]): ellipses, shapes and open named holes.
    #[must_use]
    pub fn shape_count(&self) -> usize {
        self.holes.len() + self.shapes.len() + self.open_named().count()
    }

    /// `true` when the mask cuts nothing (closed named holes cut nothing).
    pub fn is_empty(&self) -> bool {
        self.holes.is_empty() && self.shapes.is_empty() && !self.named.iter().any(|n| n.open)
    }

    /// `true` when the world XZ point lies inside any hole.
    pub fn contains(&self, x: f32, z: f32) -> bool {
        self.holes.iter().any(|h| h.contains(x, z))
            || self.shapes.iter().any(|h| h.contains(x, z))
            || self.open_named().any(|n| n.shape.contains(x, z))
    }

    /// [`Self::contains`] as the draw cuts with `lattice` (ADR-8537, spec 26 § 2a.6 "Pick"): an
    /// open named hole that `lattice` holds is cut where its lattice triangles are
    /// ([`LatticeHoleCut::cuts_hole`]), not by its coverage shape; every other footprint is its
    /// shape, as in [`Self::gpu_words_with_lattice`]. With `None` this is [`Self::contains`].
    #[must_use]
    pub fn contains_drawn(&self, x: f32, z: f32, lattice: Option<&LatticeHoleCut>) -> bool {
        let Some(lattice) = lattice else {
            return self.contains(x, z);
        };
        self.holes.iter().any(|h| h.contains(x, z))
            || self.shapes.iter().any(|h| h.contains(x, z))
            || self.open_named().any(|n| match lattice.hole(&n.id) {
                Some(_) => lattice.cuts_hole(&n.id, x, z),
                None => n.shape.contains(x, z),
            })
    }

    /// The named hole `id`.
    #[must_use]
    pub fn named_hole(&self, id: &str) -> Option<&NamedHole> {
        self.named
            .binary_search_by(|n| n.id.as_str().cmp(id))
            .ok()
            .map(|i| &self.named[i])
    }

    /// Adds `hole`, or replaces the named hole with its id (keeping `named` sorted by id).
    /// Returns the hole it replaced. The mask is left unchanged on error.
    ///
    /// # Errors
    ///
    /// [`HolesError::BadId`], an invalid shape, or [`HolesError::TooManyShapes`] when a new
    /// hole would pass [`MAX_HOLE_WINDOWS`] footprints (open or closed).
    pub fn set_named(&mut self, hole: NamedHole) -> Result<Option<NamedHole>, HolesError> {
        if !valid_hole_id(&hole.id) {
            return Err(HolesError::BadId(hole.id));
        }
        hole.shape.validate()?;
        match self.named.binary_search_by(|n| n.id.cmp(&hole.id)) {
            Ok(i) => Ok(Some(std::mem::replace(&mut self.named[i], hole))),
            Err(i) => {
                let count = self.holes.len() + self.shapes.len() + self.named.len() + 1;
                if count > MAX_HOLE_WINDOWS {
                    return Err(HolesError::TooManyShapes(count));
                }
                self.named.insert(i, hole);
                Ok(None)
            }
        }
    }

    /// Opens or closes the named hole `id`. Returns whether it was open.
    ///
    /// # Errors
    ///
    /// [`HolesError::UnknownId`] when no named hole has that id.
    pub fn set_open(&mut self, id: &str, open: bool) -> Result<bool, HolesError> {
        let i = self
            .named
            .binary_search_by(|n| n.id.as_str().cmp(id))
            .map_err(|_| HolesError::UnknownId(id.to_owned()))?;
        Ok(std::mem::replace(&mut self.named[i].open, open))
    }

    /// Removes the named hole `id` and returns it (`None` when there is none).
    pub fn remove_named(&mut self, id: &str) -> Option<NamedHole> {
        let i = self.named.binary_search_by(|n| n.id.as_str().cmp(id)).ok()?;
        Some(self.named.remove(i))
    }

    /// `true` when the world rectangle could touch any hole (conservative).
    pub fn intersects_rect(&self, min_x: f32, min_z: f32, size: f32) -> bool {
        self.shapes_ref().any(|h| {
            let ([bx0, bz0], [bx1, bz1]) = h.world_bounds();
            bx0 <= min_x + size && min_x <= bx1 && bz0 <= min_z + size && min_z <= bz1
        })
    }

    /// `true` when the world rectangle lies entirely inside one hole (node can be dropped).
    /// Corners and edge midpoints are tested, each two coverage-window texels deep (so the
    /// drawn, rasterised edge never runs inside a dropped node), and for polygons and rasters a
    /// 9 × 9 lattice, since those need not be convex. A lattice can miss a strip of ground
    /// narrower than its spacing (a C-shaped polygon's arm), so a polygon also needs no edge
    /// crossing the rectangle (grown by the margin), and a raster every coverage texel its
    /// bilinear samples there read at 50 % or more (audit-seq Low: such a strip was dropped).
    pub fn contains_rect(&self, min_x: f32, min_z: f32, size: f32) -> bool {
        let m = 2.0 * self.window_texel_m();
        let grown = ([min_x - m, min_z - m], [min_x + size + m, min_z + size + m]);
        self.shapes_ref().any(|h| {
            let n = match &*h {
                HoleShape::Ellipse(_) | HoleShape::Rect { .. } => 2,
                _ => 8,
            };
            let lattice = (0..=n).all(|j| {
                (0..=n).all(|i| {
                    let (x, z) = (
                        min_x + size * i as f32 / n as f32,
                        min_z + size * j as f32 / n as f32,
                    );
                    (-1..=1).all(|b| (-1..=1).all(|a| h.contains(x + a as f32 * m, z + b as f32 * m)))
                })
            });
            lattice
                && match &*h {
                    HoleShape::Polygon { points } => !polygon_edge_crosses(points, grown),
                    HoleShape::Raster(raster) => raster_covers(raster, grown),
                    HoleShape::Ellipse(_) | HoleShape::Rect { .. } => true,
                }
        })
    }

    /// `true` when `(x, z)` and its eight neighbours at distance `margin` are all inside.
    pub fn contains_deep(&self, x: f32, z: f32, margin: f32) -> bool {
        (-1..=1).all(|j| (-1..=1).all(|i| self.contains(x + i as f32 * margin, z + j as f32 * margin)))
    }

    /// The largest GPU coverage-window texel, metres: the most the drawn (rasterised) cut edge
    /// can differ from the exact shape edge, before the bilinear 50 % threshold.
    pub fn window_texel_m(&self) -> f32 {
        self.shapes_ref()
            .map(|s| {
                let (_, h, _) = s.window();
                2.0 * h[0].max(h[1]) / (HOLE_WINDOW_TEXELS as f32 - 4.0)
            })
            .fold(0.0, f32::max)
    }

    /// Sets to NaN every height whose sample lies in a hole, or within `margin` metres of
    /// one on both axes. Equal reaches probe a 3 × 3 lattice. A rectangular grid uses
    /// `cut_heights_xz` with half the X spacing and half the Z spacing, each axis probed so
    /// its outermost probe is exactly its reach, which is what
    /// [`crate::TerrainMap::collision_heightfield`] does. `xz(i)` is sample `i`'s world
    /// `(x, z)`. `regia_physics`'
    /// `ColliderShape::HeightField` removes every cell touching a NaN sample, and
    /// [`crate::TerrainSharedDraw::rendered_heights`] reports NaN inside holes the same way.
    /// Returns the number of samples cut.
    pub fn cut_heights(&self, heights: &mut [f32], margin: f32, xz: impl Fn(usize) -> [f32; 2]) -> usize {
        self.cut_heights_xz(heights, margin, margin, xz)
    }

    /// [`Self::cut_heights`] with a separate reach along X and along Z.
    pub(crate) fn cut_heights_xz(
        &self,
        heights: &mut [f32],
        margin_x: f32,
        margin_z: f32,
        xz: impl Fn(usize) -> [f32; 2],
    ) -> usize {
        if self.is_empty() {
            return 0;
        }
        let mut cut = 0;
        for (i, h) in heights.iter_mut().enumerate() {
            let [x, z] = xz(i);
            if self.cuts_xz(x, z, margin_x, margin_z) {
                *h = f32::NAN;
                cut += 1;
            }
        }
        cut
    }

    /// `true` when a height sample at world `(x, z)` is cut ([`Self::cut_heights`]): inside a
    /// hole, or within `margin` metres of one on both axes (a 3 × 3 lattice of offsets).
    #[must_use]
    pub fn cuts(&self, x: f32, z: f32, margin: f32) -> bool {
        self.cuts_xz(x, z, margin, margin)
    }

    /// [`Self::cuts`] with a separate reach along X and along Z.
    ///
    /// Equal reaches (every square grid) probe the historical 3 × 3 lattice at `0` and
    /// `±margin`. Unequal reaches probe each axis at `n = ceil(reach / finer)` steps of
    /// `reach / n`, so the outermost probe sits exactly on the reach even when the ratio
    /// is not an integer (`round` used to leave the coarse axis short, which could leave a
    /// hole uncut entirely). Each axis is capped at 8 steps.
    #[must_use]
    pub(crate) fn cuts_xz(&self, x: f32, z: f32, margin_x: f32, margin_z: f32) -> bool {
        if !(margin_x > 0.0 || margin_z > 0.0) {
            return self.contains(x, z);
        }
        let mx = margin_x.max(0.0);
        let mz = margin_z.max(0.0);
        // A zero reach, or two equal reaches: the 3 × 3 lattice, including its signs.
        if mx == 0.0 || mz == 0.0 || mx == mz {
            return (-1..=1)
                .any(|b| (-1..=1).any(|a| self.contains(x + a as f32 * margin_x, z + b as f32 * margin_z)));
        }
        let step = mx.min(mz);
        let steps = |reach: f32| -> i32 { ((reach / step).ceil() as i32).clamp(1, 8) };
        let nx = steps(mx);
        let nz = steps(mz);
        let dx = mx / nx as f32;
        let dz = mz / nz as f32;
        (-nz..=nz).any(|b| (-nx..=nx).any(|a| self.contains(x + a as f32 * dx, z + b as f32 * dz)))
    }

    /// The storage words `terrain.wgsl`'s `in_hole` reads: `[count, 0, 0, 0]`, then per
    /// window eight words (centre x, z, cos and sin of the rotation, 1 / half extent x, z,
    /// two zero; f32 bits), then each window's coverage, four texels a word (little end
    /// first).
    #[must_use]
    pub fn gpu_words(&self) -> Vec<u32> {
        let windows = self.windows();
        let texels = (HOLE_WINDOW_TEXELS * HOLE_WINDOW_TEXELS) as usize;
        let mut words = Vec::with_capacity(4 + windows.len() * (8 + texels / 4));
        words.extend_from_slice(&[windows.len() as u32, 0, 0, 0]);
        for w in &windows {
            let (s, c) = w.rotation_radians.sin_cos();
            words.extend(
                [
                    w.center[0],
                    w.center[1],
                    c,
                    s,
                    1.0 / w.half_extents[0].max(1e-6),
                    1.0 / w.half_extents[1].max(1e-6),
                    0.0,
                    0.0,
                ]
                .map(f32::to_bits),
            );
        }
        for w in &windows {
            words.extend(
                w.coverage
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| u32::from_le_bytes(*b)),
            );
        }
        words
    }

    /// [`Self::gpu_words`] with the filled holes of `lattice` written as lattice windows
    /// (spec 26 § 2a.4 item 2, ADR-8470). An open named hole that `lattice` holds becomes its
    /// [`LatticeWindow`]s, in its place in [`Self::all_shapes`] order: word 6 of each is
    /// [`LATTICE_WINDOW_MODE`], words 0-1 the window's corner, 2-3 its size in cells, 4-5 one over
    /// the lattice pitch, and its block one byte per cell ([`LatticeWindow::bytes`]). Every other
    /// window is the coverage window of [`Self::gpu_words`], word for word; with `None`, or a
    /// lattice cut that fills none of the open named holes, the words are [`Self::gpu_words`]'s.
    ///
    /// # Errors
    ///
    /// [`HolesError::TooManyShapes`] when the windows (a wide mouth takes several) pass
    /// [`MAX_HOLE_WINDOWS`].
    pub fn gpu_words_with_lattice(&self, lattice: Option<&LatticeHoleCut>) -> Result<Vec<u32>, HolesError> {
        let Some(lattice) = lattice else {
            return Ok(self.gpu_words());
        };
        enum Win<'a> {
            Coverage(HoleWindow),
            Lattice(&'a LatticeWindow),
        }
        let mut wins: Vec<Win<'_>> = Vec::new();
        let coverage = self
            .holes
            .iter()
            .map(|e| HoleShape::Ellipse(e.clone()))
            .chain(self.shapes.iter().cloned());
        for shape in coverage {
            wins.push(Win::Coverage(HoleWindow::rasterize(&shape)));
        }
        for named in self.open_named() {
            match lattice.hole(&named.id) {
                Some(cut) => wins.extend(cut.windows.iter().map(Win::Lattice)),
                None => wins.push(Win::Coverage(HoleWindow::rasterize(&named.shape))),
            }
        }
        if wins.len() > MAX_HOLE_WINDOWS {
            return Err(HolesError::TooManyShapes(wins.len()));
        }
        let texels = (HOLE_WINDOW_TEXELS * HOLE_WINDOW_TEXELS) as usize;
        let mut words = Vec::with_capacity(4 + wins.len() * (8 + texels / 4));
        words.extend_from_slice(&[wins.len() as u32, 0, 0, 0]);
        for w in &wins {
            match w {
                Win::Coverage(w) => {
                    let (s, c) = w.rotation_radians.sin_cos();
                    words.extend(
                        [
                            w.center[0],
                            w.center[1],
                            c,
                            s,
                            1.0 / w.half_extents[0].max(1e-6),
                            1.0 / w.half_extents[1].max(1e-6),
                            0.0,
                            0.0,
                        ]
                        .map(f32::to_bits),
                    );
                }
                Win::Lattice(w) => {
                    let head = [
                        w.corner[0],
                        w.corner[1],
                        w.size[0] as f32,
                        w.size[1] as f32,
                        lattice.inv_step,
                        lattice.inv_step,
                    ]
                    .map(f32::to_bits);
                    words.extend_from_slice(&head);
                    words.extend_from_slice(&[LATTICE_WINDOW_MODE, 0]);
                }
            }
        }
        for w in &wins {
            let bytes = match w {
                Win::Coverage(w) => &w.coverage,
                Win::Lattice(w) => &w.bytes,
            };
            words.extend(bytes.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b)));
        }
        Ok(words)
    }

    /// The GPU coverage windows, one per shape.
    #[must_use]
    pub fn windows(&self) -> Vec<HoleWindow> {
        self.shapes_ref()
            .take(MAX_HOLE_WINDOWS)
            .map(|s| HoleWindow::rasterize(&s))
            .collect()
    }

    /// Validates every footprint and the total count: at most [`MAX_HOLE_WINDOWS`] shapes,
    /// ellipses and named holes (open or closed) included (the GPU draws one coverage window per
    /// shape and drops the rest). Named holes need valid ids, unique and sorted by id.
    ///
    /// # Errors
    ///
    /// See [`HolesError`].
    pub fn validate(&self) -> Result<(), HolesError> {
        let count = self.holes.len() + self.shapes.len() + self.named.len();
        if count > MAX_HOLE_WINDOWS {
            return Err(HolesError::TooManyShapes(count));
        }
        for shape in self.shapes_ref() {
            shape.validate()?;
        }
        for (i, hole) in self.named.iter().enumerate() {
            if !valid_hole_id(&hole.id) {
                return Err(HolesError::BadId(hole.id.clone()));
            }
            if i > 0 && self.named[i - 1].id >= hole.id {
                return Err(if self.named[i - 1].id == hole.id {
                    HolesError::DuplicateId(hole.id.clone())
                } else {
                    // Out of order: a hand-built mask; `from_holes_ron` sorts before validating.
                    HolesError::BadId(hole.id.clone())
                });
            }
            if !hole.open {
                hole.shape.validate()?;
            }
        }
        Ok(())
    }

    /// Parses a `<map>.holes.ron` document ([`HOLES_FORMAT_VERSION`], `docs/formats/terrain.md`
    /// § 5): a serialised [`HolesMask`] with an optional `version` field. A file without one is
    /// version 1, so `ron::to_string(&mask)` output loads as is. Version 1 migrates to 2 by
    /// adding no named holes (v2 only adds `named`, ADR-1595); named holes are sorted by id. The version is read before the
    /// body, so a file from a newer engine is refused with [`HolesFileError::NewerVersion`]
    /// even when its body no longer matches this struct. The mask is validated: at most
    /// [`MAX_HOLE_WINDOWS`] shapes, each well formed, rasters at most
    /// [`MAX_HOLE_RASTER_SIDE`] texels a side.
    ///
    /// # Errors
    ///
    /// See [`HolesFileError`].
    pub fn from_holes_ron(text: &str) -> Result<Self, HolesFileError> {
        let version = holes_file_version(text)?;
        if version > HOLES_FORMAT_VERSION {
            return Err(HolesFileError::NewerVersion(version));
        }
        if version == 0 {
            return Err(HolesFileError::Parse("version 0 does not exist".into()));
        }
        let doc: HolesDocument = ron::from_str(text).map_err(|e| HolesFileError::Parse(e.to_string()))?;
        if version < 2 && !doc.named.is_empty() {
            return Err(HolesFileError::Parse(
                "`named` holes need format version 2".into(),
            ));
        }
        let mut named = doc.named;
        named.sort_by(|a, b| a.id.cmp(&b.id));
        let mask = Self {
            holes: doc.holes,
            shapes: doc.shapes,
            named,
        };
        mask.validate_file()?;
        Ok(mask)
    }

    /// Writes the mask as a `<map>.holes.ron` document at [`HOLES_FORMAT_VERSION`]
    /// ([`Self::from_holes_ron`] reads it back unchanged).
    ///
    /// # Errors
    ///
    /// The mask is invalid ([`HolesFileError::Invalid`], [`HolesFileError::RasterTooLarge`]),
    /// or serialisation failed.
    pub fn to_holes_ron(&self) -> Result<String, HolesFileError> {
        self.validate_file()?;
        let doc = HolesDocument {
            version: HOLES_FORMAT_VERSION,
            holes: self.holes.clone(),
            shapes: self.shapes.clone(),
            named: self.named.clone(),
        };
        ron::ser::to_string_pretty(&doc, regia_core::ron_pretty())
            .map_err(|e| HolesFileError::Parse(e.to_string()))
    }

    /// The file's raster size limit (reported as its own error), then [`Self::validate`].
    fn validate_file(&self) -> Result<(), HolesFileError> {
        for shape in self.shapes.iter().chain(self.named.iter().map(|n| &n.shape)) {
            if let HoleShape::Raster(r) = shape
                && (r.width > MAX_HOLE_RASTER_SIDE || r.height > MAX_HOLE_RASTER_SIDE)
            {
                return Err(HolesFileError::RasterTooLarge {
                    width: r.width,
                    height: r.height,
                });
            }
        }
        self.validate()?;
        Ok(())
    }
}

/// Current version of the `<map>.holes.ron` format ([`HolesMask::from_holes_ron`]). Version 2
/// (ADR-1595) adds the named holes ([`HolesMask::named`]); version 1 files load unchanged.
pub const HOLES_FORMAT_VERSION: u32 = 2;

/// Most texels along either side of a raster shape in a `<map>.holes.ron` file.
pub const MAX_HOLE_RASTER_SIDE: u32 = 2048;

/// Errors reading or writing a `<map>.holes.ron` file.
#[derive(Clone, Debug, Error)]
pub enum HolesFileError {
    /// The file could not be read.
    #[error("cannot read the holes file {path}: {message}")]
    Io {
        /// The path.
        path: String,
        /// The OS error.
        message: String,
    },
    /// The text is not a holes document.
    #[error("not a holes document: {0}")]
    Parse(String),
    /// The file was written by a newer engine.
    #[error(
        "holes format version {0} is newer than this engine's {HOLES_FORMAT_VERSION}; upgrade the engine"
    )]
    NewerVersion(u32),
    /// The mask is invalid.
    #[error("invalid holes mask: {0}")]
    Invalid(#[from] HolesError),
    /// A raster is larger than [`MAX_HOLE_RASTER_SIDE`] a side.
    #[error("hole raster {width}×{height} is larger than {MAX_HOLE_RASTER_SIDE} texels a side")]
    RasterTooLarge {
        /// Texels along local x.
        width: u32,
        /// Texels along local z.
        height: u32,
    },
}

/// The `<map>.holes.ron` document: a [`HolesMask`] with a version. Named `HolesMask` on the
/// wire, so a `HolesMask(...)` struct-named file parses as well as the bare `(...)` form.
#[derive(Serialize, Deserialize)]
#[serde(rename = "HolesMask", deny_unknown_fields)]
struct HolesDocument {
    /// Format version; absent = 1.
    #[serde(default = "first_holes_version")]
    version: u32,
    #[serde(default)]
    holes: Vec<HoleEllipse>,
    #[serde(default)]
    shapes: Vec<HoleShape>,
    /// Named holes (v2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    named: Vec<NamedHole>,
}

fn first_holes_version() -> u32 {
    1
}

/// The `<map>.holes.ron` next to a map file: the file name without a `.ron` extension and
/// then without a final kind word (letters only, such as `.level`), plus `.holes.ron`
/// (`maps/switchbacks.level.ron` → `maps/switchbacks.holes.ron`; `maps/v1.2.level.ron` →
/// `maps/v1.2.holes.ron`; a bare `maps/switchbacks` → `maps/switchbacks.holes.ron`).
#[must_use]
pub fn holes_ron_path(map: &std::path::Path) -> std::path::PathBuf {
    let name = map.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let mut stem = name.strip_suffix(".ron").unwrap_or(name);
    if let Some((base, kind)) = stem.rsplit_once('.')
        && !base.is_empty()
        && !kind.is_empty()
        && kind.chars().all(|c| c.is_ascii_alphabetic())
    {
        stem = base;
    }
    map.with_file_name(format!("{stem}.holes.ron"))
}

/// The format version of a `<map>.holes.ron` text: its top-level `version` field, 1 when the
/// field is absent (a bare `ron::to_string(&mask)`). Read before the body, so a newer file is
/// recognised even when its body no longer parses as this engine's mask.
///
/// # Errors
///
/// [`HolesFileError::Parse`] when the text is not a RON map (the version cannot be read) or its
/// `version` is not a whole number; such a file is never taken for version 1.
pub fn holes_file_version(text: &str) -> Result<u32, HolesFileError> {
    let value: ron::Value = ron::from_str(text)
        .map_err(|e| HolesFileError::Parse(format!("cannot read the format version: {e}")))?;
    let ron::Value::Map(map) = value else {
        return Err(HolesFileError::Parse("the top level is not a map".into()));
    };
    match map.get(&ron::Value::String("version".to_owned())) {
        None => Ok(1),
        Some(ron::Value::Number(number)) => ron::Value::Number(*number)
            .into_rust::<u32>()
            .map_err(|_| HolesFileError::Parse("`version` is not a whole number".into())),
        Some(_) => Err(HolesFileError::Parse("`version` is not a whole number".into())),
    }
}

/// Loads a `<map>.holes.ron` ([`HolesMask::from_holes_ron`]). `Ok(None)` when the file does
/// not exist (the map has no holes).
///
/// # Errors
///
/// The file exists but cannot be read or is not a valid holes document.
pub fn load_holes_ron(path: &std::path::Path) -> Result<Option<HolesMask>, HolesFileError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(HolesFileError::Io {
                path: path.display().to_string(),
                message: err.to_string(),
            });
        }
    };
    HolesMask::from_holes_ron(&text).map(Some)
}

/// The `from` versions of the registered `<map>.holes.ron` migrations: v1 → v2 (adds no named
/// holes; [`HolesMask::from_holes_ron`] reads v1, [`HolesMask::to_holes_ron`] writes v2).
#[must_use]
pub fn holes_migration_sources() -> Vec<u32> {
    vec![1]
}

/// The serde field-list fingerprint of the `<map>.holes.ron` format (hashed by
/// `cargo xtask check-formats`): the document, every nested struct and the shape variants.
#[must_use]
pub fn holes_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<HolesDocument>(),
        serde_field_names::<HoleEllipse>(),
        serde_field_names::<HoleRaster>(),
        serde_variant_names::<HoleShape>(),
        serde_field_names::<NamedHole>(),
    ])
}

/// How a quadtree node relates to a holes mask.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NodeHoleState {
    /// The node does not touch any hole: draw with the shared full index buffer.
    Empty,
    /// The node lies entirely inside a hole: skip it entirely.
    Full,
    /// The node partially overlaps a hole: draw with hole-culled indices.
    Partial,
}

/// Whether any edge of the closed polygon `points` meets the box `(min, max)` (boundary
/// included): with none, the even-odd fill is the same over the whole box.
fn polygon_edge_crosses(points: &[[f32; 2]], (min, max): ([f32; 2], [f32; 2])) -> bool {
    let Some(&last) = points.last() else {
        return false;
    };
    let mut prev = last;
    points.iter().any(|&p| {
        let hit = segment_meets_box(prev, p, min, max);
        prev = p;
        hit
    })
}

/// Liang–Barsky clipping of segment `a → b` against the box: `true` when part of it is inside.
fn segment_meets_box(a: [f32; 2], b: [f32; 2], min: [f32; 2], max: [f32; 2]) -> bool {
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    for axis in 0..2 {
        let d = b[axis] - a[axis];
        for (p, q) in [(-d, a[axis] - min[axis]), (d, max[axis] - a[axis])] {
            if p == 0.0 {
                if q < 0.0 {
                    return false;
                }
            } else {
                let r = q / p;
                if p < 0.0 {
                    t0 = t0.max(r);
                } else {
                    t1 = t1.min(r);
                }
            }
        }
    }
    t0 <= t1
}

/// Whether the raster reads 50 % or more everywhere in the box: the box lies inside the raster
/// rectangle and every texel a bilinear sample there blends is at least 128 (a blend of such
/// texels is never below one half).
fn raster_covers(raster: &HoleRaster, (min, max): ([f32; 2], [f32; 2])) -> bool {
    let (w, h) = (raster.width, raster.height);
    if w == 0 || h == 0 || !raster_len_ok(w, h, raster.coverage.len()) {
        return false;
    }
    let mut lo = [f32::INFINITY; 2];
    let mut hi = [f32::NEG_INFINITY; 2];
    for (x, z) in [
        (min[0], min[1]),
        (max[0], min[1]),
        (min[0], max[1]),
        (max[0], max[1]),
    ] {
        let (lx, lz) = to_local(
            x - raster.center[0],
            z - raster.center[1],
            raster.rotation_radians,
        );
        let u = (lx / raster.half_extents[0].max(1e-6)) * 0.5 + 0.5;
        let v = (lz / raster.half_extents[1].max(1e-6)) * 0.5 + 0.5;
        if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
            return false;
        }
        let (fx, fz) = (u * w as f32 - 0.5, v * h as f32 - 0.5);
        lo = [lo[0].min(fx), lo[1].min(fz)];
        hi = [hi[0].max(fx), hi[1].max(fz)];
    }
    let texel = |f: f32, n: u32| f.clamp(0.0, (n - 1) as f32).floor() as u32;
    let (x0, x1) = (texel(lo[0], w), (texel(hi[0], w) + 1).min(w - 1));
    let (z0, z1) = (texel(lo[1], h), (texel(hi[1], h) + 1).min(h - 1));
    (z0..=z1).all(|z| {
        let row = z as usize * w as usize;
        raster.coverage[row + x0 as usize..=row + x1 as usize]
            .iter()
            .all(|&c| c >= 128)
    })
}

/// Classifies a node against the mask, on a map centred on the world origin
/// ([`node_hole_state_at`] for a map with a grid origin).
pub fn node_hole_state(mask: &HolesMask, node: QuadNode, world_width: f32, root_level: u32) -> NodeHoleState {
    node_hole_state_at(mask, node, world_width, root_level, [0.0, 0.0])
}

/// Classifies a node against the mask, on a map centred on world XZ `origin`
/// ([`crate::TerrainMap::grid_origin`], ADR-8179).
pub fn node_hole_state_at(
    mask: &HolesMask,
    node: QuadNode,
    world_width: f32,
    root_level: u32,
    origin: [f32; 2],
) -> NodeHoleState {
    if mask.is_empty() {
        return NodeHoleState::Empty;
    }
    let (min_x, min_z, size) = node.world_rect_at(world_width, root_level, origin);
    // The bounds test first: most nodes touch no hole, and a rectangle inside a shape always
    // overlaps that shape's bounds, so the costly containment test only runs on the few that do.
    if !mask.intersects_rect(min_x, min_z, size) {
        NodeHoleState::Empty
    } else if mask.contains_rect(min_x, min_z, size) {
        NodeHoleState::Full
    } else {
        NodeHoleState::Partial
    }
}

/// Builds the index buffer for one node with triangles deep inside holes removed.
///
/// Returns the full grid index list when the node does not touch the mask. Triangles keep the
/// engine's heightfield diagonal split `[v00, v01, v10, v10, v01, v11]`. The cut itself is the
/// per-pixel discard in the holes pipelines; this only drops triangles that discard would
/// remove entirely anyway, so it is conservative: a triangle goes when its vertices, its edge
/// midpoints and its centroid all lie inside the mask by a margin of one grid step (the most a
/// CDLOD morph moves a vertex) plus two coverage-window texels (the most the rasterised edge
/// differs from the exact one). On a map centred on the world origin
/// ([`build_node_indices_at`] for a map with a grid origin).
pub fn build_node_indices(
    mask: &HolesMask,
    node: QuadNode,
    world_width: f32,
    root_level: u32,
    grid_size: u32,
) -> Vec<u32> {
    build_node_indices_at(mask, node, world_width, root_level, grid_size, [0.0, 0.0])
}

/// [`build_node_indices`] on a map centred on world XZ `origin`
/// ([`crate::TerrainMap::grid_origin`], ADR-8179).
pub fn build_node_indices_at(
    mask: &HolesMask,
    node: QuadNode,
    world_width: f32,
    root_level: u32,
    grid_size: u32,
    origin: [f32; 2],
) -> Vec<u32> {
    build_node_indices_lattice_at(mask, None, node, world_width, root_level, grid_size, origin)
}

/// [`node_hole_state_at`], with the lattice cut of the filled holes: a node is
/// [`NodeHoleState::Full`] only when, besides the mask, every lattice triangle under it that a
/// lattice window holds is cut ([`LatticeHoleCut::covers_rect`]); a node a window touches is never
/// [`NodeHoleState::Empty`].
pub fn node_hole_state_lattice_at(
    mask: &HolesMask,
    lattice: Option<&LatticeHoleCut>,
    node: QuadNode,
    world_width: f32,
    root_level: u32,
    origin: [f32; 2],
) -> NodeHoleState {
    let state = node_hole_state_at(mask, node, world_width, root_level, origin);
    let Some(lattice) = lattice else {
        return state;
    };
    let (min_x, min_z, size) = node.world_rect_at(world_width, root_level, origin);
    let (lo, hi) = ([min_x, min_z], [min_x + size, min_z + size]);
    if !lattice.touches_rect(lo, hi) {
        return state;
    }
    match state {
        NodeHoleState::Full if !lattice.covers_rect(lo, hi) => NodeHoleState::Partial,
        NodeHoleState::Empty => NodeHoleState::Partial,
        other => other,
    }
}

/// [`build_node_indices_at`] with the lattice cut of the filled holes (spec 26 § 2a.4 item 1,
/// ADR-8470): a node triangle that a lattice window touches is dropped only when the mask test
/// drops it and every lattice triangle of every cell its box touches is cut, so no lattice ground
/// (a C-shaped mouth's arm, an island) loses its triangle at any level.
pub fn build_node_indices_lattice_at(
    mask: &HolesMask,
    lattice: Option<&LatticeHoleCut>,
    node: QuadNode,
    world_width: f32,
    root_level: u32,
    grid_size: u32,
    origin: [f32; 2],
) -> Vec<u32> {
    let state = node_hole_state_lattice_at(mask, lattice, node, world_width, root_level, origin);
    if state == NodeHoleState::Full {
        return Vec::new();
    }
    let (min_x, min_z, size) = node.world_rect_at(world_width, root_level, origin);
    let step = size / grid_size as f32;
    let line = grid_size + 1;

    let margin = step + 2.0 * mask.window_texel_m();
    let deep = |fx: f32, fz: f32| mask.contains_deep(min_x + fx * step, min_z + fz * step, margin);
    let vertex_inside = |gx: u32, gz: u32| deep(gx as f32, gz as f32);

    let mut indices = Vec::with_capacity((grid_size * grid_size * 6) as usize);
    for gz in 0..grid_size {
        for gx in 0..grid_size {
            let v00 = gz * line + gx;
            let v01 = (gz + 1) * line + gx;
            let v10 = v00 + 1;
            let v11 = v01 + 1;
            // [v00, v01, v10] and [v10, v01, v11]: CCW seen from above, matching the cooked mesh.
            let (fx, fz) = (gx as f32, gz as f32);
            let tri_a = !(vertex_inside(gx, gz)
                && vertex_inside(gx, gz + 1)
                && vertex_inside(gx + 1, gz)
                && deep(fx, fz + 0.5)
                && deep(fx + 0.5, fz)
                && deep(fx + 0.5, fz + 0.5)
                && deep(fx + 1.0 / 3.0, fz + 1.0 / 3.0));
            let tri_b = !(vertex_inside(gx + 1, gz)
                && vertex_inside(gx, gz + 1)
                && vertex_inside(gx + 1, gz + 1)
                && deep(fx + 1.0, fz + 0.5)
                && deep(fx + 0.5, fz + 1.0)
                && deep(fx + 0.5, fz + 0.5)
                && deep(fx + 2.0 / 3.0, fz + 2.0 / 3.0));
            // A lattice window under the cell: drop only over cut lattice ground.
            let lattice_keeps = || {
                lattice.is_some_and(|l| {
                    let lo = [min_x + fx * step, min_z + fz * step];
                    let hi = [lo[0] + step, lo[1] + step];
                    l.touches_rect(lo, hi) && !l.covers_rect(lo, hi)
                })
            };
            let (tri_a, tri_b) = if (!tri_a || !tri_b) && lattice_keeps() {
                (true, true)
            } else {
                (tri_a, tri_b)
            };
            if tri_a {
                indices.extend_from_slice(&[v00, v01, v10]);
            }
            if tri_b {
                indices.extend_from_slice(&[v10, v01, v11]);
            }
        }
    }
    indices
}

/// What a [`TerrainHoles`] handle shares between a [`crate::TerrainSharedDraw`] and the agent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerrainHolesState {
    /// A draw owns this handle ([`crate::TerrainSharedDraw::holes_handle`]).
    pub linked: bool,
    /// A mask waiting for the draw's next prepare (`terrain.set_holes`).
    pub requested: Option<HolesMask>,
    /// The mask the draw cuts with.
    pub applied: HolesMask,
    /// Changes of the applied mask.
    pub generation: u64,
}

/// A shared handle on a terrain draw's holes, reachable after the draw is boxed into the
/// frame renderer: the agent protocol (`terrain.holes`, `terrain.set_holes`) requests a mask
/// here and the draw applies it on its next prepare. [`crate::TerrainPlugin`] inserts an
/// unlinked one; a host links its draw with
/// `world.insert_resource(draw.holes_handle())`.
#[derive(Resource, Clone, Debug, Default)]
pub struct TerrainHoles(std::sync::Arc<std::sync::Mutex<SharedHoles>>);

/// [`TerrainHolesState`] with the applied mask behind an `Arc`, so a pick or a ray reads it
/// without copying polygons and rasters ([`TerrainHoles::applied_mask`]).
#[derive(Debug, Default)]
struct SharedHoles {
    linked: bool,
    requested: Option<HolesMask>,
    applied: std::sync::Arc<HolesMask>,
    generation: u64,
    /// Hole ids a mesh layer fills. A shape change of one of these is [`HolesError::FilledByLayer`].
    filled_by_layer: Vec<String>,
    /// A lattice cut waiting for the draw's next prepare (ADR-8470); `Some(None)` removes it.
    lattice_request: Option<Option<LatticeHoleCut>>,
    /// Mouth bands waiting for the draw's next prepare (ADR-8569); `Some(None)` removes them.
    bands_request: Option<Option<std::sync::Arc<crate::mesh_layer::MouthBands>>>,
}

/// Refuses a shape change of a hole a mesh layer fills (ADR-8291).
///
/// Opening or closing (the shape is unchanged) and removing (`after` has no such hole) are
/// allowed. Adding a footprint for a filled id, or replacing one with a different shape, returns
/// [`HolesError::FilledByLayer`].
///
/// # Errors
///
/// [`HolesError::FilledByLayer`] for a refused shape.
pub fn filled_shape_change(
    before: &HolesMask,
    after: &HolesMask,
    filled: &[String],
) -> Result<(), HolesError> {
    for id in filled {
        match (before.named_hole(id), after.named_hole(id)) {
            (Some(prev), Some(next)) if prev.shape != next.shape => {
                return Err(HolesError::FilledByLayer(id.clone()));
            }
            (None, Some(_)) => return Err(HolesError::FilledByLayer(id.clone())),
            _ => {}
        }
    }
    Ok(())
}

impl TerrainHoles {
    pub(crate) fn linked() -> Self {
        let handle = Self::default();
        handle.lock().linked = true;
        handle
    }

    /// A handle that behaves as if a draw were bound (tools and tests without a renderer):
    /// requests wait here, as they wait for a draw's next prepare.
    #[must_use]
    pub fn new_linked() -> Self {
        Self::linked()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SharedHoles> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A copy of the shared state.
    #[must_use]
    pub fn state(&self) -> TerrainHolesState {
        let shared = self.lock();
        TerrainHolesState {
            linked: shared.linked,
            requested: shared.requested.clone(),
            applied: (*shared.applied).clone(),
            generation: shared.generation,
        }
    }

    /// The mask the draw cuts with, shared (no copy), or `None` before a draw applied one
    /// (generation 0).
    #[must_use]
    pub fn applied_mask(&self) -> Option<std::sync::Arc<HolesMask>> {
        let shared = self.lock();
        (shared.generation > 0).then(|| shared.applied.clone())
    }

    /// Asks the draw to cut `mask` (an empty mask removes every hole) on its next prepare.
    pub fn request(&self, mask: HolesMask) {
        self.lock().requested = Some(mask);
    }

    /// Runs `f` on the mask the draw cuts with, without copying it; `None` when no draw is linked
    /// to this handle (the draw's mask is the truth once linked, see [`TerrainHolesState::linked`]).
    pub fn with_applied<R>(&self, f: impl FnOnce(&HolesMask) -> R) -> Option<R> {
        let state = self.lock();
        state.linked.then(|| f(&state.applied))
    }

    /// Asks the draw to draw the filled holes from `lattice` (ADR-8470,
    /// [`crate::TerrainMeshLayers::lattice_hole_cut`]) on its next prepare; `None` goes back to
    /// coverage windows.
    pub fn request_lattice(&self, lattice: Option<LatticeHoleCut>) {
        self.lock().lattice_request = Some(lattice);
    }

    /// The lattice cut a host requested since the last call (`Some(None)`: remove it).
    pub(crate) fn take_lattice_request(&self) -> Option<Option<LatticeHoleCut>> {
        self.lock().lattice_request.take()
    }

    /// Asks the draw to draw `bands` (the open mouths' bands, ADR-8569,
    /// [`crate::MouthBands::from_layers`]) in every pass from its next prepare; `None` draws none.
    pub fn request_bands(&self, bands: Option<std::sync::Arc<crate::mesh_layer::MouthBands>>) {
        self.lock().bands_request = Some(bands);
    }

    /// The bands a host requested since the last call (`Some(None)`: remove them).
    pub(crate) fn take_bands_request(&self) -> Option<Option<std::sync::Arc<crate::mesh_layer::MouthBands>>> {
        self.lock().bands_request.take()
    }

    /// Hole ids whose footprint a mesh layer fills (ADR-8291). The next [`Self::edit`] refuses a
    /// shape change of one of them.
    pub fn set_filled_by_layer(&self, ids: &[String]) {
        self.lock().filled_by_layer = ids.to_vec();
    }

    /// Edits the mask the draw will cut next (the pending request, else the applied mask) and
    /// requests the result, under one lock, so two edits in one frame both land (`terrain.set_hole`,
    /// ADR-1595). Nothing is requested when `f` fails, the shape of a filled hole changes, or the
    /// edited mask is invalid.
    ///
    /// # Errors
    ///
    /// What `f` returns, [`HolesError::FilledByLayer`], or the edited mask's [`HolesMask::validate`]
    /// error.
    pub fn edit<R>(&self, f: impl FnOnce(&mut HolesMask) -> Result<R, HolesError>) -> Result<R, HolesError> {
        let mut shared = self.lock();
        let mut mask = shared
            .requested
            .clone()
            .unwrap_or_else(|| (*shared.applied).clone());
        let before = mask.clone();
        let out = f(&mut mask)?;
        filled_shape_change(&before, &mask, &shared.filled_by_layer)?;
        mask.validate()?;
        shared.requested = Some(mask);
        Ok(out)
    }

    pub(crate) fn take_request(&self) -> Option<HolesMask> {
        self.lock().requested.take()
    }

    /// [`Self::applied`] when `mask` differs from the applied mask (a draw re-cutting the same
    /// mask every frame does not bump the generation).
    pub(crate) fn applied_if_changed(&self, mask: Option<&HolesMask>) {
        let empty = HolesMask::default();
        let same = *self.lock().applied == *mask.unwrap_or(&empty);
        if !same {
            self.applied(mask);
        }
    }

    pub(crate) fn applied(&self, mask: Option<&HolesMask>) {
        let mut state = self.lock();
        state.applied = std::sync::Arc::new(mask.cloned().unwrap_or_default());
        state.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// audit sprint-audit-g-terrain3-determinism (High): `ron`'s default pretty config uses
    /// `\r\n` on Windows, so without an explicit `new_line("\n")` the holes file — and the
    /// level digest re-serialised from it — depended on the host. The exact bytes are pinned so
    /// a host-dependent newline can never regress the file or the digest.
    #[test]
    fn holes_ron_uses_lf_newlines_on_every_host() {
        let mask = HolesMask::new(vec![HoleEllipse {
            center: [3.0, -2.0],
            radius: [2.0, 1.5],
            rotation_radians: 0.0,
        }])
        .expect("mask");
        let text = mask.to_holes_ron().expect("ron");
        assert!(!text.contains('\r'), "holes RON must not contain CR bytes");
        assert_eq!(text, EXPECTED_ONE_ELLIPSE_RON);
    }

    /// The exact LF-serialised document for the one-ellipse mask above (pinned from the
    /// post-fix output; on Windows pre-fix every `\n` was a `\r\n`).
    const EXPECTED_ONE_ELLIPSE_RON: &str = "(\n    version: 2,\n    holes: [\n        (\n            center: (3.0, -2.0),\n            radius: (2.0, 1.5),\n            rotation_radians: 0.0,\n        ),\n    ],\n    shapes: [],\n)";

    /// A polygon built without `validate` (public fields) must not underflow or index out of range.
    #[test]
    fn degenerate_polygons_contain_nothing() {
        for n in 0..3 {
            let points = vec![[1.0f32, 1.0]; n];
            assert!(!HoleShape::Polygon { points }.contains(1.0, 1.0));
        }
    }

    /// audit-seq (Low): the 9 x 9 lattice missed an arm of ground narrower than its spacing, so
    /// a node with solid ground in it was dropped (`Full`). The slot sits between lattice samples.
    #[test]
    fn a_narrow_strip_of_ground_between_lattice_samples_keeps_the_node() {
        let (node_min, size) = (0.0f32, 16.0f32);
        let u_shape = |slot: (f32, f32)| HoleShape::Polygon {
            points: vec![
                [-10.0, -10.0],
                [slot.0, -10.0],
                [slot.0, 20.0],
                [slot.1, 20.0],
                [slot.1, -10.0],
                [26.0, -10.0],
                [26.0, 26.0],
                [-10.0, 26.0],
            ],
        };
        let probe = HolesMask::from_shapes(vec![u_shape((6.2, 6.4))]).expect("mask");
        let m = 2.0 * probe.window_texel_m();
        assert!(m < 0.5, "margin {m}");
        // Lattice x samples near 6 are 6 - m, 6, 6 + m and 8 - m: the slot fits in (6 + m, 8 - m).
        let (lo, hi) = (6.0 + m, 8.0 - m);
        let slot = (lo + (hi - lo) * 0.3, lo + (hi - lo) * 0.6);
        let mask = HolesMask::from_shapes(vec![u_shape(slot)]).expect("mask");
        let ground = (slot.0 + slot.1) * 0.5;
        assert!(!mask.contains(ground, 8.0), "the slot is ground");
        assert!(
            !mask.contains_rect(node_min, node_min, size),
            "the node has ground in it"
        );
        // Without the slot the node is inside the hole.
        let solid = HolesMask::from_shapes(vec![HoleShape::Polygon {
            points: vec![[-10.0, -10.0], [26.0, -10.0], [26.0, 26.0], [-10.0, 26.0]],
        }])
        .expect("mask");
        assert!(solid.contains_rect(node_min, node_min, size));

        // The same for a raster: one ground column between lattice samples.
        let (w, h) = (256u32, 256u32);
        let raster = |ground_column: Option<u32>| {
            let mut coverage = vec![255u8; (w * h) as usize];
            if let Some(col) = ground_column {
                for row in 0..h {
                    coverage[(row * w + col) as usize] = 0;
                }
            }
            HoleShape::Raster(HoleRaster {
                center: [8.0, 8.0],
                half_extents: [18.0, 18.0],
                rotation_radians: 0.0,
                width: w,
                height: h,
                coverage,
            })
        };
        let full = HolesMask::from_shapes(vec![raster(None)]).expect("mask");
        assert!(full.contains_rect(node_min, node_min, size));
        // Texel column whose centre is near x = 6.5 m (36 m over 256 texels).
        let col = ((6.5 - (8.0 - 18.0)) / 36.0 * w as f32) as u32;
        let cut = HolesMask::from_shapes(vec![raster(Some(col))]).expect("mask");
        assert!(
            !cut.contains_rect(node_min, node_min, size),
            "a ground column keeps the node"
        );
    }

    #[test]
    fn segment_box_clipping() {
        let (min, max) = ([0.0, 0.0], [1.0, 1.0]);
        assert!(segment_meets_box([-1.0, 0.5], [2.0, 0.5], min, max), "through");
        assert!(segment_meets_box([0.5, 0.5], [0.6, 0.6], min, max), "inside");
        assert!(!segment_meets_box([-1.0, 2.0], [2.0, 2.0], min, max), "above");
        assert!(!segment_meets_box([1.5, -1.0], [3.0, 0.5], min, max), "beside");
        assert!(
            segment_meets_box([1.0, -1.0], [1.0, 2.0], min, max),
            "on the edge"
        );
        assert!(
            !segment_meets_box([2.0, 0.0], [0.0, 2.0 + 0.5], min, max),
            "past the corner"
        );
    }

    #[test]
    fn polygon_vertex_count_is_capped() {
        let ring = |n: usize| HoleShape::Polygon {
            points: (0..n)
                .map(|i| {
                    let a = i as f32 / n as f32 * std::f32::consts::TAU;
                    [a.cos() * 5.0, a.sin() * 5.0]
                })
                .collect(),
        };
        assert!(ring(MAX_POLYGON_POINTS).validate().is_ok());
        assert!(matches!(
            ring(MAX_POLYGON_POINTS + 1).validate(),
            Err(HolesError::BadPolygon(_))
        ));
    }

    const W: f32 = 256.0;

    fn mask_at_origin() -> HolesMask {
        HolesMask::new(vec![HoleEllipse {
            center: [10.0, -4.0],
            radius: [12.0, 8.0],
            rotation_radians: 0.3,
        }])
        .expect("valid mask")
    }

    #[test]
    fn ellipse_contains_its_centre_not_far_points() {
        let hole = mask_at_origin().holes[0].clone();
        assert!(hole.contains(10.0, -4.0));
        assert!(!hole.contains(60.0, -4.0));
        assert!(!hole.contains(10.0, 60.0));
    }

    #[test]
    fn mask_validation_rejects_bad_radii() {
        assert!(matches!(
            HolesMask::new(vec![HoleEllipse {
                center: [0.0, 0.0],
                radius: [0.0, 5.0],
                rotation_radians: 0.0,
            }]),
            Err(HolesError::BadRadii { .. })
        ));
    }

    #[test]
    fn node_fully_inside_hole_is_culled_completely() {
        let mask = HolesMask::new(vec![HoleEllipse {
            center: [0.0, 0.0],
            radius: [190.0, 190.0],
            rotation_radians: 0.0,
        }])
        .expect("valid");
        // Root node (the whole 256 m map, corners at 181 m) is inside the ellipse by more than
        // two coverage-window texels (2 × 1.5 m).
        let state = node_hole_state(&mask, QuadNode::root(4), W, 4);
        assert_eq!(state, NodeHoleState::Full);
        assert!(build_node_indices(&mask, QuadNode::root(4), W, 4, 8).is_empty());
    }

    #[test]
    fn untouched_node_gets_the_full_grid() {
        let mask = mask_at_origin();
        let far = QuadNode {
            level: 0,
            x: 0,
            z: 15, // -Z corner, far from the hole at z = -4 m
        };
        assert_eq!(node_hole_state(&mask, far, W, 4), NodeHoleState::Empty);
        let indices = build_node_indices(&mask, far, W, 4, 16);
        assert_eq!(indices.len(), 16 * 16 * 6);
    }

    #[test]
    fn partial_node_drops_only_inside_triangles() {
        let mask = mask_at_origin();
        // Leaf nodes are 16 m; pick the node containing the hole centre.
        let node = QuadNode {
            level: 0,
            x: 9, // x range [8, 24]
            z: 7, // z range [-8, 8]
        };
        assert_eq!(node_hole_state(&mask, node, W, 4), NodeHoleState::Partial);
        let indices = build_node_indices(&mask, node, W, 4, 16);
        let full = 16 * 16 * 6;
        assert!(indices.len() < full, "some triangles must be culled");
        assert!(!indices.is_empty(), "some triangles must survive");

        // Reconstruct vertex positions and verify every removed triangle was inside by the morph
        // + raster margin (the per-pixel discard does the exact cut; culling is conservative).
        let (min_x, min_z, size) = node.world_rect(W, 4);
        let step = size / 16.0;
        let margin = step + 2.0 * mask.window_texel_m();
        let pos = |v: u32| {
            let gx = v % 17;
            let gz = v / 17;
            (min_x + gx as f32 * step, min_z + gz as f32 * step)
        };
        let mut kept: std::collections::HashSet<(u32, u32, u32)> = indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| (t[0], t[1], t[2]))
            .collect();
        for gz in 0..16u32 {
            for gx in 0..16u32 {
                let v00 = gz * 17 + gx;
                let v01 = (gz + 1) * 17 + gx;
                let v10 = v00 + 1;
                let v11 = v01 + 1;
                for tri in [(v00, v01, v10), (v10, v01, v11)] {
                    let deep = [tri.0, tri.1, tri.2].iter().all(|&v| {
                        let (x, z) = pos(v);
                        mask.contains_deep(x, z, margin)
                    });
                    let touches = [tri.0, tri.1, tri.2].iter().any(|&v| {
                        let (x, z) = pos(v);
                        mask.contains(x, z)
                    });
                    let present = kept.remove(&tri);
                    if !present {
                        assert!(deep, "removed triangle {tri:?} is not deep inside the hole");
                    }
                    if !touches {
                        assert!(present, "triangle {tri:?} outside the hole was removed");
                    }
                }
            }
        }
        assert!(kept.is_empty(), "no unexpected triangles");
    }

    fn ramp_rect() -> HoleShape {
        HoleShape::Rect {
            center: [5.0, 3.0],
            half_extents: [10.0, 12.5],
            rotation_radians: 0.7,
        }
    }

    #[test]
    fn rect_polygon_and_raster_contain_what_they_cover() {
        let rect = ramp_rect();
        assert!(rect.contains(5.0, 3.0));
        // Local +x axis is (cos, sin) of the rotation; 9.5 m along it is inside, 10.5 m is not.
        let (s, c) = 0.7f32.sin_cos();
        assert!(rect.contains(5.0 + 9.5 * c, 3.0 + 9.5 * s));
        assert!(!rect.contains(5.0 + 10.5 * c, 3.0 + 10.5 * s));

        // An L-shaped (non-convex) polygon.
        let poly = HoleShape::Polygon {
            points: vec![
                [0.0, 0.0],
                [10.0, 0.0],
                [10.0, 4.0],
                [4.0, 4.0],
                [4.0, 10.0],
                [0.0, 10.0],
            ],
        };
        assert!(poly.contains(2.0, 8.0));
        assert!(poly.contains(8.0, 2.0));
        assert!(!poly.contains(8.0, 8.0), "the notch of the L is ground");

        // A 4 × 4 raster with one covered quadrant.
        let mut coverage = vec![0u8; 16];
        for z in 0..2 {
            for x in 0..2 {
                coverage[z * 4 + x] = 255;
            }
        }
        let raster = HoleShape::Raster(HoleRaster {
            center: [0.0, 0.0],
            half_extents: [4.0, 4.0],
            rotation_radians: 0.0,
            width: 4,
            height: 4,
            coverage,
        });
        assert!(raster.contains(-3.0, -3.0));
        assert!(!raster.contains(3.0, 3.0));
        assert!(!raster.contains(-3.0, 3.0));
        assert!(raster.validate().is_ok());
    }

    #[test]
    fn shape_validation_rejects_degenerate_input() {
        assert!(
            HolesMask::from_shapes(vec![HoleShape::Polygon {
                points: vec![[0.0, 0.0], [1.0, 0.0]]
            }])
            .is_err()
        );
        assert!(
            HolesMask::from_shapes(vec![HoleShape::Raster(HoleRaster {
                center: [0.0, 0.0],
                half_extents: [1.0, 1.0],
                rotation_radians: 0.0,
                width: 4,
                height: 4,
                coverage: vec![0; 15],
            })])
            .is_err()
        );
        assert!(HolesMask::from_shapes(vec![ramp_rect(); MAX_HOLE_WINDOWS + 1]).is_err());
        assert!(HolesMask::from_shapes(vec![ramp_rect()]).is_ok());
    }

    #[test]
    fn window_raster_matches_the_exact_shape_to_a_texel() {
        let polygon = HoleShape::Polygon {
            points: vec![[0.0, 0.0], [20.0, 2.0], [22.0, 25.0], [3.0, 20.0], [8.0, 10.0]],
        };
        for shape in [ramp_rect(), polygon] {
            let mask = HolesMask::from_shapes(vec![shape.clone()]).expect("valid");
            let window = HoleWindow::rasterize(&shape);
            let texel = mask.window_texel_m();
            let ([x0, z0], [x1, z1]) = shape.world_bounds();
            let mut checked = 0;
            for j in 0..120 {
                for i in 0..120 {
                    let x = x0 - 2.0 + (x1 - x0 + 4.0) * i as f32 / 119.0;
                    let z = z0 - 2.0 + (z1 - z0 + 4.0) * j as f32 / 119.0;
                    let drawn = window.coverage_at(x, z) >= 0.5;
                    // Away from the edge (by 1.5 texels) the drawn cut equals the exact shape.
                    if mask.contains_deep(x, z, 1.5 * texel) {
                        assert!(drawn, "({x}, {z}) is deep inside but not cut");
                        checked += 1;
                    } else if !(-1..=1).any(|b| {
                        (-1..=1)
                            .any(|a| shape.contains(x + a as f32 * 1.5 * texel, z + b as f32 * 1.5 * texel))
                    }) {
                        assert!(!drawn, "({x}, {z}) is well outside but cut");
                        checked += 1;
                    }
                }
            }
            assert!(checked > 5000);
        }
    }

    #[test]
    fn segment_is_a_rect_along_the_centreline() {
        let ramp = HoleShape::segment([0.0, 0.0], [0.0, 25.0], 20.0);
        assert!(ramp.contains(0.0, 1.0));
        assert!(ramp.contains(9.5, 24.0));
        assert!(!ramp.contains(10.5, 12.0));
        assert!(!ramp.contains(0.0, 25.5));
        assert!(!ramp.contains(0.0, -0.5));
    }

    #[test]
    fn shapes_serialise_snake_case() {
        let json = ron::to_string(&HoleShape::Polygon {
            points: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
        })
        .expect("ron");
        assert!(json.starts_with("polygon"), "{json}");
    }

    #[test]
    fn gpu_words_lay_out_header_table_and_coverage() {
        let mask = HolesMask::from_shapes(vec![ramp_rect(), HoleShape::segment([0.0, 0.0], [5.0, 0.0], 2.0)])
            .expect("valid");
        let words = mask.gpu_words();
        let texels = (HOLE_WINDOW_TEXELS * HOLE_WINDOW_TEXELS) as usize;
        assert_eq!(words[0], 2);
        assert_eq!(words.len(), 4 + 2 * 8 + 2 * texels / 4);
        let windows = mask.windows();
        assert_eq!(f32::from_bits(words[4]), windows[0].center[0]);
        assert_eq!(f32::from_bits(words[8]), 1.0 / windows[0].half_extents[0]);
        // Texel (i, j) of window w is byte (j * N + i) % 4 of its word.
        let base = 4 + 2 * 8;
        let (i, j) = (128usize, 128usize);
        let k = j * HOLE_WINDOW_TEXELS as usize + i;
        let byte = (words[base + k / 4] >> ((k % 4) * 8)) & 255;
        assert_eq!(byte as u8, windows[0].coverage[k]);
        assert_eq!(byte, 255, "the window center is inside");
    }

    #[test]
    fn cut_heights_marks_samples_in_and_near_holes() {
        // Off the sample grid: x in 0.3..9.7, z in -1.3..2.1.
        let mask =
            HolesMask::from_shapes(vec![HoleShape::segment([0.3, 0.4], [9.7, 0.4], 3.4)]).expect("valid");
        // A 1 m grid over x, z in -5..=15.
        let n = 21usize;
        let xz = |i: usize| [(i % n) as f32 - 5.0, (i / n) as f32 - 5.0];
        let mut exact = vec![1.0f32; n * n];
        let cut = mask.cut_heights(&mut exact, 0.0, xz);
        assert!(cut > 0);
        for (i, h) in exact.iter().enumerate() {
            let [x, z] = xz(i);
            assert_eq!(h.is_nan(), mask.contains(x, z), "({x}, {z})");
        }
        let mut wide = vec![1.0f32; n * n];
        let wider = mask.cut_heights(&mut wide, 0.5, xz);
        assert!(wider > cut, "the margin cuts the samples bordering the hole too");
    }

    #[test]
    fn holes_handle_round_trips_a_request() {
        let handle = TerrainHoles::linked();
        assert!(handle.state().linked);
        handle.request(HolesMask::from_shapes(vec![ramp_rect()]).expect("valid"));
        let mask = handle.take_request().expect("pending");
        assert!(handle.take_request().is_none());
        handle.applied(Some(&mask));
        assert_eq!(handle.state().applied, mask);
        assert_eq!(handle.state().generation, 1);
        assert!(!TerrainHoles::default().state().linked);
    }

    #[test]
    fn ellipse_masks_without_shapes_still_deserialise() {
        let old = "(holes: [(center: (1.0, 2.0), radius: (3.0, 4.0), rotation_radians: 0.0)])";
        let mask: HolesMask = ron::from_str(old).expect("old format parses");
        assert_eq!(mask.holes.len(), 1);
        assert!(mask.shapes.is_empty());
        assert!(mask.contains(1.0, 2.0));
    }

    fn mouth_polygon() -> HoleShape {
        HoleShape::Polygon {
            points: vec![
                [-15.0, -9.0],
                [14.0, -10.0],
                [16.0, 8.0],
                [0.0, 12.0],
                [-14.0, 9.0],
            ],
        }
    }

    #[test]
    fn holes_ron_round_trips_with_its_version() {
        let mut coverage = vec![0u8; 64];
        coverage[27] = 255;
        let mask = HolesMask {
            holes: vec![HoleEllipse {
                center: [40.0, 2.0],
                radius: [6.0, 4.0],
                rotation_radians: 0.2,
            }],
            shapes: vec![
                mouth_polygon(),
                ramp_rect(),
                HoleShape::Raster(HoleRaster {
                    center: [-30.0, 5.0],
                    half_extents: [4.0, 4.0],
                    rotation_radians: 0.0,
                    width: 8,
                    height: 8,
                    coverage,
                }),
            ],
            named: vec![NamedHole {
                id: "tunnel_3".into(),
                open: false,
                shape: ramp_rect(),
            }],
        };
        let text = mask.to_holes_ron().expect("write");
        assert!(text.contains("version: 2"), "{text}");
        assert_eq!(HolesMask::from_holes_ron(&text).expect("read"), mask);
    }

    #[test]
    fn a_bare_serialised_mask_loads_as_version_one() {
        let mask = HolesMask::from_shapes(vec![mouth_polygon()]).expect("mask");
        let bare = ron::to_string(&mask).expect("ron");
        assert!(!bare.contains("version"));
        assert_eq!(HolesMask::from_holes_ron(&bare).expect("bare"), mask);
        // The struct-named form too.
        let named = format!("HolesMask{bare}");
        assert_eq!(HolesMask::from_holes_ron(&named).expect("named"), mask);
        // Ellipse-only files from before `shapes` existed.
        let old = "(holes: [(center: (1.0, 2.0), radius: (3.0, 4.0), rotation_radians: 0.0)])";
        assert_eq!(HolesMask::from_holes_ron(old).expect("old").holes.len(), 1);
    }

    #[test]
    fn holes_ron_refuses_newer_versions_and_invalid_masks() {
        let newer = "(version: 3, shapes: [], something_new: 3)";
        assert!(matches!(
            HolesMask::from_holes_ron(newer),
            Err(HolesFileError::NewerVersion(3))
        ));
        assert!(matches!(
            HolesMask::from_holes_ron("(version: 1, shapes: [polygon(points: [(0.0, 0.0), (1.0, 0.0)])])"),
            Err(HolesFileError::Invalid(HolesError::BadPolygon(2)))
        ));
        assert!(matches!(
            HolesMask::from_holes_ron("not ron at all ("),
            Err(HolesFileError::Parse(_))
        ));
        // 64 shapes in all, ellipses included.
        let mut too_many = HolesMask::from_shapes(vec![ramp_rect(); MAX_HOLE_WINDOWS]).expect("64");
        assert!(too_many.validate().is_ok());
        too_many.holes.push(mask_at_origin().holes[0].clone());
        let text = ron::to_string(&too_many).expect("ron");
        assert!(matches!(
            HolesMask::from_holes_ron(&text),
            Err(HolesFileError::Invalid(HolesError::TooManyShapes(65)))
        ));
        assert!(too_many.to_holes_ron().is_err());
        let side = MAX_HOLE_RASTER_SIDE + 1;
        let huge = HolesMask::from_shapes(vec![HoleShape::Raster(HoleRaster {
            center: [0.0, 0.0],
            half_extents: [1.0, 1.0],
            rotation_radians: 0.0,
            width: side,
            height: 1,
            coverage: vec![0; side as usize],
        })])
        .expect("valid in memory");
        assert!(matches!(
            huge.to_holes_ron(),
            Err(HolesFileError::RasterTooLarge { .. })
        ));
    }

    /// Review holes #1: a raster whose `width × height` wraps `u32` (65537² = 131073 mod 2³²)
    /// is refused instead of passing the length check and reading past its coverage; the file
    /// path reports the side limit first.
    #[test]
    fn a_wrapping_raster_size_is_refused() {
        let wrapping = HoleShape::Raster(HoleRaster {
            center: [0.0, 0.0],
            half_extents: [1.0, 1.0],
            rotation_radians: 0.0,
            width: 65_537,
            height: 65_537,
            coverage: vec![255; 131_073],
        });
        assert!(matches!(wrapping.validate(), Err(HolesError::BadRaster { .. })));
        assert!(HolesMask::from_shapes(vec![wrapping.clone()]).is_err());
        // A host that builds the shape without validating gets no hole, not a panic.
        assert!(!wrapping.contains(0.3, -0.4));
        let text = ron::to_string(&HolesMask {
            holes: Vec::new(),
            shapes: vec![wrapping],
            named: Vec::new(),
        })
        .expect("ron");
        assert!(matches!(
            HolesMask::from_holes_ron(&text),
            Err(HolesFileError::RasterTooLarge { .. })
        ));
    }

    /// Review holes (missing test): more than 64 ellipses through `HolesMask::new` are refused,
    /// as `from_shapes` and `validate` refuse more than 64 shapes (the GPU drops the rest).
    #[test]
    fn too_many_ellipses_are_refused() {
        let hole = |i: usize| HoleEllipse {
            center: [i as f32 * 3.0, 0.0],
            radius: [1.0, 1.0],
            rotation_radians: 0.0,
        };
        assert!(HolesMask::new((0..MAX_HOLE_WINDOWS).map(hole).collect()).is_ok());
        assert!(matches!(
            HolesMask::new((0..=MAX_HOLE_WINDOWS).map(hole).collect()),
            Err(HolesError::TooManyShapes(65))
        ));
    }

    /// Review holes #7: a non-finite rotation and a polygon with a zero-width box are refused
    /// (they put NaN or an infinite 1 / half extent into the GPU words).
    #[test]
    fn non_finite_rotations_and_flat_polygons_are_refused() {
        for r in [f32::NAN, f32::INFINITY] {
            let ellipse = HoleEllipse {
                center: [0.0, 0.0],
                radius: [1.0, 1.0],
                rotation_radians: r,
            };
            assert!(matches!(ellipse.validate(), Err(HolesError::BadRotation(_))));
            let rect = HoleShape::Rect {
                center: [0.0, 0.0],
                half_extents: [1.0, 1.0],
                rotation_radians: r,
            };
            assert!(matches!(rect.validate(), Err(HolesError::BadRotation(_))));
            let raster = HoleShape::Raster(HoleRaster {
                center: [0.0, 0.0],
                half_extents: [1.0, 1.0],
                rotation_radians: r,
                width: 1,
                height: 1,
                coverage: vec![255],
            });
            assert!(matches!(raster.validate(), Err(HolesError::BadRotation(_))));
        }
        let flat = HoleShape::Polygon {
            points: vec![[0.0, 1.0], [2.0, 1.0], [5.0, 1.0]],
        };
        assert!(matches!(flat.validate(), Err(HolesError::BadPolygon(3))));
        // A slanted line has a real box: valid (it cuts nothing) and its GPU words are finite.
        let slanted = HoleShape::Polygon {
            points: vec![[0.0, 0.0], [1.0, 1.0], [2.0, 2.0]],
        };
        let mask = HolesMask::from_shapes(vec![slanted]).expect("valid");
        assert!(
            mask.gpu_words()[4..12]
                .iter()
                .all(|w| f32::from_bits(*w).is_finite())
        );
    }

    /// Review holes #6: an unreadable file is a parse error (never taken for version 1), and a
    /// misspelt key is an error instead of an empty mask.
    #[test]
    fn unreadable_versions_and_unknown_keys_are_errors() {
        assert_eq!(holes_file_version("(shapes: [])").expect("absent = 1"), 1);
        assert_eq!(holes_file_version("(version: 3, anything: [])").expect("v3"), 3);
        assert!(holes_file_version("not ron at all (").is_err());
        assert!(holes_file_version("[1, 2]").is_err());
        assert!(holes_file_version("(version: \"two\")").is_err());
        let misspelt = "(version: 1, shape: [rect(center: (0.0, 0.0), half_extents: (1.0, 1.0), rotation_radians: 0.0)])";
        assert!(matches!(
            HolesMask::from_holes_ron(misspelt),
            Err(HolesFileError::Parse(_))
        ));
    }

    #[test]
    fn the_documented_example_parses() {
        // `book/agents/tunnels.md` and `docs/formats/terrain.md` § 5.
        let doc = "(
    version: 1,
    shapes: [
        polygon(points: [(-15.0, -9.0), (14.0, -10.0), (16.0, 8.0), (0.0, 12.0), (-14.0, 9.0)]),
        rect(center: (40.0, 2.0), half_extents: (10.0, 12.5), rotation_radians: 0.7),
        ellipse((center: (-40.0, 0.0), radius: (6.0, 4.0), rotation_radians: 0.0)),
    ],
)";
        let mask = HolesMask::from_holes_ron(doc).expect("example");
        assert_eq!(mask.shapes.len(), 3);
        assert!(mask.contains(0.0, 0.0) && mask.contains(40.0, 2.0) && mask.contains(-40.0, 0.0));
    }

    #[test]
    fn holes_ron_path_sits_next_to_the_map() {
        use std::path::Path;
        assert_eq!(
            holes_ron_path(Path::new("maps/switchbacks.level.ron")),
            Path::new("maps/switchbacks.holes.ron")
        );
        assert_eq!(
            holes_ron_path(Path::new("maps/switchbacks")),
            Path::new("maps/switchbacks.holes.ron")
        );
        // Review holes #10: dots in the map's name are kept, only the kind word goes.
        assert_eq!(
            holes_ron_path(Path::new("maps/v1.2.level.ron")),
            Path::new("maps/v1.2.holes.ron")
        );
        assert_eq!(
            holes_ron_path(Path::new("maps/v1.2")),
            Path::new("maps/v1.2.holes.ron")
        );
        assert_eq!(
            holes_ron_path(Path::new("maps/a.ron")),
            Path::new("maps/a.holes.ron")
        );
        assert_eq!(
            holes_ron_path(Path::new("maps/a.holes.ron")),
            Path::new("maps/a.holes.ron"),
            "the holes file maps to itself"
        );
        let missing = std::env::temp_dir().join("regia-no-such-map.holes.ron");
        assert!(load_holes_ron(&missing).expect("absent is fine").is_none());
    }

    #[test]
    fn holes_fingerprint_lists_the_document_and_shapes() {
        let f = holes_schema_fingerprint();
        assert!(f.starts_with("version,holes,shapes,named;"), "{f}");
        assert!(f.contains("ellipse,rect,polygon,raster"), "{f}");
        assert!(f.ends_with("id,open,shape"), "{f}");
        // v1 → v2 (ADR-1595).
        assert_eq!(holes_migration_sources(), vec![1]);
    }

    #[test]
    fn remaining_indices_stay_in_range() {
        let mask = mask_at_origin();
        let node = QuadNode { level: 2, x: 1, z: 1 };
        let indices = build_node_indices(&mask, node, W, 4, 8);
        let max_index = 8u32 * 9 + 8; // (grid + 1)^2 - 1
        assert!(indices.iter().all(|&i| i <= max_index));
    }

    fn tunnel(id: &str, open: bool, x: f32) -> NamedHole {
        NamedHole {
            id: id.into(),
            open,
            shape: HoleShape::Rect {
                center: [x, 0.0],
                half_extents: [6.0, 4.0],
                rotation_radians: 0.0,
            },
        }
    }

    /// ADR-1595: a closed named hole cuts nothing (no window, no contains, empty mask), an open
    /// one cuts like any shape, and the GPU words do not depend on the order holes were added.
    #[test]
    fn named_holes_cut_only_when_open_and_in_id_order() {
        let mut mask = HolesMask::default();
        assert_eq!(
            mask.set_named(tunnel("b_tunnel", false, 40.0)).expect("add"),
            None
        );
        assert!(mask.is_empty(), "a closed hole cuts nothing");
        assert!(!mask.contains(40.0, 0.0));
        assert!(mask.windows().is_empty());
        assert_eq!(
            mask.gpu_words(),
            vec![0, 0, 0, 0],
            "no windows: the empty-mask words"
        );
        assert_eq!(mask.shape_count(), 0);
        assert!(!mask.set_open("b_tunnel", true).expect("open"), "it was closed");
        assert!(!mask.is_empty() && mask.contains(40.0, 0.0) && !mask.contains(0.0, 0.0));
        assert_eq!(mask.shape_count(), 1);
        assert_eq!(mask.windows().len(), 1);
        mask.set_named(tunnel("a_tunnel", true, -40.0)).expect("add");
        assert_eq!(mask.named[0].id, "a_tunnel", "kept sorted by id");
        // The same holes added in the other order give the same mask and the same GPU words.
        let mut other = HolesMask::default();
        other.set_named(tunnel("a_tunnel", true, -40.0)).expect("add");
        other.set_named(tunnel("b_tunnel", true, 40.0)).expect("add");
        assert_eq!(other, mask);
        assert_eq!(other.gpu_words(), mask.gpu_words());
        // Replacing keeps one entry; removing and unknown ids.
        let old = mask.set_named(tunnel("a_tunnel", true, -60.0)).expect("replace");
        assert_eq!(old.map(|h| h.shape.contains(-40.0, 0.0)), Some(true));
        assert_eq!(mask.named.len(), 2);
        assert!(mask.contains(-60.0, 0.0) && !mask.contains(-40.0, 0.0));
        assert!(matches!(
            mask.set_open("nope", true),
            Err(HolesError::UnknownId(_))
        ));
        assert!(mask.remove_named("a_tunnel").is_some());
        assert!(mask.remove_named("a_tunnel").is_none());
        assert!(mask.named_hole("b_tunnel").is_some_and(|h| h.open));
        assert!(mask.validate().is_ok());
    }

    #[test]
    fn named_hole_ids_and_counts_are_checked() {
        let mut mask = HolesMask::default();
        let long = "x".repeat(MAX_HOLE_ID_LEN + 1);
        for bad in ["", "has space", "ünï", long.as_str()] {
            assert!(
                matches!(mask.set_named(tunnel(bad, true, 0.0)), Err(HolesError::BadId(_))),
                "{bad:?}"
            );
        }
        assert!(valid_hole_id("Tunnel-3.mouth_A"));
        // A closed hole still takes a slot: opening it can never fail.
        let mut full = HolesMask::from_shapes(vec![ramp_rect(); MAX_HOLE_WINDOWS - 1]).expect("63");
        full.set_named(tunnel("last", false, 0.0)).expect("64th");
        assert!(matches!(
            full.set_named(tunnel("one_more", false, 0.0)),
            Err(HolesError::TooManyShapes(65))
        ));
        full.set_open("last", true).expect("opening never fails");
        assert!(full.validate().is_ok());
        // Duplicates in a hand-built mask.
        let dup = HolesMask {
            named: vec![tunnel("x", true, 0.0), tunnel("x", false, 9.0)],
            ..HolesMask::default()
        };
        assert!(matches!(dup.validate(), Err(HolesError::DuplicateId(_))));
        // A bad shape is refused before anything changes.
        let mut bad_shape = tunnel("y", true, 0.0);
        bad_shape.shape = HoleShape::Polygon {
            points: vec![[0.0, 0.0]],
        };
        assert!(mask.set_named(bad_shape).is_err());
        assert!(mask.named.is_empty());
    }

    /// Format v2 (ADR-1595): named holes round-trip sorted, absent `open` means open, v1 files
    /// load as before (the v1 → v2 migration adds nothing), and `named` in a v1 file is refused.
    #[test]
    fn holes_ron_v2_named_holes_and_v1_migration() {
        let doc = "(
    version: 2,
    named: [
        (id: \"tunnel_b\", open: false, shape: rect(center: (40.0, 0.0), half_extents: (6.0, 4.0), rotation_radians: 0.0)),
        (id: \"tunnel_a\", shape: ellipse((center: (-40.0, 0.0), radius: (5.0, 3.0), rotation_radians: 0.0))),
    ],
)";
        let mask = HolesMask::from_holes_ron(doc).expect("v2");
        assert_eq!(mask.named[0].id, "tunnel_a", "sorted on load");
        assert!(mask.named[0].open, "absent = open");
        assert!(mask.contains(-40.0, 0.0) && !mask.contains(40.0, 0.0));
        let text = mask.to_holes_ron().expect("write");
        assert_eq!(HolesMask::from_holes_ron(&text).expect("again"), mask);
        let v1 = "(version: 1, shapes: [rect(center: (0.0, 0.0), half_extents: (15.0, 10.0), rotation_radians: 0.0)])";
        let migrated = HolesMask::from_holes_ron(v1).expect("v1");
        assert!(migrated.named.is_empty() && migrated.shapes.len() == 1);
        let upgraded = migrated.to_holes_ron().expect("v2 text");
        assert!(
            upgraded.contains("version: 2") && !upgraded.contains("named"),
            "{upgraded}"
        );
        assert_eq!(HolesMask::from_holes_ron(&upgraded).expect("re-read"), migrated);
        let v1_named = "(version: 1, named: [(id: \"t\", shape: rect(center: (0.0, 0.0), half_extents: (1.0, 1.0), rotation_radians: 0.0))])";
        assert!(matches!(
            HolesMask::from_holes_ron(v1_named),
            Err(HolesFileError::Parse(_))
        ));
        let dup = "(version: 2, named: [(id: \"t\", shape: rect(center: (0.0, 0.0), half_extents: (1.0, 1.0), rotation_radians: 0.0)), (id: \"t\", shape: rect(center: (5.0, 0.0), half_extents: (1.0, 1.0), rotation_radians: 0.0))])";
        assert!(matches!(
            HolesMask::from_holes_ron(dup),
            Err(HolesFileError::Invalid(HolesError::DuplicateId(_)))
        ));
    }

    /// `TerrainHoles::edit` stacks edits on the pending request and refuses an invalid result
    /// without touching the request.
    #[test]
    fn holes_handle_edits_stack_on_the_pending_request() {
        let handle = TerrainHoles::linked();
        handle
            .edit(|m| m.set_named(tunnel("a", true, 0.0)).map(|_| ()))
            .expect("first");
        handle
            .edit(|m| m.set_named(tunnel("b", false, 30.0)).map(|_| ()))
            .expect("second");
        assert!(handle.edit(|m| m.set_open("zzz", true)).is_err());
        let pending = handle.state().requested.expect("pending");
        assert_eq!(pending.named.len(), 2, "both edits landed");
        let mask = handle.take_request().expect("request");
        handle.applied(Some(&mask));
        // The next edit starts from the applied mask.
        handle.edit(|m| m.set_open("b", true)).expect("open b");
        let next = handle.take_request().expect("request");
        assert!(next.named.iter().all(|n| n.open));
    }

    /// A draw keeps a mask of closed holes on its handle although the GPU drops it (it cuts
    /// nothing), so the next edit finds them; the same mask again does not bump the generation.
    #[test]
    fn a_closed_only_mask_stays_on_the_handle() {
        let handle = TerrainHoles::linked();
        handle.applied_if_changed(None);
        assert_eq!(handle.state().generation, 0, "nothing applied yet");
        let mut closed = HolesMask::default();
        closed.set_named(tunnel("mouth", false, 0.0)).expect("closed");
        handle.applied_if_changed(Some(&closed));
        assert_eq!(handle.state().generation, 1);
        assert_eq!(handle.state().applied, closed);
        handle.applied_if_changed(Some(&closed));
        assert_eq!(handle.state().generation, 1, "unchanged mask, same generation");
        handle
            .edit(|m| m.set_open("mouth", true))
            .expect("the closed hole is still there");
        assert!(
            handle.applied_mask().is_some_and(|m| m.is_empty()),
            "closed cuts nothing"
        );
    }
}
