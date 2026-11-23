// SPDX-License-Identifier: MPL-2.0
//! Nav tile streaming (ADR-1934, `docs/specs/78-world-streaming.md` § 13.2): plain calls the world
//! streaming runtime makes when a cell comes and goes.
//!
//! A cook builds a whole [`NavTileCache`] and writes each streaming cell's tiles with
//! [`NavTileCache::cook_cell`]; the bytes go in the cell blob. At runtime
//! [`NavTileCache::streamed`] makes an empty cache of the same layout, [`NavTileCache::load_cell`]
//! and [`NavTileCache::unload_cell`] bring tiles in and out, and [`NavTileCache::cell_ready`] is
//! the `nav` gate's answer. A tile stays while any loaded cell holds it.
//!
//! A cooked tile is the tile's walkable cells, not polygons: polygon indices are not stable across
//! rebuilds, and the rebuild path already polygonises one tile from its cells. A region edit
//! ([`NavTileCache::rebuild_region`]) is remembered per tile, because a tile that unloads and
//! returns comes back as cooked: [`NavTileCache::take_replay_regions`] hands the boxes back for
//! re-voxelizing, and the tile's cell is not ready until that ran.
//!
//! Off-mesh links stream with their tiles (ADR-2589, nav cell v2): a link belongs to the tile under
//! its `from` point (a link crossing a tile border belongs to its start tile), is written into that
//! tile's cooked bytes, and joins the mesh when the tile loads and leaves it when the tile unloads.
//! A link whose other end lies on a tile that is not loaded is not a graph edge (its state is
//! `Unresolved` for that end) until that tile loads too.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use regia_core::math::Vec3;

use super::{NavTileCache, NavTileCacheError, NavTileCacheParams};
use crate::areas::{MAX_NAV_AREAS, painted_area};
use crate::config::NavMeshBuildParams;
use crate::heightfield::WalkableGrid;
use crate::links::{MAX_NAV_LINKS, NavLinkKind, NavQueryFilter};
use crate::mesh::{CellRect, OffMeshLink};
use crate::query::NavPath;

/// The format version [`NavTileCache::cook_cell`] writes (`docs/formats/nav-cell.md`). v2
/// (ADR-2589) adds each tile's off-mesh links; v1 cells still load, with no links. v3 (ADR-8899)
/// keeps the v2 byte layout and marks heights from conservative rasterisation (a cell takes every
/// triangle overlapping its square); a v1 or v2 cell still loads as the centre-sampled cell it is,
/// and a re-cook writes v3.
pub const NAV_CELL_FORMAT_VERSION: u32 = 3;

/// The first format version whose heights come from conservative rasterisation (ADR-8899). An
/// older cell loads, and [`NavTileCache::load_cell`] logs a warning naming the re-cook.
pub(crate) const NAV_CELL_CONSERVATIVE_VERSION: u32 = 3;

/// Bytes of one cooked link record (v2).
const LINK_RECORD_LEN: usize = 53;

/// The first bytes of a cooked nav cell.
const NAV_CELL_MAGIC: [u8; 4] = *b"RNVC";

/// Bytes before the first tile: magic, version, layout hash, tile count.
const HEADER_LEN: usize = 20;

/// Bytes of the trailing checksum.
const TRAILER_LEN: usize = 8;

/// The byte layout of a cooked nav cell, field by field (`docs/formats/nav-cell.md`). The cell is
/// written by hand, not through serde, so this list is its shape: `cargo xtask check-formats` hashes
/// it (ADR-2316) and a test checks the header and trailer sizes against the codec.
pub const NAV_CELL_LAYOUT: &[&str] = &[
    "magic:[u8;4]=RNVC",
    "format_version:u32",
    "layout_hash:u64",
    "tile_count:u32",
    "tile.x:u32",
    "tile.z:u32",
    "tile.heights:[f32;cells]",
    "tile.areas:[u8;cells]",
    "tile.link_count:u32",
    "tile.link.from:[f32;3]",
    "tile.link.to:[f32;3]",
    "tile.link.bidirectional:u8",
    "tile.link.area:u8",
    "tile.link.kind:u16=jump,drop,climb,teleport,custom",
    "tile.link.custom:u16",
    "tile.link.flags:u16",
    "tile.link.enabled:u8",
    "tile.link.user_id:u64",
    "tile.link.radius:f32",
    "tile.link.cost_scale:f32",
    "tile.link.duration:f32",
    "checksum:u64=fnv1a",
];

/// The fingerprint `cargo xtask check-formats` hashes for the cooked nav cell: [`NAV_CELL_LAYOUT`].
#[must_use]
pub fn nav_cell_schema_fingerprint() -> String {
    NAV_CELL_LAYOUT.join(",")
}

/// The versions the loader upgrades: a v1 cell (no link section) loads with no links (ADR-2589);
/// a v1 or v2 cell's heights are centre-sampled and load unchanged until a re-cook writes v3
/// (ADR-8899). A newer build refuses a newer cell with `nav_cell_version`; re-cook.
#[must_use]
pub fn nav_cell_migration_sources() -> Vec<u32> {
    vec![1, 2]
}

/// The most cells one streamed grid may hold (the builder's limit).
pub const MAX_STREAMED_GRID_CELLS: usize = 64 * 1024 * 1024;

/// The cell grid of a streamed cache. [`NavTileCache::layout`] reads it from the cooked cache.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavTileLayout {
    /// World position of the grid's minimum corner.
    pub origin: Vec3,
    /// Cells along X.
    pub width: usize,
    /// Cells along Z.
    pub height: usize,
}

/// What one [`NavTileCache::load_cell`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NavCellLoad {
    /// The cell's tiles, `(tile_x, tile_z)`.
    pub tiles: Vec<(u32, u32)>,
    /// Of `tiles`: the ones this call brought in (the others were held by another cell).
    pub added: Vec<(u32, u32)>,
    /// The cell was already loaded; nothing changed.
    pub already_loaded: bool,
}

/// Streaming counters of a cache, for the `nav.stream_status` agent method.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NavTileStreamStatus {
    /// The cache is streamed.
    pub streamed: bool,
    /// Tiles in the grid.
    pub tiles_total: usize,
    /// Tiles holding cells (all of them for a cache built whole).
    pub tiles_loaded: usize,
    /// Streaming cells loaded.
    pub cells_loaded: usize,
    /// Tiles with a region edit to keep across a reload.
    pub edited_tiles: usize,
    /// Tiles waiting for their edit to be replayed.
    pub replay_pending: usize,
}

/// How a [`NavTileCache::find_path_streamed`] query ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavPathOutcome {
    /// The path reaches the end point.
    Complete,
    /// The end is not reachable over the loaded tiles; the path stops at the loaded surface
    /// closest to it, beside an unloaded tile.
    Partial,
    /// No path, and no unloaded tile that could explain it.
    NoPath,
}

impl NavPathOutcome {
    /// The stable status name; [`Self::Partial`] is the spec's `nav_tile_unloaded`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "nav_tile_unloaded",
            Self::NoPath => "no_path",
        }
    }
}

/// The result of [`NavTileCache::find_path_streamed`].
#[derive(Debug, Clone, PartialEq)]
pub struct NavStreamedPath {
    /// How the query ended.
    pub outcome: NavPathOutcome,
    /// The path; `None` only for [`NavPathOutcome::NoPath`].
    pub path: Option<NavPath>,
    /// For [`NavPathOutcome::Partial`], the unloaded tile beside the path's end.
    pub unloaded_tile: Option<(u32, u32)>,
}

/// The streaming side of a [`NavTileCache`].
#[derive(Debug, Clone)]
pub(super) struct StreamState {
    /// Layout and build-parameter hash a cooked cell must match.
    layout_hash: u64,
    /// Loaded tile → how many loaded cells hold it.
    held: BTreeMap<u32, u32>,
    /// Loaded cell → its tiles.
    cells: BTreeMap<u64, Vec<u32>>,
    /// Tile → the union of the region edits touching it.
    edits: BTreeMap<u32, (Vec3, Vec3)>,
    /// Tile → the edit to replay after a reload.
    replay: BTreeMap<u32, (Vec3, Vec3)>,
    /// Tiles of `replay` already handed to the caller.
    handed_out: BTreeSet<u32>,
    /// Tiles loaded and not yet polygonised.
    arriving: BTreeSet<u32>,
    /// Sum of [`NavTileCache::shift_origin`] calls (cooked heights are in the unshifted frame).
    shift: Vec3,
    /// Loaded tile → the off-mesh links it brought (ADR-2589), in cooked order.
    pub(super) tile_links: BTreeMap<u32, Vec<OffMeshLink>>,
}

/// FNV-1a over `bytes`, continuing from `hash`.
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The FNV-1a offset basis.
const FNV_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// The hash a cooked cell carries: everything that decides what its cells mean, except the
/// origin, which a floating-origin rebase moves.
fn layout_hash(params: &NavTileCacheParams, width: usize, height: usize) -> u64 {
    let NavMeshBuildParams {
        cell_size,
        cell_height,
        border_size,
        ref agent,
    } = params.build;
    let mut hash = FNV_BASIS;
    for v in [
        cell_size,
        cell_height,
        border_size,
        agent.radius,
        agent.height,
        agent.max_climb,
        agent.max_slope,
    ] {
        hash = fnv1a(hash, &v.to_bits().to_le_bytes());
    }
    for v in [params.tile_size as u64, width as u64, height as u64] {
        hash = fnv1a(hash, &v.to_le_bytes());
    }
    hash
}

/// Little-endian reader over a checked slice.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], NavTileCacheError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&e| e <= self.bytes.len())
            .ok_or(NavTileCacheError::CorruptCell("truncated"))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u32(&mut self) -> Result<u32, NavTileCacheError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, NavTileCacheError> {
        let lo = self.u32()?;
        let hi = self.u32()?;
        Ok(u64::from(hi) << 32 | u64::from(lo))
    }

    fn u16(&mut self) -> Result<u16, NavTileCacheError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u8(&mut self) -> Result<u8, NavTileCacheError> {
        Ok(self.take(1)?[0])
    }

    fn f32(&mut self) -> Result<f32, NavTileCacheError> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn vec3(&mut self) -> Result<Vec3, NavTileCacheError> {
        Ok(Vec3::new(self.f32()?, self.f32()?, self.f32()?))
    }

    /// One v2 link record, moved by `shift` into the cache's frame.
    fn link(&mut self, shift: Vec3) -> Result<OffMeshLink, NavTileCacheError> {
        let from = self.vec3()? + shift;
        let to = self.vec3()? + shift;
        let bidirectional = match self.u8()? {
            0 => false,
            1 => true,
            _ => return Err(NavTileCacheError::CorruptCell("bad link flag")),
        };
        let area = self.u8()?;
        let kind = self.u16()?;
        let custom = self.u16()?;
        let kind = match kind {
            0 => NavLinkKind::Jump,
            1 => NavLinkKind::Drop,
            2 => NavLinkKind::Climb,
            3 => NavLinkKind::Teleport,
            4 => NavLinkKind::Custom(custom),
            _ => return Err(NavTileCacheError::CorruptCell("unknown link kind")),
        };
        let flags = self.u16()?;
        let enabled = match self.u8()? {
            0 => false,
            1 => true,
            _ => return Err(NavTileCacheError::CorruptCell("bad link flag")),
        };
        let user_id = self.u64()?;
        let link = OffMeshLink {
            from,
            to,
            bidirectional,
            area,
            user_id,
            kind,
            radius: self.f32()?,
            cost_scale: self.f32()?,
            flags,
            enabled,
            duration: self.f32()?,
        };
        if (link.area as usize) >= MAX_NAV_AREAS || link.validate().is_err() {
            return Err(NavTileCacheError::CorruptCell("invalid link"));
        }
        Ok(link)
    }
}

/// Appends one v2 link record, moved by `-shift` into the cook's frame.
fn write_link(out: &mut Vec<u8>, link: &OffMeshLink, shift: Vec3) {
    for v in [link.from - shift, link.to - shift] {
        for c in [v.x, v.y, v.z] {
            out.extend_from_slice(&c.to_bits().to_le_bytes());
        }
    }
    out.push(u8::from(link.bidirectional));
    out.push(link.area);
    let (kind, custom): (u16, u16) = match link.kind {
        NavLinkKind::Jump => (0, 0),
        NavLinkKind::Drop => (1, 0),
        NavLinkKind::Climb => (2, 0),
        NavLinkKind::Teleport => (3, 0),
        NavLinkKind::Custom(c) => (4, c),
    };
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&custom.to_le_bytes());
    out.extend_from_slice(&link.flags.to_le_bytes());
    out.push(u8::from(link.enabled));
    out.extend_from_slice(&link.user_id.to_le_bytes());
    for f in [link.radius, link.cost_scale, link.duration] {
        out.extend_from_slice(&f.to_bits().to_le_bytes());
    }
}

/// One tile of a decoded cell.
struct CookedTile {
    tile: u32,
    cells: Vec<Option<f32>>,
    areas: Vec<u8>,
    links: Vec<OffMeshLink>,
}

/// The union of two boxes.
fn union(a: (Vec3, Vec3), b: (Vec3, Vec3)) -> (Vec3, Vec3) {
    (a.0.min(b.0), a.1.max(b.1))
}

impl NavTileCache {
    /// An empty cache of `layout`, for streaming: no tile is loaded and the mesh has no polygons
    /// until [`Self::load_cell`] and [`Self::update`]. Errors: a bad tile size, a layout with
    /// no cells, too many, or a non-finite origin, or agent parameters the builder refuses
    /// ([`NavTileCacheError::Build`]).
    pub fn streamed(params: NavTileCacheParams, layout: NavTileLayout) -> Result<Self, NavTileCacheError> {
        let _span = tracing::info_span!("nav_tile_cache_streamed").entered();
        if params.tile_size == 0 || params.tile_size > super::MAX_TILE_SIZE {
            return Err(NavTileCacheError::InvalidTileSize(params.tile_size));
        }
        if !layout.origin.is_finite() {
            return Err(NavTileCacheError::InvalidLayout("origin must be finite"));
        }
        if !(params.build.cell_size.is_finite() && params.build.cell_size > 0.0) {
            return Err(NavTileCacheError::InvalidLayout(
                "cell_size must be finite and positive",
            ));
        }
        // The agent decides every tile a cell loads into (erosion, climb, headroom).
        crate::builder::validate_agent(&params.build.agent)?;
        let cells = layout
            .width
            .checked_mul(layout.height)
            .filter(|&n| n > 0 && n <= MAX_STREAMED_GRID_CELLS)
            .ok_or(NavTileCacheError::InvalidLayout(
                "the grid must hold 1 to 64 Mi cells",
            ))?;
        let grid = WalkableGrid {
            origin: layout.origin,
            cell_size: params.build.cell_size,
            width: layout.width,
            height: layout.height,
            cells: vec![None; cells],
            areas: Vec::new(),
        };
        let hash = layout_hash(&params, layout.width, layout.height);
        let mut cache = Self::with_grid(params, grid);
        cache.stream = Some(StreamState {
            layout_hash: hash,
            held: BTreeMap::new(),
            cells: BTreeMap::new(),
            edits: BTreeMap::new(),
            replay: BTreeMap::new(),
            handed_out: BTreeSet::new(),
            arriving: BTreeSet::new(),
            shift: Vec3::ZERO,
            tile_links: BTreeMap::new(),
        });
        for t in 0..cache.tiles.len() {
            cache.rebuild_tile(t as u32);
        }
        cache.stitch();
        Ok(cache)
    }

    /// The grid layout, for [`Self::streamed`].
    pub fn layout(&self) -> NavTileLayout {
        NavTileLayout {
            origin: self.base.origin,
            width: self.base.width,
            height: self.base.height,
        }
    }

    /// Whether the cache was made by [`Self::streamed`].
    pub fn is_streamed(&self) -> bool {
        self.stream.is_some()
    }

    pub(super) fn tile_is_loaded(&self, t: u32) -> bool {
        self.stream.as_ref().is_none_or(|s| s.held.contains_key(&t))
    }

    /// Whether tile `(tx, tz)` holds cells (always, for a cache built whole), or `None` out of
    /// range.
    pub fn tile_loaded(&self, tx: u32, tz: u32) -> Option<bool> {
        Some(self.tile_is_loaded(self.tile_index(tx, tz)? as u32))
    }

    /// Whether tile `(tx, tz)` is in the mesh with its region edits replayed: what a crowd agent on
    /// it waits for. A rebuild queued on a loaded tile does not count against it. `false` out of
    /// range; always true for a cache built whole.
    pub fn tile_ready(&self, tx: u32, tz: u32) -> bool {
        let Some(t) = self.tile_index(tx, tz).map(|t| t as u32) else {
            return false;
        };
        self.tile_is_loaded(t)
            && self
                .stream
                .as_ref()
                .is_none_or(|s| !s.arriving.contains(&t) && !s.replay.contains_key(&t))
    }

    /// A rebuilt tile is in the mesh.
    pub(super) fn note_tile_rebuilt(&mut self, t: u32) {
        if let Some(stream) = &mut self.stream {
            stream.arriving.remove(&t);
        }
    }

    /// The tiles the XZ box `min..max` touches, `(tile_x, tile_z)` in tile order: the tiles of a
    /// streaming cell, for [`Self::cook_cell`]. Empty when the box misses the grid or is not
    /// finite.
    pub fn tiles_in(&self, min: Vec3, max: Vec3) -> Vec<(u32, u32)> {
        if !min.is_finite() || !max.is_finite() {
            return Vec::new();
        }
        let (lo, hi) = (min.min(max), min.max(max));
        let g = &self.base;
        let (Some((x0, x1)), Some((z0, z1))) = (
            Self::cell_span(lo.x, hi.x, g.origin.x, g.cell_size, g.width),
            Self::cell_span(lo.z, hi.z, g.origin.z, g.cell_size, g.height),
        ) else {
            return Vec::new();
        };
        let ts = self.params.tile_size as usize;
        let mut out = Vec::new();
        for tz in z0 / ts..=z1 / ts {
            for tx in x0 / ts..=x1 / ts {
                out.push((tx as u32, tz as u32));
            }
        }
        out
    }

    /// Writes `tiles` as one cooked cell (`docs/formats/nav-cell.md`): the stored cells, without
    /// obstacles. Output is deterministic and independent of the order of `tiles`. Errors: a tile
    /// outside the grid, or not loaded.
    pub fn cook_cell(&self, tiles: &[(u32, u32)]) -> Result<Vec<u8>, NavTileCacheError> {
        let _span = tracing::info_span!("nav_tile_cache_cook_cell", tiles = tiles.len()).entered();
        let mut indices = BTreeSet::new();
        for &(tx, tz) in tiles {
            let t = self
                .tile_index(tx, tz)
                .ok_or(NavTileCacheError::TileOutOfRange(tx, tz))? as u32;
            if !self.tile_is_loaded(t) {
                return Err(NavTileCacheError::InvalidLayout(
                    "cannot cook a tile that is not loaded",
                ));
            }
            indices.insert(t);
        }
        let hash = layout_hash(&self.params, self.base.width, self.base.height);
        let shift = self.stream.as_ref().map_or(Vec3::ZERO, |s| s.shift);
        let shift_y = shift.y;
        let climb = self.params.build.agent.max_climb;
        let mut out = Vec::new();
        out.extend_from_slice(&NAV_CELL_MAGIC);
        out.extend_from_slice(&NAV_CELL_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&hash.to_le_bytes());
        out.extend_from_slice(&(indices.len() as u32).to_le_bytes());
        for &t in &indices {
            let (tx, tz) = self.tile_xz(t);
            let rect = self.tiles[t as usize].rect;
            out.extend_from_slice(&tx.to_le_bytes());
            out.extend_from_slice(&tz.to_le_bytes());
            for z in rect.z0..rect.z1 {
                for x in rect.x0..rect.x1 {
                    let cell = self.base.cells[z * self.base.width + x];
                    let v = cell.map_or(f32::NAN, |y| y - shift_y);
                    out.extend_from_slice(&v.to_bits().to_le_bytes());
                }
            }
            // The areas as painted now: a cooked cell needs no volumes to come back painted.
            let painting = self.volumes_on(t);
            for z in rect.z0..rect.z1 {
                for x in rect.x0..rect.x1 {
                    let i = z * self.base.width + x;
                    let base = self.base.area_at(i);
                    let area = self.base.cells[i].map_or(base, |y| {
                        painted_area(&self.base, &painting, (x, z, y), climb, base)
                    });
                    out.push(area);
                }
            }
            // The links this tile owns: the cache's own whose `from` lies on it, then the ones
            // it streamed in.
            let owned: Vec<&OffMeshLink> = self
                .off_mesh_links
                .iter()
                .filter(|l| self.link_owner(l) == Some(t))
                .chain(
                    self.stream
                        .as_ref()
                        .and_then(|s| s.tile_links.get(&t))
                        .into_iter()
                        .flatten(),
                )
                .collect();
            out.extend_from_slice(&(owned.len() as u32).to_le_bytes());
            for link in owned {
                write_link(&mut out, link, shift);
            }
        }
        let sum = fnv1a(FNV_BASIS, &out);
        out.extend_from_slice(&sum.to_le_bytes());
        Ok(out)
    }

    /// Decodes a cooked cell; nothing is applied until all of it is checked.
    fn decode_cell(&self, blob: &[u8]) -> Result<Vec<CookedTile>, NavTileCacheError> {
        let hash = self
            .stream
            .as_ref()
            .ok_or(NavTileCacheError::NotStreamed)?
            .layout_hash;
        if blob.len() < 8 || blob[..4] != NAV_CELL_MAGIC {
            return Err(NavTileCacheError::CorruptCell("not a nav cell"));
        }
        let mut r = Reader { bytes: blob, at: 4 };
        let version = r.u32()?;
        if version == 0 {
            return Err(NavTileCacheError::CorruptCell("version 0"));
        }
        if version > NAV_CELL_FORMAT_VERSION {
            return Err(NavTileCacheError::NewerCellVersion {
                found: version,
                supported: NAV_CELL_FORMAT_VERSION,
            });
        }
        if blob.len() < HEADER_LEN + TRAILER_LEN {
            return Err(NavTileCacheError::CorruptCell("truncated"));
        }
        if version < NAV_CELL_CONSERVATIVE_VERSION {
            // Readable as it is (the layout did not change), but its heights are centre-sampled:
            // a runtime region rebuild next to it rasterises conservatively, so the two can
            // disagree at the seam until the content is re-cooked (ADR-8899).
            tracing::warn!(
                version,
                current = NAV_CELL_FORMAT_VERSION,
                "nav cell v{version} was cooked with centre-sampled heights; re-cook to write v{NAV_CELL_FORMAT_VERSION}"
            );
        }
        let body = blob.len() - TRAILER_LEN;
        let mut tail = Reader {
            bytes: blob,
            at: body,
        };
        if tail.u64()? != fnv1a(FNV_BASIS, &blob[..body]) {
            return Err(NavTileCacheError::CorruptCell("checksum mismatch"));
        }
        if r.u64()? != hash {
            return Err(NavTileCacheError::LayoutMismatch);
        }
        let count = r.u32()?;
        let mut r = Reader {
            bytes: &blob[..body],
            at: r.at,
        };
        let shift = self.stream.as_ref().map_or(Vec3::ZERO, |s| s.shift);
        let mut out: Vec<CookedTile> = Vec::new();
        for _ in 0..count {
            let (tx, tz) = (r.u32()?, r.u32()?);
            let t = self
                .tile_index(tx, tz)
                .ok_or(NavTileCacheError::CorruptCell("tile outside the grid"))? as u32;
            if out.iter().any(|seen| seen.tile == t) {
                return Err(NavTileCacheError::CorruptCell("tile listed twice"));
            }
            let rect = self.tiles[t as usize].rect;
            let n = rect.width() * (rect.z1 - rect.z0);
            let bytes = r.take(n.saturating_mul(4))?;
            let mut cells = Vec::with_capacity(n);
            for (b, _) in bytes.as_chunks::<4>().0.iter().zip(0..n) {
                let v = f32::from_le_bytes(*b);
                if v.is_infinite() {
                    return Err(NavTileCacheError::CorruptCell("infinite height"));
                }
                cells.push((!v.is_nan()).then_some(v));
            }
            let areas = r.take(n)?.to_vec();
            if areas.iter().any(|&a| a as usize >= MAX_NAV_AREAS) {
                return Err(NavTileCacheError::CorruptCell("area id out of range"));
            }
            // v1 has no link section: it loads as a cell with no links.
            let mut links = Vec::new();
            if version >= 2 {
                let n = r.u32()? as usize;
                let left = r.bytes.len() - r.at;
                if n > MAX_NAV_LINKS || n.saturating_mul(LINK_RECORD_LEN) > left {
                    return Err(NavTileCacheError::CorruptCell("truncated"));
                }
                links.reserve(n);
                for _ in 0..n {
                    let link = r.link(shift)?;
                    links.push(link);
                }
            }
            out.push(CookedTile {
                tile: t,
                cells,
                areas,
                links,
            });
        }
        if r.at != body {
            return Err(NavTileCacheError::CorruptCell("trailing bytes"));
        }
        Ok(out)
    }

    /// Loads the cooked `blob` of streaming cell `cell` (the `CellLoaded` hook of spec 78 § 13.2).
    /// New tiles are queued; the mesh changes on the next [`Self::update`], and
    /// [`Self::cell_ready`] answers the `nav` gate. Loading a loaded cell changes nothing.
    /// Errors: not a streamed cache, a damaged or foreign blob (nothing changes).
    pub fn load_cell(&mut self, cell: u64, blob: &[u8]) -> Result<NavCellLoad, NavTileCacheError> {
        let _span = tracing::info_span!("nav_tile_cache_load_cell", cell, bytes = blob.len()).entered();
        let decoded = self.decode_cell(blob)?;
        let tiles: Vec<(u32, u32)> = decoded.iter().map(|d| self.tile_xz(d.tile)).collect();
        let stream = self.stream.as_mut().ok_or(NavTileCacheError::NotStreamed)?;
        if stream.cells.contains_key(&cell) {
            return Ok(NavCellLoad {
                tiles,
                added: Vec::new(),
                already_loaded: true,
            });
        }
        let shift_y = stream.shift.y;
        let mut added = Vec::new();
        for cooked in &decoded {
            let t = cooked.tile;
            let count = stream.held.entry(t).or_insert(0);
            *count += 1;
            if *count > 1 {
                continue;
            }
            if let Some(&edit) = stream.edits.get(&t) {
                stream.replay.insert(t, edit);
                stream.handed_out.remove(&t);
            }
            let rect = self.tiles[t as usize].rect;
            let width = self.base.width;
            let mut heights = cooked.cells.iter();
            let mut areas = cooked.areas.iter();
            for z in rect.z0..rect.z1 {
                for x in rect.x0..rect.x1 {
                    let i = z * width + x;
                    self.base.cells[i] = heights.next().copied().flatten().map(|y| y + shift_y);
                    self.base.areas[i] = areas.next().copied().unwrap_or(0);
                }
            }
            if cooked.links.is_empty() {
                stream.tile_links.remove(&t);
            } else {
                stream.tile_links.insert(t, cooked.links.clone());
            }
            stream.arriving.insert(t);
            self.pending.insert(t);
            added.push((t % self.tiles_x, t / self.tiles_x));
        }
        stream
            .cells
            .insert(cell, decoded.iter().map(|d| d.tile).collect());
        Ok(NavCellLoad {
            tiles,
            added,
            already_loaded: false,
        })
    }

    /// Unloads streaming cell `cell` (the `CellUnloading` hook of spec 78 § 13.2): tiles no other
    /// loaded cell holds lose their cells and are queued, so they leave the mesh on the next
    /// [`Self::update`]. Obstacles stay registered and apply again when the tile returns. Returns
    /// the tiles removed. Errors: not a streamed cache, or `cell` is not loaded.
    pub fn unload_cell(&mut self, cell: u64) -> Result<Vec<(u32, u32)>, NavTileCacheError> {
        let _span = tracing::info_span!("nav_tile_cache_unload_cell", cell).entered();
        let stream = self.stream.as_mut().ok_or(NavTileCacheError::NotStreamed)?;
        let tiles = stream
            .cells
            .remove(&cell)
            .ok_or(NavTileCacheError::UnknownCell(cell))?;
        let mut removed = Vec::new();
        for t in tiles {
            let Some(count) = stream.held.get_mut(&t) else {
                continue;
            };
            *count -= 1;
            if *count > 0 {
                continue;
            }
            stream.held.remove(&t);
            stream.replay.remove(&t);
            stream.handed_out.remove(&t);
            stream.arriving.remove(&t);
            stream.tile_links.remove(&t);
            let rect = self.tiles[t as usize].rect;
            let width = self.base.width;
            for z in rect.z0..rect.z1 {
                self.base.cells[z * width + rect.x0..z * width + rect.x1].fill(None);
                self.base.areas[z * width + rect.x0..z * width + rect.x1].fill(0);
            }
            self.pending.insert(t);
            removed.push((t % self.tiles_x, t / self.tiles_x));
        }
        Ok(removed)
    }

    /// The `nav` gate of spec 78 § 10.3: every tile of `cell` is in the mesh, with its region edits
    /// replayed. `false` for a cell that is not loaded.
    pub fn cell_ready(&self, cell: u64) -> bool {
        let Some(stream) = &self.stream else { return false };
        let Some(tiles) = stream.cells.get(&cell) else {
            return false;
        };
        tiles
            .iter()
            .all(|t| !stream.arriving.contains(t) && !stream.replay.contains_key(t))
    }

    /// The loaded streaming cells, in id order.
    pub fn loaded_cells(&self) -> Vec<u64> {
        self.stream
            .as_ref()
            .map_or_else(Vec::new, |s| s.cells.keys().copied().collect())
    }

    /// Hands out each region edit a reloaded tile has to replay, once: feed the boxes to
    /// [`Self::rebuild_region`] (`crate::NavRegionQueue` does). The tile's cell stays not ready
    /// until a rebuild covers its box.
    pub fn take_replay_regions(&mut self) -> Vec<(Vec3, Vec3)> {
        let Some(stream) = &mut self.stream else {
            return Vec::new();
        };
        let mut out: Vec<(Vec3, Vec3)> = Vec::new();
        let waiting: Vec<u32> = stream
            .replay
            .keys()
            .filter(|t| !stream.handed_out.contains(t))
            .copied()
            .collect();
        for t in waiting {
            stream.handed_out.insert(t);
            if let Some(&edit) = stream.replay.get(&t)
                && !out.contains(&edit)
            {
                out.push(edit);
            }
        }
        out
    }

    /// Gives up the replays of box `lo..hi`: its rebuild was refused, and a gate shut for good would
    /// stall the level. The tiles stay as cooked until a later reload replays the edit again.
    pub fn abandon_replay(&mut self, lo: Vec3, hi: Vec3) {
        let Some(stream) = &mut self.stream else { return };
        let handed = &mut stream.handed_out;
        stream.replay.retain(|t, edit| {
            let keep = !(lo.x <= edit.0.x && lo.z <= edit.0.z && hi.x >= edit.1.x && hi.z >= edit.1.z);
            if !keep {
                handed.remove(t);
            }
            keep
        });
    }

    /// Records a rebuilt box for the tiles under `apply`: the edit outlives an unload, and a replay
    /// the box covers is done.
    pub(super) fn note_region_applied(&mut self, lo: Vec3, hi: Vec3, apply: CellRect, ts: usize) {
        let tiles_x = self.tiles_x as usize;
        let Some(stream) = &mut self.stream else { return };
        for tz in apply.z0 / ts..=(apply.z1 - 1) / ts {
            for tx in apply.x0 / ts..=(apply.x1 - 1) / ts {
                let t = (tz * tiles_x + tx) as u32;
                let edit = stream.edits.get(&t).map_or((lo, hi), |&e| union(e, (lo, hi)));
                stream.edits.insert(t, edit);
                if stream.held.contains_key(&t)
                    && let Some(&(a, b)) = stream.replay.get(&t)
                    && lo.x <= a.x
                    && lo.z <= a.z
                    && hi.x >= b.x
                    && hi.z >= b.z
                {
                    stream.replay.remove(&t);
                    stream.handed_out.remove(&t);
                }
            }
        }
    }

    /// The streaming counters.
    pub fn stream_status(&self) -> NavTileStreamStatus {
        match &self.stream {
            Some(s) => NavTileStreamStatus {
                streamed: true,
                tiles_total: self.tiles.len(),
                tiles_loaded: s.held.len(),
                cells_loaded: s.cells.len(),
                edited_tiles: s.edits.len(),
                replay_pending: s.replay.len(),
            },
            None => NavTileStreamStatus {
                streamed: false,
                tiles_total: self.tiles.len(),
                tiles_loaded: self.tiles.len(),
                ..Default::default()
            },
        }
    }

    /// Moves the whole cache by `delta` (the floating-origin step of spec 78 § 12.2): grids, tiles,
    /// obstacles, area volumes, links and the region edits; later cells load shifted. Polygons are
    /// rebuilt, so [`Self::revision`] moves. Anything else holding world positions moves with its own
    /// `shift_origin`: [`crate::NavCrowd`], [`crate::NavLaneGraph`], [`crate::NavRegionQueue`].
    pub fn shift_origin(&mut self, delta: Vec3) {
        let _span = tracing::info_span!("nav_tile_cache_shift_origin").entered();
        if !delta.is_finite() || delta == Vec3::ZERO {
            return;
        }
        for grid in [&mut self.base, &mut self.work] {
            grid.origin += delta;
            for y in grid.cells.iter_mut().flatten() {
                *y += delta.y;
            }
        }
        for entry in self.obstacles.values_mut() {
            entry.shape = entry.shape.translated(delta);
        }
        for entry in self.area_volumes.values_mut() {
            entry.volume.shape = entry.volume.shape.translated(delta);
        }
        for link in &mut self.off_mesh_links {
            link.from += delta;
            link.to += delta;
        }
        if let Some(stream) = &mut self.stream {
            stream.shift += delta;
            for edit in stream.edits.values_mut().chain(stream.replay.values_mut()) {
                *edit = (edit.0 + delta, edit.1 + delta);
            }
            for link in stream.tile_links.values_mut().flatten() {
                link.from += delta;
                link.to += delta;
            }
            // A box handed out is in the old frame: hand it out again in the new one.
            stream.handed_out.clear();
            stream.arriving.clear();
        }
        for t in 0..self.tiles.len() {
            self.rebuild_tile(t as u32);
        }
        self.stitch();
        self.revision += 1;
        // Every tile was rebuilt and restitched: a patch still waiting describes the old numbering,
        // so the next crowd sync must replan everything instead of applying it.
        self.pending_revision = None;
    }

    /// [`crate::NavQuery::find_path`] for a streamed world: when the end is not reachable over the
    /// loaded tiles and the reachable surface borders an unloaded tile, the path stops at the
    /// point of that border closest to the end ([`NavPathOutcome::Partial`], the spec's
    /// `nav_tile_unloaded`). An end off the tile grid counts as over the grid's tile nearest to
    /// it: a path that snapped onto the loaded surface is [`NavPathOutcome::Partial`] when that
    /// tile is not loaded.
    pub fn find_path_streamed(&self, start: Vec3, end: Vec3, extents: Vec3) -> NavStreamedPath {
        self.find_path_streamed_filtered(start, end, extents, &NavQueryFilter::default())
    }

    /// [`Self::find_path_streamed`] with `filter`'s links, area costs and left-out areas. The search
    /// for the border crosses the polygons and the off-mesh links the filter allows.
    pub fn find_path_streamed_filtered(
        &self,
        start: Vec3,
        end: Vec3,
        extents: Vec3,
        filter: &NavQueryFilter,
    ) -> NavStreamedPath {
        let _span = tracing::info_span!("nav_tile_cache_find_path_streamed").entered();
        let query = self.query();
        if let Some(path) = query.find_path_filtered(start, end, extents, filter) {
            // An end snapped onto the loaded surface from an unloaded tile is not reached. An end
            // off the grid counts as over the grid's tile nearest to it: when that tile is not
            // loaded, the surface the end snapped onto may not be the one closest to it.
            let missing = self
                .tile_at(end)
                .or_else(|| self.tile_nearest_off_grid(end))
                .filter(|&(tx, tz)| self.tile_loaded(tx, tz) == Some(false));
            return NavStreamedPath {
                outcome: if missing.is_some() {
                    NavPathOutcome::Partial
                } else {
                    NavPathOutcome::Complete
                },
                path: Some(path),
                unloaded_tile: missing,
            };
        }
        let none = NavStreamedPath {
            outcome: NavPathOutcome::NoPath,
            path: None,
            unloaded_tile: None,
        };
        if !end.is_finite() || self.stream_status().tiles_loaded == self.tiles.len() {
            return none;
        }
        let Some((from, _)) = query.find_nearest_filtered(start, extents, filter) else {
            return none;
        };
        let polys = &self.mesh.polys;
        let links = self.mesh.link_table();
        let mut seen = vec![false; polys.len()];
        seen[from as usize] = true;
        let mut open = VecDeque::from([from]);
        let mut best: Option<(f32, Vec3, (u32, u32))> = None;
        while let Some(p) = open.pop_front() {
            let poly = &polys[p as usize];
            if let Some(tile) = self.unloaded_tile_beside(poly.min, poly.max, end) {
                let point = poly.closest_point_xz(end.x, end.z);
                let (dx, dz) = (point.x - end.x, point.z - end.z);
                let d = dx * dx + dz * dz;
                if best.is_none_or(|(b, ..)| d < b) {
                    best = Some((d, point, tile));
                }
            }
            let walks = poly.portals.iter().map(|portal| portal.neighbor);
            let jumps = links
                .edges_from(p)
                .iter()
                .filter(|e| filter.passes_flags(e.flags) && filter.allows_area(e.area))
                .map(|e| e.to_poly);
            for next in walks.chain(jumps) {
                let n = next as usize;
                if !seen[n] && filter.allows_area(polys[n].area) {
                    seen[n] = true;
                    open.push_back(next);
                }
            }
        }
        let Some((_, point, tile)) = best else {
            return none;
        };
        match query.find_path_filtered(start, point, extents, filter) {
            Some(path) => NavStreamedPath {
                outcome: NavPathOutcome::Partial,
                path: Some(path),
                unloaded_tile: Some(tile),
            },
            None => none,
        }
    }

    /// For a finite point off the grid, the tile at the grid's edge nearest to it in XZ (the point
    /// clamped into the grid); `None` on the grid, for a non-finite point or an empty grid.
    fn tile_nearest_off_grid(&self, point: Vec3) -> Option<(u32, u32)> {
        if !point.is_finite() || self.base.width == 0 || self.base.height == 0 {
            return None;
        }
        let c = self.base.cell_size;
        let o = self.base.origin;
        // Clamped as integers, as `tile_at` compares them (`width as f32` rounds above 2^24).
        let cell = |v: f32, origin: f32, n: usize| -> (usize, bool) {
            let f = ((v - origin) / c).floor();
            if f < 0.0 {
                (0, true)
            } else if (f as usize) >= n {
                (n - 1, true)
            } else {
                (f as usize, false)
            }
        };
        let (x, off_x) = cell(point.x, o.x, self.base.width);
        let (z, off_z) = cell(point.z, o.z, self.base.height);
        if !(off_x || off_z) {
            return None;
        }
        let ts = self.params.tile_size as usize;
        Some(((x / ts) as u32, (z / ts) as u32))
    }

    /// Of the tiles that are not loaded among those the XZ box, grown by one cell, touches: the one
    /// whose centre is closest to `end`, the lowest tile on a tie.
    fn unloaded_tile_beside(&self, min: Vec3, max: Vec3, end: Vec3) -> Option<(u32, u32)> {
        let c = self.base.cell_size;
        let o = self.base.origin;
        let ts = i64::from(self.params.tile_size);
        let tile_of = |v: f32, origin: f32| ((v - origin) / c).floor() as i64;
        let (x0, x1) = (
            tile_of(min.x - c * 0.5, o.x) / ts,
            tile_of(max.x + c * 0.5, o.x) / ts,
        );
        let (z0, z1) = (
            tile_of(min.z - c * 0.5, o.z) / ts,
            tile_of(max.z + c * 0.5, o.z) / ts,
        );
        let side = c * self.params.tile_size as f32;
        let mut best: Option<(f32, (u32, u32))> = None;
        for tz in z0.max(0)..=z1.min(i64::from(self.tiles_z) - 1) {
            for tx in x0.max(0)..=x1.min(i64::from(self.tiles_x) - 1) {
                let (tx, tz) = (tx as u32, tz as u32);
                if self
                    .tile_index(tx, tz)
                    .is_some_and(|t| !self.tile_is_loaded(t as u32))
                {
                    let cx = o.x + (tx as f32 + 0.5) * side - end.x;
                    let cz = o.z + (tz as f32 + 0.5) * side - end.z;
                    let d = cx * cx + cz * cz;
                    if best.is_none_or(|(b, _)| d < b) {
                        best = Some((d, (tx, tz)));
                    }
                }
            }
        }
        best.map(|(_, tile)| tile)
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// The byte size of a fixed-size layout field (`name:u32`, `name:[u8;4]=...`); `None` for the
    /// per-cell arrays.
    fn size(field: &str) -> Option<usize> {
        let ty = field.split_once(':')?.1;
        let ty = ty.split_once('=').map_or(ty, |(t, _)| t);
        match ty {
            "u32" => Some(4),
            "u64" => Some(8),
            "[u8;4]" => Some(4),
            _ => None,
        }
    }

    #[test]
    fn the_layout_matches_the_codec_header_and_trailer() {
        let header: usize = NAV_CELL_LAYOUT[..4].iter().map(|f| size(f).expect("fixed")).sum();
        assert_eq!(header, HEADER_LEN);
        assert_eq!(
            size(NAV_CELL_LAYOUT[NAV_CELL_LAYOUT.len() - 1]),
            Some(TRAILER_LEN)
        );
        assert!(NAV_CELL_LAYOUT[0].ends_with(std::str::from_utf8(&NAV_CELL_MAGIC).expect("ascii")));
        assert!(nav_cell_schema_fingerprint().contains("tile.areas"));
    }
}
