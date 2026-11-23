// SPDX-License-Identifier: MPL-2.0
//! Heightmap export and import (`docs/specs/82-terrain-editing.md` § 3, ADR-1853): a sculpted
//! [`TerrainMap`] back to 16-bit samples for an authoring file, and a 16-bit file onto a live
//! terrain as one undoable edit.
//!
//! The 16-bit encoding is the map's own grid, `round((h − offset) · 65535 / scale)` clamped
//! ([`TerrainMap::to_heightfield`]); an import uses [`TerrainMap::new`]'s inverse, so export then
//! import is the identity on every sample a save could hold. Row 0 is the map's row 0 (the −Z
//! edge), the order the cooker's PNG import reads. RAW (`.r16`) files are headerless row-major
//! unsigned 16-bit samples; PNG encoding lives in `regia_agent`, which links `png`.

use std::collections::BTreeSet;

use glam::IVec2;
use regia_asset::Heightfield;

use crate::Terrain;
use crate::brush::{Brush, BrushMode, StrokeRecord, StrokeSample, TileCoord, union_rect};
use crate::deform::DirtyRect;
use crate::edit::{EDIT_CELL_SIZE, StrokeUndo, TerrainEdits};
use crate::sampler::TerrainMap;

/// Most changed samples an import keeps an undo record for (16 bytes each: 64 MiB). A larger
/// import still applies; [`HeightImport::undo`] is then `None`.
pub const MAX_IMPORT_UNDO_SAMPLES: usize = 1 << 22;

/// Byte order of a RAW 16-bit heightmap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RawEndian {
    /// Least significant byte first (the common `.r16` layout; the default).
    #[default]
    Little,
    /// Most significant byte first.
    Big,
}

/// Errors from heightmap encoding and import.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HeightIoError {
    /// The byte count is not `width × height × 2`.
    #[error("a {width}x{height} RAW heightmap needs {expected} bytes, the file has {actual}")]
    RawLength {
        /// Columns asked for.
        width: u32,
        /// Rows asked for.
        height: u32,
        /// Bytes expected.
        expected: u64,
        /// Bytes found.
        actual: u64,
    },
    /// The grid is empty or not 2×2 or larger.
    #[error("a heightmap needs at least 2x2 samples, got {width}x{height}")]
    TooSmall {
        /// Columns.
        width: u32,
        /// Rows.
        height: u32,
    },
    /// The file's grid is not the terrain's grid.
    #[error("the heightmap is {file:?} samples but the terrain is {map:?}")]
    SizeMismatch {
        /// `[columns, rows]` of the file.
        file: [u32; 2],
        /// `[columns, rows]` of the terrain.
        map: [u32; 2],
    },
    /// The heightfield's sample count does not match its size.
    #[error("the heightfield holds {actual} samples, {expected} expected")]
    SampleCount {
        /// `width × height`.
        expected: u64,
        /// Samples held.
        actual: u64,
    },
}

impl HeightIoError {
    /// Stable lowercase code for the agent protocol (`book/agents/terrain-editing.md`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            HeightIoError::RawLength { .. }
            | HeightIoError::TooSmall { .. }
            | HeightIoError::SampleCount { .. } => "height_io_decode",
            HeightIoError::SizeMismatch { .. } => "height_io_size_mismatch",
        }
    }
}

/// Encodes `hf` as headerless row-major 16-bit samples.
#[must_use]
pub fn encode_r16(hf: &Heightfield, endian: RawEndian) -> Vec<u8> {
    let mut out = Vec::with_capacity(hf.samples.len() * 2);
    for &s in &hf.samples {
        out.extend_from_slice(&match endian {
            RawEndian::Little => s.to_le_bytes(),
            RawEndian::Big => s.to_be_bytes(),
        });
    }
    out
}

/// Decodes headerless row-major 16-bit samples into a `width × height` heightfield.
///
/// # Errors
///
/// [`HeightIoError::TooSmall`] below 2×2, [`HeightIoError::RawLength`] when the byte count is
/// not `width × height × 2`.
pub fn decode_r16(
    bytes: &[u8],
    width: u32,
    height: u32,
    endian: RawEndian,
) -> Result<Heightfield, HeightIoError> {
    if width < 2 || height < 2 {
        return Err(HeightIoError::TooSmall { width, height });
    }
    let expected = u64::from(width) * u64::from(height) * 2;
    if bytes.len() as u64 != expected {
        return Err(HeightIoError::RawLength {
            width,
            height,
            expected,
            actual: bytes.len() as u64,
        });
    }
    let (pairs, _) = bytes.as_chunks::<2>();
    let samples = pairs
        .iter()
        .map(|&b| match endian {
            RawEndian::Little => u16::from_le_bytes(b),
            RawEndian::Big => u16::from_be_bytes(b),
        })
        .collect();
    Ok(Heightfield {
        width,
        height,
        samples,
    })
}

/// The side of a square grid stored in `byte_len` bytes of 16-bit samples (a RAW file without a
/// size), or `None` when `byte_len` is not `2 · n²` for some `n ≥ 2`.
#[must_use]
pub fn square_side(byte_len: usize) -> Option<u32> {
    if !byte_len.is_multiple_of(2) {
        return None;
    }
    let samples = (byte_len / 2) as u64;
    let n = (samples as f64).sqrt().round() as u64;
    (n >= 2 && n * n == samples)
        .then(|| u32::try_from(n).ok())
        .flatten()
}

/// A map quantised for a file, with how many samples the 16-bit range clipped.
#[derive(Clone, Debug, PartialEq)]
pub struct HeightExport {
    /// The map's 16-bit grid ([`TerrainMap::to_heightfield`]).
    pub heightfield: Heightfield,
    /// Samples below the map's height range (written as 0).
    pub clamped_low: u64,
    /// Samples above the map's height range (written as 65535).
    pub clamped_high: u64,
}

/// Quantises `map` for export and counts the samples sculpted past its height range.
#[must_use]
pub fn export_heights(map: &TerrainMap) -> HeightExport {
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.export_heights").entered();
    let scale = map.height_scale();
    let offset = map.height_offset();
    let inv = if scale.abs() > f32::EPSILON {
        65535.0 / scale
    } else {
        0.0
    };
    let (mut clamped_low, mut clamped_high) = (0u64, 0u64);
    for &h in map.heights() {
        let raw = ((h - offset) * inv).round();
        clamped_low += u64::from(raw < 0.0);
        clamped_high += u64::from(raw > 65535.0);
    }
    HeightExport {
        heightfield: map.to_heightfield(),
        clamped_low,
        clamped_high,
    }
}

/// What [`import_heights`] did.
#[derive(Debug)]
pub struct HeightImport {
    /// Samples whose height changed.
    pub changed: u64,
    /// Bounds of the changed samples (empty when nothing changed).
    pub dirty: DirtyRect,
    /// The import as one undo step (`None` above [`MAX_IMPORT_UNDO_SAMPLES`] changed samples or
    /// when nothing changed).
    pub undo: Option<StrokeUndo>,
}

/// Writes `hf` onto `terrain`'s heights (dequantised with the map's scale and offset), notes the
/// change on `edits` (GPU upload, min/max pyramid, unsaved cells) and returns the undo record.
///
/// # Errors
///
/// [`HeightIoError::SizeMismatch`] when `hf` is not the map's grid, [`HeightIoError::SampleCount`]
/// when `hf` holds the wrong number of samples. The terrain is untouched on error.
pub fn import_heights(
    terrain: &mut Terrain,
    edits: &mut TerrainEdits,
    hf: &Heightfield,
) -> Result<HeightImport, HeightIoError> {
    import_with_undo_cap(terrain, edits, hf, MAX_IMPORT_UNDO_SAMPLES)
}

/// [`import_heights`] keeping an undo record of at most `undo_cap` changed samples (ADR-8541): a
/// host that discloses a lower undo limit before it applies an import (the editor's Terrain ▸
/// Import source) passes it here, so the limit it showed is the one applied. `undo_cap` above
/// [`MAX_IMPORT_UNDO_SAMPLES`] is lowered to it.
///
/// # Errors
///
/// As [`import_heights`]; the terrain is untouched on error.
pub fn import_heights_with_undo_cap(
    terrain: &mut Terrain,
    edits: &mut TerrainEdits,
    hf: &Heightfield,
    undo_cap: usize,
) -> Result<HeightImport, HeightIoError> {
    import_with_undo_cap(terrain, edits, hf, undo_cap.min(MAX_IMPORT_UNDO_SAMPLES))
}

/// [`import_heights`] keeping an undo record of at most `undo_cap` changed samples.
fn import_with_undo_cap(
    terrain: &mut Terrain,
    edits: &mut TerrainEdits,
    hf: &Heightfield,
    undo_cap: usize,
) -> Result<HeightImport, HeightIoError> {
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.import_heights").entered();
    let map = &mut terrain.map;
    let (w, h) = (map.width(), map.height());
    if [hf.width, hf.height] != [w, h] {
        return Err(HeightIoError::SizeMismatch {
            file: [hf.width, hf.height],
            map: [w, h],
        });
    }
    let expected = u64::from(w) * u64::from(h);
    if hf.samples.len() as u64 != expected {
        return Err(HeightIoError::SampleCount {
            expected,
            actual: hf.samples.len() as u64,
        });
    }
    let (scale, offset) = (map.height_scale(), map.height_offset());
    let mut samples: Vec<StrokeSample<f32>> = Vec::new();
    let mut keep_undo = true;
    let mut changed = 0u64;
    let mut rect: Option<DirtyRect> = None;
    let mut tiles: BTreeSet<TileCoord> = BTreeSet::new();
    let mut last_cell: Option<TileCoord> = None;
    for (i, &s) in hf.samples.iter().enumerate() {
        let after = f32::from(s) / 65535.0 * scale + offset;
        let before = map.height_at_index(i);
        if before.to_bits() == after.to_bits() {
            continue;
        }
        map.set_height(i, after);
        changed += 1;
        let (col, row) = ((i % w as usize) as u32, (i / w as usize) as u32);
        let one = DirtyRect {
            col_start: col,
            col_end: col + 1,
            row_start: row,
            row_end: row + 1,
        };
        rect = Some(rect.map_or(one, |r| union_rect(r, one)));
        // Row by row most changed samples share the previous sample's cell.
        let cell = (col / EDIT_CELL_SIZE, row / EDIT_CELL_SIZE);
        if last_cell != Some(cell) {
            tiles.insert(cell);
            last_cell = Some(cell);
        }
        if keep_undo {
            if samples.len() >= undo_cap {
                keep_undo = false;
                // Free the abandoned undo buffer now (up to the cap's 64 MiB), not when the import returns.
                drop(std::mem::take(&mut samples));
            } else {
                samples.push(StrokeSample {
                    position: IVec2::new(col as i32, row as i32),
                    before,
                    after,
                });
            }
        }
    }
    let dirty = rect.unwrap_or(DirtyRect {
        col_start: 0,
        col_end: 0,
        row_start: 0,
        row_end: 0,
    });
    edits.note(&terrain.map, &mut terrain.minmax, dirty, &tiles);
    let undo = (keep_undo && changed > 0).then(|| {
        StrokeUndo::Height(StrokeRecord {
            brush: Brush {
                mode: BrushMode::Assign { value: 0.0 },
                ..Brush::default()
            },
            samples,
            dirty,
            tiles,
        })
    });
    Ok(HeightImport { changed, dirty, undo })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terrain(map: TerrainMap) -> Terrain {
        let minmax = crate::MinMaxPyramid::build(&map, 2).expect("pyramid");
        Terrain {
            map,
            cdlod: crate::CdlodConfig::default(),
            root_level: 2,
            minmax,
            layers: crate::TerrainLayers::default(),
            holes: None,
            water: None,
        }
    }

    fn ramp_map(n: u32) -> TerrainMap {
        let heights = (0..n * n)
            .map(|i| (i % n) as f32 * 0.5 + (i / n) as f32 * 0.25)
            .collect();
        TerrainMap::from_heights(n, n, heights, 64.0, 100.0, -20.0).expect("map")
    }

    #[test]
    fn r16_round_trips_in_both_byte_orders() {
        let hf = Heightfield {
            width: 3,
            height: 2,
            samples: vec![0, 1, 0x1234, 0xfffe, 0xffff, 777],
        };
        for endian in [RawEndian::Little, RawEndian::Big] {
            let bytes = encode_r16(&hf, endian);
            assert_eq!(bytes.len(), 12);
            assert_eq!(decode_r16(&bytes, 3, 2, endian).expect("decode"), hf);
        }
        assert_eq!(&encode_r16(&hf, RawEndian::Little)[4..6], &[0x34, 0x12]);
        assert_eq!(&encode_r16(&hf, RawEndian::Big)[4..6], &[0x12, 0x34]);
    }

    #[test]
    fn r16_decode_refuses_bad_lengths_and_tiny_grids() {
        assert!(matches!(
            decode_r16(&[0; 10], 3, 2, RawEndian::Little),
            Err(HeightIoError::RawLength {
                expected: 12,
                actual: 10,
                ..
            })
        ));
        assert!(matches!(
            decode_r16(&[0; 2], 1, 1, RawEndian::Little),
            Err(HeightIoError::TooSmall { .. })
        ));
        assert_eq!(
            HeightIoError::TooSmall { width: 1, height: 1 }.code(),
            "height_io_decode"
        );
    }

    #[test]
    fn square_side_infers_only_square_grids() {
        assert_eq!(square_side(2 * 257 * 257), Some(257));
        assert_eq!(square_side(2 * 4), Some(2));
        assert_eq!(square_side(2), None);
        assert_eq!(square_side(2 * 6), None);
        assert_eq!(square_side(7), None);
    }

    #[test]
    fn export_counts_samples_past_the_height_range() {
        let mut heights = vec![10.0; 16];
        heights[0] = -50.0;
        heights[1] = 500.0;
        heights[2] = 500.0;
        let map = TerrainMap::from_heights(4, 4, heights, 10.0, 100.0, -20.0).expect("map");
        let out = export_heights(&map);
        assert_eq!((out.clamped_low, out.clamped_high), (1, 2));
        assert_eq!(out.heightfield.samples[0], 0);
        assert_eq!(out.heightfield.samples[1], 65535);
        assert_eq!(out.heightfield, map.to_heightfield());
    }

    #[test]
    fn export_then_import_is_the_identity_on_a_quantised_map() {
        let source = ramp_map(33).to_heightfield();
        let map = TerrainMap::new(&source, 64.0, 100.0, -20.0).expect("map");
        let mut terrain = terrain(map);
        let before: Vec<u32> = terrain.map.heights().iter().map(|h| h.to_bits()).collect();
        let exported = export_heights(&terrain.map).heightfield;
        assert_eq!(exported, source);
        let mut edits = TerrainEdits::default();
        let out = import_heights(&mut terrain, &mut edits, &exported).expect("import");
        assert_eq!(out.changed, 0);
        assert!(out.undo.is_none());
        let after: Vec<u32> = terrain.map.heights().iter().map(|h| h.to_bits()).collect();
        assert_eq!(before, after);
        assert!(edits.take_gpu_dirty().is_none());
    }

    #[test]
    fn import_writes_changed_samples_and_undo_restores_them_bit_for_bit() {
        let map = ramp_map(65);
        let mut terrain = terrain(map);
        let before: Vec<u32> = terrain.map.heights().iter().map(|h| h.to_bits()).collect();
        let mut hf = terrain.map.to_heightfield();
        // Raise a 5×3 block at column 40, row 10 to the top of the range.
        for row in 10..13 {
            for col in 40..45 {
                hf.samples[row * 65 + col] = 65535;
            }
        }
        let mut edits = TerrainEdits::default();
        let out = import_heights(&mut terrain, &mut edits, &hf).expect("import");
        // Every unquantised ramp sample moves onto the grid, the block to 80 m.
        assert!(out.changed >= 15);
        assert!((terrain.map.height_at_sample(42, 11) - 80.0).abs() < 1e-4);
        assert_eq!(terrain.map.to_heightfield(), hf);
        assert!(edits.take_gpu_dirty().is_some());
        assert!(!edits.unsaved_tiles().is_empty());
        let Some(StrokeUndo::Height(record)) = out.undo else {
            panic!("an import keeps a height undo record");
        };
        assert_eq!(record.samples.len() as u64, out.changed);
        record.undo(&mut terrain.map);
        let restored: Vec<u32> = terrain.map.heights().iter().map(|h| h.to_bits()).collect();
        assert_eq!(before, restored);
    }

    /// Timing for `docs/specs/82-terrain-editing.md` § 10 (no budget asserted):
    /// `cargo test -p regia_terrain --release --lib height_io_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "prints timings of a 4097² map; run by hand"]
    fn height_io_timing() {
        let n = 4097u32;
        let spacing = (n - 1) as f32;
        let heights = (0..n * n)
            .map(|i| (i as f32 * 0.0007).sin() * 30.0 + 40.0)
            .collect();
        let map = TerrainMap::from_heights(n, n, heights, spacing, 200.0, -50.0).expect("map");
        let mut terrain =
            terrain(TerrainMap::new(&map.to_heightfield(), spacing, 200.0, -50.0).expect("map"));
        let t0 = std::time::Instant::now();
        let out = export_heights(&terrain.map);
        let t1 = std::time::Instant::now();
        let bytes = encode_r16(&out.heightfield, RawEndian::Little);
        let t2 = std::time::Instant::now();
        let mut hf = decode_r16(&bytes, n, n, RawEndian::Little).expect("decode");
        let t3 = std::time::Instant::now();
        for s in &mut hf.samples {
            *s = s.wrapping_add(7);
        }
        let t4 = std::time::Instant::now();
        let mut edits = TerrainEdits::default();
        let imported = import_heights(&mut terrain, &mut edits, &hf).expect("import");
        let t5 = std::time::Instant::now();
        println!(
            "{n}²: export {:?}, encode {:?}, decode {:?}, import of {} changed samples {:?} (undo kept: {})",
            t1 - t0,
            t2 - t1,
            t3 - t2,
            imported.changed,
            t5 - t4,
            imported.undo.is_some()
        );
    }

    #[test]
    fn import_past_the_undo_cap_applies_without_an_undo_record() {
        let source = ramp_map(17).to_heightfield();
        let mut hf = source.clone();
        for s in &mut hf.samples[..10] {
            *s = s.wrapping_add(1000);
        }
        let quantised = || terrain(TerrainMap::new(&source, 64.0, 100.0, -20.0).expect("map"));

        // Exactly at the cap the record is kept.
        let mut at_cap = quantised();
        let mut edits = TerrainEdits::default();
        let out = import_with_undo_cap(&mut at_cap, &mut edits, &hf, 10).expect("import");
        assert_eq!(out.changed, 10);
        assert!(out.undo.is_some());

        // One sample past it the heights still land and the edit is still noted.
        let mut past_cap = quantised();
        let mut edits = TerrainEdits::default();
        let out = import_with_undo_cap(&mut past_cap, &mut edits, &hf, 9).expect("import");
        assert_eq!(out.changed, 10);
        assert!(out.undo.is_none());
        assert_eq!(past_cap.map.to_heightfield(), hf);
        assert!(edits.take_gpu_dirty().is_some());
        assert!(!edits.unsaved_tiles().is_empty());
    }

    #[test]
    fn import_refuses_another_grid_and_leaves_the_terrain() {
        let mut terrain = terrain(ramp_map(17));
        let before: Vec<u32> = terrain.map.heights().iter().map(|h| h.to_bits()).collect();
        let hf = Heightfield {
            width: 16,
            height: 17,
            samples: vec![0; 16 * 17],
        };
        let err = import_heights(&mut terrain, &mut TerrainEdits::default(), &hf).expect_err("size");
        assert_eq!(
            err,
            HeightIoError::SizeMismatch {
                file: [16, 17],
                map: [17, 17]
            }
        );
        assert_eq!(err.code(), "height_io_size_mismatch");
        let short = Heightfield {
            width: 17,
            height: 17,
            samples: vec![0; 10],
        };
        assert!(matches!(
            import_heights(&mut terrain, &mut TerrainEdits::default(), &short),
            Err(HeightIoError::SampleCount { .. })
        ));
        let after: Vec<u32> = terrain.map.heights().iter().map(|h| h.to_bits()).collect();
        assert_eq!(before, after);
    }
}
