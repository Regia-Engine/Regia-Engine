// SPDX-License-Identifier: MPL-2.0
//! The shared resolve test vectors (`exchange/option_graph/1/fixtures/resolve_vectors.jsonl`,
//! plan `interfaces.md` § 9): one JSON object per line, each a product key, its picks and the
//! resolution the reference implementation gives. Regia's port must give identical results
//! (ADR-2534).
//!
//! A line is `{"product_key": "...", "picks": {...}, "expect": {...}}` (`expected` and
//! `resolution` are accepted for `expect`). Every field of the expected resolution is compared
//! exactly, except the pricing fields (`cost`, `retail`, `price`, `markup`): Regia never prices.
//! A field the port does not produce is a mismatch, so a vector file that grows a field fails
//! loudly instead of being skipped.
//!
//! Blank lines and lines starting with `#` or `//` are skipped.

use serde::Serialize;
use serde_json::Value;

use crate::graph::ProductRecord;
use crate::resolve::{Picks, resolve};
use crate::schema::json_eq;

/// Expected-resolution fields Regia never produces because it never prices.
pub const PRICING_FIELDS: &[&str] = &["cost", "retail", "price", "markup"];

/// One vector.
#[derive(Debug, Clone)]
pub struct VectorCase {
    /// 1-based line number.
    pub line: usize,
    /// The product.
    pub product_key: String,
    /// The picks.
    pub picks: Picks,
    /// The expected resolution.
    pub expect: Value,
}

/// One difference between a vector and the port.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mismatch {
    /// 1-based line number.
    pub line: usize,
    /// The product.
    pub product_key: String,
    /// The field that differs (`product` when the product record is missing).
    pub field: String,
    /// What the vector expects.
    pub expected: Value,
    /// What the port gave.
    pub got: Value,
}

/// The result of a vector run.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct VectorReport {
    /// Vectors read.
    pub cases: usize,
    /// Vectors that matched in every compared field.
    pub matched: usize,
    /// Every difference.
    pub mismatches: Vec<Mismatch>,
}

/// Parse a vector file.
pub fn parse_vectors(text: &str) -> Result<Vec<VectorCase>, String> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        let product_key = v
            .get("product_key")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("line {}: no product_key", i + 1))?
            .to_owned();
        let picks: Picks = serde_json::from_value(
            v.get("picks")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        )
        .map_err(|e| format!("line {}: picks: {e}", i + 1))?;
        let expect = ["expect", "expected", "resolution"]
            .iter()
            .find_map(|k| v.get(*k))
            .cloned()
            .ok_or_else(|| format!("line {}: no expect", i + 1))?;
        if !expect.is_object() {
            return Err(format!("line {}: expect is not an object", i + 1));
        }
        out.push(VectorCase {
            line: i + 1,
            product_key,
            picks,
            expect,
        });
    }
    Ok(out)
}

/// Compare one vector with the port's resolution of it.
pub fn compare(case: &VectorCase, product: &ProductRecord) -> Vec<Mismatch> {
    let resolution = resolve(product, &case.picks);
    let got = resolution.to_json();
    let mut out = Vec::new();
    let Some(expect) = case.expect.as_object() else {
        return out;
    };
    for (field, want) in expect {
        if PRICING_FIELDS.contains(&field.as_str()) {
            continue;
        }
        let have = got.get(field).cloned().unwrap_or(Value::Null);
        let present = got.get(field).is_some();
        if !present || !json_eq(want, &have) {
            out.push(Mismatch {
                line: case.line,
                product_key: case.product_key.clone(),
                field: field.clone(),
                expected: want.clone(),
                got: if present {
                    have
                } else {
                    Value::String("<not produced>".to_owned())
                },
            });
        }
    }
    out
}

/// Run every vector; `lookup` finds a product record by key.
pub fn run_vectors<'a>(
    cases: &[VectorCase],
    lookup: &dyn Fn(&str) -> Option<&'a ProductRecord>,
) -> VectorReport {
    let mut report = VectorReport {
        cases: cases.len(),
        ..VectorReport::default()
    };
    for case in cases {
        let Some(product) = lookup(&case.product_key) else {
            report.mismatches.push(Mismatch {
                line: case.line,
                product_key: case.product_key.clone(),
                field: "product".to_owned(),
                expected: Value::String(case.product_key.clone()),
                got: Value::Null,
            });
            continue;
        };
        let diff = compare(case, product);
        if diff.is_empty() {
            report.matched += 1;
        }
        report.mismatches.extend(diff);
    }
    report
}
