# Curve tables (agents)

A curve table is a file of named float curves sampled by name at runtime: damage falloff over distance, XP per
level, difficulty ramps (`regia_framework::curve_table`, spec 51 § 2.1, ADR-1649). The `curves.*` methods list,
sample and edit the tables the running app loaded. They need `CurveTablePlugin` (added by
`app.register_curve_table("tables/falloff.curves.ron")`). `AgentPlugin` registers them on the `AgentExtension`
in every app with the agent (ADR-1809; `regia_agent::curve_tables::install_curve_table_methods` does the same
for a host that builds its own extension). Without the plugin every method answers `{ "available": false }`.

## The model

- A table holds up to 4,096 curves in file order, each named `[A-Za-z0-9_.-]{1,64}`.
- A curve holds 1 to 4,096 keys `(x, y)` with finite numbers and strictly increasing `x`. A one-key curve is a
  constant.
- Interpolation (`step`, `linear`, `cubic`) is set per table, overridden per curve and per key (the key's setting
  controls the segment that starts at it). `cubic` is cubic Hermite with tangents derived once at load by finite
  differences and the Fritsch-Carlson limiter, so a monotone run of keys never overshoots. A key's explicit
  `in_tangent` / `out_tangent` win.
- Extrapolation per side (`pre`, `post`): `hold` (default, the end value), `linear` (the end segment's slope; the
  end tangent of a cubic segment, flat after a step), `cycle`, `cycle_offset` (each loop offset by
  `y_last - y_first`), `ping_pong` (mirrored on odd loops).
- Sampling with a non-finite `x` returns the first key's value and logs `curve_sample_nonfinite` (once per curve
  per second of frame time).
- A table in memory is always valid: every edit is checked first and refused with its error, leaving the table
  unchanged. Edits bump the table's `generation` and change memory only; `curves.export { write: true }` saves.

## Methods

| Method | Params | Result | Permission |
|---|---|---|---|
| `curves.list` | none | `{ available, tables: [{ path, interp, curves, generation, errors, warnings, file, cooked }] }` | read |
| `curves.describe` | `path` | list entry + `curves: [{ name, keys, pre, post, interp, x_range, y_range }]`, `last_issues` | read |
| `curves.get` | `path`, `name` | `{ keys: [{ x, y, interp, in_tangent, out_tangent }], pre, post, interp }` | read |
| `curves.sample` | `path`, `name`, `x` or `from`, `to`, `count` (1..=4096, default 2) | `{ y }` or `{ points: [[x, y], ...] }` | read |
| `curves.set_key` | `path`, `name`, `x`, `y`, `interp?`, `in_tangent?`, `out_tangent?` | `{ generation, issues }` | assets |
| `curves.remove_key` | `path`, `name`, `x` | `{ generation }` | assets |
| `curves.set_curve` | `path`, `name`, `keys?`, `pre?`, `post?`, `interp?`, `rename?` (creates the curve when absent) | `{ generation, issues }` | assets |
| `curves.delete` | `path`, `name` | `{ generation }` | assets |
| `curves.validate` | `path` | `{ issues: [{ code, severity, message, curve, line }] }` | read |
| `curves.export` | `path`, `format?` (`ron` default, `csv`), `write?` | `{ text }`, plus `{ file }` with `write` | assets |
| `curves.import` | `path`, `csv`, `interp?` (`linear` default) | `{ curves, generation, issues }` | assets |

- `cooked` says the table came from the pak rather than source text. A source file wins when one is readable, so a dev
  run shows `false` and a packaged game shows `true`; a table that fails to validate fails `regia cook`.
- `keys` in `curves.set_curve` is `[[x, y], ...]` or `[{ x, y, interp?, in_tangent?, out_tangent? }, ...]`.
- `interp` values are `step`, `linear`, `cubic` (or `null` in `set_curve` for the table default); extrapolation
  values are `hold`, `linear`, `cycle`, `cycle_offset`, `ping_pong`.
- `curves.export` is in the `assets` class because `write: true` saves a file (atomically: a temporary file and a
  rename). A table with no source file (a packaged build) answers `read_only_asset`.
- `curves.import` replaces the table's curves with the CSV's (CSV carries no extrapolation, tangents or per-key
  interpolation).

## Errors

Protocol errors are `F0002` (bad params) or `F0003` (not found) with `data.reason`; a refused edit or import also
carries `data.issues`.

| Reason | When |
|---|---|
| `curve_table_not_found` | no table at `path` (`F0003`) |
| `curve_not_found` | no curve `name` in the table (`F0003`) |
| `key_not_found` | `curves.remove_key` at an `x` with no key (`F0003`) |
| `bad_value` | a missing or mistyped parameter |
| `read_only_asset` | `curves.export { write: true }` for a table without a source file |
| `io_failed` | the file could not be written (`F0001`) |
| `validation_failed` | an edit was refused without a more specific code |

Validator codes (also as reasons of refused edits, and in `curves.validate` / `last_issues`):

| Code | Meaning |
|---|---|
| `curve_duplicate_name` | two curves share a name |
| `curve_bad_name` | a name is not 1 to 64 characters of `A-Z a-z 0-9 _ . -` |
| `curve_empty` | a curve has no keys (or an edit would remove its last key) |
| `curve_bad_number` | a non-finite `x`, `y` or tangent |
| `curve_unsorted_x` | keys (or CSV axis headers) not in increasing `x` |
| `curve_duplicate_x` | two keys at the same `x` |
| `curve_too_many_keys` | more than 4,096 keys |
| `curve_too_many_curves` | more than 4,096 curves |
| `curve_csv_ragged` | a CSV axis header that is not a finite number or repeats |
| `curve_bad_file` | RON or CSV syntax, an unknown field, a missing `name` header |
| `curve_unsupported_version` | `format_version` newer than 1, or 0 |
| `curve_ref_unresolved` | a `CurveRef` names a table or curve that is not loaded (reported by `CurveRef::resolve` and the `validate_curve_refs` data-table validator pass) |
| `curve_sample_nonfinite` | (log warning) a curve was sampled with a non-finite `x` |

## Checks agents run

1. `curves.list`: `errors` is 0 for every table you rely on.
2. `curves.sample { path, name, from, to, count: 64 }` and check the shape (monotone where it should be, the
   values at the ends).
3. Edit with `curves.set_key`, sample again, then `curves.export { write: true }` to keep it.
4. A spreadsheet round trip: `curves.export { format: "csv" }`, change the text, `curves.import { csv }`.

Spec 53 effects read curve tables too: a `Curve((table: "tables/damage.curves.ron", name: "fire"))` amount samples
the loaded table at the effect level (see [abilities](abilities.md)).

A typed data-table row may hold a `CurveRef { table, name }` field directly, behind an `Option`,
in a nested struct, or inside any reflected container (list, array, tuple, tuple struct, map key
or value, any enum variant's fields); `regia_framework::curve_table::validate_curve_refs` walks a
loaded `DataTable` by reflection and returns one `curve_ref_unresolved` issue per row whose
reference does not resolve (naming the row key). It is a library call: a game runs it on its
typed tables after load (and after a reload) and logs or surfaces the issues — the agent's
`tables.validate` does not run it, because the erased tables it works on do not carry the typed
row reflection (spec 51 § 2.1).

The file format is `docs/formats/curve-table-ron.md`.
