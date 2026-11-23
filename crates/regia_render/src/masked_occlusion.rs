// SPDX-License-Identifier: MPL-2.0
//! Software masked-depth occlusion culling for low-end and Android devices.
//!
//! A CPU hierarchical-in-spirit masked depth buffer in the style of Hasselgren, Andersson &
//! Akenine-Möller, "Masked Software Occlusion Culling", *Proceedings of High Performance
//! Graphics* (HPG 2016); see `docs/provenance.md`. Occluder meshes are rasterised on the CPU
//! into a low-resolution buffer (256×128 by default) and axis-aligned boxes are then tested
//! against it, so a device without the GPU Hi-Z path (`crate::depth_pyramid`, the two-phase
//! prepass in `crate::prepass_occlusion`) can still skip hidden draws. Single-threaded,
//! deterministic, plain safe Rust, no SIMD intrinsics: the intended use is a small number of
//! large occluders (terrain, buildings, hulls) tested against many candidate boxes.
//!
//! # Buffer layout and the two-layer rule
//!
//! The buffer is organised in 8×8-pixel tiles. Each tile holds **two depth layers**, and each
//! layer is one depth value plus a 64-bit coverage mask (one bit per pixel). A layer keeps the
//! invariant
//!
//! > for every pixel whose bit is set in the layer's mask, some rasterised occluder surface
//! > covers the *whole* pixel at a normalised distance `t <= layer_depth`.
//!
//! "Normalised distance" `t` grows with distance from the eye and is derived from NDC depth
//! via [`DepthMode`] (`t = z` for standard depth, `t = 1 - z` for reversed depth). Layer 0 is
//! the front layer and always has the smaller (nearer) depth, `z0 <= z1`.
//!
//! Rasterising one [`MaskedOcclusionCuller::draw_occluder`] call into a tile yields a coverage
//! mask `C` (the pixels entirely inside the union of the call's triangles, see below) and a
//! conservative depth interval `[t_lo, t_hi]` (the screen-space depth planes of the call's
//! triangles evaluated at the tile's corners, intersected with each triangle's vertex depth
//! range). The merge then follows a two-layer masked update rule in the spirit of the
//! published one (which keeps one full-coverage reference layer and one masked working
//! layer); this variant keeps two masked layers and preserves the invariant above exactly:
//!
//! 1. If every bit of `C` is already covered by a layer nearer than `t_lo`, the triangle is
//!    hidden here and nothing changes.
//! 2. If `t_hi <= z0` the triangle belongs in front of (or in) the front layer: it replaces the
//!    front layer when `C` covers all of the old front mask; it pushes the old front layer into
//!    an empty back layer and becomes the new front; otherwise its mask is OR-ed into the front
//!    layer, keeping `z0` (every newly covered pixel has surface `<= t_hi <= z0`).
//! 3. Otherwise, if `t_hi <= z1`, the same merge applies to the back layer (without the push).
//! 4. Otherwise the triangle is behind both layers: its mask is OR-ed into the back layer and
//!    the back depth rises to `t_hi`, trading depth tightness for coverage.
//!
//! Draw large, near occluders first (front to back) for the tightest layers; any order stays
//! conservative, only the culling rate changes.
//!
//! # Conservativeness (never a false `Occluded`)
//!
//! Every step errs toward [`Visibility::Visible`]:
//!
//! - A coverage bit is set only when the pixel is entirely inside the union of the call's
//!   triangles: its centre lies inside one of them (fixed-point edge functions, Pineda 1988)
//!   and no *boundary* edge of the union passes within `EDGE_GUARD` of the pixel. An edge is
//!   inside the union, not on its boundary, when exactly two triangles of the call share it
//!   (same object-space endpoints) and lie on opposite sides of it on screen; clip-plane
//!   edges, open borders, folds and non-manifold edges are boundary edges. A connected pixel
//!   square holding a point of the union and no point of its boundary lies inside it, so a
//!   set bit means the whole pixel is covered, and adjacent triangles of one mesh leave no
//!   cracks along their shared edges. A pixel entirely inside one triangle's own clipped
//!   polygon (centre inside, none of its edges near) also counts, so overlapping pieces in
//!   one call still cover what each covers alone. Separate calls are not fused.
//! - A layer depth is a far bound, never a near one: merging a triangle contributes
//!   `t_hi` over the tile, and a layer only ever relaxes its depth upward when the rule above
//!   requires it, never below a surface it already represents.
//! - Box tests use the projected hull of the (near-plane-checked) box corners, expanded to
//!   whole pixels plus one guard pixel, and the box's *nearest* distance minus a small bias.
//!   A point of the box projects inside that rectangle at distance `>=` the tested depth, so
//!   "every rectangle pixel covered by a nearer layer" implies the box is truly hidden.
//! - Boxes crossing or behind the near plane, empty boxes, boxes reaching outside the buffer,
//!   and non-finite rect depths are all reported [`Visibility::Visible`]. This culler is an
//!   occlusion test only; it never frustum-culls.
//!
//! # Depth convention
//!
//! [`DepthMode`] must match the projection in the matrices handed to
//! [`MaskedOcclusionCuller::draw_occluder`] and [`MaskedOcclusionCuller::test_aabb`]. The
//! default, [`DepthMode::Reversed`], matches the engine's reversed-Z infinite projection
//! ([`regia_core::math::Frustum`]). Clip space is wgpu/DirectX: `0 <= z <= w`, and triangles
//! are clipped against the near plane of the [`DepthMode`] (`z >= 0` for standard depth,
//! `z <= w` for reversed depth, plus a `w > 0` guard) before the divide.
//!
//! # Example
//!
//! ```ignore
//! let mut culler = MaskedOcclusionCuller::new(256, 128)?;
//! culler.clear();
//! culler.draw_occluder(&wall_vertices, &wall_indices, view_proj * wall_model, Backface::Cull)?;
//! if culler.test_aabb(crate_bounds, view_proj)? == Visibility::Occluded {
//!     // skip the draw
//! }
//! ```

use regia_core::glam::{DVec4, Mat4, Vec3, Vec4};
use regia_core::math::Aabb;

/// Tile edge length in pixels. A tile is `TILE × TILE` pixels: one `u64` mask per layer.
pub const TILE: u32 = 8;
/// Default buffer width in pixels (before tiling).
pub const DEFAULT_WIDTH: u32 = 256;
/// Default buffer height in pixels (before tiling).
pub const DEFAULT_HEIGHT: u32 = 128;
/// Largest buffer edge in pixels. The culler is meant to stay tiny; this bounds the tile
/// allocation and the fixed-point rasteriser's coordinate range.
pub const MAX_SIZE: u32 = 4096;

/// Fixed-point sub-pixel bits (256 sub-units per pixel).
const SUBPIXEL_BITS: i64 = 8;
/// One pixel in fixed-point units.
const SUBPIXEL: i64 = 1 << SUBPIXEL_BITS;
/// Queries are biased this far toward the eye (normalised-depth units) so a box whose nearest
/// point merely touches an occluder depth stays visible.
const QUERY_BIAS: f32 = 1e-4;
/// Guard plane in clip space: fragments need `w` above this after near-plane clipping.
const W_GUARD: f32 = 1e-6;
/// Guard band in clip space: triangles are clipped to `|x|, |y| <= GUARD_BAND * w`, so every
/// projected vertex lies within a quarter of the buffer outside its edges.
const GUARD_BAND: f64 = 1.5;
/// Triangles with a clip-space coordinate beyond this magnitude are skipped (counted as
/// non-finite): f64 clipping keeps sub-pixel precision up to here.
const MAX_CLIP_MAGNITUDE: f64 = 1e8;
/// Boundary edges mark every pixel within this many pixels of them (covers f32 rounding in
/// projection and clipping, and the fixed-point rounding of the coverage test).
const EDGE_GUARD: f32 = 1.0 / 64.0;

/// How NDC depth `z` in `[0, 1]` maps to normalised distance `t` (larger is farther).
///
/// Must match the projection inside the matrices given to the culler.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DepthMode {
    /// Standard depth: near plane at `z = 0`, far plane at `z = 1` (`t = z`).
    Standard,
    /// Reversed depth: near plane at `z = 1`, far at `z = 0` (`t = 1 - z`). The engine's
    /// reversed-Z infinite projection uses this mode.
    #[default]
    Reversed,
}

impl DepthMode {
    /// Normalised distance of an NDC depth (larger is farther from the eye).
    #[inline]
    fn to_t(self, ndc_z: f32) -> f32 {
        match self {
            Self::Standard => ndc_z,
            Self::Reversed => 1.0 - ndc_z,
        }
    }

    /// Signed clip-space distance to the near plane, `>= 0` on the visible side: `z` for
    /// standard depth (near plane `z = 0`), `w - z` for reversed depth (near plane `z = w`).
    #[inline]
    fn near_distance(self, z: f64, w: f64) -> f64 {
        match self {
            Self::Standard => z,
            Self::Reversed => w - z,
        }
    }
}

/// Whether [`MaskedOcclusionCuller::draw_occluder`] discards back-facing triangles.
///
/// A triangle is front-facing when its vertices wind counter-clockwise in NDC (x right,
/// y up), the usual right-handed convention for a surface seen from the eye.
///
/// [`Backface::Cull`] is only safe when the GPU culls the same faces: with a mirrored model
/// matrix (negative scale) or a pipeline whose front face is clockwise, flip the indices or
/// use [`Backface::None`], or an open mesh seen from behind could hide what the GPU shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backface {
    /// Discard back-facing (clockwise in NDC) triangles.
    Cull,
    /// Rasterise triangles of either winding.
    None,
}

/// Result of an occlusion query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    /// The tested box or rectangle may be visible (the conservative default).
    Visible,
    /// The tested box or rectangle is entirely behind rasterised occluders.
    Occluded,
}

impl Visibility {
    /// `true` for [`Visibility::Occluded`].
    #[must_use]
    #[inline]
    pub fn is_occluded(self) -> bool {
        self == Self::Occluded
    }
}

/// Why a masked-occlusion call failed. Every variant is a caller input problem; the culler
/// itself is left unchanged (for [`MaskedOcclusionCuller::draw_occluder`], no triangle of the
/// rejected call is rasterised).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OcclusionError {
    /// Buffer size outside `1..=4096` on an axis.
    #[error("masked occlusion buffer size {width}x{height} is outside 1..=4096")]
    InvalidSize {
        /// Requested width in pixels.
        width: u32,
        /// Requested height in pixels.
        height: u32,
    },
    /// A matrix contained a NaN or an infinity.
    #[error("masked occlusion matrix is not finite")]
    NonFiniteMatrix,
    /// A tested box contained a NaN or an infinity.
    #[error("masked occlusion bounds are not finite")]
    NonFiniteBounds,
    /// Index count is not a multiple of three.
    #[error("masked occlusion index count {count} is not a multiple of 3")]
    InvalidIndexCount {
        /// The offending index count.
        count: usize,
    },
    /// An index named a vertex outside the vertex slice.
    #[error("masked occlusion index {index} is outside {vertex_count} vertices")]
    IndexOutOfRange {
        /// The offending index.
        index: u32,
        /// Number of vertices supplied.
        vertex_count: usize,
    },
}

/// A rectangle in buffer pixels for [`MaskedOcclusionCuller::test_rect_depth`]: minimum corner
/// inclusive, maximum corner exclusive, origin at the top-left of the buffer (x right, y down),
/// the same pixel space projected geometry lands in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScreenRect {
    /// Left edge, inclusive.
    pub x0: i32,
    /// Top edge, inclusive.
    pub y0: i32,
    /// Right edge, exclusive.
    pub x1: i32,
    /// Bottom edge, exclusive.
    pub y1: i32,
}

impl ScreenRect {
    /// Creates a rectangle from its inclusive minimum and exclusive maximum corners.
    #[must_use]
    #[inline]
    pub const fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Self { x0, y0, x1, y1 }
    }

    /// `true` when the rectangle contains no pixel.
    #[must_use]
    #[inline]
    pub fn is_empty(self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    /// Width in pixels (negative for an inverted rectangle).
    #[must_use]
    #[inline]
    pub fn width(self) -> i32 {
        self.x1 - self.x0
    }

    /// Height in pixels (negative for an inverted rectangle).
    #[must_use]
    #[inline]
    pub fn height(self) -> i32 {
        self.y1 - self.y0
    }
}

/// What one [`MaskedOcclusionCuller::draw_occluder`] call did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawReport {
    /// Triangles in the call (`indices.len() / 3`).
    pub submitted: u32,
    /// Triangles that reached the rasteriser (after clipping, one source triangle clipped to a
    /// polygon still counts once if any part of it rasterised).
    pub rasterised: u32,
    /// Triangles discarded by [`Backface::Cull`].
    pub culled_backface: u32,
    /// Triangles removed entirely by near-plane or viewport clipping.
    pub clipped_away: u32,
    /// Triangles with zero screen area (collinear or repeated vertices).
    pub degenerate: u32,
    /// Triangles skipped because a vertex or its clip position was not finite.
    pub non_finite: u32,
}

/// Cumulative draw-side counters since creation or [`MaskedOcclusionCuller::reset_stats`].
/// Queries are pure (`&self`) and are not counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaskedStats {
    /// Total triangles submitted to [`MaskedOcclusionCuller::draw_occluder`].
    pub triangles_submitted: u64,
    /// Total triangles that reached the rasteriser.
    pub triangles_rasterised: u64,
    /// Total triangles discarded by back-face culling.
    pub triangles_culled_backface: u64,
    /// Total triangles removed entirely by clipping.
    pub triangles_clipped_away: u64,
    /// Total zero-area triangles skipped.
    pub triangles_degenerate: u64,
    /// Total triangles skipped over non-finite data.
    pub triangles_non_finite: u64,
    /// Tile merges performed (one per triangle/tile pair with coverage).
    pub tile_merges: u64,
}

/// One 8×8 tile: two depth layers, front first, with `depth[0] <= depth[1]` whenever both
/// hold coverage. An empty layer has mask 0 and depth `f32::INFINITY`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tile {
    /// Far-bound normalised distance per layer (module docs: every covered pixel of layer `i`
    /// holds a surface at distance `<= depth[i]`).
    depth: [f32; 2],
    /// Coverage per layer, bit `local_y * 8 + local_x`.
    mask: [u64; 2],
}

impl Tile {
    /// The cleared tile.
    const EMPTY: Self = Self {
        depth: [f32::INFINITY, f32::INFINITY],
        mask: [0, 0],
    };

    /// Pixels of this tile holding a surface strictly nearer than normalised distance `t`.
    #[inline]
    fn occlusion_mask(&self, t: f32) -> u64 {
        let mut mask = 0u64;
        if self.depth[0] < t {
            mask |= self.mask[0];
        }
        if self.depth[1] < t {
            mask |= self.mask[1];
        }
        mask
    }

    /// Merges triangle coverage over this tile with depth interval `[t_lo, t_hi]`
    /// (normalised distance, `t_lo <= t_hi`), following the module-level two-layer rule.
    fn merge(&mut self, coverage: u64, t_lo: f32, t_hi: f32) -> MergeOutcome {
        if coverage == 0 || !t_lo.is_finite() || !t_hi.is_finite() || t_hi < t_lo {
            return MergeOutcome::NoCoverage;
        }
        if coverage & !self.occlusion_mask(t_lo) == 0 {
            return MergeOutcome::SkippedOccluded;
        }
        if t_hi <= self.depth[0] {
            if self.mask[0] == 0 {
                self.depth[0] = t_hi;
                self.mask[0] = coverage;
                MergeOutcome::ReplacedFront
            } else if coverage & self.mask[0] == self.mask[0] {
                // The nearer triangle covers everything the front layer held.
                self.depth[0] = t_hi;
                self.mask[0] = coverage;
                MergeOutcome::ReplacedFront
            } else if self.mask[1] == 0 {
                // Keep both guarantees exactly: old front becomes the back layer.
                self.depth[1] = self.depth[0];
                self.mask[1] = self.mask[0];
                self.depth[0] = t_hi;
                self.mask[0] = coverage;
                MergeOutcome::PushedFrontToBack
            } else {
                // Keep `depth[0]`: new pixels hold a surface at `<= t_hi <= depth[0]`.
                self.mask[0] |= coverage;
                MergeOutcome::MergedFront
            }
        } else if t_hi <= self.depth[1] {
            if self.mask[1] == 0 || coverage & self.mask[1] == self.mask[1] {
                self.depth[1] = t_hi;
                self.mask[1] = coverage;
                MergeOutcome::ReplacedBack
            } else {
                self.mask[1] |= coverage;
                MergeOutcome::MergedBack
            }
        } else if self.mask[1] == 0 {
            self.depth[1] = t_hi;
            self.mask[1] = coverage;
            MergeOutcome::ReplacedBack
        } else {
            // Behind both layers: widen the back layer to this depth for its coverage.
            self.depth[1] = t_hi;
            self.mask[1] |= coverage;
            MergeOutcome::MergedBack
        }
    }
}

/// What [`Tile::merge`] did (test and debug surface for the two-layer rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MergeOutcome {
    /// Nothing to merge (empty or invalid coverage).
    NoCoverage,
    /// Fully hidden behind nearer coverage already in the tile.
    SkippedOccluded,
    /// The front layer was replaced by this triangle.
    ReplacedFront,
    /// The front layer moved to the back and this triangle became the front.
    PushedFrontToBack,
    /// Coverage was OR-ed into the front layer, depth kept.
    MergedFront,
    /// The back layer was replaced by this triangle.
    ReplacedBack,
    /// Coverage was OR-ed into the back layer.
    MergedBack,
}

/// A projected vertex in buffer pixels (x right, y down) with normalised distance `t`.
#[derive(Clone, Copy, Debug)]
struct ScreenVert {
    /// Pixel-space x.
    x: f32,
    /// Pixel-space y.
    y: f32,
    /// Normalised distance (larger is farther); screen-space linear over a triangle.
    t: f32,
}

/// Edge tag of a clipped polygon vertex whose edge to the next vertex lies on a clip plane
/// (tags `0..3` name the source triangle's edge from corner `k` to corner `k + 1`).
const EDGE_CLIP: u8 = 3;

/// Object-space endpoint bit patterns of a source edge, smaller first.
type EdgeKey = ([u32; 3], [u32; 3]);

/// One source edge of a triangle that reached the coverage pass.
#[derive(Clone, Copy, Debug)]
struct EdgeRecord {
    /// The edge's identity.
    key: EdgeKey,
    /// Screen side of the directed key edge (`key.0` to `key.1`) the triangle lies on.
    side: i8,
}

/// A triangle that survived clipping and culling, waiting for the coverage pass.
#[derive(Clone, Debug)]
struct PreparedTriangle {
    /// Clipped screen polygon; each vertex carries the tag of its edge to the next vertex.
    poly: Vec<(ScreenVert, u8)>,
    /// Keys and directions of source edges `0..3` (`None` for zero-length edges).
    keys: [Option<(EdgeKey, i8)>; 3],
    /// `+1` for counter-clockwise in NDC, `-1` for clockwise.
    orientation: i8,
}

/// What rasterising one prepared triangle did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RasterOutcome {
    /// Some fan triangle reached the buffer.
    Rasterised,
    /// Every fan triangle had zero area.
    Degenerate,
    /// Non-degenerate, but entirely outside the buffer.
    OffBuffer,
}

/// Per-tile accumulation of one draw call before the merge.
#[derive(Clone, Copy, Debug)]
struct TileAccum {
    /// Pixels whose centre lies inside some triangle of the call.
    centres: u64,
    /// Pixels a boundary edge of the call's triangle union touches.
    edges: u64,
    /// Nearest depth of any call triangle over the tile.
    t_lo: f32,
    /// Farthest depth of any call triangle over the tile.
    t_hi: f32,
    /// Pixels entirely inside one triangle's own clipped polygon (centre inside, none of
    /// that polygon's edges near): covered even where other pieces' edges cross.
    full: u64,
    /// Centres inside the triangle being rasterised.
    own_centres: u64,
    /// Pixels any edge of the triangle being rasterised touches.
    own_edges: u64,
    /// Registered in `MaskedOcclusionCuller::touched`.
    touched: bool,
    /// Registered in `MaskedOcclusionCuller::own_touched`.
    own_touched: bool,
}

impl TileAccum {
    /// The reset accumulator.
    const EMPTY: Self = Self {
        centres: 0,
        edges: 0,
        t_lo: f32::INFINITY,
        t_hi: f32::NEG_INFINITY,
        full: 0,
        own_centres: 0,
        own_edges: 0,
        touched: false,
        own_touched: false,
    };
}

/// The software masked-depth occlusion culler. See the module docs for the algorithm, the
/// conservativeness argument and the depth convention.
///
/// Typical frame: [`Self::clear`], [`Self::draw_occluder`] for each large occluder mesh
/// (front to back where practical), then [`Self::test_aabb`] per candidate draw.
#[derive(Clone, Debug)]
pub struct MaskedOcclusionCuller {
    /// Buffer width in pixels.
    width: u32,
    /// Buffer height in pixels.
    height: u32,
    /// Tiles along x (`width` rounded up to whole tiles).
    tiles_x: u32,
    /// Tiles along y (`height` rounded up to whole tiles).
    tiles_y: u32,
    /// Row-major tiles, `tiles_x * tiles_y` entries, never reallocated after creation.
    tiles: Vec<Tile>,
    /// Depth convention of the caller's projection matrices.
    depth_mode: DepthMode,
    /// Cumulative draw-side counters.
    stats: MaskedStats,
    /// Per-draw tile accumulators, one per tile, reset after every draw.
    accum: Vec<TileAccum>,
    /// Accumulators written by the current draw, in first-touch order.
    touched: Vec<u32>,
    /// Accumulators written by the current triangle, in first-touch order.
    own_touched: Vec<u32>,
    /// Pass 1's prepared triangles, kept between draws so a steady frame does not allocate
    /// (ADR-10072).
    prepared: Vec<PreparedTriangle>,
    /// Pass 1's edge records, kept between draws like `prepared`.
    records: Vec<EdgeRecord>,
}

impl MaskedOcclusionCuller {
    /// Creates a culler with a `width`×`height`-pixel buffer and the default
    /// [`DepthMode::Reversed`] (the engine's projection). Sizes need not be multiples of
    /// [`TILE`]; edge tiles are partial.
    ///
    /// # Errors
    ///
    /// [`OcclusionError::InvalidSize`] when either edge is 0 or above [`MAX_SIZE`].
    pub fn new(width: u32, height: u32) -> Result<Self, OcclusionError> {
        Self::new_with_depth_mode(width, height, DepthMode::default())
    }

    /// Creates a culler with an explicit [`DepthMode`], which must match the projection in
    /// every matrix later handed to [`Self::draw_occluder`] and [`Self::test_aabb`].
    ///
    /// # Errors
    ///
    /// [`OcclusionError::InvalidSize`] when either edge is 0 or above [`MAX_SIZE`].
    pub fn new_with_depth_mode(
        width: u32,
        height: u32,
        depth_mode: DepthMode,
    ) -> Result<Self, OcclusionError> {
        if width == 0 || height == 0 || width > MAX_SIZE || height > MAX_SIZE {
            return Err(OcclusionError::InvalidSize { width, height });
        }
        let tiles_x = width.div_ceil(TILE);
        let tiles_y = height.div_ceil(TILE);
        Ok(Self {
            width,
            height,
            tiles_x,
            tiles_y,
            tiles: vec![Tile::EMPTY; (tiles_x * tiles_y) as usize],
            depth_mode,
            stats: MaskedStats::default(),
            accum: vec![TileAccum::EMPTY; (tiles_x * tiles_y) as usize],
            touched: Vec::new(),
            own_touched: Vec::new(),
            prepared: Vec::new(),
            records: Vec::new(),
        })
    }

    /// Buffer width in pixels.
    #[must_use]
    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Buffer height in pixels.
    #[must_use]
    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Tiles along each axis, `(x, y)`.
    #[must_use]
    #[inline]
    pub fn tile_grid(&self) -> (u32, u32) {
        (self.tiles_x, self.tiles_y)
    }

    /// Total tile count.
    #[must_use]
    #[inline]
    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// The depth convention this culler was created with.
    #[must_use]
    #[inline]
    pub fn depth_mode(&self) -> DepthMode {
        self.depth_mode
    }

    /// Resets every tile to empty: after `clear`, every query reports
    /// [`Visibility::Visible`]. Statistics are kept (see [`Self::reset_stats`]).
    pub fn clear(&mut self) {
        self.tiles.fill(Tile::EMPTY);
    }

    /// Cumulative draw-side counters.
    #[must_use]
    #[inline]
    pub fn stats(&self) -> MaskedStats {
        self.stats
    }

    /// Zeroes the counters returned by [`Self::stats`]; tile contents are untouched.
    pub fn reset_stats(&mut self) {
        self.stats = MaskedStats::default();
    }

    /// Pixels holding any occluder coverage in either layer (diagnostics; overlaps between
    /// the two layers are counted once).
    #[must_use]
    pub fn covered_pixel_count(&self) -> u64 {
        self.tiles
            .iter()
            .map(|tile| u64::from((tile.mask[0] | tile.mask[1]).count_ones()))
            .sum()
    }

    /// Rasterises an indexed occluder mesh into the buffer.
    ///
    /// `vertices` are object-space positions and `model_view_projection` is the full
    /// object-to-clip transform (`view_projection * model`), in wgpu clip space
    /// (`0 <= z <= w`). Each triangle is clipped against the near plane and a guard band,
    /// optionally back-face culled, and scan-converted in fixed point; the call's coverage is
    /// the set of pixels entirely inside the union of its triangles (module docs), merged
    /// once per tile. Degenerate triangles and triangles touching non-finite vertices are
    /// skipped and counted in the report, never panicked on.
    /// An empty vertex or index slice draws nothing and succeeds.
    ///
    /// # Errors
    ///
    /// - [`OcclusionError::NonFiniteMatrix`] for a matrix with a NaN or an infinity.
    /// - [`OcclusionError::InvalidIndexCount`] when `indices.len()` is not a multiple of 3.
    /// - [`OcclusionError::IndexOutOfRange`] for an index outside `vertices`.
    ///
    /// On error nothing from this call is rasterised: indices are validated before drawing.
    pub fn draw_occluder(
        &mut self,
        vertices: &[Vec3],
        indices: &[u32],
        model_view_projection: Mat4,
        backface: Backface,
    ) -> Result<DrawReport, OcclusionError> {
        if !matrix_is_finite(&model_view_projection) {
            return Err(OcclusionError::NonFiniteMatrix);
        }
        if !indices.len().is_multiple_of(3) {
            return Err(OcclusionError::InvalidIndexCount { count: indices.len() });
        }
        for &index in indices {
            if index as usize >= vertices.len() {
                return Err(OcclusionError::IndexOutOfRange {
                    index,
                    vertex_count: vertices.len(),
                });
            }
        }
        let _span = tracing::trace_span!("masked_occlusion_draw", triangles = indices.len() / 3).entered();
        let mut report = DrawReport {
            submitted: (indices.len() / 3) as u32,
            ..DrawReport::default()
        };
        // Pass 1: transform, clip and cull every triangle, recording its source edges so the
        // coverage pass knows which edges are shared inside the call's triangle union.
        // Reused between draws (taken here, given back below): a steady frame does not allocate.
        let mut prepared = std::mem::take(&mut self.prepared);
        let mut records = std::mem::take(&mut self.records);
        prepared.clear();
        records.clear();
        for tri in indices.as_chunks::<3>().0 {
            let corners = [
                vertices[tri[0] as usize],
                vertices[tri[1] as usize],
                vertices[tri[2] as usize],
            ];
            if let Some(triangle) =
                self.prepare_triangle(corners, &model_view_projection, backface, &mut report)
            {
                for (k, key) in triangle.keys.iter().enumerate() {
                    let Some((key, direction)) = *key else {
                        continue;
                    };
                    if triangle.poly.iter().any(|&(_, tag)| usize::from(tag) == k) {
                        records.push(EdgeRecord {
                            key,
                            side: direction * triangle.orientation,
                        });
                    }
                }
                prepared.push(triangle);
            }
        }
        records.sort_unstable_by(|a, b| a.key.cmp(&b.key).then(a.side.cmp(&b.side)));
        // Pass 2: pixel-centre coverage and boundary edges into the per-tile accumulators.
        for triangle in &prepared {
            match self.rasterise_prepared(triangle, &records) {
                RasterOutcome::Rasterised => report.rasterised += 1,
                RasterOutcome::Degenerate => report.degenerate += 1,
                RasterOutcome::OffBuffer => report.clipped_away += 1,
            }
        }
        // Pass 3: one two-layer merge per touched tile.
        self.flush_accumulators();
        self.prepared = prepared;
        self.records = records;
        self.stats.triangles_submitted += u64::from(report.submitted);
        self.stats.triangles_rasterised += u64::from(report.rasterised);
        self.stats.triangles_culled_backface += u64::from(report.culled_backface);
        self.stats.triangles_clipped_away += u64::from(report.clipped_away);
        self.stats.triangles_degenerate += u64::from(report.degenerate);
        self.stats.triangles_non_finite += u64::from(report.non_finite);
        Ok(report)
    }

    /// Tests a world-space box against the rasterised occluders.
    ///
    /// Conservative: [`Visibility::Occluded`] is returned only when the whole box is behind
    /// occluder surface in every pixel it could touch (module docs). In particular a box is
    /// always [`Visibility::Visible`] when it is empty, when any part of it crosses or lies
    /// behind the near plane, or when its projection reaches outside the buffer. This is an
    /// occlusion test, not frustum culling: a box outside the view is `Visible` here and left
    /// for the caller's frustum test ([`regia_core::math::Frustum::intersects_aabb`]).
    ///
    /// `view_projection` must be the same matrix the occluders were drawn with (their model
    /// transforms already folded into their draw matrices).
    ///
    /// # Errors
    ///
    /// [`OcclusionError::NonFiniteMatrix`] for a non-finite matrix,
    /// [`OcclusionError::NonFiniteBounds`] for a box with a NaN or an infinite corner.
    pub fn test_aabb(&self, world_aabb: Aabb, view_projection: Mat4) -> Result<Visibility, OcclusionError> {
        if !matrix_is_finite(&view_projection) {
            return Err(OcclusionError::NonFiniteMatrix);
        }
        if world_aabb.min.is_nan() || world_aabb.max.is_nan() {
            return Err(OcclusionError::NonFiniteBounds);
        }
        // Empty first: `Aabb::EMPTY` is built from infinities and contains no point.
        if world_aabb.is_empty() {
            return Ok(Visibility::Visible);
        }
        if !vec3_is_finite(world_aabb.min) || !vec3_is_finite(world_aabb.max) {
            return Err(OcclusionError::NonFiniteBounds);
        }
        // Project the eight corners. A corner is usable only strictly in front of the near
        // plane of the depth mode (`DepthMode::near_distance`, plus `w > 0`); a box with any unusable corner may
        // reach around the eye, so it stays visible. With every corner usable the whole box
        // is in front (the near half-space is convex), its projection is the hull of the
        // projected corners, and its nearest normalised distance is a corner minimum
        // (normalised distance is monotonic along any segment with `w > 0`).
        let mut min_sx = f32::INFINITY;
        let mut min_sy = f32::INFINITY;
        let mut max_sx = f32::NEG_INFINITY;
        let mut max_sy = f32::NEG_INFINITY;
        let mut nearest_t = f32::INFINITY;
        for corner in 0..8u32 {
            let point = Vec3::new(
                if corner & 1 == 0 {
                    world_aabb.min.x
                } else {
                    world_aabb.max.x
                },
                if corner & 2 == 0 {
                    world_aabb.min.y
                } else {
                    world_aabb.max.y
                },
                if corner & 4 == 0 {
                    world_aabb.min.z
                } else {
                    world_aabb.max.z
                },
            );
            let clip = view_projection * point.extend(1.0);
            if !vec4_is_finite(clip)
                || clip.w <= W_GUARD
                || self
                    .depth_mode
                    .near_distance(f64::from(clip.z), f64::from(clip.w))
                    < 0.0
            {
                return Ok(Visibility::Visible);
            }
            let inv_w = 1.0 / clip.w;
            let ndc_x = clip.x * inv_w;
            let ndc_y = clip.y * inv_w;
            let t = self.depth_mode.to_t(clip.z * inv_w);
            if !ndc_x.is_finite() || !ndc_y.is_finite() || !t.is_finite() {
                return Ok(Visibility::Visible);
            }
            let (sx, sy) = self.ndc_to_screen(ndc_x, ndc_y);
            min_sx = min_sx.min(sx);
            min_sy = min_sy.min(sy);
            max_sx = max_sx.max(sx);
            max_sy = max_sy.max(sy);
            nearest_t = nearest_t.min(t);
        }
        // Whole pixels the hull touches, plus one guard pixel each side against rounding in
        // the projection above. Saturating casts keep extreme projections inside `i32`, and
        // the rect test then reports them visible as partly outside the buffer.
        let rect = ScreenRect::new(
            (min_sx.floor() as i32).saturating_sub(1),
            (min_sy.floor() as i32).saturating_sub(1),
            (max_sx.ceil() as i32).saturating_add(1),
            (max_sy.ceil() as i32).saturating_add(1),
        );
        Ok(self.test_rect_t(rect, nearest_t - QUERY_BIAS))
    }

    /// Tests a screen-space rectangle at a single NDC depth (for callers that already
    /// projected a bound themselves, particles, impostors, UI-space overlays).
    ///
    /// `ndc_depth` is the *nearest* depth of the tested item in the NDC convention of this
    /// culler's [`DepthMode`]. The rectangle is [`Visibility::Occluded`] only when every one
    /// of its pixels is fully inside the buffer and covered by an occluder layer nearer than
    /// `ndc_depth`. An empty rectangle, a rectangle reaching outside the buffer, and a
    /// non-finite depth are the defined fallbacks and report [`Visibility::Visible`].
    #[must_use]
    pub fn test_rect_depth(&self, rect: ScreenRect, ndc_depth: f32) -> Visibility {
        if !ndc_depth.is_finite() {
            return Visibility::Visible;
        }
        self.test_rect_t(rect, self.depth_mode.to_t(ndc_depth) - QUERY_BIAS)
    }

    /// [`Self::test_rect_depth`] in normalised distance, shared with [`Self::test_aabb`].
    fn test_rect_t(&self, rect: ScreenRect, t_query: f32) -> Visibility {
        if rect.is_empty() || !t_query.is_finite() {
            return Visibility::Visible;
        }
        if rect.x0 < 0 || rect.y0 < 0 || rect.x1 > self.width as i32 || rect.y1 > self.height as i32 {
            return Visibility::Visible;
        }
        let tx0 = (rect.x0 as u32) / TILE;
        let ty0 = (rect.y0 as u32) / TILE;
        let tx1 = ((rect.x1 as u32) - 1) / TILE;
        let ty1 = ((rect.y1 as u32) - 1) / TILE;
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let tile = &self.tiles[(ty * self.tiles_x + tx) as usize];
                let need = rect_tile_mask(rect, tx, ty);
                if need & !tile.occlusion_mask(t_query) != 0 {
                    return Visibility::Visible;
                }
            }
        }
        Visibility::Occluded
    }

    /// NDC (x right, y up) to buffer pixels (x right, y down, top-left origin).
    #[inline]
    fn ndc_to_screen(&self, ndc_x: f32, ndc_y: f32) -> (f32, f32) {
        (
            (ndc_x * 0.5 + 0.5) * self.width as f32,
            (1.0 - (ndc_y * 0.5 + 0.5)) * self.height as f32,
        )
    }

    /// One source triangle through transform, clipping (near plane, `w` guard, guard band)
    /// and back-face culling. Returns the near-clipped screen polygon with edge tags, or
    /// `None` (counted in `report`) when nothing of it can rasterise.
    fn prepare_triangle(
        &self,
        corners: [Vec3; 3],
        mvp: &Mat4,
        backface: Backface,
        report: &mut DrawReport,
    ) -> Option<PreparedTriangle> {
        if corners.iter().any(|v| !vec3_is_finite(*v)) {
            report.non_finite += 1;
            return None;
        }
        // Transform and clip in f64: clipping a long edge cancels its large coordinates, and
        // f32 would move the cut point by a visible fraction of a pixel.
        let mvp = mvp.as_dmat4();
        let mut clip_poly: Vec<(DVec4, u8)> = Vec::with_capacity(9);
        for (k, corner) in corners.iter().enumerate() {
            let clip = mvp * corner.as_dvec3().extend(1.0);
            if !clip.is_finite() || clip.abs().max_element() > MAX_CLIP_MAGNITUDE {
                report.non_finite += 1;
                return None;
            }
            clip_poly.push((clip, k as u8));
        }
        // The depth mode's near plane, the `w > 0` guard, then a guard band around the view
        // so every projected vertex lands near the buffer (f32 stays precise there). All are
        // linear in clip space (Sutherland & Hodgman 1974).
        let mode = self.depth_mode;
        let clip_poly = clip_polygon(&clip_poly, |v| mode.near_distance(v.z, v.w));
        let clip_poly = clip_polygon(&clip_poly, |v| v.w - f64::from(W_GUARD));
        let clip_poly = clip_polygon(&clip_poly, |v| GUARD_BAND * v.w + v.x);
        let clip_poly = clip_polygon(&clip_poly, |v| GUARD_BAND * v.w - v.x);
        let clip_poly = clip_polygon(&clip_poly, |v| GUARD_BAND * v.w + v.y);
        let clip_poly = clip_polygon(&clip_poly, |v| GUARD_BAND * v.w - v.y);
        if clip_poly.len() < 3 {
            report.clipped_away += 1;
            return None;
        }
        let mut poly: Vec<(ScreenVert, u8)> = Vec::with_capacity(clip_poly.len());
        let mut ndc_area2 = 0.0f64;
        for (i, (clip, tag)) in clip_poly.iter().enumerate() {
            let inv_w = 1.0 / clip.w;
            let ndc_x = clip.x * inv_w;
            let ndc_y = clip.y * inv_w;
            let t = self.depth_mode.to_t((clip.z * inv_w) as f32);
            if !ndc_x.is_finite() || !ndc_y.is_finite() || !t.is_finite() {
                report.non_finite += 1;
                return None;
            }
            let next = &clip_poly[(i + 1) % clip_poly.len()].0;
            let next_inv_w = 1.0 / next.w;
            ndc_area2 += ndc_x * (next.y * next_inv_w) - (next.x * next_inv_w) * ndc_y;
            // Guard-band clipped: |ndc| <= GUARD_BAND, so f32 is precise here.
            let (sx, sy) = self.ndc_to_screen(ndc_x as f32, ndc_y as f32);
            poly.push((ScreenVert { x: sx, y: sy, t }, *tag));
        }
        if backface == Backface::Cull && ndc_area2 <= 0.0 {
            report.culled_backface += 1;
            return None;
        }
        if ndc_area2 == 0.0 || !ndc_area2.is_finite() {
            report.degenerate += 1;
            return None;
        }
        let keys = [
            edge_key(corners[0], corners[1]),
            edge_key(corners[1], corners[2]),
            edge_key(corners[2], corners[0]),
        ];
        Some(PreparedTriangle {
            poly,
            keys,
            orientation: if ndc_area2 > 0.0 { 1 } else { -1 },
        })
    }

    /// Accumulates one prepared triangle: its boundary edges (edges not shared with a
    /// triangle on the other side within this call) mark the pixels they touch, and its fan
    /// triangles add pixel-centre coverage and the tile depth intervals.
    fn rasterise_prepared(&mut self, triangle: &PreparedTriangle, records: &[EdgeRecord]) -> RasterOutcome {
        let n = triangle.poly.len();
        for (i, &(a, tag)) in triangle.poly.iter().enumerate() {
            let (b, _) = triangle.poly[(i + 1) % n];
            let interior = triangle
                .keys
                .get(usize::from(tag))
                .copied()
                .flatten()
                .is_some_and(|(key, _)| edge_is_interior(records, key));
            self.mark_edge(a, b, !interior);
        }
        let mut outcome = RasterOutcome::Degenerate;
        for i in 1..n - 1 {
            let fan = [triangle.poly[0].0, triangle.poly[i].0, triangle.poly[i + 1].0];
            match self.rasterise_fan_triangle(fan) {
                RasterOutcome::Rasterised => outcome = RasterOutcome::Rasterised,
                RasterOutcome::OffBuffer if outcome == RasterOutcome::Degenerate => {
                    outcome = RasterOutcome::OffBuffer;
                }
                _ => {}
            }
        }
        // Whole pixels inside this triangle's own (convex) polygon count whatever other
        // pieces of the call do around them.
        let own = std::mem::take(&mut self.own_touched);
        for &index in &own {
            let accum = &mut self.accum[index as usize];
            accum.full |= accum.own_centres & !accum.own_edges;
            accum.own_centres = 0;
            accum.own_edges = 0;
            accum.own_touched = false;
        }
        self.own_touched = own;
        self.own_touched.clear();
        outcome
    }

    /// Pixel-centre coverage and per-tile depth interval of one screen triangle (part of a
    /// clipped polygon's fan) into the accumulators.
    fn rasterise_fan_triangle(&mut self, tri: [ScreenVert; 3]) -> RasterOutcome {
        // Depth plane in pixel coordinates (normalised distance is screen-linear).
        let [a, b, c] = tri;
        // Solved in f64: on long thin triangles the f32 determinant loses most of its digits
        // and a wrong plane could pull the tile's far bound nearer than the surface.
        let [(ax, ay, at), (bx, by, bt), (cx, cy, ct)] =
            [a, b, c].map(|v| (f64::from(v.x), f64::from(v.y), f64::from(v.t)));
        let det = (bx - ax) * (cy - ay) - (by - ay) * (cx - ax);
        if !det.is_finite() || det.abs() < 1e-12 {
            return RasterOutcome::Degenerate;
        }
        let dt_dx = ((bt - at) * (cy - ay) - (ct - at) * (by - ay)) / det;
        let dt_dy = ((ct - at) * (bx - ax) - (bt - at) * (cx - ax)) / det;
        if !dt_dx.is_finite() || !dt_dy.is_finite() {
            return RasterOutcome::Degenerate;
        }
        let depth_at =
            |x: f32, y: f32| (at + dt_dx * (f64::from(x) - ax) + dt_dy * (f64::from(y) - ay)) as f32;
        // Every point of the triangle lies between its vertex depths (t is affine on it).
        let vertex_t_lo = a.t.min(b.t).min(c.t);
        let vertex_t_hi = a.t.max(b.t).max(c.t);

        // Fixed-point vertices, oriented so the edge functions are non-negative inside.
        let mut vx = [to_fixed(a.x), to_fixed(b.x), to_fixed(c.x)];
        let mut vy = [to_fixed(a.y), to_fixed(b.y), to_fixed(c.y)];
        let area2 = edge(vx[0], vy[0], vx[1], vy[1], vx[2], vy[2]);
        if area2 == 0 {
            return RasterOutcome::Degenerate;
        }
        if area2 < 0 {
            vx.swap(1, 2);
            vy.swap(1, 2);
        }
        let (Some(min_x), Some(max_x), Some(min_y), Some(max_y)) = (
            vx.iter().copied().min(),
            vx.iter().copied().max(),
            vy.iter().copied().min(),
            vy.iter().copied().max(),
        ) else {
            return RasterOutcome::Degenerate;
        };
        // Pixels touching the triangle's bounding box, padded by the vertex rounding (so the
        // depth interval reaches every tile the true triangle enters), clamped to the buffer.
        const PAD: i64 = SUBPIXEL / 64;
        let px0 = (min_x - PAD).div_euclid(SUBPIXEL).max(0);
        let px1 = (max_x + PAD).div_euclid(SUBPIXEL).min(i64::from(self.width) - 1);
        let py0 = (min_y - PAD).div_euclid(SUBPIXEL).max(0);
        let py1 = (max_y + PAD).div_euclid(SUBPIXEL).min(i64::from(self.height) - 1);
        if px0 > px1 || py0 > py1 {
            return RasterOutcome::OffBuffer;
        }
        let tile = i64::from(TILE);
        for ty in (py0 / tile)..=(py1 / tile) {
            for tx in (px0 / tile)..=(px1 / tile) {
                // Conservative depth interval of this triangle over the whole tile: the
                // plane's extrema over a rectangle sit at its corners, intersected with the
                // vertex range.
                let x0 = (tx * tile) as f32;
                let y0 = (ty * tile) as f32;
                let x1 = x0 + TILE as f32;
                let y1 = y0 + TILE as f32;
                let mut t_lo = f32::INFINITY;
                let mut t_hi = f32::NEG_INFINITY;
                for (cx, cy) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
                    let t = depth_at(cx, cy);
                    t_lo = t_lo.min(t);
                    t_hi = t_hi.max(t);
                }
                let (t_lo, t_hi) = if t_lo.is_finite() && t_hi.is_finite() {
                    (t_lo.max(vertex_t_lo), t_hi.min(vertex_t_hi))
                } else {
                    // An overflowing plane falls back to the (always valid) vertex range.
                    (vertex_t_lo, vertex_t_hi)
                };
                let mut centres = 0u64;
                let x_from = (tx * tile).max(px0);
                let x_to = (tx * tile + tile - 1).min(px1);
                let y_from = (ty * tile).max(py0);
                let y_to = (ty * tile + tile - 1).min(py1);
                for py in y_from..=y_to {
                    for px in x_from..=x_to {
                        let cx = px * SUBPIXEL + SUBPIXEL / 2;
                        let cy = py * SUBPIXEL + SUBPIXEL / 2;
                        if point_in_triangle(&vx, &vy, cx, cy) {
                            centres |= 1u64 << ((py - ty * tile) * tile + (px - tx * tile));
                        }
                    }
                }
                let accum = self.accumulator(tx as u32, ty as u32);
                accum.centres |= centres;
                accum.own_centres |= centres;
                accum.t_lo = accum.t_lo.min(t_lo);
                accum.t_hi = accum.t_hi.max(t_hi);
            }
        }
        RasterOutcome::Rasterised
    }

    /// Marks every pixel whose square, grown by `EDGE_GUARD` on each side, meets the
    /// screen segment `a`-`b`, an edge of the current triangle's polygon: always in its own
    /// edge mask, and in the call's boundary mask when `boundary` (the edge is on the
    /// boundary of the call's triangle union, so the pixels it touches may hold a gap).
    fn mark_edge(&mut self, a: ScreenVert, b: ScreenVert, boundary: bool) {
        let (w, h) = (self.width as f32, self.height as f32);
        let Some((x0, y0, x1, y1)) = clip_segment((a.x, a.y), (b.x, b.y), (-1.0, -1.0), (w + 1.0, h + 1.0))
        else {
            return;
        };
        let (y_lo, y_hi) = (y0.min(y1), y0.max(y1));
        // Row `py`'s grown band `[py - G, py + 1 + G]` meets `[y_lo, y_hi]`.
        let row_from = ((y_lo - 1.0 - EDGE_GUARD).ceil() as i64).max(0);
        let row_to = ((y_hi + EDGE_GUARD).floor() as i64).min(i64::from(self.height) - 1);
        let dy = y1 - y0;
        for py in row_from..=row_to {
            let band_lo = py as f32 - EDGE_GUARD;
            let band_hi = py as f32 + 1.0 + EDGE_GUARD;
            let (xa, xb) = if dy.abs() < 1e-12 {
                (x0, x1)
            } else {
                let sa = ((band_lo - y0) / dy).clamp(0.0, 1.0);
                let sb = ((band_hi - y0) / dy).clamp(0.0, 1.0);
                (x0 + (x1 - x0) * sa, x0 + (x1 - x0) * sb)
            };
            let (x_lo, x_hi) = (xa.min(xb), xa.max(xb));
            let col_from = ((x_lo - 1.0 - EDGE_GUARD).ceil() as i64).max(0);
            let col_to = ((x_hi + EDGE_GUARD).floor() as i64).min(i64::from(self.width) - 1);
            for px in col_from..=col_to {
                let (tx, ty) = ((px / i64::from(TILE)) as u32, (py / i64::from(TILE)) as u32);
                let bit = (py % i64::from(TILE)) * i64::from(TILE) + px % i64::from(TILE);
                let accum = self.accumulator(tx, ty);
                accum.own_edges |= 1u64 << bit;
                if boundary {
                    accum.edges |= 1u64 << bit;
                }
            }
        }
    }

    /// The per-draw accumulator of tile `(tx, ty)`, registering it for the merge pass.
    fn accumulator(&mut self, tx: u32, ty: u32) -> &mut TileAccum {
        let index = (ty * self.tiles_x + tx) as usize;
        if !self.accum[index].touched {
            self.accum[index].touched = true;
            self.touched.push(index as u32);
        }
        if !self.accum[index].own_touched {
            self.accum[index].own_touched = true;
            self.own_touched.push(index as u32);
        }
        &mut self.accum[index]
    }

    /// Merges every touched tile's accumulated coverage (pixel centres inside the call's
    /// triangle union minus pixels a boundary edge touches, plus pixels entirely inside one
    /// triangle's polygon) and resets the accumulators.
    fn flush_accumulators(&mut self) {
        let touched = std::mem::take(&mut self.touched);
        for &index in &touched {
            let index = index as usize;
            let accum = self.accum[index];
            self.accum[index] = TileAccum::EMPTY;
            let tx = index as u32 % self.tiles_x;
            let ty = index as u32 / self.tiles_x;
            let coverage = ((accum.centres & !accum.edges) | accum.full) & self.tile_valid_mask(tx, ty);
            if coverage != 0
                && self.tiles[index].merge(coverage, accum.t_lo, accum.t_hi) != MergeOutcome::NoCoverage
            {
                self.stats.tile_merges += 1;
            }
        }
        self.touched = touched;
        self.touched.clear();
    }

    /// Bits of tile `(tx, ty)` whose pixels lie inside the buffer (edge tiles are partial).
    fn tile_valid_mask(&self, tx: u32, ty: u32) -> u64 {
        let mut mask = 0u64;
        for ly in 0..TILE {
            for lx in 0..TILE {
                if tx * TILE + lx < self.width && ty * TILE + ly < self.height {
                    mask |= 1u64 << (ly * TILE + lx);
                }
            }
        }
        mask
    }
}

/// Bits of tile `(tx, ty)` inside `rect` (the rect is known to lie inside the buffer).
fn rect_tile_mask(rect: ScreenRect, tx: u32, ty: u32) -> u64 {
    let origin_x = (tx * TILE) as i32;
    let origin_y = (ty * TILE) as i32;
    let mut mask = 0u64;
    for ly in 0..TILE as i32 {
        for lx in 0..TILE as i32 {
            let (px, py) = (origin_x + lx, origin_y + ly);
            if px >= rect.x0 && px < rect.x1 && py >= rect.y0 && py < rect.y1 {
                mask |= 1u64 << (ly * TILE as i32 + lx);
            }
        }
    }
    mask
}

/// `true` when every matrix element is finite.
fn matrix_is_finite(m: &Mat4) -> bool {
    m.to_cols_array().iter().all(|v| v.is_finite())
}

/// `true` when every component is finite.
fn vec3_is_finite(v: Vec3) -> bool {
    v.x.is_finite() && v.y.is_finite() && v.z.is_finite()
}

/// `true` when every component is finite.
fn vec4_is_finite(v: Vec4) -> bool {
    v.x.is_finite() && v.y.is_finite() && v.z.is_finite() && v.w.is_finite()
}

/// Fixed-point edge function (Pineda 1988): twice the signed area of `(a, b, p)` in
/// fixed-point units; after orientation, a point is inside when all three edges are `>= 0`.
#[inline]
fn edge(ax: i64, ay: i64, bx: i64, by: i64, px: i64, py: i64) -> i64 {
    (bx - ax) * (py - ay) - (by - ay) * (px - ax)
}

/// Closed-triangle test for the oriented fixed-point triangle.
#[inline]
fn point_in_triangle(vx: &[i64; 3], vy: &[i64; 3], px: i64, py: i64) -> bool {
    edge(vx[0], vy[0], vx[1], vy[1], px, py) >= 0
        && edge(vx[1], vy[1], vx[2], vy[2], px, py) >= 0
        && edge(vx[2], vy[2], vx[0], vy[0], px, py) >= 0
}

/// Pixel coordinate to 8.8-style fixed point (round to nearest sub-unit). Coordinates of a
/// viewport-clipped vertex lie within the buffer, so the product cannot overflow.
#[inline]
fn to_fixed(v: f32) -> i64 {
    (v * SUBPIXEL as f32).round() as i64
}

/// Sutherland–Hodgman clip of a clip-space polygon against the half-space `distance(v) >= 0`.
/// Each vertex carries the tag of the edge to the next vertex (source edge `0..3` or
/// `EDGE_CLIP`); cut edges keep their source tag and new edges along the plane get `EDGE_CLIP`.
fn clip_polygon(poly: &[(DVec4, u8)], distance: impl Fn(DVec4) -> f64) -> Vec<(DVec4, u8)> {
    let mut out: Vec<(DVec4, u8)> = Vec::with_capacity(poly.len() + 1);
    let Some(&last) = poly.last() else {
        return out;
    };
    let (mut prev, mut prev_tag) = last;
    let mut prev_d = distance(prev);
    for &(curr, curr_tag) in poly {
        let curr_d = distance(curr);
        let prev_in = prev_d >= 0.0;
        let curr_in = curr_d >= 0.0;
        if prev_in != curr_in {
            let s = prev_d / (prev_d - curr_d);
            let cut = prev + (curr - prev) * s;
            // Leaving: the new edge runs along the plane. Entering: it continues the cut edge.
            out.push((cut, if prev_in { EDGE_CLIP } else { prev_tag }));
        }
        if curr_in {
            out.push((curr, curr_tag));
        }
        prev = curr;
        prev_tag = curr_tag;
        prev_d = curr_d;
    }
    out
}

/// Liang–Barsky clip of the segment `(x0, y0)`-`(x1, y1)` to a rectangle, or `None` when it
/// misses the rectangle.
fn clip_segment(
    (x0, y0): (f32, f32),
    (x1, y1): (f32, f32),
    (min_x, min_y): (f32, f32),
    (max_x, max_y): (f32, f32),
) -> Option<(f32, f32, f32, f32)> {
    let (dx, dy) = (x1 - x0, y1 - y0);
    let mut s0 = 0.0f32;
    let mut s1 = 1.0f32;
    for (p, q) in [
        (-dx, x0 - min_x),
        (dx, max_x - x0),
        (-dy, y0 - min_y),
        (dy, max_y - y0),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                s0 = s0.max(r);
            } else {
                s1 = s1.min(r);
            }
        }
    }
    if s0 > s1 {
        return None;
    }
    Some((x0 + dx * s0, y0 + dy * s0, x0 + dx * s1, y0 + dy * s1))
}

/// Source-edge identity for [`edge_is_interior`]: the endpoints' bit patterns (negative zero
/// folded to zero), smaller first, plus `+1` when the edge runs from the smaller endpoint,
/// `-1` otherwise. `None` for a zero-length edge.
fn edge_key(from: Vec3, to: Vec3) -> Option<(EdgeKey, i8)> {
    let bits = |v: Vec3| {
        [
            (v.x + 0.0).to_bits(),
            (v.y + 0.0).to_bits(),
            (v.z + 0.0).to_bits(),
        ]
    };
    let (a, b) = (bits(from), bits(to));
    match a.cmp(&b) {
        std::cmp::Ordering::Less => Some(((a, b), 1)),
        std::cmp::Ordering::Greater => Some(((b, a), -1)),
        std::cmp::Ordering::Equal => None,
    }
}

/// `true` when exactly two triangles of the call hold source edge `key` and they lie on
/// opposite sides of it on screen: the edge is then inside their union, not on its boundary.
/// Any other count (an open border, a fold, a non-manifold edge) keeps it a boundary edge.
fn edge_is_interior(records: &[EdgeRecord], key: EdgeKey) -> bool {
    let start = records.partition_point(|r| r.key < key);
    let rest = &records[start..];
    let same = &rest[..rest.partition_point(|r| r.key == key)];
    same.len() == 2 && same[0].side != same[1].side
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::glam::camera::rh::proj::directx::{perspective, perspective_infinite_reverse};
    use std::f32::consts::FRAC_PI_3;

    /// Standard-depth projection, view = identity: the eye sits at the origin looking down
    /// `-Z`, y up. 60° vertical FOV, aspect 2 (matching the 256×128 test buffer), near 0.5.
    fn proj_standard() -> Mat4 {
        perspective(FRAC_PI_3, 2.0, 0.5, 100.0)
    }

    /// Reversed-Z infinite projection (the engine's), same view.
    fn proj_reversed() -> Mat4 {
        perspective_infinite_reverse(FRAC_PI_3, 2.0, 0.5)
    }

    /// A 256×128 standard-depth culler.
    fn culler() -> MaskedOcclusionCuller {
        MaskedOcclusionCuller::new_with_depth_mode(256, 128, DepthMode::Standard).expect("valid size")
    }

    /// A camera-facing quad (counter-clockwise in NDC) centred on `(cx, cy)` with half size
    /// `s` in the plane `z = -d`, as positions plus two-triangle indices.
    fn quad(cx: f32, cy: f32, s: f32, d: f32) -> (Vec<Vec3>, Vec<u32>) {
        (
            vec![
                Vec3::new(cx - s, cy - s, -d),
                Vec3::new(cx + s, cy - s, -d),
                Vec3::new(cx + s, cy + s, -d),
                Vec3::new(cx - s, cy + s, -d),
            ],
            vec![0, 1, 2, 0, 2, 3],
        )
    }

    /// Draws [`quad`] with the standard projection as its only transform.
    fn draw_quad(c: &mut MaskedOcclusionCuller, cx: f32, cy: f32, s: f32, d: f32) -> DrawReport {
        let (verts, idx) = quad(cx, cy, s, d);
        c.draw_occluder(&verts, &idx, proj_standard(), Backface::Cull)
            .expect("quad draws")
    }

    /// A cubic box centred on `(cx, cy)` at eye distance `d`.
    fn box_at(cx: f32, cy: f32, d: f32, half: f32) -> Aabb {
        Aabb::from_center_half_extents(Vec3::new(cx, cy, -d), Vec3::splat(half))
    }

    /// [`MaskedOcclusionCuller::test_aabb`] under the standard projection.
    fn test(c: &MaskedOcclusionCuller, aabb: Aabb) -> Visibility {
        c.test_aabb(aabb, proj_standard()).expect("finite test")
    }

    /// NDC depth of the plane at eye distance `d` under `proj`.
    fn ndc_z_at(proj: Mat4, d: f32) -> f32 {
        let clip = proj * Vec4::new(0.0, 0.0, -d, 1.0);
        clip.z / clip.w
    }

    /// Deterministic fingerprint of the tile state (FNV-1a over masks and depth bits).
    fn fingerprint(c: &MaskedOcclusionCuller) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let mut mix = |word: u64| {
            hash ^= word;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        };
        for tile in &c.tiles {
            mix(tile.mask[0]);
            mix(tile.mask[1]);
            mix(u64::from(tile.depth[0].to_bits()));
            mix(u64::from(tile.depth[1].to_bits()));
        }
        hash
    }

    /// A tiny seeded generator (SplitMix-style LCG) so every random test is reproducible.
    struct Rng(u64);

    impl Rng {
        /// Next value in `[0, 1)`.
        fn unit(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 40) as f32) / ((1u64 << 24) as f32)
        }

        /// Next value in `[lo, hi)`.
        fn range(&mut self, lo: f32, hi: f32) -> f32 {
            lo + self.unit() * (hi - lo)
        }
    }

    #[test]
    fn new_rejects_zero_and_huge_sizes() {
        for (w, h) in [(0, 128), (256, 0), (0, 0), (4097, 8), (8, 5000)] {
            assert_eq!(
                MaskedOcclusionCuller::new(w, h).expect_err("bad size"),
                OcclusionError::InvalidSize { width: w, height: h }
            );
        }
        let ok = MaskedOcclusionCuller::new(256, 128).expect("default size");
        assert_eq!((ok.width(), ok.height()), (256, 128));
        assert_eq!(ok.tile_grid(), (32, 16));
        assert_eq!(ok.tile_count(), 512);
        assert_eq!(ok.depth_mode(), DepthMode::Reversed);
    }

    #[test]
    fn empty_buffer_reports_everything_visible() {
        let c = culler();
        assert_eq!(test(&c, box_at(0.0, 0.0, 10.0, 0.5)), Visibility::Visible);
        assert_eq!(
            c.test_rect_depth(ScreenRect::new(10, 10, 40, 40), ndc_z_at(proj_standard(), 10.0)),
            Visibility::Visible
        );
        assert_eq!(c.covered_pixel_count(), 0);
    }

    #[test]
    fn clear_resets_coverage_but_keeps_stats() {
        let mut c = culler();
        draw_quad(&mut c, 0.0, 0.0, 4.0, 5.0);
        assert_eq!(test(&c, box_at(0.0, 0.0, 10.0, 0.5)), Visibility::Occluded);
        c.clear();
        assert_eq!(test(&c, box_at(0.0, 0.0, 10.0, 0.5)), Visibility::Visible);
        assert_eq!(c.covered_pixel_count(), 0);
        assert_eq!(c.stats().triangles_submitted, 2);
        c.reset_stats();
        assert_eq!(c.stats(), MaskedStats::default());
    }

    #[test]
    fn big_quad_hides_a_box_behind_it() {
        let mut c = culler();
        let report = draw_quad(&mut c, 0.0, 0.0, 4.0, 5.0);
        assert_eq!(report.submitted, 2);
        assert_eq!(report.rasterised, 2);
        assert!(c.covered_pixel_count() > 10_000);
        assert_eq!(test(&c, box_at(0.0, 0.0, 10.0, 0.5)), Visibility::Occluded);
        assert_eq!(test(&c, box_at(1.0, -0.5, 25.0, 1.0)), Visibility::Occluded);
    }

    #[test]
    fn big_quad_does_not_hide_a_box_in_front_of_it() {
        let mut c = culler();
        draw_quad(&mut c, 0.0, 0.0, 4.0, 5.0);
        assert_eq!(test(&c, box_at(0.0, 0.0, 2.5, 0.4)), Visibility::Visible);
        // Straddling the quad's plane: the front half must stay visible.
        assert_eq!(test(&c, box_at(0.0, 0.0, 5.0, 0.4)), Visibility::Visible);
    }

    #[test]
    fn big_quad_does_not_hide_a_box_beside_it() {
        let mut c = culler();
        draw_quad(&mut c, 0.0, 0.0, 4.0, 5.0);
        // The quad's ray cone reaches |x| < 8 at distance 10; this box sits outside it.
        assert_eq!(test(&c, box_at(9.5, 0.0, 10.0, 0.5)), Visibility::Visible);
        assert_eq!(test(&c, box_at(-9.5, 1.0, 10.0, 0.5)), Visibility::Visible);
        assert_eq!(test(&c, box_at(0.0, 6.5, 10.0, 0.5)), Visibility::Visible);
    }

    #[test]
    fn partially_visible_box_is_never_occluded() {
        let mut c = culler();
        draw_quad(&mut c, 0.0, 0.0, 4.0, 5.0);
        // Half behind the quad's cone (|x| < 8 at distance 10), half beside it.
        assert_eq!(test(&c, box_at(8.0, 0.0, 10.0, 1.0)), Visibility::Visible);
        // Poking out above the quad (cone reaches |y| < 8 at distance 10).
        assert_eq!(test(&c, box_at(0.0, 8.0, 10.0, 1.0)), Visibility::Visible);
        // Mostly hidden, one corner in front of the quad plane.
        assert_eq!(test(&c, box_at(0.0, 0.0, 5.4, 0.45)), Visibility::Visible);
    }

    #[test]
    fn occluder_crossing_the_near_plane_still_occludes() {
        // A quad whose left edge is behind the eye (z = +1) and right edge in front
        // (z = -3): near-plane clipping keeps the front strip, which covers the centre.
        let verts = vec![
            Vec3::new(-2.0, -4.0, 1.0),
            Vec3::new(2.0, -4.0, -3.0),
            Vec3::new(2.0, 4.0, -3.0),
            Vec3::new(-2.0, 4.0, 1.0),
        ];
        let idx = vec![0, 1, 2, 0, 2, 3];
        let mut c = culler();
        let report = c
            .draw_occluder(&verts, &idx, proj_standard(), Backface::None)
            .expect("crossing quad draws");
        assert_eq!(report.rasterised, 2);
        assert_eq!(report.clipped_away, 0);
        assert_eq!(test(&c, box_at(0.0, 0.0, 10.0, 0.4)), Visibility::Occluded);
        // A box seen past the near-plane cut edge (which projects to NDC x = -0.866) stays
        // visible.
        assert_eq!(test(&c, box_at(-10.5, 0.0, 10.0, 0.4)), Visibility::Visible);
    }

    #[test]
    fn occluder_entirely_inside_the_near_plane_draws_nothing() {
        let mut c = culler();
        // Between the eye (z = 0) and the near plane (distance 0.5): clipped like on GPU.
        let report = draw_quad(&mut c, 0.0, 0.0, 4.0, 0.25);
        assert_eq!(report.rasterised, 0);
        assert_eq!(report.clipped_away, 2);
        assert_eq!(test(&c, box_at(0.0, 0.0, 10.0, 0.5)), Visibility::Visible);
    }

    #[test]
    fn box_crossing_or_behind_the_near_plane_is_visible() {
        let mut c = culler();
        draw_quad(&mut c, 0.0, 0.0, 40.0, 5.0);
        // Crossing the near plane (front face at distance 0.1).
        assert_eq!(test(&c, box_at(0.0, 0.0, 0.6, 0.5)), Visibility::Visible);
        // Entirely behind the eye (z > 0): an occlusion test never frustum-culls.
        let behind = Aabb::from_center_half_extents(Vec3::new(0.0, 0.0, 5.0), Vec3::splat(0.5));
        assert_eq!(test(&c, behind), Visibility::Visible);
        // Surrounding the eye.
        assert_eq!(test(&c, box_at(0.0, 0.0, 0.0, 2.0)), Visibility::Visible);
    }

    #[test]
    fn backface_cull_discards_reversed_winding() {
        let (verts, _) = quad(0.0, 0.0, 4.0, 5.0);
        let reversed = vec![0, 2, 1, 0, 3, 2];
        let mut culled = culler();
        let report = culled
            .draw_occluder(&verts, &reversed, proj_standard(), Backface::Cull)
            .expect("draws");
        assert_eq!(report.culled_backface, 2);
        assert_eq!(report.rasterised, 0);
        assert_eq!(test(&culled, box_at(0.0, 0.0, 10.0, 0.5)), Visibility::Visible);
        // The same mesh with culling off occludes.
        let mut kept = culler();
        let report = kept
            .draw_occluder(&verts, &reversed, proj_standard(), Backface::None)
            .expect("draws");
        assert_eq!(report.rasterised, 2);
        assert_eq!(test(&kept, box_at(0.0, 0.0, 10.0, 0.5)), Visibility::Occluded);
    }

    #[test]
    fn backface_none_draws_either_winding_identically() {
        let (verts, forward) = quad(0.5, -0.25, 3.0, 6.0);
        let reversed = vec![0, 2, 1, 0, 3, 2];
        let mut a = culler();
        let mut b = culler();
        a.draw_occluder(&verts, &forward, proj_standard(), Backface::None)
            .expect("draws");
        b.draw_occluder(&verts, &reversed, proj_standard(), Backface::None)
            .expect("draws");
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn tile_merge_replaces_front_when_coverage_is_a_superset() {
        let mut tile = Tile::EMPTY;
        assert_eq!(tile.merge(0b1111, 0.40, 0.50), MergeOutcome::ReplacedFront);
        assert_eq!(tile.depth, [0.50, f32::INFINITY]);
        // Nearer and covering everything the front layer held: front depth tightens.
        assert_eq!(tile.merge(0b1111_1111, 0.10, 0.20), MergeOutcome::ReplacedFront);
        assert_eq!(tile.depth[0], 0.20);
        assert_eq!(tile.mask[0], 0b1111_1111);
    }

    #[test]
    fn tile_merge_pushes_front_to_back_when_back_is_empty() {
        let mut tile = Tile::EMPTY;
        tile.merge(0xFF00, 0.40, 0.50);
        // Nearer, disjoint coverage: both guarantees survive, front first.
        assert_eq!(tile.merge(0x00FF, 0.10, 0.20), MergeOutcome::PushedFrontToBack);
        assert_eq!(tile.depth, [0.20, 0.50]);
        assert_eq!(tile.mask, [0x00FF, 0xFF00]);
        assert!(tile.depth[0] <= tile.depth[1]);
    }

    #[test]
    fn tile_merge_keeps_front_depth_when_both_layers_hold_coverage() {
        let mut tile = Tile::EMPTY;
        tile.merge(0xF000, 0.40, 0.50);
        tile.merge(0x0F00, 0.10, 0.20); // pushes: front 0.20, back 0.50
        // Nearer still, partial coverage, back occupied: OR into front, depth kept.
        assert_eq!(tile.merge(0x000F, 0.05, 0.08), MergeOutcome::MergedFront);
        assert_eq!(tile.depth[0], 0.20);
        assert_eq!(tile.mask[0], 0x0F0F);
        // The kept depth still bounds every covered pixel's surface (0.08 <= 0.20).
        assert_eq!(tile.occlusion_mask(0.21) & 0x000F, 0x000F);
        assert_eq!(tile.occlusion_mask(0.20) & 0x000F, 0);
    }

    #[test]
    fn tile_merge_back_layer_and_behind_both_layers() {
        let mut tile = Tile::EMPTY;
        tile.merge(0xFFFF_0000, 0.10, 0.20);
        // Behind the front bound, in front of the (empty) back: becomes the back layer.
        assert_eq!(tile.merge(0x0000_FFFF, 0.50, 0.60), MergeOutcome::ReplacedBack);
        assert_eq!(tile.depth, [0.20, 0.60]);
        // Behind both, adding new pixels: the back layer widens to the new depth.
        assert_eq!(
            tile.merge(0xFFFF_FFFF_0000_0000, 0.80, 0.90),
            MergeOutcome::MergedBack
        );
        assert_eq!(tile.depth[1], 0.90);
        assert_eq!(tile.mask[1], 0xFFFF_FFFF_0000_FFFF);
        assert!(tile.depth[0] <= tile.depth[1]);
    }

    #[test]
    fn tile_merge_skips_triangles_hidden_behind_existing_coverage() {
        let mut tile = Tile::EMPTY;
        tile.merge(u64::MAX, 0.10, 0.20);
        let before = tile;
        assert_eq!(tile.merge(0x1234, 0.50, 0.60), MergeOutcome::SkippedOccluded);
        assert_eq!(tile, before);
        // Empty / invalid merges are no-ops, never panics.
        assert_eq!(tile.merge(0, 0.1, 0.2), MergeOutcome::NoCoverage);
        assert_eq!(tile.merge(0xFF, f32::NAN, 0.2), MergeOutcome::NoCoverage);
        assert_eq!(tile.merge(0xFF, 0.3, 0.2), MergeOutcome::NoCoverage);
        assert_eq!(tile, before);
    }

    #[test]
    fn tile_occlusion_mask_respects_layer_depths() {
        let mut tile = Tile::EMPTY;
        tile.merge(0xF0, 0.10, 0.20);
        tile.merge(0x0F, 0.50, 0.60);
        assert_eq!(tile.occlusion_mask(0.15), 0);
        assert_eq!(tile.occlusion_mask(0.20), 0); // strictly nearer only
        assert_eq!(tile.occlusion_mask(0.21), 0xF0);
        assert_eq!(tile.occlusion_mask(0.61), 0xFF);
        assert_eq!(Tile::EMPTY.occlusion_mask(1.0), 0);
    }

    #[test]
    fn one_by_one_buffer_works() {
        let mut c =
            MaskedOcclusionCuller::new_with_depth_mode(1, 1, DepthMode::Standard).expect("1x1 is valid");
        assert_eq!(c.tile_grid(), (1, 1));
        let (verts, idx) = quad(0.0, 0.0, 10.0, 5.0);
        let report = c
            .draw_occluder(&verts, &idx, proj_standard(), Backface::Cull)
            .expect("draws");
        assert_eq!(report.rasterised, 2);
        assert_eq!(c.covered_pixel_count(), 1);
        let whole = ScreenRect::new(0, 0, 1, 1);
        assert_eq!(
            c.test_rect_depth(whole, ndc_z_at(proj_standard(), 10.0)),
            Visibility::Occluded
        );
        assert_eq!(
            c.test_rect_depth(whole, ndc_z_at(proj_standard(), 2.0)),
            Visibility::Visible
        );
        // A box test always expands by a guard pixel, so on a 1×1 buffer it reaches outside
        // and reports the defined fallback.
        assert_eq!(test(&c, box_at(0.0, 0.0, 10.0, 0.5)), Visibility::Visible);
    }

    #[test]
    fn odd_and_partial_tile_sizes_work() {
        for (w, h) in [(1, 8), (8, 1), (7, 5), (13, 11), (8, 8), (9, 9)] {
            let mut c =
                MaskedOcclusionCuller::new_with_depth_mode(w, h, DepthMode::Standard).expect("valid size");
            let (verts, idx) = quad(0.0, 0.0, 10.0, 5.0);
            c.draw_occluder(&verts, &idx, proj_standard(), Backface::Cull)
                .expect("draws");
            // Every buffer pixel is covered, partial edge tiles included.
            assert_eq!(c.covered_pixel_count(), u64::from(w * h), "{w}x{h}");
            let whole = ScreenRect::new(0, 0, w as i32, h as i32);
            assert_eq!(
                c.test_rect_depth(whole, ndc_z_at(proj_standard(), 10.0)),
                Visibility::Occluded,
                "{w}x{h}"
            );
            assert_eq!(
                c.test_rect_depth(whole, ndc_z_at(proj_standard(), 2.0)),
                Visibility::Visible,
                "{w}x{h}"
            );
        }
    }

    #[test]
    fn nan_and_infinite_matrices_return_errors() {
        let (verts, idx) = quad(0.0, 0.0, 4.0, 5.0);
        let mut nan_cols = proj_standard().to_cols_array_2d();
        nan_cols[1][1] = f32::NAN;
        let nan_matrix = Mat4::from_cols_array_2d(&nan_cols);
        let inf_matrix = Mat4::from_cols_array_2d(&[[f32::INFINITY; 4]; 4]);
        let mut c = culler();
        for bad in [nan_matrix, inf_matrix] {
            assert_eq!(
                c.draw_occluder(&verts, &idx, bad, Backface::Cull)
                    .expect_err("bad matrix"),
                OcclusionError::NonFiniteMatrix
            );
            assert_eq!(
                c.test_aabb(box_at(0.0, 0.0, 10.0, 0.5), bad)
                    .expect_err("bad matrix"),
                OcclusionError::NonFiniteMatrix
            );
        }
        // Nothing was drawn by the rejected calls.
        assert_eq!(c.covered_pixel_count(), 0);
        assert_eq!(c.stats().triangles_submitted, 0);
    }

    #[test]
    fn nan_bounds_error_and_empty_bounds_are_visible() {
        let mut c = culler();
        draw_quad(&mut c, 0.0, 0.0, 40.0, 5.0);
        let nan_box = Aabb::new(Vec3::new(f32::NAN, 0.0, -10.0), Vec3::new(1.0, 1.0, -9.0));
        assert_eq!(
            c.test_aabb(nan_box, proj_standard()).expect_err("nan box"),
            OcclusionError::NonFiniteBounds
        );
        let inf_box = Aabb::new(Vec3::new(-1.0, -1.0, -11.0), Vec3::splat(f32::INFINITY));
        assert_eq!(
            c.test_aabb(inf_box, proj_standard()).expect_err("inf box"),
            OcclusionError::NonFiniteBounds
        );
        assert_eq!(test(&c, Aabb::EMPTY), Visibility::Visible);
        let inverted = Aabb::new(Vec3::splat(1.0), Vec3::splat(-1.0));
        assert_eq!(test(&c, inverted), Visibility::Visible);
    }

    #[test]
    fn invalid_indices_error_and_empty_draws_succeed() {
        let (verts, _) = quad(0.0, 0.0, 4.0, 5.0);
        let mut c = culler();
        assert_eq!(
            c.draw_occluder(&verts, &[0, 1], proj_standard(), Backface::Cull)
                .expect_err("short indices"),
            OcclusionError::InvalidIndexCount { count: 2 }
        );
        assert_eq!(
            c.draw_occluder(&verts, &[0, 1, 4], proj_standard(), Backface::Cull)
                .expect_err("index out of range"),
            OcclusionError::IndexOutOfRange {
                index: 4,
                vertex_count: 4
            }
        );
        assert_eq!(c.covered_pixel_count(), 0, "rejected calls draw nothing");
        let empty = c
            .draw_occluder(&[], &[], proj_standard(), Backface::Cull)
            .expect("empty draw succeeds");
        assert_eq!(empty, DrawReport::default());
        let no_indices = c
            .draw_occluder(&verts, &[], proj_standard(), Backface::None)
            .expect("no indices succeeds");
        assert_eq!(no_indices.submitted, 0);
    }

    #[test]
    fn degenerate_and_nonfinite_triangles_are_skipped_not_fatal() {
        let verts = vec![
            Vec3::new(0.0, 0.0, -5.0),
            Vec3::new(1.0, 0.0, -5.0),
            Vec3::new(2.0, 0.0, -5.0), // collinear with the first two
            Vec3::new(f32::NAN, 0.0, -5.0),
            Vec3::new(0.0, 1.0, -5.0),
            Vec3::new(0.0, 0.0, -5.0), // repeats vertex 0
        ];
        let idx = vec![0, 1, 2, 0, 3, 4, 0, 5, 1];
        let mut c = culler();
        let report = c
            .draw_occluder(&verts, &idx, proj_standard(), Backface::None)
            .expect("degenerate input still draws");
        assert_eq!(report.submitted, 3);
        assert_eq!(report.rasterised, 0);
        assert_eq!(report.non_finite, 1);
        assert_eq!(report.degenerate, 2);
        assert_eq!(c.covered_pixel_count(), 0);
    }

    #[test]
    fn test_rect_depth_cases() {
        let mut c = culler();
        draw_quad(&mut c, 0.0, 0.0, 4.0, 5.0);
        let centre = ScreenRect::new(120, 56, 136, 72);
        let proj = proj_standard();
        assert_eq!(
            c.test_rect_depth(centre, ndc_z_at(proj, 10.0)),
            Visibility::Occluded
        );
        assert_eq!(
            c.test_rect_depth(centre, ndc_z_at(proj, 3.0)),
            Visibility::Visible
        );
        // Exactly at the occluder's depth: not occluded (strictly nearer wins).
        assert_eq!(
            c.test_rect_depth(centre, ndc_z_at(proj, 5.0)),
            Visibility::Visible
        );
        // The defined fallbacks.
        assert_eq!(c.test_rect_depth(ScreenRect::default(), 0.9), Visibility::Visible);
        assert_eq!(
            c.test_rect_depth(ScreenRect::new(50, 50, 40, 60), 0.9),
            Visibility::Visible
        );
        assert_eq!(
            c.test_rect_depth(ScreenRect::new(-1, 56, 136, 72), ndc_z_at(proj, 10.0)),
            Visibility::Visible
        );
        assert_eq!(
            c.test_rect_depth(ScreenRect::new(120, 56, 257, 72), ndc_z_at(proj, 10.0)),
            Visibility::Visible
        );
        assert_eq!(c.test_rect_depth(centre, f32::NAN), Visibility::Visible);
        assert_eq!(c.test_rect_depth(centre, f32::INFINITY), Visibility::Visible);
        // A rectangle over uncovered pixels is visible whatever the depth.
        assert_eq!(
            c.test_rect_depth(ScreenRect::new(0, 0, 8, 8), ndc_z_at(proj, 90.0)),
            Visibility::Visible
        );
    }

    #[test]
    fn reversed_depth_mode_matches_the_engine_projection() {
        let mut c = MaskedOcclusionCuller::new(256, 128).expect("valid size");
        assert_eq!(c.depth_mode(), DepthMode::Reversed);
        let (verts, idx) = quad(0.0, 0.0, 4.0, 5.0);
        c.draw_occluder(&verts, &idx, proj_reversed(), Backface::Cull)
            .expect("draws");
        let behind = c
            .test_aabb(box_at(0.0, 0.0, 10.0, 0.5), proj_reversed())
            .expect("tests");
        let front = c
            .test_aabb(box_at(0.0, 0.0, 2.5, 0.4), proj_reversed())
            .expect("tests");
        assert_eq!(behind, Visibility::Occluded);
        assert_eq!(front, Visibility::Visible);
        // Rect depths are NDC in the culler's own convention (near = 1 here).
        assert_eq!(
            c.test_rect_depth(ScreenRect::new(120, 56, 136, 72), ndc_z_at(proj_reversed(), 10.0)),
            Visibility::Occluded
        );
    }

    #[test]
    fn two_occluders_combine_to_hide_what_neither_hides_alone() {
        let wide_box = box_at(0.0, 0.0, 10.0, 1.2);
        let mut left_only = culler();
        draw_quad(&mut left_only, -1.8, 0.0, 2.2, 5.0);
        assert_eq!(test(&left_only, wide_box), Visibility::Visible);
        let mut both = culler();
        draw_quad(&mut both, -1.8, 0.0, 2.2, 5.0);
        draw_quad(&mut both, 1.8, 0.0, 2.2, 5.0);
        assert_eq!(test(&both, wide_box), Visibility::Occluded);
    }

    #[test]
    fn nearer_occluder_layers_over_a_farther_one() {
        let mut c = culler();
        draw_quad(&mut c, 0.0, 0.0, 6.0, 10.0); // far wall first
        draw_quad(&mut c, 0.0, 0.0, 1.0, 4.0); // near patch over its centre
        // Between the two walls, behind the near patch.
        assert_eq!(test(&c, box_at(0.0, 0.0, 7.0, 0.3)), Visibility::Occluded);
        // Between the two walls, beside the near patch: only the far wall is behind it.
        assert_eq!(test(&c, box_at(3.0, 0.0, 7.0, 0.3)), Visibility::Visible);
        // Behind both walls.
        assert_eq!(test(&c, box_at(3.0, 0.0, 12.0, 0.3)), Visibility::Occluded);
        assert_eq!(test(&c, box_at(0.0, 0.0, 12.0, 0.3)), Visibility::Occluded);
    }

    #[test]
    fn slanted_occluder_uses_a_conservative_depth_interval() {
        // A quad rotated about y: left edge at distance 4, right edge at distance 12.
        let verts = vec![
            Vec3::new(-3.0, -3.0, -4.0),
            Vec3::new(3.0, -3.0, -12.0),
            Vec3::new(3.0, 3.0, -12.0),
            Vec3::new(-3.0, 3.0, -4.0),
        ];
        let idx = vec![0, 1, 2, 0, 2, 3];
        let mut c = culler();
        c.draw_occluder(&verts, &idx, proj_standard(), Backface::Cull)
            .expect("draws");
        // In front of the slanted surface along its own rays (surface at ~5.04).
        assert_eq!(test(&c, box_at(-2.2, 0.0, 5.0, 0.25)), Visibility::Visible);
        // Behind the surface along its rays (surface at ~6.97, box front at 8.7).
        assert_eq!(test(&c, box_at(-1.0, 0.0, 9.0, 0.3)), Visibility::Occluded);
    }

    #[test]
    fn offscreen_geometry_is_handled() {
        let mut c = culler();
        // Mostly off the right edge; its on-screen part still occludes.
        draw_quad(&mut c, 5.0, 0.0, 4.0, 5.0);
        assert_eq!(test(&c, box_at(6.0, 0.0, 10.0, 0.5)), Visibility::Occluded);
        // A box projecting partly outside the buffer is never occluded.
        assert_eq!(test(&c, box_at(11.0, 0.0, 10.0, 0.5)), Visibility::Visible);
        // A fully off-screen occluder rasterises nothing and is not an error.
        let report = draw_quad(&mut c, 100.0, 0.0, 4.0, 5.0);
        assert_eq!(report.clipped_away, 2);
    }

    #[test]
    fn model_transform_is_the_callers_to_fold_in() {
        // draw_occluder's matrix is the full object-to-clip transform: the same quad drawn
        // through `proj * translation` lands where the translated quad lands.
        let (verts, idx) = quad(0.0, 0.0, 2.0, 5.0);
        let model = Mat4::from_translation(Vec3::new(1.5, 0.5, 0.0));
        let mut folded = culler();
        folded
            .draw_occluder(&verts, &idx, proj_standard() * model, Backface::Cull)
            .expect("draws");
        let mut direct = culler();
        draw_quad(&mut direct, 1.5, 0.5, 2.0, 5.0);
        // Coverage masks match exactly; layer depths may differ in the last ulp between the
        // two association orders, so they are not compared bit for bit.
        assert!(
            folded
                .tiles
                .iter()
                .zip(&direct.tiles)
                .all(|(a, b)| a.mask == b.mask)
        );
        assert_eq!(test(&folded, box_at(1.5, 0.5, 10.0, 0.5)), Visibility::Occluded);
    }

    #[test]
    fn repeated_builds_are_bit_identical() {
        fn build() -> (u64, Vec<Visibility>) {
            let mut rng = Rng(0x5EED_1234);
            let mut c = culler();
            for _ in 0..30 {
                let (cx, cy) = (rng.range(-6.0, 6.0), rng.range(-3.0, 3.0));
                let (s, d) = (rng.range(0.5, 3.0), rng.range(3.0, 18.0));
                draw_quad(&mut c, cx, cy, s, d);
            }
            let mut results = Vec::new();
            for _ in 0..50 {
                let aabb = box_at(
                    rng.range(-6.0, 6.0),
                    rng.range(-3.0, 3.0),
                    rng.range(3.0, 22.0),
                    rng.range(0.2, 1.2),
                );
                results.push(test(&c, aabb));
            }
            (fingerprint(&c), results)
        }
        let (first_hash, first_results) = build();
        let (second_hash, second_results) = build();
        assert_eq!(first_hash, second_hash);
        assert_eq!(first_results, second_results);
        assert!(first_results.contains(&Visibility::Occluded));
        assert!(first_results.contains(&Visibility::Visible));
    }

    /// Independent oracle for the property test: the occluders rasterised at twice the
    /// culler's resolution with a plain barycentric scan (nearest depth per pixel centre),
    /// and boxes visibility-tested by casting one ray per oracle pixel through the box
    /// (slab test). Shares no code with the fixed-point rasteriser under test.
    struct Oracle {
        /// Oracle width in pixels.
        w: usize,
        /// Oracle height in pixels.
        h: usize,
        /// Nearest occluder NDC depth per pixel, `INFINITY` where uncovered.
        depth: Vec<f32>,
        /// The shared projection (view is the identity).
        proj: Mat4,
    }

    impl Oracle {
        /// Creates an empty oracle at `w`×`h`.
        fn new(w: usize, h: usize, proj: Mat4) -> Self {
            Self {
                w,
                h,
                depth: vec![f32::INFINITY; w * h],
                proj,
            }
        }

        /// Projects a world point to oracle pixel coordinates plus NDC depth.
        fn project(&self, p: Vec3) -> (f32, f32, f32) {
            let clip = self.proj * p.extend(1.0);
            let inv_w = 1.0 / clip.w;
            (
                (clip.x * inv_w * 0.5 + 0.5) * self.w as f32,
                (1.0 - (clip.y * inv_w * 0.5 + 0.5)) * self.h as f32,
                clip.z * inv_w,
            )
        }

        /// Rasterises one world-space triangle, keeping the nearest depth.
        fn draw_triangle(&mut self, tri: [Vec3; 3]) {
            let s = [self.project(tri[0]), self.project(tri[1]), self.project(tri[2])];
            let det = (s[1].0 - s[0].0) * (s[2].1 - s[0].1) - (s[1].1 - s[0].1) * (s[2].0 - s[0].0);
            if det.abs() < 1e-9 {
                return;
            }
            let x0 = s.iter().map(|v| v.0).fold(f32::INFINITY, f32::min).floor() as i32;
            let x1 = s.iter().map(|v| v.0).fold(f32::NEG_INFINITY, f32::max).ceil() as i32;
            let y0 = s.iter().map(|v| v.1).fold(f32::INFINITY, f32::min).floor() as i32;
            let y1 = s.iter().map(|v| v.1).fold(f32::NEG_INFINITY, f32::max).ceil() as i32;
            for py in y0.max(0)..y1.min(self.h as i32) {
                for px in x0.max(0)..x1.min(self.w as i32) {
                    let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
                    let w0 = ((s[1].0 - cx) * (s[2].1 - cy) - (s[1].1 - cy) * (s[2].0 - cx)) / det;
                    let w1 = ((s[2].0 - cx) * (s[0].1 - cy) - (s[2].1 - cy) * (s[0].0 - cx)) / det;
                    let w2 = 1.0 - w0 - w1;
                    if w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0 {
                        let z = w0 * s[0].2 + w1 * s[1].2 + w2 * s[2].2;
                        let slot = &mut self.depth[py as usize * self.w + px as usize];
                        *slot = slot.min(z);
                    }
                }
            }
        }

        /// `Visible` when any oracle pixel's ray reaches the box in front of the occluder
        /// depth stored there.
        fn test_box(&self, aabb: Aabb) -> Visibility {
            let cols = self.proj.to_cols_array();
            let (p00, p11) = (cols[0], cols[5]);
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for corner in 0..8u32 {
                let p = Vec3::new(
                    if corner & 1 == 0 { aabb.min.x } else { aabb.max.x },
                    if corner & 2 == 0 { aabb.min.y } else { aabb.max.y },
                    if corner & 4 == 0 { aabb.min.z } else { aabb.max.z },
                );
                let (sx, sy, _) = self.project(p);
                min_x = min_x.min(sx);
                min_y = min_y.min(sy);
                max_x = max_x.max(sx);
                max_y = max_y.max(sy);
            }
            let x0 = (min_x.floor() as i32).max(0);
            let x1 = (max_x.ceil() as i32).min(self.w as i32);
            let y0 = (min_y.floor() as i32).max(0);
            let y1 = (max_y.ceil() as i32).min(self.h as i32);
            for py in y0..y1 {
                for px in x0..x1 {
                    let ndc_x = ((px as f32 + 0.5) / self.w as f32) * 2.0 - 1.0;
                    let ndc_y = 1.0 - ((py as f32 + 0.5) / self.h as f32) * 2.0;
                    let dir = Vec3::new(ndc_x / p00, ndc_y / p11, -1.0);
                    let Some(entry_t) = ray_box_entry(dir, aabb) else {
                        continue;
                    };
                    let entry = dir * entry_t;
                    let clip = self.proj * entry.extend(1.0);
                    let box_z = clip.z / clip.w;
                    if box_z < self.depth[py as usize * self.w + px as usize] - 1e-5 {
                        return Visibility::Visible;
                    }
                }
            }
            Visibility::Occluded
        }
    }

    /// Slab entry parameter of the ray from the origin along `dir` into `aabb`
    /// (Kay & Kajiya 1986), or `None` on a miss.
    fn ray_box_entry(dir: Vec3, aabb: Aabb) -> Option<f32> {
        let mut t_near = 0.0f32;
        let mut t_far = f32::INFINITY;
        for axis in 0..3 {
            let (d, lo, hi) = (dir[axis], aabb.min[axis], aabb.max[axis]);
            if d == 0.0 {
                if 0.0 < lo || 0.0 > hi {
                    return None;
                }
                continue;
            }
            let inv = 1.0 / d;
            let (t1, t2) = (lo * inv, hi * inv);
            t_near = t_near.max(t1.min(t2));
            t_far = t_far.min(t1.max(t2));
            if t_far < t_near {
                return None;
            }
        }
        Some(t_near)
    }

    #[test]
    fn property_no_false_occluded_against_ray_cast_oracle() {
        let proj = proj_standard();
        let cols = proj.to_cols_array();
        let (p00, p11) = (cols[0], cols[5]);
        let mut rng = Rng(0x0CC1_0510_2026);
        let mut c = culler();
        let mut oracle = Oracle::new(512, 256, proj);
        // Forty-eight seeded camera-facing occluder quads into both buffers.
        for _ in 0..48 {
            let d = rng.range(4.0, 18.0);
            let cx = rng.range(-0.8, 0.8) * d / p00;
            let cy = rng.range(-0.8, 0.8) * d / p11;
            let s = rng.range(0.15, 0.65) * d / p11;
            let (verts, idx) = quad(cx, cy, s, d);
            c.draw_occluder(&verts, &idx, proj, Backface::Cull)
                .expect("occluder draws");
            oracle.draw_triangle([verts[0], verts[1], verts[2]]);
            oracle.draw_triangle([verts[0], verts[2], verts[3]]);
        }
        let (mut false_occluded, mut false_visible) = (0u32, 0u32);
        let (mut masked_occluded, mut oracle_occluded) = (0u32, 0u32);
        for _ in 0..2000 {
            let d = rng.range(3.0, 25.0);
            let aabb = box_at(
                rng.range(-0.95, 0.95) * d / p00,
                rng.range(-0.95, 0.95) * d / p11,
                d,
                rng.range(0.15, 1.25),
            );
            let masked = c.test_aabb(aabb, proj).expect("finite test");
            let truth = oracle.test_box(aabb);
            masked_occluded += u32::from(masked == Visibility::Occluded);
            oracle_occluded += u32::from(truth == Visibility::Occluded);
            if masked == Visibility::Occluded && truth == Visibility::Visible {
                false_occluded += 1;
            }
            if masked == Visibility::Visible && truth == Visibility::Occluded {
                false_visible += 1;
            }
        }
        let false_visible_rate = false_visible as f32 / oracle_occluded.max(1) as f32;
        assert_eq!(
            false_occluded, 0,
            "false Occluded is never allowed (masked {masked_occluded}, oracle {oracle_occluded})"
        );
        assert!(
            masked_occluded > 100,
            "the culler must actually cull (got {masked_occluded} of 2000)"
        );
        assert!(
            oracle_occluded > 100,
            "the scene must contain hidden boxes (got {oracle_occluded})"
        );
        assert!(
            false_visible_rate < 0.9,
            "false-Visible rate {false_visible_rate:.3} ({false_visible}/{oracle_occluded}) is implausibly high"
        );
    }

    #[test]
    fn draw_order_never_creates_a_false_occluded() {
        // Back-to-front and front-to-back must agree on the box between the walls at least
        // in never hiding the front box; the layered tile state may differ in tightness.
        for near_first in [true, false] {
            let mut c = culler();
            if near_first {
                draw_quad(&mut c, 0.0, 0.0, 2.0, 4.0);
                draw_quad(&mut c, 0.0, 0.0, 6.0, 10.0);
            } else {
                draw_quad(&mut c, 0.0, 0.0, 6.0, 10.0);
                draw_quad(&mut c, 0.0, 0.0, 2.0, 4.0);
            }
            assert_eq!(test(&c, box_at(0.0, 0.0, 2.5, 0.3)), Visibility::Visible);
            assert_eq!(test(&c, box_at(0.0, 0.0, 15.0, 0.5)), Visibility::Occluded);
        }
    }

    #[test]
    fn performance_guard_two_thousand_triangles_five_thousand_tests() {
        use std::time::Instant;
        let mut rng = Rng(0xFEED_5EED);
        let mut verts: Vec<Vec3> = Vec::new();
        let mut idx: Vec<u32> = Vec::new();
        for _ in 0..1000 {
            let base = verts.len() as u32;
            let (cx, cy) = (rng.range(-8.0, 8.0), rng.range(-4.0, 4.0));
            let (s, d) = (rng.range(0.3, 2.0), rng.range(3.0, 20.0));
            let (q, qi) = quad(cx, cy, s, d);
            verts.extend(q);
            idx.extend(qi.iter().map(|i| i + base));
        }
        let boxes: Vec<Aabb> = (0..5000)
            .map(|_| {
                box_at(
                    rng.range(-8.0, 8.0),
                    rng.range(-4.0, 4.0),
                    rng.range(3.0, 25.0),
                    rng.range(0.2, 1.5),
                )
            })
            .collect();
        let mut c = culler();
        let started = Instant::now();
        let report = c
            .draw_occluder(&verts, &idx, proj_standard(), Backface::Cull)
            .expect("draws");
        let drawn = started.elapsed();
        let mut occluded = 0u32;
        for aabb in &boxes {
            if test(&c, *aabb) == Visibility::Occluded {
                occluded += 1;
            }
        }
        let elapsed = started.elapsed();
        // Timing is reported, not asserted: the repo forbids wall-clock budgets in tests (they
        // fail on a loaded gate host). The asserts bound the work deterministically instead.
        eprintln!(
            "masked occlusion perf: draw 2000 triangles {drawn:?}, 5000 box tests {:?}, occluded {occluded}",
            elapsed - drawn
        );
        assert_eq!(report.submitted, 2000);
        assert!(report.rasterised > 1000, "rasterised {}", report.rasterised);
        assert!(c.stats().tile_merges > 0);
        // One merge per touched tile per call: the merge work is bounded by the tile count.
        assert!(c.stats().tile_merges <= c.tile_count() as u64);
        assert!(occluded > 0, "the scene must cull something");
    }

    // ---- Verification-lane tests (2026-11-02): not part of the delivered module ----

    /// Reversed-depth twin of `occluder_entirely_inside_the_near_plane_draws_nothing`. With
    /// the engine's reversed-Z projection the near plane is `z = w`, not `z = 0`: a quad
    /// between the eye and the near plane is clipped by the GPU and must not occlude here.
    #[test]
    fn reversed_occluder_inside_the_near_plane_draws_nothing() {
        let mut c = MaskedOcclusionCuller::new(256, 128).expect("valid size");
        let (verts, idx) = quad(0.0, 0.0, 4.0, 0.25);
        let report = c
            .draw_occluder(&verts, &idx, proj_reversed(), Backface::Cull)
            .expect("draws");
        assert_eq!(report.rasterised, 0, "{report:?}");
        assert_eq!(report.clipped_away, 2, "{report:?}");
        assert_eq!(c.covered_pixel_count(), 0);
        let behind = c
            .test_aabb(box_at(0.0, 0.0, 10.0, 0.5), proj_reversed())
            .expect("tests");
        assert_eq!(behind, Visibility::Visible);
    }

    /// Reversed-depth twin of `occluder_crossing_the_near_plane_still_occludes`: only the
    /// part beyond the near plane (distance 0.5) may occlude.
    #[test]
    fn reversed_occluder_crossing_the_near_plane_is_cut_at_the_near_plane() {
        let verts = vec![
            Vec3::new(-2.0, -4.0, 1.0),
            Vec3::new(2.0, -4.0, -3.0),
            Vec3::new(2.0, 4.0, -3.0),
            Vec3::new(-2.0, 4.0, 1.0),
        ];
        let idx = vec![0, 1, 2, 0, 2, 3];
        let mut c = MaskedOcclusionCuller::new(256, 128).expect("valid size");
        c.draw_occluder(&verts, &idx, proj_reversed(), Backface::None)
            .expect("crossing quad draws");
        let at = |b: Aabb| c.test_aabb(b, proj_reversed()).expect("tests");
        assert_eq!(at(box_at(0.0, 0.0, 10.0, 0.4)), Visibility::Occluded);
        // Past the near-plane cut edge (NDC x = -0.866): the part of the quad between the
        // eye and the near plane is invisible on the GPU, so this box is visible.
        assert_eq!(at(box_at(-10.5, 0.0, 10.0, 0.4)), Visibility::Visible);
    }

    /// Ground truth by ray casting: per sample ray from the eye, the nearest occluder hit at
    /// view distance `>= near` (Moller & Trumbore 1997 ray/triangle test), so occluders of
    /// any orientation, including ones crossing the near plane or reaching behind the eye,
    /// are handled exactly at the sample positions. Shares no code with the culler.
    struct RayOracle {
        /// Samples along x.
        w: usize,
        /// Samples along y.
        h: usize,
        /// Nearest visible occluder view distance per sample, `INFINITY` where none.
        hit: Vec<f32>,
        /// Projection x scale.
        p00: f32,
        /// Projection y scale.
        p11: f32,
        /// Near-plane distance.
        near: f32,
    }

    impl RayOracle {
        /// Ray direction (view distance 1 along `-z`) through sample `(sx, sy)`.
        fn dir(&self, sx: usize, sy: usize) -> Vec3 {
            let ndc_x = ((sx as f32 + 0.5) / self.w as f32) * 2.0 - 1.0;
            let ndc_y = 1.0 - ((sy as f32 + 0.5) / self.h as f32) * 2.0;
            Vec3::new(ndc_x / self.p00, ndc_y / self.p11, -1.0)
        }

        /// Casts every sample ray against every triangle.
        fn new(w: usize, h: usize, p00: f32, p11: f32, near: f32, tris: &[[Vec3; 3]]) -> Self {
            let mut oracle = Self {
                w,
                h,
                hit: vec![f32::INFINITY; w * h],
                p00,
                p11,
                near,
            };
            for sy in 0..h {
                for sx in 0..w {
                    let d = oracle.dir(sx, sy);
                    let mut best = f32::INFINITY;
                    for tri in tris {
                        let e1 = tri[1] - tri[0];
                        let e2 = tri[2] - tri[0];
                        let p = d.cross(e2);
                        let det = e1.dot(p);
                        if det.abs() < 1e-12 {
                            continue;
                        }
                        let inv = 1.0 / det;
                        let tv = -tri[0];
                        let u = tv.dot(p) * inv;
                        if !(0.0..=1.0).contains(&u) {
                            continue;
                        }
                        let q = tv.cross(e1);
                        let v = d.dot(q) * inv;
                        if v < 0.0 || u + v > 1.0 {
                            continue;
                        }
                        let s = e2.dot(q) * inv;
                        if s >= near && s < best {
                            best = s;
                        }
                    }
                    oracle.hit[sy * w + sx] = best;
                }
            }
            oracle
        }

        /// `Visible` when some sample ray reaches the box (beyond the near plane) strictly
        /// before the nearest occluder on that ray. Boxes must lie beyond the near plane.
        fn test_box(&self, aabb: Aabb) -> Visibility {
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for corner in 0..8u32 {
                let p = Vec3::new(
                    if corner & 1 == 0 { aabb.min.x } else { aabb.max.x },
                    if corner & 2 == 0 { aabb.min.y } else { aabb.max.y },
                    if corner & 4 == 0 { aabb.min.z } else { aabb.max.z },
                );
                let sx = (p.x * self.p00 / -p.z * 0.5 + 0.5) * self.w as f32;
                let sy = (1.0 - (p.y * self.p11 / -p.z * 0.5 + 0.5)) * self.h as f32;
                min_x = min_x.min(sx);
                min_y = min_y.min(sy);
                max_x = max_x.max(sx);
                max_y = max_y.max(sy);
            }
            let x0 = (min_x.floor() as i64 - 1).max(0) as usize;
            let y0 = (min_y.floor() as i64 - 1).max(0) as usize;
            let x1 = (max_x.ceil() as i64 + 1).clamp(0, self.w as i64) as usize;
            let y1 = (max_y.ceil() as i64 + 1).clamp(0, self.h as i64) as usize;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let d = self.dir(sx, sy);
                    let mut s_in = self.near;
                    let mut s_out = f32::INFINITY;
                    let mut miss = false;
                    for axis in 0..3 {
                        let inv = 1.0 / d[axis];
                        let (a, b) = (aabb.min[axis] * inv, aabb.max[axis] * inv);
                        s_in = s_in.max(a.min(b));
                        s_out = s_out.min(a.max(b));
                        if s_out < s_in {
                            miss = true;
                            break;
                        }
                    }
                    if !miss && s_in < self.hit[sy * self.w + sx] * (1.0 - 1e-6) {
                        return Visibility::Visible;
                    }
                }
            }
            Visibility::Occluded
        }
    }

    /// Seeded scene for [`property_arbitrary_occluders_both_depth_modes`]: camera-facing
    /// walls, randomly oriented triangles, and large triangles crossing the near plane or
    /// reaching behind the eye.
    fn arbitrary_scene(rng: &mut Rng, p00: f32, p11: f32) -> Vec<[Vec3; 3]> {
        let mut tris = Vec::new();
        for _ in 0..24 {
            let d = rng.range(3.0, 18.0);
            let (cx, cy) = (rng.range(-0.8, 0.8) * d / p00, rng.range(-0.8, 0.8) * d / p11);
            let s = rng.range(0.1, 0.5) * d / p11;
            let (v, _) = quad(cx, cy, s, d);
            tris.push([v[0], v[1], v[2]]);
            tris.push([v[0], v[2], v[3]]);
        }
        for _ in 0..40 {
            let d = rng.range(2.0, 20.0);
            let c = Vec3::new(rng.range(-0.8, 0.8) * d / p00, rng.range(-0.8, 0.8) * d / p11, -d);
            let r = rng.range(0.2, 0.7) * d;
            let mut corner = || c + Vec3::new(rng.range(-r, r), rng.range(-r, r), rng.range(-r, r));
            tris.push([corner(), corner(), corner()]);
        }
        for _ in 0..8 {
            // One vertex near or behind the eye, two far out: crosses the near plane.
            let a = Vec3::new(rng.range(-0.6, 0.6), rng.range(-0.4, 0.4), rng.range(-0.45, 0.8));
            let d = rng.range(4.0, 14.0);
            let b = Vec3::new(rng.range(-1.0, 1.0) * d / p00, rng.range(-1.0, 0.0) * d / p11, -d);
            let e = Vec3::new(rng.range(-1.0, 1.0) * d / p00, rng.range(0.0, 1.0) * d / p11, -d);
            tris.push([a, b, e]);
        }
        tris
    }

    /// Zero false `Occluded` against the ray-cast oracle for arbitrary occluders (any
    /// orientation, near-plane crossings) in both depth modes, 2000 seeded boxes each.
    /// The oracle samples 4x4 rays per culler pixel; the false-Visible rate is printed.
    #[test]
    fn property_arbitrary_occluders_both_depth_modes() {
        let near = 0.5;
        let cols = proj_standard().to_cols_array();
        let (p00, p11) = (cols[0], cols[5]);
        let mut rng = Rng(0x0DD5_EED5_2026);
        let tris = arbitrary_scene(&mut rng, p00, p11);
        let oracle = RayOracle::new(1024, 512, p00, p11, near, &tris);
        let mut boxes = Vec::new();
        for _ in 0..2000 {
            let d = rng.range(1.5, 25.0);
            let half = rng.range(0.05, 1.0).min(d - near - 0.05);
            boxes.push(box_at(
                rng.range(-0.95, 0.95) * d / p00,
                rng.range(-0.95, 0.95) * d / p11,
                d,
                half,
            ));
        }
        let truth: Vec<Visibility> = boxes.iter().map(|b| oracle.test_box(*b)).collect();
        let truly_hidden = truth.iter().filter(|v| v.is_occluded()).count();
        for (mode, proj) in [
            (DepthMode::Standard, proj_standard()),
            (DepthMode::Reversed, proj_reversed()),
        ] {
            let mut c = MaskedOcclusionCuller::new_with_depth_mode(256, 128, mode).expect("valid size");
            for tri in &tris {
                c.draw_occluder(tri, &[0, 1, 2], proj, Backface::None)
                    .expect("draws");
            }
            let (mut false_occluded, mut culled) = (0usize, 0usize);
            for (aabb, truth) in boxes.iter().zip(&truth) {
                let got = c.test_aabb(*aabb, proj).expect("finite test");
                culled += usize::from(got.is_occluded());
                if got.is_occluded() && *truth == Visibility::Visible {
                    false_occluded += 1;
                }
            }
            eprintln!(
                "{mode:?}: culler Occluded {culled}, oracle Occluded {truly_hidden}, false Occluded {false_occluded}, false Visible {} ({:.1} % of hidden)",
                truly_hidden + false_occluded - culled,
                100.0 * (truly_hidden + false_occluded - culled) as f32 / truly_hidden.max(1) as f32
            );
            assert_eq!(false_occluded, 0, "{mode:?}: false Occluded");
            assert!(truly_hidden > 200, "scene must hide boxes (got {truly_hidden})");
            assert!(culled > 50, "{mode:?}: the culler must cull (got {culled})");
        }
    }

    /// An `n`x`n` grid of quads in the plane `z = -d`, side `2 * s`, centred on the view axis.
    /// With `split`, every quad gets its own four vertices (equal positions, new indices).
    fn grid(n: u32, s: f32, d: f32, split: bool) -> (Vec<Vec3>, Vec<u32>) {
        let at = |i: u32, j: u32| {
            Vec3::new(
                -s + 2.0 * s * i as f32 / n as f32,
                -s + 2.0 * s * j as f32 / n as f32,
                -d,
            )
        };
        let (mut verts, mut idx) = (Vec::new(), Vec::new());
        if split {
            for j in 0..n {
                for i in 0..n {
                    let base = verts.len() as u32;
                    verts.extend([at(i, j), at(i + 1, j), at(i + 1, j + 1), at(i, j + 1)]);
                    idx.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
                }
            }
        } else {
            for j in 0..=n {
                for i in 0..=n {
                    verts.push(at(i, j));
                }
            }
            for j in 0..n {
                for i in 0..n {
                    let v = |i: u32, j: u32| j * (n + 1) + i;
                    idx.extend([
                        v(i, j),
                        v(i + 1, j),
                        v(i + 1, j + 1),
                        v(i, j),
                        v(i + 1, j + 1),
                        v(i, j + 1),
                    ]);
                }
            }
        }
        (verts, idx)
    }

    /// A 16x16-quad grid (512 triangles, 1200+ shared edges) covers every pixel inside it,
    /// indexed or with split vertices (edges match by position), so a box behind it is hidden.
    #[test]
    fn grid_mesh_has_no_cracks_and_split_vertices_still_fuse() {
        let mut single = culler();
        single
            .draw_occluder(&[], &[], proj_standard(), Backface::Cull)
            .expect("empty draw");
        let (verts, idx) = quad(0.0, 0.0, 3.0, 5.0);
        single
            .draw_occluder(&verts, &idx, proj_standard(), Backface::Cull)
            .expect("draws");
        let reference = single.covered_pixel_count();
        for split in [false, true] {
            let mut c = culler();
            let (verts, idx) = grid(16, 3.0, 5.0, split);
            let report = c
                .draw_occluder(&verts, &idx, proj_standard(), Backface::Cull)
                .expect("grid draws");
            assert_eq!(report.rasterised, 512, "split {split}");
            assert_eq!(
                c.covered_pixel_count(),
                reference,
                "split {split}: grid covers what one quad covers"
            );
            assert_eq!(
                test(&c, box_at(0.0, 0.0, 10.0, 1.5)),
                Visibility::Occluded,
                "split {split}"
            );
            assert_eq!(
                test(&c, box_at(0.37, -0.81, 12.0, 0.3)),
                Visibility::Occluded,
                "split {split}"
            );
        }
    }

    /// Triangles of separate calls do not fuse: two abutting half quads drawn in two calls
    /// leave the seam column uncovered (the documented limitation), in one call they fuse.
    #[test]
    fn separate_calls_do_not_fuse_across_a_seam() {
        let left = vec![
            Vec3::new(-3.0, -3.0, -5.0),
            Vec3::new(0.0, -3.0, -5.0),
            Vec3::new(0.0, 3.0, -5.0),
            Vec3::new(-3.0, 3.0, -5.0),
        ];
        let right: Vec<Vec3> = left.iter().map(|v| Vec3::new(v.x + 3.0, v.y, v.z)).collect();
        let idx = [0, 1, 2, 0, 2, 3];
        let seam_box = box_at(0.0, 0.0, 10.0, 0.5);
        let mut two_calls = culler();
        two_calls
            .draw_occluder(&left, &idx, proj_standard(), Backface::Cull)
            .expect("draws");
        two_calls
            .draw_occluder(&right, &idx, proj_standard(), Backface::Cull)
            .expect("draws");
        assert_eq!(test(&two_calls, seam_box), Visibility::Visible);
        let mut one_call = culler();
        let mut both = left.clone();
        both.extend(right.iter().copied());
        one_call
            .draw_occluder(
                &both,
                &[0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7],
                proj_standard(),
                Backface::Cull,
            )
            .expect("draws");
        assert_eq!(test(&one_call, seam_box), Visibility::Occluded);
    }

    /// Two triangles sharing an edge on the SAME side (a fold) do not make the edge interior:
    /// nothing beyond the edge is covered.
    #[test]
    fn folded_shared_edge_stays_a_boundary() {
        // The shared edge sits at x = 0.0767 (screen x ~129.7): pixel column 129 has its centre
        // left of the edge but its square reaches past it, so only a boundary edge excludes it.
        let e = 0.0767;
        let verts = vec![
            Vec3::new(e, -3.0, -5.0),
            Vec3::new(e, 3.0, -5.0),
            Vec3::new(-3.0, 0.0, -5.0),
            Vec3::new(-2.0, 0.5, -5.0),
        ];
        let far = ndc_z_at(proj_standard(), 10.0);
        let column = |x: i32| ScreenRect::new(x, 60, x + 1, 68);
        // Both triangles lie left of the edge (opposite index order, opposite winding): a fold.
        let mut folded = culler();
        folded
            .draw_occluder(&verts, &[0, 1, 2, 1, 0, 3], proj_standard(), Backface::None)
            .expect("draws");
        assert_eq!(folded.test_rect_depth(column(128), far), Visibility::Occluded);
        assert_eq!(folded.test_rect_depth(column(129), far), Visibility::Visible);
        // Control: the same edge between triangles on opposite sides is interior, and the
        // right-hand triangle then covers column 129 fully.
        let mut opposite = verts.clone();
        opposite[3] = Vec3::new(3.0, 0.0, -5.0);
        let mut fused = culler();
        fused
            .draw_occluder(&opposite, &[0, 1, 2, 1, 0, 3], proj_standard(), Backface::None)
            .expect("draws");
        assert_eq!(fused.test_rect_depth(column(129), far), Visibility::Occluded);
    }

    /// Huge, tiny and overflowing coordinates never panic. A screen-covering triangle with
    /// vertices at 1e6 still covers every pixel (f64 clipping keeps it precise); triangles
    /// beyond the supported clip range (1e20, 3e38) are skipped and counted as non-finite.
    #[test]
    fn extreme_coordinates_do_not_panic() {
        let mut c = culler();
        let verts = vec![
            Vec3::new(-1e6, -1e6, -5.0),
            Vec3::new(1e6, -1e6, -5.0),
            Vec3::new(0.0, 1e6, -5.0),
            Vec3::new(-1e20, -1e20, -5.0),
            Vec3::new(1e20, -1e20, -5.0),
            Vec3::new(0.0, 1e20, -5.0),
            Vec3::new(3e38, 3e38, -3e38),
            Vec3::new(1e-38, 0.0, -1e-38),
            Vec3::new(-1e-38, 1e-38, -1e-30),
        ];
        let report = c
            .draw_occluder(
                &verts,
                &[0, 1, 2, 3, 4, 5, 6, 7, 8, 0, 7, 8],
                proj_standard(),
                Backface::None,
            )
            .expect("finite input draws");
        assert_eq!(report.submitted, 4);
        assert_eq!(report.non_finite, 2, "{report:?}");
        assert_eq!(c.covered_pixel_count(), 256 * 128, "{report:?}");
        let huge_box = Aabb::new(Vec3::splat(-1e30), Vec3::splat(1e30));
        assert_eq!(test(&c, huge_box), Visibility::Visible);
        let far_box = box_at(0.0, 0.0, 1e30, 1e28);
        let _ = c.test_aabb(far_box, proj_standard()).expect("finite box tests");
    }

    /// Closed cubes (12 triangles each, one call per cube, back faces culled) against the
    /// ray-cast oracle: zero false `Occluded` in both depth modes, and cube faces fuse.
    #[test]
    fn property_closed_cube_occluders_with_backface_culling() {
        let near = 0.5;
        let cols = proj_standard().to_cols_array();
        let (p00, p11) = (cols[0], cols[5]);
        let mut rng = Rng(0xC0BE_5EED);
        // Cube corner k = (x bit 0, y bit 1, z bit 2); faces wound counter-clockwise from outside.
        let faces: [u32; 36] = [
            0, 2, 3, 0, 3, 1, 4, 5, 7, 4, 7, 6, 0, 1, 5, 0, 5, 4, 2, 6, 7, 2, 7, 3, 0, 4, 6, 0, 6, 2, 1, 3,
            7, 1, 7, 5,
        ];
        let mut cubes = Vec::new();
        for _ in 0..30 {
            let d = rng.range(4.0, 18.0);
            let centre = Vec3::new(rng.range(-0.8, 0.8) * d / p00, rng.range(-0.8, 0.8) * d / p11, -d);
            let half = rng.range(0.08, 0.3) * d;
            let corners: Vec<Vec3> = (0..8u32)
                .map(|k| {
                    centre
                        + Vec3::new(
                            if k & 1 == 0 { -half } else { half },
                            if k & 2 == 0 { -half } else { half },
                            if k & 4 == 0 { -half } else { half },
                        )
                })
                .collect();
            cubes.push(corners);
        }
        let tris: Vec<[Vec3; 3]> = cubes
            .iter()
            .flat_map(|c| {
                faces
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|f| [c[f[0] as usize], c[f[1] as usize], c[f[2] as usize]])
            })
            .collect();
        let oracle = RayOracle::new(1024, 512, p00, p11, near, &tris);
        let mut boxes = Vec::new();
        for _ in 0..2000 {
            let d = rng.range(2.0, 25.0);
            let half = rng.range(0.05, 1.0).min(d - near - 0.05);
            boxes.push(box_at(
                rng.range(-0.95, 0.95) * d / p00,
                rng.range(-0.95, 0.95) * d / p11,
                d,
                half,
            ));
        }
        let truth: Vec<Visibility> = boxes.iter().map(|b| oracle.test_box(*b)).collect();
        let hidden = truth.iter().filter(|v| v.is_occluded()).count();
        for (mode, proj) in [
            (DepthMode::Standard, proj_standard()),
            (DepthMode::Reversed, proj_reversed()),
        ] {
            let mut c = MaskedOcclusionCuller::new_with_depth_mode(256, 128, mode).expect("valid size");
            let mut culled_faces = 0;
            for corners in &cubes {
                let report = c
                    .draw_occluder(corners, &faces, proj, Backface::Cull)
                    .expect("cube draws");
                culled_faces += report.culled_backface;
            }
            let (mut false_occluded, mut culled) = (0usize, 0usize);
            for (aabb, truth) in boxes.iter().zip(&truth) {
                let got = c.test_aabb(*aabb, proj).expect("finite test");
                culled += usize::from(got.is_occluded());
                false_occluded += usize::from(got.is_occluded() && *truth == Visibility::Visible);
            }
            eprintln!(
                "cubes {mode:?}: culler Occluded {culled}, oracle Occluded {hidden}, false Occluded {false_occluded}, back faces culled {culled_faces}"
            );
            assert_eq!(false_occluded, 0, "{mode:?}: false Occluded");
            assert!(culled_faces >= 30 * 6, "{mode:?}: about half the faces face away");
            assert!(
                culled * 2 > hidden,
                "{mode:?}: fused cube faces cull most hidden boxes ({culled} of {hidden})"
            );
        }
    }
}
