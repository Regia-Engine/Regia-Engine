// SPDX-License-Identifier: MPL-2.0
//! The cooked (pak) form of a [`CurveTable`] (spec 51 § 4.4, ADR-2012): a little-endian `u32`
//! version, then postcard of the table's default interpolation and its curves in file order.
//!
//! The payload is decoded through the same validation as text ([`CurveTable::new`]): a damaged or
//! edited pak gets the error the text path would report, and derived tangents are baked at load in
//! the same order, so a cooked table samples bit-identically to its source.

use serde::Serialize;

use super::{CURVE_TABLE_COOKED_VERSION, CurveIssue, CurveTable, CurveTableError, NamedCurve, TableInterp};

fn bad_file(message: impl Into<String>) -> CurveTableError {
    CurveTableError::one(CurveIssue::error("curve_bad_file", message))
}

/// The encoded payload of `(interp, curves)`, unvalidated (tests build broken ones with it).
pub(super) fn cooked_payload(interp: TableInterp, curves: &[NamedCurve]) -> Result<Vec<u8>, postcard::Error> {
    #[derive(Serialize)]
    struct Body<'a>(TableInterp, &'a [NamedCurve]);
    let out = CURVE_TABLE_COOKED_VERSION.to_le_bytes().to_vec();
    postcard::to_extend(&Body(interp, curves), out)
}

/// The serde shape fingerprint of the cooked form (hashed by `cargo xtask check-formats`): the
/// field lists of a curve and a key and the enums they carry.
#[must_use]
pub fn curve_table_cooked_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<NamedCurve>(),
        serde_field_names::<super::CurveKey>(),
        serde_variant_names::<TableInterp>(),
        serde_variant_names::<super::Extrap>(),
    ])
}

impl CurveTable {
    /// The form `regia cook` writes into a pak. [`from_cooked`](Self::from_cooked) of these bytes
    /// is this table; the bytes are the same for the same table.
    ///
    /// # Errors
    ///
    /// `curve_bad_file` when the encoder fails (never for a table built by this module).
    pub fn to_cooked(&self) -> Result<Vec<u8>, CurveTableError> {
        cooked_payload(self.interp(), self.curves()).map_err(|e| bad_file(e.to_string()))
    }

    /// Loads the cooked form and validates it as [`new`](Self::new) does.
    ///
    /// # Errors
    ///
    /// `curve_unsupported_version` (a version this build does not read), `curve_bad_file` (damaged
    /// or truncated bytes, bytes after the table), or every validation error.
    pub fn from_cooked(bytes: &[u8]) -> Result<Self, CurveTableError> {
        let _span = tracing::debug_span!(target: "regia_framework", "curve_table_load_cooked").entered();
        let Some((version, body)) = bytes.split_first_chunk::<4>() else {
            return Err(bad_file("cooked curve table is shorter than its version"));
        };
        let version = u32::from_le_bytes(*version);
        if version == 0 || version > CURVE_TABLE_COOKED_VERSION {
            return Err(CurveTableError::one(CurveIssue::error(
                "curve_unsupported_version",
                format!(
                    "cooked version {version}; this engine reads cooked curve tables up to {CURVE_TABLE_COOKED_VERSION}"
                ),
            )));
        }
        let ((interp, curves), rest) = postcard::take_from_bytes::<(TableInterp, Vec<NamedCurve>)>(body)
            .map_err(|e| bad_file(format!("cooked curve table: {e}")))?;
        if !rest.is_empty() {
            return Err(bad_file(format!(
                "{} bytes after the cooked curve table",
                rest.len()
            )));
        }
        Self::new(interp, curves)
    }
}
