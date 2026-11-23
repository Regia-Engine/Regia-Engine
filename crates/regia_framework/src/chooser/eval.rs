// SPDX-License-Identifier: MPL-2.0
//! Chooser evaluation (spec 51 §§ 2.3-2.6): gates, costs, selection by mode, the weighted draw with
//! recency, nested tables, the catch-all, outputs and the optional trace.

use regia_core::math::Rng;
use smallvec::SmallVec;

use super::{
    COST_EPSILON, Cell, ChooserContext, ChooserEnv, ChooserOutcome, ChooserTable, ChooserValue, Column,
    ColumnClass, ColumnKind, MAX_DEPTH, Mode, RecentPicks, ResultEntry, Row, asset_kind,
};

/// How much a trace records for rejected rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TraceDetail {
    /// Stop at a row's first failing column.
    #[default]
    Brief,
    /// Test every column of a rejected row (the editor's and agent's "why not this row").
    Full,
}

/// One cell's verdict in a trace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// The gate passed.
    Pass,
    /// The gate failed (or a `drop_beyond` distance was too far).
    Fail,
    /// A rank column added this cost.
    Score(f32),
    /// Not tested (a disabled column, or after the first failure in `Brief` detail).
    Skipped,
}

/// One cell in a trace.
#[derive(Debug, Clone, PartialEq)]
pub struct CellTrace {
    /// Column id.
    pub column: String,
    /// The verdict.
    pub verdict: Verdict,
    /// Short explanation (`input 0.4 not in [0.5, 1.0]`).
    pub detail: String,
}

/// What happened to one row.
#[derive(Debug, Clone, PartialEq)]
pub enum RowOutcome {
    /// The row is disabled.
    Disabled,
    /// This column rejected it.
    RejectedBy(String),
    /// It passed every gate.
    Candidate {
        /// Total cost of its rank columns.
        cost: f32,
        /// Its draw weight: the `Weight` cell (1 without a weight column), before the recency
        /// penalty, which depends on the draw's memory and is applied when the draw runs.
        weight: f32,
    },
    /// `First` mode stopped before it.
    NotEvaluated,
    /// It was selected, but its nested chooser returned nothing, so it was struck.
    NestedEmpty,
    /// It was selected.
    Selected,
}

/// One row in a trace.
#[derive(Debug, Clone, PartialEq)]
pub struct RowTrace {
    /// Row id.
    pub id: String,
    /// Whether the row is enabled.
    pub enabled: bool,
    /// Cell verdicts in column order.
    pub cells: Vec<CellTrace>,
    /// The outcome.
    pub outcome: RowOutcome,
}

/// A record of every decision of one evaluation ([`ChooserTable::evaluate_traced`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChooserTrace {
    /// The table's path or label (set by the caller; empty for an anonymous table).
    pub table: String,
    /// The table's mode.
    pub mode: Mode,
    /// The inputs the columns read, with their values (absent inputs are left out).
    pub inputs: Vec<(String, ChooserValue)>,
    /// Every row in order.
    pub rows: Vec<RowTrace>,
    /// The `u` of the weighted draw, when one was taken.
    pub draw: Option<f32>,
    /// Whether the catch-all answered.
    pub used_catch_all: bool,
    /// The selected row, if any.
    pub selected: Option<String>,
    /// The outputs written.
    pub outputs: Vec<(String, ChooserValue)>,
    /// Traces of nested tables, in evaluation order.
    pub nested: Vec<ChooserTrace>,
    /// Codes raised during evaluation (`chooser_depth_exceeded`, `chooser_ref_unresolved`).
    pub issues: Vec<&'static str>,
}

/// A candidate row.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    row: usize,
    cost: f32,
    weight: f32,
}

/// The nesting limit was passed; evaluation falls back to the top table's catch-all.
struct DepthExceeded;

/// Mutable state shared by one evaluation and its nested tables.
struct Run<'a> {
    env: &'a dyn ChooserEnv,
    rng: Option<&'a mut Rng>,
}

/// One table's selection before outputs are written.
struct Picked {
    result: ResultEntry,
    row: Option<String>,
    used_catch_all: bool,
    outputs: Vec<(String, ChooserValue)>,
}

impl Picked {
    fn into_outcome(self) -> ChooserOutcome {
        ChooserOutcome {
            result: self.result,
            row: self.row,
            used_catch_all: self.used_catch_all,
            outputs: self.outputs,
        }
    }
}

fn input<'c>(ctx: &'c ChooserContext, column: &Column) -> Option<&'c ChooserValue> {
    column.input.as_deref().and_then(|name| ctx.input(name))
}

/// Folds `v` into `[lo, hi)` (a circular axis).
fn fold(v: f64, lo: f64, hi: f64) -> f64 {
    let span = hi - lo;
    if span <= 0.0 {
        return v;
    }
    lo + (v - lo).rem_euclid(span)
}

fn in_range(v: f64, min: f64, max: f64, wrap: Option<(f64, f64)>) -> bool {
    match wrap {
        Some((lo, hi)) => {
            let v = fold(v, lo, hi);
            let hit = |v: f64| {
                if min <= max {
                    min <= v && v <= max
                } else {
                    v >= min || v <= max
                }
            };
            // `fold` maps into `[lo, hi)`, so `hi` lands on `lo`: both are the same heading.
            hit(v) || (v == lo && hit(hi))
        }
        None => min <= v && v <= max,
    }
}

/// Whether a gate cell passes for the input (absent input: only `Any` passes; NaN fails).
fn gate_passes(column: &Column, cell: &Cell, value: Option<&ChooserValue>) -> bool {
    if matches!(cell, Cell::Any) {
        return true;
    }
    let Some(value) = value else {
        return false;
    };
    match (cell, value) {
        (Cell::Bool(c), ChooserValue::Bool(v)) => c == v,
        // Without a wrap the bounds compare as i64, exactly: past 2^53 an f64 rounds neighbours
        // together (ADR-1809). A wrapped (circular) axis is an f64 range by definition.
        (Cell::IntRange(min, max), ChooserValue::Int(v)) => match column.wrap {
            None => min <= v && v <= max,
            Some(_) => in_range(*v as f64, *min as f64, *max as f64, column.wrap),
        },
        (Cell::FloatRange(min, max), ChooserValue::Float(v)) => {
            !v.is_nan() && in_range(f64::from(*v), f64::from(*min), f64::from(*max), column.wrap)
        }
        (Cell::Is(c), ChooserValue::Enum(v)) | (Cell::Is(c), ChooserValue::Asset(v)) => c == v,
        (Cell::IsNot(c), ChooserValue::Enum(v)) => c != v,
        (Cell::OneOf(set), ChooserValue::Enum(v)) => set.iter().any(|c| c == v),
        (Cell::Kind(kind), ChooserValue::Asset(path)) => asset_kind(path) == kind,
        (Cell::Name(c), ChooserValue::Name(v)) => {
            if column.case_sensitive {
                c == v
            } else {
                c.eq_ignore_ascii_case(v)
            }
        }
        (Cell::Tags(query), ChooserValue::Tags(tags)) => tags.matches(query),
        _ => false,
    }
}

fn describe(value: Option<&ChooserValue>) -> String {
    match value {
        None => "absent".to_owned(),
        Some(ChooserValue::Bool(v)) => v.to_string(),
        Some(ChooserValue::Int(v)) => v.to_string(),
        Some(ChooserValue::Float(v)) => v.to_string(),
        Some(ChooserValue::Enum(v) | ChooserValue::Name(v) | ChooserValue::Asset(v)) => format!("`{v}`"),
        Some(ChooserValue::Tags(tags)) => format!("{:?}", tags.names()),
    }
}

fn gate_detail(cell: &Cell, value: Option<&ChooserValue>, passed: bool) -> String {
    let input = describe(value);
    let verb = if passed { "matches" } else { "does not match" };
    match cell {
        Cell::Any => "any".to_owned(),
        Cell::IntRange(a, b) => format!(
            "input {input} {} [{a}, {b}]",
            if passed { "in" } else { "not in" }
        ),
        Cell::FloatRange(a, b) => format!(
            "input {input} {} [{a}, {b}]",
            if passed { "in" } else { "not in" }
        ),
        Cell::Bool(c) => format!("input {input} {verb} {c}"),
        Cell::Is(c) => format!("input {input} {verb} is `{c}`"),
        Cell::IsNot(c) => format!("input {input} {verb} is not `{c}`"),
        Cell::OneOf(set) => format!("input {input} {verb} one of {set:?}"),
        Cell::Kind(kind) => format!("input {input} {verb} kind `{kind}`"),
        Cell::Name(c) => format!("input {input} {verb} `{c}`"),
        Cell::Tags(query) => format!("input {input} {verb} {}", query.to_ron().unwrap_or_default()),
        _ => format!("input {input}"),
    }
}

/// The cost of a rank cell and whether `drop_beyond` rejects the row. An absent or NaN input, and
/// a gap that is not finite (wrapped or not), costs the full `influence` and is beyond
/// `max_distance`.
fn rank_cost(kind: &ColumnKind, cell: &Cell, value: Option<&ChooserValue>) -> (f32, bool) {
    let ColumnKind::FloatDistance {
        influence,
        max_distance,
        wrap,
        drop_beyond,
    } = kind
    else {
        return (0.0, false);
    };
    let Cell::Target(target) = cell else {
        return (0.0, false);
    };
    let gap = match value {
        Some(ChooserValue::Float(a)) if !a.is_nan() => {
            let d = (a - target).abs();
            match wrap {
                // An infinite gap (an infinite input, or a difference past `f32::MAX`) has no
                // place on the wrap: it stays infinite, so it is beyond `max_distance` as unwrapped.
                Some((lo, hi)) if hi > lo && d.is_finite() => {
                    let span = hi - lo;
                    let d = d.rem_euclid(span);
                    d.min(span - d)
                }
                _ => d,
            }
        }
        _ => f32::INFINITY,
    };
    let clamped = gap.min(*max_distance);
    (
        *influence * clamped / *max_distance,
        *drop_beyond && gap > *max_distance,
    )
}

impl ChooserTable {
    /// Tests one row: `Ok((cost, weight))` when it passes every gate (and no `drop_beyond`
    /// rejects it), `Err(column index)` for the first failing column. With a trace, records every
    /// cell (every column in `Full` detail).
    fn test_row(
        &self,
        row: &Row,
        ctx: &ChooserContext,
        trace: Option<(&mut Vec<CellTrace>, TraceDetail)>,
    ) -> Result<(f32, f32), usize> {
        let (mut cells, detail) = match trace {
            Some((cells, detail)) => (Some(cells), detail),
            None => (None, TraceDetail::Brief),
        };
        let mut failed: Option<usize> = None;
        let mut cost = 0.0_f32;
        let mut weight = 1.0_f32;
        for (index, column) in self.columns.iter().enumerate() {
            let class = column.kind.class();
            if class == ColumnClass::Emit {
                continue;
            }
            if column.disabled || (failed.is_some() && detail == TraceDetail::Brief) {
                if let Some(cells) = cells.as_deref_mut()
                    && class != ColumnClass::Draw
                {
                    let detail = if column.disabled {
                        "column disabled"
                    } else {
                        "after the first failure"
                    };
                    cells.push(CellTrace {
                        column: column.id.clone(),
                        verdict: Verdict::Skipped,
                        detail: detail.to_owned(),
                    });
                }
                continue;
            }
            let cell = row.cell(&column.id);
            let value = input(ctx, column);
            match class {
                ColumnClass::Gate => {
                    let passed = gate_passes(column, cell, value);
                    if !passed && failed.is_none() {
                        failed = Some(index);
                    }
                    if let Some(cells) = cells.as_deref_mut() {
                        cells.push(CellTrace {
                            column: column.id.clone(),
                            verdict: if passed { Verdict::Pass } else { Verdict::Fail },
                            detail: gate_detail(cell, value, passed),
                        });
                    }
                }
                ColumnClass::Rank => {
                    let (add, beyond) = rank_cost(&column.kind, cell, value);
                    cost += add;
                    if beyond && failed.is_none() {
                        failed = Some(index);
                    }
                    if let Some(cells) = cells.as_deref_mut() {
                        cells.push(CellTrace {
                            column: column.id.clone(),
                            verdict: if beyond {
                                Verdict::Fail
                            } else {
                                Verdict::Score(add)
                            },
                            detail: if beyond {
                                format!("input {} beyond max_distance", describe(value))
                            } else {
                                format!("input {} cost {add}", describe(value))
                            },
                        });
                    }
                }
                ColumnClass::Draw => {
                    if let Cell::Weight(w) = cell {
                        weight = *w;
                    }
                }
                ColumnClass::Emit => {}
            }
        }
        match failed {
            Some(index) => Err(index),
            None => Ok((cost, weight)),
        }
    }

    /// The outputs the row's emit columns write, in column order.
    fn row_outputs(&self, row: &Row) -> Vec<(String, ChooserValue)> {
        let mut out = Vec::new();
        for column in &self.columns {
            if column.disabled || column.kind.class() != ColumnClass::Emit {
                continue;
            }
            if let (Some(name), Some(Cell::Value(value))) = (&column.output, row.cells.get(&column.id)) {
                out.push((name.clone(), value.clone()));
            }
        }
        out
    }

    /// Resolves a result entry: plain entries as they are, nested tables evaluated one level
    /// deeper. `Ok(None)` when a nested table selects nothing (or its path does not resolve).
    fn resolve(
        &self,
        entry: &ResultEntry,
        run: &mut Run<'_>,
        ctx: &ChooserContext,
        depth: u32,
        trace: Option<&mut ChooserTrace>,
        detail: TraceDetail,
    ) -> Result<Option<Picked>, DepthExceeded> {
        let nested = match entry {
            ResultEntry::Chooser(path) => match run.env.chooser(path) {
                Some(table) => (table, path.as_str()),
                None => {
                    if let Some(trace) = trace {
                        trace.issues.push("chooser_ref_unresolved");
                    }
                    tracing::warn!(target: "regia_framework", code = "chooser_ref_unresolved", path = %path, "nested chooser not loaded");
                    return Ok(None);
                }
            },
            ResultEntry::Inline(table) => (table.as_ref(), "inline"),
            plain => {
                return Ok(Some(Picked {
                    result: plain.clone(),
                    row: None,
                    used_catch_all: false,
                    outputs: Vec::new(),
                }));
            }
        };
        if depth + 1 > MAX_DEPTH {
            return Err(DepthExceeded);
        }
        let (table, label) = nested;
        match trace {
            Some(trace) => {
                let mut sub = ChooserTrace {
                    table: label.to_owned(),
                    ..ChooserTrace::default()
                };
                let picked = table.select(run, ctx, depth + 1, None, Some(&mut sub), detail);
                trace.nested.push(sub);
                picked
            }
            None => table.select(run, ctx, depth + 1, None, None, detail),
        }
    }

    /// Picks among `candidates` by mode; `None` when there are none. `Weighted` takes one draw
    /// when the total weight is positive.
    fn pick(
        &self,
        candidates: &[Candidate],
        run: &mut Run<'_>,
        mem: Option<&RecentPicks>,
        draw: &mut Option<f32>,
    ) -> Option<usize> {
        let first = candidates.first()?;
        match self.mode {
            Mode::First | Mode::All => Some(0),
            Mode::Best => {
                let mut best = 0;
                let mut best_cost = first.cost;
                for (i, c) in candidates.iter().enumerate().skip(1) {
                    if c.cost < best_cost - COST_EPSILON {
                        best = i;
                        best_cost = c.cost;
                    }
                }
                Some(best)
            }
            Mode::Weighted => {
                let best_cost = candidates.iter().map(|c| c.cost).fold(f32::INFINITY, f32::min);
                let band = self.near_best.map(|t| t.max(0.0));
                let takes_part = |c: &Candidate| band.is_none_or(|t| c.cost <= best_cost + t);
                let penalized = |c: &Candidate, use_penalty: bool| -> f32 {
                    let mut w = c.weight.max(0.0);
                    if use_penalty
                        && let (Some(recency), Some(mem)) = (self.recency, mem)
                        && mem.contains_recent(&self.rows[c.row].id, recency.window)
                    {
                        w *= recency.penalty;
                    }
                    w
                };
                // The roulette sums in f64: f32 weights near f32::MAX would add up to infinity and
                // every draw would land on the last row (ADR-1809).
                let sum = |use_penalty: bool| -> f64 {
                    candidates
                        .iter()
                        .filter(|c| takes_part(c))
                        .map(|c| f64::from(penalized(c, use_penalty)))
                        .sum()
                };
                let mut use_penalty = true;
                let mut total = sum(true);
                if total <= 0.0 {
                    let raw = sum(false);
                    if raw > 0.0 {
                        // Every participant was zeroed by the recency penalty alone: ignore it.
                        use_penalty = false;
                        total = raw;
                    }
                }
                if total <= 0.0 {
                    return candidates.iter().position(takes_part);
                }
                let u = run.rng.as_deref_mut().map_or(0.0, Rng::f32);
                *draw = Some(u);
                let target = f64::from(u) * total;
                let mut running = 0.0_f64;
                let mut last_positive = None;
                for (i, c) in candidates.iter().enumerate() {
                    if !takes_part(c) {
                        continue;
                    }
                    let w = penalized(c, use_penalty);
                    if w <= 0.0 {
                        continue;
                    }
                    running += f64::from(w);
                    last_positive = Some(i);
                    if running > target {
                        return Some(i);
                    }
                }
                last_positive
            }
        }
    }

    /// One table's full selection: scan, pick, resolve nested results (striking rows whose nested
    /// table is empty), else the catch-all.
    fn select(
        &self,
        run: &mut Run<'_>,
        ctx: &ChooserContext,
        depth: u32,
        mem: Option<&RecentPicks>,
        mut trace: Option<&mut ChooserTrace>,
        detail: TraceDetail,
    ) -> Result<Option<Picked>, DepthExceeded> {
        if let Some(trace) = trace.as_deref_mut() {
            trace.mode = self.mode;
            trace.inputs = self.read_inputs(ctx);
        }
        let mut candidates: SmallVec<[Candidate; 16]> = SmallVec::new();
        let mut chosen: Option<(usize, Picked)> = None;
        let mut draw = None;
        let first_mode = matches!(self.mode, Mode::First | Mode::All);
        for (index, row) in self.rows.iter().enumerate() {
            if chosen.is_some() && first_mode {
                if let Some(trace) = trace.as_deref_mut() {
                    trace.rows.push(RowTrace {
                        id: row.id.clone(),
                        enabled: row.enabled,
                        cells: Vec::new(),
                        outcome: RowOutcome::NotEvaluated,
                    });
                }
                continue;
            }
            if !row.enabled {
                if let Some(trace) = trace.as_deref_mut() {
                    trace.rows.push(RowTrace {
                        id: row.id.clone(),
                        enabled: false,
                        cells: Vec::new(),
                        outcome: RowOutcome::Disabled,
                    });
                }
                continue;
            }
            let mut cells = Vec::new();
            let tested = self.test_row(row, ctx, trace.is_some().then_some((&mut cells, detail)));
            let outcome = match tested {
                // Only a trace names the rejecting column: a plain evaluation allocates nothing per row.
                Err(column) if trace.is_some() => RowOutcome::RejectedBy(self.columns[column].id.clone()),
                Err(_) => continue,
                Ok((cost, weight)) => {
                    let candidate = Candidate {
                        row: index,
                        cost,
                        weight,
                    };
                    if first_mode {
                        // `First`: resolve now; a struck row lets the scan continue.
                        match self.resolve(&row.result, run, ctx, depth, trace.as_deref_mut(), detail)? {
                            Some(picked) => {
                                chosen = Some((index, picked));
                                RowOutcome::Selected
                            }
                            None => RowOutcome::NestedEmpty,
                        }
                    } else {
                        candidates.push(candidate);
                        RowOutcome::Candidate { cost, weight }
                    }
                }
            };
            if let Some(trace) = trace.as_deref_mut() {
                trace.rows.push(RowTrace {
                    id: row.id.clone(),
                    enabled: true,
                    cells,
                    outcome,
                });
            }
        }
        if !first_mode {
            while let Some(at) = self.pick(&candidates, run, mem, &mut draw) {
                let candidate = candidates[at];
                let row = &self.rows[candidate.row];
                match self.resolve(&row.result, run, ctx, depth, trace.as_deref_mut(), detail)? {
                    Some(picked) => {
                        chosen = Some((candidate.row, picked));
                        break;
                    }
                    None => {
                        if let Some(trace) = trace.as_deref_mut()
                            && let Some(entry) = trace.rows.iter_mut().find(|r| r.id == row.id)
                        {
                            entry.outcome = RowOutcome::NestedEmpty;
                        }
                        candidates.remove(at);
                    }
                }
            }
        }
        if let Some(trace) = trace.as_deref_mut() {
            trace.draw = draw;
        }
        let picked = match chosen {
            Some((index, nested)) => {
                let row = &self.rows[index];
                let mut outputs = self.row_outputs(row);
                outputs.extend(nested.outputs);
                if let Some(trace) = trace.as_deref_mut()
                    && let Some(entry) = trace.rows.iter_mut().find(|r| r.id == row.id)
                {
                    entry.outcome = RowOutcome::Selected;
                }
                Some(Picked {
                    result: nested.result,
                    row: Some(row.id.clone()),
                    used_catch_all: false,
                    outputs,
                })
            }
            None => {
                if let Some(catch_all) = &self.catch_all {
                    self.resolve(&catch_all.result, run, ctx, depth, trace.as_deref_mut(), detail)?
                        .map(|nested| {
                            let mut outputs: Vec<(String, ChooserValue)> = catch_all
                                .outputs
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect();
                            outputs.extend(nested.outputs);
                            Picked {
                                result: nested.result,
                                row: None,
                                used_catch_all: true,
                                outputs,
                            }
                        })
                } else {
                    None
                }
            }
        };
        if let Some(trace) = trace
            && let Some(picked) = &picked
        {
            trace.used_catch_all = picked.used_catch_all;
            trace.selected.clone_from(&picked.row);
            trace.outputs.clone_from(&picked.outputs);
        }
        Ok(picked)
    }

    /// The inputs this table's enabled gate and rank columns read, with their values.
    fn read_inputs(&self, ctx: &ChooserContext) -> Vec<(String, ChooserValue)> {
        let mut out: Vec<(String, ChooserValue)> = Vec::new();
        for column in &self.columns {
            if column.disabled {
                continue;
            }
            if let Some(name) = &column.input
                && let Some(value) = ctx.input(name)
                && !out.iter().any(|(n, _)| n == name)
            {
                out.push((name.clone(), value.clone()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The top-level evaluation. A depth overrun answers with this table's catch-all.
    /// A `Chooser` or `Inline` catch-all is evaluated once, one level down (the same budget
    /// [`evaluate_all`](Self::evaluate_all) gives that expansion). If that evaluation itself
    /// exceeds depth, the result is [`ResultEntry::Nothing`] and no partial rows are kept.
    fn run_top(
        &self,
        env: &dyn ChooserEnv,
        ctx: &mut ChooserContext,
        rng: Option<&mut Rng>,
        mem: Option<&mut RecentPicks>,
        mut trace: Option<&mut ChooserTrace>,
        detail: TraceDetail,
    ) -> Option<ChooserOutcome> {
        let mut run = Run { env, rng };
        ctx.clear_outputs();
        // Bind the result before the error arm reborrows `run` and `trace`.
        let selected = self.select(&mut run, ctx, 0, mem.as_deref(), trace.as_deref_mut(), detail);
        let picked = match selected {
            Ok(picked) => picked,
            Err(DepthExceeded) => self.catch_all_after_depth(&mut run, ctx, trace, detail),
        };
        let picked = picked?;
        for (name, value) in &picked.outputs {
            ctx.write_output(name, value.clone());
        }
        if self.mode == Mode::Weighted
            && let (Some(mem), Some(row)) = (mem, &picked.row)
        {
            mem.record(row);
        }
        Some(picked.into_outcome())
    }

    /// The catch-all after the selected row's nesting passed [`MAX_DEPTH`].
    ///
    /// A plain result is returned as stored. A `Chooser` or `Inline` result is resolved once
    /// at depth 0 (so the nested table is entered at depth 1). A second depth overrun answers
    /// [`ResultEntry::Nothing`]. An unresolved reference answers nothing (`None`), the same as
    /// a catch-all reached because no row was selected. Neither path keeps rows from the overflow.
    fn catch_all_after_depth(
        &self,
        run: &mut Run<'_>,
        ctx: &ChooserContext,
        mut trace: Option<&mut ChooserTrace>,
        detail: TraceDetail,
    ) -> Option<Picked> {
        tracing::warn!(target: "regia_framework", code = "chooser_depth_exceeded", "chooser nesting deeper than {MAX_DEPTH}; answering with the catch-all");
        if let Some(trace) = trace.as_deref_mut() {
            trace.issues.push("chooser_depth_exceeded");
            trace.used_catch_all = self.catch_all.is_some();
            trace.selected = None;
        }
        let catch_all = self.catch_all.as_ref()?;
        let base_outputs: Vec<(String, ChooserValue)> = catch_all
            .outputs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let resolved = self.resolve(&catch_all.result, run, ctx, 0, trace.as_deref_mut(), detail);
        match resolved {
            Ok(Some(nested)) => {
                let mut outputs = base_outputs;
                outputs.extend(nested.outputs);
                if let Some(trace) = trace.as_deref_mut() {
                    trace.used_catch_all = true;
                    trace.outputs.clone_from(&outputs);
                }
                Some(Picked {
                    result: nested.result,
                    row: None,
                    used_catch_all: true,
                    outputs,
                })
            }
            Ok(None) => {
                if let Some(trace) = trace.as_deref_mut() {
                    trace.used_catch_all = false;
                }
                None
            }
            Err(DepthExceeded) => {
                tracing::warn!(target: "regia_framework", code = "chooser_depth_exceeded", "chooser catch-all nesting deeper than {MAX_DEPTH}; answering with Nothing");
                if let Some(trace) = trace {
                    trace.issues.push("chooser_depth_exceeded");
                    trace.outputs.clone_from(&base_outputs);
                }
                Some(Picked {
                    result: ResultEntry::Nothing,
                    row: None,
                    used_catch_all: true,
                    outputs: base_outputs,
                })
            }
        }
    }

    /// Evaluates the table for `ctx` (spec 51 § 2.3), writing the winner's outputs into `ctx`
    /// (outputs of an earlier evaluation are cleared first). `Weighted` takes one `rng.f32()` per
    /// draw and records the pick in `mem`; every other mode leaves `rng` untouched. An `All`
    /// table answers with its first candidate (use [`evaluate_all`](Self::evaluate_all) for the
    /// list).
    pub fn evaluate(
        &self,
        env: &dyn ChooserEnv,
        ctx: &mut ChooserContext,
        rng: &mut Rng,
        mem: Option<&mut RecentPicks>,
    ) -> Option<ChooserOutcome> {
        let _span = tracing::trace_span!(target: "regia_framework", "chooser_eval").entered();
        self.run_top(env, ctx, Some(rng), mem, None, TraceDetail::Brief)
    }

    /// Like [`evaluate`](Self::evaluate), and returns a [`ChooserTrace`] of every decision.
    /// `RecentPicks` are read but never recorded (a "what if" evaluation).
    pub fn evaluate_traced(
        &self,
        env: &dyn ChooserEnv,
        ctx: &mut ChooserContext,
        rng: &mut Rng,
        mem: Option<&mut RecentPicks>,
        detail: TraceDetail,
    ) -> (Option<ChooserOutcome>, ChooserTrace) {
        let _span = tracing::trace_span!(target: "regia_framework", "chooser_eval_traced").entered();
        let mut trace = ChooserTrace::default();
        let mut scratch = mem.map(|m| m.clone());
        let outcome = self.run_top(env, ctx, Some(rng), scratch.as_mut(), Some(&mut trace), detail);
        (outcome, trace)
    }

    /// Every candidate in row order, nested tables expanded in place (each with their own
    /// candidates), else the catch-all. Takes no randomness: `Weighted` weights are ignored.
    /// The outputs of every entry are written into `ctx` in order (later entries win).
    pub fn evaluate_all(&self, env: &dyn ChooserEnv, ctx: &mut ChooserContext) -> Vec<ChooserOutcome> {
        let _span = tracing::trace_span!(target: "regia_framework", "chooser_eval_all").entered();
        ctx.clear_outputs();
        let mut out = Vec::new();
        let ok = self.all_into(env, ctx, 0, &mut out).is_ok();
        if !ok {
            tracing::warn!(target: "regia_framework", code = "chooser_depth_exceeded", "chooser nesting deeper than {MAX_DEPTH}");
            out.clear();
        }
        if out.is_empty()
            && let Some(catch_all) = &self.catch_all
        {
            let mut nested = Vec::new();
            // A nested catch-all that nests too deep expands to nothing (`evaluate` answers
            // `Nothing`), never to the rows it reached before the overflow (ADR-1809).
            let nested_table = match &catch_all.result {
                ResultEntry::Chooser(path) => env.chooser(path),
                ResultEntry::Inline(table) => Some(table.as_ref()),
                _ => None,
            };
            if let Some(table) = nested_table
                && table.all_into(env, ctx, 1, &mut nested).is_err()
            {
                tracing::warn!(target: "regia_framework", code = "chooser_depth_exceeded", "chooser catch-all nesting deeper than {MAX_DEPTH}");
                nested.clear();
            }
            let result = match &catch_all.result {
                ResultEntry::Chooser(_) | ResultEntry::Inline(_) => None,
                plain => Some(plain.clone()),
            };
            let outputs: Vec<(String, ChooserValue)> = catch_all
                .outputs
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            match result {
                Some(result) => out.push(ChooserOutcome {
                    result,
                    row: None,
                    used_catch_all: true,
                    outputs,
                }),
                None => out.extend(nested.into_iter().map(|mut o| {
                    let mut all = outputs.clone();
                    all.append(&mut o.outputs);
                    o.outputs = all;
                    o.row = None;
                    o.used_catch_all = true;
                    o
                })),
            }
        }
        for outcome in &out {
            for (name, value) in &outcome.outputs {
                ctx.write_output(name, value.clone());
            }
        }
        out
    }

    fn all_into(
        &self,
        env: &dyn ChooserEnv,
        ctx: &ChooserContext,
        depth: u32,
        out: &mut Vec<ChooserOutcome>,
    ) -> Result<(), DepthExceeded> {
        if depth > MAX_DEPTH {
            return Err(DepthExceeded);
        }
        for row in &self.rows {
            if !row.enabled || self.test_row(row, ctx, None).is_err() {
                continue;
            }
            let outputs = self.row_outputs(row);
            let nested_table = match &row.result {
                ResultEntry::Chooser(path) => env.chooser(path),
                ResultEntry::Inline(table) => Some(table.as_ref()),
                plain => {
                    out.push(ChooserOutcome {
                        result: plain.clone(),
                        row: Some(row.id.clone()),
                        used_catch_all: false,
                        outputs,
                    });
                    continue;
                }
            };
            let Some(table) = nested_table else {
                continue;
            };
            let mut nested = Vec::new();
            table.all_into(env, ctx, depth + 1, &mut nested)?;
            for mut entry in nested {
                let mut all = outputs.clone();
                all.append(&mut entry.outputs);
                entry.outputs = all;
                entry.row = Some(row.id.clone());
                out.push(entry);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod range_tests {
    use super::in_range;

    #[test]
    fn a_wrapped_range_matches_its_upper_endpoint() {
        let wrap = Some((-180.0, 180.0));
        assert!(in_range(180.0, 90.0, 180.0, wrap), "the back of the arc");
        assert!(
            in_range(-180.0, 90.0, 180.0, wrap),
            "the same heading from the other side"
        );
        assert!(!in_range(0.0, 90.0, 180.0, wrap));
        assert!(in_range(-180.0, -180.0, -170.0, wrap));
    }
}
