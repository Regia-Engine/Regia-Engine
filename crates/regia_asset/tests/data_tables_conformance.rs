// SPDX-License-Identifier: MPL-2.0
//! Data-table CSV rules of `docs/specs/content-data.md` § 3.3 that the main suite does not pin,
//! through the typed loader (spec-43 audit, ADR-5662): the RFC 4180 reader's line ends, byte-order
//! mark and embedded line breaks reach typed rows, and an empty cell of a non-`String` column is
//! the serde default when the field has one, else an error naming its row and column.

use regia_app::prelude::*;
use regia_asset::data_table::{DataTable, IssueCode};
use serde::{Deserialize, Serialize};

#[derive(Reflect, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct ItemRow {
    /// No serde default: an empty cell is an error.
    weight: u32,
    name: String,
    #[serde(default)]
    stack: u8,
}

#[test]
fn line_ends_and_a_byte_order_mark_read_the_same_rows() {
    let lf = "key,weight,name,stack\na,1,Apple,3\nb,2,Bread,\n";
    let want = DataTable::<ItemRow>::from_csv_str(lf).expect("LF");
    for text in [
        lf.replace('\n', "\r\n"),
        lf.replace('\n', "\r"),
        format!("\u{feff}{lf}"),
        format!("\u{feff}{}", lf.replace('\n', "\r\n").trim_end()),
    ] {
        let table = DataTable::<ItemRow>::from_csv_str(&text).expect("variant loads");
        assert_eq!(
            table.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>(),
            "{text:?}"
        );
        for (key, row) in want.iter() {
            assert_eq!(table.get(key), Some(row), "{key} in {text:?}");
        }
    }
    let b = want.get("b").expect("b");
    assert_eq!((b.weight, b.name.as_str(), b.stack), (2, "Bread", 0));
}

#[test]
fn a_quoted_string_cell_keeps_commas_quotes_and_line_breaks() {
    let csv = "key,weight,name\r\nsign,4,\"Keep out,\r\n\"\"really\"\"\"\r\n";
    let table = DataTable::<ItemRow>::from_csv_str(csv).expect("load");
    assert_eq!(table.get("sign").expect("sign").name, "Keep out,\r\n\"really\"");
    // Written back, the cell is quoted again and reads back unchanged.
    let again = DataTable::<ItemRow>::from_csv_str(&table.to_csv_string().expect("csv")).expect("reload");
    assert_eq!(again.get("sign"), table.get("sign"));
}

#[test]
fn an_empty_cell_without_a_serde_default_is_an_error_naming_row_and_column() {
    let csv = "key,weight,name,stack\r\nok,1,Fine,2\r\nbad,,Broken,2\r\n";
    let err = DataTable::<ItemRow>::from_csv_str(csv).expect_err("weight has no default");
    let issue = err
        .issues
        .iter()
        .find(|i| i.key.as_deref() == Some("bad"))
        .unwrap_or_else(|| panic!("no issue for row bad: {:?}", err.issues));
    assert_eq!(issue.code, IssueCode::MissingColumn, "{issue:?}");
    assert_eq!(issue.column.as_deref(), Some("weight"), "{issue:?}");
    assert!(
        err.issues.iter().all(|i| i.key.as_deref() != Some("ok")),
        "{:?}",
        err.issues
    );
}
