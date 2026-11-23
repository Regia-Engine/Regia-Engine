# Spec 32: Asset validation, reference graph and size map

> **Naming (truth pass 2026-11-02, ADR-2312).** ADR-1661 renamed this spec's "size map" to "footprint map" before anything was built, because a cleanroom audit found the old name matched another engine's tool name. The builder (ADR-1668, shipped in v0.2.7) used the earlier names, so the shipped API is `SizeMap`, `SizeMapQuery`, `editor.asset_size_map`, `editor.size_map_panel`, `regia assets size-map`, error `size_map_bad_group` and span `asset_size_map`. This spec now follows the shipped API. Renaming the shipped surface (with the old names kept as aliases for one release line) is an open question for the owner and counsel (`docs/open-questions.md`).

Status: proposed (ADR-1577). Plan item G1 (`BUILD-PLAN-1027.md` § 3), rung v0.3.x (b). Behavior written from the
public-sources spec set (ADR-1526); set area `editor-content` (audit and asset-management specs). No code is
implied beyond what this page states.
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: `regia_asset_pipeline::{asset_graph, validators}` (ADR-1568, on `integ/ext2`, not yet on main; this spec
extends it and must be built after that branch merges), `regia_asset::registry` (ADR-1466, main).
Built status (conformance audit 2026-11-10, ADR-5632): implemented. ADR-1668 built §§ 4-7 (findings, run control,
rules, bridge, graph, tree, size map, CSV, CLI, cook gate), ADR-1953 the editor panels and menus (§ 8), ADR-2755 the
editor's save trigger (§ 4.5). Later additions outside this page: the localized-twin check (ADR-1882, which revisits
the § 4.4 deferral), UI-layout picture checks (ADR-3147, ADR-3148), dialogue and provenance-policy validators. The
"Gaps this spec closes" list in § 2 is the state before ADR-1668. Refinements ADR-5632 records are marked in the text.

## 1. Purpose and scope

A project with thousands of assets needs four answers without opening the editor: *is this content allowed* (naming,
size budgets, forbidden dependencies), *what uses what* (a depth-limited reference graph), *what does this cost* (a
size roll-up of an asset plus everything it pulls in) and *give me the table* (a CSV audit). ADR-1568 built the
data model and a first validator set over an in-memory `AssetSet`. This spec records what it already does and adds:

1. a **bridge** from the cooked asset registry to an `AssetSet`, so the same code serves editor, CLI and CI;
2. **run control** (scopes, caps, exclusions, severities, triggers: save, cook, CI, agent);
3. **rule validators** driven by `config/validation.ron` (path style, size budgets, dependency rules);
4. a **reference graph** query (columns, depth, width limits, intersections, filters) and a text reference tree;
5. a **size map** with shared versus exclusive bytes and grouping;
6. an **audit CSV** export, CLI commands and editor panels.

Everything reads data already in the registry or the `AssetSet`; nothing loads an asset. Out of scope: § 11.

## 2. Baseline (implemented, spec records gaps)

Implemented by ADR-1568 (pure data, no disk access, `regia_asset_pipeline`, desktop only):

| Item | Where | Notes |
|---|---|---|
| `AssetSet` (`format_version` 2, `roots`, `assets`), `AssetEntry { path, kind, id, references, cooked_bytes, texture_size, triangles, tags }`, `AssetKind` | `asset_graph` | RON in and out; newer version and repeated paths refused (`asset_set_version`, `asset_set_duplicate_path`). Now `format_version` 2 (`tags`, § 4.2); a v1 file loads and is upgraded in memory, so it is written back as v2 (ADR-5632) |
| `DependencyGraph`: `dependencies`, `referrers`, `missing_references`, `transitive_dependencies`, `transitive_referrers`, `reachable_from`, `in_cycle`, `size_rollup` -> `SizeRollup { own_bytes, total_bytes, dependency_count }` | `asset_graph` | path keyed; cycles legal; ordered maps |
| `Validator` trait (`name`, `description`, `validate(&ValidationContext, &AssetEntry) -> Verdict`), `Verdict` (`Outcome::{Pass, Fail, NotValidated}` plus string messages), `ValidatorRegistry` (`with_builtins`, `register`, `unregister`, `names`, `describe`, `run`, `run_only`) | `validators` | names are unique lowercase ascii/digits/`_` |
| Built-ins `missing_references`, `naming_rules`, `texture_size_limits`, `duplicate_ids`, `orphaned_assets`, `mesh_triangle_budget`; `ValidationConfig` (4 limits, serde default) | `validators` | |
| `ValidationReport` (`format_version` 1, `summary { passed, failed, not_validated }`, rows ordered by asset path then validator), `to_json`/`to_ron`/`from_ron` | `validators` | |
| Agent `editor.asset_validate { set, validators?, config?, list?, include_ron? }`, `editor.asset_graph { set, asset? }` | editor extension methods (`regia_agent` cannot depend on the pipeline) | page `book/agents/asset-validators.md` |

Implemented on main: `regia_asset::registry` (`RegistryEntry { id, path, type_path, dependencies: Vec<AssetId>, tags,
size }`, `AssetRegistry::{by_id, by_path, resolve, query, dependencies, referencers}`), agent `registry.query|describe|
dependencies|referencers|tags`, CLI `regia assets query|tags`, `regia_asset::csv::{write_record, to_string}`.

Gaps this spec closes: no registry-to-set bridge, no severities or stable finding codes, no scopes or caps, no panic
isolation, no rule config, no budgets or dependency rules, no depth/width graph query, no tree, no size map by group, no
shared/exclusive bytes, no CSV, no CLI, no save/cook/CI triggers, no panels.

## 3. Regia terms

| Term | Meaning |
|---|---|
| Finding | One result line from a validator: stable code, severity, text, optional hint |
| Severity | `Info`, `Warning`, `Error` |
| Trigger | Why a run started: `Manual`, `Save`, `Cook`, `Ci`, `Agent` |
| Scope | What a run covers (§ 4.3) |
| Reference edge | "A uses B"; a *referrer* of B is such an A. One kind today (`Hard`) |
| Closure | The roots plus all transitive dependencies |
| Size map | Byte breakdown of a closure |

## 4. Validation

### 4.1 Crate layout

All new code lives in `regia_asset_pipeline` (modules `validators` extended, new `validation_rules`, `asset_audit`,
`registry_bridge`). No new crate, no new dependency, no layer change. `regia_asset` is unchanged except that the
bridge reads its public registry API. The editor panels live in `regia_editor`, the CLI in `regia_cli`
(`commands::assets_validate`), the agent methods are editor extension methods like the existing two. `ship_check`
keeps its own checks; calling the validators from it is optional (§ 4.7).

### 4.2 Bridge: registry to `AssetSet`

`AssetSet::from_registry(&AssetRegistry) -> AssetSet` (in `registry_bridge`):

- `path` = registry path (sub-assets `file#fragment` stay separate entries; their size is 0 because the parent file
  carries the bytes, already true in the registry); `id` = the `AssetId` rendered with its `Display`; `cooked_bytes` =
  `size`; `references` = the paths of the dependency ids; a dependency id with no record becomes the string
  `unresolved:<id>` so `missing_references` fires and the graph lists it under `missing`.
- `kind` from the cooked type path's last segment (`CookedMesh` -> `Mesh`, `CookedTexture` -> `Texture`, scene types ->
  `Scene`, material types -> `Material`, audio -> `Audio`, anything else `Other`). `texture_size` and `triangles` stay
  `None` (the registry does not store them): `texture_size_limits` and `mesh_triangle_budget` then report
  `NotValidated`, which is correct. Adding those two fields to the registry is a later registry v2 change (derived
  data, rebuilt by the next cook, no migrator); until then a project may enrich the set with the cook ledger if it
  wants the budgets enforced. Decision (ADR-1585): do not bump the registry format for this spec.
- `roots`: scene-kind assets, as `effective_roots` already does, unless the config lists `roots` globs.
- Tags are not copied into the set; `tag_filter` and `GroupBy::Tag` (§ 5) need them, so `AssetEntry` gains
  `tags: BTreeMap<String, String>` (`#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]`). Because the
  `AssetSet` format is new and unreleased (ADR-1568 is not on main), the field is added to v1 in place. If v1 has
  shipped by the time this is built, bump `ASSET_SET_FORMAT_VERSION` to 2 and keep reading 1 (host-visible: struct
  literals of `AssetEntry` break; list it under Upgrade).

### 4.3 Findings, severities and run control

`Verdict` gains `findings: Vec<Finding>`:

```rust
pub enum Severity { Info, Warning, Error }             // serde snake_case
pub struct Finding { pub code: String, pub severity: Severity, pub text: String, pub hint: Option<String> }
```

`Verdict::messages` stays and equals the findings' texts, so ADR-1568 callers and the agent answer keep working.
Constructors: `Verdict::fail(Vec<String>)` keeps its meaning (each message becomes an `Error` finding with code
`<validator name>`); new `Verdict::fail_with(Vec<Finding>)`, `Verdict::pass_with(Vec<Finding>)` (a pass may carry
`Warning`/`Info` findings). Contract: `Fail` implies at least one `Error` finding (the runner adds one, code
`validate_no_finding`, when a validator fails without any); a `Pass` carrying an `Error` finding is a contract bug and
the runner turns the outcome into `Fail`. `NotValidated` records no findings. Codes are lowercase ascii/digits/`_`.

`ValidationReport` (still `format_version` 1, added fields all `#[serde(default)]`; see the bump rule above if v1 has
shipped) gains per-row `findings`, and `summary` gains `warnings` (assets with at least one `Warning`), `requested`,
`checked`, `skipped_excluded` and `cap_reached`. `ValidationReport::exit_failed(strict: bool)` is true when
`failed > 0`, or when `strict` and `warnings > 0`.

`ValidatorRegistry::run_with(&self, set: &AssetSet, settings: &RunSettings) -> ValidationReport`:

```rust
pub struct RunSettings {
    pub scope: Scope,                  // default Project
    pub trigger: Trigger,              // default Manual
    pub max_assets: Option<usize>,     // stop gathering at the cap; sets cap_reached
    pub exclude: Vec<String>,          // path globs applied to gathered (not directly named) assets
    pub warnings_fail: bool,           // a Warning also fails the asset; default false
    pub only: Option<Vec<String>>,     // validator names (run_only semantics)
}
pub enum Scope {
    Assets(Vec<String>),               // paths or ids; named directly, so exclude does not apply
    Folder { path: String, recursive: bool },
    WithDependencies(Vec<String>),     // the assets plus transitive dependencies
    Changed(Vec<String>),              // paths the caller says changed (from its VCS diff or watcher)
    Project,
}
pub enum Trigger { Manual, Save, Cook, Ci, Agent }
```

`run` and `run_only` stay and call `run_with` with defaults. Behavior:

- Gathering is sorted by path; the cap cuts the sorted list; `requested` counts the scope, `checked` what ran.
- `Changed` also checks every asset that still references a changed path which no longer exists in the set (a deletion
  that breaks a user); those report `missing_references`. Regia has no version-control abstraction; the caller
  supplies the list.
- A scope path or id the set cannot resolve counts in `requested` and yields a run-level row with validator
  `validation` and finding code `validate_unknown_asset` (a finding, not an error); the call still succeeds.
  *Refined (ADR-5632):* the row's outcome is `Fail` with an `Error` finding, so a run naming a missing asset exits
  failed; under `Changed` a path that no longer resolves is an expected deletion: it gives no unknown-asset row and
  does not count in `requested` (its referrers report `missing_references` instead).
- Each validator call runs inside `std::panic::catch_unwind`; a panic becomes an `Error` finding
  `validate_validator_panicked` on that asset and the run continues. (Shipping uses `panic=abort`, but validation is
  an editor/CLI path built with unwinding.)
- A validator's `enabled` test is the registry itself (`unregister`, or `only`); no per-run enabled hook is added.
- Determinism: output order is (asset path, validator name, finding order); no clock, no thread dependence.

### 4.4 Rule validators and `config/validation.ron` (v1)

New validators (names obey the registry rule), registered by `with_builtins` when the config has rules for them and
listed by `list: true` either way: `path_style`, `size_budget`, `dependency_rules`. They read only the set.

| Validator | Checks | Finding codes |
|---|---|---|
| `path_style` | For assets matching a glob: file stem or path against a style (`SnakeCase`, `KebabCase`, `LowerAscii`), required prefix or suffix, allowed characters, maximum length. Non-ASCII is rejected by `LowerAscii`/styles | `validate_name_style`, `validate_name_length`, `validate_name_prefix` |
| `size_budget` | `cooked_bytes` against a limit per glob or kind; optional closure limit (bytes and asset count) via the graph | `validate_size_over_budget`, `validate_closure_over_budget` |
| `dependency_rules` | Forbidden edge (`from` glob must not use `to` glob); maximum direct dependency count; cycle among assets matching a glob | `validate_dependency_forbidden`, `validate_dependency_count`, `validate_dependency_cycle` |

`naming_rules`, `missing_references`, `duplicate_ids`, `orphaned_assets` (orphans) are the existing built-ins and are
not duplicated. Their findings get codes `validate_name_invalid`, `validate_name_case_collision`,
`validate_reference_missing`, `validate_id_duplicate`,
`validate_asset_orphaned` (severity `Error`, outcome `Fail`, the same as the other three).
`naming_rules` allows uppercase ASCII letters in stems and directories (ADR-7961). A space, a character outside
ASCII letters, digits, `_`, `-` and `.`, and a Windows device name in any letter case stay `validate_name_invalid`.
Two file paths that differ only by ASCII letter case are `validate_name_case_collision`. A project that wants
lowercase adds `PathStyle` with `style: LowerAscii` or `SnakeCase` (`validate_name_style`). A localized-variant-without-source check and a
content-hash duplicate check from earlier drafts are dropped: Regia has no localized asset tree to check and the
registry has no content hash. Decision (ADR-1585): defer both; the second needs a registry hash field.

`config/validation.ron` (new frozen format, `format_version: 1`; doc `docs/formats/validation-ron.md`; the lead runs
`cargo xtask check-formats --update` and `cargo xtask manual`):

```ron
(
    format_version: 1,
    validate_on_save: true,
    cook: Warn,                       // Off | Warn | Fail
    exclude: ["assets/dev/**"],
    roots: ["assets/levels/**", "assets/ui/**"],   // reachability roots for orphans (default: scene assets)
    limits: (max_texture_dimension: 4096, texture_power_of_two: false,
             mesh_triangle_budget: 200000, max_path_len: 200),   // the existing ValidationConfig, same names
    rules: [
        PathStyle(id: "lowercase", glob: "assets/**", style: SnakeCase, severity: Error),
        SizeBudget(id: "texture-cap", kind: Texture, max_bytes: 16777216, severity: Warning),
        SizeBudget(id: "level-closure", glob: "assets/levels/*", max_closure_bytes: 1073741824,
                   max_closure_assets: 5000, severity: Error),
        Forbid(id: "ui-not-dev", from: "assets/ui/**", to: "assets/dev/**", severity: Error),
        MaxDependencies(id: "prefab-fanout", glob: "assets/prefabs/**", max: 64, severity: Warning),
    ],
)
```

Rules: unknown variants or fields are a load error `invalid_config` (with line and column in the message; no silent
ignore; the same code `editor.asset_validate` already uses for a bad `config`); rule ids unique. A missing file
means defaults: no rules, `Warn`, only the built-ins act. `limits` is exactly `ValidationConfig`, so the existing
inline `config` parameter keeps working and overrides the file. The format is frozen from its first release: a later
shape change needs version 2, a migrator and `regia upgrade-project` support (spec 12 § 4a). No earlier version exists,
so no migrator now.

### 4.5 Triggers

1. **Manual**: editor commands (asset, selection, folder, "with dependencies", whole project), CLI, agent.
2. **Save**: after the live cook (ADR-1466) re-cooks sources and updates the registry, the editor validates the saved
   assets once on the next editor tick, so several saves in one tick make one run (counted in ticks, never by wall
   clock). `SaveValidationGate::hold()` returns a guard; while any guard lives, saved paths queue and run when the last
   guard drops (batch tools such as importers hold one). `validate_on_save: false` disables it; the default `true`
   queues each level save. Run with `Trigger::Save` and `Scope::Changed`. *Built (ADR-2755):* every level save in the
   editor or through the agent calls `SaveValidationGate::note_saved` (`regia_editor::save_validation`); a worker-thread
   run fills the Problems panel; the agent steers it with `editor.save_validation`. Saving a source that is not a level
   (a texture, for example) does not call `note_saved`.
3. **Cook**: `regia cook` validates the cooked set after the registry is written. `cook: Off | Warn | Fail` (default
   `Warn`); `regia package` uses `Fail` unless the config says otherwise. `Fail` stops the package step, not the cook
   output already written.
4. **CI**: `regia assets validate --strict --json`, exit status 1 on failure (§ 7).
5. **Agent**: `editor.asset_validate` (read permission, never mutates).

The validators and the runner never log by themselves; the caller presents the report, so a test or agent can validate
without side effects.

### 4.6 Source-reading validators

The ADR-1568 contract is "must not touch the disk". It stays. A validator needing source bytes (none ships in v1) is
out of scope; add a separate trait later with its own ADR. Decision (ADR-1585): no `needs_source` hook in v1.
Source-byte evidence is a separate API, `source_lint`, not this trait (ADR-8248).

### 4.7 `ship_check` and packaged builds

Validation is an editor/CLI/CI tool (desktop). A packaged build is not expected to run it. `regia assets validate
--registry <pak>.registry.bin` validates a shipped pak's registry offline, which covers the diagnostic need.

## 5. Reference graph, tree and size map

All functions are in `asset_audit` and work on `DependencyGraph` (so they serve any `AssetSet`, including one from a
registry). Ids and paths are accepted for roots; internal key is the path.

### 5.1 Graph query

```rust
pub struct GraphQuery {
    pub roots: Vec<String>,            // 1..=64
    pub combine: Combine,              // Union (default) | Intersect
    pub referrer_depth: u32,           // 0..=8, default 1
    pub dependency_depth: u32,         // 0..=8, default 1
    pub max_per_column: u32,           // 1..=500, default 50
    pub kinds: Vec<AssetKind>,         // empty = all
    pub path_glob: Option<String>,     // restricts every node but the roots
    pub tag_filters: Vec<(String, Option<String>)>, // key present / key=value; AND
    pub pages: BTreeMap<(Side, u32), u32>, // page index per (side, hop) column
}
pub enum Side { Referrers, Dependencies }
pub enum EdgeKind { Hard }             // #[non_exhaustive]; JSON "dep"
```

- **Columns.** Dependencies expand breadth-first to `dependency_depth`, referrers to `referrer_depth`. Column *k* holds
  nodes first reached at hop *k* (a node reached by two paths sits in the shallower column once; every edge is kept in
  `edges`). Cycles are safe.
- **Width.** A column lists at most `max_per_column` nodes sorted by path; the rest is a `more { hidden, pages }` entry
  and `pages[(side, k)] = p` returns the p-th chunk. 20 nodes with width 10 gives 10 plus `hidden: 10`.
- **Several roots.** `Union` expands all roots, the middle column lists them. `Intersect` keeps only nodes reachable
  from every root on that side; none in common gives an empty side.
- **Filters** hide nodes and the edges touching them but never alter data; roots are never filtered; a filtered node
  does not use width. `types_present` lists every kind in the unfiltered graph, sorted.
- An asset nothing uses has an empty referrer side (not an error). Folder selections are expanded by the caller (cap
  5,000 roots, else `graph_too_many_roots`); the 64-root limit applies to explicit lists.
- Result `GraphResult { roots, columns: Vec<Column { side, hop, nodes, hidden }>, edges: Vec<Edge { from, to, kind }>,
  types_present, truncated, missing }`.
- `EdgeKind` has one variant now. A soft kind (path references loaded on demand) needs the cooked header to carry the
  flag: a registry format bump (derived data, rebuilt by the next cook). Not part of this spec; the field exists so the
  addition is not an API break.

### 5.2 Reference tree

`reference_tree(graph, root, side, max_depth, max_nodes) -> String`: indented text, one asset per line, `dep <path>`
(the edge-kind tag then the path). A node already on the current chain is printed with a `(cycle)` suffix and not
expanded; output ends with a `(truncated)` line at `max_nodes` (default 2,000). Sub-assets (`file#fragment`) are listed
as separate nodes; `include_sub_assets` (default true) can hide them.

### 5.3 Size map

```rust
pub struct SizeMapQuery { pub roots: Vec<String>, pub group_by: GroupBy, pub top: u32 /* default 200 */ }
pub enum GroupBy { Asset, Kind, Folder(u32), Tag(String) }
pub struct SizeMap { pub closure_assets: u64, pub total_bytes: u64, pub roots_bytes: u64, pub shared_bytes: u64,
                     pub rows: Vec<SizeRow>, pub missing: Vec<String> }
pub struct SizeRow { pub name: String, pub assets: u64, pub bytes: u64, pub exclusive_bytes: u64, pub shared_bytes: u64 }
```

- `total_bytes` sums `cooked_bytes` over the closure, each asset once (a diamond counts the shared leaf once).
- A member is **shared** when it has a referrer outside the closure. **Exclusive** members would become unreferenced if
  all roots were removed: fixed point (a member is exclusive when every referrer is a root or an exclusive member). A
  row's `exclusive_bytes` is "what deleting this saves". *Refined (ADR-5632):* every non-root member is exactly one of
  the two, so a member reached only through a shared member is shared too (deleting the roots would not free it);
  `SizeMap` also carries the closure's `exclusive_bytes`, and `roots_bytes + shared_bytes + exclusive_bytes =
  total_bytes`.
- Rows sort by bytes descending then name; `Asset` folds everything past `top` into an `(other)` row so sums stay exact;
  `Folder(n)` groups by the first n path segments; `Tag(k)` by value (`(none)` when absent).
- Size is cooked bytes on disk. An in-memory or GPU size view needs per-asset decoded sizes the registry lacks: out of
  scope (§ 11).
- `missing` lists unresolved dependency ids with 0 bytes.

### 5.4 Audit CSV

`audit_csv(set, graph, selection) -> String` through `regia_asset::csv::to_string` (RFC 4180). Columns in fixed order:
`path,kind,size,dependency_count,referrer_count,closure_assets,closure_bytes,exclusive_bytes,tags`; `tags` is sorted
`key=value` pairs joined by `;`; rows sorted by path; closure and exclusive columns use the § 5.3 definitions with the
row's asset as the only root. The CLI writes atomically (temp file then rename).

## 6. Agent protocol surface

Page: `book/agents/asset-validators.md` (extend the existing page); guide topic `asset-validators` (added by ADR-5632);
`book/COVERAGE.md`
rows for each new method. All are editor extension methods, class **read**; errors lowercase ascii/digits/`_`, listed on
the page.

| Method | Params (added to the existing ones) | Result |
|---|---|---|
| `editor.asset_validate` (extended) | `scope?` (`{assets}`, `{folder, recursive}`, `{with_dependencies}`, `{changed}`), `trigger?`, `max_assets?`, `exclude?`, `warnings_fail?`, `strict?`, `limit?` | existing answer plus `findings` per row, `summary.{warnings, requested, checked, skipped_excluded, cap_reached}`, `exit_failed`; rows truncated to `limit` (default 500) with `rows_total` |
| `editor.asset_graph` (extended) | `assets`, `combine?`, `referrer_depth?`, `dependency_depth?`, `max_per_column?`, `kinds?`, `path?`, `tags?`, `pages?` | `GraphResult` (the existing `asset` form stays) |
| `editor.asset_tree` | `asset`, `side`, `max_depth?`, `max_nodes?`, `include_sub_assets?` | `{ text, nodes, truncated }` |
| `editor.asset_size_map` | `assets`, `group_by?`, `levels?`, `tag?`, `top?` | `SizeMap` |
| `editor.asset_audit` | `assets?`, `folder?` | `{ csv, rows }` (the CLI writes the file; the agent gets text) |

Existing `set` stays the source of truth in the request; an additional `source: "registry"` (default `"set"`) builds the
set from the editor's loaded registry via the bridge (`registry_not_loaded` when none). Errors: existing
`asset_set_invalid`, `asset_set_version`, `asset_set_duplicate_path`, `unknown_validator`, `invalid_config`,
`unknown_asset`; new `validate_bad_scope` (conflicting or empty scope), `graph_bad_depth` (depth above 8, width outside
1..=500), `graph_too_many_roots`, `size_map_bad_group`, `registry_not_loaded`. Unresolvable scope paths are findings
(`validate_unknown_asset`), not errors. Identical inputs give byte-identical JSON.

## 7. CLI (`regia assets`, extends `commands::assets_query`'s subcommand enum)

```
regia assets validate [--asset P]... [--folder P [--recursive]] [--with-deps] [--changed FILE] [--strict] [--json] [--max N] [--config FILE]
regia assets graph <asset>... [--referrers N] [--dependencies N] [--width N] [--intersect] [--tree] [--json]
regia assets size-map <asset>... [--group asset|kind|folder:N|tag:KEY] [--top N] [--json]
regia assets audit [--folder P] [--asset P]... --csv <file>
```

Registry location flags reuse `RegistrySource` (`--project`, `--platform`, `--registry`). `validate` exit status: 0
clean, 1 on failure (with `--strict` also on warnings), 2 on usage error or unreadable config. Text output: one line per
finding `<severity> <code> <asset>: <text>` and a summary line.

## 8. Editor (desktop only)

- Content-browser asset, folder and project menus gain *Validate*, *Validate with dependencies*, *Reference graph*,
  *Size map* and *Audit to CSV*.
- **Problems panel**: sorted findings, filter by severity and validator, click selects the asset, re-run.
- **Reference graph panel**: referrers, selection, dependencies columns; back/forward history of queries; depth and
  width steppers; a type filter built from `types_present`; a "more" node that pages in the rest; re-center; "open size
  map".
- **Size map panel**: squarified treemap (Bruls, Huizing, van Wijk 2000) of `SizeMap.rows`; hover shows bytes, share,
  exclusive bytes.
- Built (ADR-1953): `regia_editor::asset_panels` (`ProblemsState`, `ReferenceGraphState`, `SizeMapState`) and
  `regia_editor::treemap`.
- Panels never mutate content; they emit editor selection events. Each ships its agent surface above.

## 9. Determinism and performance

- Report, graph, tree, size map and CSV are pure functions of (set, config, query): sorted maps and sorted output only.
- Budgets on 50,000 assets / 400,000 edges, desktop, one core: project-wide registry-bridge validation <= 400 ms;
  graph query depth 2/2 width 50 <= 10 ms; size map for a 5,000-asset closure <= 50 ms; save-time run for <= 10 assets
  <= 5 ms. These are benchmark targets recorded in `docs/progress.md`, not test asserts. No per-frame work.
  Built (ADR-2704): `regia_asset_pipeline/tests/perf_micro_assetdb.rs` (`#[ignore]`, hand-run) prints them for
  `cargo xtask perf-budgets` (`docs/perf/budgets.ron`).
- `tracing` spans: `validate_run`, `validate_validator{name}` (per validator), `asset_graph_query`, `asset_size_map`.

## 10. Tests and acceptance

Unit tests in `regia_asset_pipeline` over synthetic sets and `AssetRegistry::from_entries`; agent tests in
`regia_agent` (dev-dependency on the pipeline already exists). No golden, no GPU, no wall clock.

1. Bridge: ids, paths, sizes, kinds map as § 4.2; an unknown dependency id becomes `unresolved:<id>` and
   `missing_references` fails; sub-asset size 0 is not double counted in a folder group.
2. Scopes: folder (recursive flag respected), project, `WithDependencies` (a cycle terminates), `Changed` with a
   deleted path pulls in its referrers; unknown scope path gives `validate_unknown_asset`.
3. `max_assets` cuts the sorted list and sets `cap_reached`; named assets bypass `exclude`, gathered ones honor it.
4. `NotValidated` never fails an asset; severities: `Error` fails, `Warning` does not unless `warnings_fail`;
   `exit_failed(strict)` honors both; a `Pass` carrying an `Error` finding becomes `Fail`.
5. A panicking validator yields `validate_validator_panicked` and the others' rows are intact.
6. Each rule code has a failing and a passing fixture (styles incl. non-ASCII, budget at the boundary, closure budget,
   forbidden edge, 3-asset cycle, fan-out).
7. Config: unknown field is `invalid_config` with a line; missing file gives defaults; duplicate rule ids refused;
   `limits` equals `ValidationConfig` defaults when absent.
8. Save gate: three saves in one tick give one run covering all three; a live `hold()` guard defers it, nested guards
   need all dropped. Ticks are counted by the test.
9. Cook gate: `Warn` completes with findings; `Fail` returns failure to `package`; `Off` runs nothing.
10. Graph: depth 2 shows related-of-related; width 10 with 20 references gives 10 plus `hidden: 10`, page 1 the other
    10; `Intersect` shows only shared nodes, empty when none; unreferenced asset has an empty referrer side; a cycle
    appears once; filters hide nodes while `types_present` stays unfiltered and roots are never filtered.
11. Tree: indentation, `(cycle)` marker, `(truncated)` at `max_nodes`.
12. Size map: diamond counts the leaf once; an externally referenced member is shared not exclusive; chain fixed point;
    group sums equal `total_bytes` including `(other)`; missing ids listed at 0 bytes.
13. Audit CSV: fixed header and order, RFC 4180 quoting of a path with comma and quote, sorted rows, atomic write.
14. Agent: every method round-trips; every error code in § 6 is produced; two calls give byte-identical JSON.
15. `cargo xtask agent-coverage`, `book-check`, `check-layers` pass (no new crate edge).

Tests proving each item (ADR-5632): 1 `registry_bridge::tests::{bridge_maps_ids_paths_sizes_and_unresolved_ids,
kinds_follow_the_type_path, root_globs_replace_scene_roots}` and `tests/spec32_run_control.rs::
bridge_sub_assets_are_not_double_counted_in_a_folder_group`; 2-5 `tests/spec32_run_control.rs` (`folder_scope_*`,
`with_dependencies_*`, `changed_with_a_deleted_path_*`, `an_unknown_scope_name_*`, `the_cap_cuts_*`,
`the_verdict_contract_and_severities`, `a_panicking_validator_is_isolated`) and `tests/tqs2_sweep.rs`; 6-7
`validation_rules::tests`; 8-9 `validation_triggers::tests` and `regia_cli` `assets_validate::tests::{cook_validation_modes,
package_defaults_the_cook_gate_to_fail_unless_the_config_names_it}`; 10-12 `asset_audit::tests`; 13
`asset_audit::tests::audit_csv_header_order_quoting_and_sorting` and `regia_cli` `graph_size_map_and_audit_commands`
(atomic write); 14 `regia_editor` `asset_validate_agent::tests`; determinism also
`tests/spec32_run_control.rs::reports_are_ordered_and_byte_identical`; the § 4.4 built-in codes
`builtin_failures_carry_their_spec_codes`.

## 11. Platform, scope, open points

- Linux and Windows desktop for the pipeline, CLI and editor; the Android aarch64 build must still compile the
  workspace (the pipeline is already excluded from runtime crates). macOS testing-only, nothing special.
- **Out of scope:** automatic fixes (validators only report), redirector repair, version-control integration, scripting
  validators, in-memory/GPU size view, soft reference kinds, collections from a graph node, source-reading validators,
  content-hash duplicates, localized-twin checks.
- Migration: `config/validation.ron` v1 is new; `AssetSet`/`ValidationReport` additions are additive (bump rule in
  § 4.2/4.3). Lead: `cargo xtask check-formats --update`, `cargo xtask manual`.
- Provenance (§ 12) and the book page/`COVERAGE.md` rows are part of the work.

### Owner decisions

None.

## 12. Provenance

Public user documentation of asset validation, reference viewing and size maps (behavior only); algorithms:
squarified treemaps (Bruls, Huizing, van Wijk 2000), breadth-first search, fixed-point reachability, RFC 4180. Set area:
`editor-content` (`editor-content-asset-audit`, `editor-content-asset-management`). Row in `docs/provenance.md`
(ADR-1577). Algorithms are standard; the set's files are not a source of any of them.

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

## 13. Source-lint evidence (ADR-8248)

`Validator` stays disk-free. Source-byte evidence is `regia_asset_pipeline::source_lint`. It resolves
`config/source_lint.ron` (version 1) in importer order: engine defaults, project defaults, the named target
profile, then per-asset overlays. A present overlay field replaces that field. Overlay globs match
ASCII-case-insensitively. Suppression paths are exact, and a suppression also names the rule id, the BLAKE3
content hash, a reason, and an optional location.

`produce` emits one report (`SOURCE_LINT_REPORT_VERSION` 1) with stable rule ids, locations, metric units,
support status and work counters. A missing metric is unknown, never zero. Truncation and cancellation cannot
report `complete: true`. The same findings encode as compact JSON and SARIF 2.1.0. Existing geometry, material
and validation reports stay as they are; adapters map locations both ways and do not recover examined counts.

The policy file is the frozen format `source-lint`. The report schema is included in that format's fingerprint,
so a report field change fails `cargo xtask check-formats` until the format version is coordinated. This is a
new v1 file. It does not change `config/validation.ron`.

The editor runs `produce` on one `content_lint` worker and publishes findings to the Problems panel
with validator `source_lint` (ADR-8391). A save or import of `.gltf` or `.glb` queues one coalesced
scan. A result is published only when the source generation, the settings generation, and the content
hashes still match; a mismatch is discarded. The frozen `source-lint` v1 file and the report schema
are unchanged.
