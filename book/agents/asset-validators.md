# Asset validators and the dependency graph (agents)

ADR-1568, extended by ADR-1668 (spec 32). Rust API: `regia_asset_pipeline::{asset_graph, validators,
validation_rules, validation_triggers, registry_bridge, asset_audit}`. Agent methods (editor extension, `read`
permission; nothing is written to disk or to the world): `editor.asset_validate`, `editor.asset_graph`,
`editor.asset_tree`, `editor.asset_size_map` and `editor.asset_audit`. The asset set comes in the request, or, with
`source: "registry"`, from the editor's loaded registry plus every `.ui.ron` under the open project's `assets/`
(those files are read; an inline `set` does not walk the disk). CLI: `regia assets validate|graph|size-map|audit`
(see the end of this page). `agent.guide { "topic": "asset-validators" }` returns a summary of this surface
(ADR-5632).

## The asset set

An `AssetSet` is data: `format_version`, optional `roots` and a list of assets. Each asset has a project-relative
`path`, a `kind` (`mesh`, `texture`, `material`, `scene`, `audio`, `other`), an optional `id`, the `references` it
makes (paths), `cooked_bytes`, optional `tags` (key to value, used by the graph and size-map filters) and the facts the
budgets need: `texture_size` (`(w, h)`) and `triangles`. It is RON;
put `#![enable(implicit_some)]` first to write `id: "x"` instead of `id: Some("x")`.

```ron
#![enable(implicit_some)]
(
    format_version: 2,
    assets: [
        (path: "scenes/main.scene", kind: scene, references: ["meshes/rock.mesh"], cooked_bytes: 10),
        (path: "meshes/rock.mesh", kind: mesh, id: "m1", triangles: 1200, references: ["textures/rock.png"], cooked_bytes: 100),
        (path: "textures/rock.png", kind: texture, id: "t1", texture_size: (1024, 1024), cooked_bytes: 1000),
    ],
)
```

Without `roots`, every `scene` asset is a root (the orphan check starts there). A newer `format_version` is refused;
version 1 files (no `tags`) still load and are upgraded in memory, so writing one back records version 2 (ADR-5632).

## Validators

A validator answers `pass`, `fail` or `not_validated` (the check does not apply, or the set lacks the data) plus
messages for one asset. The built-ins, in order:

| Name | Fails when |
|---|---|
| `missing_references` | an asset references a path that is not in the set |
| `naming_rules` | a path is empty, absolute, has `\`, an empty or `.`-leading segment, a character outside ASCII letters, digits, `_`, `-` and `.`, a Windows device name in any letter case (`CON`, `Aux`, `COM1`–`COM9`, `LPT1`–`LPT9`, with or without an extension), or is longer than `max_path_len`. Uppercase ASCII letters in stems and directories are allowed. Two file paths that differ only by ASCII letter case fail with `validate_name_case_collision` (Windows and the default macOS volume cannot hold both). The file path is the source before a cooker `#` label when that source is in the set. A cooker sub-asset `<source>#<label>` whose source is in the set has only its source part checked (the label is generated; an empty label fails); any other `#` is a bad character. A project that wants lowercase adds `PathStyle(id: "lowercase", glob: "assets/**", style: LowerAscii, severity: Error)` or `style: SnakeCase` (code `validate_name_style`) |
| `texture_size_limits` | a texture side is 0 or over `max_texture_dimension` (or not a power of two when `texture_power_of_two`); `not_validated` for non-textures and textures without a size |
| `duplicate_ids` | two assets share an `id`, or two entries share a path; `not_validated` without an id |
| `orphaned_assets` | no root reaches the asset (the message names roots that are not in the set); `not_validated` when the set has no roots |
| `mesh_triangle_budget` | a mesh has more than `mesh_triangle_budget` triangles; `not_validated` for non-meshes and meshes without a count |
| `localized_twin` | a localized variant (`localized/<L>/<rel>`) has another cooked type than its base (`validate_localized_type_mismatch`, error; the loader ignores it), or its base is not in the set (`validate_localized_no_base`, warning); `not_validated` for anything else (spec 36 § 6, ADR-1882) |
| `ui_image_missing` | a UI layout (`.ui.ron` under `assets/`, added to the set by `regia assets validate` with its component `use` paths and the paths its `image` and `mask` fields name) names a picture path no cooked asset matches (code `ui_image_missing`, one finding per path), or does not parse (`ui_layout_invalid`); `not_validated` for anything else. `missing_references` reports only a layout's missing `use` documents (ADR-3147, ADR-3148, [UI pictures](ui-image-assets.md)) |

`dialogue_assets` is not a built-in: `regia assets validate`, `regia cook`, `regia package` and the editor's save trigger
register it when the project has `.dialogue.ron` files (spec 36 § 11, ADR-2755). It runs the dialogue checks of
`regia loc validate` once (`regia_framework::dialogue::validate_dialogues`) and reports each issue with the issue's own code on
the dialogue file (`assets/<file>`; dialogue files are not cooked assets, so the run adds the ones with findings to the set
as `other` roots): `dialogue_duplicate_line`, `dialogue_unknown_voice`, `dialogue_missing_text`, `dialogue_missing_sound`
(errors), `subtitle_past_end` (warning, a cue starts after the sound's known length) and `dialogue_parse_error` (a file that
does not parse). `not_validated` for any other asset. Localization comes from `assets/localization` and
`localization.ron`, as `regia loc` reads it.

Rust hosts implement `Validator` (`name`, `description`, `validate(&ValidationContext, &AssetEntry) -> Verdict`) and
`ValidatorRegistry::register` it. Names are lowercase ascii, digits and `_` and unique.

## `editor.asset_validate`

`editor.asset_validate { set?, source?, validators?, config?, rules?, scope?, trigger?, max_assets?, exclude?,
warnings_fail?, strict?, limit?, list?, include_ron? }`

- `set`: the RON text (required unless `list`, or `source: "registry"`). `source: "registry"` builds the set from
  the editor's loaded asset registry and also reads every `.ui.ron` under the open project's `assets/` (ADR-3147):
  a layout already in the registry gains its component and picture references, and a layout that is not cooked is
  added as `other`. `registry_not_loaded` when no registry is loaded. An inline `set` does not walk the disk.
- `validators`: names to run (default all). `config`: `max_texture_dimension` (4096), `texture_power_of_two` (false),
  `mesh_triangle_budget` (200000), `max_path_len` (200); it overrides the limits of `rules`.
- `rules`: the text of `config/validation.ron` (below).
- `scope`: `{assets: [..]}` (named directly, `exclude` does not apply), `{folder, recursive}`,
  `{with_dependencies: [..]}` or `{changed: [..]}` (the caller's list of changed paths; assets still referencing a
  changed path that no longer exists are checked too); default the whole set. A scope that is empty, not an object
  or names two kinds is `validate_bad_scope`. A path or id in the scope that the set does not have is a failing
  `validate_unknown_asset` row (validator `validation`), not an error.
- `trigger`: `manual`, `save`, `cook`, `ci` or `agent` (default `agent`); it labels the run.
- `max_assets`: stop gathering at this many assets (sorted by path); `summary.cap_reached` says it cut.
  `exclude`: path globs applied to gathered assets. `warnings_fail`: a warning finding also fails the asset.
  `strict`: `exit_failed` is also true when any asset has a warning. `limit`: most rows returned (default 500;
  `rows_total` has the full count).
- `list: true` returns `{ validators: [{ name, description }] }` (the six built-ins plus `path_style`, `size_budget`
  and `dependency_rules`, plus `localized_twin` and `ui_image_missing`).

The answer is `{ ok, exit_failed, rows_total, format_version, summary: { passed, failed, not_validated, warnings,
requested, checked, skipped_excluded, cap_reached }, results: [{ validator, asset, outcome, messages, findings }] }`
with rows ordered by asset path, then validator name (`ron` too with `include_ron`). A finding is `{ code, severity
(info, warning, error), text, hint? }`. `Fail` always carries an `error` finding (`validate_no_finding` when the
validator gave none); a `Pass` carrying an `error` becomes `Fail`; `not_validated` carries none. A validator that
panics becomes `validate_validator_panicked` on that asset and the run goes on.

Finding codes of the built-ins: `validate_reference_missing`, `validate_name_invalid`,
`validate_name_case_collision`, `validate_id_duplicate`, `validate_asset_orphaned`; the texture and
triangle checks use their validator name as the code.

### Rules (`config/validation.ron`, format_version 1)

```ron
(
    format_version: 1,
    validate_on_save: true,
    cook: Warn,                       // Off | Warn | Fail
    exclude: ["assets/dev/**"],
    roots: ["assets/levels/**"],      // reachability roots for orphans (default: scene assets)
    limits: (max_texture_dimension: 4096),   // the `config` keys above
    rules: [
        PathStyle(id: "lowercase", glob: "assets/**", style: SnakeCase, severity: Error),
        SizeBudget(id: "texture-cap", kind: Texture, max_bytes: 16777216, severity: Warning),
        SizeBudget(id: "level-closure", glob: "assets/levels/*", max_closure_bytes: 1073741824, max_closure_assets: 5000),
        Forbid(id: "ui-not-dev", from: "assets/ui/**", to: "assets/dev/**"),
        MaxDependencies(id: "prefab-fanout", glob: "assets/prefabs/**", max: 64, severity: Warning),
        NoCycles(id: "no-cycles", glob: "assets/**"),
    ],
)
```

Unknown fields or variants, a repeated rule id and a malformed rule are `invalid_config` (the message carries the
line). A missing file means no rules. The built-in `naming_rules` check allows uppercase ASCII letters. A project
that wants lowercase keeps or adds this rule (the example above uses `SnakeCase`, which also refuses `-`):

```ron
PathStyle(id: "lowercase", glob: "assets/**", style: LowerAscii, severity: Error),
```

That rule judges only the file stem of paths under `assets/**`: `assets/Textures/rock.png`, `assets/rock.PNG` and
an uppercase path outside `assets/` all pass it. To judge directories everywhere too, use
`PathStyle(id: "lowercase", glob: "**", target: Path, style: LowerAscii, severity: Error)`; `target: Path` also
judges the segments of a cooker `#` label. Extensions are never judged, so neither form flags `.PNG`.

`PathStyle` judges the file stem (`target: Path` also the directories) against
`style` (`SnakeCase`, `KebabCase`, `LowerAscii`; non-ASCII fails), `prefix`, `suffix`, `allowed_chars` and `max_len`.
Codes: `validate_name_style`, `validate_name_length`, `validate_name_prefix`, `validate_size_over_budget`,
`validate_closure_over_budget`, `validate_dependency_forbidden`, `validate_dependency_count`,
`validate_dependency_cycle`. Every rule finding names its rule in `hint`.

## `editor.asset_graph`

`editor.asset_graph { set?, source?, asset? }` with `asset`: `dependencies`, `referrers` (reverse lookup),
`transitive_dependencies`, `transitive_referrers`, `missing_references`, `in_cycle` and `size` (`own_bytes`,
`total_bytes` = own plus everything it reaches, each asset once, `dependency_count`). Without `asset`: the size map,
`assets: [{ asset, own_bytes, total_bytes, dependency_count, referrer_count }]`, `count` and `roots`. Cycles are legal.

With `assets` (1 to 64 paths or ids) the answer is the column view:
`editor.asset_graph { assets, combine?, referrer_depth?, dependency_depth?, max_per_column?, kinds?, path?, tags?, pages? }`.

- Depths are 0 to 8 (default 1), width 1 to 500 (default 50); anything else is `graph_bad_depth`.
- Column *k* of a side holds the nodes first reached at hop *k*; a node reached twice sits in the shallower column. A
  column lists `max_per_column` nodes sorted by path; the rest is `hidden`. `pages: [{side, hop, page}]` shows the
  p-th chunk (so 20 nodes at width 10 give 10 plus `hidden: 10`, page 1 the other 10). Only listed nodes are
  expanded to the next hop.
- `combine: "intersect"` keeps only nodes every root reaches on that side; none in common gives no columns.
- `kinds`, `path` (glob) and `tags` (`"key"` or `"key=value"`, all must hold) hide nodes; roots are never hidden and
  `types_present` always lists every kind of the unfiltered set.
- Answer: `{ roots, columns: [{ side, hop, nodes, hidden, total, page, pages }], edges: [{ from, to, kind: "dep" }],
  types_present, truncated, missing }`.

## `editor.asset_tree`, `editor.asset_size_map`, `editor.asset_audit`

- `editor.asset_tree { asset, side, max_depth?, max_nodes?, include_sub_assets? }` returns `{ text, nodes, truncated }`:
  one asset per line, two spaces per level, `dep <path>`; a node already on the current chain gets a `(cycle)`
  suffix and is not expanded; a cut tree ends with `(truncated)`.
- `editor.asset_size_map { assets, group_by?, levels?, tag?, top? }` returns `{ closure_assets, total_bytes,
  roots_bytes, shared_bytes, exclusive_bytes, rows: [{ name, assets, bytes, exclusive_bytes, shared_bytes }],
  missing }`. `total_bytes` counts each asset once. A member is exclusive when every referrer is a root or exclusive
  (what removing the roots frees); the other non-root members are shared. `group_by` is `asset` (the rows past `top`
  fold into `(other)`), `kind`, `folder` (with `levels`) or `tag` (with `tag`; `(none)` when absent); another value is
  `size_map_bad_group`.
- `editor.asset_audit { assets?, folder? }` returns `{ csv, rows }`: RFC 4180 text with the columns `path, kind, size,
  dependency_count, referrer_count, closure_assets, closure_bytes, exclusive_bytes, tags` (`key=value` joined by `;`).

All answers are pure functions of the set and the parameters: identical inputs give byte-identical JSON.

## The panels

The editor shows the same answers in three panels (Window layout: the bottom dock), filled from the Content Browser:
each asset row's menu has *Validate*, *Validate with dependencies*, *Reference graph*, *Size map* and *Audit to CSV*,
and the header has *Validate project*. They read the editor's loaded registry and, like `source: "registry"`, every
`.ui.ron` under `assets/`, so a project is cooked first; the panels never change content. A click on a row is a
selection event: the Content Browser opens the asset's folder and marks it.

- **Problems**: findings sorted worst first, then by asset and validator, with severity and validator filters and
  *Re-run*, which repeats the last scope.
- **Reference graph**: referrers, selection and dependencies columns, back and forward through the queries shown, depth
  and width steppers, a type filter built from `types_present`, a "more" button that pages a column, a click that
  re-centers, and *Open size map*.
- **Size map**: a squarified treemap of `SizeMap.rows`; hover shows bytes, share and exclusive bytes.

*Audit to CSV* writes `.regia/audit/<path>.csv` atomically.

An agent reads and drives the one model a human sees. All four are `read` permission: they change the panels, not
content. `set` (RON) replaces the registry, as in the methods above.

| Method | Params | Result |
|---|---|---|
| `editor.asset_panels_state` | `limit?` | `{ source, error, problems, graph, size_map }` |
| `editor.problems_panel` | `run?`, `assets?`, `folder?`, `hide?`, `validator?`, `select?`, `set?`, `open_graph_error?` | `{ ran, summary, rows, rows_total, visible, hidden_severities, validator, validators }` |
| `editor.reference_graph_panel` | `action` (`open`, `recenter`, `back`, `forward`, `depth`, `width`, `kind`, `more`, `open_size_map`), `asset?`, `folder?`, `side?`, `depth?`, `width?`, `kind?`, `hop?`, `set?` | `{ query, result, can_back, can_forward, moved }` |
| `editor.size_map_panel` | `assets?`, `folder?`, `group_by?`, `top?`, `area?` (`[w, h]`), `hover?` (`[x, y]`), `set?` | `{ query, result, cells, hover? }`; `share_bp` in `hover` is parts per 10,000 |

```json
{ "method": "editor.problems_panel", "params": { "run": true, "hide": ["info"] } }
{ "method": "editor.reference_graph_panel", "params": { "action": "open", "asset": "assets/levels/forest.level.ron" } }
{ "method": "editor.reference_graph_panel", "params": { "action": "depth", "side": "dependencies", "depth": 2 } }
{ "method": "editor.size_map_panel", "params": { "assets": ["assets/levels/forest.level.ron"], "area": [800, 400], "hover": [10, 10] } }
```

`folder` (ADR-7725) is a Content Browser folder (`assets/props`), the routes of a folder row's menu: with `run`
and no `assets`, the Problems panel validates what is under it, recursively; `open` without `asset` and the Size map
without `assets` take every asset under it as the roots (`asset_folder_empty` when there is none). See
`book/agents/content-browser.md`.

`open_graph_error` (w13, ADR-8187) follows one material-graph compile error from the panel's `graph_errors`
list. The answer and the `graph_error_missing` code are in `book/agents/project-health.md`.

A failed action leaves the panel as it was. Errors are the codes below, plus `registry_not_loaded` when no `set` is
given and nothing is cooked.

## Error codes

`asset_set_invalid` (the RON does not parse), `asset_set_version` (newer `format_version`),
`asset_set_duplicate_path`, `unknown_validator`, `invalid_config`, `unknown_asset`, `validate_bad_scope`,
`graph_bad_depth`, `graph_too_many_roots`, `size_map_bad_group`, `registry_not_loaded`, `asset_folder_empty`. A missing `set` or a wrongly
typed parameter is `F0002`.

## CLI

```
regia assets validate [--asset P]... [--folder P [--recursive]] [--with-deps] [--changed FILE] [--strict] [--json] [--max N] [--config FILE]
regia assets graph <asset>... [--referrers N] [--dependencies N] [--width N] [--intersect] [--tree] [--json]
regia assets size-map <asset>... [--group asset|kind|folder:N|tag:KEY] [--top N] [--json]
regia assets audit [--folder P] [--asset P]... --csv <file>
```

They read the cooked registry (`--project`, `--platform`, `--registry`, as `regia assets query`). `validate` reads
`<project>/config/validation.ron` when it exists; it prints one line per finding (`<severity> <code> <asset>: <text>`)
and a summary, and exits 0 when clean, 1 on failure (with `--strict` also on warnings) and 2 when the configuration
cannot be read. `audit` writes the CSV atomically (temp file, then rename).

## Triggers

### Save (`editor.save_validation`)

Every level save the editor or an agent asks for (`editor.save_level`, the File menu, the headless editor) ends in
`level_io::save_level_and_terrain`, which calls `save_validation::note_saved(path)`: a queue insert, so saving is never blocked
or slowed by validation. The editor's `Last` system ticks the gate once per frame; the tick that releases a batch starts one run
on a worker thread (`trigger: save`, `scope: {changed: [paths]}`, the project's `config/validation.ron`, the loaded registry's
set, plus `dialogue_assets`), and a later tick hands the finished report to the Problems panel model, so
`editor.problems_panel` shows it (rows, filters, the summary). The first save reads `validate_on_save` from
`config/validation.ron`; `false` queues nothing.

`editor.save_validation { enabled?, hold?, wait? }` (permission `assets`) reports and steers the trigger:
`{ enabled, held, holds, pending, running, runs, last_batch, last_error, summary, rows_total }`. `hold: true` defers runs while
a batch tool saves many files (the paths keep queuing and run once after `hold: false`); `wait: true` blocks until the run
started by this call has finished (for scripts and tests; the editor itself never waits). `enabled: false` also drops the
queue. A wrongly typed parameter is `F0002`.

### Gates

`validate_on_save` runs the checks after a level save through `SaveValidationGate`: `note_saved(path)` queues, one `tick()`
per editor tick returns everything saved since as one batch (run it with `trigger: save` and a `changed` scope), and a
live `hold()` guard defers the run until the last guard drops. `false` queues nothing (the default is `true`).
`cook: Off | Warn | Fail` is what `cook_gate` makes of
a finished report; `Fail` stops the step, not the cook output already written. `regia cook` validates the cooked registry
after it is written, with the config's `cook` mode (default `Warn`: findings print, the step succeeds). `regia package`
calls `cook_validation_for(project, platform, true)` (ADR-4908): `Fail` unless the config names `cook` itself. No registry
or no `validation.ron` is not an error.

### Source lint (ADR-8390)

`regia assets validate` also scans glTF sources with the project source-lint policy. Extra flags:
`--target NAME` (default `default`), `--report FILE` and `--cancel-file FILE`. `--json` adds
`source_lint`. `--strict` fails on any unsuppressed source warning.

```text
regia assets lint-geometry [PATHS...] [--project DIR] [--target NAME] [--changed FILE] [--report FILE] [--cancel-file FILE] [--skip-staging] [--strict] [--json]
regia assets lint-materials <paths>... --profile compatibility --json
```

`lint-geometry` has no `--fix`. Geometry warnings fail it. `lint-materials` delegates to the
same material check as before.

A missing cooked registry on cook still prints `validate: no asset registry for <platform>; skipped`.
A registry file that exists but does not decode is an error, including when the package step did
not require the file. A required registry that is missing is an error. Those errors do not say
the check was skipped. Source diagnostics run before cook or package publishes; they do not
repair sources. `cook: Fail` and package stop on an unsuppressed geometry warning or an
incomplete source report. Material warnings are reported and do not by themselves stop those
steps. See [Content lint](../user-guide/content-lint.md) and [Source lint policy](content-lint.md).
