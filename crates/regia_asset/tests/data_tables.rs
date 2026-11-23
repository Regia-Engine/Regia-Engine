// SPDX-License-Identifier: MPL-2.0
//! Data tables (`docs/specs/content-data.md` § 3): typed RON and CSV forms, migration, the
//! validator's codes, type-erased edits and hot reload.

use std::path::PathBuf;

use regia_app::prelude::*;
use regia_asset::SourceAssetChanged;
use regia_asset::data_table::{
    CsvImport, DataTable, DataTableAppExt, DataTables, ErasedTable, IssueCode, RawTable, Severity,
    TableFormat, TableIssue, read_raw_ron,
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

/// One weapon.
#[derive(Reflect, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct WeaponRow {
    /// Damage per hit.
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

fn weapons() -> DataTable<WeaponRow> {
    DataTable::from_ron_str(WEAPONS).expect("weapons load")
}

fn codes(issues: &[TableIssue]) -> Vec<&'static str> {
    issues.iter().map(|i| i.code.code()).collect()
}

#[test]
fn ron_table_loads_with_lookup_by_key_in_file_order() {
    let table = weapons();
    assert_eq!(table.len(), 3);
    assert_eq!(table.keys().collect::<Vec<_>>(), vec!["rifle", "pistol", "smg"]);
    let rifle = table.get("rifle").expect("rifle");
    assert_eq!(rifle.damage, 30);
    assert_eq!(rifle.tags, vec!["long", "auto"]);
    assert_eq!(
        table.get("pistol").expect("pistol").muzzle,
        Some(Offset { x: 1, y: -2 })
    );
    assert_eq!(table.get("smg").expect("smg").kind, WeaponKind::Burst(3));
    assert_eq!(table.get("smg").expect("smg").icon, "say \"hi\"\nline two");
    assert!(table.get("shotgun").is_none());
    assert!(table.validate().is_empty());
}

#[test]
fn columns_come_from_reflection() {
    let columns = weapons().columns();
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["damage", "spread", "kind", "icon", "tags", "muzzle", "sound"]
    );
    let damage = &columns[0];
    assert_eq!(damage.type_path, "u32");
    assert!(!damage.text);
    assert_eq!(damage.doc.as_deref(), Some("Damage per hit."));
    assert!(columns[3].text, "icon is a String column");
    assert!(!columns[6].text, "a PathBuf column is not a String column");
}

#[test]
fn csv_round_trip_keeps_every_value() {
    let table = weapons();
    let csv = table.to_csv_string().expect("csv");
    let mut lines = csv.split("\r\n");
    assert_eq!(
        lines.next(),
        Some("key,damage,spread,kind,icon,tags,muzzle,sound")
    );
    assert_eq!(
        lines.next(),
        Some("rifle,30,0.5,Rifle,ui/rifle.png,\"[\"\"long\"\",\"\"auto\"\"]\",None,")
    );
    let back = DataTable::<WeaponRow>::from_csv_str(&csv).expect("csv load");
    assert_eq!(back.keys().collect::<Vec<_>>(), table.keys().collect::<Vec<_>>());
    for (key, row) in table.iter() {
        assert_eq!(back.get(key), Some(row), "row {key}");
    }
    // And back to RON: the same rows again.
    let ron_again =
        DataTable::<WeaponRow>::from_ron_str(&back.to_ron_string().expect("ron")).expect("ron load");
    for (key, row) in table.iter() {
        assert_eq!(ron_again.get(key), Some(row), "row {key}");
    }
}

#[test]
fn csv_cells_follow_the_column_types() {
    // String cells are plain text (a number-looking name stays a string); a PathBuf cell may be
    // plain text too; an empty non-string cell takes the serde default.
    let csv = "key,damage,spread,kind,icon,sound\r\n7,5,1.0,Pistol,42,sfx/a b.wav\r\nx,6,1.0,Rifle,,\r\n";
    let table = DataTable::<WeaponRow>::from_csv_str(csv).expect("load");
    let seven = table.get("7").expect("row 7");
    assert_eq!(seven.icon, "42");
    assert_eq!(seven.sound, PathBuf::from("sfx/a b.wav"));
    let x = table.get("x").expect("row x");
    assert_eq!(x.icon, "", "an empty String cell is the empty string");
    assert_eq!(x.sound, PathBuf::new());
}

#[test]
fn bad_cells_name_their_row_and_column() {
    let csv = "key,damage,spread,kind,icon\r\nok,1,1.0,Rifle,a\r\nbad,lots,1.0,Rifle,b\r\n";
    let err = DataTable::<WeaponRow>::from_csv_str(csv).expect_err("bad damage");
    let issue = &err.issues[0];
    assert_eq!(issue.code, IssueCode::BadCell);
    assert_eq!(issue.key.as_deref(), Some("bad"));
    assert_eq!(issue.column.as_deref(), Some("damage"));
    assert_eq!(issue.line, Some(3));

    let ron = WEAPONS.replace("kind: Pistol", "kind: Shotgun");
    let err = DataTable::<WeaponRow>::from_ron_str(&ron).expect_err("bad kind");
    let issue = &err.issues[0];
    assert_eq!(issue.code, IssueCode::BadCell);
    assert_eq!(issue.key.as_deref(), Some("pistol"));
    assert_eq!(issue.column.as_deref(), Some("kind"));
    assert_eq!(issue.line, Some(7));
}

#[test]
fn header_and_record_errors() {
    let unknown = "key,damage,spread,kind,icon,weight\r\na,1,1.0,Rifle,x,3\r\n";
    let err = DataTable::<WeaponRow>::from_csv_str(unknown).expect_err("unknown column");
    assert_eq!(codes(&err.issues), vec!["table_unknown_column"]);
    assert_eq!(err.issues[0].column.as_deref(), Some("weight"));

    let missing = "key,damage,kind,icon\r\na,1,Rifle,x\r\n";
    let err = DataTable::<WeaponRow>::from_csv_str(missing).expect_err("spread has no default");
    assert_eq!(codes(&err.issues), vec!["table_missing_column"]);
    assert_eq!(err.issues[0].column.as_deref(), Some("spread"));

    let ragged = "key,damage,spread,kind,icon\r\na,1,1.0,Rifle\r\n";
    let err = DataTable::<WeaponRow>::from_csv_str(ragged).expect_err("ragged");
    assert_eq!(codes(&err.issues), vec!["table_bad_record"]);
    assert_eq!(err.issues[0].line, Some(2));

    let no_key = "name,damage\r\na,1\r\n";
    let err = DataTable::<WeaponRow>::from_csv_str(no_key).expect_err("no key column");
    assert_eq!(codes(&err.issues), vec!["table_bad_file"]);
}

#[test]
fn row_type_is_checked() {
    let other = WEAPONS.replace("\"WeaponRow\"", "\"game::ArmorRow\"");
    let err = DataTable::<WeaponRow>::from_ron_str(&other).expect_err("wrong type");
    assert_eq!(codes(&err.issues), vec!["table_wrong_row_type"]);
    // The full type path is accepted too.
    let full = WEAPONS.replace(
        "\"WeaponRow\"",
        &format!("\"{}\"", <WeaponRow as regia_ecs::reflect::TypePath>::type_path()),
    );
    assert!(DataTable::<WeaponRow>::from_ron_str(&full).is_ok());
}

#[test]
fn version_zero_migrates_and_newer_versions_are_refused() {
    let v0 = r#"[("rifle", (damage: 30, spread: 0.5, kind: Rifle, icon: "r")), ("pistol", (damage: 12, spread: 1.5, kind: Pistol, icon: "p"),),]"#;
    let raw = read_raw_ron(v0).expect("v0");
    assert_eq!(raw.source_version, 1);
    assert_eq!(raw.row_type, "");
    let table = DataTable::<WeaponRow>::from_ron_str(v0).expect("v0 typed");
    assert_eq!(table.get("pistol").expect("pistol").damage, 12);
    // Written back, it is a v1 file.
    let v1 = raw.to_ron();
    assert!(v1.contains("format_version: 1"), "{v1}");
    assert_eq!(read_raw_ron(&v1).expect("v1").rows.len(), 2);

    let v9 = WEAPONS.replace("format_version: 1", "format_version: 9");
    let err = DataTable::<WeaponRow>::from_ron_str(&v9).expect_err("newer");
    assert_eq!(codes(&err.issues), vec!["table_unsupported_version"]);
}

#[test]
fn validator_reports_duplicate_and_empty_keys_and_rules() {
    let text = r#"(format_version: 1, row_type: "", rows: {
        "a": (damage: 1, spread: 1.0, kind: Rifle, icon: "x"),
        "a": (damage: 2, spread: 1.0, kind: Rifle, icon: "y"),
        "": (damage: 3, spread: 1.0, kind: Rifle, icon: "z"),
        "b": (damage: 0, spread: 1.0, kind: Rifle, icon: "w"),
    })"#;
    let table = DataTable::<WeaponRow>::from_ron_str(text)
        .expect("loads")
        .with_rule(|_, row| {
            if row.damage == 0 {
                Err("damage must be positive".to_owned())
            } else {
                Ok(())
            }
        });
    let issues = table.validate();
    assert_eq!(
        codes(&issues),
        vec!["table_duplicate_key", "table_empty_key", "table_rule"]
    );
    assert_eq!(issues[0].line, Some(3));
    assert_eq!(issues[2].key.as_deref(), Some("b"));
    // The first of a duplicated key wins lookups.
    assert_eq!(table.get("a").expect("a").damage, 1);
}

#[test]
fn raw_tables_convert_without_the_row_type() {
    let raw = read_raw_ron(WEAPONS).expect("raw");
    assert_eq!(raw.row_type, "WeaponRow");
    assert_eq!(
        raw.columns,
        vec!["damage", "spread", "kind", "icon", "tags", "muzzle", "sound"]
    );
    assert!(raw.validate().iter().all(|i| i.severity == Severity::Warning));
    let csv = raw.to_csv();
    // Text columns are learned from the RON (all cells are string literals).
    let import = CsvImport {
        text_columns: raw.string_columns(),
        ..CsvImport::default()
    };
    assert_eq!(import.text_columns, vec!["icon", "sound"]);
    let mut back = RawTable::from_csv(&csv, &import).expect("csv");
    back.row_type = raw.row_type.clone();
    let typed = DataTable::<WeaponRow>::from_ron_str(&back.to_ron()).expect("typed");
    assert_eq!(typed.get("smg"), weapons().get("smg"));
    // Without hints, a column with a non-literal cell is text, an enum column is not.
    let guessed =
        RawTable::from_csv("key,kind,icon\r\na,Rifle,ui/a.png\r\n", &CsvImport::default()).expect("guess");
    assert_eq!(
        guessed.rows[0].cells,
        vec![
            ("kind".to_owned(), "Rifle".to_owned()),
            ("icon".to_owned(), "\"ui/a.png\"".to_owned()),
        ]
    );
    let dup = RawTable::from_csv("key,v\r\na,1\r\na,2\r\n,3\r\n", &CsvImport::default()).expect("dup");
    let issues = dup.validate();
    assert_eq!(codes(&issues), vec!["table_duplicate_key", "table_empty_key"]);
}

#[test]
fn erased_edits_apply_only_valid_values() {
    let mut table = weapons().with_rule(|_, row| {
        if row.damage > 100 {
            Err("damage over 100".to_owned())
        } else {
            Ok(())
        }
    });
    let erased: &mut dyn ErasedTable = &mut table;
    erased.set_cell("rifle", "damage", "45").expect("set damage");
    erased
        .set_cell("rifle", "icon", "ui/new rifle.png")
        .expect("plain text into a String column");
    erased.set_cell("rifle", "kind", "Burst(2)").expect("enum");
    assert_eq!(
        erased
            .set_cell("rifle", "damage", "lots")
            .expect_err("bad")
            .code(),
        "table_bad_cell"
    );
    assert_eq!(
        erased
            .set_cell("rifle", "damage", "500")
            .expect_err("rule")
            .code(),
        "table_rule"
    );
    assert_eq!(
        erased.set_cell("nope", "damage", "1").expect_err("key").code(),
        "table_unknown_key"
    );
    assert_eq!(
        erased.set_cell("rifle", "weight", "1").expect_err("col").code(),
        "table_unknown_column"
    );
    erased
        .insert_row(
            "shotgun",
            &[
                ("damage".to_owned(), "60".to_owned()),
                ("spread".to_owned(), "4.0".to_owned()),
                ("kind".to_owned(), "Pistol".to_owned()),
                ("icon".to_owned(), "ui/shotgun.png".to_owned()),
            ],
        )
        .expect("insert");
    assert_eq!(
        erased.insert_row("shotgun", &[]).expect_err("dup").code(),
        "table_duplicate_key"
    );
    assert!(erased.remove_row("pistol"));
    let row = erased.row("rifle").expect("row");
    assert!(row.contains(&("damage".to_owned(), "45".to_owned())), "{row:?}");
    let csv = erased.export(TableFormat::Csv).expect("csv");
    assert!(csv.contains("shotgun,60,4.0,Pistol,ui/shotgun.png"), "{csv}");
    let rifle = table.get("rifle").expect("rifle");
    assert_eq!(
        (rifle.damage, rifle.kind.clone(), rifle.icon.as_str()),
        (45, WeaponKind::Burst(2), "ui/new rifle.png")
    );
    assert!(table.get("pistol").is_none());
}

#[test]
fn reload_swaps_good_files_and_keeps_rows_on_bad_ones() {
    let root = regia_test_utils::unique_temp_dir("data_tables_reload");
    let file = root.join("assets").join("tables").join("weapons.table.ron");
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    let mut app = App::new();
    app.register_data_table::<WeaponRow>("tables/weapons.table.ron");
    // No asset server or VFS: the startup load records the read failure and an empty table.
    app.update();
    {
        let tables = app.world().resource::<DataTables>();
        let slot = tables.slot("tables/weapons.table.ron").expect("slot");
        assert!(slot.table.is_empty());
        assert_eq!(codes(&slot.last_issues), vec!["table_bad_file"]);
    }

    write_atomic(&file, WEAPONS);
    app.world_mut()
        .write_message(SourceAssetChanged::new(file.clone()));
    app.update();
    {
        let tables = app.world().resource::<DataTables>();
        let slot = tables.slot("tables/weapons.table.ron").expect("slot");
        assert_eq!((slot.generation, slot.reloads, slot.rejected_reloads), (1, 1, 0));
        let typed = tables
            .get::<WeaponRow>("tables/weapons.table.ron")
            .expect("typed");
        assert_eq!(typed.get("rifle").expect("rifle").damage, 30);
    }

    write_atomic(&file, &WEAPONS.replace("damage: 30", "damage: \"thirty\""));
    app.world_mut()
        .write_message(SourceAssetChanged::new(file.clone()));
    // An unrelated change is ignored.
    app.world_mut().write_message(SourceAssetChanged::new(
        root.join("assets").join("other.table.ron"),
    ));
    app.update();
    {
        let tables = app.world().resource::<DataTables>();
        let slot = tables.slot("tables/weapons.table.ron").expect("slot");
        assert_eq!((slot.generation, slot.reloads, slot.rejected_reloads), (1, 1, 1));
        assert_eq!(codes(&slot.last_issues), vec!["table_bad_cell"]);
        let typed = tables
            .get::<WeaponRow>("tables/weapons.table.ron")
            .expect("typed");
        assert_eq!(typed.get("rifle").expect("rifle").damage, 30, "old rows stay");
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn write_atomic(path: &std::path::Path, text: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

#[test]
fn the_published_example_reads_as_typed_rows() {
    let text = include_str!("../../../docs/formats/data-table.example.table.ron");
    let mut raw = read_raw_ron(text).expect("example parses");
    assert_eq!(raw.row_type, "game::WeaponRow");
    // The example names the game's type; read it as this test's struct of the same shape.
    raw.row_type.clear();
    let table = DataTable::<WeaponRow>::from_raw(&raw, false).expect("example rows");
    assert_eq!(table.keys().collect::<Vec<_>>(), vec!["rifle", "pistol", "smg"]);
    assert_eq!(
        table.get("smg").expect("smg").muzzle,
        Some(Offset { x: 1, y: -2 })
    );
}

/// A file that ends on a backslash inside a string or character literal after an inner
/// attribute must return an error, not slice past the end of the text (shipping aborts on panic).
#[test]
fn truncated_escape_after_inner_attribute_does_not_panic() {
    for text in ["#!\"\\", "#!'\\", "#!(\"\\", "\"\\", "#!\"abc\\"] {
        // Any result is fine; reaching the assertion means no panic.
        let _ = RawTable::from_ron(text);
    }
}

#[test]
fn raw_table_attributes_end_before_the_table_body() {
    use regia_asset::data_table::RawTable;
    for prefix in [
        "#![enable(implicit_some)]\n",
        "/* before */ #![enable(implicit_some)]\n// between\n#![enable(unwrap_newtypes)]\n",
    ] {
        let text = format!("{prefix}[(\"sword\", (damage: 1))]\n");
        let raw = RawTable::from_ron(&text).expect("attribute then legacy table");
        assert_eq!(raw.source_version, 0);
        assert_eq!(raw.rows.len(), 1);
        assert_eq!(raw.rows[0].key, "sword");
        let text = format!("{prefix}(format_version: 1, row_type: \"\", rows: {{}})\n");
        let raw = RawTable::from_ron(&text).expect("attribute then envelope");
        assert_eq!(raw.source_version, 1);
        assert!(raw.rows.is_empty());
    }
    assert!(RawTable::from_ron("#![enable(implicit_some)\n[]").is_err());
}
