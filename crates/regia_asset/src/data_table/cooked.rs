// SPDX-License-Identifier: MPL-2.0
//! The cooked (pak) form of a data table (`data-table-cooked` v1, ADR-2132).
//!
//! A little-endian `u32` version, then postcard. The cook runs without the game's row struct, so the
//! payload keeps what the source said in the form the typed loader reads: a `.table.ron` table as its
//! keys and cell RON texts (columns by index), a `.table.csv` table as its records (the typed CSV
//! rules, which decide whether a cell is plain text from the row struct's field types, run at load
//! exactly as for the text file). The cook checks the structure first; a table with structural errors
//! does not cook.
//!
//! A cooked payload is not trusted: [`CookedDataTable::from_bytes`] checks the version, that nothing
//! follows the table, every column index and every record length. Loading is one pass over the rows:
//! no nesting, so no recursion a crafted payload could drive. It is a cache: a shape change bumps
//! [`DATA_TABLE_COOKED_VERSION`], a payload of another version is refused by name
//! (`table_unsupported_version`) and the project is cooked again; no source file changes shape.

use serde::{Deserialize, Serialize};

use super::raw::RawTable;
use super::{
    CsvImport, DATA_TABLE_FORMAT_VERSION, DataTable, IssueCode, RawRow, TableFormat, TableIssue,
    TableLoadError, columns_of, csv_import_for, read_raw_ron,
};
use crate::csv::{self, CsvRecord};

/// Version of the cooked data-table payload (`data-table-cooked`).
pub const DATA_TABLE_COOKED_VERSION: u32 = 1;

/// Type path of a cooked data table in the cooked header (`TypePathHash::of`).
pub const DATA_TABLE_COOKED_TYPE_PATH: &str = "regia_asset::data_table::CookedDataTable";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WireRow {
    key: String,
    line: u32,
    /// `(column index, RON literal text)` in file order.
    cells: Vec<(u32, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WireRecord {
    line: u32,
    /// The key, then one field per column, as the CSV held them (quoting removed).
    fields: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum WireBody {
    Ron(Vec<WireRow>),
    Csv(Vec<WireRecord>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Wire {
    row_type: String,
    columns: Vec<String>,
    body: WireBody,
}

/// A data table in its cooked form: what the cook writes and a packaged game loads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CookedDataTable {
    wire: Wire,
}

fn bad_file(message: impl Into<String>) -> TableIssue {
    TableIssue::new(IssueCode::BadFile, message.into())
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl CookedDataTable {
    /// Cooks source `text` in `format`: reads it without the row type (migrating an old `.table.ron`),
    /// checks the structure and keeps the rows. Warnings (a row without a column other rows have) do
    /// not stop the cook.
    ///
    /// # Errors
    ///
    /// Every structural error: the file does not read, an empty or repeated key, a cell that is not
    /// one RON value.
    pub fn from_text(text: &str, format: TableFormat) -> Result<Self, Vec<TableIssue>> {
        let _span = tracing::debug_span!(target: "regia_asset", "data_table_cook").entered();
        let (raw, wire) = match format {
            TableFormat::Ron => {
                let raw = read_raw_ron(text).map_err(|issue| vec![issue])?;
                let rows = raw
                    .rows
                    .iter()
                    .map(|row| WireRow {
                        key: row.key.clone(),
                        line: to_u32(row.line),
                        cells: row
                            .cells
                            .iter()
                            .map(|(column, value)| {
                                let at = raw.columns.iter().position(|c| c == column).unwrap_or(0);
                                (to_u32(at), value.clone())
                            })
                            .collect(),
                    })
                    .collect();
                let wire = Wire {
                    row_type: raw.row_type.clone(),
                    columns: raw.columns.clone(),
                    body: WireBody::Ron(rows),
                };
                (raw, wire)
            }
            TableFormat::Csv => {
                let raw = RawTable::from_csv(text, &CsvImport::default()).map_err(|issue| vec![issue])?;
                let records = csv::parse(text).map_err(|e| {
                    vec![TableIssue::new(IssueCode::BadRecord, e.kind.to_string()).with_line(e.line)]
                })?;
                let body = records
                    .into_iter()
                    .skip(1)
                    .map(|r| WireRecord {
                        line: to_u32(r.line),
                        fields: r.fields,
                    })
                    .collect();
                let wire = Wire {
                    row_type: raw.row_type.clone(),
                    columns: raw.columns.clone(),
                    body: WireBody::Csv(body),
                };
                (raw, wire)
            }
        };
        let errors: Vec<TableIssue> = raw.validate().into_iter().filter(TableIssue::is_error).collect();
        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(Self { wire })
    }

    /// Reads a cooked payload (version word plus postcard) and checks its structure.
    ///
    /// # Errors
    ///
    /// `table_unsupported_version` for a version this build does not read; `table_bad_file` for
    /// damaged or truncated bytes, bytes after the table, a column index out of range or a column
    /// named twice in a row; `table_bad_record` for a CSV record of the wrong length.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, TableIssue> {
        let Some((version, body)) = bytes.split_first_chunk::<4>() else {
            return Err(bad_file("cooked data table is shorter than its version"));
        };
        let version = u32::from_le_bytes(*version);
        if version == 0 || version > DATA_TABLE_COOKED_VERSION {
            return Err(TableIssue::new(
                IssueCode::UnsupportedVersion,
                format!(
                    "cooked version {version}; this engine reads cooked data tables up to {DATA_TABLE_COOKED_VERSION}; cook the project again"
                ),
            ));
        }
        let (wire, rest) = postcard::take_from_bytes::<Wire>(body)
            .map_err(|e| bad_file(format!("cooked data table: {e}")))?;
        if !rest.is_empty() {
            return Err(bad_file(format!(
                "{} bytes after the cooked data table",
                rest.len()
            )));
        }
        let columns = wire.columns.len();
        match &wire.body {
            WireBody::Ron(rows) => {
                let mut seen = vec![usize::MAX; columns];
                for (index, row) in rows.iter().enumerate() {
                    for (column, _) in &row.cells {
                        let column = *column as usize;
                        let Some(slot) = seen.get_mut(column) else {
                            return Err(bad_file(format!(
                                "row `{}`: column {column} of {columns}",
                                row.key
                            )));
                        };
                        if *slot == index {
                            return Err(bad_file(format!("row `{}` names a column twice", row.key)));
                        }
                        *slot = index;
                    }
                }
            }
            WireBody::Csv(records) => {
                if let Some(record) = records.iter().find(|r| r.fields.len() != columns + 1) {
                    return Err(TableIssue::new(
                        IssueCode::BadRecord,
                        format!(
                            "a record holds {} fields, the header {}",
                            record.fields.len(),
                            columns + 1
                        ),
                    )
                    .with_line(record.line as usize));
                }
            }
        }
        Ok(Self { wire })
    }

    /// The payload: version word plus postcard.
    ///
    /// # Errors
    ///
    /// The encoder failed (it does not for a table in memory).
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let out = DATA_TABLE_COOKED_VERSION.to_le_bytes().to_vec();
        postcard::to_extend(&self.wire, out).map_err(|e| e.to_string())
    }

    /// The source form the table was cooked from.
    #[must_use]
    pub fn format(&self) -> TableFormat {
        match self.wire.body {
            WireBody::Ron(_) => TableFormat::Ron,
            WireBody::Csv(_) => TableFormat::Csv,
        }
    }

    /// The row struct's type path the source named (empty for a CSV table or a v0 file).
    #[must_use]
    pub fn row_type(&self) -> &str {
        &self.wire.row_type
    }

    /// Number of rows.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.wire.body {
            WireBody::Ron(rows) => rows.len(),
            WireBody::Csv(records) => records.len(),
        }
    }

    /// Whether the table has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The untyped table. For a CSV table `import` decides which columns are plain text, as in
    /// [`RawTable::from_csv`]; a RON table ignores it.
    #[must_use]
    pub fn to_raw(&self, import: &CsvImport) -> RawTable {
        match &self.wire.body {
            WireBody::Ron(rows) => RawTable {
                row_type: self.wire.row_type.clone(),
                source_version: DATA_TABLE_FORMAT_VERSION,
                columns: self.wire.columns.clone(),
                rows: rows
                    .iter()
                    .map(|row| RawRow {
                        key: row.key.clone(),
                        line: row.line as usize,
                        cells: row
                            .cells
                            .iter()
                            .map(|(column, value)| {
                                let name = self
                                    .wire
                                    .columns
                                    .get(*column as usize)
                                    .cloned()
                                    .unwrap_or_default();
                                (name, value.clone())
                            })
                            .collect(),
                    })
                    .collect(),
            },
            WireBody::Csv(records) => {
                let body: Vec<CsvRecord> = records
                    .iter()
                    .map(|r| CsvRecord {
                        fields: r.fields.clone(),
                        line: r.line as usize,
                    })
                    .collect();
                RawTable::from_csv_records(self.wire.columns.clone(), &body, import)
            }
        }
    }
}

impl<R: regia_ecs::reflect::Typed + Serialize + serde::de::DeserializeOwned + Send + Sync + 'static>
    DataTable<R>
{
    /// Loads a cooked table (the form a pak holds) into typed rows: the same row builder, cell rules
    /// and errors as [`Self::parse`] on the source text.
    ///
    /// # Errors
    ///
    /// The payload's own errors ([`CookedDataTable::from_bytes`]), then every row-type, column and
    /// cell error, as for text.
    pub fn from_cooked(bytes: &[u8]) -> Result<Self, TableLoadError> {
        let _span = tracing::debug_span!(target: "regia_asset", "data_table_load_cooked").entered();
        let cooked =
            CookedDataTable::from_bytes(bytes).map_err(|issue| TableLoadError { issues: vec![issue] })?;
        match cooked.format() {
            TableFormat::Ron => Self::from_raw(&cooked.to_raw(&CsvImport::default()), false),
            TableFormat::Csv => {
                let import = csv_import_for(&columns_of::<R>());
                Self::from_raw(&cooked.to_raw(&import), true)
            }
        }
    }
}

/// The cooked payload's shape, for the format registry (`cargo xtask check-formats`).
#[must_use]
pub fn data_table_cooked_schema_fingerprint() -> String {
    "CookedDataTable{version:u32le,row_type:String,columns:Vec<String>,body:enum{Ron(Vec<{key:String,line:u32,\
     cells:Vec<(u32,String)>}>),Csv(Vec<{line:u32,fields:Vec<String>}>)}}"
        .to_owned()
}
