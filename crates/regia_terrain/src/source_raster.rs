// SPDX-License-Identifier: MPL-2.0
//! The engine-owned raster descriptor of a count source (CT-21, ADR-8542).
//!
//! A count raster is described by the spacing of its samples on X and Z, which corner it stores
//! first, whether its origin names the first sample (sample-center registration) or the outer
//! edge of the first cell (cell-edge registration), and which samples hold data. Terms follow the
//! public GDAL raster data model (the affine geotransform and the `AREA_OR_POINT` metadata item);
//! no GDAL code, dependency or reprojection is involved. Original Regia Rust.
//!
//! The descriptor never resamples: a row or column order other than south/west is normalized by
//! an exact index permutation, and missing samples stay missing until a caller selects an explicit
//! policy ([`MissingDataPolicy`]). External schema-2 JSON is unchanged: it maps to
//! [`RasterDescriptor::legacy`].

use serde::{Deserialize, Serialize};

use super::{ImportLimits, SourceError, check_bytes, invalid};

/// The raster descriptor version this build reads (`"raster_descriptor": 1`).
pub const RASTER_DESCRIPTOR_VERSION: u32 = 1;

/// Which edge the stored rows start from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowOrder {
    /// Row 0 is minimum Z (the canonical order and the schema-2 default).
    #[default]
    South,
    /// Row 0 is maximum Z (a north-up image's top row).
    North,
}

/// Which edge the stored columns start from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColOrder {
    /// Column 0 is minimum X (the canonical order and the schema-2 default).
    #[default]
    West,
    /// Column 0 is maximum X.
    East,
}

/// What the descriptor's origin names (GDAL's `AREA_OR_POINT`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RasterRegistration {
    /// The origin is the position of the minimum-X/minimum-Z sample (`Point`; schema 2).
    #[default]
    Center,
    /// The origin is the outer corner of the minimum-X/minimum-Z cell (`Area`): every sample
    /// sits half a spacing inside it on each axis.
    Edge,
}

/// What a terrain map does with samples that hold no data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingDataPolicy {
    /// Refuse a source with any missing sample (`terrain_raster_missing_data`). The default.
    #[default]
    Refuse,
    /// Cut a terrain hole ([`crate::holes::HoleShape::Raster`]) over every missing sample. The
    /// samples under the hole are stored at the datum (`zero_counts`), which no query reads.
    Holes,
}

/// A valid-data mask as a descriptor stores it: `valid[row * cols + col]` is `1` for a sample
/// with data and `0` for one without, in the raster's stored order (the same order as its counts).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RasterMaskSpec {
    /// Mask columns; must equal the raster's.
    pub cols: u32,
    /// Mask rows; must equal the raster's.
    pub rows: u32,
    /// `cols * rows` entries, each `0` or `1`.
    pub valid: Vec<u8>,
}

/// The engine-owned, versioned description of how a count raster sits in the world.
///
/// JSON form (`docs/formats/terrain-raster-descriptor.schema.json`):
///
/// ```json
/// {"raster_descriptor": 1, "spacing_m": [2.0, 0.5], "row0": "north", "col0": "west",
///  "registration": "edge", "origin_m": [100.0, -40.0], "nodata_counts": 65535}
/// ```
///
/// `origin_m` names the minimum-X/minimum-Z corner whichever corner the raster stores first;
/// `registration` is required in JSON so a half-cell shift is never implied.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RasterDescriptor {
    /// Descriptor version; this build reads [`RASTER_DESCRIPTOR_VERSION`].
    #[serde(rename = "raster_descriptor")]
    pub version: u32,
    /// Positive, finite distance between adjacent samples on X and on Z, meters.
    pub spacing_m: [f32; 2],
    /// GDAL's rotation terms (X change per row, Z change per column), meters. Only `[0, 0]`
    /// (axis-aligned) is supported; anything else is refused with `terrain_raster_transform`.
    #[serde(default)]
    pub skew_m: [f32; 2],
    /// Which edge stored row 0 is.
    #[serde(default)]
    pub row0: RowOrder,
    /// Which edge stored column 0 is.
    #[serde(default)]
    pub col0: ColOrder,
    /// What `origin_m` names.
    pub registration: RasterRegistration,
    /// World X/Z of the minimum corner's registration point. Absent: the schema-2 default
    /// `-dimension * spacing / 2` (the sample for center registration, the cell edge for edge).
    #[serde(default)]
    pub origin_m: Option<[f32; 2]>,
    /// A count that means "no data" (GDAL's nodata value). Such samples are missing.
    #[serde(default)]
    pub nodata_counts: Option<u16>,
    /// An explicit valid-data mask, in stored order.
    #[serde(default)]
    pub valid_mask: Option<RasterMaskSpec>,
}

/// The geometry a descriptor resolves to for one grid size, in canonical (south/west) order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RasterGeometry {
    /// Spacing on X and Z, meters.
    pub spacing_m: [f32; 2],
    /// World X/Z of canonical sample `(0, 0)` (the minimum corner), meters.
    pub sample0_m: [f32; 2],
}

/// How a source's samples were stored and registered before canonicalization (reports only;
/// the source itself is always canonical).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RasterLayout {
    /// Descriptor version that described the source; `None` for a schema-2/PNG source as decoded.
    pub version: Option<u32>,
    /// Declared row order.
    pub row0: RowOrder,
    /// Declared column order.
    pub col0: ColOrder,
    /// Declared registration.
    pub registration: RasterRegistration,
}

impl RasterLayout {
    /// The layout of a source as schema 2 or a PNG import decodes it.
    pub const LEGACY: Self = Self {
        version: None,
        row0: RowOrder::South,
        col0: ColOrder::West,
        registration: RasterRegistration::Center,
    };
}

/// A packed valid-data mask in canonical order (row 0 south, column 0 west).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ValidMask {
    cols: u32,
    rows: u32,
    bits: Vec<u64>,
    invalid: usize,
}

impl ValidMask {
    /// A mask from `cols * rows` row-major flags (`true` = has data).
    ///
    /// # Errors
    /// `terrain_raster_mask` when the flag count is not `cols * rows`.
    pub fn from_flags(cols: u32, rows: u32, flags: &[bool]) -> Result<Self, SourceError> {
        let len = (cols as usize).checked_mul(rows as usize);
        if len != Some(flags.len()) {
            return Err(invalid(format!(
                "terrain_raster_mask: {} flags for a {cols}x{rows} mask",
                flags.len()
            )));
        }
        let mut bits = vec![0u64; flags.len().div_ceil(64)];
        let mut invalid_count = 0;
        for (i, &valid) in flags.iter().enumerate() {
            if valid {
                bits[i / 64] |= 1 << (i % 64);
            } else {
                invalid_count += 1;
            }
        }
        Ok(Self {
            cols,
            rows,
            bits,
            invalid: invalid_count,
        })
    }

    /// `[columns, rows]`.
    #[must_use]
    pub fn dimensions(&self) -> [u32; 2] {
        [self.cols, self.rows]
    }

    /// Whether sample `(col, row)` holds data; `false` outside the mask.
    #[must_use]
    pub fn is_valid(&self, col: u32, row: u32) -> bool {
        if col >= self.cols || row >= self.rows {
            return false;
        }
        self.flag(row as usize * self.cols as usize + col as usize)
    }

    fn flag(&self, i: usize) -> bool {
        self.bits.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }

    /// Row-major flags, the inverse of [`Self::from_flags`].
    #[must_use]
    pub fn to_flags(&self) -> Vec<bool> {
        (0..self.cols as usize * self.rows as usize)
            .map(|i| self.flag(i))
            .collect()
    }

    /// Samples without data.
    #[must_use]
    pub fn invalid_count(&self) -> usize {
        self.invalid
    }

    /// The flags packed eight to a byte, row-major, least significant bit first (`1` = has data),
    /// `ceil(cols * rows / 8)` bytes with the unused bits of the last byte zero: the cooked
    /// terrain index's `source_valid_mask` (version 3, ADR-8871).
    #[must_use]
    pub fn to_packed_bytes(&self) -> Vec<u8> {
        let len = (self.cols as usize * self.rows as usize).div_ceil(8);
        let mut out = Vec::with_capacity(len);
        for word in &self.bits {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// The inverse of [`Self::to_packed_bytes`].
    ///
    /// # Errors
    /// `terrain_raster_mask` for a byte count other than `ceil(cols * rows / 8)` or a set bit past
    /// the last sample (one mask has one encoding).
    pub fn from_packed_bytes(cols: u32, rows: u32, bytes: &[u8]) -> Result<Self, SourceError> {
        let samples = u64::from(cols) * u64::from(rows);
        if samples.div_ceil(8) != bytes.len() as u64 {
            return Err(invalid(format!(
                "terrain_raster_mask: {} packed bytes for a {cols}x{rows} mask",
                bytes.len()
            )));
        }
        let samples = samples as usize;
        let tail = samples % 8;
        if tail != 0 && bytes.last().is_some_and(|&b| b >> tail != 0) {
            return Err(invalid("terrain_raster_mask: a bit past the last sample is set"));
        }
        let mut bits = vec![0u64; samples.div_ceil(64)];
        for (word, chunk) in bits.iter_mut().zip(bytes.chunks(8)) {
            let mut le = [0u8; 8];
            le[..chunk.len()].copy_from_slice(chunk);
            *word = u64::from_le_bytes(le);
        }
        let valid: usize = bits.iter().map(|w| w.count_ones() as usize).sum();
        Ok(Self {
            cols,
            rows,
            bits,
            invalid: samples - valid,
        })
    }
}

/// Index in stored order of canonical sample `(col, row)` for a raster stored as `row0`/`col0`.
#[must_use]
pub fn stored_index(dimensions: [u32; 2], row0: RowOrder, col0: ColOrder, col: u32, row: u32) -> usize {
    let [cols, rows] = dimensions;
    let sc = match col0 {
        ColOrder::West => col,
        ColOrder::East => cols - 1 - col,
    };
    let sr = match row0 {
        RowOrder::South => row,
        RowOrder::North => rows - 1 - row,
    };
    sr as usize * cols as usize + sc as usize
}

/// Reorders stored-order values into canonical order by exact index permutation (no
/// interpolation). South/west input is returned as is, without a copy.
pub(super) fn to_canonical<T: Copy>(
    values: Vec<T>,
    dimensions: [u32; 2],
    row0: RowOrder,
    col0: ColOrder,
) -> Vec<T> {
    if row0 == RowOrder::South && col0 == ColOrder::West {
        return values;
    }
    let [cols, rows] = dimensions;
    let mut out = Vec::with_capacity(values.len());
    for row in 0..rows {
        for col in 0..cols {
            out.push(values[stored_index(dimensions, row0, col0, col, row)]);
        }
    }
    out
}

impl RasterDescriptor {
    /// The descriptor a schema-2 document or a PNG import implies: equal spacing `cell_m`,
    /// south/west order, center registration, the given (or default) first sample, all valid.
    #[must_use]
    pub fn legacy(cell_m: f32, sample0: Option<[f32; 2]>) -> Self {
        Self {
            version: RASTER_DESCRIPTOR_VERSION,
            spacing_m: [cell_m, cell_m],
            skew_m: [0.0, 0.0],
            row0: RowOrder::South,
            col0: ColOrder::West,
            registration: RasterRegistration::Center,
            origin_m: sample0,
            nodata_counts: None,
            valid_mask: None,
        }
    }

    /// Parses a descriptor document, bounded by `limits.max_bytes` and (for a mask)
    /// `limits.max_samples`. Unknown fields are refused.
    ///
    /// # Errors
    /// `height_source_limit_bytes`, malformed JSON (`height_io_decode`), or any
    /// [`Self::validate`] refusal.
    pub fn from_json(bytes: &[u8], limits: ImportLimits) -> Result<Self, SourceError> {
        let limits = limits.effective();
        check_bytes(bytes, limits)?;
        // The version decides before anything else is read, so a future document is refused
        // with its own code rather than as an unknown field.
        let version: VersionProbe =
            serde_json::from_slice(bytes).map_err(|e| invalid(format!("raster descriptor: {e}")))?;
        if version.raster_descriptor != RASTER_DESCRIPTOR_VERSION {
            return Err(invalid(format!(
                "terrain_raster_version: raster_descriptor {} is not supported (this build reads {RASTER_DESCRIPTOR_VERSION})",
                version.raster_descriptor
            )));
        }
        let descriptor: Self =
            serde_json::from_slice(bytes).map_err(|e| invalid(format!("raster descriptor: {e}")))?;
        if descriptor
            .valid_mask
            .as_ref()
            .is_some_and(|m| m.valid.len() > limits.max_samples)
        {
            return Err(invalid(
                "height_source_limit_samples: the valid mask exceeds max_samples",
            ));
        }
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// Checks the version, the transform, the spacing and the origin.
    ///
    /// # Errors
    /// `terrain_raster_version`, `terrain_raster_transform` (a rotated or sheared raster), or a
    /// nonpositive/nonfinite spacing or nonfinite origin (`height_io_decode`).
    pub fn validate(&self) -> Result<(), SourceError> {
        if self.version != RASTER_DESCRIPTOR_VERSION {
            return Err(invalid(format!(
                "terrain_raster_version: raster_descriptor {} is not supported (this build reads {RASTER_DESCRIPTOR_VERSION})",
                self.version
            )));
        }
        if self.skew_m != [0.0, 0.0] {
            return Err(invalid(format!(
                "terrain_raster_transform: skew_m {:?} describes a rotated or sheared raster; only axis-aligned rasters are supported and nothing is resampled",
                self.skew_m
            )));
        }
        if self.spacing_m.iter().any(|s| !s.is_finite() || *s <= 0.0) {
            return Err(invalid(
                "raster descriptor: spacing_m must be positive and finite",
            ));
        }
        if self.origin_m.is_some_and(|o| o.iter().any(|v| !v.is_finite())) {
            return Err(invalid("raster descriptor: origin_m must be finite"));
        }
        Ok(())
    }

    /// True when this descriptor places and reads samples exactly as schema 2 does (equal
    /// spacing, south/west, center registration, no mask and no nodata value).
    #[must_use]
    pub fn is_legacy_layout(&self) -> bool {
        self.spacing_m[0].to_bits() == self.spacing_m[1].to_bits()
            && self.skew_m == [0.0, 0.0]
            && self.row0 == RowOrder::South
            && self.col0 == ColOrder::West
            && self.registration == RasterRegistration::Center
            && self.nodata_counts.is_none()
            && self.valid_mask.is_none()
    }

    /// The canonical spacing and first sample for a `[cols, rows]` grid, with the extent checks
    /// [`super::TerrainSource`] applies.
    ///
    /// # Errors
    /// [`Self::validate`]'s refusals or an extent that is not finite and increasing.
    pub fn geometry(&self, dimensions: [u32; 2]) -> Result<RasterGeometry, SourceError> {
        self.validate()?;
        let sample0: [f32; 2] = std::array::from_fn(|axis| {
            let spacing = self.spacing_m[axis];
            // The legacy default, the same `f32` expression as schema 2.
            let reference = self
                .origin_m
                .map_or_else(|| -(dimensions[axis] as f32 * spacing) * 0.5, |o| o[axis]);
            match self.registration {
                RasterRegistration::Center => reference,
                RasterRegistration::Edge => (f64::from(reference) + 0.5 * f64::from(spacing)) as f32,
            }
        });
        let geometry = RasterGeometry {
            spacing_m: self.spacing_m,
            sample0_m: sample0,
        };
        for (axis, ((&origin, &dimension), &spacing)) in
            sample0.iter().zip(&dimensions).zip(&self.spacing_m).enumerate()
        {
            let last = dimension.saturating_sub(1);
            let end = origin + last as f32 * spacing;
            let exact = geometry.sample_axis(axis, last);
            if !origin.is_finite()
                || !end.is_finite()
                || end <= origin
                || !exact.is_finite()
                || exact <= origin
            {
                return Err(invalid("sample extent must be finite and representable"));
            }
        }
        Ok(geometry)
    }

    /// The stored-order valid flags for `counts` (mask and nodata value combined), or `None`
    /// when every sample holds data.
    ///
    /// # Errors
    /// `terrain_raster_mask` when the mask's size is not the raster's or an entry is not 0/1.
    pub fn stored_validity(
        &self,
        dimensions: [u32; 2],
        counts: &[u16],
    ) -> Result<Option<Vec<bool>>, SourceError> {
        let mut flags: Option<Vec<bool>> = None;
        if let Some(mask) = &self.valid_mask {
            if [mask.cols, mask.rows] != dimensions {
                return Err(invalid(format!(
                    "terrain_raster_mask: the mask is {}x{} but the raster is {}x{}",
                    mask.cols, mask.rows, dimensions[0], dimensions[1]
                )));
            }
            if mask.valid.len() != counts.len() {
                return Err(invalid(format!(
                    "terrain_raster_mask: the mask has {} entries for {} samples",
                    mask.valid.len(),
                    counts.len()
                )));
            }
            if let Some(i) = mask.valid.iter().position(|&v| v > 1) {
                return Err(invalid(format!(
                    "terrain_raster_mask: entry {i} is {}; entries are 0 or 1",
                    mask.valid[i]
                )));
            }
            flags = Some(mask.valid.iter().map(|&v| v == 1).collect());
        }
        if let Some(nodata) = self.nodata_counts {
            let f = flags.get_or_insert_with(|| vec![true; counts.len()]);
            for (flag, &count) in f.iter_mut().zip(counts) {
                *flag &= count != nodata;
            }
        }
        Ok(flags)
    }
}

#[derive(Deserialize)]
struct VersionProbe {
    raster_descriptor: u32,
}

/// The `raster_descriptor` version of a descriptor document, read before the typed parse and
/// without validating anything else (`regia upgrade-project`, ADR-8639).
///
/// # Errors
/// Text that is not a JSON object with an unsigned `raster_descriptor` key.
pub fn raster_descriptor_file_version(bytes: &[u8]) -> Result<u32, SourceError> {
    serde_json::from_slice::<VersionProbe>(bytes)
        .map(|probe| probe.raster_descriptor)
        .map_err(|e| invalid(format!("raster descriptor: {e}")))
}

/// The serde shape of the raster descriptor, frozen as `terrain-raster-descriptor` in
/// `docs/formats/format-hashes.ron` (`cargo xtask check-formats`, ADR-8639): the document, its
/// mask and the variant names of every enum it reaches. [`MissingDataPolicy`] is a caller's
/// choice, not a field, so it is not part of the shape.
#[must_use]
pub fn raster_descriptor_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names as f, serde_variant_names as v};
    join_field_lists(&[
        f::<RasterDescriptor>(),
        f::<RasterMaskSpec>(),
        v::<RowOrder>(),
        v::<ColOrder>(),
        v::<RasterRegistration>(),
    ])
}

/// The raster descriptor is at its first version: no migration steps.
#[must_use]
pub fn raster_descriptor_migration_sources() -> Vec<u32> {
    Vec::new()
}

impl RasterGeometry {
    /// World coordinate of canonical sample `i` on `axis` (0 = X, 1 = Z): `sample0 + i *
    /// spacing`, exact in `f64` and rounded to `f32` once (the [`super::CountMapping`] rule).
    #[must_use]
    pub fn sample_axis(&self, axis: usize, i: u32) -> f32 {
        let axis = axis.min(1);
        (f64::from(self.sample0_m[axis]) + f64::from(i) * f64::from(self.spacing_m[axis])) as f32
    }
}
