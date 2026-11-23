// SPDX-License-Identifier: MPL-2.0
//! The `.curves.ron` (format `curve_table` v1) and `.curves.csv` forms of a [`CurveTable`].

use std::fmt;

use regia_asset::csv;
use regia_core::version;
use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

use super::{
    CURVE_TABLE_FORMAT_VERSION, CurveIssue, CurveKey, CurveTable, CurveTableError, Extrap, NamedCurve,
    TableInterp,
};

/// The `.curves.ron` envelope.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CurveFile {
    format_version: u32,
    #[serde(default)]
    interp: TableInterp,
    #[serde(default, deserialize_with = "ordered_curves")]
    curves: Vec<(String, CurveBody)>,
}

/// One curve's body in the `curves` map.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CurveBody {
    #[serde(default)]
    pre: Extrap,
    #[serde(default)]
    post: Extrap,
    #[serde(default, deserialize_with = "bare_some")]
    interp: Option<TableInterp>,
    keys: Vec<RonKey>,
}

/// `interp: Cubic` (no `Some(..)` wrapper) for an optional field.
fn bare_some<'de, D: Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

/// The `curves` map, keeping file order.
fn ordered_curves<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<(String, CurveBody)>, D::Error> {
    struct Ordered;
    impl<'de> Visitor<'de> for Ordered {
        type Value = Vec<(String, CurveBody)>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a map of curve name to curve")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(entry) = map.next_entry::<String, CurveBody>()? {
                out.push(entry);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_map(Ordered)
}

/// A key: `(x, y)` or `(x: .., y: .., interp: .., in_tangent: .., out_tangent: ..)`.
struct RonKey(CurveKey);

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum KeyField {
    X,
    Y,
    Interp,
    InTangent,
    OutTangent,
}

impl<'de> Deserialize<'de> for RonKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl<'de> Visitor<'de> for KeyVisitor {
            type Value = RonKey;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a key `(x, y)` or `(x: .., y: .., interp: .., in_tangent: .., out_tangent: ..)`")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<RonKey, A::Error> {
                let x: f32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let y: f32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                if seq.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::invalid_length(3, &self));
                }
                Ok(RonKey(CurveKey::new(x, y)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RonKey, A::Error> {
                let mut key = CurveKey::default();
                let (mut has_x, mut has_y) = (false, false);
                while let Some(field) = map.next_key::<KeyField>()? {
                    match field {
                        KeyField::X => {
                            key.x = map.next_value()?;
                            has_x = true;
                        }
                        KeyField::Y => {
                            key.y = map.next_value()?;
                            has_y = true;
                        }
                        KeyField::Interp => key.interp = Some(map.next_value()?),
                        KeyField::InTangent => key.in_tangent = Some(map.next_value()?),
                        KeyField::OutTangent => key.out_tangent = Some(map.next_value()?),
                    }
                }
                if !has_x {
                    return Err(de::Error::missing_field("x"));
                }
                if !has_y {
                    return Err(de::Error::missing_field("y"));
                }
                Ok(RonKey(key))
            }
        }
        deserializer.deserialize_any(KeyVisitor)
    }
}

/// The `.curves.ron` migration chain: v1 is the first version, so there are no steps.
#[must_use]
pub fn curve_table_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape fingerprint of `.curves.ron` (hashed by `cargo xtask check-formats`): the
/// envelope's and the curve body's field lists, the key's field spellings, and the variant
/// spellings of [`TableInterp`] and [`Extrap`].
#[must_use]
pub fn curve_table_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<CurveFile>(),
        serde_field_names::<CurveBody>(),
        ["x", "y", "interp", "in_tangent", "out_tangent"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        serde_variant_names::<TableInterp>(),
        serde_variant_names::<Extrap>(),
    ])
}

/// Shortest round-trip text of `v` with a decimal point (`1.0`, `0.25`, `1e-7`), as RON writes it.
fn ron_num(v: f32) -> String {
    format!("{v:?}")
}

/// Shortest round-trip text of `v` without an exponent (`1`, `0.25`), as spreadsheets write it.
fn csv_num(v: f32) -> String {
    format!("{v}")
}

impl CurveTable {
    /// Parses a `.curves.ron`: version check, shape, then [`validate_curves`](super::validate_curves).
    ///
    /// # Errors
    ///
    /// `curve_bad_file` (syntax or shape), `curve_unsupported_version` (newer than
    /// [`CURVE_TABLE_FORMAT_VERSION`], or 0), or every validation error.
    pub fn from_ron_str(text: &str) -> Result<Self, CurveTableError> {
        let _span = tracing::debug_span!(target: "regia_framework", "curve_table_load").entered();
        let unsupported = |found: u32| {
            CurveTableError::one(CurveIssue::error(
                "curve_unsupported_version",
                format!(
                    "format_version {found}; this engine reads curve tables up to {CURVE_TABLE_FORMAT_VERSION}"
                ),
            ))
        };
        // One parse on the happy path, as the chooser does; the version is peeked only to explain a
        // failure (a newer file whose body no longer matches reports its version, not a shape
        // error). The peek reads the whole file as an untyped value, and the `ron` this workspace
        // locks scans the rest of the text for every untyped number: a 4,096-curve file took
        // minutes through it (ADR-2012).
        let file: CurveFile = match ron::from_str(text) {
            Ok(file) => file,
            Err(error) => {
                return Err(match version::peek_ron_version(text) {
                    Ok(found) if found == 0 || found > CURVE_TABLE_FORMAT_VERSION => unsupported(found),
                    Ok(_) => CurveTableError::one(CurveIssue::error("curve_bad_file", error.to_string())),
                    Err(peek) => CurveTableError::one(CurveIssue::error("curve_bad_file", peek)),
                });
            }
        };
        if file.format_version == 0 || file.format_version > CURVE_TABLE_FORMAT_VERSION {
            return Err(unsupported(file.format_version));
        }
        tracing::debug!(target: "regia_framework", format_version = file.format_version, "curve table read");
        let curves = file
            .curves
            .into_iter()
            .map(|(name, body)| NamedCurve {
                name,
                pre: body.pre,
                post: body.post,
                interp: body.interp,
                keys: body.keys.into_iter().map(|k| k.0).collect(),
            })
            .collect();
        Self::new(file.interp, curves)
    }

    /// Writes the canonical `.curves.ron` text: curves in table order, a key as `(x, y)` when it has
    /// nothing else set, otherwise in the named form; ends with a newline. Writing a table read
    /// from this text gives the same bytes.
    #[must_use]
    pub fn to_ron_string(&self) -> String {
        let mut out = String::new();
        out.push_str("(\n");
        out.push_str(&format!("    format_version: {CURVE_TABLE_FORMAT_VERSION},\n"));
        out.push_str(&format!("    interp: {},\n", self.interp().ron_name()));
        out.push_str("    curves: {\n");
        for curve in self.curves() {
            out.push_str(&format!(
                "        {:?}: (pre: {}, post: {}, ",
                curve.name,
                curve.pre.ron_name(),
                curve.post.ron_name()
            ));
            if let Some(interp) = curve.interp {
                out.push_str(&format!("interp: {}, ", interp.ron_name()));
            }
            out.push_str("keys: [");
            for (i, key) in curve.keys.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                if key.interp.is_none() && key.in_tangent.is_none() && key.out_tangent.is_none() {
                    out.push_str(&format!("({}, {})", ron_num(key.x), ron_num(key.y)));
                } else {
                    out.push_str(&format!("(x: {}, y: {}", ron_num(key.x), ron_num(key.y)));
                    if let Some(interp) = key.interp {
                        out.push_str(&format!(", interp: {}", interp.ron_name()));
                    }
                    if let Some(t) = key.in_tangent {
                        out.push_str(&format!(", in_tangent: {}", ron_num(t)));
                    }
                    if let Some(t) = key.out_tangent {
                        out.push_str(&format!(", out_tangent: {}", ron_num(t)));
                    }
                    out.push(')');
                }
            }
            out.push_str("]),\n");
        }
        out.push_str("    },\n)\n");
        out
    }

    /// Parses a `.curves.csv`: header `name,<x>,<x>,...` (finite, strictly increasing), one curve
    /// per record, `y` cells, a blank cell meaning no key at that `x`. Every curve gets `interp`
    /// as the table default and `Hold` extrapolation.
    ///
    /// # Errors
    ///
    /// `curve_bad_file` (RFC 4180 record errors, a missing `name` header), `curve_csv_ragged`
    /// (an axis header that is not a finite number, or repeats), `curve_unsorted_x`,
    /// `curve_bad_number` (a cell that is not a finite number), or every validation error.
    pub fn from_csv_str(text: &str, interp: TableInterp) -> Result<Self, CurveTableError> {
        let _span = tracing::debug_span!(target: "regia_framework", "curve_table_load_csv").entered();
        let records = csv::parse(text).map_err(|e| {
            CurveTableError::one(CurveIssue::error("curve_bad_file", e.kind.to_string()).with_line(e.line))
        })?;
        let Some((header, rows)) = records.split_first() else {
            return Err(CurveTableError::one(CurveIssue::error(
                "curve_bad_file",
                "the CSV has no header record",
            )));
        };
        if header.fields.first().map(|f| f.trim()) != Some("name") {
            return Err(CurveTableError::one(
                CurveIssue::error("curve_bad_file", "the first header must be `name`").with_line(header.line),
            ));
        }
        let mut issues = Vec::new();
        let mut axis: Vec<f32> = Vec::with_capacity(header.fields.len().saturating_sub(1));
        for field in &header.fields[1..] {
            let x = match field.trim().parse::<f32>() {
                Ok(x) if x.is_finite() => x,
                _ => {
                    issues.push(
                        CurveIssue::error(
                            "curve_csv_ragged",
                            format!("axis header `{field}` is not a finite number"),
                        )
                        .with_line(header.line),
                    );
                    continue;
                }
            };
            if let Some(&prev) = axis.last() {
                if x == prev {
                    issues.push(
                        CurveIssue::error("curve_csv_ragged", format!("axis x = {x} appears twice"))
                            .with_line(header.line),
                    );
                    continue;
                }
                if x < prev {
                    issues.push(
                        CurveIssue::error("curve_unsorted_x", format!("axis x = {x} follows x = {prev}"))
                            .with_line(header.line),
                    );
                    continue;
                }
            }
            axis.push(x);
        }
        if !issues.is_empty() {
            return Err(CurveTableError { issues });
        }
        let mut curves = Vec::with_capacity(rows.len());
        let mut lines = Vec::with_capacity(rows.len());
        for record in rows {
            let name = record.fields[0].trim();
            let mut keys = Vec::new();
            for (cell, &x) in record.fields[1..].iter().zip(&axis) {
                let cell = cell.trim();
                if cell.is_empty() {
                    continue;
                }
                match cell.parse::<f32>() {
                    Ok(y) if y.is_finite() => keys.push(CurveKey::new(x, y)),
                    _ => issues.push(
                        CurveIssue::error(
                            "curve_bad_number",
                            format!("cell `{cell}` at x = {x} is not a finite number"),
                        )
                        .with_curve(name)
                        .with_line(record.line),
                    ),
                }
            }
            curves.push(NamedCurve::new(name, keys));
            lines.push((name.to_owned(), record.line));
        }
        if !issues.is_empty() {
            return Err(CurveTableError { issues });
        }
        Self::new(interp, curves).map_err(|mut error| {
            for issue in &mut error.issues {
                if issue.line.is_none()
                    && let Some(curve) = &issue.curve
                    && let Some((_, line)) = lines.iter().find(|(name, _)| name == curve)
                {
                    issue.line = Some(*line);
                }
            }
            error
        })
    }

    /// Writes the `.curves.csv` form: the axis is the sorted union of every curve's `x`, a blank
    /// cell where a curve has no key. CSV carries no extrapolation, per-curve or per-key
    /// interpolation, or tangents. Reading the text back gives the same keys.
    ///
    /// # Errors
    ///
    /// `curve_csv_ragged` when a key's `x` is not finite (never for a table built by this module).
    pub fn to_csv_string(&self) -> Result<String, CurveTableError> {
        let mut axis: Vec<f32> = Vec::new();
        for curve in self.curves() {
            for key in &curve.keys {
                if !key.x.is_finite() {
                    return Err(CurveTableError::one(
                        CurveIssue::error("curve_csv_ragged", format!("key x = {} is not finite", key.x))
                            .with_curve(&curve.name),
                    ));
                }
                axis.push(key.x);
            }
        }
        axis.sort_by(f32::total_cmp);
        axis.dedup();
        let mut records: Vec<Vec<String>> = Vec::with_capacity(self.len() + 1);
        let mut header = Vec::with_capacity(axis.len() + 1);
        header.push("name".to_owned());
        header.extend(axis.iter().map(|&x| csv_num(x)));
        records.push(header);
        for curve in self.curves() {
            let mut record = Vec::with_capacity(axis.len() + 1);
            record.push(curve.name.clone());
            let mut keys = curve.keys.iter().peekable();
            for &x in &axis {
                match keys.peek() {
                    Some(key) if key.x == x => {
                        record.push(csv_num(key.y));
                        keys.next();
                    }
                    _ => record.push(String::new()),
                }
            }
            records.push(record);
        }
        Ok(csv::to_string(&records))
    }
}
