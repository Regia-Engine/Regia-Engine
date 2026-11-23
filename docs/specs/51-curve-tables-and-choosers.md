# Spec 51: Curve tables and choosers

Status: proposed (ADR-1579). Target rung: v0.3.x (b). Gap: G12. Crates: `regia_framework` (modules
`curve_table`, `chooser`), `regia_editor` (one panel). Extends `specs/content-data.md` § 3 (data tables) and
`specs/14` (gameplay framework). Agent surface: `specs/23`.

Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: data tables, tags, `regia_core::math::{curve, rng}` (§ 1.1); no curve-table or chooser code exists.

Source area: runtime-gameplay/runtime-gameplay-choosers, editor-asset-editors/editor-asset-editors-data
(ids only; no text carried over).

## 1. Purpose and scope

Two pieces of gameplay data plumbing that data tables (baseline) do not cover:

1. **Curve tables**: named float curves kept in one file, sampled by name at runtime (damage falloff over
   distance, XP per level, difficulty ramps), edited as text, as CSV from a spreadsheet, by an agent, or in an
   editor panel.
2. **Choosers**: an ordered table of rows. Each row has condition cells (one per column) and a result. Given a
   **context** (a bag of named input values), evaluation returns the result of the row that matches, with a
   recorded explanation of every decision. Used to choose a clip, a montage, an audio cue, a loot entry, a
   dialogue line, or any value, without writing an `if` ladder.

Not in scope: the animation/audio/dialogue assets themselves (they stay in their own crates); a visual graph
language; random loot "tables with economy rules" (a game builds those on the `Weighted` mode here); gameplay
tag definitions (baseline `regia_framework::tags`, reused as is).

### 1.1 Baseline (already in the repo) and what this spec adds

| Piece | Baseline | New here |
|---|---|---|
| Typed row tables, `.table.ron` / `.table.csv`, validator, `DataTables` resource, `tables.*` agent methods | `regia_asset::data_table`, `book/agents/data-tables.md` | none; reused for issue types, CSV parser/writer, migrator loop |
| Hierarchical tags and queries | `regia_framework::tags` (`Tag`, `TagContainer`, `TagQuery`) | reused as a column type |
| Keyed float curve evaluator (`Curve<T>`, `Interp`: step, linear, cubic Hermite) | `regia_core::math::curve` | reused as the sampling engine |
| Seedable RNG | `regia_core::math::Rng` (PCG32) | reused for weighted picks |
| Curve table asset, loader, CSV/RON forms, `CurveTables` resource, `curves.*` methods, panel | missing | new |
| Chooser asset, evaluator, trace, bridges, `chooser.*` methods, panel | missing | new |

## 2. Regia terms and model

### 2.1 Curve tables

- **Curve table**: an asset (`.curves.ron`, `.curves.csv`) holding up to 4,096 **curves**, each named by a unique
  string (`[A-Za-z0-9_.-]{1,64}`). Order is file order.
- **Curve**: a list of **keys** `(x, y)` with `x` finite and strictly increasing, `y` finite, 1 to 4,096 keys.
  Backed by `regia_core::math::Curve<f32>`; the table adds naming, per-curve settings and file forms.
- **Interpolation** (table default, overridable per curve and per key in RON): `Step` (hold the left key),
  `Linear`, `Cubic`. In the table layer `Cubic` means cubic Hermite with tangents derived at load by finite
  differences over the (possibly non-uniform) neighbouring keys, using the Fritsch-Carlson limiter so a monotone
  run of keys stays monotone (no overshoot); end keys use the one-sided difference. A key may instead carry
  explicit `in_tangent` / `out_tangent` (RON only), which win over derived ones.
- **Extrapolation** before the first and after the last key, set per side per curve:

| Mode | Behavior for x outside `[x_first, x_last]` |
|---|---|
| `Hold` (default) | the end key's value |
| `Linear` | continue the end segment's slope (end tangent for `Cubic`) |
| `Cycle` | repeat the keyed span (`x` wrapped into the span), values absolute |
| `CycleOffset` | repeat the span, each loop offset by `y_last - y_first` times the loop count |
| `PingPong` | repeat the span mirrored on alternate loops |

- A one-key curve is a constant for every `x`. A zero-key curve is rejected on load.
- **Sampling** is a pure function `sample(curve, x) -> f32`. Non-finite `x` returns the curve's first key value
  and counts a `curve_sample_nonfinite` warning once per curve per second of frame time (tracing, not an error).
  Keys in a segment are found by binary search; a `CurveHandle` (index + generation) avoids the name lookup.
- **Cross-references**: a data-table row field or a chooser cell may hold a `CurveRef { table: AssetRef<CurveTable>,
  name: String }`; the validator reports a missing table or curve (`curve_ref_unresolved`).

### 2.2 Choosers

Nouns:

- **Chooser table** (asset `.chooser.ron`): schema + columns + rows + catch_all + mode + result type.
- **Schema**: the declared **inputs** the table may read (`name`, `kind`) and the declared **outputs** it may
  write. Kinds: `Bool`, `Int`, `Float`, `Enum(variants)`, `Name` (string), `Tags` (a `TagContainer`),
  `Asset` (an asset path/id). A column binds to one input name and must match its kind.
- **Context** (`ChooserContext`): the per-evaluation value map for inputs, plus an output map filled by output
  columns. Inputs are read only; outputs are written only by evaluation and are never visible to columns of the
  same evaluation. A missing input counts as **absent**: a gate column on an absent input fails its row unless
  the cell is `Any`.
- **Column**: one condition or action applied to every row. Cell values are per row. Column kinds:

| Class | Kind | Cell | Effect |
|---|---|---|---|
| gate | `Bool` | `Any`, `True`, `False` | row passes if cell is `Any` or equals the input |
| gate | `IntRange` / `FloatRange` | `Any` or `(min, max)` inclusive; column may set `wrap: (lo, hi)` for circular values (angles) | passes if input in range; with `wrap`, ranges may run past `hi` back to `lo` |
| gate | `Enum` | `Any`, `Is(v)`, `IsNot(v)`, `OneOf([v..])` | variant comparison |
| gate | `Name` | `Any` or string; column option `case_sensitive` (default true) | equality |
| gate | `Tags` | `Any` or a `TagQuery` (baseline RON form) | passes if the input container satisfies the query |
| gate | `Asset` | `Any`, `Is(path)`, `Kind(kind)` | exact asset or asset-kind equality |
| rank | `FloatDistance` | `Any` (cost 0) or a target `f32`; column sets `influence` (default 1), `max_distance` (> 0), `wrap`, `drop_beyond: bool` | adds cost, see § 2.4; with `drop_beyond` also filters |
| draw | `Weight` | `f32 >= 0` (default 1); at most one per table | weight in `Weighted` mode |
| emit | `Set(kind)` | a value of that kind | writes the declared output name when the row is the result |

- **Row**: `id` (unique name, stable across edits, the unit tests and traces refer to), `enabled` (default
  true), `cells` (one per column, by column id; a missing cell means `Any` / default), `result`, and a
  `note` string for authors. Order is evaluation order.
- **Result entry** (per row and for the catch-all):

| Entry | Meaning |
|---|---|
| `Asset(path)` | an asset reference; resolved or loaded by the caller, never by the evaluator |
| `Value(v)` | an inline value (bool, int, float, name, tags) |
| `Chooser(path)` | a nested chooser asset, evaluated only if this row is the selected one |
| `Inline(table)` | a nested table stored in this file (same shape, own columns), same rule |
| `Nothing` | selected row yields no result (output-only effects) |

- **Result type** (table-level): `Asset(kind)` where `kind` is a free tag such as `clip`, `montage`, `cue`, `prefab`,
  `any`; `Value(kind)`; or `OutputOnly`. Entries must agree with it (`chooser_result_type_mismatch`).
- **CatchAll**: an optional result entry (and optional output values) used when no row is selected. With no
  catch_all and no selected row the evaluation returns `None`.
- **Modes** (table-level `mode`):

| Mode | Selection among rows that pass every gate ("candidates") |
|---|---|
| `First` (default) | the first candidate in row order |
| `Best` | the candidate with the lowest total cost; ties go to the earlier row |
| `Weighted` | a random draw proportional to weights, optionally among the best-cost band (§ 2.4) |
| `All` | every candidate in row order; returns a list (multi-result) |

### 2.3 Evaluation order (precise)

For one table and one context:

1. Skip columns with `disabled: true` (treated as `Any` for every row) and rows with `enabled: false`.
2. For each remaining row in order, for each gate column in column order, test the cell; the first failing
   column rejects the row (recorded as `RejectedBy(column)`; later columns are not tested for that row unless
   tracing asks for full detail, see § 2.6). Rank columns then add to the row's cost; a `drop_beyond` column
   that exceeds its `max_distance` rejects the row at that point.
3. The survivors are the candidates. If `mode` is `First`, stop scanning at the first candidate (later rows are
   recorded `NotEvaluated`).
4. Select by mode (§ 2.2). If there are no candidates, go to the catch-all step.
5. Resolve the selected row's result. If it is a nested chooser and that chooser returns `None`, the row is
   struck from the candidate list (recorded `NestedEmpty`) and selection repeats from step 4 over the remaining
   candidates. Nested depth is limited to 8 (`Chooser` / `Inline` results). Deeper is `chooser_depth_exceeded`:
   evaluation returns the top table's catch-all. A plain catch-all (`Asset`, `Value`, `Nothing`) is returned as
   stored. A `Chooser` or `Inline` catch-all is evaluated once, entered one level down (the same budget
   `evaluate_all` uses for that expansion). If that evaluation itself exceeds depth, `evaluate` returns
   `Nothing` and `evaluate_all` returns an empty list; rows reached before the overflow are dropped. An
   unresolved `Chooser` catch-all returns nothing (`None` from `evaluate`, an empty list from `evaluate_all`).
6. Write output columns of the selected row into the context outputs. If the catch-all answers, write the
   catch_all's output values instead.
7. Return `ChooserOutcome { result, row: Option<RowId>, used_catch_all, outputs }`, or for `All` a `Vec` of them
   in row order (nested entries expanded in place, depth limited as above).

`Weighted` is the only mode that touches the RNG; every other mode is a pure function of table and context.

As built (ADR-5638): step 2 tests a row's columns in column order, so a `drop_beyond` rank column placed before a
failing gate is the column the trace names; selection is the same. A trace's `Candidate.weight` is the `Weight` cell
before the recency penalty.

### 2.4 Scoring and weighted draw

- Distance cost for one `FloatDistance` cell with input `a` and target `b`: `gap = |a - b|` (with `wrap = (lo, hi)`,
  `d = |a - b| mod (hi - lo)` and `gap = min(d, (hi - lo) - d)`); `cost = influence * min(gap, max_distance) /
  max_distance`, so each column contributes 0 to `influence`. Row cost is the sum over rank columns, accumulated in
  column order in `f32`.
- `Best`: lowest cost wins; costs within `1e-6` (absolute) count as equal and the earlier row wins.
- `Weighted`: weight of a candidate is its `Weight` cell (1 with no weight column). If the table sets
  `near_best: Some(t)` (`t >= 0`, negative values clamp to 0), only candidates with `cost <= best_cost + t` take part.
  Weights are first scaled by the recency penalty (below). Let `total` be the sum of weights; when
  `total > 0` take exactly one `u = rng.f32()` in `[0, 1)` and pick the first candidate, in row order,
  whose running sum exceeds `u * total`. When `total == 0` (all weights zero), the first
  participating candidate wins and no draw is consumed.
- **Recent picks**: an optional value `RecentPicks { rows: VecDeque<String> }` that the caller owns (per character
  or per context, never global) and passes in. The table sets `recency: Option<Recency { window: u32, penalty: f32 }>`
  with `window` in `1..=16` and `penalty` in `[0, 1]` (default 0). A candidate whose row id is among the last `window`
  entries of the memory has its weight multiplied by `penalty`, so 0 excludes it and 0.25 makes a repeat a quarter
  as likely. If every participant ends with weight 0 because of the penalty alone, the penalty is ignored for that
  draw. The memory records the selected row after each `Weighted` evaluation that was given one, and never for traced
  "what if" evaluations or `evaluate_all`.
- Nested choosers and the RNG: each nested evaluation shares the caller's RNG, in depth-first order.

### 2.5 Limits and edge cases

| Item | Limit / rule | Code on violation |
|---|---|---|
| Rows per table | 4,096 | `chooser_too_many_rows` |
| Columns per table | 64 | `chooser_too_many_columns` |
| Inputs / outputs | 64 each | `chooser_too_many_params` |
| Nested depth | 8 (cycle: a table that reaches itself) | `chooser_depth_exceeded`, `chooser_cycle` |
| Duplicate row id / column id | error | `chooser_duplicate_row`, `chooser_duplicate_column` |
| Column bound to undeclared input, or kind mismatch | error | `chooser_unknown_input`, `chooser_kind_mismatch` |
| Cell of the wrong kind for its column | error | `chooser_bad_cell` |
| Range with `min > max` and no `wrap` | error | `chooser_bad_range` |
| `max_distance <= 0`, negative `influence` or weight, NaN anywhere | error | `chooser_bad_number` |
| Nested path not found / not a chooser | error | `chooser_ref_unresolved` |
| A row that can never be selected (all earlier rows are `Any` in every gate, in `First` mode) | warning | `chooser_unreachable_row` |
| No catch_all and the table has no catch-all row | info | `chooser_no_catch_all` |
| Result entry disagrees with the result type | error | `chooser_result_type_mismatch` |

Float comparisons are exact (`min <= v <= max`); NaN inputs fail every gate except `Any`. `Int` inputs given
to `FloatRange` columns are not converted: the kinds must match.

### 2.6 Trace

`ChooserTrace` is produced only when the caller asks (`evaluate_traced`); the plain call allocates nothing for
it. Contents:

- table path, mode, context snapshot (the inputs actually read, with values);
- per row, in order: `id`, `enabled`, per-column cell verdict `{ column, verdict: Pass | Fail | Score(cost) |
  Skipped, detail }` (`detail` is a short string such as `input 0.4 not in [0.5, 1.0]`), and a row outcome:
  `Disabled`, `RejectedBy(column)`, `Candidate { cost, weight }`, `NotEvaluated`, `NestedEmpty`, `Selected`;
- `draw: Option<f32>` (the `u` used), `used_catch_all`, the output writes, and `nested: Vec<ChooserTrace>`.

`detail = Full` traces every column of a rejected row (for the editor and agent "why not this row"); `Brief`
stops at the first failure. A ring of the last 32 traces per evaluation site (`site` string: a binding name or
caller label) is kept in the `ChooserTraceLog` resource when the log is enabled (off by default in shipping).

### 2.7 Integration without upward dependencies

The evaluator (`chooser::core`) works on `ChooserContext` and returns data. It names no animation, audio or UI
type. Producers of context and consumers of results are **bridges**, thin systems in `regia_framework`
(layer 3), which already sits above `regia_animation` and `regia_audio` (layer 2); neither of those crates ever
learns that choosers exist, and no new crate edge is created. Rules:

- A **binding** is a component `ChooserBinding { table: AssetRef<ChooserTable>, provider: String, site: String,
  reeval: Reeval, seed: u64 }` with `Reeval = OnChange | EveryTick | Manual`. `provider` names a function
  registered with `app.register_chooser_provider("locomotion", fn(&World, Entity, &mut ChooserContext))`, the
  one place game code fills inputs from components. `OnChange` re-evaluates when the filled context differs
  from the last one (value equality, float bits compared exactly).
- Swapping the table (asset reference) on a binding is the way to switch a whole set at once (unarmed versus
  rifle locomotion): the next evaluation uses the new table; an in-flight result is not interrupted.
- A binding raises `ChooserPicked { entity, site, row, result, used_catch_all }` (a message) when the result
  differs from the previous one, or when the table changed (reload, edit, swap) and the same row's outputs differ
  (ADR-1809), and always stores the last pick in the component.
- **Animation bridge** (`chooser::bridge::montage_bridge`): a `montage` pick calls
  `MontagePlayer::play_named` with the montage named by the result path, else its file stem, and the
  outputs `rate` (float), `section` (name or enum) and `start_time` (float) as `PlayOptions`. An entity
  without a `MontagePlayer` is left alone. `clip` picks are not bridged (ADR-1585: no named-slot entry
  point). Reading graph variables into the context is the provider's job.
- **Audio bridge** (`chooser::bridge::cue_bridge`): a `cue` pick calls `Audio::play_cue` with the result
  path, else the file stem. The cue's own variation, volume and bus are unchanged. Position, `volume_db`
  and `bus` outputs are not applied (ADR-1649: `Audio` has no entry point for them). A chooser narrows
  which cue; the cue still picks its sample.
- **Loot and dialogue**: no bridge. Game code calls `ChooserTable::evaluate(&ctx, &mut rng, ...)` (or the
  binding with `Manual`) and reads `Value(Name)` (a loot table name, a localization key per `content-data.md`
  § 1.1) or `Asset` results. Weighted loot uses `Weighted` with a `Weight` column and `recency`.

## 3. API shape in Rust

Crate `regia_framework`, modules `curve_table` and `chooser`; plugins `CurveTablePlugin`, `ChooserPlugin`
(both in `FrameworkPlugins`, both optional; absent plugin means the agent methods degrade to
`{ available: false }`). Public items documented; no `unwrap` on content paths.

```rust
// curve_table
#[derive(Reflect, Serialize, Deserialize, Clone, Debug)]
pub struct CurveTable { pub interp: TableInterp, pub curves: Vec<NamedCurve> }
pub struct NamedCurve { pub name: String, pub pre: Extrap, pub post: Extrap, pub interp: Option<TableInterp>,
                        pub keys: Vec<CurveKey> }
pub struct CurveKey { pub x: f32, pub y: f32, pub interp: Option<TableInterp>,
                      pub in_tangent: Option<f32>, pub out_tangent: Option<f32> }
pub enum TableInterp { Step, Linear, Cubic }
pub enum Extrap { Hold, Linear, Cycle, CycleOffset, PingPong }
impl CurveTable {
    pub fn from_ron_str(text: &str) -> Result<Self, CurveTableError>;
    pub fn from_csv_str(text: &str, interp: TableInterp) -> Result<Self, CurveTableError>;
    pub fn to_ron_string(&self) -> String;
    pub fn to_csv_string(&self) -> Result<String, CurveTableError>;   // fails for non-shared x axes: curve_csv_ragged
    pub fn validate(&self) -> Vec<TableIssue>;                        // regia_asset::data_table::TableIssue
    pub fn handle(&self, name: &str) -> Option<CurveHandle>;
    pub fn sample(&self, h: CurveHandle, x: f32) -> f32;
    pub fn sample_named(&self, name: &str, x: f32) -> Option<f32>;
    pub fn range(&self, h: CurveHandle) -> (f32, f32, f32, f32);      // x_min, x_max, y_min, y_max over the keys
}
#[derive(Resource, Default)] pub struct CurveTables { /* path -> loaded table, generation, last issues */ }
impl CurveTables { pub fn get(&self, path: &str) -> Option<&CurveTable>; pub fn generation(&self, path: &str) -> u64; }
// app.register_curve_table("tables/falloff.curves.ron"); hot-reloads like data tables (a reload is applied only if it validates)

// chooser
pub enum ChooserValue { Bool(bool), Int(i64), Float(f32), Enum(String), Name(String), Tags(TagContainer), Asset(String) }
#[derive(Default, Clone)] pub struct ChooserContext { /* inputs: BTreeMap<String, ChooserValue>, outputs: same */ }
impl ChooserContext { pub fn set(&mut self, name: &str, v: ChooserValue); pub fn input(&self, name: &str) -> Option<&ChooserValue>;
                      pub fn output(&self, name: &str) -> Option<&ChooserValue>; }
pub struct ChooserTable { pub schema: Schema, pub mode: Mode, pub result_type: ResultType, pub columns: Vec<Column>,
                          pub rows: Vec<Row>, pub catch_all: Option<CatchAll>, pub near_best: Option<f32>, pub recency: Option<Recency> }
pub struct ChooserOutcome { pub result: ResultEntry, pub row: Option<String>, pub used_catch_all: bool }
impl ChooserTable {
    pub fn from_ron_str(text: &str) -> Result<Self, ChooserError>;
    pub fn to_ron_string(&self) -> String;
    pub fn validate(&self, env: &ChooserEnv) -> Vec<TableIssue>;      // env resolves nested paths and CurveRefs
    pub fn evaluate(&self, env: &ChooserEnv, ctx: &mut ChooserContext, rng: &mut Rng,
                    mem: Option<&mut RecentPicks>) -> Option<ChooserOutcome>;
    pub fn evaluate_all(&self, env: &ChooserEnv, ctx: &mut ChooserContext) -> Vec<ChooserOutcome>;
    pub fn evaluate_traced(&self, env: &ChooserEnv, ctx: &mut ChooserContext, rng: &mut Rng,
                    mem: Option<&mut RecentPicks>, detail: TraceDetail) -> (Option<ChooserOutcome>, ChooserTrace);
}
#[derive(Resource, Default)] pub struct Choosers { /* path -> table, generation, last issues; implements the env lookup */ }
#[derive(Resource, Default)] pub struct ChooserTraceLog { /* ring per site */ }
#[derive(Component, Reflect)] pub struct ChooserBinding { /* § 2.7 */ }
#[derive(Message)] pub struct ChooserPicked { /* § 2.7 */ }
```

- `ChooserTable` and `CurveTable` derive `Reflect` (editor inspection, hot reload) plus serde; `ChooserValue`
  is not generic, so the editor and agent need no type registry for cells. As built (ADR-1649, ADR-5638): neither
  derives `Reflect`; `ChooserTable` has no serde derive (its RON writer and reader and the cooked form are the file
  forms). There is no `FrameworkPlugins` group: `CurveTablePlugin` and `ChooserPlugin` are added one by one.
- Layer check: `curve_table` depends on `regia_core`, `regia_asset`; `chooser` additionally on `tags` and (bridges
  only) `regia_animation`, `regia_audio`. The editor panel (`regia_editor`, layer 6) depends on `regia_framework`;
  nothing in layers 0-4 depends on the editor. `regia_agent` already depends on `regia_framework`. No new edge.
- Systems run in `FixedUpdate` (binding evaluation, once per fixed step, `ChooserSet::Evaluate`; ADR-4756) and
  `PostUpdate` (the montage and cue bridges, before the animation and audio
  update sets, so a pick takes effect the same frame). `ChooserPicked` messages written by a fixed step are
  read by the bridges later in the same frame.

## 4. Files and formats

### 4.1 Curve table: `.curves.ron` (format `curve_table` v1)

```ron
(
    format_version: 1,
    interp: Linear,                         // table default: Step | Linear | Cubic
    curves: {
        "falloff": (pre: Hold, post: Hold, keys: [(0.0, 1.0), (10.0, 0.6), (30.0, 0.0)]),
        "xp": (interp: Cubic, post: Linear, keys: [(1, 0), (2, 100), (3, 250, out_tangent: 160.0)]),
    },
)
```

A key is `(x, y)` or `(x, y, interp: .., in_tangent: .., out_tangent: ..)`. `curves` keeps file order.

### 4.2 Curve table: `.curves.csv` (RFC 4180, parser shared with data tables)

First column header `name`; every other header is the `x` position (a finite number, strictly increasing left to
right). Each row is one curve, cells are `y` values. An empty cell means no key at that `x` for that curve, so a
curve may use a subset of the shared axis. The import dialog or `curves.import` takes `interp` (`step`, `linear`,
`cubic`). CSV carries neither per-curve extrapolation nor tangents; those stay at their defaults. A curve whose
keys are not representable on one shared axis (different `x` sets per curve) is exported by `to_csv_string` as
the union axis with blanks, which round-trips; only non-finite or repeated `x` is `curve_csv_ragged`.

### 4.3 Chooser: `.chooser.ron` (format `chooser` v1)

```ron
(
    format_version: 1,
    mode: Best,
    result: Asset("montage"),
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

Cell and column RON spellings follow the tables in § 2.2. `Inline(...)` results hold a full nested table body.
Comments are allowed. Row order is evaluation order and is preserved by every writer.

### 4.4 Versioning and cook

Both formats start at `format_version: 1`. A future v2 adds a migration step to the shared
`regia_core::version` loop (as `DATA_TABLE_MIGRATIONS` does); the loader runs all steps before deserializing;
`regia upgrade-project` rewrites files at the current version; a file with a newer version is refused
(`curve_unsupported_version`, `chooser_unsupported_version`). A v0 does not exist for either. Per `specs/12`
§ 4a no frozen format changes without a migrator. Cook: both are postcard-encoded into the pak after validation
(a table with errors fails the cook; warnings do not), keeping the type path and version header; the asset
registry records `type_path` and tags `kind=curve_table` / `kind=chooser`, `result=<kind>` for choosers, and
dependencies for every `Asset(..)`, `Chooser(..)` and `CurveRef`.

As built (ADR-2012): the cooked payload is a little-endian `u32` version plus postcard, versioned separately from the RON
format (`curves-cooked`, `chooser-cooked`); it is a cache, so a pak of another version is cooked again. A load re-validates and
has no recursion a crafted payload could drive. Source text wins over a cooked entry at run time, so a dev tree runs what is on
disk, and `regia package` ships curve tables and choosers in the pak only. A dependency is recorded only for an asset a pak holds.

## 5. Agent-protocol surface

Guide pages: `book/agents/curve-tables.md` and `book/agents/choosers.md`. Cells and values cross as JSON
(numbers, booleans, strings; tags as arrays of names; enums as the variant string); RON text is also accepted
for a whole row or key. Access classes follow `specs/23`: `read`, `play` (changes running game state), `edit_assets`.

### 5.1 Curve tables

| Method | Params | Result | Class |
|---|---|---|---|
| `curves.list` | none | `{ tables: [{ path, interp, curves, generation, errors, warnings, file }] }` | read |
| `curves.describe` | `{ path }` | list entry + `curves: [{ name, keys, pre, post, interp, x_range, y_range }]`, `last_issues` | read |
| `curves.get` | `{ path, name }` | `{ keys: [{ x, y, interp, in_tangent, out_tangent }], pre, post, interp }` | read |
| `curves.sample` | `{ path, name, x }` or `{ path, name, from, to, count }` (count <= 4,096) | `{ y }` or `{ points: [[x, y]...] }` | read |
| `curves.set_key` | `{ path, name, x, y, interp?, in_tangent?, out_tangent? }` (insert or replace the key at `x`) | `{ generation, issues }` | edit_assets |
| `curves.remove_key` | `{ path, name, x }` | `{ generation }` | edit_assets |
| `curves.set_curve` | `{ path, name, keys?, pre?, post?, interp?, rename? }` (creates the curve if absent) | `{ generation, issues }` | edit_assets |
| `curves.delete` | `{ path, name }` | `{ generation }` | edit_assets |
| `curves.validate` | `{ path }` | `{ issues: [{ code, severity, message, curve, line }] }` | read |
| `curves.export` | `{ path, format: ron\|csv, write?: bool }` | `{ text }` and, with `write`, `{ file }` | edit_assets when `write`, else read |
| `curves.import` | `{ path, csv, interp }` (CSV text) | `{ curves, generation, issues }` | edit_assets |

Edits touch the in-memory table, bump `generation`, and are saved only by `curves.export { write: true }`
(same model as `tables.set`). An edit that would make the table invalid (duplicate `x`, non-finite) is refused
with the error and changes nothing.

### 5.2 Choosers

| Method | Params | Result | Class |
|---|---|---|---|
| `chooser.list` | none | `{ tables: [{ path, mode, result, rows, columns, generation, errors, warnings }] }` | read |
| `chooser.describe` | `{ path }` | list entry + `inputs`, `outputs`, `columns`, `row_ids`, `last_issues` | read |
| `chooser.get` | `{ path, row? }` | whole table as JSON, or one row | read |
| `chooser.set_cell` | `{ path, row, column, value }` | `{ generation, issues }` | edit_assets |
| `chooser.set_row` | `{ path, row, enabled?, result?, cells?, before? }` (create when absent, `before` positions it) | `{ generation, issues }` | edit_assets |
| `chooser.remove_row` | `{ path, row }` | `{ generation }` | edit_assets |
| `chooser.set_column` / `chooser.remove_column` | column definition / `{ path, column }` | `{ generation, issues }` | edit_assets |
| `chooser.set_options` | `{ path, mode?, result?, catch_all?, near_best?, recency? }` | `{ generation, issues }` | edit_assets |
| `chooser.validate` | `{ path }` | `{ issues: [{ code, severity, message, row, column, line }] }` | read |
| `chooser.evaluate` | `{ path, context: { name: value }, seed?: u64, trace?: none\|brief\|full, all?: bool }` | `{ result, row, used_catch_all, outputs, draw?, trace? }` | read |
| `chooser.evaluate_binding` | `{ entity, trace? }` (the live context the provider would build) | same, plus `context` | play |
| `chooser.trace` | `{ site?, limit? }` | `{ traces: [{ site, frame, outcome, trace }] }` from `ChooserTraceLog` | read |
| `chooser.export` | `{ path, write?: bool }` | `{ text, file? }` | edit_assets when `write` |

`chooser.evaluate` is side-effect free: it uses a fresh `Rng::new(seed)` (default 0), no `RecentPicks`, and never
touches components, so a test can call it any number of times. `evaluate_binding` evaluates with the entity's own
RNG state advanced; it is the only variant that records recent picks.

Error codes (lowercase ascii/digits/underscore): `chooser_not_found`, `curve_table_not_found`, `curve_not_found`,
`row_not_found`, `column_not_found`, `bad_value`, `bad_context`, `unknown_input`, `plugin_missing`,
`validation_failed`, `read_only_asset`, plus every validator code of § 2.5 and `curve_*` codes of § 8.
As built (ADR-5638): `plugin_missing` is never sent; without the plugin every method answers `{ available: false }`.

### 5.3 Coverage rows (`book/COVERAGE.md`)

- `curves.* (11 methods)` : `agents/curve-tables.md` : "Named float curve tables: list, sample, edit keys, import/export CSV."
- `chooser.* (13 methods)` : `agents/choosers.md` : "Context-driven result tables: edit rows, evaluate with a context, read the trace."
- File rows: `.curves.ron` (v1), `.curves.csv`; `.chooser.ron` (v1), with `docs/formats/curve-table-ron.md` and
  `docs/formats/chooser-ron.md`.

Verification an agent can do: `chooser.evaluate` with a context and `trace: full`; `expect` on `row`; edit a cell
then re-evaluate to see the change; `screenshot` of the panel (§ 6) showing the selected row highlighted;
`wait_until` on `ChooserPicked` counter in `debug.stats` after driving a binding.

## 6. Editor surface

`regia_editor` gets one dockable **Tables** panel extension (desktop only), two tabs:

- **Curves**: a list of curves of the open table, a graph (x horizontal, y vertical; pan/zoom; drag keys and
  tangent handles; click a segment to add a key; delete key; per-curve interpolation and extrapolation drop-downs),
  and a grid view of the same data (x row, one y cell per column; CSV paste). Multiple curves overlay in the graph
  with per-curve colors that persist in the editor layout file, not the asset. All edits go through the undo
  stack by calling the same `set_key` / `set_curve` operations as the agent.
- **Chooser**: a grid with one row per chooser row and one column per chooser column (headers show input name and
  kind; row result in the last column). Cells edit inline with a type-appropriate widget. A **test context**
  strip at the top lets the author enter input values; the grid then colors each cell Pass / Fail / Score and
  highlights the selected row, driven by `evaluate_traced` with `Full` detail (the "debug test" mode). Drag to
  reorder rows, toggle row and column disabled. Validation issues appear as badges on the offending cell.

Both tabs are thin over the library calls; anything they can do, § 5 can do.

## 7. Determinism notes

- Curve sampling is a pure `f32` function; derived cubic tangents are computed once at load in a fixed
  left-to-right order. Results are bit-exact for the same inputs on one platform and within 1e-6 relative across
  platforms (no fused multiply-add reliance; evaluation uses plain `f32` operations in a fixed order).
- Chooser evaluation in `First`, `Best`, `All` is bit-exact and platform-independent for the same table and context
  (float comparisons only; costs summed in column order).
- `Weighted` is deterministic given the RNG state: one `Rng::f32` per evaluation that reaches the draw with a
  positive total weight, taken in depth-first order across nested tables. Gameplay code should give each
  binding its own `Rng` stream (`Rng::with_stream(seed, stream)`), seeded from the session seed, so evaluation
  order between entities does not change picks. The stream id is the bound entity's `EntityGuid` (folded to 64
  bits) when it has one, else its entity bits (spawn-order dependent); the `Rng` state is saved with the
  binding, so a seek continues the stream (ADR-4756).
- Bindings evaluate in entity-id order inside `ChooserSet::Evaluate`. Replays that record inputs reproduce picks
  exactly because contexts are value-equal and the RNG state is part of the saved binding state.
- Tolerance-based: only the cross-platform `f32` curve results above.

## 8. Tests and acceptance

Curve-table codes: `curve_duplicate_name`, `curve_empty`, `curve_bad_number`, `curve_unsorted_x`,
`curve_duplicate_x`, `curve_too_many_keys`, `curve_ref_unresolved`, `curve_csv_ragged`, `curve_sample_nonfinite`.

Tests and acceptance, all CPU-only (no GPU) unless stated; no wall-clock assertions.

1. Sampling: step, linear and cubic at key positions return the key value exactly; midpoint of a linear segment is
   the mean; cubic through monotone keys never overshoots (property test over random monotone key sets);
   non-uniform key spacing is respected.
2. Extrapolation: each of the five modes on a three-key curve at `x` = first - 1, last + 1, last + 2.5 spans,
   with expected values stated in the test.
3. One-key curve is constant; zero keys, duplicate `x`, unsorted `x` (RON), non-finite values are rejected with
   the named codes.
4. CSV: header axis, blank cells, RFC 4180 quoting/BOM/CRLF cases (reusing the data-table conformance fixtures),
   ragged-axis export as union axis, import then export then import is identical. As built: the line-endings /
   BOM equivalence and the quoted-fields acceptance of the data-table conformance fixtures are pinned by
   `csv_line_endings_and_a_byte_order_mark_read_the_same_curves` and
   `quoted_fields_and_names_read_like_plain_ones`.
5. RON round trip including tangents and per-key interpolation; unknown newer `format_version` refused.
6. `CurveRef` in a data-table row resolves, and a missing curve reports `curve_ref_unresolved`. As built:
   `CurveRef` derives `Reflect` (a typed row can hold one, also behind an `Option`, in a nested
   struct, or inside any reflected container: list, array, tuple, tuple struct, map, any enum
   variant) and the validator pass `validate_curve_refs` walks a loaded `DataTable` by reflection,
   returning one `curve_ref_unresolved` issue per row whose reference does not resolve, naming the
   row key. It is a library call only: the agent's `tables.validate` does not run it (the erased
   tables it works on do not carry the typed row reflection); a game calls it on its typed tables
   after load/reload.
7. Chooser gates: one test per column kind (bool, int/float range incl. wrap across the seam, enum Is / IsNot /
   OneOf, name case option, tags with a `TagQuery`, asset kind), covering absent inputs and NaN.
8. Modes: `First` returns the earliest candidate and marks later rows `NotEvaluated`; `Best` picks the lowest cost
   and breaks ties to the earlier row; `All` returns candidates in row order; disabled rows and disabled columns
   behave as specified.
9. Score: wrap distance across `-180/180`, `max_distance` clamp, `influence` scaling, `drop_beyond` filtering.
10. Weighted: with `Rng::new(7)` the pick sequence over 1,000 evaluations is a fixed golden list stored in the
    test (data, not an image); empirical frequencies are within 3 sigma of the weights; `near_best` restricts
    participants; recency `(window: 1, penalty: 0.0)` never repeats the previous row while an alternative exists and does
    repeat when it is the only survivor; total weight 0 consumes no draw.
11. CatchAll: used exactly when no candidate remains, writes catch_all outputs; `None` without a catch-all.
12. Nested: referenced and embedded tables are evaluated only when their row is selected; a nested `None` strikes
    the row and re-selects; depth 9 returns the catch-all with `chooser_depth_exceeded` (a `Chooser` or `Inline`
    catch-all is evaluated once; if that evaluation itself exceeds depth, the result is `Nothing`); a cycle is
    reported by `validate`.
13. Output columns write the winning row's values to the context outputs and never alter inputs.
14. Validation: one fixture per code in § 2.5, each asserting code, row and column.
15. Trace: for a fixed table and context, `full` trace lists every row and column verdict, `selected` equals the
    returned row, `draw` is present only for `Weighted`, and a plain `evaluate` allocates no trace (counting
    allocator test in `regia_framework`).
16. Bindings: with a test provider, `OnChange` evaluates once for an unchanged context across 100 fixed steps;
    changing an input produces exactly one `ChooserPicked`; swapping the table switches the set.
17. Bridges: an animation test scene receives a montage play for a `montage` pick (`rate`, `section`,
    `start_time`) and nothing for a `clip` pick or an entity without a player; an audio test receives one
    cue play for a `cue` pick through `play_cue` (the cue's own volume and bus; `volume_db`, `bus` and
    position outputs are not applied). (Uses test doubles for the player/cue entry points where a device
    is needed; a real-device run is a TODO for the lead.)
18. Agent: `curves.*` and `chooser.*` round trips in `regia_agent` tests including edit, validate, evaluate
    with trace, degradation without plugins; `cargo xtask agent-coverage` and `check-layers` pass.
19. Hot reload: a bad `.chooser.ron` leaves the previous table active and the error visible to `chooser.list`.
20. Editor panel (desktop, needs a display; golden images are a TODO for the lead, never created by the builder):
    smoke test opens a table, edits a cell, sees the Pass/Fail coloring change after changing the test context.
21. Performance acceptance in § 10 via the micro-benchmark in `regia_framework/benches`.

## 9. Platform notes

Pure CPU data code; identical on Linux, Windows, Android aarch64; macOS testing-only. File access goes through
the asset server (`assets://`), so Android reads from the APK pak. The only platform difference is `f32` fused
multiply-add contraction: forbidden by writing the arithmetic as separate operations and not enabling
`-ffast-math`-like flags (Rust does not contract by default). Dependencies: none new. CSV parsing reuses the
data-table parser, RON/serde/postcard are existing. Nothing is proposed for a new crate.

## 10. Performance budget (min-spec machine class)

| Operation | Budget | Span |
|---|---|---|
| `CurveTable::sample` by handle | <= 50 ns (binary search over <= 4,096 keys) | none (too hot; counter only) |
| Chooser evaluate, 64 rows x 8 columns, no trace | <= 15 microseconds | `chooser_eval` |
| Same with `Full` trace | <= 150 microseconds | `chooser_eval_traced` |
| Binding pass, 256 `OnChange` bindings unchanged | <= 0.1 ms per frame | `chooser_bindings` |
| Table load + validate, 4,096 rows | <= 20 ms | `chooser_load` |
| Memory | <= 1 MiB per 4,096-row table; trace log <= 256 KiB | n/a |

Provider functions are game code and not part of the budget; the bridge systems take <= 0.05 ms per frame at 256
bindings (`chooser_bridge`).

As built (ADR-2132): test 21's micro-benchmark is `regia_framework/benches/tables_load.rs` (criterion; it measures
the cooked and text loads of 4,096-row choosers, curve tables and typed data tables, the sample and the 64 x 8 evaluation);
the hand-run `tests/chooser_perf.rs` stays.

As built (ADR-2012): `Row.cells` is a `RowCells`, a sorted exact-size vector with the map's method names. The memory budget holds
for one cell per row (0.78 MiB, asserted by `chooser_alloc`); 8 filled columns hold 2.6 MiB, about 198 bytes a row plus 66 a cell.

## 11. Out of scope / follow-ups

- Visual graph authoring, expression columns, and arbitrary script predicates (Rust providers cover them).
- Curve assets of vector and color kinds (three channels = three curves in one table for now; a typed
  vector/color curve asset is a follow-up).
- Spreadsheet-style formulas, curve filters (smoothing, baking, simplification), weighted tangents, buffered
  curve comparison in the panel, and keyframe copy/paste across curves.
- Multi-column randomization beyond one `Weight` column; per-row cooldowns.
- Multi-threaded evaluation (evaluation is already pure for non-`Weighted` modes; a parallel batch API is a
  follow-up if a profile needs it).
- A dialogue system or loot economy built on top.

## 12. Provenance

| Area | Algorithm/behavior | Public source |
|---|---|---|
| Cubic curve tangents | Cubic Hermite spline with monotone tangent limiter (Fritsch-Carlson), finite-difference tangents on non-uniform keys | F. N. Fritsch and R. E. Carlson, "Monotone Piecewise Cubic Interpolation", SIAM J. Numer. Anal. 17(2), 1980; Hermite basis as in any numerical analysis text |
| CSV forms | Record, quoting, line ending and BOM handling | RFC 4180, IETF; baseline `docs/specs/content-data.md` § 3.3 |
| Weighted draw with repeat avoidance | Roulette-wheel (fitness-proportionate) selection over cumulative weights, weights scaled by a recency multiplier | standard roulette-wheel selection; PCG32 per M. E. O'Neill, "PCG: A Family of Simple Fast Space-Efficient Statistically Good Algorithms for Random Number Generation", 2014 |
| Cost-based best match | Weighted normalized-distance scoring with an upper clamp, as in nearest-neighbor pose/clip selection by feature distance | D. Holden et al., "Learned Motion Matching", ACM TOG 39(4), 2020 (cost = weighted feature distance); G. Buttner and S. Clavet, "Motion Matching - The Road to Next Gen Animation", GDC 2015 |
| Rule tables with a trace | Ordered decision tables with first-hit and collect policies, and a default row | OMG Decision Model and Notation (DMN) 1.3 specification, decision-table hit policies (first, collect); cited for the policy concept only |
| Circular distance | Shortest signed distance on a ring by modulo fold | standard modular-arithmetic angle difference; no source needed beyond textbooks |

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 13. Decisions (ADR-1585)

- Decision (ADR-1585): the grouping of columns into gate / rank / draw / emit and the recency penalty are Regia's own
  design (a single `penalty` over a `window`, not a per-age multiplier list); the cost formula is the normalized
  clamped distance of § 2.4.
- Decision (ADR-1585): a nested chooser that returns `None` strikes its row and selection repeats (§ 2.3 step 5). The
  extra `Rng::f32` draws this can cost in `Weighted` mode are accepted; the order is deterministic.
- Decision (ADR-1585): the curve-table loader stays in `regia_framework` (modules `curve_table`, `chooser`). Layer-1
  crates have no need to sample curve tables today; moving it to `regia_asset` later is a non-breaking re-export.
- Decision (ADR-1585): the animation bridge supports `montage` picks through `regia_animation::montage_player::
  MontagePlayer::play_named` with `PlayOptions`. `clip` picks are cut from the first release unless the builder finds
  an existing named-slot entry point in `regia_animation`; adding one needs its own ADR. The audio bridge targets the
  existing `regia_audio::cue` player. Update §§ 2.7 and 8 test 17 accordingly while building.
- Decision (ADR-1585): agent access class `edit_assets` is the spec-23 token for the `Permission::Assets` variant in
  `regia_agent::schema`. Protocol errors travel as `F0002` with `data.reason` set to the codes listed here.
- Decision (ADR-1585): limits (4,096 rows/curves/keys, 64 columns, nesting depth 8) are Regia budget choices.
- Formats to register: `curve_table` v1 and `chooser` v1 (the lead runs `cargo xtask check-formats --update` and
  `cargo xtask manual`); format docs `docs/formats/curve-table-ron.md` and `chooser-ron.md`.

### Owner decisions

None.
