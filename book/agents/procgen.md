# Procedural generators: graphs, runs, reports, bakes

A **procedural graph** (`.pgraph.ron`) turns rules into placed content: nodes read the world (the
generator's bounds, the terrain, tagged entities, data tables), make and refine **point sets** with
attributes, and spawn mesh instances, prefabs or data. A **generator** — the
`regia_procgen::world::ProcGenerator` component on a level entity — binds a graph, parameter
overrides and a seed, and runs the graph over its bounds. The same graph, parameters, seed and inputs
give the same output bytes on Linux, Windows, Android aarch64 and macOS (specs/62 § 6).

The feature lives in `regia_procgen` (`ProcGenPlugin`). Without the plugin every method answers
`F0003` with `data.reason: "procgen_unavailable"`.

## Run a generator and read its report

```json
procgen.list {}
→ { "generators": [{ "entity": 4294967301, "guid": "…", "graph": "procgen/rocks.pgraph.ron",
      "mode": "baked", "state": "ready", "run_id": 3, "output_hash": "73b3…", "stale": false,
      "output": { "instances": 1742, "instance_sets": 2, "entities": 0, "tables": 0 } }] }

procgen.run { "entity": 4294967301, "wait": true }
→ { "run_id": 4, "state": "ready", "output_hash": "73b3…", "counts": { "instances": 1742, … },
    "report": { "nodes": 8, "ms": 3.1, "cache_hits": 0, "inactive": 0, "warnings": [] } }
```

Without `wait` the run is queued (`{ "run_id": 5, "state": "queued" }`); a run already in flight is
cancelled first, so `procgen.cancelled` for the old run precedes `procgen.started` for the new one.
A scheduled generator (`Anchored` or `Baked`, with `tiles.runtime`) has no whole-graph run (spec 66
§ 4.2a). `procgen.run` re-queues its resident, applying and failed cells and answers the current
`run_id` and `state`, plus `scheduled: true`, `run_requested: false` and `requeued`
(`[[cell x, cell z, tier]]`). `wait` does not block until those cells finish.
States: `idle`, `queued`, `capturing`, `running`, `applying`, `ready`, `failed`, `cancelled`, `cleared`.
A failed or cancelled run leaves the previous output in place. `procgen.run` also takes `params`
(overrides stored on the generator; JSON values, `{"ron": "…"}` for enum variants) and `seed`. With `wait: true`,
`threads` picks the schedule of that run: 0 is serial, `n` a pool of `n` workers, absent the compute pool; the output
hash is the same on every schedule (a bad value answers `procgen_param_type`).

`procgen.get { entity }` shows the component fields and the **effective** parameters with their source
(`generator`, `variant:<path>` or `default`). `procgen.cancel` and `procgen.clear` stop a run or remove
the output (spawned entities and instance sets).

`procgen.report { entity }` lists every node of the last run: `id`, `ty`, `ms`, `items`, `elements`,
`cache_hit`, `inactive` (skipped because a `flow.if` branch was not taken), `bypassed` (`enabled:
false`), `warnings` and `counters` (for example `unmatched` from `spawn.mesh ByAttr`).

## Inspect a node's points

Run with `keep_debug: true`, then read any node's output:

```json
procgen.run { "entity": 4294967301, "wait": true, "keep_debug": true }
procgen.inspect { "entity": 4294967301, "node": 2, "limit": 3, "attributes": ["position", "density"] }
→ { "items": [{ "pin": 0, "kind": "Points", "tags": [], "len": 2500,
      "columns": [["position", "Vec3"], ["density", "Float"]],
      "rows": { "position": [[-98.0, 1.2, -98.0], …], "density": [1.0, 0.8, 1.0] } }] }
```

`pin` and `item` narrow to one output pin or item; `offset`/`limit` page the rows (at most 10 000).

## Edit a graph

`procgen.node_types { category? }` lists the node library (type id, family, doc line, pins with the
kinds they take, settings with type, default, range and enum options). `procgen.graph_get { path }`
reads a graph with its compile errors and graph hash. `procgen.graph_edit { path, ops, save? }`
applies ops in order — all or none, and the result must compile:

| Op | Fields |
|---|---|
| `add_node` | `ty`, `settings?` (object of JSON values), `at?` `[x, y]`; answers the new id in `added` |
| `remove_node` | `id` (its edges go too) |
| `set_setting` | `id`, `name`, `value` (`{"bind": "param"}` binds a parameter; `{"ron": "ByAttr(attr: \"kind\")"}` is literal RON) |
| `connect`, `disconnect` | `from: [id, "pin"]`, `to: [id, "pin"]` |
| `set_enabled`, `set_seed_salt`, `move` | `id` and `enabled` (a boolean) / `salt` (a u32; missing, fractional or above 4294967295 is `procgen_param_type` and the graph is unchanged) / `at` |
| `add_note` | `text`, `at?` |
| `add_param`, `set_param_default`, `rename_param` (updates every `Bind`), `remove_param` | `name`, `ty`, `default`, `min?`, `max?`, `doc?` / `value` / `from`, `to` |
| `set_graph_pins`, `set_flags`, `set_description` | `inputs?`, `outputs?` / `starter?`, `tool?`, `domain?` / `text` |

Without `save` the edited graph is kept as an unsaved overlay that generators read at once; `save:
true` writes the file (desktop tools only: the `pipeline` feature).

```json
procgen.graph_edit { "path": "procgen/rocks.pgraph.ron", "ops": [
  { "op": "add_node", "ty": "points.jitter", "settings": { "offset": [[-1, 0, -1], [1, 0, 1]] } },
  { "op": "connect", "from": [2, "out"], "to": [9, "in"] } ] }
→ { "graph_hash": "…", "errors": [], "added": [{ "op": 0, "id": 9 }], "saved": false }
```

Outside the engine, `regia procgen graph check <file> [--project DIR] [--json]` compiles a graph or variant the same way
and prints every error (exit code 1 on errors).

## Bake and verify

`Baked` generators (the default mode) store their output in the level's bake directory as
`<generator guid>.pgout` and load it with the level instead of running. `procgen.bake { entity? }`
runs them and writes the files atomically; `procgen.verify { entity? }` re-runs them without
publishing and compares each output hash with its file (`{ ok, mismatches: [{ entity, file_hash,
run_hash, file_error? }], findings: [...] }`; `findings` lists entities that lost an anchored generator, see
[Placed levels](#placed-levels-streaming-cells-and-sublevels-adr-7262)). A hand-edited file fails to load (`pgout_corrupt` or another `pgout_*`
code) and fails verify.

Headless, from a shell or CI: `regia procgen bake <level>` runs a level's `Baked` generators (in guid order)
and writes the same `.pgout` files as `procgen.bake`, into `<level>.procgen/` (`--out DIR` overrides;
`--project DIR` resolves graph paths, default the current directory). To bake one generator pass its guid
or entity name as the target with `--level <level>`. `regia procgen verify <level>` re-bakes into a
temporary directory (removed afterwards, the bake directory is never written) and compares bytes with the
files already there; a file that differs (`changed:`), one the re-bake writes but the directory lacks
(`missing:`) and one in the directory no generator writes (`extra:`, tagged `orphan_bake`, whole-level
verify only) are printed, one per line, and the exit code is 1. `regia procgen bake <level> --prune` bakes
the whole level and then deletes the orphan `.pgout` files: those whose name is not a bake name, or whose
guid no entity of the level (or of a level it places, or of a prefab instance in it) holds. A file whose
entity is still in the level stays and is printed as `kept <file>: <reason>`: an entity that lost its anchored
generator keeps its cell bake (re-add the generator, or remove the entity first), and so does an entity whose
generator is not `Baked`. While the level names a placed level or a prefab template the project does not
have, prune deletes no guid-named file (it may belong to the missing content); the same holds while a prefab
instance of a loaded level has no members in the session: its record did not spawn (its data layer starts
`Absent`, or its placement failed), it still waits for a template, or its instantiation failed. Their
members' bakes are named by guids the session cannot see, so each is printed as the `kept` reason
(`prefab instance <guid> did not spawn ...`, `... waits for prefab <guid>`, `... failed to instantiate`). Other files and `detached/`
stay. `regia procgen bake <level> --prune --dry-run` prints `would prune` and `kept` lines and the count, and
neither bakes nor deletes anything; `--dry-run` needs `--prune`. `--prune` is refused with a single
generator, and `verify --prune` is refused (verify never deletes); an `extra:` line of verify says whether
prune would remove or keep the file. The CLI loads the prefab templates the level's instances need from the
project's `.prefab.ron` files (by their `.meta` id), so a `Baked` generator inside a template bakes and
verifies under the guid the instance gives it, the file the game reads. Both read the level in either form (`.level.ron` or `.level.d`).
The CLI world has no physics and no terrain plugin: graphs that cast rays against physics see no hits, as
in `procgen.bake` on a world without physics.

A `.pgout` (version 4) holds generated meshes, terrain edit records (rectangle, new and original samples), road
decals, each instance set's data layer (`target_layer`) next to the instance sets, and the `spawn.prefab` overrides
of each prefab record (ADR-7750), so a loaded bake draws its roads, applies its terrain edits and spawns its prefab
instances with their overrides without running the graph. `regia upgrade-project` rewrites older files (a version 3 file
has no set layers and keeps its hashes); a version 2 bake of a graph with generated meshes or terrain edits never held
them, fails its hash check (`pgout_corrupt`) and needs a re-bake.

A tiled generator writes one section per cell. `procgen.bake { entity, cells: [[i, j]], tier? }` re-bakes those cells:
every other cell's bytes are copied unchanged and the file is replaced atomically. A generator in `Baked` mode with
`tiles.runtime` set publishes nothing whole: its cells load from the file around anchors (see "Tiles and anchors").

## Drawing

Instance sets are drawn as instances of one mesh each: the host registers meshes by asset path in
`regia_procgen::draw::ProcMeshLibrary` (`insert(path, mesh, material)`), and every frame the instances within an entry's
`draw_distance_m` are added to the frame (the last `fade_m` meters fade out). `cull_below_px` is stored on the
set, included in its hash and written to `.pgout`; drawing does not read it. `procgen.list` answers
`draw: { drawn, culled, missing_meshes, generated_meshes, generated_triangles, missing_materials }` for the last frame;
a mesh path in `missing_meshes` has no library entry and is not drawn. Check a generator visually with
`seeing.screenshot` after `procgen.run { wait: true }`.

Generated meshes (ribbons, junction patches, markings, spline-deformed pieces) draw without a library entry: each one
uploads once per published output, in world space. Materials come from `regia_procgen::draw::ProcMaterialLibrary`
(`insert(path, material)`), which instance-set `material` overrides read too; a path with no entry draws with the default
material and is listed in `missing_materials`. `spawn.spline_mesh { collision: mesh }` also builds a static mesh collider
per generated mesh under the output root.

## Detach

`procgen.detach { entity }` (editor only, `ProcGenSettings::editor`) turns the current output into ordinary level
content: spawned entities lose `Generated` and get guids so the level saves them, each instance set becomes an
`InstancedMeshSet` entity (a saved component, `procgen-instanced-set` v1) under the root, each generated mesh becomes a
mesh entity whose `.glb` goes to `<bake dir>/detached/` (`MeshSource` names it), terrain edits stay applied as ordinary
terrain content, the generator's `.pgout` is deleted and its output cleared. It answers `{ root, entities, instance_sets,
meshes, unsaved_meshes }` and sends `ProcGenDetached`; `unsaved_meshes` counts meshes with no file (no bake directory, or
the host installed no `ProcMeshWriter`): they draw this session and are not saved, and the `.pgout` stays, as their only copy.
A `MeshSource` path is relative to the project when the bake directory is inside it. Outside the editor it answers
`procgen_editor_only`, and with nothing published `procgen_no_output`.

Bakes live in `ProcGenSettings::bake_dir`. The editor keeps it on the open level (`<level path without extension>.procgen`, from
`regia_procgen::world::level_bake_dir`), so an unsaved level answers `procgen_no_bake_dir`. `regia run --level L` sets it to `L`'s,
over any value the game set in `Startup`; otherwise a game sets it itself when it opens a level, because the level manager keeps no
file paths. The editor also loads the meshes and materials a generator asks for into the libraries, the first part of a model with
several.

The cook puts every shipping `.pgout` into the pak (byte for byte, type `regia_procgen::pgout::PgoutBytes`, ADR-7214), and
`regia package` ships no loose bake. The loader reads a bake file from disk when it is there, else from the paks through the
asset server, so a packaged game (Android included, which reads nothing loose from its APK) loads its baked generators. An
`editor_only` generator's bake is not cooked. A bake that fails to decode fails the cook with `procedural bake error:` and
its `pgout_*` code.

### Placed levels, streaming cells and sublevels (ADR-7262)

`regia procgen bake --level sub.level.ron` writes a sublevel's bakes beside the sublevel, under its generators' own guids.
A build profile ships the bakes of every level its closure reaches, not only of its entry levels: a level placed by asset
id, a streaming cell or sublevel a world file names, any level a raw file names. `editor_only` bakes still stay out.

At runtime a generator that a placed level (inline or streamed) or another additive level brought in loads its bake from
the primary directory first (the editor may have baked the host level with the placement filled, under the member's
derived guid), else from its own level's directory: an inline member's source guid is found among the placed level's
records, and a streamed or additive member keeps its own. The level directories come from
`regia_procgen::world::ProcLevelBakeDirs`, which the `regia` default plugins fill from every level `load_level_source`
read. A host that loads levels another way inserts `level id -> level_bake_dir(path)` itself; a level missing there is not
guessed, so the generator stays `idle` with `procgen_bake_missing`.

`procgen.list` and `procgen.get` show where each loaded bake came from, and `procgen.list` names the levels whose bakes
loaded, so you can check that a package carries a sublevel's bakes:

```json
procgen.list {}
→ { "generators": [{ "guid": "…", "state": "ready",
                     "bake": { "path": "assets/levels/sub.procgen/….pgout", "level": "…", "source": "pak" }, … }],
    "bake_levels": [{ "level": null, "dir": "assets/levels/main.procgen", "disk": 0, "pak": 4 },
                    { "level": "…", "dir": "assets/levels/sub.procgen", "disk": 0, "pak": 1 }], "draw": { … } }
```

`bake` is `null` for a generator whose output did not come from a bake (a live run, or nothing loaded). In `bake_levels`,
`level: null` is the opened level's directory (`ProcGenSettings::bake_dir`). A packaged game reads every bake from the
pak (`pak` counts), a dev run from disk.

The same rule covers anchored generators (ADR-7287). A `Baked` tiled generator with `runtime: Some(...)` inside a placed
level streams its cells from the sublevel's `<source guid>.pgout`, on disk or in the pak. A generator that a prefab
instance's `added_children` brought into the sublevel maps back to its source guid too: an inline placement derives its
guid the same way. Where the host level has its own file for the member (under the member's guid), that file wins.
`bake.dir` names the directory the file was read from, and `bake.kind` is `whole` for a published output or `cells` for
a generator streaming its cells:

```json
procgen.get { "entity": 4294967310 }
→ { "mode": "Baked",
    "bake": { "path": "assets/levels/sub.procgen/….pgout", "dir": "assets/levels/sub.procgen",
              "level": "…", "source": "pak", "kind": "cells" }, … }
```

A generator inside a prefab template itself (a prefab-internal entity, not an added child) maps back too (ADR-7331): its
guid derives from its prefab instance's guid and its local id, so the loader finds the instance's source guid and derives
the guid the same generator has in the sublevel, the one `regia procgen bake --level sub` names its file after. Nested
prefabs map through each instance in turn.

**Re-bake and verify use the same directories (ADR-7331).** `procgen.bake`, `procgen.verify`, `regia procgen bake` and
`regia procgen verify` of a placed or additive member write and compare its file where the loader reads it: the host
level's own file under the member's guid when it exists, else the source level's directory (from `ProcLevelBakeDirs`)
under the source guid. `procgen.bake` lists that path in `files`. A `Baked` inline member with no pinned seed runs with
its source guid's seed there, so its re-bake writes the same bytes as the sublevel's own bake. The CLI loads the levels
a level places (as the game does) and registers their directories; `regia procgen verify` re-bakes into scratch copies
of every directory involved and names a member's stale file with its full path:

```text
$ regia procgen verify assets/levels/main.level.ron
changed: assets/levels/sub.procgen/….pgout (2104 bytes on disk, 2104 bytes re-baked)
```

**Levels that lost an anchored generator.** Before ADR-7287 (5), saving a level from the editor dropped the
`ProcGenerator` of every anchored generator; the record stayed, and so did its cell bake. `procgen.verify` (without
`entity`) and `regia procgen verify` name each entity with a cell bake (in the primary directory under its guid, or in its
level's directory under its source guid) but no generator. Nothing is rewritten: add the generator again and re-bake.

```json
procgen.verify {}
→ { "ok": false, "mismatches": [],
    "findings": [{ "code": "anchored_generator_missing", "entity": 4294967311, "guid": "…",
                   "level": "…", "path": "assets/levels/sub.procgen/….pgout", "message": "…" }] }
```

`level` is the level whose directory held the file (or the entity's own level; `null` without one). The CLI prints
`anchored_generator_missing: <guid> in <level path>: <file> …` instead of calling the file `extra:`. An anchored
generator whose cells were never baked leaves nothing to find.

## Runtime: editor-only, on load, per tier, collision, file changes

These settings of `ProcGenerator` act at runtime (ADR-2421):

- **`editor_only: true`.** Outside the editor (`ProcGenSettings::editor` false) the generator never
  runs (its run fails with `procgen_editor_only`, and `procgen.run` answers `F0002` with that reason),
  and its bake is not loaded. A bake or verify (a tool) still runs it. `regia package` leaves the
  generator, its descendants and its `.pgout` files out of the staged level (`regia_procgen::cook`).
  `procgen.get` reports `editor_only_refused`.
- **`mode: OnLoad`.** Outside the editor the generator runs once when it spawns with a level, at the
  running quality tier. The **level-load gate** waits for these runs and for anchored generators with
  `tiles.runtime.wait_on_load` (until their cells around the anchors are resident). The gate closes
  with the `ProcGenLoadFinished` message and a report; a load that takes more than
  `ProcGenSettings::on_load_timeout_frames` frames (default 600) closes with `timed_out` and names the
  late generators with `procgen_on_load_timeout` (an error in the log; the late run still publishes when
  it finishes). `procgen.load_report {}` answers `{ pending, waiting: [{ entity, guid, mode }], open,
  last }`; a report is `{ level, ok, timed_out, frames, ms, generators: [{ entity, guid, mode, state,
  frames, ms, reason }] }`. A loading screen (game code: `ProcGenLoadGate::pending()`) keeps showing
  after `LevelLoaded` until the gate closes; an agent polls `procgen.load_report` until `pending` is
  false and then reads `last`.

- **`quality_tier: Some(PerTier)`.** A bake writes one file per tier of `ProcGenSettings::bake_tiers`
  (`Regia.toml` `[procgen] bake_tiers = ["MobileLow", "High"]`, default all seven; parse names with
  `parse_bake_tiers`), named `<guid>.<Tier>.pgout` (`<guid>.High.pgout`). The loader takes the running
  tier's file, else the nearest lower tier, else the nearest higher one; `procgen.get` reports
  `baked_tier`. `procgen.bake` lists the `files` it wrote; `procgen.verify` checks every tier and
  reports `tiers: [{ tier, ok }]`. Baking with a single tier again removes the per-tier files. A
  generator that streams its baked cells around anchors bakes one tier (`procgen_per_tier_streaming`).
- **`spawn.mesh` `collision: Simple` or `Mesh`.** The output gets static colliders under its output
  root (removed with it): one compound of boxes (each instance's mesh bounds from the host's
  `ProcMeshLibrary` through its transform; a unit box when the library lacks the mesh, logged as
  `procgen_collision_mesh_missing`) and one triangle mesh of every `Mesh` instance whose geometry the
  library has. Loaded bakes and anchored cells build them too. Each carries `ProcInstanceCollider`;
  `procgen.get` lists `colliders: [{ kind, instances }]`. When `ProcMeshLibrary` changes (it appears,
  goes away, or gains, loses or replaces a colliding mesh) the colliders of the outputs whose colliding
  meshes changed are rebuilt without a re-run (`refresh_instance_colliders`, v0.5.1), as the drawn
  instance lists are: an output applied before the editor loaded its meshes collides as unit boxes
  only until they load.
- **Files.** A run records the content hash of every file it read (graph, variants, subgraphs, data
  tables, road profiles, biome files); every `ProcGenSettings::file_check_frames` frames (default 30)
  a changed hash marks the generator stale, and `live` (editor) or `refresh_on_change` (runtime)
  generators re-run. At runtime files are read only for `refresh_on_change` generators (another generator that reads the same file is marked stale too). `procgen.get` lists `files: [{ path, hash }]`.
- **`bounds: FromShape` / `FromSpline { margin_m }`.** The run region is the entity's own `Shape` in
  world space, or the world box of its own `Spline` grown by the margin; editing that shape or spline
  marks the generator stale. Without one the run fails with `procgen_bounds_missing`. `procgen.get`
  reports `bounds: { min, max }` and `bounds_problem`.
- **Level changes.** `regia` moves `bake_dir` to the bake directory of each level opened with
  `LoadMode::Replace` after `load_level_source`; hosts that open levels another way call
  `regia_procgen::world::follow_level`. Generators that never ran look for their bake again in the
  new directory, and a baked streaming (anchored) generator that reported `procgen_bake_missing` or
  `procgen_no_bake_dir` opens the new directory's file (v0.5.1); the file appearing in the same
  directory (an external copy or sync) is picked up on a later update the same way.
- **Layers.** The output root copies the generator's `LayerMembership` when an output is applied
  (spec 62 § 18, ADR-1585; v0.5.1), so a generator in a `Parked` or `Absent` layer applies its output
  parked: the root, its prefab instances and its colliders carry `LayerParked` (out of physics and
  picking), and its instance sets are not drawn while the output's entity (the generator, or a tile
  cell's root) is parked. Switching the layer live brings them back. A root in a layer has a guid derived
  from its generator (or cell), so runtime switches (`LayerTable::request` + `apply_layer_switches`) park
  and unpark it, its prefabs and colliders with the generator, both ways, and a cell streamed in while
  the layer is off arrives parked. Moving a generator to another layer (or joining one after the
  apply) moves the applied root with it on the next update, without a re-run; a generator that leaves
  every layer keeps its applied root until the next apply. A generator without an `EntityGuid`
  has none to derive its root's guid from: the root's guid derives from the generator's entity id
  instead, mixed in before a tile cell's scope, so two such generators' roots (whole outputs and tile
  cells alike) never share one guid (the guid holds for the session; level
  generators always have an `EntityGuid`). When the generator goes away (an `Absent` switch despawns one that
  has a level template record) its output roots and resident cells are despawned with it; the switch
  back respawns the generator, which applies a new output (ADR-7556).

## Error codes

Errors answer `F0002` (invalid params) or `F0003` (unknown entity, node or path) with `data.reason`:

| `data.reason` | Meaning |
|---|---|
| `procgen_unavailable` | `ProcGenPlugin` is not installed |
| `procgen_not_generator` | The entity is missing or has no `ProcGenerator` |
| `procgen_unknown_graph` | The graph or variant file cannot be read |
| `procgen_bad_path` | `graph_get`/`graph_edit` `path` is absolute (`/x`, `\\server\share`), holds a drive letter or `:`, a `..` segment, a NUL byte or a device name (`CON`, `NUL`, ...), or (on save) leads out of the project through a symlink; the message names the sub-code (`path_absolute`, `path_prefix`, `path_parent`, `path_nul`, `path_device`, `path_empty`, `path_escapes`, `path_io`) |
| `procgen_graph_invalid` | The graph does not compile; `data.errors` lists every problem as `{ node, reason, message }` |
| `procgen_unknown_node`, `procgen_unknown_pin` | No such node id or output pin (inspect, graph ops) |
| `procgen_unknown_param`, `procgen_param_type` | Parameter op on a missing or invalid name; a value of the wrong type (`procgen.run` `params` the graph rejects are not stored, and `data.errors` lists the compile errors) |
| `procgen_run_failed` | The run failed; `data.node` and `data.errors[0].reason` name the node and its code |
| `procgen_limit_exceeded` | The run hit a limit (`limit_exceeded`) |
| `procgen_no_debug_data` | `procgen.inspect` before a run made with `keep_debug: true` |
| `procgen_editor_only` | A file write in a build without the `pipeline` feature, `procgen.detach` outside the editor, or `procgen.run` on an `editor_only` generator outside the editor |
| `procgen_no_output` | `procgen.detach` on a generator with nothing published |
| `procgen_wrong_domain` | `set_flags` with an unknown domain |
| `procgen_param_type` | `procgen.set_debug` with a flag that is not a boolean |

Codes inside `data.errors` and run failures come from the compiler and the nodes: `unknown_node_type`,
`unknown_setting`, `setting_type`, `setting_out_of_range`, `unknown_param`, `param_type_mismatch`, `param_name` (a parameter name outside `[a-z][a-z0-9_]{0,63}`),
`unknown_pin`, `pin_kind_mismatch`, `pin_too_many_edges`, `pin_unconnected`, `graph_cycle`,
`node_wrong_domain`, `setting_migration`, `setting_version_newer` (node settings written by a newer build), `attr_missing`, `attr_type_mismatch`, `attr_reserved_name`,
`attr_type_conflict` (an attribute of one name has different types in one point set: inside a
node, or across two cells of a tiled output or a bake being merged — the run and the decode fail
naming both cells and the attribute), `attr_exists`, `attr_nan`, `input_nan` (a data table cell, `in.table`), `not_spatial`, `limit_exceeded`, `check_failed`,
`stamp_count_mismatch`, `table_missing`, `table_row_missing`, `table_ragged`, `override_path_unknown`,
`unknown_graph`, `procgen_unknown_graph`, `loop_count_mismatch`, `mesh_bad_index`, `not_in_loop`, `generator_cycle`, `cancelled`, `terrain_edit_invalid` (a record does not fit the terrain: the run fails and the old output stays). `terrain_edit_overlap` (another generator's applied edit owns one of the same samples: the run fails, nothing is written, and clearing that generator lets it run). Bake codes: `procgen_no_bake_dir`, `procgen_not_baked` (bake or verify on a generator whose mode is not `Baked`), `procgen_bake_missing`, `pgout_magic`,
`pgout_version`, `pgout_truncated`, `pgout_corrupt`, `pgout_io`, `orphan_bake` (CLI
verify: a `.pgout` no `Baked` generator writes). Graph and variant file codes: `pgraph_parse`, `pgraph_shape`,
`pgraph_version_newer` (a file written by a newer build is refused), `pgvar_parse`, `pgvar_shape`,
`pgvar_version_newer`, `variant_cycle`,
`variant_chain_too_long` (more than 8 variants above a graph). Verify finding: `anchored_generator_missing` (an entity
has a cell bake but no `ProcGenerator`, ADR-7331). Warnings (in reports): `param_clamped`,
`variant_unknown_param`, `output_unreachable`, `no_terrain`, `no_self`, `no_spawn_weight`,
`div_by_zero`, `index_ignored`, `max_points`, `column_reserved` (`points.from_table` skipped a column with a built-in or reserved name), `export_missing`, `mask_missing`, `mesh_missing`, `junction_sharp`,
`flatten_limit`, `output_name_replaced` (a called graph published a table, point set or mask name the run already
had; the last write wins).
Runtime codes (ADR-2421): `procgen_bounds_missing` (a run of a `FromShape`/`FromSpline` generator without
its shape or spline), `procgen_on_load_timeout` (in a load report and the log), `procgen_cell_failed` (an anchored
generator's cell failed while the load waited), `procgen_per_tier_streaming`, `procgen_unknown_tier`,
`procgen_collision_mesh_missing` (log).
In the log only: `procgen_terrain_mismatch` (the ground under a loaded edit is neither its original nor its result; re-bake),
`procgen_detach_mesh_unsaved` (a detached mesh could not be written).

## Splines, polygons and fences

Spline items are world-space splines (specs/64 § 2; see [Splines and shapes](splines.md)); polygon
items are rings in a horizontal plane, outer rings counter-clockwise, holes clockwise.

- `sample.spline` places points along a spline (`by: step | count | per_segment`, `fit_ends` makes
  both ends land on a point), across it (`place: band`, `band_m` times the width scale) or inside a
  closed one (`place: inside`). Rotation is the spline frame (local X along the tangent, Y up). It
  writes `along`, `distance_m`, `key`, `tangent`, `curvature`, `turn_deg` and the spline's own values.
- `spline.from_points`, `spline.simplify`, `spline.split` (item scope `piece`), `spline.intersect`
  (crossings as points with `a`, `b`, `key_a`, `key_b`; `insert` adds control points),
  `spline.offset` (positive = right; loops removed), `spline.reverse`, `spline.clip` (pieces inside
  or outside polygons).
- `polygon.from` (convex hull or ordered ring of points, or a closed spline), `polygon.bool`
  (`union | intersection | difference | xor`), `polygon.offset` (`miter | round | square` joins),
  `polygon.cut` (split along cutter splines; the removed band is `gap_m` wide and at least 1 mm, pieces ordered by centroid),
  `polygon.to_spline`. Booleans and offsets run on a 1 mm integer grid, so results are the same on
  every platform; a coordinate more than 2 147 km from the polygon's origin fails with
  `polygon_range`.

`spline.modules` fills each spline with modules (meshes or prefabs with a length):

```ron
(
    modules: {
        "post":  (mesh: "fence/post.glb",  length_m: 0.2),
        "panel": (mesh: "fence/panel.glb", length_m: 2.0, stretch: (0.85, 1.15)),
        "gate":  (prefab: "fence/gate.prefab.ron", length_m: 3.0),
    },
    pattern: "post repeat(panel post) first(gate, panel) post",
    fit: Stretch,   // Stretch | Scale | Gap
)
```

Pattern terms: a module name; `repeat(seq)` (as many as fit), `repeat(seq, n)`, `first(a, b, …)` (the
first alternative that fits what is left), `pick(a: w, b: w, …)` (weighted; the draw comes from the
spline's seed and the term's position, so a spline always gets the same choices) and `gap(m)`.
`pattern_from` reads the pattern from an item-scope Name instead. The leftover length is spread over
stretchable modules within their range (`Stretch`, rest as gaps), over every module (`Scale`) or as
equal gaps (`Gap`). Output: one point per module at its start with `module`, `mesh` or `prefab`,
`length_m`, `index` and `distance_m`, and `scale.x` (or `scale.z` with `forward_axis: Z`) = the
stretch; feed it to `spawn.mesh` / `spawn.prefab`, or to `spawn.spline_mesh` (with the spline on its `spline` pin) to
bend each module along the curve (there is no separate `deform` setting, ADR-5645).

Codes: `spline_degenerate` (warning: fewer than 2 points or no length), `spline_not_closed`,
`spline_too_many_points`, `polygon_range`, `pattern_syntax` (the message names the column),
`pattern_unknown_module`, `pattern_nesting` (`repeat` nests one level deep), `pattern_too_large`
(the counted repeats expand past 400 000 terms, `MAX_EXPANDED_TERMS`; a `pick` or `first` of k
alternatives counts 1 + all k per copy, so a counted repeat of a pick can be refused below 100 000
resolved modules: a compile error for a literal
`pattern`, a run failure for a `pattern_from` pattern, checked before anything is built; v0.5.1),
`pattern_too_long` (warning: the spline gets the first module only).

`spawn.spline_mesh` bends each point's `mesh` (from `spline.modules`) along its spline from
`distance_m` over `length_m` (`forward_axis`, `segments_per_m` extra subdivision, `smooth_normals` across
seams, `uv_along`, `material`). `road.ribbon` builds the carriageway and shoulders of each road from a
road profile (`profile`: a `.road.ron` path the host captures, or an inline `RoadProfile(...)`; an item-scope
`road_profile` wins): rows every `1 / samples_per_m` meters plus every control point, crowned by
`cross_slope_pct`, smoothed over `smooth_rows`, never below ground + `surface_lift_m`; `out_edges` gets both
outer edges as points (`side`, `distance_m`). A profile with a `shoulder_material` draws its shoulders as a second
generated mesh. `road.markings` lays the profile's markings (`solid` or `dash(on_m, off_m)`, offset, width, `color` as
vertex colors) 5 mm over the ribbon surface as one generated mesh per road (node setting `material`; use the same
`smooth_rows` as the ribbon). `road.exclusion` gives a polygon per road (carriageway, shoulders, `flatten_extra_m` +
`extra_m`) for `region.subtract`. The mesh nodes write **generated meshes** into the run output (world space, merged per
material up to 65 536 vertices; `procgen.inspect` shows them as Mesh items). Code `road_profile_missing` when neither the
item nor the node names a profile.

### Roads and fences

The starter graph `regia_procgen::road::STARTER_ROAD_NETWORK_GRAPH` (`starter/road_network.pgraph.ron`) reads splines tagged
`road` through `road.junctions` into `road.ribbon` and `road.markings`.

A road profile (`<name>.road.ron`, format `road` v1) with every field (the spec 64 § 7.1 example). A field left out takes
the value shown, except `material` (empty), `shoulder_material` (`None`: the carriageway's), `pieces` and `markings`
(none) and `terrain.flatten` (`false`):

```ron
RoadProfile(
    format_version: 1,
    width_m: 7.0,                              // carriageway
    shoulders_m: (0.5, 0.5),                   // left, right
    surface_lift_m: 0.02,                      // above the terrain
    cross_slope_pct: 2.0,                      // crown fall from the center line
    material: "materials/asphalt.material.ron",
    shoulder_material: Some("materials/gravel.material.ron"),
    uv_m_per_tile: 8.0,                        // V = distance along the road / this
    samples_per_m: 0.5,                        // ribbon rows per meter (plus a row at every control point)
    pieces: [ (mesh: "road/curb.glb", side: Left, offset_m: 3.5), (mesh: "road/curb.glb", side: Right, offset_m: 3.5) ],
    markings: [ (pattern: "dash(3, 9)", offset_m: 0.0, width_m: 0.15, color: (1, 1, 1)) ],
    terrain: (flatten: true, flatten_extra_m: 1.0, falloff_m: 6.0, carve_max_m: 3.0, fill_max_m: 3.0),
    junction: (margin_m: 1.0, material: None),
)
```

A road entity is a `Spline` with a `RoadOf { profile: "<path>.road.ron" }` component and the tag `road`. The editor's road
tool binds its own preset graph `tool_road.pgraph.ron` (specs/67 § 5), which uses the same nodes.

- `road.junctions` (pins `splines`, `ground`; outputs `out_meshes`, `out_splines`) finds crossings in XZ and T-ends (a
  spline end within half the other road's total width plus `junction.margin_m` plus `snap_m`). Meets whose cut zones touch
  are one junction. Each road is cut back by half the other road's total width (carriageway and shoulders) plus its own
  `margin_m`; the pieces go to `out_splines` (item scope `piece`) for `road.ribbon` and `road.markings`. The patch uses
  the pieces' end rows, computed by the ribbon's own code, so its boundary is the ribbons' vertices: a fan from the rows'
  centroid closed to the convex hull, with planar UVs and `junction.material` (else the road's). Set `smooth_rows` to the
  ribbon's. Arms closer than `min_arm_deg` warn `junction_sharp`.
- `road.decals` (`max_error_m`, `color`) converts each road into segments for a host that draws roads in the terrain
  shader: they are in the run output as `road_decals` (`ProcOutput`, baked) and on `out` as a table (`a_x`, `a_z`, `b_x`,
  `b_z`, `half_width`).
- `procgen.report` answers `roads: [{ road, length_m, junctions, flatten_rect, flatten_limit_at_m }]` and
  `totals.junctions`.

`terrain.flatten_road` (bake only) brings the ground under each road (profile `terrain`: `flatten`, `flatten_extra_m`,
smoothstep `falloff_m`, `carve_max_m` / `fill_max_m`) to the ribbon surface minus the lift. It fails with
`flatten_runtime_forbidden` outside a `Baked` generator or with anchors, and `terrain_edits_not_allowed` unless the generator
has `allow_terrain_edits` and the project sets `[procgen] terrain_edits = true` in `Regia.toml`
(`ProcGenSettings::terrain_edits`); warning `flatten_limit` names where the limits stop it.

The edit lands when the run publishes: the previous edit's originals go back, the new samples are written with
`regia_terrain::edit::apply_height_patch` (GPU re-upload and re-cook are noted), and `ProcTerrainApplied` on the generator
remembers the records. A run captures the terrain with its own edit lifted out, so repeated bakes never accumulate and
`procgen.verify` compares against the original ground; a failed or cancelled run leaves the published ground in place.
`procgen.clear` restores every sample bit for bit. A generator's own edit does not mark it stale; other edits do
(`ProcTerrainEdited::source`). A loaded bake applies its records, as soon as the terrain exists; a terrain saved already
flattened is left as it is. A baked tiled generator records in each cell the samples inside that cell.

A record's rectangle is the bounding box of what it changed, so it may cover ground it has nothing to do with. It owns only
the samples it changed, and gives one back only while it still holds the record's value. So an author's sculpt inside the box
stays through re-runs and clears, the next run's capture sees it, and it marks the generator stale; a sculpt on a sample
the road owns goes under the next flatten and returns with `procgen.clear`. A terrain replaced under an applied edit keeps its own heights.
Deleting a generator leaves its edit in the ground, like a detach: clear it first to restore the terrain.

In the editor a `procgen.run`, `procgen.bake` or `procgen.clear` that changes what a generator's terrain holds is one undo entry
(`Generator terrain edit`). `editor.undo` puts the ground back bit for bit. It marks a generator stale, because its output no
longer matches, unless the generator is `live`: a stale `live` generator re-runs at once, which would undo the undo. A re-run
that staleness starts by itself gets no entry: undoing the edit that caused it re-runs the generator, and the ground follows.

A road is a spline entity with `regia_procgen::road::RoadOf { profile }` (saved, `road-of` v1): `in.entities { as: splines }`
puts its profile path in the item scope as `road_profile`, the host captures that file, and the road nodes use it before
their own `profile` setting. `in.entities { as: splines | shapes | all }` reads level splines (in world space) and
shapes (capsules and hulls as their bounding box), item scope `entity`, plus `exclude_width_m` (the largest width factor
along it) on a spline tagged `procgen_exclude`; editing a read spline or shape marks the generator stale. `in.self` also
lists the generator entity's own `Shape` and `Spline`, and a closed spline as a polygon, tagged `self`.

`samples/procgen_roads` is the worked example: three roads with a crossing and a T, curbs from `out_edges`, markings,
fences around the blocks `polygon.cut` makes of a field, and rocks cleared with `road.exclusion`.

```text
cargo test -p procgen_roads                        # procgen.run, procgen.report (2 junctions), bake, verify through the agent
cargo run -p procgen_roads -- --out target/roads   # overview.png for review
cargo run -p procgen_roads -- --window             # a window with the agent server
```

## Subgraphs and generator reads

`graph.call { graph }` runs another graph inline: its declared `inputs` and `outputs` are the node's pins, and inside it
`graph.input { pin }` / `graph.output { pin }` stand for them. A graph may call itself; a call whose inputs carry no
data does not run, which ends the recursion (at most 32 levels: `limit_exceeded` naming `call_depth`). `graph.loop {
graph, each, carry }` runs the body once per item of the `each` pins; a `carry` pin starts with the caller's bundle and
then takes the previous iteration's output of the same name. What a called graph publishes joins the caller's output:
instance sets, prefabs, meshes, terrain edits, road decals and exports accumulate over calls and loop iterations, in
order. A named `out.table`, `out.points` or `out.mask` holds one value per run: a call that publishes a name the run
already has replaces it (the last write wins, as with two `out.*` nodes of one name in a graph) and the calling node
warns `output_name_replaced` once. To keep every iteration's points, return them through a loop output pin
(concatenated over iterations) and publish them once in the caller. `in.generator_output { generator, table }` reads another
generator's `out.table` / `out.points` export (by guid or entity name). An idle or cleared source is queued ahead of
the reader, so it runs first; a source that is already ready is not run again. A failed or cancelled source is not
restarted, and the read warns `export_missing` when that source published nothing. A tiled source, or an `editor_only`
source outside the editor, is not started by the reader. A new output of the source marks the reader stale; two
generators that read each other fail with `generator_cycle`.

## Limits

Per run (desktop / Android): 4 M / 1 M points per item, 32 M / 8 M live elements, 1 GiB / 256 MiB
estimated memory, 20 000 / 5 000 spawned prefabs; a generator's `limits` may lower them. Exceeding one
fails the run with `limit_exceeded` naming it; nothing is clamped silently. Authored sizes are checked before
they are built (v0.5.1): `spline.modules` patterns (`pattern_too_large`), biome child counts (definition
`biome_invalid`, run `limit_exceeded`), and `points.project`'s target-box grid holds at most `16 × boxes + 65 536`
cells however far apart the boxes are. Agent call limits: `ops` ≤
512, `limit` ≤ 10 000, `timeout_ms` ≤ 600 000. `procgen.run`, `procgen.inspect`, `procgen.bake` and
`procgen.verify` are heavy calls.

## Determinism rules for custom node types

Game code adds node types in Rust (`ProcNodeRegistry::register`, the `NodeKind` trait). To keep
outputs identical on every platform:

- Draw randomness only through `NodeCtx::draw(element_seed, slot)` (PCG32 streams keyed by the
  element's seed and the node seed), with a fixed slot per use; never from the thread, chunk or order.
- Use `regia_procgen::detmath` (`sin_cos`, `atan2`, `exp2`, `log2`, `pow`) instead of the platform's
  `sin`/`cos`/`exp`/`ln`, and `regia_procgen::rot::Rot` for quaternion math on placement paths.
- No hash maps with random state in anything that affects output order; sort stably with a full
  tie-break; reduce floats in a fixed order.
- Never write NaN or infinity: the evaluator fails the run with `attr_nan`.

## In the editor

When the editor has a graph open in its Procedural Graph panel, `procgen.graph_edit` on that path edits the panel's
document instead of the overlay: the ops land as one undo entry (`editor.undo` reverts them), `save: true` saves the
document, and the result carries `"document": true`. An open variant is the exception: the same call on the variant
path, its parent, or any file further up its variant chain (the graph it resolves to included), in any spelling of
those paths (`./g.pgraph.ron`, `sub\g.pgraph.ron`, a case variant on Windows), returns `procgen_read_only` and does
not load or write a file. See [Procedural graph editor](procgen-editor.md).

Override pins (spec 62 § 4.1): an edge into the input pin `@<setting>` of any node carries a `Table`; row 0 of the column
named like the setting (or the only column) replaces the setting for the run (it wins over literals and `Bind`). An
`@name` that is not a setting fails compile with `unknown_pin`; a value that does not convert fails the run with
`override_invalid` naming the node. A float converts to an integer setting only when it is a finite integer inside
`i64` (`2^63` and `1e19` do not); the run does not store a saturated integer.

## Tiles and anchors

A generator with `tiles: Some(TileSettings { cell_m, halo_m, runtime })` (spec 66) runs per world-aligned cell instead of
over its whole bounds at once. Cell `(i, j)` covers `[i·cell_m, (i+1)·cell_m) × [j·cell_m, (j+1)·cell_m)` in XZ; a point
on a cell's max edge belongs to the next cell. `cell_m` is a power of two between 8 and 4096 m (default 128;
otherwise `tile_cell_invalid`).

- **Ownership.** Every instance, prefab and exported point belongs to the cell containing its final XZ position;
  outputs outside a cell's half-open rectangle are dropped after its run, so nothing is placed twice or lost at a seam.
  Exported tables and masks are per cell: the merged output names them `name@tier/i/j`. Road ribbons and markings get a
  row on every cell edge their center line crosses, and each stretch between rows belongs to the cell holding its
  midpoint, so neighboring cells share a row exactly; a spline-mesh piece belongs to the cell holding its start point, a
  junction patch to the one holding its centroid, a road decal segment to the one holding its midpoint, and a baked
  terrain edit is recorded per cell.
- **Halo.** Each cell runs over its rectangle grown by `halo_m` (default twice the graph's declared reach, at least
  4 m) so neighborhood nodes see their neighbors. Declared reach: `points.prune` 2 × `max_footprint_m` (default 8),
  `points.neighbors` `radius_m`, `sample.poisson` 2 × `min_dist_m`, `biome.resolve` 2 × `max_core_m` (default 4); other
  nodes 0. Point seeds never depend on the
  cell, so a point computed in a halo gets the same draws as in its own cell. `points.prune` counts
  `seam_mismatch_risk` (decisions made against a point within 1 m of the halo edge) in the cell's report.
- **Tiers.** `tiles.tier { cell_m: Base | Whole | Meters(m) }` puts everything downstream on another grid; `m` must be
  the base cell × 2^k (k may be negative) within 8 to 4096 m (`tier_not_nested`). A node's tier is the finest tier
  among its inputs; source nodes (no inputs) run in each tier that consumes them. Data flows coarse to fine only
  (`tier_upward_edge`); a fine cell receives coarse points clipped to its rectangle plus halo, unless they pass
  through `tiles.no_clip`. Tier indices: the whole tier first (when the graph has one), then by decreasing cell size;
  a graph without `tiles.tier` nodes has one tier, index 0, and gives the same output tiled and untiled.
- **Anchored runtime generation.** Mode `Anchored` with `runtime: Some(AnchorSettings)` builds cells around anchors
  (the primary view, `ProcGenAnchor` entities, or both) each frame in `ProcGenSet::Schedule`. A cell is wanted within
  `build_m` of an anchor (divided by its `reach_scale`, scaled by `quality_reach` for the running quality tier) and
  released beyond `build_m × release_factor` (at least 1.1); a view gate builds only cells within the frustum widened by
  `build_margin_deg` and releases outside `release_margin_deg`. Priority `(d / build_m) · (1 − view_bias · facing)`,
  ties coarser tier first, then cell ascending. At most `max_new_per_frame` (desktop 8, Android 4) cells start per frame
  and `max_running` (4 / 2) run at once; `apply_budget_elements` paces applying deterministically. A fine cell starts
  only after the coarse cell containing it is resident. `max_resident_mb` (512 / 128) releases the farthest finest-tier
  cells early (`procgen_memory_cap` in the log). `ProcGenSettings::paused` freezes scheduling. Each resident cell is an
  entity with `ProcTileCell` and its own `ProcOutput`; releasing it removes its instances and prefabs.
- **Edits.** A terrain edit marks only the cells whose halo-grown rectangle it touches (`stale_cells` in `procgen.list`),
  and the next run evaluates only those, plus the fine cells that read a re-run coarse cell; the other cells keep their
  outputs and the result equals a full run. A change without a rectangle (a moved entity, a file) re-runs every cell, and
  so does any change to a graph with road nodes, `terrain.flatten_road` or `graph.call`: a road row takes its height from
  the whole road, farther than the halo reaches.
- **Baked cells around anchors.** Mode `Baked` with `runtime: Some(...)` streams with the same scheduler: `procgen.bake`
  writes every cell and publishes nothing, then the cells near an anchor load from the `.pgout` (a section at a time, no
  evaluation, `cells_loaded` in `procgen.tiles`). No file shows as `error` in `procgen.tiles`; `procgen.tiles_rebuild`
  reloads cells.
- **Determinism.** A cell's output depends only on the graph, parameters, seed, tier, cell and captured inputs, never on
  the anchor path, budgets or which cells were resident: every client computes the same cells.

| Method | Class | Params | Result |
|---|---|---|---|
| `procgen.tiles` | read | `{ entity, tier?, state? }` | `{ entity, error, warnings: [{ reason, message }], cell_m, tiers: [{ index, cell_m }], cells: [{ i, j, tier, state, instances, ms, mb, priority }], pool: { free, used, cells }, memory_mb, cells_loaded }` |
| `procgen.anchors` | read | `{}` | `{ primary_view: { position, view_dir, reach_scale } \| null, tagged: [...], extra: [...] }` — `extra` is the host-supplied `ProcGenExtraAnchors` (the editor's "generate around view" camera), never counted in `primary_view`/`tagged` |
| `procgen.tiles_wait` | read, heavy, deferred | `{ entity?, anchor?: [x, y, z], tier?, timeout_ms? }` | `{ ready: true, cells }` or `procgen_tiles_timeout` |
| `procgen.tiles_set` | edit_scene | `{ entity, tier, build_m?, release_factor?, view_bias? }` | `{ before, after }`. The reply echoes the requested `release_factor` (at least 1.1) and `view_bias` (at most 1). A cell-sized tier stores that record and the scheduler uses it. The whole tier (no cell size) stores only `build_m` as `base_reach_m`; the scheduler then uses release 1.25 and `view_bias` 0.5, so if that is the only tier the release distance is `build_m × 1.25`. Live; saved only if the editor saves the level. |
| `procgen.tiles_rebuild` | edit_scene, heavy | `{ entity, cells?: [[i, j]], tier? }` | `{ requeued: [[i, j, tier]], run_requested }` (baked and on-load tiled generators that publish whole run again whole) |
| `procgen.set_debug` | edit_scene | `{ tiles?: bool, regions?: bool }` | `{ before: { tiles, regions }, after: { tiles, regions }, drawn }` |

`procgen.inspect { entity, node, cell: [i, j], tier? }` reads one cell's node outputs from a tiled run made with
`keep_debug: true` (`procgen_unknown_cell` for a malformed `cell`).

`procgen.set_debug { tiles: true }` turns on the overlay in the `procgen` gizmo group: each tier's cell grid colored by
state (queued gray, capturing and running amber, applying blue, resident green, releasing brown, failed red), build and
release circles around every anchor, and priority numbers on queued cells. A cell whose `seam_mismatch_risk` is
greater than 0 is outlined in magenta instead of its state color, in any state; the priority number on a queued cell
stays the queued gray. `regions: true` outlines generator bounds. `drawn` is false in an app without the renderer's gizmos.

Cell states: `queued`, `capturing`, `running`, `applying`, `resident`, `releasing`, `failed`. A failed cell stays failed
until `procgen.tiles_rebuild` or until it is no longer wanted and wanted again. `procgen.tiles_wait` keeps at most 64
pending calls (`too_many_pending`); `timeout_ms` defaults to 10 000, at most 120 000.

Errors (`data.reason`): `procgen_not_tiled` (no `tiles`, or no `runtime` for `procgen.tiles_set`),
`procgen_tiles_timeout`, `procgen_unknown_cell` (a cell with no scheduler entry, or a malformed `cells` list),
`procgen_tier_unknown`, `procgen_unknown_state` (a `state` filter that is not a cell state), plus the generator errors
above (`procgen_not_generator`, `procgen_unavailable`, `procgen_param_type`). Compile errors of tiled graphs:
`tier_not_nested`, `tier_upward_edge`, `tile_cell_invalid`. Warnings (`procgen.tiles` `warnings`, and the log when the
scheduler plans the generator): `tier_reach_order` when a finer tier's `build_m` exceeds a coarser tier's (its cells could
not start: the parent cell would not be resident).

`samples/procgen_open_world` flies a camera over 4 km of procedural terrain with a biome on a 256 m tier and clutter on a
32 m tier: `cargo test -p procgen_open_world` runs the route through the agent, `--out <dir>` writes a shot per stop, and
`--window` opens a window for `play.hitch_report`.

Example: wait for the cells around a waypoint, then check the cap.

```json
{ "method": "procgen.tiles_wait", "params": { "anchor": [120.0, 0.0, -40.0], "timeout_ms": 30000 } }
{ "method": "procgen.tiles", "params": { "entity": 4294967301, "state": "resident" } }
```

## Plants

Plant graphs (`flags.domain: Plant`, spec 68) grow trees and bushes from rules and export them as glTF in the tree
conventions. They run in pipeline builds only (editor, CLI, this agent); games ship the exported meshes. The plant nodes:
`plant.seed` (one plant per input point), `plant.grow` (light-driven growth steps: shadow voxels, Borchert-Honda resource
allocation with `apical` control, tropisms, shedding, blooming), `plant.grow_envelope` (space colonization toward points in
an envelope shape), `plant.radius` (pipe model), `plant.bend`, `plant.lean`, `plant.cut`, `plant.simplify`, `plant.scale`,
`plant.twist`, `plant.attach`, `plant.mesh` (bark tubes with `_WIND` / `_WIND_PIVOT`, `docs/formats/plant-wind.md`),
`plant.leaves` (cards or leaf meshes by site rules) and `plant.export` (records an export job). The shipped starters are
`starter/plant_broadleaf.pgraph.ron`, `starter/plant_conifer.pgraph.ron` and `starter/plant_bush.pgraph.ron`.

`procgen.plant_stats { graph, seed?, node?, params? }` (read, heavy) grows a Plant graph in memory and answers
`{ graph, seed, node, kind, plants: [{ seed, nodes, max_order, height_m, crown_width_m, branches_shed, triangles }],
exports: [{ node, out, status }] }` for the plant data on `node` (default: the last node that produced some).
`triangles` is the mesh triangle count when that skeleton has one plant, and 0 on every plant when it has more than one.
Nothing is written: `plant.export` jobs come back with `status: "not_written"`. Check a grown plant by eye with
`procgen.node_types { category: "plant" }` for the settings and `plant_stats` for the shape numbers.

`procgen.plant_build { graph, out, seed?, variants?, params? }` (assets, heavy) writes those jobs: one
`<name>_<k>.glb` per variant (seeds `seed + k`) and a `<name>.plants.ron` index under the project-relative directory
`out`, and answers `{ graph, files: [{ path, seed, triangles_per_lod, bounds }], index, graph_hash }`. It needs a host
that links the asset pipeline: the editor installs `PlantExportHook`; anywhere else, use `regia procgen plant`, which
writes the same files. The bytes repeat for the same graph, parameters and seed.

In the editor, a Plant graph open in the procedural panel previews in an asset viewport with a six-seed **variants**
strip; a click on a variant sets the plant's seed. Agents read the same preview with `editor.plant_preview { asset,
seeds?, seed?, node? }` ([Procedural graph editor: Plant preview](procgen-editor.md#plant-preview)).

Each file has the LODs of `plant.export`'s `lods`, the `_WIND` and `_WIND_PIVOT` attributes on every vertex, capsules
with `collision: "capsules"`, and with `billboard: true` an impostor on the last LOD: LOD 0 baked into an atlas of 8 × 8
captures of 64 pixels, two crossed quads reading the side captures, and `regia_impostor` extras on that LOD's node (`grid`, `cell`,
`centre_y`, `radius`, `albedo_image`, `normal_depth_image`). The bake has no bark or leaf sheets, so its colors are a
neutral tint per slot kind.

The `envelope` pins of `plant.grow` and `plant.grow_envelope` take shapes and closed meshes; a point inside any of them
is inside. `plant.envelope_mesh { asset }` is the mesh source: the host's mesh library, else the glTF file through
`PlantExportHook` or the CLI. A point is in a mesh by ray parity along +X, and a point on the surface is inside. Vertices
at one position count as one; winding order is not read. A node is tested where it lands, not along its segment.
From a glTF file the default scene's triangles are read, except nodes named `<name>_LOD<n>` with n ≥ 1, so an exported
plant contributes its LOD 0 only; an index past its primitive's vertices fails the run (`procgen_run_failed`).

Plant errors (`data.reason`):

| `data.reason` | Meaning |
|---|---|
| `procgen_wrong_domain` | A World graph given to `procgen.plant_stats`, or a Plant graph bound to a level generator |
| `procgen_plant_runtime_forbidden` | A Plant graph in a build without the `pipeline` feature |
| `procgen_plant_not_meshed` | An export of a skeleton (add `plant.mesh` before `plant.export`) |
| `procgen_plant_no_export` | `procgen.plant_build` of a graph with no `plant.export` node |
| `procgen_plant_bad_name` | `procgen.plant_build` of a `plant.export` whose `name` is a path (a `/`, `\`, `:`, a control character, or only dots) or a name Windows cannot write (a `<`, `>`, `"`, `|`, `?` or `*`, or a device name such as `CON`, `NUL` or `COM1`): it must be a file name |
| `procgen_plant_host_required` | `procgen.plant_build` in a host with no `PlantExportHook` |
| `procgen_plant_write_failed` | A file or directory could not be written |
| `procgen_envelope_empty` | `procgen.plant_build`: the envelope mesh has no triangles, or the host has no such mesh |
| `procgen_envelope_not_manifold` | `procgen.plant_build`: an envelope edge is not on exactly two triangles, or a triangle repeats a vertex |
| `procgen_envelope_invalid` | `procgen.plant_build`: an envelope position is not finite, or an index is out of range |

Node codes: `node_wrong_domain` (a plant node in a World graph, or an `in.*` reader or `spawn.*` node in a Plant graph),
`plant_runtime_forbidden`, `plant_not_meshed`, `envelope_empty`, `envelope_not_manifold`, `envelope_invalid` (see the
table above), `limit_exceeded` (more than 200 000 nodes in one plant). `plant.grow`'s `growth_steps` is
0..=64; a value outside that fails compile with `setting_out_of_range`. `plant.grow_envelope`'s `growth_steps`
is 1..=512. Run reports of `plant.grow` carry counters `step_NN_nodes`, `step_NN_active_buds` and `step_NN_shed` per growth
step; `plant.mesh` counts `triangles` and `plant.leaves` counts `leaves`.

## Wave Function Collapse

`regia_procgen::wfc` fills a 2D or 3D grid so that every pair of neighboring cells obeys adjacency rules (API page:
`docs/wave-function-collapse.md`). `procgen.wfc_solve` (read, heavy) runs it in memory on an inline model; nothing in
the world is read or written:

- **Tiled model:** `tiles: [{ name?, weight? }]` (ids are the array indices; weight default 1, finite and > 0) and
  `rules: [{ tile, dir, allowed: [ids] }]` with `dir` one of `east`, `west`, `south`, `north`, `up`, `down`. A side
  with no rule allows every tile; the first rule for a side replaces that with its `allowed` list, later rules add.
- **Overlapping model:** `sample: { size: [w, h] or [w, h, d], cells: [u32 symbols, x-fastest] }`, `n` (pattern size,
  default 3) and `periodic_input` (patterns may wrap the sample edges, default true). Every `n`-block of the output's
  `values` occurs in the sample.
- Common: `size` (`[w, h]` or `[w, h, d]`, at most 65 536 cells), `constraints: [{ cell: [x, y] or [x, y, z], allowed:
  [ids] }]`, `seed` (default 0), `periodic` (wrap the output, default false), `max_backtracks` (default 10 000; 0 turns
  backtracking off).

The answer is `{ model, size: [w, h, d], tile_count, seed, tiles: [id], values: [symbol] }`, x-fastest (`index = x +
w * (y + h * z)`); `values` are the tile ids for a tiled model and the sample symbols for an overlapping one. The same
params and seed always give the same answer on every platform.

```json
{ "method": "procgen.wfc_solve", "params": { "size": [16, 8], "seed": 4,
  "tiles": [{ "name": "grass", "weight": 4 }, { "name": "road" }],
  "rules": [{ "tile": 1, "dir": "east", "allowed": [1] }, { "tile": 1, "dir": "west", "allowed": [1] }],
  "constraints": [{ "cell": [0, 3], "allowed": [1] }] } }
```

All errors are `F0002` with `data.reason`:

| `data.reason` | Meaning |
|---|---|
| `wfc_bad_params` | Malformed params: no model or both models, a bad `size`/`cell`/`dir`, a non-integer id, more than 65 536 cells, more than 2 048 tile variants (tiles times the transforms `symmetry` adds to socket tiles), a sample whose cells times its symmetry transforms exceed 4 096, or an `n` whose pattern block exceeds 64 cells |
| `wfc_invalid_model` | The model is malformed: no tiles, a weight that is NaN, infinite or not > 0, a rule or constraint naming a missing tile, a constraint outside the grid or with an empty list, `n` of 0 or larger than the sample, a 2D sample on a 3D size, or a support table over 2^28 entries |
| `wfc_contradiction` | No grid satisfies the model and constraints (the search was exhausted) |
| `wfc_backtrack_limit` | The `max_backtracks` budget ran out before a grid was found or ruled out; raise it or relax the model |

## Parallel evaluation, prefab overrides and the WFC node (ADR-2724)

**Schedules.** Runs use the compute pool by default: every node whose inputs are ready runs at once, nodes that write
run output (spawns, exports, `graph.call`) keep their compiled order, and element loops of `points.jitter`,
`density.noise` and `filter.range` run in 4096-element chunks. The output is the serial run's, bit for bit, at any
thread count. `procgen.report` totals stay as before; each node line's `ms` is its own wall time.

`procgen.profile` (read, heavy) runs a generator's graph on its captured world once per thread count, without the node
cache and without publishing anything:

| Method | Params | Result |
|---|---|---|
| `procgen.profile` | `{ entity, threads?: [n] or n }` (1 to 8 entries of 0..=256; 0 = serial; default `[0, 1, 2, 8]`) | `{ entity, runs: [{ threads, ok, output_hash, total_ms, nodes: [{ id, ty, ms, elements }], error?: { node, reason } }], identical, slowest: [{ id, ty, ms }] }` |

`identical: false` means two schedules disagreed, which is a bug to report. Times are informational and vary by host.

```json
{ "method": "procgen.profile", "params": { "entity": 4294967301, "threads": [0, 8] } }
```

Errors: `procgen_unavailable`, `procgen_not_generator` (`F0003`), `procgen_param_type` (bad `threads`), and the compile
codes of `procgen.run`.

**`spawn.prefab` overrides.** `overrides: [(attr: "position:x", path: "Glow.intensity"), (attr: "kind", path:
"my_game::Sign.text")]` writes each point's attribute into a reflected component field of its instance (the root, or
else its first descendant that has the component). Numbers go into float, integer (whole values only) and bool fields,
2-4 component values into vector-like structs, tuples and arrays, names into `String`. Bad rows fail compile with
`setting_type` or `override_path_unknown`; at spawn time a missing component or field logs `override_path_unknown`, a
value that does not fit `override_type_mismatch`, a refused patch `override_failed` (each naming the prefab; the other
rows still apply). A bake stores each record's overrides in the `.pgout` (ADR-7750), and a loaded bake applies them the
same way when its instances spawn, so a baked level matches a live run.

**`wfc.solve`.** A grid of points from Wave Function Collapse: `mode: tiled` with socket tiles
`tiles: [(name: "road", sockets: ("r", "g", "r", "g"))]` (east, south, west, north, read clockwise; a side fits the
reverse of the facing side) or `mode: overlapping` with `sample: ["..#", ".##"]` (one character per symbol) and `n`.
`symmetry: rotate | reflect | all` adds the quarter turns and/or mirror of every tile or pattern (variant names
`road@r1`, `corner@m`, `corner@mr2`). Each cell becomes a point at its center (`size`, `cell_m`, `origin`), turned by
the variant, with `tile`, `tile_base`, `tile_id`, `tile_turns` and `tile_mirrored`; feed it to `spawn.mesh` with
`ByAttr(attr: "tile_base")`. The generator seed picks the grid. `procgen.wfc_solve` takes the same `symmetry` and
`tiles[].sockets` (it then answers `variants: [{ name, base, turns, mirrored }]`). The search scans every cell per
decision, so its time grows with the square of the cell count: keep grids small. A cancelled run stops during the
search; a new `procgen.run` on the generator cancels an old one.

| Code | Where | Meaning |
|---|---|---|
| `override_path_unknown` | compile, spawn | A path that is not `Component.field`; at spawn, no such component on the instance or no such field |
| `override_type_mismatch` | spawn | The attribute value does not convert to the field's type |
| `override_failed` | spawn | The reflected patch was refused |
| `wfc_invalid_model` | run | No tiles or sample, a bad weight, `n` larger than the sample, `border` naming no tile |
| `wfc_contradiction` | run | No grid fits the tiles (and `border`) |
| `wfc_backtrack_limit` | run | `max_backtracks` ran out; raise it or relax the tiles |

## Copy-on-write points, masks, ground layers and road pieces (ADR-2731)

Point columns are shared until written, so a chain of in-place nodes copies nothing and a node copies only the columns
it writes. A node cache holds each output, so a cached run's next writer copies what it writes; `keep_debug` holds every
output the same way. Results do not depend on any of this. `REGIA_PROCGEN_THREADS=n` sets the default schedule of a
process, for running a test suite on one schedule.

`in.mask` reads the host's `ProcMaskLibrary` copy of its `asset`, else a PNG in the project, as stored numbers with no
sRGB decoding (16-bit files keep 16-bit precision). A file that is missing, does not decode, or has more than
4096 × 4096 pixels warns `mask_missing` and gives an empty bundle.

`in.ground { layers: ["rock"] }` carries the named terrain layers' weights, procedural and painted as biome rules read
them, and warns `layer_missing` for a name the terrain lacks. `density.ground_layer { layer, mode }` combines a layer's
weight under each point into density (pins `in`, `ground`); a layer the ground item does not carry fails with
`layer_missing`. Follow it with `filter.range` to filter by layer.

`road.pieces { profile?, smooth_rows, forward_axis, segments_per_m, material, collision }` (pins `splines`, `ground`,
`out`) places the road profile's `pieces` rows: each mesh repeats along the whole road, `round(length / mesh length)`
copies stretched to fit, bent to the curve at `offset_m` on its `side` and laid on the road surface. Register the meshes
in `ProcMeshLibrary`; a missing one warns `mesh_missing`. Pieces are not cut back at junctions.


## Tile classes, override checks and node-setting additions (ADR-3027)

**Tile classes.** A `wfc.solve` (and `procgen.wfc_solve`) tile may name its picture's symmetry with `class`: `X`
(fully symmetric, 1 variant), `I` (a north-south bar, 2), `\` (a north-west to south-east diagonal, 2), `T`
(symmetric about north-south, 4), `L` (a corner joining north and east, 4) or `F` (none, 8); the counts are for
`symmetry: all` (`rotate` gives 1/2/2/4/4/4, `reflect` 1/1/2/1/2/2). A classed tile keeps one variant per distinct
picture, even when two of its variants have the same sockets; a tile without a class keeps one per distinct socket set,
as before. Sockets must have the class's symmetry, else the model is refused with `wfc_invalid_model`.

```ron
tiles: [
    (name: "grass", sockets: ("g", "g", "g", "g"), class: "X", up: "a", down: "a"),
    (name: "road", sockets: ("g", "r", "g", "r"), class: "I", up: "a", down: "a"),
    (name: "corner", sockets: ("r", "g", "g", "r"), class: "L", up: "a", down: "a"),
    (name: "beam", sockets: ("e", "e", "e", "e"), class: "I", up: "^b", down: "^b"),
]
```

`up` / `down` are vertical sockets for 3D grids (`size: (x, rows, layers)` with more than one layer): a tile may sit on
another when its `down` equals the other's `up`; a tile without them stacks on anything. Quarter turns are about the
vertical axis. A vertical socket starting with `^` only meets the same socket on a variant turned and mirrored the
same way (a beam stacks on a beam running the same direction).

**Tile-set assets.** Keep a tile set in a data table and feed it with `in.table` to `wfc.solve`'s `tileset` input:
one row per tile, columns `name`, `east`, `south`, `west`, `north` (names), optional `weight` (number), `class`, `up`
and `down` (names; empty = none). A connected table replaces the `tiles` setting and gives the same grid as the same
tiles written inline.

**`wfc.solve` seed and bounds.** `seed: -1` (default) picks the grid from the generator seed; `seed: n` (n ≥ 0) is
the solver seed itself, the same seed `procgen.wfc_solve` takes, so the node and the method answer the same grid for
the same tiles. Connect any spatial item to the `bounds` input to cover its world box: the box's min corner becomes the
origin and its x/z extent over `cell_m` (rounded up) the cell counts; the layer count stays `size`'s third value.

**Override paths at compile.** When a generator compiles in a world with a type registry, each `spawn.prefab`
override path is checked against the registered reflected types: no such type, a type that is not a reflected
component, or no such field fails `procgen_graph_invalid` with `override_path_unknown`; a field no attribute can fill
(a list, a handle), or a built-in point property (`position`, `rotation`, `scale`, `color`, `density`, ...) or `:xyz`
selector with another component count than the field, fails with `override_type_mismatch`. Each error names the node
id and the path. Custom attributes are only typed at run time, so their values are still checked per instance.
Overrides inside a `graph.call` body are checked when the instances spawn.

**Node settings.**

| Node | Setting | Meaning |
|---|---|---|
| `graph.call` | `graph_from`, `graphs` | `graph_from: "item.graph"` reads, per item of the first input pin, an item-scope Asset or Name naming the graph to run (items without one run `graph`). Items naming the same graph run together, groups in order of first appearance, the other pins whole; outputs concatenate in group order. List every graph it may name in `graphs` so the host captures it: another path fails `unknown_graph`; a graph whose declared pins differ from `graph`'s fails `call_pins_mismatch` |
| `attr.lookup` | `nearest_within` | Default -1 = exact keys. With a distance, keys whose table column is Float, Vec2-4 or Quat match the closest row within it (Euclidean; rotations by angle in degrees; with several such keys the largest per-key distance counts); other keys still match exactly; rows tied at the closest distance go to `multi` |
| `points.align` | `keep_yaw` | Default true: the shortest turn from the current rotation (the heading about the new up is kept). False: the shortest turn from no rotation, so the old heading is dropped |
| `points.project` | `onto` kinds | The kind of the first `onto` item picks the target. Ground: as before. Spline: the nearest curve point (every point kept; `rotation` aligns up to the spline frame). Polygon: its plane height (points outside dropped unless `keep_outside`). Points: the top of the highest target box under the point (`rotation` aligns up to that point's up; points over no box dropped unless `keep_outside`) |
| `in.rays` | (output) | Hits also carry `phys_material`: the hit collider's physics material name (`PhysicsMaterialRef`), empty without one |
| `in.overlap` | (test) | Each point's oriented box is tested against the collider's shape (box, sphere, capsule, cylinder, cone, convex hull, mesh triangles, compound parts), not only its bounds. Height fields and meshes over 65 536 triangles are tested by their bounds; round shapes are exact under uniform scale. A mesh counts by its surface: a box wholly inside a closed mesh does not overlap it |
| `in.entities` | (output) | Each point's `bounds_min` / `bounds_max` are the entity's `MeshRenderer` mesh bounds (local, before scale); the unit box without a mesh |

| Code | Where | Meaning |
|---|---|---|
| `call_pins_mismatch` | run | A `graph_from` graph declares other input or output pins than `graph` |
| `override_type_mismatch` | compile, spawn | Also at compile: the field takes no attribute, or the selector's component count differs |

`points.project` also takes `attributes: none | all | list` (with `list`) and `color: source | target | mul`: with a
points target, the projected point takes the hit target point's attributes (target wins; points with no target keep
theirs or get the type's default) and color. `tags: source | target | both` keeps the item's tags, takes the `onto`
items' tags, or adds them, for every target kind.

## Events

The agent event stream (`events.read`, see [events](events.md)) carries the generator life cycle:
`procgen.run_started`, `procgen.run_finished` (`ok`, output counts, or the error `code`),
`procgen.run_cancelled`, `procgen.bake_written` (from `procgen.bake`), `procgen.cells_changed`
(cells that became resident or were released, as `[tier, i, j]`), `procgen.cleared` and
`procgen.detached`. Each carries the generator's `generator` guid and `entity` id. A run's
`run_started` and `run_finished` share one `run_id`:

```json
events.read { "since": 0 }
→ { "records": [
    { "seq": 7, "kind": "procgen.run_started",  "data": { "generator": "…", "entity": 42, "run_id": 3 } },
    { "seq": 8, "kind": "procgen.run_finished", "data": { "generator": "…", "entity": 42, "run_id": 3,
                                                          "ok": true, "counts": { "instances": 120, "entities": 0, "tables": 0 } } } ],
    "next": 8 }
```

## Road clear/regenerate regression recipe

For the road sample, compare `procgen.report` road lengths, junction counts, flatten rectangles and
limit distances after `procgen.run { entity, wait: true }`. Take `procgen.list` draw totals after
`play.step { frames: 2 }`. Repeat clear/run three times and require the same content and totals;
ignore entity IDs, run counters, timing/cache statistics and temporary file paths when comparing
content. With no intervening author edits, clear must restore the original terrain samples bit for
bit and leave no generated draw or collision output. Unrelated authored entities must survive.

`procgen.clear` removes the live output and restores owned terrain samples; it leaves the saved
`.pgout` file available for a later load. Test bake/clear/run in the current app, then load that bake
in a separate fresh app. A loaded bake is ready without incrementing `run_id`; loading must reproduce
its mesh/instance content, flattened terrain and requested colliders. `procgen.verify` should succeed
both before and after loading without accumulating output.

`samples/procgen_roads/tests/clear_regenerate.rs` exercises this recipe through the existing protocol,
with read-only probes for exact terrain/content comparisons. The shipped graph requests no collision;
a test-only in-memory graph overlay enables existing Simple curb and Mesh rock collision so cleanup
is also checked with nonempty colliders. Run it with lavapipe explicitly selected (no skip when absent):

```sh
VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json cargo test -p procgen_roads --test clear_regenerate -- --test-threads=1
```

## Evaluation fingerprint

`procgen.fingerprint` (read, heavy) digests a list of instances the caller has already quantized, so two bakes
compare without reading the world file. Nothing in the world is read or written. The mix is not cryptographic.

Each record is `{ node, instance, position_mm: [x, y, z], yaw_mdeg }`. `node` and `instance` are `u32`.
`position_mm` is three `i32` millimeters (x, y, z; `round(meters * 1000)`, kept signed). `yaw_mdeg` is yaw in
`i32` millidegrees and is not wrapped. Omit `records`, or pass `[]`, for the empty digest
`f7d710e043e5193e` (sixteen lowercase hex digits). The same records in any order share a digest, including two
records that share a node and an instance index. Changing one millimeter, or one millidegree, changes the digest.

At most 1 048 576 records. One more answers `procgen_fingerprint_overflow` and does not allocate the mix buffer.
The parameter gate allows 1 048 577 items so that one extra record reaches this error; a longer list is refused
by the gate (`F0002`) and is not mixed either.

```json
{ "method": "procgen.fingerprint", "params": { "records": [
  { "node": 1, "instance": 0, "position_mm": [1000, 0, -250], "yaw_mdeg": 90000 }
] } }
```

The answer is `{ digest, count }`. `digest` is sixteen lowercase hex digits.

All errors are `F0002` with `data.reason`:

| `data.reason` | Meaning |
|---|---|
| `procgen_fingerprint_bad_params` | `records` is not an array, or a record is missing a field, has the wrong type, or a component is outside `u32` / `i32` |
| `procgen_fingerprint_overflow` | `records` holds more than 1 048 576 entries. The mix buffer is not allocated |

This stateless method is available without `ProcGenPlugin`.

## Node notes (ADR-5644)

- `filter.compare` compares a scalar: `attr: "position"` fails with `attr_type_mismatch`; write `position:y`. With
  `rhs_from: constant`, `rhs_attr: "w"` compares each element against its own `w` instead of `value`.
- `debug.log` messages take `{len}`, `{items}` and `{attr:<sel>}` (the value at the first element of the first item;
  `item.<name>` for item scope; `-` with no element, `?` when the selector cannot be read). Shipping builds (release,
  without the `pipeline` feature) pass the data through without logging.

## Data layers (ADR-7214)

A generator is an ordinary data-layer member: give it a `LayerMembership` (spec 37, see
[levels, layers and instances](levels-layers-instances.md)). Its output follows it; the output itself is not a member.

- **Parked** (or `Absent` for a generator that is not in a level file): the generator's output roots (whole outputs
  and tile cells) are parked with it: hidden, and the spawned prefab instances and instance colliders under them are
  skipped like any parked subtree. Its instance sets and generated meshes are not drawn. Going `Live` restores all of it.
  The output is not rebuilt, and a run may still finish while parked (it stays hidden).
- **Absent** for a generator from a level file: the layer switch despawns the generator and its output goes with it.
  When the layer comes back the generator respawns from the level and loads its bake (`Baked`) or runs again
  (`OnLoad`, anchored cells): the output comes back with the same hash.
- `spawn.mesh { target_layer: "fences" }` puts the node's instance sets in that layer: they draw only while it is
  `Live`, and the static collider built for them (when an entry sets `collision`) is parked with it. The generator's
  own presence still applies on top. A name that is not a valid layer name fails the node with `setting_type`; a layer
  the level does not bind hides nothing.

`procgen.list` and `procgen.get` report each generator's `layers` (its membership) and `presence`
(`live`, `parked` or `absent`: the lowest effective state of its layers):

```json
procgen.list {}
→ { "generators": [{ "entity": 42, "guid": "…", "graph": "forest.pgraph.ron", "state": "ready",
                     "layers": ["quest_a"], "presence": "parked", "output": { "instances": 1200, … } }], "draw": { … } }
```

Check it with `level.set_layer_state { layer: "quest_a", state: "parked" }`, then `procgen.list` (`presence: "parked"`, `draw.drawn`
drops by the generator's instances on the next frame) and a `seeing.screenshot`.

## Biome grass on the GPU (ADR-7214)

Biome generators write the per-class grass multipliers into `GrassMultipliers` (spec 65 § 8). A host that draws GPU
grass inserts `regia_procgen::biome::ecs::GrassRebake::new(set, layer.handle())` (with `texel_m` set to the texel size the
layer's mask was baked with): whenever the multipliers change, the mask is re-baked from the primary terrain and queued
on the handle, and the grass layer takes it on its next frame. A re-baked mask of another side or class count is refused
(`TerrainGrassHandle::masks_refused`). A class that names a density map re-bakes with the map the host put in
`GrassRebake::density_maps` under the path the `.grass.ron` gives (the same `DensityMap` the layer's first bake used,
ADR-7262). A map missing there fails the re-bake (`GrassRebake::last_error`, warning `procgen_grass_rebake_failed`), and
the mask keeps its old bake. `terrain.grass` reports
the uploaded `mask_generation`, which moves on each re-bake the layer takes.
