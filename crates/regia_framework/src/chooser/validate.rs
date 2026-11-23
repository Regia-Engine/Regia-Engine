// SPDX-License-Identifier: MPL-2.0
//! Chooser validation (spec 51 § 2.5): structure (no environment needed) and references (nested
//! `Chooser(..)` paths, cycles and depth, through a [`ChooserEnv`]).

use std::collections::BTreeSet;

use super::{
    Cell, ChooserEnv, ChooserIssue, ChooserTable, ChooserValue, Column, ColumnClass, ColumnKind,
    IssueSeverity, MAX_COLUMNS, MAX_DEPTH, MAX_PARAMS, MAX_RECENCY_WINDOW, MAX_ROWS, Mode, ResultEntry,
    ResultType, ValueKind, asset_kind,
};

fn issue(code: &'static str, message: impl Into<String>) -> ChooserIssue {
    ChooserIssue::error(code, message)
}

fn bad_float(v: f32) -> bool {
    !v.is_finite()
}

/// Whether `cell` is a cell of `column`'s kind (and its values are declared).
fn cell_problem(table: &ChooserTable, column: &Column, cell: &Cell) -> Option<(&'static str, String)> {
    let enum_variants = || -> Option<&Vec<String>> {
        match column.input.as_deref().and_then(|i| table.schema.inputs.get(i)) {
            Some(ValueKind::Enum(variants)) => Some(variants),
            _ => None,
        }
    };
    let undeclared = |v: &String| -> Option<(&'static str, String)> {
        match enum_variants() {
            Some(variants) if !variants.contains(v) => {
                Some(("chooser_bad_cell", format!("`{v}` is not a variant of the input")))
            }
            _ => None,
        }
    };
    match (&column.kind, cell) {
        (_, Cell::Any) if column.kind.class() != ColumnClass::Draw => None,
        (ColumnKind::Bool, Cell::Bool(_)) => None,
        (ColumnKind::IntRange, Cell::IntRange(min, max)) => (min > max && column.wrap.is_none())
            .then(|| ("chooser_bad_range", format!("min {min} > max {max}"))),
        (ColumnKind::FloatRange, Cell::FloatRange(min, max)) => {
            if bad_float(*min) || bad_float(*max) {
                Some(("chooser_bad_number", "range bound is not finite".to_owned()))
            } else {
                (min > max && column.wrap.is_none())
                    .then(|| ("chooser_bad_range", format!("min {min} > max {max}")))
            }
        }
        (ColumnKind::Enum, Cell::Is(v) | Cell::IsNot(v)) => undeclared(v),
        (ColumnKind::Enum, Cell::OneOf(set)) => set.iter().find_map(undeclared),
        (ColumnKind::Name, Cell::Name(_)) | (ColumnKind::Tags, Cell::Tags(_)) => None,
        (ColumnKind::Asset, Cell::Is(_) | Cell::Kind(_)) => None,
        (ColumnKind::FloatDistance { .. }, Cell::Target(t)) => {
            bad_float(*t).then(|| ("chooser_bad_number", "target is not finite".to_owned()))
        }
        (ColumnKind::Weight, Cell::Weight(w)) => (bad_float(*w) || *w < 0.0).then(|| {
            (
                "chooser_bad_number",
                format!("weight {w} is not a finite number >= 0"),
            )
        }),
        (kind, Cell::Value(value)) if kind.class() == ColumnClass::Emit => {
            let declared = column.output.as_deref().and_then(|o| table.schema.outputs.get(o));
            match declared {
                Some(kind) if !kind.admits(value) => Some((
                    "chooser_bad_cell",
                    format!("value is not a {} the output declares", kind.name()),
                )),
                _ if matches!(value, ChooserValue::Float(v) if v.is_nan()) => {
                    Some(("chooser_bad_number", "NaN".to_owned()))
                }
                _ => None,
            }
        }
        _ => Some((
            "chooser_bad_cell",
            format!("not a {} cell", column.kind.ron_name()),
        )),
    }
}

/// Whether a row passes every gate whatever the context (all gate cells `Any`, nothing that can
/// drop it, and no nested result that could strike it).
fn always_passes(table: &ChooserTable, row_index: usize) -> bool {
    let row = &table.rows[row_index];
    if !row.enabled || matches!(row.result, ResultEntry::Chooser(_) | ResultEntry::Inline(_)) {
        return false;
    }
    table
        .columns
        .iter()
        .filter(|c| !c.disabled)
        .all(|c| match c.kind.class() {
            ColumnClass::Gate => matches!(row.cell(&c.id), Cell::Any),
            ColumnClass::Rank => {
                !matches!(
                    c.kind,
                    ColumnKind::FloatDistance {
                        drop_beyond: true,
                        ..
                    }
                ) || matches!(row.cell(&c.id), Cell::Any)
            }
            _ => true,
        })
}

/// Whether a result entry is a `Value(..)` holding a NaN float (spec 51 § 2.5: NaN anywhere is
/// `chooser_bad_number`; the text and cooked loaders refuse it first, a table built in code here).
fn is_nan_result(entry: &ResultEntry) -> bool {
    matches!(entry, ResultEntry::Value(ChooserValue::Float(v)) if v.is_nan())
}

fn result_problem(result_type: &ResultType, entry: &ResultEntry) -> Option<String> {
    match (result_type, entry) {
        (_, ResultEntry::Nothing | ResultEntry::Chooser(_)) => None,
        (ResultType::Asset(kind), ResultEntry::Asset(path)) => (kind != "any" && asset_kind(path) != kind)
            .then(|| format!("asset `{path}` is not of kind `{kind}`")),
        (ResultType::Value(kind), ResultEntry::Value(value)) => {
            (!kind.value_kind_admits(value)).then(|| format!("value is not a {kind:?}"))
        }
        (_, ResultEntry::Inline(table)) => (&table.result_type != result_type).then(|| {
            format!(
                "inline table yields {:?}, this table {result_type:?}",
                table.result_type
            )
        }),
        _ => Some(format!(
            "entry does not agree with the result type {result_type:?}"
        )),
    }
}

impl super::ScalarKind {
    fn value_kind_admits(self, value: &ChooserValue) -> bool {
        self.value_kind().admits(value)
    }
}

impl ChooserTable {
    /// The checks that need no other table: limits, ids, column bindings and kinds, cells, ranges,
    /// numbers, result types, unreachable rows (warning) and a missing catch-all (info). Inline
    /// tables are checked too (their row ids prefixed `outer/`).
    #[must_use]
    pub fn validate_structure(&self) -> Vec<ChooserIssue> {
        let mut issues = Vec::new();
        self.structure_into(&mut issues, "");
        issues
    }

    fn structure_into(&self, issues: &mut Vec<ChooserIssue>, prefix: &str) {
        let row_id = |id: &str| format!("{prefix}{id}");
        if self.rows.len() > MAX_ROWS {
            issues.push(issue(
                "chooser_too_many_rows",
                format!("{} rows; at most {MAX_ROWS}", self.rows.len()),
            ));
        }
        if self.columns.len() > MAX_COLUMNS {
            issues.push(issue(
                "chooser_too_many_columns",
                format!("{} columns; at most {MAX_COLUMNS}", self.columns.len()),
            ));
        }
        if self.schema.inputs.len() > MAX_PARAMS || self.schema.outputs.len() > MAX_PARAMS {
            issues.push(issue(
                "chooser_too_many_params",
                format!(
                    "{} inputs and {} outputs; at most {MAX_PARAMS} each",
                    self.schema.inputs.len(),
                    self.schema.outputs.len()
                ),
            ));
        }
        let mut seen = BTreeSet::new();
        let mut weights = 0;
        for column in &self.columns {
            if !seen.insert(column.id.as_str()) {
                issues.push(
                    issue(
                        "chooser_duplicate_column",
                        format!("two columns have id `{}`", column.id),
                    )
                    .with_column(&column.id),
                );
            }
            let class = column.kind.class();
            let (wanted, declared, side) = match class {
                ColumnClass::Gate | ColumnClass::Rank => (
                    column.input.as_deref(),
                    column.input.as_deref().and_then(|i| self.schema.inputs.get(i)),
                    "input",
                ),
                ColumnClass::Emit => (
                    column.output.as_deref(),
                    column.output.as_deref().and_then(|o| self.schema.outputs.get(o)),
                    "output",
                ),
                ColumnClass::Draw => {
                    weights += 1;
                    if weights > 1 {
                        issues.push(
                            issue("chooser_kind_mismatch", "a table has at most one Weight column")
                                .with_column(&column.id),
                        );
                    }
                    (None, None, "")
                }
            };
            if class != ColumnClass::Draw {
                match (wanted, declared) {
                    (None, _) => issues.push(
                        issue("chooser_unknown_input", format!("the column names no {side}"))
                            .with_column(&column.id),
                    ),
                    (Some(name), None) => issues.push(
                        issue(
                            "chooser_unknown_input",
                            format!("{side} `{name}` is not declared"),
                        )
                        .with_column(&column.id),
                    ),
                    (Some(name), Some(kind)) if !column.kind.fits(kind) => issues.push(
                        issue(
                            "chooser_kind_mismatch",
                            format!(
                                "{side} `{name}` is {} but the column is {}",
                                kind.name(),
                                column.kind.ron_name()
                            ),
                        )
                        .with_column(&column.id),
                    ),
                    _ => {}
                }
            }
            if let ColumnKind::FloatDistance {
                influence,
                max_distance,
                wrap,
                ..
            } = &column.kind
            {
                let bad_wrap = wrap.is_some_and(|(lo, hi)| bad_float(lo) || bad_float(hi) || lo >= hi);
                if bad_float(*max_distance)
                    || *max_distance <= 0.0
                    || bad_float(*influence)
                    || *influence < 0.0
                    || bad_wrap
                {
                    issues.push(
                        issue(
                            "chooser_bad_number",
                            "FloatDistance needs max_distance > 0, influence >= 0 and a wrap with lo < hi",
                        )
                        .with_column(&column.id),
                    );
                }
            }
            if let Some((lo, hi)) = column.wrap
                && (!lo.is_finite() || !hi.is_finite() || lo >= hi)
            {
                issues.push(issue("chooser_bad_number", "wrap needs finite lo < hi").with_column(&column.id));
            }
        }
        if self.near_best.is_some_and(f32::is_nan) {
            issues.push(issue("chooser_bad_number", "near_best is NaN"));
        }
        if let Some(recency) = self.recency
            && (recency.window == 0
                || recency.window > MAX_RECENCY_WINDOW
                || !(0.0..=1.0).contains(&recency.penalty))
        {
            issues.push(issue(
                "chooser_bad_number",
                format!("recency needs window in 1..={MAX_RECENCY_WINDOW} and penalty in [0, 1]"),
            ));
        }
        let mut rows_seen = BTreeSet::new();
        for row in &self.rows {
            let id = row_id(&row.id);
            if !rows_seen.insert(row.id.as_str()) {
                issues.push(
                    issue("chooser_duplicate_row", format!("two rows have id `{}`", row.id)).with_row(&id),
                );
            }
            for (column_id, cell) in &row.cells {
                let Some(column) = self.column(column_id) else {
                    issues.push(
                        issue("chooser_bad_cell", format!("no column `{column_id}`"))
                            .with_row(&id)
                            .with_column(column_id),
                    );
                    continue;
                };
                if let Some((code, message)) = cell_problem(self, column, cell) {
                    issues.push(issue(code, message).with_row(&id).with_column(column_id));
                }
            }
            if let Some(message) = result_problem(&self.result_type, &row.result) {
                issues.push(issue("chooser_result_type_mismatch", message).with_row(&id));
            }
            if is_nan_result(&row.result) {
                issues.push(issue("chooser_bad_number", "result value is NaN").with_row(&id));
            }
            if let ResultEntry::Inline(table) = &row.result {
                table.structure_into(issues, &format!("{id}/"));
            }
        }
        if let Some(catch_all) = &self.catch_all {
            if let Some(message) = result_problem(&self.result_type, &catch_all.result) {
                issues.push(issue("chooser_result_type_mismatch", message).with_row(&row_id("catch_all")));
            }
            if is_nan_result(&catch_all.result) {
                issues
                    .push(issue("chooser_bad_number", "result value is NaN").with_row(&row_id("catch_all")));
            }
            for (name, value) in &catch_all.outputs {
                match self.schema.outputs.get(name) {
                    None => issues.push(
                        issue(
                            "chooser_unknown_input",
                            format!("output `{name}` is not declared"),
                        )
                        .with_row(&row_id("catch_all"))
                        .with_column(name),
                    ),
                    Some(kind) if !kind.admits(value) => issues.push(
                        issue("chooser_bad_cell", format!("value is not a {}", kind.name()))
                            .with_row(&row_id("catch_all"))
                            .with_column(name),
                    ),
                    _ if matches!(value, ChooserValue::Float(v) if v.is_nan()) => issues.push(
                        issue("chooser_bad_number", "output value is NaN")
                            .with_row(&row_id("catch_all"))
                            .with_column(name),
                    ),
                    _ => {}
                }
            }
            if let ResultEntry::Inline(table) = &catch_all.result {
                table.structure_into(issues, &row_id("catch_all/"));
            }
        }
        if matches!(self.mode, Mode::First)
            && let Some(blocker) = (0..self.rows.len()).find(|&i| always_passes(self, i))
        {
            for row in self.rows.iter().skip(blocker + 1).filter(|r| r.enabled) {
                issues.push(
                    issue(
                        "chooser_unreachable_row",
                        format!(
                            "row `{}` always passes, so this row is never selected",
                            self.rows[blocker].id
                        ),
                    )
                    .with_severity(IssueSeverity::Warning)
                    .with_row(&row_id(&row.id)),
                );
            }
        }
        let has_catch_all_row = (0..self.rows.len()).any(|i| always_passes(self, i));
        if self.catch_all.is_none() && !has_catch_all_row {
            issues.push(
                issue(
                    "chooser_no_catch_all",
                    "no catch_all and no row that always passes: some contexts select nothing",
                )
                .with_severity(IssueSeverity::Info),
            );
        }
    }

    /// Every issue: [`validate_structure`](Self::validate_structure) plus nested references
    /// resolved through `env` (`chooser_ref_unresolved`, a nested table of another result type,
    /// `chooser_cycle`, `chooser_depth_exceeded`). `path` is this table's own asset path, when it
    /// has one (it lets a table that reaches itself be reported as a cycle).
    #[must_use]
    pub fn validate(&self, env: &dyn ChooserEnv) -> Vec<ChooserIssue> {
        self.validate_at(None, env)
    }

    /// [`validate`](Self::validate) for the table at asset path `path`.
    #[must_use]
    pub fn validate_at(&self, path: Option<&str>, env: &dyn ChooserEnv) -> Vec<ChooserIssue> {
        let mut issues = self.validate_structure();
        let mut stack: Vec<String> = path.map(str::to_owned).into_iter().collect();
        self.references_into(env, &mut stack, 0, &mut issues);
        issues
    }

    fn references_into(
        &self,
        env: &dyn ChooserEnv,
        stack: &mut Vec<String>,
        depth: u32,
        issues: &mut Vec<ChooserIssue>,
    ) {
        let entries = self
            .rows
            .iter()
            .map(|r| (r.id.as_str(), &r.result))
            .chain(self.catch_all.as_ref().map(|c| ("catch_all", &c.result)));
        for (row, entry) in entries {
            let nested: &ChooserTable = match entry {
                ResultEntry::Chooser(path) => {
                    if stack.iter().any(|p| p == path) {
                        issues.push(
                            issue(
                                "chooser_cycle",
                                format!("`{path}` reaches itself through nested choosers"),
                            )
                            .with_row(row),
                        );
                        continue;
                    }
                    match env.chooser(path) {
                        Some(table) => {
                            if table.result_type != self.result_type {
                                issues.push(
                                    issue(
                                        "chooser_result_type_mismatch",
                                        format!(
                                            "`{path}` yields {:?}, this table {:?}",
                                            table.result_type, self.result_type
                                        ),
                                    )
                                    .with_row(row),
                                );
                            }
                            table
                        }
                        None => {
                            issues.push(
                                issue("chooser_ref_unresolved", format!("no chooser `{path}` is loaded"))
                                    .with_row(row),
                            );
                            continue;
                        }
                    }
                }
                ResultEntry::Inline(table) => table,
                _ => continue,
            };
            if depth + 1 > MAX_DEPTH {
                issues.push(
                    issue(
                        "chooser_depth_exceeded",
                        format!("nested deeper than {MAX_DEPTH}"),
                    )
                    .with_row(row),
                );
                continue;
            }
            let pushed = if let ResultEntry::Chooser(path) = entry {
                stack.push(path.clone());
                true
            } else {
                false
            };
            nested.references_into(env, stack, depth + 1, issues);
            if pushed {
                stack.pop();
            }
        }
    }
}
