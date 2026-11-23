// SPDX-License-Identifier: MPL-2.0
//! JSON import and export of tables (`regia table to-json|from-json`, the Tables panel's Import and
//! Export, `docs/specs/content-data.md` § 3.4), built on the CSV form of a [`RawTable`].
//!
//! The JSON form is an array of row objects. Each object has the row key under `"key"` and one member
//! per column, in column order; a column a row does not set is left out. A number or `true`/`false` cell
//! is a JSON number or boolean (its text as written), every other cell is a JSON string: a string
//! literal as its text, any other RON literal as its RON text. Reading goes through the same cell rules
//! as the CSV form ([`CsvImport`]): a JSON string that reads as a RON literal is a literal unless
//! its column is a text column, a number or boolean is a literal, `null` is an unset cell.

use regia_asset::csv::CsvRecord;
use regia_asset::data_table::{CsvImport, IssueCode, RawTable, TableIssue};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::Value;

/// A JSON object with its members in file order (`serde_json::Map` sorts them).
struct Ordered(Vec<(String, Value)>);

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Members;
        impl<'de> Visitor<'de> for Members {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Value>()? {
                    out.push((k, v));
                }
                Ok(Ordered(out))
            }
        }
        deserializer.deserialize_map(Members)
    }
}

/// Whether `text` is one JSON number token as written.
fn is_json_number(text: &str) -> bool {
    let b = text.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - start
    };
    let int_start = i;
    if digits(&mut i) == 0 || (b[int_start] == b'0' && i - int_start > 1) {
        return false;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        if digits(&mut i) == 0 {
            return false;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    i == b.len()
}

/// The JSON text of one cell: a number or boolean as written, anything else a string.
fn json_cell(text: &str) -> String {
    if is_json_number(text) || text == "true" || text == "false" {
        text.to_owned()
    } else {
        serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
    }
}

/// The JSON text of CSV `records` (the first is the header; its first field names the key column).
fn records_to_json(records: &[CsvRecord]) -> String {
    let Some((header, body)) = records.split_first() else {
        return "[]\n".to_owned();
    };
    let mut out = String::from("[\n");
    for (n, record) in body.iter().enumerate() {
        out.push_str("  {");
        let mut first = true;
        for (i, name) in header.fields.iter().enumerate() {
            let Some(value) = record.fields.get(i) else {
                continue;
            };
            // The key is always written; an empty cell of any other column is left out.
            if i > 0 && value.is_empty() {
                continue;
            }
            if !first {
                out.push_str(", ");
            }
            first = false;
            let name = if i == 0 { "key" } else { name };
            let name = serde_json::to_string(name).unwrap_or_default();
            let value = if i == 0 {
                serde_json::to_string(value).unwrap_or_default()
            } else {
                json_cell(value)
            };
            out.push_str(&format!("{name}: {value}"));
        }
        out.push('}');
        if n + 1 < body.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("]\n");
    out
}

/// CSV records (header first) of a JSON array of row objects.
fn json_to_records(text: &str) -> Result<Vec<CsvRecord>, TableIssue> {
    let bad = |message: String| TableIssue::new(IssueCode::BadFile, message);
    let rows: Vec<Ordered> = serde_json::from_str(text).map_err(|e| {
        TableIssue::new(
            IssueCode::BadFile,
            format!("the JSON is not an array of row objects: {e}"),
        )
        .with_line(e.line())
    })?;
    let mut columns: Vec<String> = Vec::new();
    for (n, row) in rows.iter().enumerate() {
        if !row.0.iter().any(|(k, _)| k == "key") {
            return Err(bad(format!("row {} has no \"key\" member", n + 1)).with_line(n + 1));
        }
        for (name, _) in &row.0 {
            if name != "key" && !columns.contains(name) {
                columns.push(name.clone());
            }
        }
    }
    let mut records = Vec::with_capacity(rows.len() + 1);
    let mut header = vec!["key".to_owned()];
    header.extend(columns.iter().cloned());
    records.push(CsvRecord {
        fields: header,
        line: 1,
    });
    for (n, row) in rows.iter().enumerate() {
        let mut fields = Vec::with_capacity(columns.len() + 1);
        let key = match row.0.iter().find(|(k, _)| k == "key").map(|(_, v)| v) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(num)) => num.to_string(),
            _ => return Err(bad(format!("row {}: \"key\" must be a string", n + 1)).with_line(n + 1)),
        };
        fields.push(key);
        for column in &columns {
            let cell = match row.0.iter().find(|(k, _)| k == column).map(|(_, v)| v) {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(s)) => s.clone(),
                Some(Value::Bool(b)) => b.to_string(),
                Some(Value::Number(num)) => num.to_string(),
                Some(other) => other.to_string(),
            };
            fields.push(cell);
        }
        records.push(CsvRecord { fields, line: n + 2 });
    }
    Ok(records)
}

/// The JSON array of row objects of CSV text (curve tables and any `key`-first CSV).
///
/// # Errors
///
/// `table_bad_record` for a CSV syntax error; `table_bad_file` for an empty input.
pub fn csv_to_json(csv_text: &str) -> Result<String, TableIssue> {
    let records = regia_asset::csv::parse(csv_text)
        .map_err(|e| TableIssue::new(IssueCode::BadRecord, e.kind.to_string()).with_line(e.line))?;
    if records.is_empty() {
        return Err(TableIssue::new(
            IssueCode::BadFile,
            "the CSV has no header record".to_owned(),
        ));
    }
    Ok(records_to_json(&records))
}

/// The CSV text (CRLF records) of a JSON array of row objects, the key column named `"key"`.
///
/// # Errors
///
/// `table_bad_file` when the JSON is not an array of objects with a `"key"` member.
pub fn json_to_csv(json_text: &str) -> Result<String, TableIssue> {
    json_to_csv_keyed(json_text, "key")
}

/// [`json_to_csv`] with the first CSV column named `key_column` (a curve table's is `name`); the JSON
/// member stays `"key"`.
///
/// # Errors
///
/// `table_bad_file` when the JSON is not an array of objects with a `"key"` member.
pub fn json_to_csv_keyed(json_text: &str, key_column: &str) -> Result<String, TableIssue> {
    let mut records = json_to_records(json_text)?;
    if let Some(header) = records.first_mut().and_then(|r| r.fields.first_mut()) {
        key_column.clone_into(header);
    }
    let mut out = String::new();
    for record in &records {
        regia_asset::csv::write_record(&mut out, &record.fields);
    }
    Ok(out)
}

/// The table as a JSON array of row objects (see the module docs).
///
/// # Errors
///
/// `table_bad_record` when the table's own CSV form cannot be read back (it cannot happen for a table
/// read from a file).
pub fn to_json(table: &RawTable) -> Result<String, TableIssue> {
    csv_to_json(&table.to_csv())
}

/// Reads a JSON array of row objects, with the cell rules of [`RawTable::from_csv`].
///
/// # Errors
///
/// `table_bad_file` when the text is not a JSON array of objects with a `"key"` member.
pub fn from_json(text: &str, import: &CsvImport) -> Result<RawTable, TableIssue> {
    RawTable::from_csv(&json_to_csv(text)?, import)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RON: &str = r#"(
    format_version: 1,
    row_type: "game::WeaponRow",
    rows: {
        "rifle": (damage: 30, ratio: 1.5, auto: true, kind: Rifle, icon: "ui/rifle.png", tag: "a,\"b\""),
        "pistol": (damage: 12, kind: Pistol, icon: "ui/pistol.png"),
    },
)
"#;

    #[test]
    fn a_table_exports_as_an_array_of_row_objects_in_column_order() {
        let raw = RawTable::from_ron(RON).expect("ron");
        let json = to_json(&raw).expect("json");
        let value: Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(value[0]["key"], "rifle");
        assert_eq!(value[0]["damage"], 30);
        assert_eq!(value[0]["ratio"], 1.5);
        assert_eq!(value[0]["auto"], true);
        assert_eq!(value[0]["kind"], "Rifle");
        assert_eq!(value[0]["tag"], "a,\"b\"");
        assert!(value[1].get("ratio").is_none(), "an unset column is left out");
        // Members stay in column order (a plain parse would sort them).
        let first = json.lines().nth(1).expect("first row");
        assert!(first.find("damage") < first.find("ratio") && first.find("ratio") < first.find("kind"));
    }

    #[test]
    fn json_round_trips_the_table_through_the_csv_cell_rules() {
        let raw = RawTable::from_ron(RON).expect("ron");
        let import = CsvImport {
            text_columns: vec!["kind".to_owned(), "icon".to_owned(), "tag".to_owned()],
            ..CsvImport::default()
        };
        let back = from_json(&to_json(&raw).expect("json"), &import).expect("from json");
        assert_eq!(back.rows.len(), 2);
        assert_eq!(back.to_csv(), raw.to_csv());
        // `kind: Rifle` is an enum variant in the file; as a text column it becomes a string.
        assert!(back.to_ron().contains("kind: \"Rifle\""));
        let guessed = from_json(&to_json(&raw).expect("json"), &CsvImport::default()).expect("from json");
        assert!(
            guessed.to_ron().contains("kind: Rifle") && guessed.to_ron().contains("icon: \"ui/rifle.png\""),
            "{}",
            guessed.to_ron()
        );
    }

    #[test]
    fn bad_json_is_one_table_bad_file_issue() {
        let none = from_json("{}", &CsvImport::default()).expect_err("not an array");
        assert_eq!(none.code.code(), "table_bad_file");
        let keyless = from_json(r#"[{"damage": 1}]"#, &CsvImport::default()).expect_err("no key");
        assert_eq!(keyless.code.code(), "table_bad_file");
        assert!(keyless.to_string().contains("key"), "{keyless}");
        let null = from_json(r#"[{"key": "a", "x": null, "y": 2}]"#, &CsvImport::default())
            .expect("null is an unset cell");
        assert_eq!(null.rows[0].cells, [("y".to_owned(), "2".to_owned())]);
    }

    #[test]
    fn csv_and_json_convert_without_a_row_type() {
        let csv = "key,0,10\r\nspeed,1,2.5\r\n";
        let json = csv_to_json(csv).expect("csv to json");
        let value: Value = serde_json::from_str(&json).expect("json");
        assert_eq!(value[0]["10"], 2.5);
        assert_eq!(json_to_csv(&json).expect("back"), csv);
    }

    #[test]
    fn number_tokens_follow_the_json_grammar() {
        for ok in ["0", "-1", "12", "1.5", "-0.25", "1e3", "2.5E-3"] {
            assert!(is_json_number(ok), "{ok}");
        }
        for bad in ["", "-", "01", "1.", ".5", "1e", "+1", "0x10", "1_000", "NaN"] {
            assert!(!is_json_number(bad), "{bad}");
        }
    }
}
