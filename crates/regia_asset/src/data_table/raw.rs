// SPDX-License-Identifier: MPL-2.0
//! Tables without their row type: what `regia table` works on.
//!
//! A [`RawTable`] keeps every cell as the RON literal text the file holds (`30`, `Rifle`,
//! `"ui/rifle.png"`, `(x: 1)`), split out of the file by a small scanner that knows RON's lexical
//! shape (strings, raw strings, characters, comments, nested brackets) but not its types. That is
//! enough to check keys and columns, convert between the RON and CSV forms, and migrate the
//! envelope, all without the game's row struct. Typed checks (does `damage` parse as a `u32`) run
//! in the game through [`super::DataTable`].

use super::{DATA_TABLE_FORMAT_VERSION, IssueCode, Severity, TableIssue, is_string_literal, unquote};
use crate::csv;

/// One row of a [`RawTable`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawRow {
    /// Row key.
    pub key: String,
    /// Line of the row in its file (1-based).
    pub line: usize,
    /// `(column, RON literal text)` in file order.
    pub cells: Vec<(String, String)>,
}

/// A table read without its row type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawTable {
    /// The file's `row_type` (empty when the file does not name one, as in format v0).
    pub row_type: String,
    /// The version the file was written in (v0 = bare list, v1 = envelope).
    pub source_version: u32,
    /// Every column in first-seen order (the CSV header order for CSV input).
    pub columns: Vec<String>,
    /// Rows in file order.
    pub rows: Vec<RawRow>,
}

/// How [`RawTable::from_csv`] decides which cells are strings.
#[derive(Clone, Debug, Default)]
pub struct CsvImport {
    /// Columns whose cells are always plain text (quoted into RON strings).
    pub text_columns: Vec<String>,
    /// Columns known to hold RON literals (never quoted).
    pub literal_columns: Vec<String>,
}

impl RawTable {
    /// Reads a `.table.ron` file (v1 envelope or v0 bare list).
    ///
    /// # Errors
    ///
    /// One `table_bad_file` (or `table_unsupported_version`) issue naming the line.
    pub fn from_ron(text: &str) -> Result<Self, TableIssue> {
        let mut s = Scanner::new(text);
        s.skip_ws();
        s.skip_inner_attributes().map_err(|e| s.issue(e))?;
        s.skip_ws();
        if s.peek() == Some(b'[') {
            return parse_v0(&mut s);
        }
        let mut table = RawTable {
            source_version: DATA_TABLE_FORMAT_VERSION,
            ..RawTable::default()
        };
        let fields = s.struct_fields().map_err(|e| s.issue(e))?;
        let mut version = None;
        let mut rows_at = None;
        for (name, start, end) in fields {
            match name.as_str() {
                "format_version" => {
                    version = Some(
                        text[start..end]
                            .trim()
                            .parse::<u32>()
                            .map_err(|_| s.issue_at(start, "`format_version` is not a number".to_owned()))?,
                    );
                }
                "row_type" => {
                    table.row_type = unquote(&text[start..end])
                        .ok_or_else(|| s.issue_at(start, "`row_type` is not a string".to_owned()))?;
                }
                "rows" => rows_at = Some(start),
                other => {
                    return Err(s.issue_at(start, format!("unknown table field `{other}`")));
                }
            }
        }
        let version = version.ok_or_else(|| s.issue_at(0, "the table has no `format_version`".to_owned()))?;
        if version > DATA_TABLE_FORMAT_VERSION {
            return Err(TableIssue::new(
                IssueCode::UnsupportedVersion,
                format!(
                    "table format v{version} is newer than this engine reads (v{DATA_TABLE_FORMAT_VERSION}); upgrade the engine"
                ),
            ));
        }
        table.source_version = version;
        let rows_at = rows_at.ok_or_else(|| s.issue_at(0, "the table has no `rows`".to_owned()))?;
        let mut rows = Scanner::new(text);
        rows.pos = rows_at;
        let mut lines = LineCounter::default();
        for (key_start, key_end, value_start, value_end) in rows.map_entries().map_err(|e| rows.issue(e))? {
            let key = unquote(&text[key_start..key_end])
                .ok_or_else(|| rows.issue_at(key_start, "a row key must be a string".to_owned()))?;
            let row = row_cells(text, key, lines.line(text, key_start), value_start, value_end)?;
            table.push_row(row);
        }
        Ok(table)
    }

    /// Reads a CSV table: the first record is the header and its first column is `key`.
    ///
    /// A column is plain text when [`CsvImport::text_columns`] names it, or when it is not named in
    /// [`CsvImport::literal_columns`] and one of its cells is not a RON literal (`ui/rifle.png`). Text
    /// cells become RON strings; an empty literal cell is left out of the row (the field's default).
    ///
    /// # Errors
    ///
    /// `table_bad_record` for a CSV syntax error or a record of the wrong length;
    /// `table_bad_file` for an empty input or a header whose first column is not `key`.
    pub fn from_csv(text: &str, import: &CsvImport) -> Result<Self, TableIssue> {
        let records = csv::parse(text)
            .map_err(|e| TableIssue::new(IssueCode::BadRecord, e.kind.to_string()).with_line(e.line))?;
        let Some((header, body)) = records.split_first() else {
            return Err(TableIssue::new(
                IssueCode::BadFile,
                "the CSV has no header record".to_owned(),
            ));
        };
        if header.fields.first().map(String::as_str) != Some("key") {
            return Err(TableIssue::new(
                IssueCode::BadFile,
                "the first column of the header must be `key`".to_owned(),
            )
            .with_line(header.line));
        }
        Ok(Self::from_csv_records(header.fields[1..].to_vec(), body, import))
    }

    /// The table of CSV records `body` under header `columns` (without `key`): the second half of
    /// [`Self::from_csv`], shared with the cooked form, which keeps the records as the file had them.
    /// Every record holds `columns.len() + 1` fields.
    pub(crate) fn from_csv_records(
        columns: Vec<String>,
        body: &[crate::csv::CsvRecord],
        import: &CsvImport,
    ) -> Self {
        let text_column: Vec<bool> = columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                if import.text_columns.contains(name) {
                    true
                } else if import.literal_columns.contains(name) {
                    false
                } else {
                    body.iter()
                        .filter_map(|r| r.fields.get(i + 1))
                        .any(|cell| !cell.is_empty() && !looks_like_literal(cell))
                }
            })
            .collect();
        let mut table = RawTable {
            source_version: DATA_TABLE_FORMAT_VERSION,
            columns: columns.clone(),
            ..RawTable::default()
        };
        for record in body {
            let key = record.fields.first().cloned().unwrap_or_default();
            let mut cells = Vec::with_capacity(columns.len());
            for (i, column) in columns.iter().enumerate() {
                let Some(cell) = record.fields.get(i + 1) else {
                    continue;
                };
                if text_column[i] {
                    cells.push((column.clone(), quote(cell)));
                } else if !cell.is_empty() {
                    cells.push((column.clone(), cell.clone()));
                }
            }
            table.rows.push(RawRow {
                key,
                line: record.line,
                cells,
            });
        }
        table
    }

    /// The table without its rows (row type, version, columns).
    pub(crate) fn clone_shape(&self) -> Self {
        Self {
            row_type: self.row_type.clone(),
            source_version: self.source_version,
            columns: self.columns.clone(),
            rows: Vec::new(),
        }
    }

    fn push_row(&mut self, row: RawRow) {
        for (column, _) in &row.cells {
            if !self.columns.contains(column) {
                self.columns.push(column.clone());
            }
        }
        self.rows.push(row);
    }

    /// Structural checks that need no row type: empty and duplicate keys, cells that are not one
    /// RON value, and rows that lack a column other rows have (a warning: the field may have a
    /// default).
    #[must_use]
    pub fn validate(&self) -> Vec<TableIssue> {
        let mut issues = Vec::new();
        let mut seen = std::collections::HashMap::new();
        for row in &self.rows {
            if row.key.trim().is_empty() {
                issues.push(
                    TableIssue::new(IssueCode::EmptyKey, "a row has an empty key".to_owned())
                        .with_line(row.line),
                );
            } else if let Some(first) = seen.insert(row.key.as_str(), row.line) {
                issues.push(
                    TableIssue::new(
                        IssueCode::DuplicateKey,
                        format!("key `{}` is used again (first on line {first})", row.key),
                    )
                    .with_key(&row.key)
                    .with_line(row.line),
                );
            }
            for (column, cell) in &row.cells {
                if !is_one_value(cell) {
                    issues.push(
                        TableIssue::new(IssueCode::BadCell, format!("`{cell}` is not one RON value"))
                            .with_key(&row.key)
                            .with_column(column)
                            .with_line(row.line),
                    );
                }
            }
            for column in &self.columns {
                if !row.cells.iter().any(|(c, _)| c == column) {
                    issues.push(
                        TableIssue::new(
                            IssueCode::MissingColumn,
                            format!(
                                "row `{}` has no `{column}` (its default applies, if it has one)",
                                row.key
                            ),
                        )
                        .with_key(&row.key)
                        .with_column(column)
                        .with_line(row.line)
                        .with_severity(Severity::Warning),
                    );
                }
            }
        }
        issues
    }

    /// Writes the table in the current `.table.ron` form (v1 envelope).
    #[must_use]
    pub fn to_ron(&self) -> String {
        let mut out = String::new();
        out.push_str("(\n");
        out.push_str(&format!("    format_version: {DATA_TABLE_FORMAT_VERSION},\n"));
        out.push_str(&format!("    row_type: {},\n", quote(&self.row_type)));
        out.push_str("    rows: {\n");
        for row in &self.rows {
            let cells: Vec<String> = row.cells.iter().map(|(c, v)| format!("{c}: {v}")).collect();
            out.push_str(&format!("        {}: ({}),\n", quote(&row.key), cells.join(", ")));
        }
        out.push_str("    },\n)\n");
        out
    }

    /// Writes the table as CSV: header `key` plus [`Self::columns`]; string literals become plain
    /// text, every other cell keeps its RON text, a missing cell is empty.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let mut out = String::new();
        let mut header = vec!["key".to_owned()];
        header.extend(self.columns.iter().cloned());
        csv::write_record(&mut out, &header);
        for row in &self.rows {
            let mut record = vec![row.key.clone()];
            for column in &self.columns {
                let cell = row
                    .cells
                    .iter()
                    .find(|(c, _)| c == column)
                    .map(|(_, v)| csv_cell(v))
                    .unwrap_or_default();
                record.push(cell);
            }
            csv::write_record(&mut out, &record);
        }
        out
    }

    /// Columns whose every present cell is a string literal (how `--into` learns the text columns
    /// of an existing table).
    #[must_use]
    pub fn string_columns(&self) -> Vec<String> {
        self.columns
            .iter()
            .filter(|column| {
                let mut cells = self
                    .rows
                    .iter()
                    .filter_map(|r| r.cells.iter().find(|(c, _)| c == *column))
                    .peekable();
                cells.peek().is_some() && cells.all(|(_, v)| is_string_literal(v))
            })
            .cloned()
            .collect()
    }
}

/// The CSV form of one RON cell: a string literal's text, any other literal as written.
pub(crate) fn csv_cell(ron_text: &str) -> String {
    if is_string_literal(ron_text) {
        unquote(ron_text).unwrap_or_else(|| ron_text.to_owned())
    } else {
        ron_text.to_owned()
    }
}

/// A RON string literal holding `text`.
pub(crate) fn quote(text: &str) -> String {
    // `ron::to_string` on a `str` cannot fail; fall back to a hand-escaped literal anyway.
    ron::to_string(text).unwrap_or_else(|_| format!("{text:?}"))
}

/// Whether a CSV cell reads as a RON literal rather than plain text: a number, `true`/`false`, a
/// quoted string or character, a bracketed value, or an identifier (an enum variant, `None`)
/// optionally followed by a bracketed payload (`Some(3)`, `Burst(2)`).
pub(crate) fn looks_like_literal(cell: &str) -> bool {
    let cell = cell.trim();
    let Some(first) = cell.chars().next() else {
        return false;
    };
    if matches!(first, '(' | '[' | '{' | '"' | '\'') || cell.starts_with("r#") || cell.starts_with("r\"") {
        return is_one_value(cell);
    }
    if first.is_ascii_digit() || first == '-' || first == '+' || first == '.' {
        return cell.parse::<f64>().is_ok()
            || cell.parse::<i128>().is_ok()
            || cell.starts_with("0x")
            || cell.starts_with("0b")
            || cell.starts_with("0o");
    }
    if first.is_alphabetic() || first == '_' {
        let ident_end = cell
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
            .map_or(cell.len(), |(i, _)| i);
        let rest = cell[ident_end..].trim_start();
        return rest.is_empty() || ((rest.starts_with('(') || rest.starts_with('{')) && is_one_value(rest));
    }
    false
}

/// Whether `text` is exactly one RON value (by the scanner's lexical rules).
pub(crate) fn is_one_value(text: &str) -> bool {
    let mut s = Scanner::new(text);
    s.skip_ws();
    if s.at_end() {
        return false;
    }
    match s.value() {
        Ok(_) => {
            s.skip_ws();
            s.at_end()
        }
        Err(_) => false,
    }
}

fn parse_v0(s: &mut Scanner<'_>) -> Result<RawTable, TableIssue> {
    let text = s.text;
    let mut table = RawTable::default();
    let mut lines = LineCounter::default();
    // `[("key", (field: value, ...)), ...]`
    s.expect(b'[').map_err(|e| s.issue(e))?;
    loop {
        s.skip_ws();
        if s.eat(b']') {
            break;
        }
        s.expect(b'(').map_err(|e| s.issue(e))?;
        s.skip_ws();
        let key_start = s.pos;
        let (_, key_end) = s.value().map_err(|e| s.issue(e))?;
        let key = unquote(&text[key_start..key_end])
            .ok_or_else(|| s.issue_at(key_start, "a row key must be a string".to_owned()))?;
        s.skip_ws();
        s.expect(b',').map_err(|e| s.issue(e))?;
        s.skip_ws();
        let value_start = s.pos;
        let (_, value_end) = s.value().map_err(|e| s.issue(e))?;
        s.skip_ws();
        s.eat(b',');
        s.skip_ws();
        s.expect(b')').map_err(|e| s.issue(e))?;
        let row = row_cells(text, key, lines.line(text, key_start), value_start, value_end)?;
        table.push_row(row);
        s.skip_ws();
        if !s.eat(b',') {
            s.skip_ws();
            s.expect(b']').map_err(|e| s.issue(e))?;
            break;
        }
    }
    s.skip_ws();
    if !s.at_end() {
        return Err(s.issue("text after the end of the table".to_owned()));
    }
    Ok(table)
}

fn row_cells(text: &str, key: String, line: usize, start: usize, end: usize) -> Result<RawRow, TableIssue> {
    let mut row = Scanner::new(&text[..end]);
    row.pos = start;
    let fields = row
        .struct_fields()
        .map_err(|e| row.issue(format!("row `{key}`: {e}")))?;
    Ok(RawRow {
        line,
        cells: fields
            .into_iter()
            .map(|(name, s, e)| (name, text[s..e].trim().to_owned()))
            .collect(),
        key,
    })
}

/// 1-based lines of increasing byte offsets, counted from the last offset asked: rows come in file order,
/// so a table's lines cost one pass over its text, not one pass per row (ADR-2132: `line_of` per row made
/// a 4,096-row load quadratic).
#[derive(Default)]
struct LineCounter {
    at: usize,
    line: usize,
}

impl LineCounter {
    fn line(&mut self, text: &str, at: usize) -> usize {
        let at = at.min(text.len());
        if at < self.at {
            // Not in order (it is, for every caller): count from the start.
            *self = Self::default();
        }
        self.line += text.as_bytes()[self.at..at]
            .iter()
            .filter(|&&b| b == b'\n')
            .count();
        self.at = at;
        self.line + 1
    }
}

/// 1-based line of byte offset `at`.
pub(crate) fn line_of(text: &str, at: usize) -> usize {
    1 + text.as_bytes()[..at.min(text.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

/// A byte cursor over RON text.
pub(crate) struct Scanner<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Scanner<'a> {
    pub(crate) fn new(text: &'a str) -> Self {
        Self { text, pos: 0 }
    }

    fn bytes(&self) -> &'a [u8] {
        self.text.as_bytes()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes().get(self.pos).copied()
    }

    fn at_end(&self) -> bool {
        self.pos >= self.text.len()
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.eat(b) {
            Ok(())
        } else {
            Err(format!("expected `{}`", b as char))
        }
    }

    fn issue(&self, message: String) -> TableIssue {
        self.issue_at(self.pos, message)
    }

    fn issue_at(&self, at: usize, message: String) -> TableIssue {
        TableIssue::new(IssueCode::BadFile, message).with_line(line_of(self.text, at))
    }

    /// Skips whitespace, `//` line comments and (nested) `/* */` block comments.
    fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(b) if b.is_ascii_whitespace() => self.pos += 1,
                Some(b'/') if self.bytes().get(self.pos + 1) == Some(&b'/') => {
                    while let Some(b) = self.peek() {
                        self.pos += 1;
                        if b == b'\n' {
                            break;
                        }
                    }
                }
                Some(b'/') if self.bytes().get(self.pos + 1) == Some(&b'*') => {
                    self.pos += 2;
                    let mut depth = 1usize;
                    while depth > 0 && !self.at_end() {
                        if self.bytes()[self.pos..].starts_with(b"/*") {
                            depth += 1;
                            self.pos += 2;
                        } else if self.bytes()[self.pos..].starts_with(b"*/") {
                            depth -= 1;
                            self.pos += 2;
                        } else {
                            self.pos += 1;
                        }
                    }
                }
                _ => return,
            }
        }
    }

    /// Skips `#![enable(...)]` extension attributes at the top of a file.
    fn skip_inner_attributes(&mut self) -> Result<(), String> {
        while self
            .bytes()
            .get(self.pos..)
            .is_some_and(|rest| rest.starts_with(b"#!"))
        {
            self.pos += 2;
            if self.peek() != Some(b'[') {
                return Err("expected `[` after `#!`".to_owned());
            }
            self.value_to_boundary(true)?;
            self.skip_ws();
        }
        Ok(())
    }

    fn ident(&mut self) -> Option<String> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80 {
                self.pos += 1;
            } else {
                break;
            }
        }
        (self.pos > start).then(|| self.text[start..self.pos].to_owned())
    }

    /// `Name?( field: value, ... )` → `(field, value start, value end)`.
    fn struct_fields(&mut self) -> Result<Vec<(String, usize, usize)>, String> {
        self.skip_ws();
        if self.peek().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_') {
            self.ident();
            self.skip_ws();
        }
        self.expect(b'(')?;
        let mut fields = Vec::new();
        loop {
            self.skip_ws();
            if self.eat(b')') {
                return Ok(fields);
            }
            let name = self.ident().ok_or_else(|| "expected a field name".to_owned())?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let (start, end) = self.value()?;
            fields.push((name, start, end));
            self.skip_ws();
            if !self.eat(b',') {
                self.skip_ws();
                self.expect(b')')?;
                return Ok(fields);
            }
        }
    }

    /// `{ key: value, ... }` → `(key start, key end, value start, value end)`.
    fn map_entries(&mut self) -> Result<Vec<(usize, usize, usize, usize)>, String> {
        self.skip_ws();
        self.expect(b'{')?;
        let mut entries = Vec::new();
        loop {
            self.skip_ws();
            if self.eat(b'}') {
                return Ok(entries);
            }
            let (ks, ke) = self.value()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let (vs, ve) = self.value()?;
            entries.push((ks, ke, vs, ve));
            self.skip_ws();
            if !self.eat(b',') {
                self.skip_ws();
                self.expect(b'}')?;
                return Ok(entries);
            }
        }
    }

    /// One value: returns its byte range. Stops before a `,`, `:` or closing bracket at depth 0.
    fn value(&mut self) -> Result<(usize, usize), String> {
        self.value_to_boundary(false)
    }

    /// Attributes end at their closing bracket even when another value follows (ADR-4864).
    /// Cell values retain their existing delimiter-based termination.
    fn value_to_boundary(&mut self, stop_after_group: bool) -> Result<(usize, usize), String> {
        let start = self.pos;
        let mut depth: Vec<u8> = Vec::new();
        loop {
            let Some(b) = self.peek() else {
                if depth.is_empty() && self.pos > start {
                    return Ok((start, self.pos));
                }
                return Err("unexpected end of text".to_owned());
            };
            match b {
                b'"' => self.string()?,
                b'r' if matches!(self.bytes().get(self.pos + 1), Some(b'"' | b'#')) => self.raw_string()?,
                b'\'' => self.char_literal()?,
                b'/' if matches!(self.bytes().get(self.pos + 1), Some(b'/' | b'*')) => self.skip_ws(),
                b'(' | b'[' | b'{' => {
                    depth.push(b);
                    self.pos += 1;
                }
                b')' | b']' | b'}' => {
                    let Some(open) = depth.pop() else {
                        if self.pos > start {
                            return Ok((start, trim_end(self.text, start, self.pos)));
                        }
                        return Err(format!("unexpected `{}`", b as char));
                    };
                    let want = match open {
                        b'(' => b')',
                        b'[' => b']',
                        _ => b'}',
                    };
                    if b != want {
                        return Err(format!("`{}` closes `{}`", b as char, open as char));
                    }
                    self.pos += 1;
                    if stop_after_group && depth.is_empty() {
                        return Ok((start, self.pos));
                    }
                }
                b',' | b':' if depth.is_empty() => {
                    if self.pos == start {
                        return Err("expected a value".to_owned());
                    }
                    return Ok((start, trim_end(self.text, start, self.pos)));
                }
                _ => self.pos += self.text[self.pos..].chars().next().map_or(1, char::len_utf8),
            }
            if depth.is_empty()
                && self.pos > start
                && matches!(
                    self.bytes()[self.pos - 1],
                    b')' | b']' | b'}' | b'"' | b'\'' | b'#'
                )
            {
                // A closed bracket or quote at depth 0 ends a value unless an identifier payload
                // follows (`Some(..)` is ident then bracket, handled by continuing above).
                let mut look = Scanner {
                    text: self.text,
                    pos: self.pos,
                };
                look.skip_ws();
                if matches!(look.peek(), None | Some(b',' | b':' | b')' | b']' | b'}')) {
                    return Ok((start, self.pos));
                }
            }
        }
    }

    fn string(&mut self) -> Result<(), String> {
        self.pos += 1;
        while let Some(b) = self.peek() {
            self.pos += 1;
            match b {
                b'\\' => {
                    if self.peek().is_some() {
                        self.pos += 1;
                    }
                }
                b'"' => return Ok(()),
                _ => {}
            }
        }
        Err("a string is not closed".to_owned())
    }

    fn raw_string(&mut self) -> Result<(), String> {
        self.pos += 1;
        let mut hashes = 0usize;
        while self.eat(b'#') {
            hashes += 1;
        }
        self.expect(b'"')?;
        let mut close = vec![b'"'];
        close.extend(std::iter::repeat_n(b'#', hashes));
        while !self.at_end() {
            if self.bytes()[self.pos..].starts_with(&close) {
                self.pos += close.len();
                return Ok(());
            }
            self.pos += 1;
        }
        Err("a raw string is not closed".to_owned())
    }

    fn char_literal(&mut self) -> Result<(), String> {
        self.pos += 1;
        while let Some(b) = self.peek() {
            self.pos += 1;
            match b {
                b'\\' => {
                    if self.peek().is_some() {
                        self.pos += 1;
                    }
                }
                b'\'' => return Ok(()),
                _ => {}
            }
        }
        Err("a character literal is not closed".to_owned())
    }
}

fn trim_end(text: &str, start: usize, end: usize) -> usize {
    start + text[start..end].trim_end().len()
}
