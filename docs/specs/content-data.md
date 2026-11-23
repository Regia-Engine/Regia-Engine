# Content data: localization workflow, asset registry queries, data tables

Status: proposed (ADR-1466). Extends `specs/18` § 6 (localization), `specs/12` § 3 and § 6 (manifest,
asset database) and `specs/14` (gameplay data). Agent surface: `specs/23`.

This page covers three pieces of content plumbing a game developer needs before a game has much text or data:

1. a **localization workflow** on top of the Fluent runtime (`regia_ui::localization`);
2. **asset registry queries** that answer "which assets have tag X, type Y, depend on Z" without
   loading any asset;
3. **data tables**: typed rows kept in RON or CSV, edited by people, spreadsheets and agents.

Sources (what each feature does as a user sees it, never an implementation): the Project Fluent syntax
guide and the `fluent-bundle` crate docs; the Unicode CLDR plural rules specification (UTS #35 part 3
§ 5.1, "Language Plural Rules") and the CLDR plural rule tables; RFC 4180 (CSV); general localization
practice (pseudo-localization, fallback chains, string extraction). Rows are in `docs/provenance.md`.

## 1. Localization workflow

### 1.1 Text keys in content and code

Text that a player reads is never a literal in content; it is a **key** into the project's Fluent files.
Keys are Fluent identifiers: `[a-zA-Z][a-zA-Z0-9_-]*` (`menu-start`, `hud_ammo`).

| Where | How a key is written | Who reads it |
|---|---|---|
| `.ui.ron` | `Loc("menu-start")` or `(text: "loc:menu-start")` | UI spawn, `regia loc gather` |
| Any `.ron` asset or scene | the same two forms, in any string field | `regia loc gather` |
| Rust source | `regia_ui::loc!("menu-start")` or `loc!("items", count = n)` | runtime, `regia loc gather` |
| `.ftl` files | `menu-start = Start Game` | runtime |

`loc!` is a declarative macro: `loc!("key")` is a `LocText { key, args: [] }` value and `loc!("key", name = expr, ...)`
also carries the arguments (each formatted with `Display`). The gather tool finds `loc!("...")` by text
scan, so the key must be a string literal (a computed key is invisible to the gather tool; the validator
reports it as unused if nothing else names it).

### 1.2 Files

```
assets/localization/<locale>/<file>.ftl     # one directory per locale; all .ftl in it are combined
assets/localization/localization.ron        # optional: source locale, fallbacks, locale list
```

`assets/localization/localization.ron` (format `localization` v1; under `assets/` so it ships and the
runtime reads `source` and `fallbacks` from `assets://localization/localization.ron`):

```ron
(
    format_version: 1,
    source: "en",                       // the locale that keys are authored in
    locales: ["en", "fr", "fr-CA", "de", "es", "ru", "pl", "ar", "ja"],
    fallbacks: { "pt-BR": ["pt-PT"] },  // extra edges, tried before the parent chain
    gather: (roots: ["assets", "src"], exclude: ["assets/localization"]),
)
```

### 1.3 Fallback chain

Resolving a key in locale `L` tries, in order, stopping at the first locale that has the message:

1. `L` itself;
2. each locale listed in `fallbacks[L]`, each followed by its own chain;
3. `L`'s parents by dropping the last subtag (`fr-CA` → `fr`; `zh-Hant-TW` → `zh-Hant` → `zh`);
4. the project fallback (`source`, default `en`).

Each locale appears once (the first time it is reached). `Localization::fallback_chain(L)` returns the chain
and `localization.status` reports it. A key missing from the whole chain shows `⟦key⟧` in dev builds and
the bare key in shipping builds (unchanged from `specs/18` § 6).

### 1.4 Runtime switch and event

`Localization::set_locale` accepts an exact locale or, when only a parent is loaded, the nearest loaded
parent (`fr-CA` with only `fr` loaded selects `fr`). Each switch bumps `generation` (texts re-resolve on the
next frame, as before) and the `LocalizationPlugin` systems send one `LocaleChanged { from, to,
generation }` message the frame after it; game code reads it with `MessageReader<LocaleChanged>`.

### 1.4a Start-up order, write-back and font locale (ADR-1618)

The start-up locale is the first of these that yields a *loaded* locale (matched by `locale_startup::match_loaded`:
the cleaned tag exactly, then at each step its script/region equivalents (`zh-TW` ↔ `zh-Hant`, `zh-CN` ↔
`zh-Hans`), then its parents; ASCII case ignored):

1. the `--locale <tag>` launch flag;
2. the `REGIA_LOCALE` environment variable (tests, CI);
3. the saved settings language, `GameSettings::language`, when not empty;
4. the Steam per-game language (`SteamBackend::game_language`, mapped to a tag by the fixed table
   `regia_steam::STEAM_LANGUAGES`) when Steam is connected;
5. the OS locale (`regia_platform::system_locale`);
6. the project source locale (`source`).

A value that matches nothing is skipped. The subsystems offer their raw values in the `regia_platform::LocaleHints`
resource; `load_localization` reads it at `Startup` and inserts `LocaleStartup { source, raw, selected, skipped }`.
`localization.status` reports it as `startup`. Tests set `LocaleHints::isolated()` so no real argument,
environment variable or OS locale is read.

A player's choice in a language menu is a `PlayerSetLocale(tag)` message. The UI selects the locale and sends
`LocaleWriteBack { locale }`; the framework settings plugin stores it in `GameSettings::language`, which the next
start-up reads as source 3. A switch from an agent or game code is not written back; `localization.set_locale`
takes `persist: true` to do so. Setting `GameSettings::language` (an options menu, `settings.set`) selects that
locale live.

Each locale should define the message `locale-name` with its own name in its own language, for language menus
(`Localization::self_name`). The text system's font-fallback locale follows the selected locale
(`text::sync_font_locale`), and the Han fallback prefers the matching regional face (`ja` → Japanese, `ko` →
Korean, `zh-Hant`/`zh-TW` → Traditional, `zh-HK` → Hong Kong) before the bundled Simplified subset.

### 1.5 Plural rules

Regia has its own CLDR plural table, `regia_ui::plural`, written from the CLDR specification's operands
(`n`, `i`, `v`, `w`, `f`, `t`) and the published rule text for: en, fr, de, es, it, pt, nl, sv, nb, nn, no,
ru, uk, be, pl, cs, ar, he, ja, zh, ko, tr (and the languages that share their rules). `plural::categories(locale)` lists
the categories a locale uses; `plural::category(locale, operands)` picks one.

The runtime uses the same table: when a locale's files load, every select whose variants name a plural
category is rewritten to select on `REGIA_PLURAL($n)`, a Fluent function backed by the table. Exact-number
variants (`[0]`) still win: they stay on an outer select whose default holds the routed one. The reason
is a bug in the rule data bundled with `fluent-bundle` 0.16 (`intl_pluralrules` 7.0.2): its Arabic rule
tests the integer digits where CLDR tests `n % 100`, so 103 read as `other` and 3.5 as `few`. The same
bundle's Belarusian `few` tests the whole integer `i` against `2..=4` where CLDR tests `n % 10`, so 22
read as `other` (the table says `few`) and a fraction such as 1.5 read as `one` (the table says `other`).
Norwegian Bokmål (`nb`), Norwegian (`no`) and Norwegian Nynorsk (`nn`) use `n = 1`, so `1.0` is `one`.
A test pins those upstream readings and checks every other locale agrees with the table on integers
0..=1000 and a set of decimals (including `1.0`), and that players see the table's category everywhere.

### 1.6 Validator

`regia_ui::loc_check::validate` takes the parsed locale files (and optionally the gathered key list) and
reports, each with locale, key, file and line:

| Code | Meaning | Severity |
|---|---|---|
| `loc_parse_error` | An `.ftl` file does not parse | error |
| `loc_duplicate_key` | A key is defined twice in one locale (the later one wins, as at runtime) | warning |
| `loc_missing_key` | A gathered key (or a source-locale key) is absent from a locale's whole fallback chain | error |
| `loc_untranslated` | A source-locale key is absent from the locale itself but a fallback has it | warning |
| `loc_unused_key` | A source-locale key no content or code names (needs the gather list) | warning |
| `loc_extra_key` | A locale has a key the source locale does not | warning |
| `loc_variable_mismatch` | A translation uses a `$variable` the source message does not, or drops one the source uses | error (added) / warning (dropped) |
| `loc_plural_missing` | A plural selector (one with a category variant such as `[one]`) has no variant for a category the locale uses, other than the one its default variant names (a Russian `items` with only `[one]` and `*[other]` lacks `few` and `many`) | warning |
| `loc_plural_unreachable` | A variant names a category the locale never selects (`[few]` in English) | warning |
| `loc_key_set_differs` | Summary: the locale's key set differs from the source locale's (count of missing / extra) | info |
| `loc_no_self_name` | Some locale defines `locale-name` (§ 1.4a) and this one does not (a project that defines it nowhere has no language menu and gets no finding) | warning |
| `loc_fallback_cycle` | The `fallbacks` edges lead back to a locale on their own path (`pt` → `es` → `pt`); one finding per cycle, on its smallest tag | warning |

`--strict` turns warnings into errors (CI). Exit status of `regia loc validate` is non-zero on errors.

### 1.7 Gather

`regia loc gather` walks `gather.roots`, collects keys from `.ron` (both forms of § 1.1) and `.rs`
(`loc!("...")`), and for every locale writes the keys missing from that locale into
`assets/localization/<locale>/gathered.ftl` (sorted, each with a `# source: path:line` comment). In the source
locale a new key gets its key as placeholder text (`menu-start = menu-start`); in other locales the
message is copied from the source with a `# TODO translate` comment, so the file stays valid Fluent and the
validator reports `loc_untranslated` until a translator replaces it. Existing messages are never
rewritten. `--dry-run` prints what would be written. Writes are atomic (temp file + rename).

### 1.8 Pseudo-localization

A test mode that keeps text readable while showing layout problems:

- every letter is replaced by an accented look-alike (`a` → `á`, `o` → `ö`, ...);
- the text is padded by about 40 % (`~` fill, at least 2 characters) to simulate languages that run long;
- the result is wrapped in `[` `]` so clipped text is visible;
- placeables (`{ $count }`) and markup are untouched.

`Localization::set_pseudo(true)` applies it to every resolved message (Fluent's text transform, so
placeables keep their real values); `localization.set_pseudo` and `regia loc pseudo <locale>` (which writes
a pseudo locale directory, `en-XA` by default) expose it. Bidi testing uses the `ar` locale instead.

## 2. Asset registry queries

### 2.1 What is recorded

At cook time, after the manifest, `regia_asset_pipeline::cook_project` writes `asset_registry.bin` (format
`asset_registry` v1, postcard, with a magic and version) into the cooked cache; packing writes `<pak stem>.registry.bin`
next to each pak (`base.registry.bin` for the default pak) and packaging ships it in `content/`. One record per manifest entry:

| Field | Source |
|---|---|
| `id`, `path`, `type_path` | the manifest entry |
| `dependencies` | the cooked header's dependency list (header read only, never the payload) |
| `tags` (`BTreeMap<String, String>`) | `config/asset_tags.ron` rules plus automatic tags |
| `size` | cooked file length in bytes |

Automatic tags: `ext` (lower-case source extension), `folder` (the source's directory under `assets/`),
`importer` (from `.meta`) when known. Authored tags come from `config/asset_tags.ron` (format `asset_tags` v1):

```ron
(
    format_version: 1,
    rules: [
        (paths: ["assets/props/forest/**"], tags: { "biome": "forest" }),
        (paths: ["assets/ui/**"], tags: { "group": "ui", "streaming": "always" }),
    ],
)
```

Later rules override earlier ones key by key; glob syntax is the cook's chunk syntax (`*`, `**`).

### 2.2 Queries

`regia_asset::registry::AssetRegistry` answers, with no asset loaded and no file opened after the registry:

- `by_id`, `by_path`;
- `query(&Query)`: all of `path` (glob), `path_contains` (substring, any case), `type_path` (exact or
  suffix after `::`), `tags` (each `key` or `key=value`), combined with AND; sorted by path; `limit`;
- `dependencies(id, transitive)` and `referencers(id, transitive)` (reverse index built once at load),
  transitive walks breadth-first, cycle-safe, in path order per level;
- `tag_keys()` and `tag_values(key)` for browsing.

### 2.3 Incremental updates

`AssetRegistry::apply_changes(changes)` replaces, inserts or removes single records and updates the reverse
index for just those records; the editor's live cook calls it after each recook, so the registry the editor
and its agent see is current without a full rebuild. `generation` counts updates.

### 2.4 Surfaces

- CLI: `regia assets query [--tag k=v]... [--type T] [--path GLOB] [--deps ID|PATH] [--refs ID|PATH]
  [--transitive] [--json] [--platform P]` reads the project's cooked registry (or `--registry FILE`).
- Agent: `registry.query`, `registry.describe`, `registry.dependencies`, `registry.referencers`,
  `registry.tags` (book `agents/asset-registry.md`).

## 3. Data tables

### 3.1 Model

A data table is a list of rows of one Rust struct type, each row named by a unique **key** (a string).
The row type is any struct with `#[derive(Reflect, Serialize, Deserialize)]`; reflection gives the column
list (field names, types, doc comments), serde reads and writes cells.

```rust
#[derive(Reflect, Serialize, Deserialize, Clone, Default)]
struct WeaponRow { damage: u32, spread: f32, kind: WeaponKind, icon: String }

let table: DataTable<WeaponRow> = DataTable::from_ron_str(text)?;
let rifle = table.get("rifle");
```

### 3.2 RON form (`.table.ron`, format `data_table` v1)

```ron
(
    format_version: 1,
    row_type: "game::WeaponRow",      // the row struct's type path (checked on load)
    rows: {
        "rifle": (damage: 30, spread: 0.5, kind: Rifle, icon: "ui/rifle.png"),
        "pistol": (damage: 12, spread: 1.5, kind: Pistol, icon: "ui/pistol.png"),
    },
)
```

Rows keep file order. Loading runs the table migrators first (`data_table` v0 = a bare `[(key, row)]`
list without the envelope; future versions add steps), so the format is migrator-aware like the other
frozen formats (`specs/12` § 4a).

### 3.3 CSV form (`.table.csv`, RFC 4180)

The first record is the header. The first column is the key column, its header is `key`. Each other header
names a field of the row struct. String fields are plain cell text; every other field is a RON literal
(`30`, `0.5`, `true`, `Rifle`, `(x: 1, y: 2)`, `["a", "b"]`). An empty cell in a `String` column is the empty string;
in any other column it leaves the field at its `Default` (when the struct implements serde defaults) or is an error.

The parser follows RFC 4180: CRLF or LF line ends (also a lone CR), fields optionally enclosed in double
quotes, `""` inside a quoted field is one quote, quoted fields may hold commas and line breaks, every record
must have the header's field count; a UTF-8 byte-order mark at the start is skipped. The writer quotes a
field only when it contains a comma, a quote, CR or LF, or leading/trailing spaces or tabs, and ends records with CRLF.
A record of one empty field is written as `""`, since an empty line holds no record when read back (ADR-5662).

### 3.4 Validation

`DataTable::validate` (and `regia table validate`) reports, with row key and column:
`table_duplicate_key`, `table_empty_key`, `table_unknown_column`, `table_missing_column`,
`table_bad_cell` (with the parse error), `table_wrong_row_type`, `table_bad_record` (a CSV record with the
wrong field count), plus row-level rules a game adds with `DataTable::with_rule(|key, row| ...)`.

### 3.5 Runtime registry, hot reload, agents

`DataTables` (a resource) holds every loaded table by asset path, type-erased behind `ErasedTable`
(columns, keys, get/set a cell as RON text, export RON/CSV, validate). `app.register_data_table::<R>("tables/weapons.table.ron")`
loads it at startup and, with `dev-assets`, reloads it when the file changes (the reload is applied only
when the new file validates; otherwise the old rows stay and the error is logged and kept for agents).
`generation` per table counts reloads and edits.

Agent methods: `tables.list`, `tables.describe`, `tables.get`, `tables.set`, `tables.validate`,
`tables.export` (book `agents/data-tables.md`). CLI: `regia table validate|to-csv|to-ron <file>` works without
the row type (structural checks only, cells kept as text); typed checks run in the game through agents.

### 3.6 Cook and editor (as built, ADR-2132)

`regia cook` writes each `.table.ron` / `.table.csv` as `data-table-cooked` v1 (version word plus postcard;
`docs/formats/data-table-ron.md` § Cooked form) after its structural checks. Text wins at run time; with no text,
`register_data_table` loads the pak's copy (`DataTable::from_cooked`, same rows and errors). `regia package` ships the raw
files as before and the cooked copy too; a table that does not check is a cook warning and stays out of the pak.
The editor's Tables panel has a **Data** tab: a grid of keys and fields whose schema comes from the loaded table, else
the reflected row struct; schema errors badge their cells, an edit that would add an error is refused, 32 undo steps,
Save writes the file (`editor.tables_panel_data_edit`, `book/agents/editor-data-panels.md`).

## 4. Acceptance

- Localization: a menu in three locales with a plural (`samples`-free worked example in the book page and a
  test, `regia_ui/tests/loc_workflow.rs`): gather finds keys in a `.ui.ron`, a scene `.ron` and a `.rs`
  file; validate reports each code of § 1.6 on a crafted fixture; fallback `fr-CA → fr → en`; locale switch
  sends `LocaleChanged`; pseudo mode output; plural table agrees with Fluent.
- Registry: queries over a built registry; cook writes it; incremental update keeps the reverse index equal
  to a full rebuild.
- Tables: RFC 4180 conformance cases (quotes, embedded CRLF, BOM, ragged records); RON ↔ CSV round trip of a
  typed table; validator codes; agent edit then export; reload keeps the old rows on a bad file.
