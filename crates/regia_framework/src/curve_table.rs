// SPDX-License-Identifier: MPL-2.0
//! Curve tables: named float curves kept in one file and sampled by name at runtime (damage
//! falloff over distance, XP per level, difficulty ramps).
//!
//! Specification: `docs/specs/51-curve-tables-and-choosers.md` §§ 2.1, 4.1, 4.2 (ADR-1649);
//! guide: `book/agents/curve-tables.md`; format: `docs/formats/curve-table-ron.md`.
//!
//! - [`CurveTable`]: up to [`MAX_CURVES`] [`NamedCurve`]s in file order, each 1 to [`MAX_KEYS`]
//!   [`CurveKey`]s with finite, strictly increasing `x`. Built only through validating
//!   constructors and edits, so a table in hand always samples.
//! - Interpolation per table, curve or key ([`TableInterp`]): step, linear, or cubic Hermite with
//!   tangents derived once at load by finite differences and the Fritsch-Carlson limiter (a
//!   monotone run of keys never overshoots). Explicit tangents (RON only) win.
//! - Extrapolation per side ([`Extrap`]): hold, linear, cycle, cycle with offset, ping-pong.
//! - Forms: `.curves.ron` (format `curve_table` v1) and `.curves.csv` (RFC 4180, one curve per
//!   row, header `name,<x>,<x>,...`, blank cell = no key).
//! - [`CurveTables`] (a resource) holds the tables an app registered with
//!   [`CurveTableAppExt::register_curve_table`], with hot reload that applies only valid files.
//!
//! ```
//! use regia_framework::curve_table::CurveTable;
//!
//! let table = CurveTable::from_ron_str(
//!     r#"(format_version: 1, interp: Linear, curves: {
//!         "falloff": (keys: [(0.0, 1.0), (10.0, 0.6), (30.0, 0.0)]),
//!     })"#,
//! )
//! .expect("valid table");
//! assert_eq!(table.sample_named("falloff", 5.0), Some(0.8));
//! assert_eq!(table.sample_named("falloff", 99.0), Some(0.0)); // `Hold` after the last key
//! ```

mod cooked;
mod format;
mod runtime;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU32, Ordering};

use regia_app::prelude::{Reflect, bevy_reflect};
use regia_core::math::{Curve, Interp, Keyframe};
use regia_ecs::reflect::Typed;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[cfg(test)]
use cooked::cooked_payload;
pub use cooked::curve_table_cooked_schema_fingerprint;
pub use format::{curve_table_migration_sources, curve_table_schema_fingerprint};
use regia_asset::data_table::DataTable;
pub use regia_asset::data_table::Severity;
pub use runtime::{
    CurveTableAppExt, CurveTablePlugin, CurveTableRegistrations, CurveTableSlot, CurveTables,
    PendingCurveTableReloads, apply_curve_table_reloads, collect_curve_table_changes,
    load_registered_curve_tables, parse_curve_table, warn_nonfinite_samples,
};

/// Current `.curves.ron` format version (`curve_table` v1).
pub const CURVE_TABLE_FORMAT_VERSION: u32 = 1;

/// Current version of the cooked (pak) form ([`CurveTable::to_cooked`]).
pub const CURVE_TABLE_COOKED_VERSION: u32 = 1;

/// Type path of a cooked curve table in the asset manifest and the cooked header.
pub const CURVE_TABLE_TYPE_PATH: &str = "regia_framework::curve_table::CurveTable";

/// Most curves one table may hold.
pub const MAX_CURVES: usize = 4096;

/// Most keys one curve may hold.
pub const MAX_KEYS: usize = 4096;

/// Longest curve name, in bytes.
pub const MAX_CURVE_NAME_LEN: usize = 64;

/// How a curve moves from one key to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum TableInterp {
    /// Hold the left key's value.
    Step,
    /// Straight line between the keys.
    #[default]
    Linear,
    /// Cubic Hermite; tangents derived by finite differences with the Fritsch-Carlson limiter
    /// unless the key carries explicit ones.
    Cubic,
}

impl TableInterp {
    /// The lowercase name agents and CSV import use (`step`, `linear`, `cubic`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Step => "step",
            Self::Linear => "linear",
            Self::Cubic => "cubic",
        }
    }

    /// The RON spelling (`Step`, `Linear`, `Cubic`).
    #[must_use]
    pub fn ron_name(self) -> &'static str {
        match self {
            Self::Step => "Step",
            Self::Linear => "Linear",
            Self::Cubic => "Cubic",
        }
    }

    /// Parses either spelling, ignoring ASCII case.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "step" => Some(Self::Step),
            "linear" => Some(Self::Linear),
            "cubic" => Some(Self::Cubic),
            _ => None,
        }
    }

    fn core(self) -> Interp {
        match self {
            Self::Step => Interp::Step,
            Self::Linear => Interp::Linear,
            Self::Cubic => Interp::CubicHermite,
        }
    }
}

/// What a curve returns outside its keyed span, set per side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Extrap {
    /// The end key's value.
    #[default]
    Hold,
    /// Continue the end segment's slope (the end tangent for a cubic segment, 0 for a step).
    Linear,
    /// Repeat the keyed span; values absolute.
    Cycle,
    /// Repeat the span, each loop offset by `y_last - y_first` times the loop count.
    CycleOffset,
    /// Repeat the span mirrored on alternate loops.
    PingPong,
}

impl Extrap {
    /// The lowercase name agents use (`hold`, `linear`, `cycle`, `cycle_offset`, `ping_pong`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Linear => "linear",
            Self::Cycle => "cycle",
            Self::CycleOffset => "cycle_offset",
            Self::PingPong => "ping_pong",
        }
    }

    /// The RON spelling (`Hold`, `Linear`, `Cycle`, `CycleOffset`, `PingPong`).
    #[must_use]
    pub fn ron_name(self) -> &'static str {
        match self {
            Self::Hold => "Hold",
            Self::Linear => "Linear",
            Self::Cycle => "Cycle",
            Self::CycleOffset => "CycleOffset",
            Self::PingPong => "PingPong",
        }
    }

    /// Parses the lowercase name or the RON spelling.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "hold" | "Hold" => Some(Self::Hold),
            "linear" | "Linear" => Some(Self::Linear),
            "cycle" | "Cycle" => Some(Self::Cycle),
            "cycle_offset" | "CycleOffset" => Some(Self::CycleOffset),
            "ping_pong" | "PingPong" => Some(Self::PingPong),
            _ => None,
        }
    }
}

/// One key of a [`NamedCurve`]. `interp` (when set) controls the segment that starts at this key.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct CurveKey {
    /// Position on the curve's axis; finite and strictly increasing along the curve.
    pub x: f32,
    /// Value at `x`; finite.
    pub y: f32,
    /// Interpolation of the segment starting here (overrides the curve's and the table's).
    pub interp: Option<TableInterp>,
    /// Incoming tangent (value per unit `x`); overrides the derived one.
    pub in_tangent: Option<f32>,
    /// Outgoing tangent (value per unit `x`); overrides the derived one.
    pub out_tangent: Option<f32>,
}

impl CurveKey {
    /// A plain key at `(x, y)`.
    #[must_use]
    pub fn new(x: f32, y: f32) -> Self {
        Self {
            x,
            y,
            ..Self::default()
        }
    }
}

/// One named curve of a [`CurveTable`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NamedCurve {
    /// Unique name in the table, `[A-Za-z0-9_.-]{1,64}`.
    pub name: String,
    /// Extrapolation before the first key.
    pub pre: Extrap,
    /// Extrapolation after the last key.
    pub post: Extrap,
    /// Interpolation for this curve (overrides the table's default).
    pub interp: Option<TableInterp>,
    /// The keys, by strictly increasing `x`.
    pub keys: Vec<CurveKey>,
}

impl NamedCurve {
    /// A curve with `Hold` on both sides and the table's interpolation.
    #[must_use]
    pub fn new(name: &str, keys: Vec<CurveKey>) -> Self {
        Self {
            name: name.to_owned(),
            keys,
            ..Self::default()
        }
    }
}

/// A cheap reference to one curve of one [`CurveTable`]: skips the name lookup. A handle stays
/// valid across key edits and goes stale when curves are added, removed or renamed, and when a
/// reload or import replaces the table with one whose curve names or order differ. A handle is
/// never valid in a different table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CurveHandle {
    index: u32,
    epoch: u32,
}

impl CurveHandle {
    /// The curve's position in the table (file order).
    #[must_use]
    pub fn index(self) -> usize {
        self.index as usize
    }
}

/// One problem found while reading, validating or editing a curve table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurveIssue {
    /// Stable lowercase code (`curve_duplicate_x`, ...; see `book/agents/curve-tables.md`).
    pub code: &'static str,
    /// Error or warning.
    pub severity: Severity,
    /// Human-readable detail.
    pub message: String,
    /// The curve, when the issue belongs to one.
    pub curve: Option<String>,
    /// Line in the source file (1-based), when known (CSV).
    pub line: Option<usize>,
}

impl CurveIssue {
    /// An error with no location.
    #[must_use]
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: Severity::Error,
            message: message.into(),
            curve: None,
            line: None,
        }
    }

    /// Sets the curve.
    #[must_use]
    pub fn with_curve(mut self, curve: &str) -> Self {
        self.curve = Some(curve.to_owned());
        self
    }

    /// Sets the line.
    #[must_use]
    pub fn with_line(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }

    /// Whether the issue is an error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl std::fmt::Display for CurveIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.severity.name(), self.code)?;
        if let Some(line) = self.line {
            write!(f, " line {line}")?;
        }
        if let Some(curve) = &self.curve {
            write!(f, " curve `{curve}`")?;
        }
        write!(f, ": {}", self.message)
    }
}

/// A table that did not load or an edit that was refused: every error found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurveTableError {
    /// The issues (at least one error).
    pub issues: Vec<CurveIssue>,
}

impl CurveTableError {
    fn one(issue: CurveIssue) -> Self {
        Self { issues: vec![issue] }
    }

    /// The code of the first error (`curve_bad_file` when there is none).
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.issues
            .iter()
            .find(|issue| issue.is_error())
            .map_or("curve_bad_file", |issue| issue.code)
    }
}

impl std::fmt::Display for CurveTableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let issues: Vec<String> = self.issues.iter().map(ToString::to_string).collect();
        f.write_str(&issues.join("; "))
    }
}

impl std::error::Error for CurveTableError {}

/// One curve prepared for sampling: the core curve with resolved interpolation and tangents, plus
/// the extrapolation data.
#[derive(Debug)]
struct BakedCurve {
    curve: Curve<f32>,
    x_first: f32,
    x_last: f32,
    y_first: f32,
    y_last: f32,
    y_min: f32,
    y_max: f32,
    pre: Extrap,
    post: Extrap,
    pre_slope: f32,
    post_slope: f32,
    /// Non-finite `x` samples since the counter was last taken (`curve_sample_nonfinite`).
    nonfinite: AtomicU32,
}

impl Clone for BakedCurve {
    fn clone(&self) -> Self {
        Self {
            curve: self.curve.clone(),
            x_first: self.x_first,
            x_last: self.x_last,
            y_first: self.y_first,
            y_last: self.y_last,
            y_min: self.y_min,
            y_max: self.y_max,
            pre: self.pre,
            post: self.post,
            pre_slope: self.pre_slope,
            post_slope: self.post_slope,
            nonfinite: AtomicU32::new(self.nonfinite.load(Ordering::Relaxed)),
        }
    }
}

/// Derived tangents for every key: finite differences over the neighbouring keys (one-sided at the
/// ends), then the Fritsch-Carlson limiter applied segment by segment, left to right.
/// `value` in the f32 range: the nearest finite f32.
fn narrow(value: f64) -> f32 {
    value.clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32
}

fn derived_tangents(keys: &[CurveKey]) -> Vec<f32> {
    let n = keys.len();
    let mut tangents = vec![0.0_f32; n];
    if n < 2 {
        return tangents;
    }
    let secants: Vec<f32> = keys
        .windows(2)
        .map(|pair| {
            let secant = (pair[1].y - pair[0].y) / (pair[1].x - pair[0].x);
            if secant.is_finite() {
                secant
            } else {
                // Finite keys, but a slope steeper than f32 holds: the steepest finite one.
                narrow(
                    (f64::from(pair[1].y) - f64::from(pair[0].y))
                        / (f64::from(pair[1].x) - f64::from(pair[0].x)),
                )
            }
        })
        .collect();
    tangents[0] = secants[0];
    tangents[n - 1] = secants[n - 2];
    for (k, pair) in secants.windows(2).enumerate() {
        let (left, right) = (pair[0], pair[1]);
        // A sign change or a flat neighbour is a local extremum: a flat tangent keeps it there.
        tangents[k + 1] = if left * right <= 0.0 {
            0.0
        } else if (left + right).is_finite() {
            (left + right) * 0.5
        } else {
            left * 0.5 + right * 0.5
        };
    }
    for (k, &secant) in secants.iter().enumerate() {
        if secant == 0.0 {
            tangents[k] = 0.0;
            tangents[k + 1] = 0.0;
            continue;
        }
        let alpha = tangents[k] / secant;
        let beta = tangents[k + 1] / secant;
        let radius = alpha * alpha + beta * beta;
        if !radius.is_finite() {
            // Beside a much steeper neighbour (a secant of 1e-38 next to one of 250) the f32 ratio
            // overflowed and the limiter below made `0 * inf`: the curve sampled NaN (found by the
            // `curves.cooked` fuzz target, ADR-2015). The same limit in f64, where any ratio of two
            // f32 values and its square fit; every finite case keeps its f32 result bit for bit.
            let (secant, t0, t1) = (
                f64::from(secant),
                f64::from(tangents[k]),
                f64::from(tangents[k + 1]),
            );
            let (alpha, beta) = (t0 / secant, t1 / secant);
            let tau = 3.0 / (alpha * alpha + beta * beta).sqrt();
            tangents[k] = narrow(tau * alpha * secant);
            tangents[k + 1] = narrow(tau * beta * secant);
        } else if radius > 9.0 {
            let tau = 3.0 / radius.sqrt();
            tangents[k] = tau * alpha * secant;
            tangents[k + 1] = tau * beta * secant;
        }
    }
    tangents
}

fn bake(table_interp: TableInterp, named: &NamedCurve) -> BakedCurve {
    let derived = derived_tangents(&named.keys);
    let interp_of = |key: &CurveKey| key.interp.or(named.interp).unwrap_or(table_interp);
    let keyframes: Vec<Keyframe<f32>> = named
        .keys
        .iter()
        .zip(&derived)
        .map(|(key, &tangent)| Keyframe {
            time: key.x,
            value: key.y,
            interp: interp_of(key).core(),
            in_tangent: key.in_tangent.unwrap_or(tangent),
            out_tangent: key.out_tangent.unwrap_or(tangent),
        })
        .collect();
    let n = keyframes.len();
    let (x_first, y_first, x_last, y_last) = match (keyframes.first(), keyframes.last()) {
        (Some(first), Some(last)) => (first.time, first.value, last.time, last.value),
        _ => (0.0, 0.0, 0.0, 0.0),
    };
    let segment_slope = |a: &Keyframe<f32>, b: &Keyframe<f32>, end_tangent: f32| match a.interp {
        Interp::Step => 0.0,
        Interp::Linear => (b.value - a.value) / (b.time - a.time),
        Interp::CubicHermite => end_tangent,
    };
    let (pre_slope, post_slope) = if n >= 2 {
        (
            segment_slope(&keyframes[0], &keyframes[1], keyframes[0].out_tangent),
            segment_slope(&keyframes[n - 2], &keyframes[n - 1], keyframes[n - 1].in_tangent),
        )
    } else {
        (0.0, 0.0)
    };
    let (y_min, y_max) = named
        .keys
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), key| {
            (lo.min(key.y), hi.max(key.y))
        });
    BakedCurve {
        curve: Curve::new(keyframes),
        x_first,
        x_last,
        y_first,
        y_last,
        y_min,
        y_max,
        pre: named.pre,
        post: named.post,
        pre_slope,
        post_slope,
        nonfinite: AtomicU32::new(0),
    }
}

impl BakedCurve {
    fn inside(&self, x: f32) -> f32 {
        self.curve.sample(x).unwrap_or(self.y_first)
    }

    fn sample(&self, x: f32) -> f32 {
        if !x.is_finite() {
            self.nonfinite.fetch_add(1, Ordering::Relaxed);
            return self.y_first;
        }
        if x < self.x_first {
            self.outside(x, self.pre, true)
        } else if x > self.x_last {
            self.outside(x, self.post, false)
        } else {
            self.inside(x)
        }
    }

    fn outside(&self, x: f32, mode: Extrap, before: bool) -> f32 {
        let span = self.x_last - self.x_first;
        if span <= 0.0 {
            return self.y_first;
        }
        match mode {
            Extrap::Hold => {
                if before {
                    self.y_first
                } else {
                    self.y_last
                }
            }
            Extrap::Linear => {
                if before {
                    self.y_first + self.pre_slope * (x - self.x_first)
                } else {
                    self.y_last + self.post_slope * (x - self.x_last)
                }
            }
            Extrap::Cycle | Extrap::CycleOffset | Extrap::PingPong => {
                // The fold runs in f64: an f32 quotient far from the keys rounds to the wrong loop
                // (and every f32 at or above 2^24 is even, which froze ping-pong; ADR-1809).
                let offset = f64::from(x) - f64::from(self.x_first);
                let span64 = f64::from(span);
                let loops = (offset / span64).floor();
                let local = (offset - loops * span64).clamp(0.0, span64) as f32;
                match mode {
                    Extrap::Cycle => self.inside(self.x_first + local),
                    Extrap::CycleOffset => {
                        (f64::from(self.inside(self.x_first + local))
                            + loops * (f64::from(self.y_last) - f64::from(self.y_first)))
                            as f32
                    }
                    _ => {
                        if loops.rem_euclid(2.0) == 1.0 {
                            self.inside(self.x_last - local)
                        } else {
                            self.inside(self.x_first + local)
                        }
                    }
                }
            }
        }
    }
}

/// Whether `name` is a valid curve name: 1 to [`MAX_CURVE_NAME_LEN`] bytes of `[A-Za-z0-9_.-]`.
#[must_use]
pub fn is_valid_curve_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_CURVE_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Every problem in `curves`, in file order (an empty list means the curves build a table).
#[must_use]
pub fn validate_curves(curves: &[NamedCurve]) -> Vec<CurveIssue> {
    let mut issues = Vec::new();
    if curves.len() > MAX_CURVES {
        issues.push(CurveIssue::error(
            "curve_too_many_curves",
            format!("{} curves; a table holds at most {MAX_CURVES}", curves.len()),
        ));
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for curve in curves {
        let name = curve.name.as_str();
        if !is_valid_curve_name(name) {
            issues.push(
                CurveIssue::error(
                    "curve_bad_name",
                    format!("`{name}` is not 1 to {MAX_CURVE_NAME_LEN} characters of A-Z a-z 0-9 _ . -"),
                )
                .with_curve(name),
            );
        }
        if !seen.insert(name) {
            issues.push(
                CurveIssue::error("curve_duplicate_name", format!("two curves are named `{name}`"))
                    .with_curve(name),
            );
        }
        if curve.keys.is_empty() {
            issues.push(CurveIssue::error("curve_empty", "a curve needs at least one key").with_curve(name));
        }
        if curve.keys.len() > MAX_KEYS {
            issues.push(
                CurveIssue::error(
                    "curve_too_many_keys",
                    format!("{} keys; a curve holds at most {MAX_KEYS}", curve.keys.len()),
                )
                .with_curve(name),
            );
        }
        for key in &curve.keys {
            let tangents_finite =
                key.in_tangent.is_none_or(f32::is_finite) && key.out_tangent.is_none_or(f32::is_finite);
            if !key.x.is_finite() || !key.y.is_finite() || !tangents_finite {
                issues.push(
                    CurveIssue::error(
                        "curve_bad_number",
                        format!("key ({}, {}) has a non-finite number", key.x, key.y),
                    )
                    .with_curve(name),
                );
            }
        }
        for pair in curve.keys.windows(2) {
            let (a, b) = (pair[0].x, pair[1].x);
            if !a.is_finite() || !b.is_finite() {
                continue;
            }
            if a == b {
                issues.push(
                    CurveIssue::error("curve_duplicate_x", format!("two keys at x = {a}")).with_curve(name),
                );
            } else if b < a {
                issues.push(
                    CurveIssue::error(
                        "curve_unsorted_x",
                        format!("key x = {b} follows x = {a}; keys must have increasing x"),
                    )
                    .with_curve(name),
                );
            }
        }
    }
    issues
}

/// A table of named float curves (`.curves.ron` / `.curves.csv`). See the module docs.
///
/// The table is only built through validating constructors and edits, so every curve in it has
/// at least one key, finite numbers and strictly increasing `x`; sampling never fails.
#[derive(Debug, Clone)]
pub struct CurveTable {
    interp: TableInterp,
    curves: Vec<NamedCurve>,
    baked: Vec<BakedCurve>,
    index: BTreeMap<String, u32>,
    /// Bumped when curves are added, removed or renamed (handles of an older epoch are stale).
    epoch: u32,
}

impl PartialEq for CurveTable {
    fn eq(&self, other: &Self) -> bool {
        self.interp == other.interp && self.curves == other.curves
    }
}

impl Default for CurveTable {
    fn default() -> Self {
        Self::empty(TableInterp::Linear)
    }
}

impl CurveTable {
    /// A table with no curves.
    #[must_use]
    pub fn empty(interp: TableInterp) -> Self {
        Self {
            interp,
            curves: Vec::new(),
            baked: Vec::new(),
            index: BTreeMap::new(),
            epoch: 0,
        }
    }

    /// Builds a table from curves in file order.
    ///
    /// # Errors
    ///
    /// Every error [`validate_curves`] finds.
    pub fn new(interp: TableInterp, curves: Vec<NamedCurve>) -> Result<Self, CurveTableError> {
        let issues = validate_curves(&curves);
        if issues.iter().any(CurveIssue::is_error) {
            return Err(CurveTableError { issues });
        }
        let mut table = Self::empty(interp);
        table.curves = curves;
        table.rebake();
        Ok(table)
    }

    fn rebake(&mut self) {
        let _span =
            tracing::debug_span!(target: "regia_framework", "curve_table_bake", curves = self.curves.len())
                .entered();
        self.baked = self.curves.iter().map(|curve| bake(self.interp, curve)).collect();
        let index: BTreeMap<String, u32> = self
            .curves
            .iter()
            .enumerate()
            .map(|(i, curve)| (curve.name.clone(), i as u32))
            .collect();
        if index != self.index {
            self.epoch = next_epoch();
            self.index = index;
        }
    }

    /// Keeps `previous`'s handles valid when this table names the same curves at the same
    /// positions (a reload that only changed keys); otherwise this table keeps its own epoch, which
    /// no handle of `previous` carries.
    pub(crate) fn adopt_epoch(&mut self, previous: &CurveTable) {
        if !self.index.is_empty() && self.index == previous.index {
            self.epoch = previous.epoch;
        }
    }

    /// The table's default interpolation.
    #[must_use]
    pub fn interp(&self) -> TableInterp {
        self.interp
    }

    /// The curves in file order.
    #[must_use]
    pub fn curves(&self) -> &[NamedCurve] {
        &self.curves
    }

    /// The curve named `name`.
    #[must_use]
    pub fn curve(&self, name: &str) -> Option<&NamedCurve> {
        self.index.get(name).map(|&i| &self.curves[i as usize])
    }

    /// The number of curves.
    #[must_use]
    pub fn len(&self) -> usize {
        self.curves.len()
    }

    /// Whether the table has no curves.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.curves.is_empty()
    }

    /// Re-checks the table. A table in hand never has errors (constructors and edits refuse
    /// them), so this is empty unless a future warning applies.
    #[must_use]
    pub fn validate(&self) -> Vec<CurveIssue> {
        validate_curves(&self.curves)
    }

    /// A handle to the curve named `name`.
    #[must_use]
    pub fn handle(&self, name: &str) -> Option<CurveHandle> {
        self.index.get(name).map(|&index| CurveHandle {
            index,
            epoch: self.epoch,
        })
    }

    /// Whether `handle` still names a curve of this table.
    #[must_use]
    pub fn is_valid(&self, handle: CurveHandle) -> bool {
        handle.epoch == self.epoch && (handle.index as usize) < self.baked.len()
    }

    /// The curve's value at `x`, or `None` for a stale handle. A non-finite `x` returns the curve's
    /// first key value and counts a `curve_sample_nonfinite` warning.
    #[must_use]
    pub fn try_sample(&self, handle: CurveHandle, x: f32) -> Option<f32> {
        if handle.epoch != self.epoch {
            return None;
        }
        self.baked.get(handle.index as usize).map(|curve| curve.sample(x))
    }

    /// The curve's value at `x` (see [`try_sample`](Self::try_sample)); 0 for a stale handle.
    #[must_use]
    pub fn sample(&self, handle: CurveHandle, x: f32) -> f32 {
        self.try_sample(handle, x).unwrap_or(0.0)
    }

    /// The value of the curve named `name` at `x`, or `None` when there is no such curve.
    #[must_use]
    pub fn sample_named(&self, name: &str, x: f32) -> Option<f32> {
        self.index.get(name).map(|&i| self.baked[i as usize].sample(x))
    }

    /// `(x_min, x_max, y_min, y_max)` over the curve's keys; all 0 for a stale handle.
    #[must_use]
    pub fn range(&self, handle: CurveHandle) -> (f32, f32, f32, f32) {
        if handle.epoch != self.epoch {
            return (0.0, 0.0, 0.0, 0.0);
        }
        self.baked
            .get(handle.index as usize)
            .map_or((0.0, 0.0, 0.0, 0.0), |c| (c.x_first, c.x_last, c.y_min, c.y_max))
    }

    /// Takes the non-finite sample counts since the last call: `(curve, count)` for each curve
    /// that had any.
    pub fn take_nonfinite_counts(&self) -> Vec<(String, u32)> {
        self.baked
            .iter()
            .zip(&self.curves)
            .filter_map(|(baked, curve)| {
                let count = baked.nonfinite.swap(0, Ordering::Relaxed);
                (count > 0).then(|| (curve.name.clone(), count))
            })
            .collect()
    }

    /// Applies `edit` to a copy of the curves and keeps the result only if it validates.
    ///
    /// # Errors
    ///
    /// The edit's own error, or every validation error of the edited curves; the table is
    /// unchanged.
    pub fn edit(
        &mut self,
        edit: impl FnOnce(&mut Vec<NamedCurve>) -> Result<(), CurveIssue>,
    ) -> Result<(), CurveTableError> {
        let mut curves = self.curves.clone();
        edit(&mut curves).map_err(CurveTableError::one)?;
        let issues = validate_curves(&curves);
        if issues.iter().any(CurveIssue::is_error) {
            return Err(CurveTableError { issues });
        }
        self.curves = curves;
        self.rebake();
        Ok(())
    }

    /// Inserts `key` into curve `name`, replacing a key at the same `x`.
    ///
    /// # Errors
    ///
    /// `curve_not_found`, or the validation error the key would cause (non-finite numbers).
    pub fn set_key(&mut self, name: &str, key: CurveKey) -> Result<(), CurveTableError> {
        self.edit(|curves| {
            let curve = find_curve(curves, name)?;
            match curve.keys.iter().position(|k| k.x == key.x) {
                Some(at) => curve.keys[at] = key,
                None => {
                    let at = curve.keys.partition_point(|k| k.x < key.x);
                    curve.keys.insert(at, key);
                }
            }
            Ok(())
        })
    }

    /// Removes the key at exactly `x` from curve `name`.
    ///
    /// # Errors
    ///
    /// `curve_not_found`, `key_not_found`, or `curve_empty` when it is the curve's last key.
    pub fn remove_key(&mut self, name: &str, x: f32) -> Result<(), CurveTableError> {
        self.edit(|curves| {
            let curve = find_curve(curves, name)?;
            let at = curve.keys.iter().position(|k| k.x == x).ok_or_else(|| {
                CurveIssue::error("key_not_found", format!("curve `{name}` has no key at x = {x}"))
                    .with_curve(name)
            })?;
            curve.keys.remove(at);
            Ok(())
        })
    }

    /// Replaces curve `curve.name` (keeping its position) or appends it when absent.
    ///
    /// # Errors
    ///
    /// Every validation error of the new curve.
    pub fn upsert_curve(&mut self, curve: NamedCurve) -> Result<(), CurveTableError> {
        self.edit(|curves| {
            match curves.iter().position(|c| c.name == curve.name) {
                Some(at) => curves[at] = curve,
                None => curves.push(curve),
            }
            Ok(())
        })
    }

    /// Renames curve `from` to `to`.
    ///
    /// # Errors
    ///
    /// `curve_not_found`, `curve_bad_name` or `curve_duplicate_name`.
    pub fn rename_curve(&mut self, from: &str, to: &str) -> Result<(), CurveTableError> {
        self.edit(|curves| {
            find_curve(curves, from)?.name = to.to_owned();
            Ok(())
        })
    }

    /// Deletes curve `name`.
    ///
    /// # Errors
    ///
    /// `curve_not_found`.
    pub fn delete_curve(&mut self, name: &str) -> Result<(), CurveTableError> {
        self.edit(|curves| {
            let at = curves
                .iter()
                .position(|c| c.name == name)
                .ok_or_else(|| curve_not_found(name))?;
            curves.remove(at);
            Ok(())
        })
    }

    /// Sets the table's default interpolation.
    pub fn set_interp(&mut self, interp: TableInterp) {
        self.interp = interp;
        self.rebake();
    }
}

/// Epochs come from one process-wide counter, so two tables (or two loads of one file) never share
/// an epoch unless [`CurveTable::adopt_epoch`] carries one over: a handle of an older table is
/// stale in a newer one even when both have the same number of curves (audit-gameplay H6).
fn next_epoch() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    // 0 is the empty table's epoch; skip it on wrap.
    loop {
        let epoch = NEXT.fetch_add(1, Ordering::Relaxed);
        if epoch != 0 {
            return epoch;
        }
    }
}

fn curve_not_found(name: &str) -> CurveIssue {
    CurveIssue::error("curve_not_found", format!("no curve named `{name}`")).with_curve(name)
}

fn find_curve<'a>(curves: &'a mut [NamedCurve], name: &str) -> Result<&'a mut NamedCurve, CurveIssue> {
    curves
        .iter_mut()
        .find(|c| c.name == name)
        .ok_or_else(|| curve_not_found(name))
}

/// A reference to one curve of a curve-table asset, for data-table rows and chooser cells. It
/// derives `Reflect`, so a typed data-table row can hold it as a field and the validator pass
/// ([`validate_curve_refs`]) can find it (spec 51 § 2.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Reflect)]
pub struct CurveRef {
    /// Asset path of the `.curves.ron` / `.curves.csv` table.
    pub table: String,
    /// Curve name in that table.
    pub name: String,
}

impl CurveRef {
    /// Resolves the reference against `tables`.
    ///
    /// # Errors
    ///
    /// `curve_ref_unresolved` (a missing table or curve).
    pub fn resolve(&self, tables: &CurveTables) -> Result<CurveHandle, CurveIssue> {
        let table = tables.get(&self.table).ok_or_else(|| {
            CurveIssue::error(
                "curve_ref_unresolved",
                format!("no curve table `{}` is loaded", self.table),
            )
            .with_curve(&self.name)
        })?;
        table.handle(&self.name).ok_or_else(|| {
            CurveIssue::error(
                "curve_ref_unresolved",
                format!("curve table `{}` has no curve `{}`", self.table, self.name),
            )
            .with_curve(&self.name)
        })
    }

    /// The referenced curve's value at `x`, or `None` when the reference does not resolve.
    #[must_use]
    pub fn sample(&self, tables: &CurveTables, x: f32) -> Option<f32> {
        tables.get(&self.table)?.sample_named(&self.name, x)
    }
}

/// Appends every [`CurveRef`] reachable from `value` to `out`: a direct reference, or one
/// inside any reflected container — struct fields (nested structs and options included), tuple
/// and tuple-struct fields, fixed arrays and lists, map keys and values, and every enum
/// variant's fields (`Some` and user variants alike).
fn collect_curve_refs(value: &dyn regia_ecs::reflect::PartialReflect, out: &mut Vec<CurveRef>) {
    if let Some(reference) = value.try_downcast_ref::<CurveRef>() {
        out.push(reference.clone());
        return;
    }
    use regia_ecs::reflect::ReflectRef;
    match value.reflect_ref() {
        ReflectRef::Struct(fields) => {
            for index in 0..fields.field_len() {
                if let Some(field) = fields.field_at(index) {
                    collect_curve_refs(field, out);
                }
            }
        }
        ReflectRef::TupleStruct(fields) => {
            for index in 0..fields.field_len() {
                if let Some(field) = fields.field(index) {
                    collect_curve_refs(field, out);
                }
            }
        }
        ReflectRef::Tuple(fields) => {
            for index in 0..fields.field_len() {
                if let Some(field) = fields.field(index) {
                    collect_curve_refs(field, out);
                }
            }
        }
        ReflectRef::Array(items) => {
            for item in items.iter() {
                collect_curve_refs(item, out);
            }
        }
        ReflectRef::List(items) => {
            for item in items.iter() {
                collect_curve_refs(item, out);
            }
        }
        ReflectRef::Map(entries) => {
            for (key, entry) in entries.iter() {
                collect_curve_refs(key, out);
                collect_curve_refs(entry, out);
            }
        }
        ReflectRef::Enum(variant) => {
            // Every variant's fields: `Some` carries the payload here, and a user enum's
            // variants can hold rows too (`enum Damage { Curve(CurveRef), Flat(f32) }`).
            for index in 0..variant.field_len() {
                if let Some(field) = variant.field_at(index) {
                    collect_curve_refs(field, out);
                }
            }
        }
        _ => {}
    }
}

/// The validator pass for data-table rows holding [`CurveRef`]s (spec 51 § 2.1): every reference
/// found by reflection in every row is resolved against `tables`, and each one that does not
/// resolve is a `curve_ref_unresolved` error naming its row key, table and curve. Rows with no
/// reference are not touched. The walk reaches through any reflected container: struct fields
/// (nested structs and options included), tuples and tuple structs, fixed arrays and lists, map
/// keys and values, and every enum variant's fields.
///
/// This is a library call: a game runs it on its typed tables after load and after reload. The
/// agent's `tables.validate` does not run it — the erased tables it works on do not carry the
/// typed row reflection (see the book's curve-tables page).
///
/// # Example
///
/// ```
/// use regia_app::prelude::*;
/// use regia_framework::curve_table::{CurveRef, CurveTable, CurveTables, validate_curve_refs};
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Serialize, Deserialize, Reflect)]
/// struct Weapon {
///     falloff: CurveRef,
/// }
///
/// let table = regia_asset::data_table::DataTable::<Weapon>::from_ron_str(
///     r#"(format_version: 1, row_type: "Weapon", rows: {
///         "rifle": (falloff: (table: "none.curves.ron", name: "falloff")),
///     })"#,
/// )
/// .expect("table");
/// let mut tables = CurveTables::default();
/// tables.insert("falloff.curves.ron", CurveTable::empty(regia_framework::curve_table::TableInterp::Linear));
/// let issues = validate_curve_refs(&table, &tables);
/// assert_eq!(issues[0].code, "curve_ref_unresolved");
/// assert!(issues[0].message.contains("rifle"));
/// ```
#[must_use]
pub fn validate_curve_refs<R>(table: &DataTable<R>, tables: &CurveTables) -> Vec<CurveIssue>
where
    R: Typed + Serialize + DeserializeOwned + Send + Sync + Reflect + 'static,
{
    let mut issues = Vec::new();
    for (key, row) in table.iter() {
        let mut references = Vec::new();
        collect_curve_refs(row.as_reflect(), &mut references);
        for reference in references {
            if let Err(issue) = reference.resolve(tables) {
                issues.push(
                    CurveIssue::error(issue.code, format!("row `{key}`: {}", issue.message))
                        .with_curve(&reference.name),
                );
            }
        }
    }
    issues
}

#[cfg(test)]
mod tests;
