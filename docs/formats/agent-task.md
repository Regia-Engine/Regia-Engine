# agent-task: agent benchmark task v1

Files: `bench/agent_tasks/<id>.toml` and `bench/agent_tasks/<id>.oracle.jsonl`.
Library: `xtask::agent_bench` (`parse_task`, `parse_oracle`, `evaluate`). Command: `cargo xtask agent-bench`.
Guide page: `book/agents/agent-benchmark.md`. Decision: ADR-2621. Spec: `docs/specs/23-agent-native.md` § 8.

A task is a request for a model, the world it starts in, and the criteria that say it was done. The criteria
are protocol queries judged by predicates, so a run needs no human and no model to be scored.

```toml
format_version = 1
id = "rocks-off-the-road"
title = "Scatter thirty rocks, none on the road"
prompt = "Place 30 rocks on the ground between x = -40 and x = 40. Keep every rock at least 3 m from the road."
features = ["scene", "physics"]
crates = ["regia_agent", "regia_physics"]
call_budget = 12

[setup]
target = "physics"

[[setup.call]]
method = "scene.spawn"
params = { name = "Road marker", components = { Transform = { translation = [0.0, 0.0, 0.0] } } }
save = { road = "/entity" }

[[check]]
name = "thirty rocks"
method = "scene.query_entities"
params = { name = "Rock*", limit = 200 }
pointer = "/entities"
expect = { kind = "count_range", min = 30, max = 30 }
```

| Field | Meaning |
|---|---|
| `format_version` | Must be 1. Another value fails with `format_version N is not supported`. |
| `id` | Lowercase ascii, digits and `-`; equal to the file stem. |
| `title`, `prompt` | One line for the report; the natural-language request the model gets. |
| `features` | Areas the task exercises: `scene`, `terrain`, `materials`, `lighting`, `ui`, `audio`, `nav`, `physics`, `sequencer`, `scripting`. At least one. |
| `crates` | Engine crates whose features it needs. Informational. |
| `call_budget` | The most protocol calls a model may use. The oracle has to fit it. |
| `setup.target` | The headless target to start (below). |
| `setup.call` | Calls that build the starting world, run once before the model. `save = { name = "/json/pointer" }` binds a result value as `"$name"` for later calls. |
| `check` | One or more criteria; every one must hold. |
| `check.method`, `check.params` | The read-only protocol query. |
| `check.pointer` | JSON pointer into the result (`""` is the whole result). |
| `check.expect` | A predicate, below. |

## Predicates

| `kind` | Fields | Holds when |
|---|---|---|
| `equals` / `not_equals` | `value` | The value at the pointer equals (differs from) `value`; `1` equals `1.0`. |
| `count_range` | `min`, `max` | The array (object, string) has `min..=max` items. |
| `number_range` | `min`, `max` | The number is within the range. |
| `within_distance` | `point = [x, y, z]`, `max` | The `[x, y, z]` position is at most `max` m from `point`. |
| `exists` / `absent` | none | The pointer resolves to something (nothing, or `null`). |
| `contains` | `text` | The string contains `text`, or an array item equals it. |
| `image_differs` | none | The value (usually a screenshot's `png_base64`) differs from what it was when the setup ended. |

## Targets

| `target` | The app has |
|---|---|
| `scene` | Minimal plugins, the scene plugin and the agent server: entities, components, spawn and edit, play and expect. |
| `physics` | The scene target plus the physics plugin: bodies, colliders, ray casts, overlaps. |
| `nav` | The scene target plus a flat 8 m by 8 m navigation mesh (x and z from -4 to 4) and its obstacle set. |
| `audio` | The scene target plus the audio plugin on a headless device: buses, emitters, zones. |
| `sequencer` | The scene target plus sequence players (`sequence.player`). |
| `lighting` | The scene target plus the level settings (`level.get_look` / `level.set_look`) and the directional, point and spot light components. |
| `materials` | The scene target plus the material graph library: `material.graph_*`, `material.compile`. |
| `terrain` | The scene target plus the terrain plugin and a flat 64 m by 64 m terrain (65 by 65 samples, 0 to 20 m of height range) at the origin. |
| `ui` | The scene target plus the runtime UI (code-only, no window) hooked to the agent: `ui.*`. |
| `scripting` | The scene target plus script slots, tags, coroutines and the functional test runner (`scripts.*`, `tags.*`, `coroutines.list`, `functest.*`). |

## Oracle files

`<id>.oracle.jsonl` holds one JSON call per line: `{"method": "scene.spawn", "params": {...}, "save": {"rock": "/entity"}}`.
Lines starting with `#` and empty lines are skipped. `"$rock"` (a string that is exactly `$name`) in a later `params` is
replaced by the saved value. The oracle plays after the setup; its length must not exceed `call_budget`.

Migration: none at v1. A shape change needs a `format_version` bump and a migrator (`task_migration_sources()`),
and `cargo xtask check-formats --update` re-records the hash (`docs/specs/12-assets.md` § 4a).
