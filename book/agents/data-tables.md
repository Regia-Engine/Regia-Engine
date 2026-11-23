# Data tables (agents)

A data table is a list of rows of one Rust struct, each row named by a key (`docs/specs/content-data.md` § 3,
ADR-1466). The game registers each table with `app.register_data_table::<WeaponRow>("tables/weapons.table.ron")`;
the table is read at startup into the `DataTables` resource and, in dev builds, read again when its file changes.
The `tables.*` methods read and edit those tables in the running game. The user-facing guide is
[Data tables](../user-guide/data-tables.md); the file format is in `docs/formats/data-table-ron.md`.

Cells travel as **RON literal text**: `"30"`, `"0.5"`, `"true"`, `"Rifle"`, `"Burst(3)"`, `"(x: 1, y: 2)"`,
`"[\"a\", \"b\"]"`, `"\"ui/rifle.png\""`. A column of type `String` also takes plain text (`"ui/rifle.png"`),
as a CSV cell does. A number or boolean JSON value is accepted for `value` and written as RON.

## `tables.list`

Read-only, no parameters. Returns `{ tables: [...] }`, one entry per table:
`path`, `row_type`, `format` (`ron` or `csv`), `rows`, `columns`, `generation` (bumped by every applied reload
and every edit), `reloads` (applied), `rejected_reloads` (a changed file that had errors: the old rows stayed),
`cooked` (true when no source text existed and the startup load read the table's cooked form from a pak),
`errors` and `warnings` (of the last load or reload) and `file` (where `tables.export` with `write` saves; `null`
without an asset server). A game without tables answers `{ tables: [] }`.

## `tables.describe`

`{ path }`. The `tables.list` entry plus `columns: [{ name, type_path, text, doc }]` from the row struct's
reflection (`text` is true for a `String` column, `doc` is the field's doc comment), `keys` (every row key in
order) and `last_issues` (each `{ code, severity, message, key, column, line }`).

## `tables.get`

`{ path, key? | keys? | offset?, limit? }`. With `key`, one row; with `keys`, those rows in that order; otherwise a
page of rows in order (`offset` default 0, `limit` default 200). Returns
`{ path, generation, total, rows: [{ key, cells: { column: RON text } }] }`.

```json
{ "method": "tables.get", "params": { "path": "tables/weapons.table.ron", "key": "rifle" } }
→ { "rows": [{ "key": "rifle", "cells": { "damage": "30", "kind": "Rifle", "icon": "\"ui/rifle.png\"" } }], ... }
```

## `tables.set`

Mutating (`assets` permission). Exactly one of:

- `{ path, key, column, value }`: set one cell;
- `{ path, key, cells: { column: value, ... } }`: set several cells, all or nothing; when `key` is not in the
  table the row is **added** (fields left out take their serde defaults);
- `{ path, key, remove: true }`: remove the row.

The edit applies only when the edited row still parses and passes the game's row rules
(`DataTable::with_rule`); otherwise nothing changes. Returns `{ path, action: "set" | "added" | "removed",
generation, rows, row: { key, cells } | null }`. The change lives in memory until `tables.export` with `write`
saves it.

```json
{ "method": "tables.set", "params": { "path": "tables/weapons.table.ron", "key": "rifle", "column": "damage", "value": "45" } }
```

## `tables.validate`

Read-only. `{ path? }` (every table without it). Returns
`{ tables: [{ path, ok, issues }], errors, warnings }`. Issues are the table's own checks (`table_duplicate_key`,
`table_empty_key`, `table_rule`) plus the errors of a load or reload that failed, so a refused hot reload stays
visible until the file is fixed. Check it after editing a table file on disk: `play.wait_until` on the
`generation` of `tables.list`, then `tables.validate`.

## `tables.export`

`{ path, format?: "ron" | "csv", write? }`. Returns `{ path, format, bytes, text, written }`. `format` defaults to
the table's own form. With `write: true` the text replaces the table's file (written to a sibling and renamed, so
the live reload never reads a half-written file; the reload that follows sees the same rows). Saving needs the
table's own form and a dev build with an asset server.

## Issue codes

The codes of `issues`, `last_issues` and the `regia table validate` CLI:

| Code | Meaning |
|---|---|
| `table_duplicate_key` | Two rows share a key (lookups use the first). |
| `table_empty_key` | A row has an empty key. |
| `table_unknown_column` | A CSV header or a row names a field the row struct does not have. |
| `table_missing_column` | No value for a field that has no default (an error), or a row lacks a column other rows have (a warning in the CLI). |
| `table_bad_cell` | A cell does not parse as its column's type; the issue names the row, column and line. |
| `table_wrong_row_type` | The file's `row_type` is not the table's row struct. |
| `table_bad_record` | A CSV record does not parse (a stray quote, an unclosed quoted field) or has the wrong number of fields. |
| `table_rule` | A game row rule rejected a row. |
| `table_bad_file` | The file is not a table (syntax, missing `format_version` or `rows`, no `key` header column) or could not be read. |
| `table_unsupported_version` | The file is a newer format version than this engine reads. |

## Error codes

| Code | When |
|---|---|
| `bad_path` | `path` is missing or not a string. |
| `table_unknown` | No table is loaded at `path`. |
| `bad_key` | `key` or `keys` is not a string (array of strings). |
| `table_unknown_key` | No row has the key. |
| `bad_column` | `column` is not a string. |
| `table_unknown_column` | The row struct has no such field. |
| `bad_value` | `value`/`cells` is not a string, number or boolean (`cells` not an object), or `value` is missing with `column`. |
| `bad_edit` | Not exactly one of `column` + `value`, `cells`, `remove`. |
| `table_bad_cell` | The value does not parse as the column's type, or a new row lacks a field with no default. |
| `table_rule` | A row rule rejected the edited row; nothing changed. |
| `table_duplicate_key` | Adding a row whose key exists. |
| `table_empty_key` | Adding a row with an empty key. |
| `table_encode` | The row type does not serialize as a struct (a game bug). |
| `bad_format` | `format` is not `"ron"` or `"csv"`. |
| `table_no_file` | `write` with no file on disk (no asset server). |
| `table_format_mismatch` | `write` in the other form than the table's file. |
| `table_write_failed` | The file could not be written. |
