// SPDX-License-Identifier: MPL-2.0
//! `regia table validate|to-csv|to-ron|to-json|from-json`: data tables without the game's row type
//! (`docs/specs/content-data.md` § 3.4, book `user-guide/data-tables.md`).
//!
//! The CLI reads a table as keys, columns and cell texts ([`RawTable`]): it checks what needs no
//! row type (syntax, RFC 4180 records, empty and duplicate keys, cells that are not one RON value,
//! rows missing a column) and converts between `.table.ron` and CSV. Typed checks (does `damage`
//! parse as a `u32`, the game's row rules) run in the game: `tables.validate` over the agent
//! protocol, or `DataTable::validate` in a test.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use regia_asset::data_table::{CsvImport, RawTable, Severity, TableFormat, TableIssue, read_raw_ron};
use regia_asset_pipeline::table_json;

/// `regia table` subcommands.
#[derive(Debug, Subcommand)]
pub enum TableCommand {
    /// Check a `.table.ron` or `.table.csv` file; exits non-zero on errors.
    Validate(ValidateArgs),
    /// Write a `.table.ron` table as RFC 4180 CSV.
    ToCsv(ToCsvArgs),
    /// Write a CSV table (or an older `.table.ron`) as a current `.table.ron`.
    ToRon(ToRonArgs),
    /// Write a `.table.ron` or `.table.csv` table as a JSON array of row objects keyed by `key`.
    ToJson(ToJsonArgs),
    /// Write a JSON array of row objects as a current `.table.ron`.
    FromJson(FromJsonArgs),
}

/// `regia table to-json`.
#[derive(Debug, Args)]
pub struct ToJsonArgs {
    /// The `.table.ron` or `.table.csv` file.
    pub file: PathBuf,
    /// Write here instead of standard output.
    #[arg(short, long)]
    pub out: Option<PathBuf>,
}

/// `regia table from-json`.
#[derive(Debug, Args)]
pub struct FromJsonArgs {
    /// The JSON file: an array of objects, each with a `key` member and one member per column.
    pub file: PathBuf,
    /// An existing `.table.ron` to re-import into: its row type is kept, its string columns stay
    /// strings, and it is the default output.
    #[arg(long)]
    pub into: Option<PathBuf>,
    /// The row struct's type path to record (when `--into` names none).
    #[arg(long)]
    pub row_type: Option<String>,
    /// Columns whose cells are plain text (comma separated); other columns are guessed: a column
    /// with a cell that is not a RON literal is text.
    #[arg(long, value_delimiter = ',')]
    pub text_columns: Vec<String>,
    /// Write here instead of standard output (or the `--into` file).
    #[arg(short, long)]
    pub out: Option<PathBuf>,
}

/// `regia table validate`.
#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// The table file (`.ron` or `.csv`).
    pub file: PathBuf,
    /// Treat warnings as errors.
    #[arg(long)]
    pub strict: bool,
    /// Print the issues as JSON.
    #[arg(long)]
    pub json: bool,
}

/// `regia table to-csv`.
#[derive(Debug, Args)]
pub struct ToCsvArgs {
    /// The `.table.ron` file.
    pub file: PathBuf,
    /// Write here instead of standard output.
    #[arg(short, long)]
    pub out: Option<PathBuf>,
}

/// `regia table to-ron`.
#[derive(Debug, Args)]
pub struct ToRonArgs {
    /// The CSV file, or a `.table.ron` to rewrite in the current format version.
    pub file: PathBuf,
    /// An existing `.table.ron` to re-import into: its row type is kept, its string columns stay
    /// strings, and it is the default output.
    #[arg(long)]
    pub into: Option<PathBuf>,
    /// The row struct's type path to record (when the source names none).
    #[arg(long)]
    pub row_type: Option<String>,
    /// Columns whose cells are plain text (comma separated); other columns are guessed: a column
    /// with a cell that is not a RON literal is text.
    #[arg(long, value_delimiter = ',')]
    pub text_columns: Vec<String>,
    /// Write here instead of standard output (or the `--into` file).
    #[arg(short, long)]
    pub out: Option<PathBuf>,
}

/// Runs a `regia table` subcommand.
///
/// # Errors
///
/// A file cannot be read or written, does not parse, or (`validate`) has errors.
pub fn run(command: &TableCommand) -> Result<()> {
    match command {
        TableCommand::Validate(args) => {
            let (issues, failed) = validate(args)?;
            print_issues(&args.file, &issues, args.json);
            if failed {
                bail!("{}: table has errors", args.file.display());
            }
            Ok(())
        }
        TableCommand::ToCsv(args) => {
            let text = to_csv(&args.file)?;
            emit(args.out.as_deref(), &text)
        }
        TableCommand::ToRon(args) => {
            let text = to_ron(args)?;
            emit(args.out.as_deref().or(args.into.as_deref()), &text)
        }
        TableCommand::ToJson(args) => {
            let text = to_json(&args.file)?;
            emit(args.out.as_deref(), &text)
        }
        TableCommand::FromJson(args) => {
            let text = from_json(args)?;
            emit(args.out.as_deref().or(args.into.as_deref()), &text)
        }
    }
}

fn read(file: &Path) -> Result<String> {
    std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))
}

/// Reads a table file without its row type.
///
/// # Errors
///
/// The file cannot be read or does not parse (the error names the line).
pub fn read_raw(file: &Path, import: &CsvImport) -> Result<RawTable> {
    let text = read(file)?;
    let shown = file.display().to_string();
    let raw = match TableFormat::of_path(&shown) {
        TableFormat::Ron => read_raw_ron(&text),
        TableFormat::Csv => RawTable::from_csv(&text, import),
    };
    raw.map_err(|issue| anyhow::anyhow!("{shown}: {issue}"))
}

/// The issues of `regia table validate` and whether they fail the check.
///
/// # Errors
///
/// The file cannot be read.
pub fn validate(args: &ValidateArgs) -> Result<(Vec<TableIssue>, bool)> {
    let text = read(&args.file)?;
    let shown = args.file.display().to_string();
    let raw = match TableFormat::of_path(&shown) {
        TableFormat::Ron => read_raw_ron(&text),
        TableFormat::Csv => RawTable::from_csv(&text, &CsvImport::default()),
    };
    let issues = match raw {
        Ok(raw) => raw.validate(),
        Err(issue) => vec![issue],
    };
    let failed = issues
        .iter()
        .any(|i| i.severity == Severity::Error || args.strict);
    Ok((issues, failed))
}

fn print_issues(file: &Path, issues: &[TableIssue], json: bool) {
    if json {
        let list: Vec<serde_json::Value> = issues
            .iter()
            .map(|i| {
                serde_json::json!({
                    "code": i.code.code(),
                    "severity": i.severity.name(),
                    "message": i.message,
                    "key": i.key,
                    "column": i.column,
                    "line": i.line,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "file": file.display().to_string(), "issues": list })
        );
        return;
    }
    for issue in issues {
        println!("{}: {issue}", file.display());
    }
    let errors = issues.iter().filter(|i| i.is_error()).count();
    println!(
        "{}: {errors} error(s), {} warning(s)",
        file.display(),
        issues.len() - errors
    );
}

/// The CSV form of a `.table.ron` file.
///
/// # Errors
///
/// The file cannot be read or does not parse.
pub fn to_csv(file: &Path) -> Result<String> {
    Ok(read_raw(file, &CsvImport::default())?.to_csv())
}

/// The JSON form of a `.table.ron` or `.table.csv` file: an array of row objects keyed by `key`.
///
/// # Errors
///
/// The file cannot be read or does not parse.
pub fn to_json(file: &Path) -> Result<String> {
    let raw = read_raw(file, &CsvImport::default())?;
    table_json::to_json(&raw).map_err(|issue| anyhow::anyhow!("{}: {issue}", file.display()))
}

/// The `.table.ron` form of a CSV (or older `.table.ron`) file.
///
/// # Errors
///
/// A file cannot be read or does not parse.
pub fn to_ron(args: &ToRonArgs) -> Result<String> {
    convert(
        &args.file,
        args.into.as_deref(),
        args.row_type.clone(),
        &args.text_columns,
        |text, import| {
            let shown = args.file.display().to_string();
            match TableFormat::of_path(&shown) {
                TableFormat::Ron => read_raw_ron(text),
                TableFormat::Csv => RawTable::from_csv(text, import),
            }
        },
    )
}

/// The `.table.ron` form of a JSON file (`regia table from-json`).
///
/// # Errors
///
/// A file cannot be read or does not parse.
pub fn from_json(args: &FromJsonArgs) -> Result<String> {
    convert(
        &args.file,
        args.into.as_deref(),
        args.row_type.clone(),
        &args.text_columns,
        table_json::from_json,
    )
}

/// Reads `file` with `parse` under the cell rules `into` and `text_columns` give, and writes the
/// table as `.table.ron` text.
fn convert(
    file: &Path,
    into: Option<&Path>,
    row_type: Option<String>,
    text_columns: &[String],
    parse: impl FnOnce(&str, &CsvImport) -> Result<RawTable, TableIssue>,
) -> Result<String> {
    let mut import = CsvImport {
        text_columns: text_columns.to_vec(),
        ..CsvImport::default()
    };
    let mut row_type = row_type;
    if let Some(into) = into {
        let existing = read_raw(into, &CsvImport::default())?;
        let strings = existing.string_columns();
        import.literal_columns = existing
            .columns
            .iter()
            .filter(|c| !strings.contains(c) && !import.text_columns.contains(c))
            .cloned()
            .collect();
        import.text_columns.extend(strings);
        if row_type.is_none() && !existing.row_type.is_empty() {
            row_type = Some(existing.row_type.clone());
        }
    }
    let text = read(file)?;
    let mut raw = parse(&text, &import).map_err(|issue| anyhow::anyhow!("{}: {issue}", file.display()))?;
    if let Some(row_type) = row_type {
        raw.row_type = row_type;
    }
    Ok(raw.to_ron())
}

fn emit(out: Option<&Path>, text: &str) -> Result<()> {
    let Some(out) = out else {
        print!("{text}");
        return Ok(());
    };
    // Atomic, so a running game's live reload never reads a half-written table.
    let name = out
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = out.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, out))
        .with_context(|| format!("writing {}", out.display()))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
    eprintln!("wrote {}", out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RON: &str = r#"(
    format_version: 1,
    row_type: "game::WeaponRow",
    rows: {
        "rifle": (damage: 30, kind: Rifle, icon: "ui/rifle.png"),
        "pistol": (damage: 12, kind: Pistol, icon: "ui/pistol.png"),
    },
)
"#;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).expect("write fixture");
        path
    }

    #[test]
    fn ron_to_csv_and_back_into_the_existing_table() {
        let dir = regia_test_utils::unique_temp_dir("cli_table_convert");
        let ron = write(&dir, "weapons.table.ron", RON);
        let csv = to_csv(&ron).expect("to csv");
        assert_eq!(
            csv,
            "key,damage,kind,icon\r\nrifle,30,Rifle,ui/rifle.png\r\npistol,12,Pistol,ui/pistol.png\r\n"
        );
        // A spreadsheet edit: a new row whose icon looks like an identifier.
        let edited = write(&dir, "weapons.csv", &format!("{csv}smg,9,Pistol,smg\r\n"));
        let back = to_ron(&ToRonArgs {
            file: edited,
            into: Some(ron),
            row_type: None,
            text_columns: Vec::new(),
            out: None,
        })
        .expect("to ron");
        assert!(back.contains("row_type: \"game::WeaponRow\""), "{back}");
        assert!(
            back.contains("\"smg\": (damage: 9, kind: Pistol, icon: \"smg\")"),
            "{back}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_fails_on_errors_and_strict_warnings() {
        let dir = regia_test_utils::unique_temp_dir("cli_table_validate");
        let good = write(&dir, "good.table.ron", RON);
        let args = |file: &Path, strict| ValidateArgs {
            file: file.to_path_buf(),
            strict,
            json: false,
        };
        let (issues, failed) = validate(&args(&good, false)).expect("validate");
        assert!(issues.is_empty() && !failed);

        let dup = write(&dir, "dup.table.csv", "key,damage\r\na,1\r\na,2\r\n");
        let (issues, failed) = validate(&args(&dup, false)).expect("validate");
        assert!(failed);
        assert_eq!(issues[0].code.code(), "table_duplicate_key");

        let ragged = write(&dir, "ragged.table.csv", "key,damage\r\na,1,2\r\n");
        let (issues, failed) = validate(&args(&ragged, false)).expect("validate");
        assert!(failed);
        assert_eq!(issues[0].code.code(), "table_bad_record");
        assert_eq!(issues[0].line, Some(2));

        let sparse = write(
            &dir,
            "sparse.table.ron",
            r#"(format_version: 1, row_type: "", rows: { "a": (x: 1, y: 2), "b": (x: 3) })"#,
        );
        let (issues, failed) = validate(&args(&sparse, false)).expect("validate");
        assert!(!failed, "a missing column is a warning");
        assert_eq!(issues[0].code.code(), "table_missing_column");
        let (_, failed) = validate(&args(&sparse, true)).expect("validate");
        assert!(failed, "--strict fails on warnings");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_tables_are_rewritten_in_the_current_version() {
        let dir = regia_test_utils::unique_temp_dir("cli_table_upgrade");
        let v0 = write(&dir, "old.table.ron", r#"[("a", (x: 1)), ("b", (x: 2))]"#);
        let out = to_ron(&ToRonArgs {
            file: v0,
            into: None,
            row_type: Some("game::Row".to_owned()),
            text_columns: Vec::new(),
            out: None,
        })
        .expect("upgrade");
        assert!(out.contains("format_version: 1"), "{out}");
        assert!(out.contains("row_type: \"game::Row\""), "{out}");
        assert!(out.contains("\"b\": (x: 2)"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ron_to_json_and_back_into_the_existing_table() {
        let dir = regia_test_utils::unique_temp_dir("cli_table_json");
        let ron = write(&dir, "weapons.table.ron", RON);
        let json = to_json(&ron).expect("to json");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(value[0]["key"], "rifle");
        assert_eq!(value[0]["damage"], 30);
        assert_eq!(value[1]["icon"], "ui/pistol.png");
        // A tool's edit: a new row whose icon looks like an identifier.
        let edited = write(
            &dir,
            "weapons.json",
            &json.replace(
                "\n]",
                ",\n  {\"key\": \"smg\", \"damage\": 9, \"kind\": \"Pistol\", \"icon\": \"smg\"}\n]",
            ),
        );
        let back = from_json(&FromJsonArgs {
            file: edited,
            into: Some(ron),
            row_type: None,
            text_columns: Vec::new(),
            out: None,
        })
        .expect("from json");
        assert!(back.contains("row_type: \"game::WeaponRow\""), "{back}");
        assert!(
            back.contains("\"smg\": (damage: 9, kind: Pistol, icon: \"smg\")"),
            "{back}"
        );
        // A CSV source converts to the same JSON.
        let csv = write(
            &dir,
            "weapons.table.csv",
            &to_csv(&write(&dir, "w2.table.ron", RON)).expect("csv"),
        );
        assert_eq!(to_json(&csv).expect("csv to json"), json);
        let bad = write(&dir, "bad.json", "{\"key\": 1}");
        let err = from_json(&FromJsonArgs {
            file: bad,
            into: None,
            row_type: None,
            text_columns: Vec::new(),
            out: None,
        })
        .expect_err("not an array");
        assert!(
            err.to_string().contains("table_bad_file") || err.to_string().contains("array"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
