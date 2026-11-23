# Choosers (agents)

A chooser is an ordered table of rows. Each row has one condition cell per column and a result; given a context
(named input values) evaluation returns the result of the row that matches, and can record why every other row did
not (`regia_framework::chooser`, spec 51 §§ 2.2-2.7, ADR-1649). Games use it to pick a clip, a montage, a sound
cue, a loot table or any value without an `if` ladder. The `chooser.*` methods inspect, edit and evaluate the
choosers the running app loaded. They need `ChooserPlugin` (added by `app.register_chooser(path)` or
`app.register_chooser_provider(name, fn)`). `AgentPlugin` registers them on the `AgentExtension` in every app
with the agent (ADR-1809; `regia_agent::choosers::install_chooser_methods` does the same for a host that builds
its own extension). Without the plugin every method answers `{ "available": false }`.

## The model

- **Schema**: declared `inputs` and `outputs` with kinds `Bool`, `Int`, `Float`, `Enum([..])`, `Name`, `Tags`,
  `Asset`. Columns bind one input (or output) of a matching kind.
- **Columns** by class:

| Class | Kind | Cell |
|---|---|---|
| gate | `Bool` | `Any`, `True`, `False` |
| gate | `IntRange`, `FloatRange` | `Any` or `(min, max)` inclusive; the column's `wrap: (lo, hi)` lets a range run past `hi` back to `lo` |
| gate | `Enum` | `Any`, `Is(v)`, `IsNot(v)`, `OneOf([..])` |
| gate | `Name` | `Any` or a string; column option `case_sensitive` (default true) |
| gate | `Tags` | `Any` or a tag query (`Has("A.B")`, `AllOf([..])`, ...) |
| gate | `Asset` | `Any`, `Is(path)`, `Kind(kind)` (`anim/hit.montage.ron` is kind `montage`) |
| rank | `FloatDistance(influence, max_distance, wrap, drop_beyond)` | `Any` (cost 0) or a target; adds `influence * min(gap, max_distance) / max_distance` |
| draw | `Weight` | `f32 >= 0` (default 1); one per table |
| emit | `SetBool` ... `SetAsset` | the value written to the column's output when the row wins |

- A missing cell is `Any`. A missing input fails every gate except `Any`; NaN fails every gate except `Any`; an
  absent or NaN input costs a rank column its full `influence` (and `drop_beyond` rejects it). Kinds must match:
  an `Int` input never satisfies a `FloatRange`.
- **Modes**: `first` (the first passing row), `best` (lowest cost; within 1e-6 the earlier row wins), `weighted`
  (one `Rng::f32()` draw proportional to weights, optionally only among rows within `near_best` of the best cost,
  with `recency` repeat avoidance), `all` (every passing row, `all: true` in `chooser.evaluate`).
- **Results**: `Asset(path)`, `Value(v)`, `Chooser(path)` (a nested chooser, evaluated only when its row wins),
  `Inline((..))` (a nested table in the same file), `Nothing`. A nested chooser that selects nothing strikes its
  row and selection repeats over the rest (in `first` mode the scan continues). Nesting deeper than 8 answers with
  the top table's catch-all and reports `chooser_depth_exceeded`. A plain catch-all is returned as stored. A
  `Chooser` or `Inline` catch-all is evaluated once; if that evaluation itself exceeds depth, the result is
  `Nothing`, and `chooser.evaluate` with `all: true` returns `results: []`.
- **Catch-all**: the result (and outputs) used when no row is selected; without one the answer is `null`.
- Outputs of the winning row (then of its nested winner) are written to the context's outputs; inputs never change.

## Methods

| Method | Params | Result | Permission |
|---|---|---|---|
| `chooser.list` | none | `{ available, tables: [{ path, mode, result, rows, columns, generation, errors, warnings, file, cooked }] }` | read |
| `chooser.describe` | `path` | list entry + `inputs`, `outputs`, `columns`, `row_ids`, `last_issues` | read |
| `chooser.get` | `path`, `row?` | `{ table }` (mode, result, inputs, outputs, columns, rows, catch_all, near_best, recency) or `{ row }` | read |
| `chooser.set_cell` | `path`, `row`, `column`, `value` | `{ generation, issues }` | assets |
| `chooser.set_row` | `path`, `row`, `enabled?`, `result?`, `cells?`, `note?`, `before?` (creates the row when absent; `before` places it) | `{ generation, issues }` | assets |
| `chooser.remove_row` | `path`, `row` | `{ generation }` | assets |
| `chooser.set_column` | `path`, `column: { id, input?, output?, kind, wrap?, case_sensitive?, disabled? }`, `before?` | `{ generation, issues }` | assets |
| `chooser.remove_column` | `path`, `column` (its cells go too) | `{ generation, issues }` | assets |
| `chooser.set_options` | `path`, `mode?`, `result?`, `catch_all?`, `near_best?`, `recency?` | `{ generation, issues }` | assets |
| `chooser.validate` | `path` | `{ issues: [{ code, severity, message, row, column, line }] }` | read |
| `chooser.evaluate` | `path`, `context?`, `seed?`, `trace?` (`none`, `brief`, `full`), `all?` | `{ result, row, used_catch_all, outputs, context_outputs, draw?, trace? }` or `{ results }` | read |
| `chooser.evaluate_binding` | `entity`, `trace?` | `{ result, row, used_catch_all, outputs, context, trace? }` (`context` is the inputs the binding's provider built; `draw` and `context_outputs` are only on `chooser.evaluate`) | play |
| `chooser.trace` | `site?`, `limit?` (default 32) | `{ enabled, traces: [{ site, frame, outcome, trace }] }` | read |
| `chooser.export` | `path`, `write?` | `{ text }`, plus `{ file }` with `write` | assets |

- `cooked` in `chooser.list` says the table came from the pak rather than source text. A source file wins when one is
  readable, so a dev run shows `false` and a packaged game shows `true`. A table with errors fails `regia cook`,
  including a nested reference that does not resolve (`chooser_ref_unresolved`) or a cycle (`chooser_cycle`).
- `chooser.evaluate` has no side effects: a fresh `Rng::new(seed)` (seed 0 by default), no recent-pick memory,
  no components touched, so it can be called any number of times with the same answer.
- `chooser.evaluate_binding` evaluates an entity's `ChooserBinding` with the context its registered provider builds
  from the live components, advances that binding's own random stream and records its recent pick; it does not
  raise `ChooserPicked`.
- `chooser.trace` reads the `ChooserTraceLog` ring (the last 32 traces per binding site). It is off by default;
  while it is on, bindings evaluate traced (`brief`).
- `chooser.set_cell` requires `value`. Omitting it is `bad_value` and the cell is unchanged. JSON `null` is `Any` and clears the cell.
- Edits change memory only and are refused (table unchanged) when they would leave a structural error;
  `chooser.export { write: true }` saves the canonical RON (atomically).

### JSON forms

- Values: numbers, booleans, strings (enum variants, names, asset paths), tags as arrays of names.
- Cells: `"Any"` (or `null`); `true`/`false` for a `Bool` column; `[min, max]` for a range; a number for a
  distance target or weight; a string for a name; `{ "is": v }`, `{ "is_not": v }`, `{ "one_of": [..] }`,
  `{ "kind": k }`; `{ "query": "<tag query RON>" }`; or any cell as `{ "ron": "<RON text>" }`.
- Results: `"nothing"`, `{ "asset": path }`, `{ "chooser": path }`, `{ "value": v }`, `{ "ron": "<RON>" }`.
- Column kinds and the table's result type are RON text: `"FloatDistance(max_distance: 10.0)"`, `"Asset(\"clip\")"`.
- A trace: `{ table, mode, inputs, rows: [{ id, enabled, cells: [{ column, verdict, cost, detail }], outcome }],
  draw, used_catch_all, selected, outputs, nested, issues }` with verdicts `pass`, `fail`, `score`, `skipped` and
  outcomes `disabled`, `rejected_by` (with `column`), `candidate` (with `cost`, `weight`), `not_evaluated`,
  `nested_empty`, `selected`. `full` tests every column of a rejected row; `brief` stops at the first failure.

## Errors

`F0002` (bad params) or `F0003` (not found) with `data.reason`; a refused edit also carries `data.issues`.

| Reason | When |
|---|---|
| `chooser_not_found` | no chooser at `path` (`F0003`) |
| `row_not_found` | no such row (`F0003`) |
| `column_not_found` | no such column (`F0003`) |
| `binding_not_found` | the entity has no `ChooserBinding` (`F0003`) |
| `provider_not_found` | the binding's provider is not registered (`F0003`) |
| `bad_value` | a missing or mistyped parameter, or a cell that is not of its column's kind |
| `bad_context` | a context value of the wrong kind |
| `unknown_input` | a context names an input the table does not declare |
| `read_only_asset` | `chooser.export { write: true }` without a source file |
| `io_failed` | the file could not be written (`F0001`) |
| `validation_failed` | an edit was refused without a more specific code |

Validator codes (reasons of refused edits; `chooser.validate`, `last_issues`):

| Code | Severity | Meaning |
|---|---|---|
| `chooser_too_many_rows` | error | more than 4,096 rows |
| `chooser_too_many_columns` | error | more than 64 columns |
| `chooser_too_many_params` | error | more than 64 inputs or outputs |
| `chooser_duplicate_row` / `chooser_duplicate_column` | error | a repeated id |
| `chooser_unknown_input` | error | a column (or catch-all output) names an undeclared input or output |
| `chooser_kind_mismatch` | error | a column's kind does not fit its input or output, or a second `Weight` column |
| `chooser_bad_cell` | error | a cell of the wrong kind, an undeclared enum variant, a cell for a missing column |
| `chooser_bad_range` | error | `min > max` without `wrap` |
| `chooser_bad_number` | error | `max_distance <= 0`, negative `influence` or weight, NaN anywhere (a cell, a `Value(..)` result, a catch-all output, `near_best`), a bad `wrap` or `recency` (window `1..=16`, penalty `0..=1`) |
| `chooser_ref_unresolved` | error | a `Chooser(path)` that is not loaded |
| `chooser_cycle` | error | a table reaches itself through nested choosers |
| `chooser_depth_exceeded` | error | nesting deeper than 8 |
| `chooser_result_type_mismatch` | error | an entry that does not agree with the result type |
| `chooser_unreachable_row` | warning | in `first` mode, an earlier row always passes |
| `chooser_no_catch_all` | info | no catch-all and no row that always passes |
| `chooser_bad_file` | error | RON syntax, an unknown field |
| `chooser_unsupported_version` | error | `format_version` newer than 1, or 0 |

## Checks agents run

1. `chooser.evaluate { path, context, trace: "full" }` and `play.expect` on `row`.
2. Why not that row: read `trace.rows[i].cells` (every verdict with a short detail such as
   `input 4 not in [5, 100]`).
3. Edit a cell with `chooser.set_cell`, evaluate again, then `chooser.export { write: true }`.
4. For a live entity: `chooser.evaluate_binding { entity, trace: "brief" }`; to watch picks over time, turn the
   trace log on and read `chooser.trace { site }`.

With `ChooserBridgePlugin`, a `montage` pick plays on the entity's `MontagePlayer` (the montage named by the
result's path, else its file stem, with the `rate`, `section` and `start_time` outputs) and a `cue` pick plays the
registered sound cue through `Audio::play_cue`; `ChooserStats.montage_plays` and `cue_plays` count them. `clip`
picks are not bridged.

Bindings raise `ChooserPicked` messages when the picked result changes, and also after a reload or edit of the
table that changes the same row's outputs (a new `rate` reaches the montage bridge); `ChooserStats` (`evaluations`,
`picks`, `skipped`, `unresolved`) counts them for `wait_until`-style checks.

The file format is `docs/formats/chooser-ron.md`.
