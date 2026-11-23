# Project health, staged validation and the Problems and Output Log lists

ADR-7789. Code: `regia_editor::project_health` (`editor.project_health`, `editor.precommit_hook`), `asset_panels`
(Problems grouping), `log_panel` (Output Log grouping), `log_links` (entity links),
`regia_asset_pipeline::precommit` (the staged check and the hook) and the CLI's `regia assets validate --staged` and
`regia hooks`. The validators themselves are on [Asset validators and the dependency graph](asset-validators.md); the
toolchain checks on [Toolchain doctor](doctor.md).

## Validation before commit

`regia assets validate --staged [--project DIR] [--json] [--strict]` validates what is staged in git:

1. The staged changes under the project (`git diff --cached`, run as a program with an argument vector, never through
   a shell). Files elsewhere in the repository are ignored.
2. Every staged text asset is read **from the index**, so the check sees what the commit will hold, not the working
   tree: `.ron` assets and `.meta` sidecars must parse as RON, `.gltf` and `.json` as JSON. A file that does not parse
   is an error, `staged_parse_error`; one git cannot read is `staged_unreadable`; one over 64 MiB is a note,
   `staged_too_large`, and is not parsed.
3. With a cooked registry, the validators of `config/validation.ron` run over the staged paths (scope `changed`: a
   `.meta` names its asset; assets that reference a deleted staged path are checked too). Without one the run says
   `registry validators skipped` and only the staged files are checked, so the hook works in a fresh clone.

Exit 0 clean, 1 on a failure (with `--strict` also on warnings), 2 when the project is not in a git work tree
(`not_a_git_repo`) or git does not run (`git_not_found`). Inside a hook git exports `GIT_DIR` and `GIT_INDEX_FILE`;
they are honored (made absolute first), so `git commit -a` is checked against the index it commits.

`--json` answers `{ staged: { repo_root, files: [{ path, project_path, status }], checked, findings: [{ path, finding
}], changed_assets }, report, registry_note, exit_failed }`.

## The pre-commit hook

| Command | What it does |
|---|---|
| `regia hooks install [--project DIR] [--force] [--json]` | Writes the hook where git runs it (`git rev-parse --git-path hooks/pre-commit`, so `core.hooksPath` and linked worktrees work). A hook Regia wrote is rewritten; another is refused (`hook_foreign`) unless `--force`, which moves it to `pre-commit.before-regia` (or `pre-commit.before-regia.2` and so on when an earlier backup is there; no backup is ever replaced) after the new script is written, so a failed install leaves the old hook in place. A symlink to a missing script counts as another hook. |
| `regia hooks uninstall [--project DIR] [--force] [--json]` | Removes Regia's hook and puts back the one it replaced. `hook_missing` when there is none; another hook is refused unless `--force`. |
| `regia hooks status [--project DIR] [--json]` | `{ path, exists, regia, backup }`. |

The hook is a POSIX `sh` script (Git for Windows runs it too) carrying the line `regia-pre-commit-hook 1`. It runs the
`regia` that installed it, or `regia` on `PATH` when that file is gone, with `assets validate --staged --project
<project relative to the work tree>`. `git commit --no-verify` skips it once.

In the editor, **Project Settings ▸ Validation** shows the hook's state with **Install pre-commit hook**, **Remove
pre-commit hook** and, when another hook is installed, a checkbox to replace it. The agent route is
`editor.precommit_hook`:

| `action` | Does | Answers |
|---|---|---|
| `state` (default) | Reads the hook | `{ state: { path, exists, regia, backup } }` |
| `install` (`force?`) | As `regia hooks install`, with the CLI the editor finds (`$REGIA_CLI`, beside the editor, `PATH`) | `{ change: { path, action, backup }, state }` |
| `uninstall` (`force?`) | As `regia hooks uninstall` | `{ change, state }` |

`change.action` is `installed`, `updated`, `replaced`, `removed` or `restored`.

## Problems: groups, duplicates, entity links, copy

The Problems panel can list its findings under one collapsible header per **source** (the validator or system that
reported them), worst source first and then by name; **Collapse duplicates** (on by default) shows identical
consecutive findings once with a count (`×3`). A finding whose text names an entity GUID (`67e55044-10b1-426f-9247-
bb680e5fe0c8`) has a link that selects that entity of the open level and frames it. **Copy all** copies the list as
drawn (a header as `[source] (N)`, a run with ` (×N)`); **Copy selected** the last clicked row with its hint.

`editor.problems_panel` takes the same controls before it answers. It needs the `play` permission and counts as mutating (like `editor.log_open_link`), because `select_entity` changes the selection and frames the camera:

| Param | As |
|---|---|
| `group: bool` | Group by source |
| `collapse: bool` | Collapse duplicates |
| `toggle_group: "<source>"` | A click on the source's header |
| `select_row: <index>` | A click on the row (picks its asset; Copy selected copies it) |
| `select_entity: "<guid>"` | The row's entity link |
| `copy: "all" \| "selected"` | Answers `copied`, the text the button copies |

The answer adds `grouped`, `collapse_duplicates`, `groups: [{ source, rows, worst, open }]`, `selected_row` and, per row,
`index`, `count` and `entities`. A `select_row` past the rows is refused before anything changes:
`problem_row_missing`. An unknown entity is `F0003` (not found).

## Output Log: groups, duplicates, entity links, copy

The Output Log's **Collapse duplicates** (on by default) shows identical consecutive lines (same level, target and
message) as one row with a count; **Group by source** lists the rows under one collapsible header per log target (the
module that logged them), headers sorted by name. Both views are kept as lines arrive, so a frame lays out the rows on
screen only. **Copy all** copies the rows as drawn, **Copy selected** the expanded line. Entity GUIDs in a line are links
(kind `entity` in `editor.log_links`; `editor.log_open_link` selects and frames the entity and answers `{ opened:
"entity", guid, entity }`). The panel's filters and groups are view state of the window; agents read the same records
with `logs.read`.

## Project Health

**Tools ▸ Project Health** (and the palette's *Window ▸ Open the Project Health tab*) opens one ranked list built from
the checks the project already has. Each row has a severity, a category, what was found, a recommended action and a
link (**Open**, **Show in Problems** or **Copy command**).

| Category | Source | Link |
|---|---|---|
| `missing_reference` | the `missing_references` and `ui_image_missing` validators | Problems, filtered to the validator |
| `budget` | `texture_size_limits`, `mesh_triangle_budget`, `size_budget`: the limits and rules of `config/validation.ron` | Problems |
| `validation` | every other validator (one row per validator and finding code, with a count and example assets) | Problems |
| `toolchain` | `regia doctor --json` checks that warn or fail; a note when the CLI did not run | the fix command |
| `cook_warning` | the cook's import warnings (`import_reports.json`) per source | the source |
| `unreferenced_large` | assets of 1 MiB or more cooked that nothing references and that are no root (20 listed, the rest one row) | the asset |
| `recovery` | autosaves left by a crash (a warning while the recovery is on offer, a note once the level was saved after it) | the file |

Rows rank errors first, then warnings, then notes; within a severity by the category order above, then by title.
**Re-run** runs the checks as an editor job (`project_health:<n>`, with a progress row; `editor.cancel_job` stops it
between sources and the rows that ran stay). The run also fills the Problems panel with the same validator report.

`editor.project_health`:

| `action` | Params | Answers |
|---|---|---|
| `state` (default) | | `{ running, status, ran, errors, warnings, notes, cancelled, rows: [{ rank, severity, category, title, detail, action, link, count }], sources: [{ source, ran, note }] }` |
| `run` | `wait?` (block until done), `doctor?` (default true) | the state plus `job` |
| `open` | `row` (a rank) | `{ status, link, copy }`: an asset opens as in the Content Browser, a Problems link filters Problems and brings it forward, a command is answered in `copy` (never run) |

`link` is `{ kind: none }`, `{ kind: asset, path }`, `{ kind: problems, validator }` or `{ kind: command, command }`.

## Error codes

| Code | Where |
|---|---|
| `staged_parse_error`, `staged_unreadable`, `staged_too_large` | finding codes of `regia assets validate --staged` |
| `not_a_git_repo`, `git_not_found`, `git_failed` | `--staged`, `regia hooks`, `editor.precommit_hook` |
| `hook_foreign`, `hook_missing`, `hook_io` | `regia hooks`, `editor.precommit_hook` |
| `no_project`, `precommit_hook_action` | `editor.precommit_hook` |
| `project_health_running`, `project_health_no_row`, `project_health_action`, `project_health_failed`, `job_start_failed` | `editor.project_health` and its job |
| `problem_row_missing` | `editor.problems_panel` |
| `graph_error_missing` | `editor.problems_panel` (`open_graph_error` names no listed material graph error, w13) |
| `F0003` | an entity link whose GUID no entity has |

## Material graph compile errors (w13, P-185)

The Problems panel's third source lists the compile errors of every material graph document open in the editor
(**Material graphs**), each with an **Open** button that activates the tab and selects the node.
`editor.problems_panel` answers them as `graph_errors: [{ index, tab, label, path, kind, target, node, message }]`
and follows one with `open_graph_error: <index>` (answers `opened_graph_error`; `graph_error_missing` for an index
that is not listed, checked before any other change). See [Material graph editor](material-graph-editor.md).
