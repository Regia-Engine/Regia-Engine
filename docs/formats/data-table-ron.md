# `.table.ron` and `.table.csv` (data table, format v1)

Rows of one reflected Rust struct, each named by a unique string key (ADR-1466, `regia_asset::data_table`,
spec `docs/specs/content-data.md` § 3, user guide `book/user-guide/data-tables.md`). The rows' fields are the game's
row struct, so only the envelope is a Regia Engine format.

## RON form

```ron
(
    format_version: 1,
    row_type: "game::WeaponRow",   // the row struct's type path, its short name, or "" (unchecked)
    rows: {
        "rifle": (damage: 30, spread: 0.5, kind: Rifle, icon: "ui/rifle.png"),
        "pistol": (damage: 12, spread: 1.5, kind: Pistol, icon: "ui/pistol.png", tags: ["sidearm"]),
    },
)
```

- `rows` keeps file order; a duplicated key is kept and reported (`table_duplicate_key`); lookups use the first.
- A row may name its struct (`WeaponRow(damage: 30, ...)`) or not. Fields left out take their serde default.
- Comments (`//`, `/* */`) are allowed anywhere RON allows them.

Versions: **v0** is a bare list `[("rifle", (damage: 30, ...)), ...]` with no envelope; it is read and migrated on
load (`DATA_TABLE_MIGRATIONS`, the shared `regia_core::version` loop) and `regia table to-ron old.table.ron
--row-type game::WeaponRow -o new.table.ron` rewrites it as v1. A file with a newer `format_version` is refused with
`table_unsupported_version`. The source is registered in `cargo xtask check-formats` (`data-table`; the cooked form is
`data-table-cooked`) since ADR-2316, and `regia upgrade-project --write` rewrites a v0 list as the v1 envelope. A
`.table.csv` has no version (its header is the row type's columns) and is not migrated.

## CSV form (RFC 4180)

```
key,damage,spread,kind,icon,tags
rifle,30,0.5,Rifle,ui/rifle.png,[]
pistol,12,1.5,Pistol,ui/pistol.png,"[""sidearm""]"
```

- The first record is the header; its first column is `key`, every other column names a field.
- `String` fields are plain cell text; every other field is a RON literal (`30`, `0.5`, `true`, `Rifle`,
  `Burst(3)`, `(x: 1, y: 2)`, `["a"]`). A field whose RON form is a string but whose type is not `String`
  (`PathBuf`) may also be plain text.
- An empty cell: the empty string in a `String` column; the field's serde default in any other column.
- Records end with CRLF when written; LF and a lone CR are read too. Fields with a comma, a quote, a line break or
  leading/trailing spaces are quoted, and a quote inside is doubled. A UTF-8 byte-order mark is skipped; empty
  lines are skipped.

The example file is `docs/formats/data-table.example.table.ron`.

## Cooked form (`data-table-cooked` v1, ADR-2132)

`regia cook` reads `.table.ron` and `.table.csv` without the game's row struct, checks their structure (keys, one RON
value per cell), and writes a cooked asset of type `regia_asset::data_table::CookedDataTable`: a little-endian `u32`
version (`DATA_TABLE_COOKED_VERSION`, 1) then postcard. A RON table keeps its row type, its columns, and per row the
key, the source line and `(column index, RON literal text)` cells; a CSV table keeps its header and records as the file
had them, so the typed CSV cell rules (plain text in a `String` field) run at load exactly as for the text.
`DataTable::from_cooked` reads it to the same typed rows and the same errors, with the source lines, as the text.

A cooked payload is not trusted: a version this build does not read is `table_unsupported_version`; damaged or
truncated bytes, bytes after the table, a column index out of range or a column named twice in a row are
`table_bad_file`; a CSV record of the wrong length is `table_bad_record`. The load is one pass over the rows.

The cooked form is a cache: a shape change bumps the version and a pak of another version is cooked again; nothing to
migrate. At run time the source text wins when one is readable, else `register_data_table` loads the pak's cooked copy
(`tables.list` reports `cooked: true`). `regia package` still ships the raw files (a host or a dedicated server may read
them through `assets://`) and puts the cooked copy in the pak as well. A table whose structure does not check is left
out of the pak with a cook warning, not an error: it ships raw and its load reports the same issue. The registry tags the
asset `kind=data_table` and `row_type=<the file's row type>`.
