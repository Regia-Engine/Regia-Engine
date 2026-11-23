// SPDX-License-Identifier: MPL-2.0
//! `regia assets chunks --max-size <SIZE> [--max-total <SIZE>]`: the chunk size-budget gate for CI
//! (spec 40 § 11, ADR-2710).
//!
//! Each chunk is measured (`--measure disk`, the default: its pak's bytes, what a player
//! downloads; `--measure memory`: its decompressed cooked bytes) and compared with the per-chunk
//! budget, and the sum with the total budget. Any chunk or the total over budget prints the budget
//! table (or JSON with `--json`) and exits non-zero with `content_size_budget`. Measuring disk
//! bytes of a chunk whose pak was never written is an error (`content_size_unmeasured`), never a
//! silent zero.

use anyhow::{Result, bail};
use regia_asset_pipeline::cook_cache::parse_size;
use serde_json::{Value, json};

use super::content::AssetsChunksArgs;

/// What a budget measures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Measure {
    /// Pak bytes on disk (what a player downloads).
    #[default]
    Disk,
    /// Decompressed cooked bytes.
    Memory,
}

impl Measure {
    /// `disk` or `memory`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Memory => "memory",
        }
    }

    fn field(self) -> &'static str {
        match self {
            Self::Disk => "disk_bytes",
            Self::Memory => "memory_bytes",
        }
    }
}

/// One chunk against the per-chunk budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetRow {
    /// Chunk name.
    pub chunk: String,
    /// Measured bytes.
    pub bytes: u64,
    /// Over the per-chunk budget.
    pub over: bool,
}

/// The whole check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetReport {
    /// What was measured.
    pub measure: Measure,
    /// Per-chunk budget, if one was given.
    pub max_size: Option<u64>,
    /// Total budget, if one was given.
    pub max_total: Option<u64>,
    /// One row per chunk, in manifest order.
    pub rows: Vec<BudgetRow>,
    /// Sum of every chunk.
    pub total: u64,
}

impl BudgetReport {
    /// Checks `sizes` (chunk name, bytes) against the budgets.
    #[must_use]
    pub fn check(
        measure: Measure,
        sizes: &[(String, u64)],
        max_size: Option<u64>,
        max_total: Option<u64>,
    ) -> Self {
        let rows = sizes
            .iter()
            .map(|(chunk, bytes)| BudgetRow {
                chunk: chunk.clone(),
                bytes: *bytes,
                over: max_size.is_some_and(|max| *bytes > max),
            })
            .collect();
        Self {
            measure,
            max_size,
            max_total,
            rows,
            total: sizes.iter().map(|(_, b)| *b).fold(0u64, u64::saturating_add),
        }
    }

    /// Whether the total is over the total budget.
    #[must_use]
    pub fn total_over(&self) -> bool {
        self.max_total.is_some_and(|max| self.total > max)
    }

    /// Whether anything is over budget.
    #[must_use]
    pub fn over(&self) -> bool {
        self.total_over() || self.rows.iter().any(|r| r.over)
    }

    /// The JSON report (`--json`).
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "measure": self.measure.label(),
            "max_size": self.max_size,
            "max_total": self.max_total,
            "chunks": self.rows.iter().map(|r| json!({
                "name": r.chunk,
                "bytes": r.bytes,
                "budget": self.max_size,
                "over": r.over,
            })).collect::<Vec<_>>(),
            "total": { "bytes": self.total, "budget": self.max_total, "over": self.total_over() },
            "over_budget": self.rows.iter().filter(|r| r.over).map(|r| r.chunk.as_str()).collect::<Vec<_>>(),
            "ok": !self.over(),
        })
    }

    /// The text table.
    #[must_use]
    pub fn to_text(&self) -> String {
        let budget = |b: Option<u64>| b.map_or_else(|| "-".to_owned(), |b| b.to_string());
        let mut out = format!(
            "{:<24} {:>14} {:>14}  status ({} bytes)\n",
            "chunk",
            "bytes",
            "budget",
            self.measure.label()
        );
        for r in &self.rows {
            out.push_str(&format!(
                "{:<24} {:>14} {:>14}  {}\n",
                r.chunk,
                r.bytes,
                budget(self.max_size),
                if r.over { "OVER" } else { "ok" }
            ));
        }
        out.push_str(&format!(
            "{:<24} {:>14} {:>14}  {}\n",
            "(total)",
            self.total,
            budget(self.max_total),
            if self.total_over() { "OVER" } else { "ok" }
        ));
        for r in self.rows.iter().filter(|r| r.over) {
            let max = self.max_size.unwrap_or(0);
            out.push_str(&format!(
                "error: content_size_budget: chunk {} is {} bytes, {} over its budget of {max}\n",
                r.chunk,
                r.bytes,
                r.bytes - max
            ));
        }
        if self.total_over() {
            let max = self.max_total.unwrap_or(0);
            out.push_str(&format!(
                "error: content_size_budget: all chunks are {} bytes, {} over the total budget of {max}\n",
                self.total,
                self.total - max
            ));
        }
        out.trim_end().to_owned()
    }
}

/// Runs the gate: (text or JSON, over budget).
///
/// # Errors
/// A size that does not parse, no cooked registry, or (`--measure disk`) a chunk without a pak
/// (`content_size_unmeasured`).
pub fn gate(args: &AssetsChunksArgs) -> Result<(String, bool)> {
    let parse = |flag: &str, v: &Option<String>| -> Result<Option<u64>> {
        v.as_deref()
            .map(|t| parse_size(t).map_err(|e| anyhow::anyhow!("{flag}: {e}")))
            .transpose()
    };
    let max_size = parse("--max-size", &args.max_size)?;
    let max_total = parse("--max-total", &args.max_total)?;
    let (rows, _problems) = super::content::chunk_rows(&args.project, args.platform.as_ref())?;
    if args.measure == Measure::Disk {
        let pak_dir = super::content::pak_dir_of(&args.project, args.platform.as_ref());
        let missing: Vec<&str> = rows
            .iter()
            .filter_map(|r| r["name"].as_str())
            .filter(|name| !pak_dir.join(format!("{name}.fpak")).is_file())
            .collect();
        if !missing.is_empty() {
            bail!(
                "content_size_unmeasured: no pak for chunk(s) {} in {} (run `regia cook` or `regia package` first, or use --measure memory)",
                missing.join(", "),
                pak_dir.display()
            );
        }
    }
    let sizes: Vec<(String, u64)> = rows
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap_or_default().to_owned(),
                r[args.measure.field()].as_u64().unwrap_or(0),
            )
        })
        .collect();
    let report = BudgetReport::check(args.measure, &sizes, max_size, max_total);
    let text = if args.json {
        serde_json::to_string_pretty(&report.to_json())?
    } else {
        report.to_text()
    };
    Ok((text, report.over()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sizes() -> Vec<(String, u64)> {
        vec![("base".into(), 1000), ("arena".into(), 3 << 20)]
    }

    #[test]
    fn per_chunk_and_total_budgets() {
        let ok = BudgetReport::check(Measure::Disk, &sizes(), Some(3 << 20), Some((3 << 20) + 1000));
        assert!(!ok.over());
        assert_eq!(ok.to_json()["ok"], json!(true));
        let chunk = BudgetReport::check(Measure::Disk, &sizes(), Some((3 << 20) - 1), None);
        assert!(chunk.over() && !chunk.total_over());
        assert_eq!(chunk.to_json()["over_budget"], json!(["arena"]));
        assert!(
            chunk
                .to_text()
                .contains("error: content_size_budget: chunk arena is 3145728 bytes, 1 over")
        );
        let total = BudgetReport::check(Measure::Memory, &sizes(), None, Some(3 << 20));
        assert!(total.over() && total.total_over());
        assert_eq!(total.to_json()["total"]["over"], json!(true));
        assert_eq!(total.to_json()["measure"], json!("memory"));
        assert!(total.to_text().contains("(total)"));
    }

    #[test]
    fn sizes_take_binary_units() {
        assert_eq!(parse_size("3MiB"), Ok(3 << 20));
        assert_eq!(parse_size("512KiB"), Ok(512 << 10));
        assert_eq!(parse_size("1048576"), Ok(1 << 20));
        assert!(parse_size("lots").is_err());
    }
}
