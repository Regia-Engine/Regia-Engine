// SPDX-License-Identifier: MPL-2.0
//! The cooked form of data tables (`data-table-cooked` v1, ADR-2132): a cooked table loads to the
//! same typed rows as its source text, the cook refuses structural errors, a damaged payload is
//! refused by name and never panics, and the load is one pass over the rows (counted, not timed).

use std::path::PathBuf;

use regia_app::prelude::*;
use regia_asset::data_table::{
    CookedDataTable, CsvImport, DATA_TABLE_COOKED_VERSION, DataTable, TableFormat, TableIssue,
};
use serde::{Deserialize, Serialize};

#[derive(Reflect, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
enum WeaponKind {
    #[default]
    Rifle,
    Pistol,
    Burst(u8),
}

#[derive(Reflect, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct Offset {
    x: i32,
    y: i32,
}

#[derive(Reflect, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct WeaponRow {
    damage: u32,
    spread: f32,
    kind: WeaponKind,
    icon: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    muzzle: Option<Offset>,
    #[serde(default)]
    sound: PathBuf,
}

const WEAPONS: &str = r#"(
    format_version: 1,
    row_type: "WeaponRow",
    rows: {
        "rifle": (damage: 30, spread: 0.5, kind: Rifle, icon: "ui/rifle.png", tags: ["long", "auto"]),
        // A comment between rows.
        "pistol": (damage: 12, spread: 1.5, kind: Pistol, icon: "ui/pistol, small.png", muzzle: Some((x: 1, y: -2))),
        "smg": (damage: 9, spread: 2.0, kind: Burst(3), icon: "say \"hi\"\nline two", sound: "sfx/smg.wav"),
    },
)
"#;

fn codes(issues: &[TableIssue]) -> Vec<&'static str> {
    issues.iter().map(|i| i.code.code()).collect()
}

fn cook(text: &str, format: TableFormat) -> Vec<u8> {
    CookedDataTable::from_text(text, format)
        .expect("cooks")
        .to_bytes()
        .expect("encodes")
}

fn assert_same(a: &DataTable<WeaponRow>, b: &DataTable<WeaponRow>) {
    assert_eq!(a.keys().collect::<Vec<_>>(), b.keys().collect::<Vec<_>>());
    for (key, row) in a.iter() {
        assert_eq!(b.get(key), Some(row), "row {key}");
    }
}

#[test]
fn a_cooked_ron_table_loads_the_same_rows_as_its_text() {
    let bytes = cook(WEAPONS, TableFormat::Ron);
    assert_eq!(&bytes[..4], &DATA_TABLE_COOKED_VERSION.to_le_bytes());
    let text = DataTable::<WeaponRow>::from_ron_str(WEAPONS).expect("text");
    let cooked = DataTable::<WeaponRow>::from_cooked(&bytes).expect("cooked");
    assert_same(&text, &cooked);
    // Lines survive the cook, so an error found at load still names the source line.
    let decoded = CookedDataTable::from_bytes(&bytes).expect("decode");
    assert_eq!(decoded.format(), TableFormat::Ron);
    assert_eq!(decoded.row_type(), "WeaponRow");
    assert_eq!(decoded.len(), 3);
    let raw = decoded.to_raw(&CsvImport::default());
    assert_eq!(raw.rows.iter().map(|r| r.line).collect::<Vec<_>>(), vec![5, 7, 8]);
    // Encoding is deterministic: the same source cooks to the same bytes.
    assert_eq!(bytes, cook(WEAPONS, TableFormat::Ron));
    assert_eq!(decoded.to_bytes().expect("again"), bytes);
}

#[test]
fn a_cooked_csv_table_keeps_the_typed_csv_cell_rules() {
    // `icon` is a `String` column whose cell looks like a number, `sound` a `PathBuf` given as plain
    // text, an empty non-string cell takes the default: the cook does not know the row struct, so
    // these rules must run at load, exactly as for the text.
    let csv = "key,damage,spread,kind,icon,sound\r\n7,5,1.0,Pistol,42,sfx/a b.wav\r\nx,6,1.0,Rifle,,\r\n";
    let bytes = cook(csv, TableFormat::Csv);
    let text = DataTable::<WeaponRow>::from_csv_str(csv).expect("text");
    let cooked = DataTable::<WeaponRow>::from_cooked(&bytes).expect("cooked");
    assert_same(&text, &cooked);
    assert_eq!(cooked.get("7").expect("7").icon, "42");
    assert_eq!(cooked.get("7").expect("7").sound, PathBuf::from("sfx/a b.wav"));
    assert_eq!(
        CookedDataTable::from_bytes(&bytes).expect("decode").format(),
        TableFormat::Csv
    );
    // And a whole table written by the CSV writer round-trips through the cook.
    let csv = DataTable::<WeaponRow>::from_ron_str(WEAPONS)
        .expect("text")
        .to_csv_string()
        .expect("csv");
    let cooked = DataTable::<WeaponRow>::from_cooked(&cook(&csv, TableFormat::Csv)).expect("cooked");
    assert_same(&DataTable::from_ron_str(WEAPONS).expect("text"), &cooked);
}

#[test]
fn the_cook_refuses_structural_errors_and_keeps_warnings() {
    let duplicate = WEAPONS.replace("\"pistol\":", "\"rifle\":");
    let issues = CookedDataTable::from_text(&duplicate, TableFormat::Ron).expect_err("duplicate key");
    assert_eq!(codes(&issues), vec!["table_duplicate_key"]);
    let empty = WEAPONS.replace("\"smg\":", "\"\":");
    let issues = CookedDataTable::from_text(&empty, TableFormat::Ron).expect_err("empty key");
    assert_eq!(codes(&issues), vec!["table_empty_key"]);
    let newer = WEAPONS.replace("format_version: 1", "format_version: 9");
    let issues = CookedDataTable::from_text(&newer, TableFormat::Ron).expect_err("newer");
    assert_eq!(codes(&issues), vec!["table_unsupported_version"]);
    let ragged = "key,damage\r\na,1,2\r\n";
    let issues = CookedDataTable::from_text(ragged, TableFormat::Csv).expect_err("ragged");
    assert_eq!(codes(&issues), vec!["table_bad_record"]);
    // A row without a column other rows have is a warning: its field default may fill it.
    assert!(CookedDataTable::from_text(WEAPONS, TableFormat::Ron).is_ok());
    // A v0 table (bare list) is migrated by the cook.
    let v0 = r#"[("a", (damage: 1, spread: 1.0, kind: Rifle, icon: "i"))]"#;
    let cooked = DataTable::<WeaponRow>::from_cooked(&cook(v0, TableFormat::Ron)).expect("v0");
    assert_eq!(cooked.get("a").expect("a").damage, 1);
}

#[test]
fn typed_errors_of_a_cooked_table_name_their_row_column_and_line() {
    let bad = WEAPONS.replace("damage: 12,", "damage: \"twelve\",");
    let error = DataTable::<WeaponRow>::from_cooked(&cook(&bad, TableFormat::Ron)).expect_err("bad cell");
    assert_eq!(codes(&error.issues), vec!["table_bad_cell"]);
    assert_eq!(error.issues[0].key.as_deref(), Some("pistol"));
    assert_eq!(error.issues[0].column.as_deref(), Some("damage"));
    assert_eq!(error.issues[0].line, Some(7));
    let other = WEAPONS.replace("row_type: \"WeaponRow\"", "row_type: \"game::Other\"");
    let error = DataTable::<WeaponRow>::from_cooked(&cook(&other, TableFormat::Ron)).expect_err("row type");
    assert_eq!(codes(&error.issues), vec!["table_wrong_row_type"]);
}

#[test]
fn a_damaged_payload_is_refused_by_name_and_never_panics() {
    let bytes = cook(WEAPONS, TableFormat::Ron);
    let refuse = |b: &[u8]| CookedDataTable::from_bytes(b).expect_err("refused").code.code();
    assert_eq!(refuse(&[]), "table_bad_file");
    assert_eq!(refuse(&[1, 0]), "table_bad_file");
    let mut newer = bytes.clone();
    newer[..4].copy_from_slice(&(DATA_TABLE_COOKED_VERSION + 1).to_le_bytes());
    assert_eq!(refuse(&newer), "table_unsupported_version");
    let mut zero = bytes.clone();
    zero[..4].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(refuse(&zero), "table_unsupported_version");
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(refuse(&trailing), "table_bad_file");
    // Every truncation and every single-byte change: a clean answer each time, no panic. A changed
    // byte may still decode (it may sit inside a cell's text); then the typed load answers too.
    for len in 0..bytes.len() {
        assert!(
            CookedDataTable::from_bytes(&bytes[..len]).is_err(),
            "prefix {len}"
        );
        let _ = DataTable::<WeaponRow>::from_cooked(&bytes[..len]);
    }
    for at in 0..bytes.len() {
        for flip in [0x01u8, 0x80, 0xff] {
            let mut changed = bytes.clone();
            changed[at] ^= flip;
            if let Ok(table) = CookedDataTable::from_bytes(&changed) {
                let _ = table.to_raw(&CsvImport::default());
            }
            let _ = DataTable::<WeaponRow>::from_cooked(&changed);
        }
    }
}

/// `rows` rows of eight filled columns, as `.table.ron` text.
fn big_table(rows: usize) -> String {
    let mut text = String::from("(\n    format_version: 1,\n    row_type: \"WeaponRow\",\n    rows: {\n");
    for i in 0..rows {
        text.push_str(&format!(
            "        \"w{i:05}\": (damage: {d}, spread: {s}.5, kind: Burst({b}), icon: \"ui/w{i}.png\", tags: [\"t{t}\"], \
             muzzle: Some((x: {i}, y: -1)), sound: \"sfx/{i}.wav\"),\n",
            d = i % 97,
            s = i % 7,
            b = i % 5,
            t = i % 3,
        ));
    }
    text.push_str("    },\n)\n");
    text
}

/// Spec 51 § 10 / content-data § 3: the 4,096-row load. No wall clock here (the measured numbers come
/// from `benches/data_table_load.rs`); this pins the shape that makes the budget hold: the cooked
/// payload is smaller than the text, and decoding it touches each row once (the decoded size grows
/// exactly with the rows).
#[test]
fn a_4096_row_cooked_table_loads_whole_and_scales_with_its_rows() {
    let text = big_table(4096);
    let bytes = cook(&text, TableFormat::Ron);
    assert!(
        bytes.len() < text.len(),
        "cooked {} vs text {}",
        bytes.len(),
        text.len()
    );
    let table = DataTable::<WeaponRow>::from_cooked(&bytes).expect("4,096 rows");
    assert_eq!(table.len(), 4096);
    assert_eq!(
        table.get("w04095").expect("last").muzzle,
        Some(Offset { x: 4095, y: -1 })
    );
    assert_same(&DataTable::from_ron_str(&text).expect("text"), &table);
    let quarter = cook(&big_table(1024), TableFormat::Ron);
    // The payload is linear in the rows: four times the rows is about four times the bytes (row
    // texts grow by a digit or so).
    let ratio = bytes.len() as f64 / quarter.len() as f64;
    assert!((3.9..4.3).contains(&ratio), "payload ratio {ratio}");
}

/// Hand-run timing of the 4,096-row load, text and cooked (`docs/06-testing-and-ci.md` § 9): prints, asserts no
/// time. `cargo test -p regia_asset --test data_tables_cooked --release -- --ignored --nocapture`.
#[test]
#[ignore = "hand-run microbenchmark"]
fn hand_run_4096_row_load_times() {
    let text = big_table(4096);
    let bytes = cook(&text, TableFormat::Ron);
    let time = |name: &str, op: &dyn Fn()| {
        op();
        let mut samples: Vec<std::time::Duration> = (0..15)
            .map(|_| {
                let start = std::time::Instant::now();
                op();
                start.elapsed()
            })
            .collect();
        samples.sort_unstable();
        println!("perf-micro {name}: p50={:?} max={:?}", samples[7], samples[14]);
    };
    time("data_table_text_4096", &|| {
        let table = DataTable::<WeaponRow>::from_ron_str(&text).expect("text");
        assert_eq!(table.validate().len(), 0);
    });
    time("data_table_cooked_4096", &|| {
        let table = DataTable::<WeaponRow>::from_cooked(&bytes).expect("cooked");
        assert_eq!(table.validate().len(), 0);
    });
    time("data_table_cook_4096", &|| {
        assert!(CookedDataTable::from_text(&text, TableFormat::Ron).is_ok());
    });
    println!(
        "perf-micro sizes: text {} bytes, cooked {} bytes",
        text.len(),
        bytes.len()
    );
}
