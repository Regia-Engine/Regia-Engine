# Content closure roots

The chunk cook (`[project] startup_level`, `[cook.chunks.*] entry`) and named build profiles
(`[profiles.*] entry_levels`, `include`) find their roots through one resolver
(`regia_asset_pipeline::closure_roots`, CT-25, ADR-8477). Equivalent declarations ship the same
cooked ids, and every root records why it exists.

## What becomes a root

| `kind` | Declared by | Boot |
|---|---|---|
| `startup_level` | `[project] startup_level` (chunk cook) | yes |
| `entry_level` | `[profiles] entry_levels[<i>]` | yes |
| `startup_ui` | every top-level `ui/*.ui.ron` (the runtime loads them at start) | yes |
| `declared` | `[profiles] include[<i>]` or `[cook.chunks.<name>] entry[<i>]`: explicit dynamic roots | no |
| `level_bake` | the `*.procgen/` bakes of a level the walk reached (`via` names the level) | as the level |
| `reference` | named by a raw file the walk read (`via` names it) | as the file, but a level named by a level is a travel target: not boot |

The chunk cook pins **boot** roots in `base`. With `content = "closure"` it ships every root.

## How raw files are read

Each raw file is read at most once (breadth first, declarations in canonical order):

- **UI layouts** (`*.ui.ron`): the typed UI dependency extractor. Only image and mask `asset` fields
  are references; a node named `textures/x.png` is not.
- **Levels** (`*.level.ron`, the files of `*.level.d/`): every RON string literal, decoded
  (`\"`, `\u{e9}`, `\x41`, raw strings), that names a raw file, a cooked asset path, an asset id
  (`asset:<id>` or bare) or a level id.
- **Unknown text formats**: the generic quoted-string scan. These files are listed under
  `coverage.fallback` and `coverage.complete` is `false`: the closure is conservative there, not proven.
- **Binary files**: not scanned (`coverage.binary`).

## Dynamic roots

Content the game opens by a name built at run time (a level-select screen, a texture picked by code)
is referenced by no file. Declare it with the settings you already have:

```toml
[profiles.demo]
entry_levels = ["levels/a.level.ron"]
include = ["textures/dyn/**", "levels/side.level.ron"]

[cook.chunks.base]
entry = ["textures/dyn/*.png", "levels/side.level.ron"]
```

An `entry` glob that matches only raw files (a level) is satisfied: the level is read and its
content ships. The sample `crates/regia_cli/tests/fixtures/closure_roots/` is this setup.

## Reports

- `regia assets chunks --json`: `roots.records[]` (`target`, `id`, `kind`, `declared_by`, `via`,
  `boot`), `roots.coverage` (`typed`, `binary`, `fallback`, `complete`), `roots.unmatched`,
  `roots.counters` (`files_read`, `bytes_read`, `strings_examined`, `references`, `peak_queue`). The
  text form prints a `roots:` line and each fallback file.
- `regia assets why <asset> --json`: `root` when the asset is a root.
- `regia package --profile <name> --dry-run`: a `roots` section and the root coverage line.
- `editor.content_chunks_state`: `roots` (at most 100), `roots_total`, `root_coverage`.

## Error codes

| Code | Meaning |
|---|---|
| `content_root_missing` | A declared startup or entry level is neither a raw file nor a cooked asset. |
| `content_root_malformed` | A level or UI layout in the closure does not parse (bad RON tokens, not UTF-8, a layout field of the wrong type). |
| `content_root_unreadable` | A raw file in the closure cannot be read or listed. |
| `content_root_limit` | A raw file in the closure is larger than 256 MiB. |

`regia cook` also prints a `content_root_fallback` warning (not an error) when any raw file needed
the fallback scan.

The four codes above fail `regia cook`, `regia package`, `regia assets chunks` and the Content Chunks panel; none
becomes a smaller closure. The CLI JSON error (`error.code`) and the agent package job report keep
the code; `problems[0].root_path` names the file.

## Verify it

```json
{"method": "play.wait_until", "params": {"condition": {"all": [
  {"events_contain": {"kind": "closure_roots", "contains": "[cook.chunks.base] entry[0]"}},
  {"events_contain": {"kind": "closure_roots", "contains": "config/spawn.table"}}
]}, "timeout_ms": 10000}}
```

`cargo test -p regia_cli --test closure_root_parity` runs this against the sample's `roots` report
and follows it with `play.expect`.
