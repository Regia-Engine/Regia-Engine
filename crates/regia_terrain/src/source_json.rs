// SPDX-License-Identifier: MPL-2.0
//! Single-pass schema-2 reader with bounded containers and typed Serde scalars.
//! Original Regia implementation; ADR-8362. RFC 8259 §§ 4–5; Serde
//! StreamDeserializer / IgnoredAny API documentation (docs/provenance.md).

use std::ops::ControlFlow;

use serde::de::{DeserializeOwned, IgnoredAny};

use super::{ImportLimits, JsonImportProgress, SourceError, check_dimensions, invalid};

pub(super) struct JsonSource {
    pub schema: u32,
    pub grid: JsonGrid,
    pub units: JsonUnits,
    pub counts: Vec<u16>,
}
pub(super) struct JsonGrid {
    pub cols: u32,
    pub rows: u32,
    pub cell_m: f32,
    pub row0: String,
    pub col0: String,
    pub sample0_x_m: Option<f32>,
    pub sample0_z_m: Option<f32>,
}
pub(super) struct JsonUnits {
    pub kind: String,
    pub meters_per_count: f32,
    pub zero_counts: u16,
}

struct Reader<'a, 'b, F> {
    bytes: &'a [u8],
    offset: usize,
    limits: ImportLimits,
    progress: JsonImportProgress,
    observer: &'b mut F,
}

fn duplicate<T>(slot: &Option<T>, name: &str) -> Result<(), SourceError> {
    if slot.is_some() {
        Err(invalid(format!("duplicate field `{name}`")))
    } else {
        Ok(())
    }
}
fn required<T>(slot: Option<T>, name: &str) -> Result<T, SourceError> {
    slot.ok_or_else(|| invalid(format!("missing field `{name}`")))
}

// Each typed slot reads its original token as T, especially f32; no Value/f64
// intermediary. Option<Option<f32>> preserves duplicate detection for nulls.
macro_rules! object_reader {
    ($method:ident, $value:ident, { $($field:ident : $ty:ty => $finish:ident),* $(,)? }) => {
        fn $method(&mut self) -> Result<$value, SourceError> {
            self.depth(2)?;
            self.punctuation(b'{')?;
            $(let mut $field: Option<$ty> = None;)*
            let mut first = true;
            while self.item(&mut first, b'}')? {
                let key = self.key()?;
                match key.as_str() {
                    $(stringify!($field) => {
                        duplicate(&$field, stringify!($field))?;
                        self.depth(3)?;
                        $field = Some(self.scalar()?);
                    },)*
                    _ => {
                        self.charge_unknown(3)?; // Field name, then its value.
                        self.skip(3)?;
                    }
                }
            }
            Ok($value { $($field: finish_field!($finish, $field),)* })
        }
    };
}
macro_rules! finish_field {
    (required, $field:ident) => {
        required($field, stringify!($field))?
    };
    (south, $field:ident) => {
        $field.unwrap_or_else(|| "south".into())
    };
    (west, $field:ident) => {
        $field.unwrap_or_else(|| "west".into())
    };
    (optional, $field:ident) => {
        $field.flatten()
    };
}

impl<F: FnMut(JsonImportProgress) -> ControlFlow<()>> Reader<'_, '_, F> {
    fn observe(&mut self) -> Result<(), SourceError> {
        if (self.observer)(self.progress).is_break() {
            return Err(invalid("height_source_cancelled: import cancelled"));
        }
        Ok(())
    }

    fn depth(&mut self, depth: usize) -> Result<(), SourceError> {
        if depth > self.limits.max_json_depth {
            return Err(invalid("height_source_limit_depth: max_json_depth exceeded"));
        }
        self.progress.max_depth = self.progress.max_depth.max(depth);
        Ok(())
    }

    fn charge_unknown(&mut self, depth: usize) -> Result<(), SourceError> {
        self.depth(depth)?;
        if self.progress.unknown_values == self.limits.max_unknown_values {
            return Err(invalid(
                "height_source_limit_metadata: max_unknown_values exceeded",
            ));
        }
        self.progress.unknown_values += 1;
        self.observe()
    }

    fn peek(&mut self) -> Option<u8> {
        while matches!(self.bytes.get(self.offset), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.offset += 1;
        }
        self.bytes.get(self.offset).copied()
    }

    fn punctuation(&mut self, byte: u8) -> Result<(), SourceError> {
        if self.peek() != Some(byte) {
            return Err(invalid(format!(
                "expected `{}` at byte {}",
                char::from(byte),
                self.offset
            )));
        }
        self.offset += 1;
        Ok(())
    }

    // Array elements and object members have the same comma/closing grammar.
    // Consume a close only before a comma, so trailing commas are never accepted.
    fn item(&mut self, first: &mut bool, close: u8) -> Result<bool, SourceError> {
        if self.peek() == Some(close) {
            self.offset += 1;
            return Ok(false);
        }
        if !*first {
            self.punctuation(b',')?;
            if self.peek() == Some(close) {
                return Err(invalid(format!("trailing comma at byte {}", self.offset)));
            }
        }
        *first = false;
        Ok(true)
    }

    fn scalar<T: DeserializeOwned>(&mut self) -> Result<T, SourceError> {
        // Never hand a container to IgnoredAny: its recursive skipping would
        // bypass the node/depth charges. Known fields also require scalars.
        if matches!(self.peek(), Some(b'[' | b'{')) {
            return Err(invalid(format!("expected scalar at byte {}", self.offset)));
        }
        let mut stream = serde_json::Deserializer::from_slice(&self.bytes[self.offset..]).into_iter::<T>();
        let value = stream
            .next()
            .ok_or_else(|| invalid("unexpected end of JSON"))?
            .map_err(|e| invalid(format!("at source byte {}: {e}", self.offset)))?;
        self.offset += stream.byte_offset();
        Ok(value)
    }

    fn key(&mut self) -> Result<String, SourceError> {
        let key = self.scalar::<String>()?;
        self.punctuation(b':')?;
        Ok(key)
    }

    fn root(&mut self) -> Result<JsonSource, SourceError> {
        self.depth(1)?;
        self.punctuation(b'{')?;
        let (mut schema, mut grid, mut units, mut counts) = (None, None, None, None);
        let mut first = true;
        while self.item(&mut first, b'}')? {
            let key = self.key()?;
            match key.as_str() {
                "schema" => {
                    duplicate(&schema, "schema")?;
                    self.depth(2)?;
                    schema = Some(self.scalar()?);
                }
                "grid" => {
                    duplicate(&grid, "grid")?;
                    let value = self.grid()?;
                    check_dimensions([value.cols, value.rows], self.limits)?;
                    grid = Some(value);
                }
                "units" => {
                    duplicate(&units, "units")?;
                    units = Some(self.units()?);
                }
                "counts" => {
                    duplicate(&counts, "counts")?;
                    counts = Some(self.counts()?);
                }
                _ => {
                    self.charge_unknown(2)?;
                    self.skip(2)?;
                }
            }
        }
        Ok(JsonSource {
            schema: required(schema, "schema")?,
            grid: required(grid, "grid")?,
            units: required(units, "units")?,
            counts: required(counts, "counts")?,
        })
    }

    object_reader!(grid, JsonGrid, {
        cols: u32 => required,
        rows: u32 => required,
        cell_m: f32 => required,
        row0: String => south,
        col0: String => west,
        sample0_x_m: Option<f32> => optional,
        sample0_z_m: Option<f32> => optional,
    });
    object_reader!(units, JsonUnits, {
        kind: String => required,
        meters_per_count: f32 => required,
        zero_counts: u16 => required,
    });

    fn counts(&mut self) -> Result<Vec<u16>, SourceError> {
        self.depth(2)?;
        self.punctuation(b'[')?;
        let mut counts = Vec::new();
        let mut first = true;
        while self.item(&mut first, b']')? {
            if self.progress.samples_charged == self.limits.max_samples {
                return Err(invalid("height_source_limit_samples: counts exceed max_samples"));
            }
            self.depth(3)?;
            self.progress.samples_charged += 1;
            self.observe()?;
            let value = self.scalar::<u16>()?;
            if counts.len() == counts.capacity() {
                let desired = counts
                    .capacity()
                    .max(128)
                    .saturating_mul(2)
                    .min(self.limits.max_samples);
                counts
                    .try_reserve_exact(desired - counts.len())
                    .map_err(|_| invalid("height_source_allocation: sample storage unavailable"))?;
            }
            counts.push(value);
            self.progress.samples_stored = counts.len();
            self.progress.sample_capacity = counts.capacity();
            self.observe()?;
        }
        Ok(counts)
    }

    fn skip(&mut self, depth: usize) -> Result<(), SourceError> {
        self.charge_unknown(depth)?;
        match self.peek() {
            Some(b'[') => {
                self.offset += 1;
                let mut first = true;
                while self.item(&mut first, b']')? {
                    self.skip(depth + 1)?;
                }
            }
            Some(b'{') => {
                self.offset += 1;
                let mut first = true;
                while self.item(&mut first, b'}')? {
                    self.charge_unknown(depth + 1)?;
                    if self.peek() != Some(b'"') {
                        return Err(invalid("object keys must be strings"));
                    }
                    self.scalar::<IgnoredAny>()?;
                    self.punctuation(b':')?;
                    self.skip(depth + 1)?;
                }
            }
            _ => {
                self.scalar::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

pub(super) fn parse(
    bytes: &[u8],
    limits: ImportLimits,
    observer: &mut impl FnMut(JsonImportProgress) -> ControlFlow<()>,
) -> Result<(JsonSource, JsonImportProgress), SourceError> {
    let mut reader = Reader {
        bytes,
        offset: 0,
        limits,
        progress: JsonImportProgress::default(),
        observer,
    };
    reader.observe()?;
    let doc = reader.root()?;
    if reader.peek().is_some() {
        return Err(invalid("trailing data after terrain source"));
    }
    Ok((doc, reader.progress))
}
