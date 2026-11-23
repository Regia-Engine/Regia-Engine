// SPDX-License-Identifier: MPL-2.0
//! The count mapping of a terrain source, carried through the cook, the cooked terrain index and
//! the level load (ADR-8471).
//!
//! A count source stores raw `u16` counts. Three numbers turn a count into meters (the datum
//! `zero_counts` and `meters_per_count`) and three place each sample in the world (`cell_m` and
//! the first sample's X/Z, the registration). [`CountMapping`] holds exactly those numbers and the
//! grid size, so the cook can persist them beside the verbatim tiled counts and the runtime can
//! rebuild the same meters and the same sample positions without rearranging the arithmetic.

use regia_asset::Heightfield;
use serde::{Deserialize, Serialize};

use super::{SourceError, invalid};
use crate::sampler::TerrainMap;

/// Exact datum, scale, cell size and registration of a rectangular count grid.
///
/// Heights are `(f32::from(count) - f32::from(zero_counts)) * meters_per_count`, in that order
/// (the [`super::TerrainSource`] formula). Sample `(col, row)` sits at world X/Z
/// `sample0_m + (col, row) * cell_m`, computed in `f64` and rounded to `f32` once
/// ([`Self::sample_world`]). Row 0 is minimum Z; column 0 is minimum X.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CountMapping {
    /// Sample columns (X), at least two.
    pub cols: u32,
    /// Sample rows (Z), at least two.
    pub rows: u32,
    /// Positive, finite meters per raw count.
    pub meters_per_count: f32,
    /// The count that is height zero (the datum).
    pub zero_counts: u16,
    /// Positive, finite distance between adjacent samples on both axes, meters.
    pub cell_m: f32,
    /// World X/Z of sample `(0, 0)`, meters.
    pub sample0_m: [f32; 2],
}

/// Length of [`CountMapping::canonical_bytes`].
pub const COUNT_MAPPING_CANONICAL_LEN: usize = 26;

impl CountMapping {
    /// The first-sample position a source gets when it names none: `-dimension * cell_m / 2` on
    /// each axis (the [`super::TerrainSource`] default, same `f32` expression).
    #[must_use]
    pub fn default_sample0(cols: u32, rows: u32, cell_m: f32) -> [f32; 2] {
        [-(cols as f32 * cell_m) * 0.5, -(rows as f32 * cell_m) * 0.5]
    }

    /// Checks the mapping: a grid of at least 2x2, a positive finite scale whose full count range
    /// is finite, a positive finite cell, a finite first sample and a finite last sample past it
    /// on both axes (in the `f32` expression [`super::TerrainSource`] validates and in
    /// [`Self::sample_world`]'s).
    ///
    /// # Errors
    /// A message naming the field.
    pub fn validate(&self) -> Result<(), SourceError> {
        if self.cols < 2 || self.rows < 2 {
            return Err(invalid("grid must be at least 2x2"));
        }
        let scale = self.meters_per_count;
        if !scale.is_finite() || scale <= 0.0 || !(65535.0 * scale).is_finite() {
            return Err(invalid(
                "meters_per_count must be positive with a finite full range",
            ));
        }
        let cell = self.cell_m;
        if !cell.is_finite() || cell <= 0.0 || self.sample0_m.iter().any(|v| !v.is_finite()) {
            return Err(invalid("cell_m must be positive and placement finite"));
        }
        for (axis, dimension) in [self.cols, self.rows].into_iter().enumerate() {
            let origin = self.sample0_m[axis];
            let end = origin + (dimension - 1) as f32 * cell;
            let exact = self.sample_axis(axis, dimension - 1);
            if !end.is_finite() || end <= origin || !exact.is_finite() || exact <= origin {
                return Err(invalid("sample extent must be finite and representable"));
            }
        }
        Ok(())
    }

    /// Height in meters of one raw count: `(count - zero_counts) * meters_per_count` in `f32`.
    #[must_use]
    pub fn height_m(&self, count: u16) -> f32 {
        (f32::from(count) - f32::from(self.zero_counts)) * self.meters_per_count
    }

    /// World coordinate of sample `i` on `axis` (0 = X, 1 = Z): `sample0 + i * cell_m`, exact in
    /// `f64` and rounded to `f32` once.
    #[must_use]
    pub fn sample_axis(&self, axis: usize, i: u32) -> f32 {
        let origin = self.sample0_m[axis.min(1)];
        (f64::from(origin) + f64::from(i) * f64::from(self.cell_m)) as f32
    }

    /// World X/Z of sample `(col, row)` ([`Self::sample_axis`] on each axis).
    #[must_use]
    pub fn sample_world(&self, col: u32, row: u32) -> [f32; 2] {
        [self.sample_axis(0, col), self.sample_axis(1, row)]
    }

    /// World X/Z of the first sample (the minimum corner).
    #[must_use]
    pub fn world_min(&self) -> [f32; 2] {
        self.sample0_m
    }

    /// World X/Z of the last sample (the maximum corner).
    #[must_use]
    pub fn world_max(&self) -> [f32; 2] {
        self.sample_world(self.cols.max(1) - 1, self.rows.max(1) - 1)
    }

    /// The platform-neutral bytes of the mapping, every value little-endian: `cols u32 | rows
    /// u32 | meters_per_count f32 bits | zero_counts u16 | cell_m f32 bits | sample0 X f32 bits |
    /// sample0 Z f32 bits`. A default first sample is resolved before this, so `1024`, `1024.0`
    /// and an omitted default hash alike.
    #[must_use]
    pub fn canonical_bytes(&self) -> [u8; COUNT_MAPPING_CANONICAL_LEN] {
        let mut out = [0u8; COUNT_MAPPING_CANONICAL_LEN];
        out[0..4].copy_from_slice(&self.cols.to_le_bytes());
        out[4..8].copy_from_slice(&self.rows.to_le_bytes());
        out[8..12].copy_from_slice(&self.meters_per_count.to_bits().to_le_bytes());
        out[12..14].copy_from_slice(&self.zero_counts.to_le_bytes());
        out[14..18].copy_from_slice(&self.cell_m.to_bits().to_le_bytes());
        out[18..22].copy_from_slice(&self.sample0_m[0].to_bits().to_le_bytes());
        out[22..26].copy_from_slice(&self.sample0_m[1].to_bits().to_le_bytes());
        out
    }

    /// Reads the count mapping a terrain's `heightfield.json` declares (`docs/formats/terrain.md`
    /// § 9): `"schema": 2` with the [`super::TerrainSource`] `grid` (`cols`, `rows`, `cell_m`,
    /// optional `sample0_x_m` / `sample0_z_m`, `row0` `south`, `col0` `west`) and `units`
    /// (`kind` `counts`, `meters_per_count`, `zero_counts`) objects. A document without `schema`
    /// is a legacy extents file: `Ok(None)`, whatever else it holds. Unknown fields are ignored.
    ///
    /// # Errors
    /// Malformed JSON, a schema other than 2, a missing or duplicated field, another orientation
    /// or unit, an invalid mapping ([`Self::validate`]), or a document that also gives the legacy
    /// `size_m` or `origin` (the registration would be given twice).
    pub fn from_heightfield_json(bytes: &[u8]) -> Result<Option<Self>, SourceError> {
        #[derive(Deserialize)]
        struct Grid {
            cols: u32,
            rows: u32,
            cell_m: f32,
            #[serde(default)]
            sample0_x_m: Option<f32>,
            #[serde(default)]
            sample0_z_m: Option<f32>,
            #[serde(default)]
            row0: Option<String>,
            #[serde(default)]
            col0: Option<String>,
        }
        #[derive(Deserialize)]
        struct Units {
            kind: String,
            meters_per_count: f32,
            zero_counts: u16,
        }
        #[derive(Deserialize)]
        struct Doc {
            #[serde(default)]
            schema: Option<u32>,
            #[serde(default)]
            grid: Option<Grid>,
            #[serde(default)]
            units: Option<Units>,
            #[serde(default)]
            size_m: Option<serde::de::IgnoredAny>,
            #[serde(default)]
            origin: Option<serde::de::IgnoredAny>,
        }
        #[derive(Deserialize)]
        struct Probe {
            #[serde(default)]
            schema: Option<serde::de::IgnoredAny>,
        }
        // Only `schema` decides: a legacy file's other keys are never read as a mapping.
        let probe: Probe = serde_json::from_slice(bytes).map_err(|e| invalid(e.to_string()))?;
        if probe.schema.is_none() {
            return Ok(None);
        }
        let doc: Doc = serde_json::from_slice(bytes).map_err(|e| invalid(e.to_string()))?;
        let Some(schema) = doc.schema else {
            return Ok(None);
        };
        if schema != 2 {
            return Err(invalid("heightfield.json: only schema 2 carries a count mapping"));
        }
        if doc.size_m.is_some() || doc.origin.is_some() {
            return Err(invalid(
                "heightfield.json: a schema-2 mapping replaces size_m and origin; give one registration",
            ));
        }
        let grid = doc.grid.ok_or_else(|| invalid("missing field `grid`"))?;
        let units = doc.units.ok_or_else(|| invalid("missing field `units`"))?;
        if grid.row0.as_deref().unwrap_or("south") != "south"
            || grid.col0.as_deref().unwrap_or("west") != "west"
        {
            return Err(invalid("row0 must be south and col0 west"));
        }
        if units.kind != "counts" {
            return Err(invalid("units.kind must be counts"));
        }
        let default = Self::default_sample0(grid.cols, grid.rows, grid.cell_m);
        let mapping = Self {
            cols: grid.cols,
            rows: grid.rows,
            meters_per_count: units.meters_per_count,
            zero_counts: units.zero_counts,
            cell_m: grid.cell_m,
            sample0_m: [
                grid.sample0_x_m.unwrap_or(default[0]),
                grid.sample0_z_m.unwrap_or(default[1]),
            ],
        };
        mapping.validate()?;
        Ok(Some(mapping))
    }

    /// The runtime [`TerrainMap`] of `heightfield`'s counts under this mapping, or a refusal when
    /// the runtime map cannot hold the source exactly. Nothing is resampled and no coordinate is
    /// rearranged: every height is [`Self::height_m`] of its count, bit for bit, and every
    /// sample's world position must equal [`Self::sample_world`] bit for bit.
    ///
    /// The runtime map is square in world space with one grid origin on X and Z
    /// ([`TerrainMap::with_grid_origin`], ADR-8179), so the source must be square (`cols ==
    /// rows`) and its center must be the same number on X and Z. The map's `height_scale` and
    /// `height_offset` are `65535 * meters_per_count` and `-zero_counts * meters_per_count`, so
    /// [`TerrainMap::to_heightfield`] gives the source counts back; that is checked too.
    ///
    /// # Errors
    /// `terrain_mapping_size: ...` when the heightfield is not the mapping's grid;
    /// `terrain_mapping_unrepresentable: ...` (agent code `level_terrain_mapping`) otherwise.
    pub fn terrain_map(&self, heightfield: &Heightfield) -> Result<TerrainMap, SourceError> {
        let _span = tracing::info_span!(target: "regia_terrain", "terrain.count_mapping_map").entered();
        self.validate()?;
        let expected = u64::from(self.cols) * u64::from(self.rows);
        if [heightfield.width, heightfield.height] != [self.cols, self.rows]
            || heightfield.samples.len() as u64 != expected
        {
            return Err(invalid(format!(
                "terrain_mapping_size: the heightfield is {}x{} with {} samples, the mapping {}x{}",
                heightfield.width,
                heightfield.height,
                heightfield.samples.len(),
                self.cols,
                self.rows
            )));
        }
        let unrepresentable = |why: String| {
            invalid(format!(
                "terrain_mapping_unrepresentable: {why}; nothing was resampled"
            ))
        };
        if self.cols != self.rows {
            return Err(unrepresentable(format!(
                "a {}x{} grid needs a world depth separate from its width",
                self.cols, self.rows
            )));
        }
        let n = self.cols;
        let last = n - 1;
        let world_width = (f64::from(last) * f64::from(self.cell_m)) as f32;
        let center = |axis: usize| {
            (f64::from(self.sample0_m[axis]) + 0.5 * f64::from(last) * f64::from(self.cell_m)) as f32
        };
        let origin = [center(0), center(1)];
        if origin[0].to_bits() != origin[1].to_bits() {
            return Err(unrepresentable(format!(
                "the grid center {origin:?} differs on X and Z"
            )));
        }
        let heights: Vec<f32> = heightfield.samples.iter().map(|&c| self.height_m(c)).collect();
        let scale = 65535.0 * self.meters_per_count;
        let offset = -(f32::from(self.zero_counts) * self.meters_per_count);
        let mut map = TerrainMap::from_heights(n, n, heights, world_width, scale, offset)
            .map_err(|e| unrepresentable(e.to_string()))?;
        if origin[0] != 0.0 {
            map = map
                .with_grid_origin(origin)
                .map_err(|e| unrepresentable(e.to_string()))?;
        }
        for i in 0..n {
            let got = map.sample_to_world(i, i);
            let want = self.sample_world(i, i);
            if got[0].to_bits() != want[0].to_bits() || got[1].to_bits() != want[1].to_bits() {
                return Err(unrepresentable(format!(
                    "sample {i} would sit at {got:?}, the source places it at {want:?}"
                )));
            }
        }
        let back = map.to_heightfield();
        if let Some(i) = back
            .samples
            .iter()
            .zip(&heightfield.samples)
            .position(|(a, b)| a != b)
        {
            return Err(unrepresentable(format!(
                "count {} at sample {i} would export as {}",
                heightfield.samples[i], back.samples[i]
            )));
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping() -> CountMapping {
        CountMapping {
            cols: 5,
            rows: 5,
            meters_per_count: 0.125,
            zero_counts: 1000,
            cell_m: 2.0,
            sample0_m: [30.0, 30.0],
        }
    }

    #[test]
    fn a_legacy_extents_file_has_no_mapping() {
        assert_eq!(
            CountMapping::from_heightfield_json(br#"{"size_m": 64, "origin": [0, 1, 0]}"#).expect("legacy"),
            None
        );
        assert_eq!(CountMapping::from_heightfield_json(b"{}").expect("empty"), None);
    }

    #[test]
    fn a_schema_2_document_resolves_its_default_first_sample() {
        let json = br#"{"schema":2,"grid":{"cols":3,"rows":2,"cell_m":2.0},
            "units":{"kind":"counts","meters_per_count":0.125,"zero_counts":1000}}"#;
        let m = CountMapping::from_heightfield_json(json)
            .expect("parse")
            .expect("mapping");
        assert_eq!(m.sample0_m, [-3.0, -2.0]);
        assert_eq!(m.world_max(), [1.0, 0.0]);
        assert_eq!(m.height_m(992), -1.0);
    }

    #[test]
    fn conflicting_or_invalid_documents_are_refused() {
        for bad in [
            &br#"{"schema":1}"#[..],
            br#"{"schema":2,"size_m":8,"grid":{"cols":2,"rows":2,"cell_m":1},"units":{"kind":"counts","meters_per_count":1,"zero_counts":0}}"#,
            br#"{"schema":2,"grid":{"cols":2,"rows":2,"cell_m":1}}"#,
            br#"{"schema":2,"grid":{"cols":2,"rows":2,"cell_m":0},"units":{"kind":"counts","meters_per_count":1,"zero_counts":0}}"#,
            br#"{"schema":2,"grid":{"cols":2,"rows":2,"cell_m":1,"row0":"north"},"units":{"kind":"counts","meters_per_count":1,"zero_counts":0}}"#,
            br#"{"schema":2,"grid":{"cols":2,"rows":2,"cell_m":1},"units":{"kind":"meters","meters_per_count":1,"zero_counts":0}}"#,
            br#"{"schema":2,"schema":2}"#,
        ] {
            assert!(
                CountMapping::from_heightfield_json(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn the_runtime_map_holds_the_counts_and_positions_exactly() {
        let m = mapping();
        let samples: Vec<u16> = (0..25).map(|i| 990 + i as u16 * 3).collect();
        let hf = Heightfield {
            width: 5,
            height: 5,
            samples: samples.clone(),
        };
        let map = m.terrain_map(&hf).expect("square, equal center");
        for row in 0..5u32 {
            for col in 0..5u32 {
                let i = (row * 5 + col) as usize;
                assert_eq!(
                    map.height_at_sample(i64::from(col), i64::from(row)).to_bits(),
                    m.height_m(samples[i]).to_bits()
                );
                assert_eq!(map.sample_to_world(col, row), m.sample_world(col, row));
            }
        }
        assert_eq!(map.to_heightfield().samples, samples);
        assert_eq!(map.grid_origin(), [34.0, 34.0]);
    }

    #[test]
    fn unrepresentable_grids_are_refused_not_resampled() {
        let rect = CountMapping { rows: 3, ..mapping() };
        let hf = Heightfield {
            width: 5,
            height: 3,
            samples: vec![1000; 15],
        };
        let err = rect.terrain_map(&hf).expect_err("rectangular");
        assert!(err.0.contains("terrain_mapping_unrepresentable"), "{err}");
        let skew = CountMapping {
            sample0_m: [30.0, 31.0],
            ..mapping()
        };
        let square = Heightfield {
            width: 5,
            height: 5,
            samples: vec![1000; 25],
        };
        let err = skew.terrain_map(&square).expect_err("unequal center");
        assert!(err.0.contains("differs on X and Z"), "{err}");
        let err = mapping().terrain_map(&hf).expect_err("wrong grid");
        assert!(err.0.starts_with("terrain_mapping_size"), "{err}");
    }
}
