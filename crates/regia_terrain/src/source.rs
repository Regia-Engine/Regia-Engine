// SPDX-License-Identifier: MPL-2.0
//! Bounded, count-based terrain source imports (ADR-8063).
//!
//! Requirements adapted from DO27 `ir-content/src/heightfield_{json,png}.rs`,
//! d6ed37677 (regia-absorb-1), owner-authorized 2026-11-16. Original Regia Rust;
//! game scale, identifiers and implicit fallback policies are not carried over.
//! These are authoring inputs; the cooked [`regia_asset::Heightfield`] stays unchanged.

use std::io::Cursor;

use serde::{Deserialize, Serialize};

#[path = "source_json.rs"]
mod json;
#[path = "source_mapping.rs"]
mod mapping;
#[path = "source_raster.rs"]
mod raster;
#[path = "source_tiles.rs"]
mod tiles;

pub use mapping::{COUNT_MAPPING_CANONICAL_LEN, CountMapping};
pub use raster::{
    ColOrder, MissingDataPolicy, RASTER_DESCRIPTOR_VERSION, RasterDescriptor, RasterGeometry, RasterLayout,
    RasterMaskSpec, RasterRegistration, RowOrder, ValidMask, raster_descriptor_file_version,
    raster_descriptor_migration_sources, raster_descriptor_schema_fingerprint, stored_index,
};
pub use tiles::{
    SourceTileRef, TILE_REGISTRATION_TOLERANCE_M, TILE_SET_MAX_DESCRIPTOR_BYTES, TILE_SET_MAX_TILES,
    TILE_SET_VERSION, TileReadStats, TileSetCoverage, TileSetDescriptor, TileSetReader,
    tile_set_file_version, tile_set_migration_sources, tile_set_schema_fingerprint,
};

/// Resource budgets checked before decoding pixels and before parsing JSON.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ImportLimits {
    /// Maximum encoded source length in bytes.
    pub max_bytes: usize,
    /// Maximum decoded sample count.
    pub max_samples: usize,
    /// Maximum JSON value depth (root object is depth one).
    pub max_json_depth: usize,
    /// Maximum ignored JSON values and object keys, shared across the document.
    pub max_unknown_values: usize,
}

impl ImportLimits {
    /// Fixed engine ceilings, also the default policy. Callers can only lower them.
    pub const HARD_MAX: Self = Self {
        max_bytes: 128 << 20,
        max_samples: 16 << 20,
        max_json_depth: 32,
        max_unknown_values: 65_536,
    };

    /// Clamp this request to a host policy and the fixed engine ceilings.
    pub fn restricted_by(self, policy: Self) -> Self {
        Self {
            max_bytes: self.max_bytes.min(policy.max_bytes).min(Self::HARD_MAX.max_bytes),
            max_samples: self
                .max_samples
                .min(policy.max_samples)
                .min(Self::HARD_MAX.max_samples),
            max_json_depth: self
                .max_json_depth
                .min(policy.max_json_depth)
                .min(Self::HARD_MAX.max_json_depth),
            max_unknown_values: self
                .max_unknown_values
                .min(policy.max_unknown_values)
                .min(Self::HARD_MAX.max_unknown_values),
        }
    }

    /// Effective budgets after applying the fixed engine ceilings.
    pub fn effective(self) -> Self {
        self.restricted_by(Self::HARD_MAX)
    }
}

/// Deterministic JSON work and storage counters, including failed imports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JsonImportProgress {
    /// Samples charged before decoding; never exceeds the effective sample limit.
    pub samples_charged: usize,
    /// Successfully decoded samples retained so far.
    pub samples_stored: usize,
    /// Sample vector capacity, in u16 elements (not bytes).
    pub sample_capacity: usize,
    /// Ignored values and object keys charged so far.
    pub unknown_values: usize,
    /// Greatest accepted JSON value depth (root is one).
    pub max_depth: usize,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self::HARD_MAX
    }
}

/// Policy for a PNG without its requested datum text keyword.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MissingDatum {
    /// Refuse ambiguous heights (default).
    #[default]
    Reject,
    /// Use the sample at `(width / 2, height / 2)` as zero.
    CenterSample,
    /// Use this caller-provided zero count.
    Counts(u16),
}

/// Placement and height interpretation of a PNG source.
#[derive(Clone, Debug)]
pub struct PngImportSettings {
    /// Positive finite meters per raw count.
    pub meters_per_count: f32,
    /// Positive finite distance between adjacent samples, on both axes.
    pub cell_m: f32,
    /// X/Z coordinates of sample (0, 0); absent uses `-dimension * cell_m / 2`.
    pub sample0: Option<[f32; 2]>,
    /// Uncompressed Latin-1 PNG text keyword storing an integer in 0..=65535.
    pub datum_keyword: String,
    /// What to do if the keyword is absent; malformed metadata always fails.
    pub missing_datum: MissingDatum,
}

impl Default for PngImportSettings {
    fn default() -> Self {
        Self {
            meters_per_count: 1.0,
            cell_m: 1.0,
            sample0: None,
            datum_keyword: "terrain_zero_counts".into(),
            missing_datum: MissingDatum::Reject,
        }
    }
}

/// An invalid source, metadata value or exhausted import budget.
#[derive(Debug, thiserror::Error)]
#[error("terrain source: {0}")]
pub struct SourceError(
    /// Human-readable validation or codec diagnostic.
    pub String,
);

impl SourceError {
    /// Stable agent-compatible refusal code; other validation errors use `height_io_decode`.
    pub fn code(&self) -> &'static str {
        for code in [
            "height_source_limit_bytes",
            "height_source_limit_samples",
            "height_source_limit_depth",
            "height_source_limit_metadata",
            "height_source_cancelled",
            "height_source_allocation",
            "terrain_raster_version",
            "terrain_raster_transform",
            "terrain_raster_mask",
            "terrain_raster_missing_data",
            "terrain_tiles_version",
            "terrain_tiles_limit",
            "terrain_tiles_path",
            "terrain_tiles_bounds",
            "terrain_tiles_overlap",
            "terrain_tiles_gap",
            "terrain_tiles_dimensions",
            "terrain_tiles_mapping",
            "terrain_tiles_misregistered",
            "terrain_tiles_seam",
            "terrain_overview_cancelled",
        ] {
            if self
                .0
                .strip_prefix(code)
                .is_some_and(|tail| tail.starts_with(':'))
            {
                return code;
            }
        }
        "height_io_decode"
    }
}

fn invalid(message: impl Into<String>) -> SourceError {
    SourceError(message.into())
}

/// Validated rectangular row-major samples. Row zero is minimum Z; column zero minimum X.
///
/// Values and placement are retained without resampling. Conversion is exactly
/// `(f32::from(count) - f32::from(zero_counts)) * meters_per_count`.
///
/// A source built through a [`RasterDescriptor`] ([`Self::from_raster`], ADR-8542) is still
/// canonical (row zero south, column zero west: its stored order was permuted exactly) and may
/// have a different spacing on Z than on X and samples without data ([`Self::valid_mask`]).
#[derive(Clone, Debug)]
pub struct TerrainSource {
    width: u32,
    height: u32,
    counts: Vec<u16>,
    meters_per_count: f32,
    zero_counts: u16,
    cell_m: f32,
    sample0: [f32; 2],
    /// Spacing on Z; equals `cell_m` for every legacy source.
    spacing_z: f32,
    /// Canonical valid-data mask; `None` when every sample holds data.
    valid: Option<ValidMask>,
    layout: RasterLayout,
}

impl TerrainSource {
    /// Parse external schema-2 JSON, with numeric values decoded directly into f32.
    /// Unknown fields (including application identifiers) are ignored.
    ///
    /// # Errors
    /// Refuses wrong schemas, axes, units, nonfinite mappings, invalid counts or budgets.
    #[tracing::instrument(skip_all, target = "regia_terrain")]
    pub fn from_json(bytes: &[u8], limits: ImportLimits) -> Result<Self, SourceError> {
        Self::from_json_with_observer(bytes, limits, |_| std::ops::ControlFlow::Continue(()))
    }

    /// Parse JSON with deterministic counters and cooperative cancellation.
    ///
    /// The observer runs before parsing, before each sample decode or ignored value,
    /// after each stored sample, and before returning a validated source. Returning
    /// `ControlFlow::Break(())` cancels and drops all partial storage. No partial source
    /// is returned. Encoded bytes bound work between callbacks (including string tokens).
    ///
    /// # Errors
    /// Returns the same validation/limit errors as [`Self::from_json`], or
    /// `height_source_cancelled` when the observer requests cancellation.
    #[tracing::instrument(skip_all, target = "regia_terrain")]
    pub fn from_json_with_observer(
        bytes: &[u8],
        limits: ImportLimits,
        mut observer: impl FnMut(JsonImportProgress) -> std::ops::ControlFlow<()>,
    ) -> Result<Self, SourceError> {
        let limits = limits.effective();
        check_bytes(bytes, limits)?;
        let (doc, progress) = json::parse(bytes, limits, &mut observer)?;
        if doc.schema != 2 {
            return Err(invalid("only schema 2 is supported"));
        }
        if doc.grid.row0 != "south" || doc.grid.col0 != "west" {
            return Err(invalid("row0 must be south and col0 west"));
        }
        if doc.units.kind != "counts" {
            return Err(invalid("units.kind must be counts"));
        }
        let dimensions = [doc.grid.cols, doc.grid.rows];
        let sample0 = [
            doc.grid
                .sample0_x_m
                .unwrap_or(-(dimensions[0] as f32 * doc.grid.cell_m) * 0.5),
            doc.grid
                .sample0_z_m
                .unwrap_or(-(dimensions[1] as f32 * doc.grid.cell_m) * 0.5),
        ];
        let source = Self::validated(
            dimensions,
            doc.counts,
            doc.units.meters_per_count,
            doc.units.zero_counts,
            [doc.grid.cell_m; 2],
            sample0,
            limits,
        )?;
        if observer(progress).is_break() {
            return Err(invalid("height_source_cancelled: import cancelled"));
        }
        Ok(source)
    }

    /// Decode noninterlaced grayscale 8/16-bit PNG and its zero datum.
    /// Eight-bit counts expand by 257. Metadata is checked through IEND, including
    /// text after IDAT. Duplicate datum keywords are refused, even if equal.
    ///
    /// # Errors
    /// Refuses color/animated/interlaced images, invalid metadata, dimensions or budgets.
    #[tracing::instrument(skip_all, target = "regia_terrain")]
    pub fn from_png(
        bytes: &[u8],
        settings: &PngImportSettings,
        limits: ImportLimits,
    ) -> Result<Self, SourceError> {
        let limits = limits.effective();
        check_bytes(bytes, limits)?;
        if settings.datum_keyword.is_empty() || settings.datum_keyword.len() > 79 {
            return Err(invalid("datum keyword must contain 1..=79 bytes"));
        }
        let mut decoder = png::Decoder::new(Cursor::new(bytes));
        decoder.set_limits(png::Limits {
            bytes: limits.max_bytes,
        });
        decoder.set_transformations(png::Transformations::IDENTITY);
        let mut reader = decoder.read_info().map_err(|e| invalid(e.to_string()))?;
        let info = reader.info();
        let dimensions = [info.width, info.height];
        let count = check_dimensions(dimensions, limits)?;
        if info.color_type != png::ColorType::Grayscale
            || info.interlaced
            || info.animation_control.is_some()
            || !matches!(info.bit_depth, png::BitDepth::Eight | png::BitDepth::Sixteen)
        {
            return Err(invalid(
                "expected noninterlaced, nonanimated grayscale 8/16-bit PNG",
            ));
        }
        let depth = info.bit_depth;
        let size = reader
            .output_buffer_size()
            .ok_or_else(|| invalid("PNG buffer overflow"))?;
        if size > count * 2 {
            return Err(invalid("unexpected PNG buffer size"));
        }
        let mut pixels = vec![0; size];
        let frame = reader
            .next_frame(&mut pixels)
            .map_err(|e| invalid(e.to_string()))?;
        let data = &pixels[..frame.buffer_size()];
        let counts: Vec<u16> = if depth == png::BitDepth::Sixteen {
            data.as_chunks::<2>()
                .0
                .iter()
                .map(|&v| u16::from_be_bytes(v))
                .collect()
        } else {
            data.iter().map(|&v| u16::from(v) * 257).collect()
        };
        reader.finish().map_err(|e| invalid(e.to_string()))?;
        let info = reader.info();
        if info
            .compressed_latin1_text
            .iter()
            .any(|t| t.keyword == settings.datum_keyword)
            || info.utf8_text.iter().any(|t| t.keyword == settings.datum_keyword)
        {
            return Err(invalid("datum must use an uncompressed Latin-1 tEXt chunk"));
        }
        let mut entries = info
            .uncompressed_latin1_text
            .iter()
            .filter(|t| t.keyword == settings.datum_keyword);
        let zero = if let Some(text) = entries.next() {
            if entries.next().is_some() {
                return Err(invalid("duplicate datum keyword"));
            }
            text.text
                .trim()
                .parse::<u16>()
                .map_err(|_| invalid("datum must be an integer in 0..=65535"))?
        } else {
            match settings.missing_datum {
                MissingDatum::Reject => return Err(invalid("missing datum keyword")),
                MissingDatum::Counts(value) => value,
                MissingDatum::CenterSample => *counts
                    .get((dimensions[1] as usize / 2) * dimensions[0] as usize + dimensions[0] as usize / 2)
                    .ok_or_else(|| invalid("missing center sample"))?,
            }
        };
        let sample0 = settings.sample0.unwrap_or([
            -(dimensions[0] as f32 * settings.cell_m) * 0.5,
            -(dimensions[1] as f32 * settings.cell_m) * 0.5,
        ]);
        Self::validated(
            dimensions,
            counts,
            settings.meters_per_count,
            zero,
            [settings.cell_m; 2],
            sample0,
            limits,
        )
    }

    fn validated(
        dimensions: [u32; 2],
        counts: Vec<u16>,
        scale: f32,
        zero: u16,
        spacing: [f32; 2],
        sample0: [f32; 2],
        limits: ImportLimits,
    ) -> Result<Self, SourceError> {
        let count = check_dimensions(dimensions, limits)?;
        if counts.len() != count {
            return Err(invalid("counts length does not match cols * rows"));
        }
        if !scale.is_finite() || scale <= 0.0 || !(65535.0 * scale).is_finite() {
            return Err(invalid(
                "meters_per_count must be positive with a finite full range",
            ));
        }
        if spacing.iter().any(|s| !s.is_finite() || *s <= 0.0) || sample0.iter().any(|v| !v.is_finite()) {
            return Err(invalid("cell_m must be positive and placement finite"));
        }
        for ((&dimension, &origin), &cell_m) in dimensions.iter().zip(&sample0).zip(&spacing) {
            let end = origin + (dimension - 1) as f32 * cell_m;
            if !end.is_finite() || end <= origin {
                return Err(invalid("sample extent must be finite and representable"));
            }
        }
        Ok(Self {
            width: dimensions[0],
            height: dimensions[1],
            counts,
            meters_per_count: scale,
            zero_counts: zero,
            cell_m: spacing[0],
            sample0,
            spacing_z: spacing[1],
            valid: None,
            layout: RasterLayout::LEGACY,
        })
    }

    /// Builds a canonical source from stored-order `counts` described by `descriptor`
    /// (ADR-8542): a north-first or east-first raster is permuted exactly into south/west order,
    /// the registration resolves the first sample, X and Z keep their own spacing, and the mask
    /// and nodata value mark missing samples. Counts are never interpolated or filled.
    ///
    /// # Errors
    /// The descriptor's refusals (`terrain_raster_version`, `terrain_raster_transform`,
    /// `terrain_raster_mask`), `terrain_raster_missing_data` when no sample holds data, and the
    /// grid, scale and budget refusals of [`Self::from_counts`].
    #[tracing::instrument(skip_all, target = "regia_terrain")]
    pub fn from_raster(
        counts: Vec<u16>,
        dimensions: [u32; 2],
        meters_per_count: f32,
        zero_counts: u16,
        descriptor: &RasterDescriptor,
        limits: ImportLimits,
    ) -> Result<Self, SourceError> {
        let limits = limits.effective();
        let count = check_dimensions(dimensions, limits)?;
        if counts.len() != count {
            return Err(invalid("counts length does not match cols * rows"));
        }
        let geometry = descriptor.geometry(dimensions)?;
        let flags = descriptor.stored_validity(dimensions, &counts)?;
        let (row0, col0) = (descriptor.row0, descriptor.col0);
        let counts = raster::to_canonical(counts, dimensions, row0, col0);
        let valid = match flags {
            None => None,
            Some(flags) => {
                let mask = ValidMask::from_flags(
                    dimensions[0],
                    dimensions[1],
                    &raster::to_canonical(flags, dimensions, row0, col0),
                )?;
                if mask.invalid_count() == count {
                    return Err(invalid("terrain_raster_missing_data: no sample holds data"));
                }
                (mask.invalid_count() > 0).then_some(mask)
            }
        };
        let mut source = Self::validated(
            dimensions,
            counts,
            meters_per_count,
            zero_counts,
            geometry.spacing_m,
            geometry.sample0_m,
            limits,
        )?;
        source.valid = valid;
        source.layout = RasterLayout {
            version: Some(descriptor.version),
            row0,
            col0,
            registration: descriptor.registration,
        };
        Ok(source)
    }

    /// Reinterprets this source's decoded counts, datum and scale under `descriptor`
    /// ([`Self::from_raster`]); the descriptor replaces the cell size and first sample.
    ///
    /// # Errors
    /// [`Self::from_raster`]'s refusals, or `height_io_decode` when a descriptor was already
    /// applied (its counts are no longer in their stored order).
    pub fn with_raster(
        &self,
        descriptor: &RasterDescriptor,
        limits: ImportLimits,
    ) -> Result<Self, SourceError> {
        if self.layout != RasterLayout::LEGACY {
            return Err(invalid("a raster descriptor was already applied to this source"));
        }
        Self::from_raster(
            self.counts.clone(),
            self.dimensions(),
            self.meters_per_count,
            self.zero_counts,
            descriptor,
            limits,
        )
    }

    /// `[columns, rows]`, each at least two.
    pub fn dimensions(&self) -> [u32; 2] {
        [self.width, self.height]
    }
    /// Original row-major counts, without resampling.
    pub fn counts(&self) -> &[u16] {
        &self.counts
    }
    /// Meters per raw count.
    pub fn meters_per_count(&self) -> f32 {
        self.meters_per_count
    }
    /// Count corresponding to height zero.
    pub fn zero_counts(&self) -> u16 {
        self.zero_counts
    }
    /// Square-cell spacing in meters; for a descriptor source with unequal spacing, the X
    /// spacing ([`Self::spacing_m`] has both).
    pub fn cell_m(&self) -> f32 {
        self.cell_m
    }
    /// Spacing on X and on Z, meters (equal for every schema-2 and PNG source).
    #[must_use]
    pub fn spacing_m(&self) -> [f32; 2] {
        [self.cell_m, self.spacing_z]
    }
    /// How the source was stored and registered before canonicalization.
    #[must_use]
    pub fn layout(&self) -> RasterLayout {
        self.layout
    }
    /// The canonical valid-data mask, or `None` when every sample holds data.
    #[must_use]
    pub fn valid_mask(&self) -> Option<&ValidMask> {
        self.valid.as_ref()
    }
    /// Samples without data (0 for every schema-2 and PNG source).
    #[must_use]
    pub fn missing_samples(&self) -> usize {
        self.valid.as_ref().map_or(0, ValidMask::invalid_count)
    }
    /// Whether canonical sample `(col, row)` holds data (clamped like [`Self::height_m`]).
    #[must_use]
    pub fn is_valid(&self, col: u32, row: u32) -> bool {
        let (col, row) = (col.min(self.width - 1), row.min(self.height - 1));
        self.valid.as_ref().is_none_or(|m| m.is_valid(col, row))
    }
    /// Height of a clamped sample in meters, or `None` for a sample without data. A nodata
    /// sentinel never becomes a height.
    #[must_use]
    pub fn sample_height_m(&self, col: u32, row: u32) -> Option<f32> {
        self.is_valid(col, row).then(|| self.height_m(col, row))
    }
    /// The [`CountMapping`] a cook persists, refused when it cannot describe this source: an
    /// unequal X/Z spacing (`terrain_mapping_unrepresentable`, the cooked index holds one cell
    /// size) or missing samples (`terrain_raster_missing_data`).
    ///
    /// # Errors
    /// As above.
    pub fn count_mapping(&self) -> Result<CountMapping, SourceError> {
        if self.cell_m.to_bits() != self.spacing_z.to_bits() {
            return Err(self.unequal_spacing_error());
        }
        if self.missing_samples() > 0 {
            return Err(self.missing_error());
        }
        Ok(self.mapping())
    }
    fn unequal_spacing_error(&self) -> SourceError {
        invalid(format!(
            "terrain_mapping_unrepresentable: spacing {:?} differs on X and Z; a count mapping holds one cell size and nothing is resampled",
            self.spacing_m()
        ))
    }
    fn missing_error(&self) -> SourceError {
        invalid(format!(
            "terrain_raster_missing_data: {} of {} samples hold no data; fill them or select MissingDataPolicy::Holes",
            self.missing_samples(),
            self.counts.len()
        ))
    }
    /// The runtime [`crate::sampler::TerrainMap`] of this source ([`CountMapping::terrain_map`])
    /// with missing samples handled by `policy`: refused (the default) or cut as a terrain hole
    /// over each missing sample, returned beside the map. Nothing is resampled and no height is
    /// invented where a query can read it.
    ///
    /// # Errors
    /// `terrain_raster_missing_data` under [`MissingDataPolicy::Refuse`],
    /// `terrain_mapping_unrepresentable` for unequal spacing or a grid the runtime map cannot
    /// hold exactly.
    pub fn terrain_map(
        &self,
        policy: MissingDataPolicy,
    ) -> Result<(crate::sampler::TerrainMap, Option<crate::holes::HoleShape>), SourceError> {
        let _span = tracing::info_span!(target: "regia_terrain", "terrain.source_terrain_map").entered();
        if self.missing_samples() > 0 && policy == MissingDataPolicy::Refuse {
            return Err(self.missing_error());
        }
        if self.cell_m.to_bits() != self.spacing_z.to_bits() {
            return Err(self.unequal_spacing_error());
        }
        let mut heightfield = self.heightfield();
        let mut hole = None;
        if let Some(mask) = &self.valid {
            let mut coverage = Vec::with_capacity(heightfield.samples.len());
            for (i, sample) in heightfield.samples.iter_mut().enumerate() {
                let valid = mask.is_valid((i % self.width as usize) as u32, (i / self.width as usize) as u32);
                if !valid {
                    *sample = self.zero_counts;
                }
                coverage.push(if valid { 0 } else { 255 });
            }
            let center = |axis: usize, n: u32| {
                (f64::from(self.sample0[axis]) + 0.5 * f64::from(n - 1) * f64::from(self.spacing_m()[axis]))
                    as f32
            };
            hole = Some(crate::holes::HoleShape::Raster(crate::holes::HoleRaster {
                center: [center(0, self.width), center(1, self.height)],
                half_extents: [
                    (0.5 * f64::from(self.width) * f64::from(self.cell_m)) as f32,
                    (0.5 * f64::from(self.height) * f64::from(self.spacing_z)) as f32,
                ],
                rotation_radians: 0.0,
                width: self.width,
                height: self.height,
                coverage,
            }));
        }
        let map = self.mapping().terrain_map(&heightfield)?;
        Ok((map, hole))
    }
    /// World X/Z position of the first sample.
    pub fn sample0(&self) -> [f32; 2] {
        self.sample0
    }
    /// Rebuilds a validated source from raw counts and their [`CountMapping`] (a cooked terrain
    /// index's mapping over its tiled counts, ADR-8471). Counts are kept as given.
    ///
    /// # Errors
    /// The mapping's own refusals ([`CountMapping::validate`]), a count total other than
    /// `cols * rows`, or a grid past `limits`.
    pub fn from_counts(
        counts: Vec<u16>,
        mapping: CountMapping,
        limits: ImportLimits,
    ) -> Result<Self, SourceError> {
        mapping.validate()?;
        Self::validated(
            [mapping.cols, mapping.rows],
            counts,
            mapping.meters_per_count,
            mapping.zero_counts,
            [mapping.cell_m; 2],
            mapping.sample0_m,
            limits.effective(),
        )
    }
    /// The datum, scale, cell and registration of this source with its grid size: what the cook
    /// persists in the terrain index (ADR-8471). Its `cell_m` is the X spacing: for a descriptor
    /// source with unequal spacing or missing samples use [`Self::count_mapping`], which refuses.
    #[must_use]
    pub fn mapping(&self) -> CountMapping {
        CountMapping {
            cols: self.width,
            rows: self.height,
            meters_per_count: self.meters_per_count,
            zero_counts: self.zero_counts,
            cell_m: self.cell_m,
            sample0_m: self.sample0,
        }
    }
    /// World X/Z of a sample: `sample0 + (col, row) * spacing` per axis, exact in `f64` and
    /// rounded to `f32` once ([`CountMapping::sample_world`] when the spacing is equal).
    #[must_use]
    pub fn sample_world(&self, col: u32, row: u32) -> [f32; 2] {
        let geometry = RasterGeometry {
            spacing_m: self.spacing_m(),
            sample0_m: self.sample0,
        };
        [geometry.sample_axis(0, col), geometry.sample_axis(1, row)]
    }
    /// Height at a clamped column/row, in meters relative to the datum. A sample without data
    /// is `f32::NAN`, never its count's height ([`Self::sample_height_m`]).
    pub fn height_m(&self, col: u32, row: u32) -> f32 {
        let (col, row) = (col.min(self.width - 1), row.min(self.height - 1));
        if self.valid.as_ref().is_some_and(|m| !m.is_valid(col, row)) {
            return f32::NAN;
        }
        let i = row as usize * self.width as usize + col as usize;
        (f32::from(self.counts[i]) - f32::from(self.zero_counts)) * self.meters_per_count
    }
    /// All samples converted to meters in canonical row order; samples without data are
    /// `f32::NAN`.
    pub fn heights_m(&self) -> Vec<f32> {
        let mut out: Vec<f32> = self
            .counts
            .iter()
            .map(|&v| (f32::from(v) - f32::from(self.zero_counts)) * self.meters_per_count)
            .collect();
        if let Some(mask) = &self.valid {
            for (i, flag) in mask.to_flags().into_iter().enumerate() {
                if !flag {
                    out[i] = f32::NAN;
                }
            }
        }
        out
    }
    /// Raw cooked grid for hosts that retain the source mapping separately.
    pub fn heightfield(&self) -> regia_asset::Heightfield {
        regia_asset::Heightfield {
            width: self.width,
            height: self.height,
            samples: self.counts.clone(),
        }
    }
}

fn check_bytes(bytes: &[u8], limits: ImportLimits) -> Result<(), SourceError> {
    if bytes.len() > limits.max_bytes {
        Err(invalid(
            "height_source_limit_bytes: encoded source exceeds max_bytes",
        ))
    } else {
        Ok(())
    }
}
fn check_dimensions([width, height]: [u32; 2], limits: ImportLimits) -> Result<usize, SourceError> {
    if width < 2 || height < 2 {
        return Err(invalid("grid must be at least 2x2"));
    }
    let count = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .filter(|&count| count <= limits.max_samples && count <= usize::MAX / 2)
        .ok_or_else(|| invalid("height_source_limit_samples: cols * rows must fit max_samples"))?;
    Ok(count)
}
