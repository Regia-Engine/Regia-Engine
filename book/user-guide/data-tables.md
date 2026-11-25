# Data tables

<!-- index: Data table -->
<!-- index: .table.ron | see: Data table -->

A data table holds rows of one Rust struct, each row named by a key: weapons, enemy waves, level-up costs, shop
prices. Designers edit the table in a text editor or a spreadsheet, the game reads typed rows, and agents can read
and change it while the game runs. Spec: `docs/specs/content-data.md` § 3; format: `docs/formats/data-table-ron.md`;
agent methods: [Data tables (agents)](../agents/data-tables.md).

## 1. Write the row struct

The row is an ordinary struct that derives `Reflect` (its fields become the table's columns, with their doc
comments) and serde's `Serialize`/`Deserialize` (cells are read and written as RON):

```rust
use regia::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Reflect, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
enum WeaponKind { #[default] Rifle, Pistol, Burst(u8) }

/// One weapon.
#[derive(Reflect, Serialize, Deserialize, Clone, Debug, Default)]
struct WeaponRow {
    /// Damage per hit.
    damage: u32,
    spread: f32,
    kind: WeaponKind,
    icon: String,
    #[serde(default)]
    tags: Vec<String>,
}
```

## 2. Write the table

`assets/tables/weapons.table.ron`:

```ron
(
    format_version: 1,
    row_type: "WeaponRow",
    rows: {
        "rifle": (damage: 30, spread: 0.5, kind: Rifle, icon: "ui/rifle.png"),
        "pistol": (damage: 12, spread: 1.5, kind: Pistol, icon: "ui/pistol.png", tags: ["sidearm"]),
    },
)
```

`row_type` is the struct's type path or just its name; the game refuses a file that names another type.

Or the same table as a spreadsheet, `assets/tables/weapons.table.csv` (save as "CSV UTF-8" from any spreadsheet):

```
key,damage,spread,kind,icon,tags
rifle,30,0.5,Rifle,ui/rifle.png,[]
pistol,12,1.5,Pistol,ui/pistol.png,"[""sidearm""]"
```

The first column is `key`. Text columns (`String` fields) hold plain text; every other column holds a RON value:
`30`, `0.5`, `true`, an enum variant (`Rifle`, `Burst(3)`), a struct (`(x: 1, y: 2)`), a list (`["a"]`). An empty
cell is the empty string in a text column and the field's default in any other.

## 3. Register and read it

```rust,wrap
# use regia::prelude::*;
# #[derive(Reflect, serde::Serialize, serde::Deserialize, Clone, Debug, Default)] struct WeaponRow { damage: u32 }
# let app: &mut App = todo!();
use regia_asset::data_table::{DataTableAppExt, DataTables};

app.register_data_table::<WeaponRow>("tables/weapons.table.ron");

fn fire(tables: Res<DataTables>) {
    let Some(weapons) = tables.get::<WeaponRow>("tables/weapons.table.ron") else { return };
    if let Some(rifle) = weapons.get("rifle") {
        // rifle.damage, rifle.kind, ...
    }
}
```

`DataTable<R>` also has `iter()` (rows in file order), `keys()`, `len()`, `insert`, `remove`, and
`from_ron_str` / `from_csv_str` / `to_ron_string` / `to_csv_string` for tools and tests.

Rules a game wants to hold for every row go on the table:

```rust
# use regia::prelude::*;
# use regia_asset::data_table::DataTables;
# #[derive(Reflect, serde::Serialize, serde::Deserialize, Clone, Debug, Default)] struct WeaponRow { damage: u32 }
fn add_rules(mut tables: ResMut<DataTables>) {
    if let Some(weapons) = tables.get_mut::<WeaponRow>("tables/weapons.table.ron") {
        weapons.add_rule(|_key, row| if row.damage == 0 { Err("damage must be positive".into()) } else { Ok(()) });
    }
}
```

## 4. Live editing

In a dev build (`regia run --dev`) saving the file reloads the table on the next frame. A file with an error (a
cell that does not parse, an unknown column, a duplicate key, a failed rule) is **not** applied: the old rows stay,
the error is logged with its row, column and line, and `tables.validate` reports it until the file is fixed.

The editor's **Tables** panel opens a table on its **Data** tab: one grid row per key, one column per field, with
the field types from the row struct. Errors badge their cells, an edit that would add one is refused, Undo steps back
and Save writes the file (agents: `editor.tables_panel_data_edit`, [editor gameplay data panels](../agents/editor-data-panels.md)).

`regia cook` also writes a checked, compact copy of every table into the pak (`data-table-cooked`). A game reads the
text when it has it and the pak's copy otherwise; `regia package` ships both, so nothing changes for code that reads
the files itself.

## 5. Checking and converting from the command line

The CLI works without the game's row struct, so it checks what needs no types and converts between the forms:

```sh
regia table validate assets/tables/weapons.table.ron      # syntax, keys, cells, missing columns; --strict, --json
regia table to-csv assets/tables/weapons.table.ron -o weapons.csv
# edit weapons.csv in a spreadsheet, then bring it back into the same table:
regia table to-ron weapons.csv --into assets/tables/weapons.table.ron
```

JSON works the same way, for tools and scripts that prefer it: the table is an array of row objects, the row key under
`"key"` and one member per column in column order (a number or `true`/`false` cell as a JSON number or boolean, a string cell
as its text, any other RON value as its RON text; a column a row does not set is left out):

```sh
regia table to-json assets/tables/weapons.table.ron -o weapons.json
regia table from-json weapons.json --into assets/tables/weapons.table.ron
```

`from-json` takes the same `--into`, `--row-type` and `--text-columns` as `to-ron`. A `.table.csv` converts too
(`regia table to-json` reads either form). In the editor, the Tables panel's **Import…** and **Export…** do the same
for the open data table and for curve tables (see [the panel's agent page](../agents/editor-data-panels.md)).

`--into` keeps the existing table's row type and keeps its text columns text, so a cell such as `smg` in the icon
column stays a string. Without `--into`, a column is text when one of its cells is not a RON value
(`ui/rifle.png`); name others with `--text-columns icon,name`. `regia table to-ron old.table.ron` also rewrites a
table from an older format version in the current one.

Typed checks (does `damage` parse as a number, the game's rules) run in the game: `tables.validate` over the agent
protocol, or `DataTable::validate` in a unit test.
