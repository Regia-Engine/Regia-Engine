# `.chooser.ron` (chooser, format `chooser` v1)

A context-driven result table (spec `docs/specs/51-curve-tables-and-choosers.md` §§ 2.2-2.6, 4.3, ADR-1649,
`regia_framework::chooser`, agent guide `book/agents/choosers.md`).

```ron
(
    format_version: 1,
    mode: Best,                             // First (default) | Best | Weighted | All
    result: Asset("montage"),               // Asset("<kind>" | "any") | Value(Bool|Int|Float|Name|Tags) | OutputOnly
    inputs: { "hit_angle": Float, "armed": Bool, "stance": Enum(["stand", "crouch"]) },
    outputs: { "rate": Float },
    columns: [
        (id: "armed",  input: "armed",     kind: Bool),
        (id: "stance", input: "stance",    kind: Enum),
        (id: "angle",  input: "hit_angle", kind: FloatDistance(influence: 1.0, max_distance: 180.0, wrap: Some((-180.0, 180.0)))),
        (id: "rate",   output: "rate",     kind: SetFloat),
    ],
    rows: [
        (id: "hit_front", cells: { "armed": True, "angle": 0.0, "rate": 1.0 }, result: Asset("anim/hit_front.montage.ron")),
        (id: "hit_back",  cells: { "angle": 180.0, "stance": Is("crouch"), "rate": 1.2 }, result: Asset("anim/hit_back.montage.ron")),
        (id: "nested",    enabled: false, cells: {}, result: Chooser("anim/hit_generic.chooser.ron")),
    ],
    catch_all: Some((result: Asset("anim/hit_generic.montage.ron"), outputs: { "rate": 1.0 })),
)
```

- Top level: `format_version`, `mode` (default `First`), `result` (required), `inputs`, `outputs`, `columns`,
  `rows`, `catch_all: Some((result, outputs))`, `near_best: Some(f32)`, `recency: Some((window, penalty))`.
  Unknown fields are refused.
- A column: `id`, `input` or `output` (bare strings), `kind` (`Bool`, `IntRange`, `FloatRange`, `Enum`, `Name`,
  `Tags`, `Asset`, `FloatDistance(influence: 1.0, max_distance: .., wrap: Some((lo, hi)), drop_beyond: false)`,
  `Weight`, `SetBool`, `SetInt`, `SetFloat`, `SetEnum`, `SetName`, `SetTags`, `SetAsset`), and the options
  `wrap: Some((lo, hi))` (ranges), `case_sensitive: false` (names), `disabled: true`.
- A row: `id`, `enabled` (default `true`), `cells: { column: cell }` (a missing cell is `Any`), `result` (default
  `Nothing`), `note`.
- A cell is read as RON text and typed by its column: `Any`, `True` / `False`, `(min, max)`, `Is("v")`,
  `IsNot("v")`, `OneOf(["a", "b"])`, `Kind("montage")`, `"name"`, a tag query (`Has("A.B")`), a number (distance
  target, weight), or an output value (`true`, `3`, `1.5`, `"name"`, `["A.B"]`).
- A result: `Asset("path")`, `Value(v)` (typed by `result: Value(kind)`), `Chooser("path")`,
  `Inline((result: .., columns: [..], rows: [..]))` (a nested table body without `format_version`), `Nothing`.
- Row order is evaluation order; every writer keeps it. Comments are allowed.
- The canonical writer (`ChooserTable::to_ron_string`, `chooser.export`) writes maps sorted by key, rows and
  columns in table order; writing a table read from that text gives the same bytes.

## Versions

v1 is the first version (`CHOOSER_FORMAT_VERSION`, `chooser_migration_sources()` is empty,
`chooser_schema_fingerprint()` for `cargo xtask check-formats`). A newer `format_version`, or 0, is refused with
`chooser_unsupported_version`. A future v2 adds a step to the shared `regia_core::version` loop and
`regia upgrade-project` support (spec 12 § 4a).

## Cooked form

`regia cook` validates a chooser, including the checks that need other tables (nested references, cycles, depth), so a
table with errors fails the cook; warnings and infos do not. It is stored in the pak as a cooked asset of type
`regia_framework::chooser::ChooserTable`: a little-endian `u32` version (`CHOOSER_COOKED_VERSION`, 1), then postcard of
a flat list of tables, each table's `Inline(..)` tables before it and the top table last. Cells are a typed enum keyed by
column index, and tag queries are post-order node lists. A load is a flat loop, so a crafted payload cannot drive
recursion: every `Inline` names one earlier table, once, and nesting stops at `MAX_COOKED_NESTING` (64,
`chooser_depth_exceeded`). `ChooserTable::from_cooked` then runs the structural checks text gets, and refuses `NaN` in a
value as the text parser does (`chooser_bad_number`). A version this build does not read is
`chooser_unsupported_version`; damaged bytes, a dangling or reused `Inline`, or a table nothing uses are `chooser_bad_file`.

The cooked form is a cache: a source file wins when one is readable, `regia package` ships choosers in the pak only, and
a pak of another cooked version is cooked again. The cooked header lists the assets the table names (`Asset(..)` and
`Chooser(..)` results, `Is(..)` cells of `Asset` columns, `Asset` values) that a pak holds; a raw file ships as itself and
is not a dependency. The registry tags the asset `kind=chooser` and `result=<kind>`: the asset kind of an `Asset(..)`
result type, the lowercase value kind of a `Value(..)` one, `output_only` otherwise.
