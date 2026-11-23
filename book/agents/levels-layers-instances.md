# Levels: per-entity files, data layers and placed levels

Three connected features (spec 37) keep a level editable by several people and let gameplay switch
groups of content:

- **Directory form**: a level stored as `<name>.level.d/`, one small file per entity, so version
  control merges per entity and a save rewrites only what changed. Authoring only: the cooked scene
  is the same for both forms and a shipped package never contains a `.level.d` path
  (the ship checks of `regia package` refuse it as editor content).
- **Data layers**: named sets of entities switched together (night props, a quest's actors, a
  destruction state).
- **Placed levels**: a saved level placed inside another level as one transformed object.

Methods: `level.storage`, `level.convert`, `level.layers`, `level.set_layer_state`,
`level.set_membership`, `level.placed`, `level.place_level`, `level.break_placed`, `level.status`,
`editor.layer_delete`, `editor.layer_create`, `editor.layer_bind`, `editor.layer_initial`, `editor.break_placed`.

## Task 1: split a level into per-entity files

```json
{ "method": "level.storage", "params": { "level": "levels/forest.level.ron" } }
```

answers `{ level, form: "file", entities, files: 1, header_dirty, dirty_entities }`. The dirty fields come from a
`LevelDirtyTracker` resource that the host feeds; the editor does not feed it yet (a v0.5.x gap), so in the editor they
read `false` and `[]`, and `level_dirty` below is only answered when a host has marked the level. Convert it:

```json
{ "method": "level.convert", "params": { "path": "levels/forest.level.ron", "to": "dir" } }
```

answers `{ written, deleted, unchanged, form: "dir" }`; the single file is removed (a level exists
in one form only; both at once is `ambiguous_level_form`). `to: "file"` merges back. The round
trip is lossless: entities come back in guid order, byte-identical after normalization. A level
with unsaved editor changes is refused (`level_dirty`).

The directory layout:

```text
levels/forest.level.d/
  level.ron                                  # LevelDirFile: versions + resources, no entities
  entities/8f/8f1c3a...(32 hex).entity.ron   # one EntityFile per entity, shard = first two hex digits
```

`regia level validate <level> [--json]` checks a level's bound definitions and memberships (spec 37 § 4.5): one
`<reason> <detail>` line per finding, exit 1 on any.

From a shell: `regia level split levels/forest.level.ron` and `regia level merge levels/forest.level.d`
(`--out` to choose the target, `--keep` to keep the source). `regia upgrade-project --write`
rewrites every level directory at the latest versions and keeps its form (a `<name>.level.d.bak`
copy is made first). Format reference: `docs/formats/level-dir.md`.

## Task 2: toggle a layer from a test

```json
{ "method": "level.layers", "params": {} }
{ "method": "level.set_layer_state", "params": { "layer": "night_props", "state": "parked" } }
```

`level.layers` lists `{ name, kind, parent, requested, effective, members, peers }` ordered by
name, plus `despawned` (members not spawned now, whose records wait in their level's template) and `parked` (members
parked now, not counting the descendants that follow them), so a test can tell the two states apart. `level.set_layer_state` answers `{ layer, changed, effective }`; the switch is applied at the
next frame's layer switch (`PostUpdate`, before transforms propagate), so a test follows it with
`play.wait_until` on the entity count it expects, for example
`{ "query": { "method": "level.layers", "params": { "layer": "night_props" } } ... }` or a
`scene.query_entities` count.

| State | Members at run time |
|---|---|
| `absent` | not spawned; their records stay in the level's template. A member with no record there, such as a script-spawned one, is parked instead: nothing could bring it back |
| `parked` | spawned, carry `LayerParked`, hidden, not simulated, not pickable |
| `live` | spawned, visible, simulated |

- `effective = min(requested, effective(parent))`: a child layer is never more active than its parent.
- **Intersection rule**: an entity in several layers is present at the **lowest** effective state
  among them: spawned only when all are at least `parked`, simulated only when all are `live`. An
  entity without `LayerMembership` is not governed by layers; a layer name no definition binds does
  not govern it either (validation reports it).
- `recursive: true` sets the layer and every descendant to the same requested state.
- The `state` parameter also accepts `unloaded` / `loaded` / `activated` for `absent` / `parked` / `live`.
- `side` (`local` default, `server`, `client`): a write by a side that does not own the layer's
  `peers` setting changes nothing and answers `changed: false, reason: "side_not_owner"`.
  `local` owns every layer; `server` owns `Everyone` and `ServerOnly`; `client` owns `ClientsOnly`.

`level.set_membership { entity | entities, add?, remove? }` edits `LayerMembership` and answers
`{ updated }`.

Layer definitions live in `*.layers.ron` assets (`docs/formats/layers-ron.md`);
a level binds them through its `regia_scene::layers::LevelLayers` resource with initial states.
A definition file needs a `.meta` sidecar holding the id the level names.

What `parked` stops, and where, so a game knows what to filter the same way:

| Area | What leaves |
|---|---|
| Physics | Rigid bodies, colliders and joints leave the rapier world until `live`; the components stay, and a body comes back with its `Transform` and `Velocity`. 2D bodies and platformer actors and solids too. A vehicle keeps its chassis, disabled. |
| Scripts | No `start`, updates, events or messages, broadcast or targeted; `start` runs once `live`. |
| AI | Behavior trees, state trees, ability cooldowns, `MoveTo` and nav agents do not tick; smart object slots leave the index (and come back when unparked, with no other change needed). |
| Render | Meshes, lights, glass, occlusion proxies, interior volumes and static decals. A terrain leaves the editor's viewport and play view; a game that draws its terrain itself skips an entity with `LayerParked`. |
| Animation, audio | Animators do not evaluate, and the pose is not re-applied (no gaze, spring-bone or socket step); property animators, node morph players and motion matchers do not advance. An audio emitter stops as a despawned one does, with a 50 ms fade; an autoplaying one starts again once `live`. |
| Picking | `world_pick` never hits a parked entity, `include_hidden` or not. |

Children without a `LayerMembership` of their own (a model's parts, what a script attached) follow their member, also
when they arrive while it is parked. A child with a membership is governed by it.

A `ThrusterPlume` and a `HoverWash` stop and start again when the layer goes live, and a `StaticDecal` leaves the decal
pool until then. Other particle systems keep running (`SpawnEffect` instances belong to no entity): stop them from the
layer's `LayerChanged` when it matters. A terrain is drawn by its host through the terrain renderer hook, not by an
entity query: the editor's terrain bridge skips a parked terrain (and draws the next live one); a game host that
installs its own `TerrainSharedDraw` does the same check.

Opening a level again under its id starts its layers over at their `initial` states.

## Loading levels

`regia_framework::level_assets::load_level_source(world, path)` takes a `.level.ron` file or a `.level.d` directory
and fills the `LevelLibrary`, with the `.layers.ron` definitions the level binds and the levels it places, found by
their `.meta` ids under `assets/`. It answers `{ level, levels, layer_defs, missing }`: what is not found is listed
and the level still loads. Call `open_level` with the id. `regia run --level` and the editor take either form. With
both forms of one level on disk the answer is `ambiguous_level_form`. `LevelAssetsPlugin` (part of `DefaultPlugins`)
reloads what changed in a dev run: an edited entity file reopens its level if active, an edited `.layers.ron` rebuilds
the layer table.

`regia package` writes one `.level.ron` per level, in every profile: entity files fold, members of `EditorOnly` layers
and those layers go, and the definitions the level binds move into it (`LevelLayers.embedded`), because a package has no
`.layers.ron` files. Levels with no layers and no tests ship byte for byte. A level's `.meta` ships with it, the one
sidecar a package carries, so placed levels are found by id. A named profile may name a level in either form as an entry
level, and its closure follows the levels placed by id.

## Load progress and the startup level

`level.status { level }` answers `{ level, state, progress, current, failed, loading_screen }` for a level the
`LevelManager` tracks (`level_not_found` otherwise; a `level` that is not a guid is `bad_guid`); without `level` it answers `{ current, loading_screen,
levels: [...] }`. `progress` is `1`
once the level is `active`. While it is `loading` it is the resident fraction of the load set named
`level:<guid>` (spec 12 § 5b; whoever streams the level's assets defines it), `0` without one, and never `1`
before the level is active. A loading screen reads the same value from
`regia_framework::level_progress::level_load_progress(world, id)` (ADR-7203).

```json
{ "method": "level.status", "params": { "level": "asset:8f1c3a00-0000-4000-8000-000000000001" } }
```

At run time `StartLevelPlugin` opens `regia run --level` when given, else the project's `Regia.toml`
`[project] startup_level` (a path under `assets/`), the latter only when the game's own `Startup` opened no level.

## Loading screens

`Regia.toml` `[project] loading_screen` names the project's loading-screen prefab (spec 14 § A4, ADR-7263): a
`.prefab.ron` path under `assets/`, or `asset:<guid>` for a prefab or variant the game registers itself.

```toml
[project]
loading_screen = "ui/loading.prefab.ron"
```

While a `Replace` open waits for its data, one instance of it is alive; its root carries
`regia_scene::level::LoadingScreen { level, progress }`, where `progress` follows `level.status`'s value and
never goes down. It goes when the load completes, fails or is unloaded. An open whose data is already in the
`LevelLibrary` completes in the call and shows none; `Additive` opens show none. `level.status` names the root
entity as `loading_screen` (on the level it shows, and at the top of the list form; `null` when none is up), so
an agent can find the screen's entities, then poll `level.status` until `loading_screen` is `null`:

```json
{ "method": "level.status", "params": {} }
→ { "current": null, "loading_screen": 4294967310,
    "levels": [{ "level": "asset:8f1c3a00-0000-4000-8000-000000000001", "state": "loading", "progress": 0.25,
                 "current": false, "failed": [], "loading_screen": 4294967310 }] }
```

## The editor's dirty set

The editor tracks which entities of the open level changed (spec 37 § 3.4, ADR-7203): each frame the undo
history changed (an edit, an undo, a redo), the entities whose components changed since the last check, and those
added or removed, join the level's dirty set; a change to the level's settings or layer binding dirties `level.ron`.
`level.storage` shows it as `dirty_entities` / `header_dirty`, and `level.convert` refuses a dirty level
(`level_dirty`). Saving a `.level.d` the editor opened (or last saved) writes only the dirty entity files; other
files keep their bytes. A first save, or a save to another directory, writes the whole level.

## Deleting a layer

`editor.layer_delete { defs, layer }` removes `layer` from the definition file `defs` (relative to `assets/`).
It is refused with `layer_in_use` while any level of the project that binds the definition has members in the
layer (the open level's unsaved state counts instead of its file); `data.members` lists them as
`{ level, entity }` so you can take them out first (`editor.layers_panel_set` `remove_selection`). A layer that is
another layer's parent is refused with `layer_parent_invalid`, a missing name with `layer_unknown`. On success it
answers `{ defs, layer, layers }`; the file is rewritten (not an undo step).

```json
{ "method": "editor.layer_delete", "params": { "defs": "layers/town.layers.ron", "layer": "night_props" } }
```

## Creating and binding layers, and their initial states

Three more editor methods (ADR-7614) do what the Layers panel's *Layer definitions* section and *initial* column do;
the panel runs the same code, so both write the same bytes.

| Method | Mode | Params | Result |
|---|---|---|---|
| `editor.layer_create` | edit scene | `defs`, `layer`, `kind?` (`runtime`, the default, or `editor_only`), `parent?` | `{ defs, asset, layer, layers, created_file }` |
| `editor.layer_bind` | edit scene | `defs`, `bind?` (default `true`; `false` unbinds) | `{ defs, asset, bound, changed }` |
| `editor.layer_initial` | edit scene | `layer`, `initial` (`absent`, `parked`, `live`; `unloaded`, `loaded`, `activated` too) | `{ layer, initial, changed }` |

`editor.layer_create` adds a layer to the definition file `defs` (relative to `assets/`), creating the
`.layers.ron` and its `.meta` when the file does not exist (`created_file: true`). It is a file edit, not an undo step.
Refusals: `bad_layer_name` (not `[a-z0-9_]{1,48}`), `layer_exists`, `layer_parent_invalid` (`parent` is not a layer of
that file), `bad_defs_path` (the name does not end in `.layers.ron`), `path_outside_project`.

`editor.layer_bind` adds the file to the open level's `LevelLayers` (`defs: ["asset:<id>"]`), or removes it with
`bind: false`, which also drops the level's entries for the layers only that file defines. `editor.layer_initial`
sets the state a layer starts in when the level loads (the binding entry's `initial`; the panel's *visible* and *loaded*
toggles stay a working state). Both change the open level's header: one undo step each (`editor.undo` reverts it),
written by the next `editor.save_level`. Undo restores the previous world binding too, including absence
when the level had no binding before the edit (ADR-7956). Refusals: `level_not_found` (no open level), `layer_defs_not_found`,
`layer_unknown` (`editor.layer_initial` on a layer the level does not bind).

```json
{ "method": "editor.layer_create", "params": { "defs": "layers/town.layers.ron", "layer": "night_props" } }
{ "method": "editor.layer_bind", "params": { "defs": "layers/town.layers.ron" } }
{ "method": "editor.layer_initial", "params": { "layer": "night_props", "initial": "absent" } }
{ "method": "editor.save_level", "params": {} }
```

## The Layers panel

The editor's Layers panel lists the open level's layers with *visible* (`live` or `parked`) and *loaded* (`absent` or
not) toggles, the *current layer set* and buttons that add the selection to a layer or take it out (one undo step).
Entities placed while the current set is not empty join it. The toggles are a working state: they change no file, and
*Play* starts from the `initial` states. `EditorOnly` layers are absent in play. The play world's entities are members of
the play level (the open level's id), whose template is the copy play started from, so an `absent` layer despawns its
members in play and brings them back as in a game; an entity gameplay spawned has no record there and parks
(ADR-2161). Game code that opens a level with `Replace` during play replaces the play level, as in a game.

Check it against a running game (`regia run`), or from a functional test in play (the Tests panel runs them in the play
world):

```json
{ "method": "level.set_layer_state", "params": { "layer": "night_props", "state": "absent" } }
{ "method": "level.layers", "params": { "layer": "night_props" } }
```

After the next frame the second answer has `despawned` equal to the layer's template members and `parked: 0`; with
`"state": "parked"` it is the other way round.

| Method | Mode | Params | Result |
|---|---|---|---|
| `editor.layers_panel_state` | read | none | `{ layers: [{ name, kind, parent, requested, effective, members, visible, loaded, current }], current, warnings, problems, session }` |
| `editor.layers_panel_set` | edit scene | `current?`, `layer?`, `visible?`, `loaded?`, `add_selection?`, `remove_selection?` | the state, plus `updated` for selection changes |
| `editor.placed_edit` | edit scene | `action` (`begin`, `commit`, `discard`), `entity?` | the state; `session` while one is open |
| `editor.placed_create` | edit scene | `path`, `pivot?` | `{ entity, level, path, moved, pivot }` |

The panel's *Layer definitions* section lists the project's `.layers.ron` files with *Bind* / *Unbind*, and a
*Create layer* form (file, name, *Editor only*); each layer row has an *initial* state and *Delete layer*. With a
placed-level instance selected, the panel shows *Edit placed level* (`editor.placed_edit` `begin`) and *Break placed
level* (`editor.break_placed`); the Outliner's row menu has the same two items on an instance. Panel errors show on the
panel's error line with the agent's message.

`editor.placed_edit` opens the level an instance places on its own, with a banner in the panel. `commit` saves it and
reopens the host, so every copy shows it; `discard` reopens the host. `begin` needs a saved host (`level_dirty`) and an
instance (`not_placed`, `level_not_found`). The host is closed during the session, not dimmed (ADR-1953).
If another level was opened during the session, `commit` writes nothing and answers `placed_level_not_open`, and
`discard` just ends the session; either way the level that is open stays open.

`editor.placed_create` is *Create placed level*: the selected entities move into a new level at `path` (under `assets/`;
`.level.ron` or `.level.d` chooses the form) with their guids, positions relative to the pivot, and one instance takes
their place, so nothing moves in the world. `pivot` is `lowest_center` (the default), `center` or the guid of a selected
entity. It only translates: the entities keep their rotations. The level gets a `.meta` sidecar and is placed by id. It
is one undo step, and an undo leaves the new files on disk. Refusals change nothing: `selection_empty`, `placed_member`,
`pivot_not_selected`, `level_path_invalid`, `level_exists`, `placed_cycle`, `placed_limit`.

## Task 3: place and break an instance

```json
{ "method": "level.place_level",
  "params": { "level": "asset:6f1e2b3c-4d5a-4a35-9d20-3f7a1c2e8b64",
              "transform": { "translation": [12, 0, -4] }, "mode": "inline" } }
```

answers `{ entity, spawned }`. `transform` takes `translation`, `rotation` (a quaternion `[x, y, z, w]`, normalized;
a zero-length one is refused) and `scale`, each optional. Inline mode spawns the level's entities as a child subtree of the
instance; each guid is `blake3(placement guid ‖ source guid)` truncated to 16 bytes, so the same
instance always spawns the same ids and two instances never collide. The placed level's resources
(settings, environment) are ignored: the host's win. Members carry `PlacedMember` and inherit the
instance's layers. `mode: "streamed"` loads the level additively, parents it under the instance,
writes `PlacedLevelLoaded { owner, level }`, and unloads it when the instance despawns (a level
streams once at a time).

`level.placed` lists `{ entity, level, mode, spawned, error? }`. `level.break_placed { entity }`
replaces an inline instance with plain entities (derived guids become permanent, world transforms
are kept) and answers `{ entities: [guid] }`.

In the editor, `editor.break_placed { entity }` does the same break as one undo step (what *Break placed level* in the
Layers panel and the Outliner does): `editor.undo` despawns the former members, puts the instance back and fills it
again, so the members get the same derived guids and the level saves to the same bytes as before the break. An
instance under a parent comes back under it, at its place among its siblings, and the undo entries recorded before the
Break (a move of the instance) or after it (a move of a former member) still find their entities. An entity that is
not a member but hangs under the instance (the host level put it there) or under a member (the user parented it there)
stays: Break moves the instance's direct children to the instance's parent at the same world place (as `level.break_placed`
does), and the undo puts each such entity back under the restored instance or the refilled member, at its old place
among the siblings, with its old local transform and its entity id unchanged (ADR-8593). Refusals:
`placed_not_found`, `not_placed` (not an inline instance with spawned members).

```json
{ "method": "editor.break_placed", "params": { "entity": 4294967298 } }
```

Limits: nesting depth at most 8, at most 100,000 entities per instance; a level containing itself
directly or through others is refused. A missing level leaves the instance in place with a
`PlacedError` and the game keeps running.

## Reasons

Refusals are `F0002` with `data.reason`:

| Reason | When |
|---|---|
| `ambiguous_level_form` | both `x.level.ron` and `x.level.d/` exist |
| `level_not_found` | the level (or a directory's `level.ron`) does not exist, or a placed level is not in the library |
| `entity_file_mismatch` | an entity file's name is not the guid it holds |
| `entity_wrong_shard` | (warning) an entity file sits in the wrong shard; the next save moves it |
| `duplicate_entity` | two entity files hold one guid |
| `mixed_scene_versions` | files of one directory declare different scene versions; run `regia upgrade-project --write` |
| `unsupported_version` | a file is newer than this build |
| `level_parse_error` | a file is not valid RON for its type |
| `level_invalid` | the assembled scene fails migration or validation (unknown parent, cycle) |
| `level_io_error` | a file could not be read or written; nothing was deleted (the header is the last write, and deletions wait for every write, the header's included) |
| `path_outside_project` | `level.storage` / `level.convert` with an absolute, drive or `..` path, or one that leaves the project through a link |
| `level_dirty` | `level.convert` on a level with unsaved editor changes |
| `layer_unknown` | no bound layer has that name |
| `layer_ambiguous` | (validation) a name is in two bound definitions; the first wins |
| `layer_membership_invalid` | (validation) an entity's `LayerMembership` does not parse, so at run time it counts as in no layer |
| `layer_in_use` | `editor.layer_delete` on a layer that still has members in a level binding the definition; `data.members` lists `{ level, entity }` |
| `layer_defs_not_found` | `editor.layer_delete` or `editor.layer_bind` `defs` is not a `.layers.ron` with a `.meta` under `assets/` |
| `layer_parent_invalid` | (validation, fatal) a layer's parent is missing or a cycle |
| `layer_parent_mismatch` | (validation) a member's entity-parent is in layers the member is not in |
| `bad_layer_name` | `editor.layer_create` with a name that is not `[a-z0-9_]{1,48}` |
| `layer_exists` | `editor.layer_create` with a name the file already has |
| `bad_defs_path` | `editor.layer_create` `defs` does not end in `.layers.ron` |
| `placed_cycle` | a level contains itself |
| `placed_limit` | nesting over 8, more than 100,000 entities, or a level already streamed |
| `placed_not_found` | `level.break_placed` or `editor.break_placed` on an entity that does not exist |
| `placed_member` | `editor.placed_create` with a member of an instance selected: select the instance |
| `pivot_not_selected` | `editor.placed_create` with a pivot guid outside the selection |
| `selection_empty` | `editor.placed_create` with nothing selected |
| `level_path_invalid` | `editor.placed_create` path is not relative to `assets/`, or has no level suffix |
| `level_exists` | `editor.placed_create` path is taken, in either form |
| `missing_instance_guid` | a placed-level instance entity has no `EntityGuid`, or it is nil; set a real guid and remove its `PlacedError` to retry |
| `not_placed` | `level.break_placed` or `editor.break_placed` on an entity that is not an inline instance |
| `placed_level_not_open` | `editor.placed_edit` `commit` after another level was opened during the session; nothing was saved and the session ended |
| `side_not_owner` | (not an error) the write was skipped by `peers`; `changed: false` |
| `spawn_failed` | a placed level is in the library but its scene failed to spawn or open (validation, prefab nesting) |

## Notes for game code

- Systems that must not step parked entities filter with `Without<LayerParked>`. `LayerParked.inherited` marks a
  descendant that follows its member.
- `LayerTable::request(layer, state, recursive, side)` is the Rust form of `level.set_layer_state`;
  `LayerChanged { layer, from, to }` is written once per effective change, in layer-name order.
- `regia_scene::level_dir::{read_level_dir, write_level_dir, write_level_dir_dirty}` read and
  write the directory form; `read_level_source` reads either form as the cook input.
