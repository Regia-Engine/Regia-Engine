// SPDX-License-Identifier: MPL-2.0
//! Multi-file terrain source tiles (ADR-8595, `docs/formats/terrain.md` § 15).
//!
//! A tile-set descriptor (`heightfield.tiles.json`, `"terrain_source_tiles": 1`) names schema-2
//! JSON or 16-bit PNG source files and where each sits in one global count grid. The structure
//! (tile count, bounds, overlap width, gaps) is validated from the descriptor alone, before any
//! sample is decoded; each tile's dimensions and mapping are checked when it is decoded, and the
//! samples two tiles share must be equal count for count. Nothing is resampled or smoothed: a
//! global sample is the count every tile covering it holds.
//!
//! [`TileSetReader`] reads rectangles of the global grid by decoding only the tiles that cover
//! the rectangle and dropping the others, so its charged sample buffers depend on the tile and
//! rectangle sizes, never on the area of the whole grid ([`TileReadStats`]).

use serde::Deserialize;

use super::{
    CountMapping, ImportLimits, MissingDatum, PngImportSettings, SourceError, TerrainSource, invalid,
};

/// The tile-set descriptor version this build reads.
pub const TILE_SET_VERSION: u32 = 1;

/// Most tiles one descriptor may name.
pub const TILE_SET_MAX_TILES: usize = 4096;

/// Largest descriptor accepted, bytes.
pub const TILE_SET_MAX_DESCRIPTOR_BYTES: usize = 1 << 20;

/// How far (meters, per axis) a JSON tile's own `sample0` may sit from the position its
/// `origin` gives in the global grid. Placement always comes from the descriptor; a tile
/// further off than this is refused as misregistered (`terrain_tiles_misregistered`).
pub const TILE_REGISTRATION_TOLERANCE_M: f64 = 0.001;

#[derive(Deserialize)]
struct VersionProbe {
    terrain_source_tiles: u32,
}

/// The `terrain_source_tiles` version of a tile-set descriptor, read before the typed parse and
/// without validating anything else (`regia upgrade-project`, ADR-8639).
///
/// # Errors
/// Text that is not a JSON object with an unsigned `terrain_source_tiles` key.
pub fn tile_set_file_version(bytes: &[u8]) -> Result<u32, SourceError> {
    serde_json::from_slice::<VersionProbe>(bytes)
        .map(|probe| probe.terrain_source_tiles)
        .map_err(|e| invalid(format!("tile-set descriptor: {e}")))
}

/// The serde shape of the tile-set descriptor, frozen as `terrain-source-tiles` in
/// `docs/formats/format-hashes.ron` (`cargo xtask check-formats`, ADR-8639): the document, its
/// grid, units and tile entries.
#[must_use]
pub fn tile_set_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names as f};
    join_field_lists(&[
        f::<TileSetJson>(),
        f::<GridJson>(),
        f::<UnitsJson>(),
        f::<TileJson>(),
    ])
}

/// The tile-set descriptor is at its first version: no migration steps.
#[must_use]
pub fn tile_set_migration_sources() -> Vec<u32> {
    Vec::new()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TileSetJson {
    terrain_source_tiles: u32,
    grid: GridJson,
    units: UnitsJson,
    #[serde(default = "default_overlap")]
    max_overlap: u32,
    tiles: Vec<TileJson>,
}

fn default_overlap() -> u32 {
    1
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GridJson {
    cols: u32,
    rows: u32,
    cell_m: f32,
    sample0_m: [f32; 2],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitsJson {
    meters_per_count: f32,
    zero_counts: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TileJson {
    path: String,
    origin: [u32; 2],
    size: [u32; 2],
}

/// One source tile of a [`TileSetDescriptor`]: its file and the rectangle it covers in the
/// global grid (columns/rows, canonical south/west order).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceTileRef {
    /// File path relative to the descriptor's folder (`/` separators, no `..`).
    pub path: String,
    /// Global column and row of the tile's sample (0, 0).
    pub origin: [u32; 2],
    /// Columns and rows the tile holds.
    pub size: [u32; 2],
}

impl SourceTileRef {
    /// The stable tile id used in reports and errors: `r<row>_c<col>` of its origin.
    #[must_use]
    pub fn id(&self) -> String {
        format!("r{}_c{}", self.origin[1], self.origin[0])
    }

    /// Exclusive end column and row in the global grid.
    #[must_use]
    pub fn end(&self) -> [u32; 2] {
        [self.origin[0] + self.size[0], self.origin[1] + self.size[1]]
    }

    /// Samples the tile holds.
    #[must_use]
    pub fn samples(&self) -> usize {
        self.size[0] as usize * self.size[1] as usize
    }

    /// True when the tile covers part of `rect` (`[x0, y0, x1, y1)`, exclusive ends).
    #[must_use]
    pub fn intersects(&self, rect: [u32; 4]) -> bool {
        let [x1, y1] = self.end();
        self.origin[0] < rect[2] && rect[0] < x1 && self.origin[1] < rect[3] && rect[1] < y1
    }
}

/// A validated tile-set descriptor: the global count mapping and its tiles, sorted by origin
/// (row, then column), so the order the file lists them in never changes a result.
#[derive(Clone, Debug, PartialEq)]
pub struct TileSetDescriptor {
    mapping: CountMapping,
    max_overlap: u32,
    tiles: Vec<SourceTileRef>,
}

/// What the structure check of a descriptor found, before any sample is decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileSetCoverage {
    /// Tiles named.
    pub tiles: usize,
    /// Global samples.
    pub grid_samples: u64,
    /// Samples counted once per tile covering them (`>= grid_samples`; the excess is the
    /// shared borders).
    pub tile_samples: u64,
    /// Tile pairs that share samples (each must agree count for count).
    pub overlapping_pairs: usize,
    /// Samples of the largest tile.
    pub max_tile_samples: usize,
}

impl TileSetDescriptor {
    /// Parses and validates a descriptor: version (probed first, `terrain_tiles_version`), the
    /// global mapping (the [`CountMapping`] rules), the tile count (`terrain_tiles_limit`), each
    /// tile's path (`terrain_tiles_path`), size and bounds (`terrain_tiles_bounds`; a tile is at
    /// least 2x2 and at most `limits.max_samples`), overlaps (`terrain_tiles_overlap`: two tiles
    /// may share a border at most `max_overlap` samples wide) and coverage (`terrain_tiles_gap`:
    /// every global sample is in a tile). No tile file is read.
    ///
    /// # Errors
    /// As above; malformed JSON is `height_io_decode`, an oversized descriptor
    /// `height_source_limit_bytes`.
    #[tracing::instrument(skip_all, target = "regia_terrain")]
    pub fn from_json(bytes: &[u8], limits: ImportLimits) -> Result<Self, SourceError> {
        let limits = limits.effective();
        if bytes.len() > TILE_SET_MAX_DESCRIPTOR_BYTES {
            return Err(invalid(format!(
                "height_source_limit_bytes: the tile-set descriptor is over {TILE_SET_MAX_DESCRIPTOR_BYTES} bytes"
            )));
        }
        let probe: VersionProbe =
            serde_json::from_slice(bytes).map_err(|e| invalid(format!("tile-set descriptor: {e}")))?;
        if probe.terrain_source_tiles != TILE_SET_VERSION {
            return Err(invalid(format!(
                "terrain_tiles_version: terrain_source_tiles {} is not supported (this build reads {TILE_SET_VERSION})",
                probe.terrain_source_tiles
            )));
        }
        let doc: TileSetJson =
            serde_json::from_slice(bytes).map_err(|e| invalid(format!("tile-set descriptor: {e}")))?;
        debug_assert_eq!(doc.terrain_source_tiles, TILE_SET_VERSION);
        let mapping = CountMapping {
            cols: doc.grid.cols,
            rows: doc.grid.rows,
            meters_per_count: doc.units.meters_per_count,
            zero_counts: doc.units.zero_counts,
            cell_m: doc.grid.cell_m,
            sample0_m: doc.grid.sample0_m,
        };
        mapping.validate()?;
        if doc.tiles.is_empty() || doc.tiles.len() > TILE_SET_MAX_TILES {
            return Err(invalid(format!(
                "terrain_tiles_limit: a tile set names 1..={TILE_SET_MAX_TILES} tiles, this one {}",
                doc.tiles.len()
            )));
        }
        let mut tiles = Vec::with_capacity(doc.tiles.len());
        for tile in doc.tiles {
            check_tile_path(&tile.path)?;
            let tile = SourceTileRef {
                path: tile.path,
                origin: tile.origin,
                size: tile.size,
            };
            check_tile_rect(&tile, &mapping, limits)?;
            tiles.push(tile);
        }
        tiles.sort_by(|a, b| (a.origin[1], a.origin[0], &a.path).cmp(&(b.origin[1], b.origin[0], &b.path)));
        let descriptor = Self {
            mapping,
            max_overlap: doc.max_overlap,
            tiles,
        };
        descriptor.coverage()?;
        Ok(descriptor)
    }

    /// The global count mapping (grid size, datum, scale, cell and registration of sample 0).
    #[must_use]
    pub fn mapping(&self) -> CountMapping {
        self.mapping
    }

    /// The tiles, sorted by origin row, then column.
    #[must_use]
    pub fn tiles(&self) -> &[SourceTileRef] {
        &self.tiles
    }

    /// The widest shared border allowed between two tiles, samples.
    #[must_use]
    pub fn max_overlap(&self) -> u32 {
        self.max_overlap
    }

    /// Indices (into [`Self::tiles`]) of the tiles covering part of `rect` (`[x0, y0, x1, y1)`).
    pub fn tiles_intersecting(&self, rect: [u32; 4]) -> impl Iterator<Item = usize> + '_ {
        self.tiles
            .iter()
            .enumerate()
            .filter(move |(_, t)| t.intersects(rect))
            .map(|(i, _)| i)
    }

    /// The structure check: overlap widths and gaps, from the rectangles alone.
    ///
    /// # Errors
    /// `terrain_tiles_overlap` or `terrain_tiles_gap`.
    pub fn coverage(&self) -> Result<TileSetCoverage, SourceError> {
        let mut overlapping_pairs = 0;
        for (i, a) in self.tiles.iter().enumerate() {
            let [ax1, ay1] = a.end();
            for b in &self.tiles[i + 1..] {
                let [bx1, by1] = b.end();
                let wide = ax1.min(bx1).saturating_sub(a.origin[0].max(b.origin[0]));
                let tall = ay1.min(by1).saturating_sub(a.origin[1].max(b.origin[1]));
                if wide == 0 || tall == 0 {
                    continue;
                }
                if wide.min(tall) > self.max_overlap {
                    return Err(invalid(format!(
                        "terrain_tiles_overlap: tiles {} and {} share a {wide}x{tall} sample region; a shared border is at most {} samples wide",
                        a.id(),
                        b.id(),
                        self.max_overlap
                    )));
                }
                overlapping_pairs += 1;
            }
        }
        // Sweep the rows in slabs between tile edges: in each slab, the union of the column
        // intervals of the tiles spanning it must be the whole grid width.
        let mut edges: Vec<u32> = vec![0, self.mapping.rows];
        for t in &self.tiles {
            edges.push(t.origin[1]);
            edges.push(t.end()[1]);
        }
        edges.sort_unstable();
        edges.dedup();
        let mut spans: Vec<[u32; 2]> = Vec::new();
        for slab in edges.windows(2) {
            let (y0, y1) = (slab[0], slab[1]);
            if y0 >= self.mapping.rows {
                break;
            }
            spans.clear();
            spans.extend(
                self.tiles
                    .iter()
                    .filter(|t| t.origin[1] <= y0 && t.end()[1] >= y1)
                    .map(|t| [t.origin[0], t.end()[0]]),
            );
            spans.sort_unstable();
            let mut covered = 0u32;
            for span in &spans {
                if span[0] > covered {
                    break;
                }
                covered = covered.max(span[1]);
            }
            if covered < self.mapping.cols {
                return Err(invalid(format!(
                    "terrain_tiles_gap: global sample (col {covered}, row {y0}) is in no tile; a tile set covers every sample of its {}x{} grid",
                    self.mapping.cols, self.mapping.rows
                )));
            }
        }
        Ok(TileSetCoverage {
            tiles: self.tiles.len(),
            grid_samples: u64::from(self.mapping.cols) * u64::from(self.mapping.rows),
            tile_samples: self.tiles.iter().map(|t| t.samples() as u64).sum(),
            overlapping_pairs,
            max_tile_samples: self.tiles.iter().map(SourceTileRef::samples).max().unwrap_or(0),
        })
    }

    /// Decodes one tile's file bytes (`.json`: schema 2; `.png`: 16-bit grayscale whose datum,
    /// if it has one, must be the descriptor's) and checks it against the descriptor: its
    /// dimensions (`terrain_tiles_dimensions`), datum, scale and cell, bit for bit
    /// (`terrain_tiles_mapping`), and for JSON its `sample0` against the position of its origin
    /// (`terrain_tiles_misregistered`, [`TILE_REGISTRATION_TOLERANCE_M`]).
    ///
    /// # Errors
    /// As above, plus the decoder's own refusals and budgets (`limits` apply per tile).
    pub fn decode_tile(
        &self,
        index: usize,
        bytes: &[u8],
        limits: ImportLimits,
    ) -> Result<TerrainSource, SourceError> {
        let tile = self
            .tiles
            .get(index)
            .ok_or_else(|| invalid(format!("no tile {index} in the tile set")))?;
        let expected0 = self.mapping.sample_world(tile.origin[0], tile.origin[1]);
        let at = |e: SourceError| SourceError(format!("{} (tile {} {})", e.0, tile.id(), tile.path));
        let is_png = tile
            .path
            .rsplit('.')
            .next()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("png"));
        let source = if is_png {
            let settings = PngImportSettings {
                meters_per_count: self.mapping.meters_per_count,
                cell_m: self.mapping.cell_m,
                sample0: Some(expected0),
                missing_datum: MissingDatum::Counts(self.mapping.zero_counts),
                ..Default::default()
            };
            TerrainSource::from_png(bytes, &settings, limits).map_err(at)?
        } else {
            TerrainSource::from_json(bytes, limits).map_err(at)?
        };
        if source.dimensions() != tile.size {
            return Err(invalid(format!(
                "terrain_tiles_dimensions: tile {} ({}) is {:?} samples but the descriptor says {:?}",
                tile.id(),
                tile.path,
                source.dimensions(),
                tile.size
            )));
        }
        if source.meters_per_count().to_bits() != self.mapping.meters_per_count.to_bits()
            || source.zero_counts() != self.mapping.zero_counts
            || source.cell_m().to_bits() != self.mapping.cell_m.to_bits()
            || source.spacing_m()[1].to_bits() != self.mapping.cell_m.to_bits()
        {
            return Err(invalid(format!(
                "terrain_tiles_mapping: tile {} ({}) has meters_per_count {}, zero_counts {}, cell_m {} but the tile set has {}, {}, {}; tiles are never rescaled",
                tile.id(),
                tile.path,
                source.meters_per_count(),
                source.zero_counts(),
                source.cell_m(),
                self.mapping.meters_per_count,
                self.mapping.zero_counts,
                self.mapping.cell_m
            )));
        }
        let got = source.sample0();
        let off = [
            f64::from(got[0]) - f64::from(expected0[0]),
            f64::from(got[1]) - f64::from(expected0[1]),
        ];
        if off.iter().any(|d| d.abs() > TILE_REGISTRATION_TOLERANCE_M) {
            return Err(invalid(format!(
                "terrain_tiles_misregistered: tile {} ({}) starts at {got:?} m but origin {:?} of the grid is at {expected0:?} m (off by {off:?} m); a tile is never shifted",
                tile.id(),
                tile.path,
                tile.origin
            )));
        }
        Ok(source)
    }
}

fn check_tile_path(path: &str) -> Result<(), SourceError> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path.split('/').any(|s| s.is_empty() || s == "." || s == "..");
    if bad {
        return Err(invalid(format!(
            "terrain_tiles_path: tile path {path:?} must be relative to the descriptor's folder, `/`-separated, without `.` or `..`"
        )));
    }
    let lower = path.to_ascii_lowercase();
    if !(lower.ends_with(".json") || lower.ends_with(".png")) {
        return Err(invalid(format!(
            "terrain_tiles_path: tile {path:?} must be a .json (schema 2) or .png source"
        )));
    }
    Ok(())
}

fn check_tile_rect(
    tile: &SourceTileRef,
    mapping: &CountMapping,
    limits: ImportLimits,
) -> Result<(), SourceError> {
    if tile.size[0] < 2 || tile.size[1] < 2 {
        return Err(invalid(format!(
            "terrain_tiles_bounds: tile {} is {:?} samples; a tile is at least 2x2",
            tile.id(),
            tile.size
        )));
    }
    let end = [
        tile.origin[0].checked_add(tile.size[0]),
        tile.origin[1].checked_add(tile.size[1]),
    ];
    if end[0].is_none_or(|x| x > mapping.cols) || end[1].is_none_or(|y| y > mapping.rows) {
        return Err(invalid(format!(
            "terrain_tiles_bounds: tile {} at {:?} size {:?} leaves the {}x{} grid",
            tile.id(),
            tile.origin,
            tile.size,
            mapping.cols,
            mapping.rows
        )));
    }
    if tile.samples() > limits.max_samples {
        return Err(invalid(format!(
            "height_source_limit_samples: tile {} holds {} samples, over max_samples {}; split it into smaller tiles",
            tile.id(),
            tile.samples(),
            limits.max_samples
        )));
    }
    Ok(())
}

/// Deterministic counters of a [`TileSetReader`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TileReadStats {
    /// Tile files decoded (a tile spanning several rectangle rows is decoded once per row).
    pub tiles_decoded: u64,
    /// Samples compared between overlapping tiles (all equal, or the read failed).
    pub seam_samples_checked: u64,
    /// Largest sum of decoded tile samples held at once.
    pub peak_charged_samples: usize,
    /// Samples delivered.
    pub samples_read: u64,
}

/// Reads rectangles of a tile set's global grid, decoding only the tiles that cover each
/// rectangle (a decoded tile is kept while consecutive rectangles still need it) and checking
/// that every sample two tiles share is the same count (`terrain_tiles_seam`).
///
/// `read` returns a tile file's bytes; `progress` hears each decode (`tile index`, `tiles
/// decoded so far`) and may cancel with `ControlFlow::Break` (`height_source_cancelled`).
pub struct TileSetReader<'a, R, P> {
    descriptor: &'a TileSetDescriptor,
    limits: ImportLimits,
    read: R,
    progress: P,
    held: Vec<(usize, TerrainSource)>,
    filled: Vec<bool>,
    stats: TileReadStats,
}

impl<'a, R, P> TileSetReader<'a, R, P>
where
    R: FnMut(&SourceTileRef) -> Result<Vec<u8>, SourceError>,
    P: FnMut(usize, u64) -> std::ops::ControlFlow<()>,
{
    /// A reader over `descriptor`; each tile is decoded under `limits`.
    pub fn new(descriptor: &'a TileSetDescriptor, limits: ImportLimits, read: R, progress: P) -> Self {
        Self {
            descriptor,
            limits,
            read,
            progress,
            held: Vec::new(),
            filled: Vec::new(),
            stats: TileReadStats::default(),
        }
    }

    /// The counters so far.
    #[must_use]
    pub fn stats(&self) -> TileReadStats {
        self.stats
    }

    /// Fills `out` with the counts of `rect` (`[x0, y0, x1, y1)`, row-major, canonical order).
    ///
    /// # Errors
    /// A read or decode failure of a covering tile, its descriptor checks
    /// ([`TileSetDescriptor::decode_tile`]), `terrain_tiles_seam` when two tiles disagree on a
    /// shared sample, `terrain_tiles_gap` for an uncovered sample, or a cancel.
    pub fn read_rect(&mut self, rect: [u32; 4], out: &mut Vec<u16>) -> Result<(), SourceError> {
        let [x0, y0, x1, y1] = rect;
        let mapping = self.descriptor.mapping();
        if x0 >= x1 || y0 >= y1 || x1 > mapping.cols || y1 > mapping.rows {
            return Err(invalid(format!(
                "rectangle {rect:?} is not inside the tile-set grid"
            )));
        }
        let width = (x1 - x0) as usize;
        let len = width * (y1 - y0) as usize;
        out.clear();
        out.resize(len, 0);
        self.filled.clear();
        self.filled.resize(len, false);
        let needed: Vec<usize> = self.descriptor.tiles_intersecting(rect).collect();
        // Drop what this rectangle no longer needs before decoding more.
        self.held.retain(|(i, _)| needed.contains(i));
        for &index in &needed {
            if !self.held.iter().any(|(i, _)| *i == index) {
                let tile = &self.descriptor.tiles()[index];
                if (self.progress)(index, self.stats.tiles_decoded).is_break() {
                    self.held.clear();
                    return Err(invalid("height_source_cancelled: tile-set read cancelled"));
                }
                let bytes = (self.read)(tile)?;
                let source = self.descriptor.decode_tile(index, &bytes, self.limits)?;
                drop(bytes);
                self.stats.tiles_decoded += 1;
                self.held.push((index, source));
                let charged: usize = self.held.iter().map(|(_, s)| s.counts().len()).sum();
                self.stats.peak_charged_samples = self.stats.peak_charged_samples.max(charged);
            }
        }
        // Deterministic merge order: tiles in descriptor (origin) order.
        self.held.sort_by_key(|(i, _)| *i);
        for (index, source) in &self.held {
            let tile = &self.descriptor.tiles()[*index];
            let [tx1, ty1] = tile.end();
            let (cx0, cx1) = (x0.max(tile.origin[0]), x1.min(tx1));
            let (cy0, cy1) = (y0.max(tile.origin[1]), y1.min(ty1));
            let counts = source.counts();
            let tw = tile.size[0] as usize;
            for gy in cy0..cy1 {
                let src_row = (gy - tile.origin[1]) as usize * tw;
                let dst_row = (gy - y0) as usize * width;
                for gx in cx0..cx1 {
                    let v = counts[src_row + (gx - tile.origin[0]) as usize];
                    let d = dst_row + (gx - x0) as usize;
                    if self.filled[d] {
                        self.stats.seam_samples_checked += 1;
                        if out[d] != v {
                            let other = self
                                .held
                                .iter()
                                .map(|(i, _)| &self.descriptor.tiles()[*i])
                                .find(|t| {
                                    let [ex, ey] = t.end();
                                    t.origin[0] <= gx && gx < ex && t.origin[1] <= gy && gy < ey
                                })
                                .map_or_else(String::new, SourceTileRef::id);
                            return Err(invalid(format!(
                                "terrain_tiles_seam: global sample (col {gx}, row {gy}) is {} in tile {other} but {v} in tile {}; shared border samples must be equal (nothing is blended)",
                                out[d],
                                tile.id()
                            )));
                        }
                    } else {
                        out[d] = v;
                        self.filled[d] = true;
                    }
                }
            }
        }
        if let Some(missing) = self.filled.iter().position(|f| !f) {
            let (col, row) = (x0 as usize + missing % width, y0 as usize + missing / width);
            return Err(invalid(format!(
                "terrain_tiles_gap: global sample (col {col}, row {row}) is in no tile"
            )));
        }
        self.stats.samples_read += len as u64;
        Ok(())
    }

    /// Reads the whole grid in `band`-sample square rectangles (row-major), calling `visit`
    /// with each rectangle and its counts: a full validation and statistics pass whose memory
    /// is bounded by the band and tile sizes.
    ///
    /// # Errors
    /// As [`Self::read_rect`], or `visit`'s own error.
    pub fn for_each_rect(
        &mut self,
        band: u32,
        mut visit: impl FnMut([u32; 4], &[u16]) -> Result<(), SourceError>,
    ) -> Result<(), SourceError> {
        let band = band.max(2);
        let mapping = self.descriptor.mapping();
        let mut buffer = Vec::new();
        for y0 in (0..mapping.rows).step_by(band as usize) {
            for x0 in (0..mapping.cols).step_by(band as usize) {
                let rect = [
                    x0,
                    y0,
                    (x0 + band).min(mapping.cols),
                    (y0 + band).min(mapping.rows),
                ];
                self.read_rect(rect, &mut buffer)?;
                visit(rect, &buffer)?;
            }
        }
        self.held.clear();
        Ok(())
    }

    /// Assembles the whole grid into one [`TerrainSource`] (for hosts that edit it in memory,
    /// such as the editor import). Refused past `limits.max_samples` with a hint: a tile set
    /// that large is cooked tile by tile instead.
    ///
    /// # Errors
    /// `height_source_limit_samples`, or any error of [`Self::for_each_rect`].
    pub fn assemble(&mut self, band: u32) -> Result<TerrainSource, SourceError> {
        let mapping = self.descriptor.mapping();
        let total = u64::from(mapping.cols) * u64::from(mapping.rows);
        if total > self.limits.effective().max_samples as u64 {
            return Err(invalid(format!(
                "height_source_limit_samples: the tile set's {}x{} grid is over max_samples {}; cook it tile by tile (regia cook streams source tiles) instead of importing it whole",
                mapping.cols,
                mapping.rows,
                self.limits.effective().max_samples
            )));
        }
        let cols = mapping.cols as usize;
        let mut counts = vec![0u16; total as usize];
        self.for_each_rect(band, |rect, samples| {
            let width = (rect[2] - rect[0]) as usize;
            for (r, row) in samples.chunks(width).enumerate() {
                let start = (rect[1] as usize + r) * cols + rect[0] as usize;
                counts[start..start + width].copy_from_slice(row);
            }
            Ok(())
        })?;
        TerrainSource::from_counts(counts, mapping, self.limits)
    }
}
