# 37 — One-file-per-entity levels, layers and placed levels

Status: proposed (ADR-1578). Plan item G6 (`<scratch>` § 3), target v0.3.x (b),
together with the streaming start. Crates: `regia_scene` (file forms, layers, placed levels), `regia_asset`
(cook, dependency closure), `regia_cli` (conversion command), `regia_agent` (surface), `regia_editor`
(authoring; desktop only).
Reviewed: integ/specs (ADR-1585), 2026-10-28. Built baseline: scene and level loading (below) is implemented; the
streaming planner on `integ/ext2` (ADR-1566) is related but separate.

**Baseline (implemented, what a builder must know).** `regia_scene::format`: `SceneFile { format_version
(`SCENE_FORMAT_VERSION` = 10), entities: Vec<SceneEntity>, resources: BTreeMap<String, Box<RawValue>> }`;
`SceneEntity { guid: Guid (u128, `regia_core::id`), name, parent: Option<Guid>, components: BTreeMap<String,
Box<RawValue>>, prefab_instance }`; migrations run through `migrate_to_latest`. `regia_scene::level`: `LevelId(Guid)`
(the level asset id), `LevelManager::open_level(&mut self, world, LevelId, LoadMode::{Replace, Additive})`,
`LevelLibrary` (loaded `Level`s keyed by id; the seam the asset server fills), `LevelState::{Loading, Active}`,
messages `LevelLoading`, `LevelLoaded { level, entities }`, `LevelUnloading`, `LevelSettings` resource. Cooking:
`regia_scene::cooked::CookedScene::cook(&SceneFile, &TypeRegistry)`. `regia_scene::text_split` splits RON per
element with `RawValue`s. `regia_cli upgrade-project` migrates `.level.ron` files (a table of formats with file
suffixes in `commands/upgrade_project.rs`). `EntityGuid(Guid)` is the component carrying an entity's guid. Spec 12
§ 4a (frozen formats) applies. Nothing in this spec exists yet.

Source area (public-sources spec set, ADR-1526; behavior only): `runtime-worldgrid-external-actors`,
`runtime-worldgrid-data-layers`, `runtime-worldgrid-level-instances`, with `runtime-worldgrid-overview` for context.
Nothing here is copied or edited from them; § 12 records what was taken.

## 1. Purpose

Three problems with today's single `.level.ron`:

1. **Two people cannot edit one level.** One file means one writer and merge conflicts on every placed object.
2. **Whole-level edit/save.** Moving one crate rewrites and re-diffs the whole level file.
3. **No way to switch groups of content at run time or to reuse a whole level as an object.** Prefabs cover one
   subtree; they cannot carry a level's lighting-independent set dressing or be toggled by gameplay.

This spec adds three connected features, none of which changes the shape of a `SceneEntity` record:

- **Directory form** of a level: each entity is its own small file, so version control merges per entity.
- **Layers**: named sets of entities that gameplay or the editor switch together (night props, a quest's
  entities, a destruction state).
- **Placed levels**: a saved level placed inside another level as a single transformed object, either inline
  into the host world or streamed.

The single-file `.level.ron` stays valid and is the default. Cooked output is identical for both forms (§ 3.6).

## 2. Terms (Regia glossary)

| Term | Meaning |
|---|---|
| **Level file** | The existing single-file `.level.ron` (`SceneFile`, `format_version` 10 today). |
| **Level directory** | The new directory form `<name>.level.d/`. Authoring only; never shipped. |
| **Entity file** | One file in a level directory holding exactly one `SceneEntity` record. |
| **Layer definition** | A shareable asset (`.layers.ron`) naming layers: name, kind, debug color, parent. |
| **Layer binding** | A level's use of a definition: initial states plus per-level settings (`LevelLayers` resource). |
| **Layer membership** | The `LayerMembership` component on an entity: the layer names it belongs to. |
| **Placed level** | The `PlacedLevel` component on an entity: a reference to another level plus a placement mode. |

## 3. The directory form

### 3.1 Layout

```
assets/levels/forest.level.d/
  level.ron                  # LevelDirFile: header, resources, layer bindings (no entities)
  entities/
    8f/8f1c3a...(32 hex).entity.ron      # one EntityFile per entity, sharded by the first two hex digits
    2a/2ab0...entity.ron
```

- A level directory is referenced by asset path exactly like a level file: the asset id lives in `forest.level.d.meta`
  next to the directory (`.meta` rules of `specs/12` unchanged). Everything that takes a level reference
  (`LevelManager::open_level`, `PlacedLevel`, `Regia.toml` `entry_levels`, the dependency closure walk) accepts
  either form; the two forms share one asset type, `Level`.
- A path may exist in only one form. If both `forest.level.ron` and `forest.level.d/` exist the loader reports
  `ambiguous_level_form` and loads neither.
- The set of entity files **is** the set of entities; no index lists them. Adding or deleting an entity adds or
  deletes one file, which is what makes merges conflict-free.
- The shard directory is the first two lowercase hex digits of the entity guid (256 shards) so no folder holds
  more than a few hundred files for a 50,000-entity level.

### 3.2 Types (`regia_scene::level_dir`)

```rust
/// Version of the `level.ron` header and entity-file wrappers in a level directory.
pub const LEVEL_DIR_FORMAT_VERSION: u32 = 1;

/// The header file of a level directory (`level.ron`). It holds everything in a `SceneFile` except the entities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LevelDirFile {
    pub format_version: u32,                 // LEVEL_DIR_FORMAT_VERSION
    pub scene_format_version: u32,           // SceneFile version the entity files were written at
    #[serde(default)]
    pub resources: BTreeMap<String, Box<RawValue>>,   // same as SceneFile::resources
}

/// One entity file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EntityFile {
    pub format_version: u32,                 // LEVEL_DIR_FORMAT_VERSION
    pub scene_format_version: u32,
    pub entity: SceneEntity,                 // exactly the record a SceneFile would hold
}

/// Reads a level directory into an in-memory `SceneFile` (sorted by guid) plus the report of what was read.
pub fn read_level_dir(root: &Path) -> Result<(SceneFile, LevelDirReport), LevelDirError>;
/// Writes `scene` as a level directory, touching only files whose bytes changed (see § 3.4).
pub fn write_level_dir(root: &Path, scene: &SceneFile) -> Result<LevelDirWriteReport, LevelDirError>;
/// Lossless conversions used by the CLI and the editor.
pub fn split_level_file(level_file: &Path, out_dir: &Path) -> Result<LevelDirWriteReport, LevelDirError>;
pub fn merge_level_dir(root: &Path, out_file: &Path) -> Result<(), LevelDirError>;
```

`LevelDirReport { entities: usize, files_read: usize, warnings: Vec<String> }`;
`LevelDirWriteReport { written: Vec<PathBuf>, deleted: Vec<PathBuf>, unchanged: usize }`. `LevelDirError`
variants carry the reason strings of § 9.

### 3.3 Reading

1. List `entities/**/*.entity.ron` (shard directories only; anything else under `entities/` is reported as a
   warning and ignored, so editor temp files cannot break a load).
2. Parse each file independently, in parallel on the IO pool when there are more than 256 files. Parsing keeps
   components as `RawValue`s like the single-file loader. Built form (ADR-5779): each entity file is parsed whole as
   one small `EntityFile` record (`ron::from_str`), not through the per-element splitter; the quadratic number scan of
   `text_split.rs` comes from scanning a whole multi-entity document, and a one-record file is linear in its size.
3. Validate each file: its file name guid equals `entity.guid` (`entity_file_mismatch`), its shard directory
   equals the guid prefix (`entity_wrong_shard`, a warning that the writer fixes on the next save), no guid appears
   twice (`duplicate_entity`, both paths listed).
4. Merge into one `SceneFile`, entities sorted ascending by guid bytes, resources from the header. The result is
   then migrated and spawned by the existing pipeline. **Order never depends on directory iteration order.**
5. Version handling: if every file (header and entities) declares the same `scene_format_version`, migrate the
   assembled `SceneFile` once with `migrate_to_latest`. If versions differ, return `mixed_scene_versions`; the
   remedy is `regia upgrade-project`, which rewrites every file at the latest version. A file with a newer
   `LEVEL_DIR_FORMAT_VERSION` or `scene_format_version` than this build fails with `unsupported_version`
   (the same rule as `SceneFile`).

### 3.4 Writing

- The writer renders each entity with the canonical pretty RON configuration used for `SceneFile` (so a single
  entity's bytes are identical in either form's text apart from indentation) and compares to the existing file's
  bytes. Only differing files are written, each atomically (temp file in the same directory, `fsync`, rename).
  The save of an unchanged level touches no entity file.
- Entities absent from the scene have their file deleted and empty shard directories removed. Deleting happens
  after all writes succeed, the header's included; if any write fails nothing is deleted and the error lists what was
  written (ADR-5779: the header write had followed the deletions).
- The header is the last write, so a crash between entity files and header leaves a loadable directory (the header is
  only about resources and versions); deletions come after it.
- Built form (ADR-5779): `LevelDirtyTracker` and `write_level_dir_dirty` exist, but the editor does not feed the tracker
  yet; its save goes through `write_level_dir`, whose byte comparison still writes only what changed. Feeding the dirty
  set from change tracking and undo, and so `level.storage`'s dirty fields and `level_dirty` in the editor, is a v0.5.x
  gap.
- The editor keeps a per-level *dirty set* of entity guids (changed, added, removed) fed by reflection change
  tracking and scene undo; **save writes only that set**, with the byte comparison above as a safety net. A change to
  level resources (settings) dirties only `level.ron`. Entity edits never touch `level.ron`, so a level file is not
  checked out or rewritten when only entities change.
- Replacing one entity with another (the editor's convert-to-prefab, replace-with) hands the dirty set both the old
  and new guid.
- As built (ADR-7203): the editor feeds the set from ECS change ticks (`regia_editor::level_dirty`) each frame the
  undo history changed and before a save; a `.level.d` save uses `write_level_dir_dirty` when the tracking is
  complete for that directory (opened from it or last saved to it), else the whole write.

### 3.5 Conversion (the migrator)

The directory form is an additive layout, not a change of the `SceneEntity` shape, so it needs no migration of
existing levels and the single-file `SceneFile` version does not change. The "migrator required" rule of the plan
is met by two lossless tools and one registered format:

- `regia level split <level.ron> [--out <dir>]` and `regia level merge <dir> [--out <file>]` (CLI, `regia_cli`). The
  round trip `file → dir → file` is byte-identical after both are normalized through the writer (§ 10 test 1).
- `regia upgrade-project` (existing command) rewrites every level directory it finds to the latest
  `scene_format_version` and `LEVEL_DIR_FORMAT_VERSION`, and leaves each level in the form it had.
- The directory form is registered in `cargo xtask check-formats` as format name `level-dir` (v1; schema
  fingerprint = `LevelDirFile` and `EntityFile` fields). A future shape change bumps `LEVEL_DIR_FORMAT_VERSION`,
  adds a step to the directory migrator, and re-records the hash (`cargo xtask check-formats --update`).

Sub-levels and placed levels (§ 6) convert one at a time; converting a host level does not convert levels it
references.

### 3.6 Cook and ship

Cook folds the directory into the same cooked scene as the single-file form: the assembled `SceneFile` is the cook
input, so **the cooked bytes of a level are identical whichever form it was authored in** (acceptance A3). The
shipped pak never contains entity files, `level.ron` of a directory, or any `*.level.d` path; `ship_check` fails a
package that does (it already rejects editor content). Built form (ADR-1953): levels are not cooked blobs, `regia package`
folds a directory into one `.level.ron`, in every profile including named ones, and ships its `.meta`.

## 4. Layers

### 4.1 Model

- **Layer definition asset** `*.layers.ron` (`regia_scene::layers::LayerDefs`, `format_version: 1`):

```ron
LayerDefs(
  format_version: 1,
  layers: [
    (name: "night_props", kind: Runtime, debug_color: (0.1, 0.1, 0.5), parent: None),
    (name: "quest_a",     kind: Runtime, debug_color: (0.8, 0.3, 0.1), parent: None),
    (name: "quest_a_boss",kind: Runtime, debug_color: (0.9, 0.1, 0.1), parent: Some("quest_a")),
    (name: "blockout",    kind: EditorOnly, debug_color: (0.5, 0.5, 0.5), parent: None),
  ],
)
```

  Layer names are unique per definition asset, ASCII `[a-z0-9_]{1,48}`, and are the stable key (renaming a layer
  is a deliberate migration that rewrites member references; the display label is the editor's affair).
- `kind: EditorOnly` layers exist only in the editor (show/hide, load/unload while working) and are dropped from
  cooked output entirely, members included unless they also belong to a Runtime layer. `kind: Runtime` layers are
  cooked and switchable by gameplay.
- **Layer binding** — resource `LevelLayers` in the level (`regia_scene::layers::LevelLayers`, reflected, in
  `resources`): `defs: Vec<AssetRef<LayerDefs>>` plus `entries: Vec<LayerEntry>`; `LayerEntry { layer, initial:
  LayerState, editor_visible: bool, editor_resident: bool, peers: LayerPeers }`. Several levels may bind one
  definition asset with different initial states. A name resolves against the bound definitions in order; a name in
  two bound definitions is `layer_ambiguous`.
- **Membership** — component `LayerMembership { layers: Vec<String> }` (reflected, pinned type path
  `regia_scene::layers::LayerMembership`). Stored with the entity, so changing membership dirties one entity file.
  Unknown names are kept (and reported by validation) so a missing definition never silently drops data.
- `LayerPeers { Everyone, ClientsOnly, ServerOnly }`: which peers resolve the layer. Meaningful once `regia_net` exists;
  in single-player and listen-server tests every `peers` value resolves locally, and writes by the wrong side are no-ops
  returning `false` (the rule is implemented and tested now with a `Side` argument so the later net lane needs
  no semantic change).

### 4.2 States and effective state

`LayerState { Absent, Parked, Live }`:

| State | Meaning at run time |
|---|---|
| `Absent` | Member entities are not spawned. Their data stays in memory in the level's cold template (cooked scene), not on disk. A member with no template record parks instead (ADR-1953 (14)). In play-in-editor the play world's copy is the play level's template, so play despawns too (ADR-2161). |
| `Parked` | Member entities are spawned, carry the marker component `LayerParked`, are not visible, not simulated (physics, scripts, AI skip `LayerParked`), and not pickable. |
| `Live` | Members are spawned, visible and simulated. |

- `requested` is the last state asked for (initial: the binding's `initial`). `effective` is `min(requested,
  effective(parent))` for a layer with a parent, `requested` otherwise. A child can never be more active than its
  parent.
- Descendants of a member that have no `LayerMembership` of their own (a model's part entities, things attached at run
  time) follow it into `Parked` (`LayerParked { inherited: true }`, ADR-1953). A descendant with a membership is governed
  by it.
- An entity with several layers is present at the **lowest** effective state among them: spawned only when every
  one of its layers is at least `Parked`, simulated only when all are `Live`. An entity with no `LayerMembership`
  is not governed by layers. (Reviewer: this intersection rule is a Regia design choice, see § 12.)
- A *recursive* request sets the layer and all descendants to the same requested state. A non-recursive request
  sets only that layer.
- Transitions are applied at a fixed point of the frame (the `LayerSwitch` set in `PostUpdate`, before transform
  propagation) so a frame never observes a half-switched layer. Spawning a layer's members uses the same
  deterministic path as `open_level` (sorted by guid, entity ids allocated in that order).
- `LayerChanged { layer, from, to }` is an event written once per effective change, in layer-name order within
  a frame.
- Switch costs are bounded: switching `Parked ↔ Live` only toggles `LayerParked` (no spawn/despawn) and must
  not allocate per entity beyond command-buffer growth.

### 4.3 API (`regia_scene::layers`)

```rust
pub enum LayerKind { EditorOnly, Runtime }
pub enum LayerState { Absent, Parked, Live }
pub struct LayerTable { /* resource, rebuilt on level load */ }
impl LayerTable {
    pub fn layers(&self) -> impl Iterator<Item = LayerInfo<'_>>;     // name, kind, parent, requested, effective, member_count
    pub fn requested(&self, layer: &str) -> Option<LayerState>;
    pub fn effective(&self, layer: &str) -> Option<LayerState>;
    pub fn request(&mut self, layer: &str, state: LayerState, recursive: bool, side: Side) -> Result<bool, LayerError>;
    pub fn any_in_state(&self, names: &[&str], state: LayerState) -> bool;
    pub fn all_in_state(&self, names: &[&str], state: LayerState) -> bool;
}
```

`request` returns `Ok(true)` when the requested state changed, `Ok(false)` when it was already that state or
the `side` does not own the layer (§ 4.1 `peers`), and `Err` for an unknown layer. It never panics on user data.

### 4.4 The current layer set

The editor keeps a **current layer set**: entities placed while it is non-empty join those layers automatically
(written into `LayerMembership` of each new entity). Clearing the set stops auto-assignment and leaves existing
members alone. Removing an entity from a layer leaves it in the world. Deleting a layer from a definition asset is
refused while any level binds it with members (`layer_in_use`); `regia validate` lists the members. As built
(ADR-7203): `regia_scene::layer_delete::delete_layer` and agent `editor.layer_delete` (the open level's unsaved
members count); a layer that is another's parent is refused with `layer_parent_invalid`.

### 4.5 Validation (validators contributed to spec 32's framework)

Reported, not fatal: `LayerMembership` naming an unbound layer; a layer cycle or a parent that does not exist
(`layer_parent_invalid`, fatal for the definition); an editor-only layer that is the parent of a Runtime layer;
a member entity whose own parent entity is in a different layer set so that the parent can be absent while the
child is present (`layer_parent_mismatch`: a child must be in all layers its entity-parent is in, or the parent
must have no layers).

## 5. Per-entity storage and layers together

`LayerMembership` is an ordinary component, so a layer change is a one-entity-file diff. Level resource `LevelLayers`
lives in `level.ron`, so changing a layer's *initial* state dirties the header only.

## 6. Placed levels

### 6.1 Component

```rust
#[derive(Component, Reflect)]
pub struct PlacedLevel {
    pub level: AssetRef<Level>,
    pub mode: PlacedMode,           // Inline (default) | Streamed
    pub filter: PlacedFilter,       // which contained entities participate (layers include/exclude), default: all
}
pub enum PlacedMode { Inline, Streamed }
pub struct PlacedFilter { pub layers: Vec<(String, bool)> }   // (layer name, include?) ; empty = everything
```

The instance entity carries a `Transform`; its `GlobalTransform` is the root transform applied to every contained
entity. Contained entities are in the instance's coordinate space.

### 6.2 Inline mode (default)

At the instance's load, every entity of the referenced level that passes the filter is spawned as a **child
subtree of the instance entity** (the level's root entities become its children; their `parent` links stay
inside the subtree). Properties:

- Entity identity: each spawned entity's guid is `derive(placement_guid, source_guid)` where `derive` is the 128-bit
  hash `blake3(placement_guid ‖ source_guid)` truncated to 16 bytes. The same instance always spawns the same ids;
  two instances of one level never collide. The placement guid is the instance entity's `EntityGuid`: an instance
  without one, or whose guid is nil (`EntityGuid`'s default), is refused (`missing_instance_guid`), never derived
  from process-local entity ids, which would make the ids depend on how many entities were spawned before the
  placement. A nil id would make every such instance derive the same member ids.
- The referenced level's `resources` (settings, environment, baked data) are **ignored**: a placed level
  contributes entities only; the host level's settings win. A diagnostic lists ignored resources once per instance
  fill (built form, ADR-5779; a respawn after Commit fills again).
- Contained entities receive the marker `PlacedMember(instance_entity)` and are not selectable as separate
  roots in the editor outliner (shown nested, read-only).
- Layers: members inherit the instance entity's `LayerMembership` and keep their own additional ones. The effective
  state rule of § 4.2 applies to the union.
- Inline instances cost one spawn pass; they contribute to the host's culling and streaming as if placed
  directly. Repeating the same instance many times is the intended use.

### 6.3 Streamed mode

The instance loads the referenced level through `LevelManager` in additive mode when the instance entity itself
is loaded, parented under a transform-carrying root so the instance transform applies. It adds a stream-level
entry per loaded instance, so heavy repetition is discouraged (the editor warns above 32 streamed instances in
one level; budget in § 11). It unloads when the instance entity despawns. Order guarantee: the instance entity
exists first, the placed level's `LevelState` reaches `Active` after it. The existing `LevelLoaded { level, entities }` message
keeps its shape (adding a field would break struct literals); a new message `PlacedLevelLoaded { owner: Entity,
level: LevelId }` is written beside it so scripts can wait for the owner.

### 6.4 Limits and failure

- Nesting depth ≤ 8 and total spawned entities per instance ≤ 100,000; exceeding either reports `placed_limit`
  and spawns nothing for that instance.
- A cycle (a level instancing itself, directly or transitively) is refused at placement and at load
  (`placed_cycle`), naming the chain.
- A missing or unloadable level leaves the instance entity in place with a `PlacedError(reason)` component and
  logs once per load; the game keeps running.

### 6.5 Editing, committing, breaking (editor)

- **Create:** select entities → *Create placed level*: choose a path, pivot (lowest-point center / center / a
  named member) and storage form; the selected entities move into the new level (guids kept), re-expressed
  relative to the pivot, and an instance entity takes their place at the pivot with the original world transform.
  Undoable as one action. Built by ADR-1953 (`editor.placed_create`): the pivot translates only, and an undo leaves the
  new level's files on disk.
- **Edit:** opens the referenced level in the editor on its own (built form, ADR-1953: the host is closed during the
  session, where this spec first said dimmed and non-selectable; `editor.placed_edit`);
  *Commit* saves the referenced level and every copy updates (live instances respawn; ids are stable so selection
  and overrides by derived guid survive); *Discard* drops the session. Editing never touches the host level.
- **Break:** replaces the instance entity with plain entities (derived guids become the permanent guids), keeps the
  source level asset, and is one undo step.
- Per-instance property overrides are out of scope in v1 (a prefab instance covers that need).
- As built (ADR-7614): *Break* in the editor is `regia_editor::placed_break::break_placed_undoable` (agent
  `editor.break_placed`; the Layers panel and the Outliner row menu): one undo step whose undo despawns the former
  members, respawns the instance from its saved record and fills it again, so the members get the same derived guids
  and the level saves to the same bytes. *Edit* has a button in the Layers panel (selected instance) and the Outliner
  row menu. Layer definitions and bindings have UI and agent methods too: `editor.layer_create` (adds a layer, creating
  the `.layers.ron` and its `.meta` if needed; a file edit), `editor.layer_bind` (binds or unbinds a definition to the
  open level) and `editor.layer_initial` (the binding entry's `initial`); the last two edit the open level's
  `LevelLayers` (its `LevelLibrary` copy, which a save writes) as one undo step each and dirty the header only (§ 5).

## 7. Streaming interaction

World streaming has a pure-logic planner on `integ/ext2` (ADR-1566: `regia_scene::{stream_grid, stream_plan, hlod,
floating_origin}`, agent `scene.stream_plan`; not on main yet) that decides which cells load; it spawns nothing and has
no on-disk format. Spawning by cell is a later lane. This spec only fixes the contract that lane needs:
(a) entity files are individually loadable by guid from a level directory in the editor, so an editor can open a
partial region without reading every file; (b) cooked scenes keep per-entity records addressable by guid so
streaming can load them by cell. Neither requires a format change here. When cell streaming spawns entities, layer rules apply to them
through the same path as `open_level` (a member whose effective state is `Absent` is not spawned).

## 8. Agent protocol (`book/agents/levels-layers-instances.md`)

All methods are lowercase `level.*`. Errors use the standard `data.reason` strings of § 9.

| Method | Mode | Params | Result |
|---|---|---|---|
| `level.storage` | read | `level?` (path, default current) | `{ form: "file"\|"dir", entities, header_dirty, dirty_entities: [guid], files }` |
| `level.convert` | edit scene | `path`, `to: "file"\|"dir"` | `{ written, deleted, unchanged }`; refuses when the editor has unsaved changes to that level (`level_dirty`) |
| `level.layers` | read | `layer?` | `[{ name, kind, parent, requested, effective, members, peers }]`, ordered by name; built form `{ layers, despawned, parked }` (ADR-2161) |
| `level.set_layer_state` | play / edit scene | `layer`, `state: "unloaded"\|"loaded"\|"activated"`, `recursive?` (default false) | `{ changed: bool, effective }` |
| `level.set_membership` | edit scene | `entity` or `entities`, `add?`, `remove?` | `{ updated: n }`; edits `LayerMembership` |
| `level.placed` | read | none | `[{ entity, level, mode, spawned: n, error? }]` |
| `level.place_level` | edit scene | `level`, `transform` (`translation?`, `rotation?` `[x, y, z, w]`, `scale?`; ADR-5779), `mode?`, `parent?` | `{ entity }` |
| `level.break_placed` | edit scene | `entity` | `{ entities: [guid] }` |

Guide page content: the three tasks (split a level, toggle a layer from a test, place and break an instance), the
state table of § 4.2, the intersection rule, and the reason list. `book/COVERAGE.md` gets one row per method.

## 9. Error reasons (`data.reason`)

`ambiguous_level_form`, `entity_file_mismatch`, `entity_wrong_shard`, `duplicate_entity`, `mixed_scene_versions`,
`unsupported_version`, `level_dirty`, `layer_unknown`, `layer_ambiguous`, `layer_in_use`, `layer_parent_invalid`,
`layer_parent_mismatch`, `placed_cycle`, `placed_limit`, `placed_not_found`, `level_not_found`,
`not_placed`, `side_not_owner` (a write skipped by `peers`; also returned as `changed: false`).

## 10. Tests and acceptance (tests first)

Tests live in `crates/regia_scene/tests/level_dir.rs`, `layers.rs`, `placed.rs`, the agent crate's
`level_tools.rs`, and `regia_cli` tests. Temp paths use `unique_temp_dir`; files written atomically; no wall-clock
asserts.

1. **Round trip.** Fixture levels (the `level.example.ron` set, a 2,000-entity generated level, a level with prefab
   instances and nested parents): `split → merge` equals the original after normalization; `merge → split` leaves
   every entity file byte-identical.
2. **Deterministic read.** Shuffle the creation order of entity files in a temp dir (three seeds); the assembled
   `SceneFile` is identical (guid order).
3. **Cook equivalence (A3).** Cook the same level authored as a file and as a directory; cooked bytes are equal.
4. **Dirty-only save.** Change one entity's `Transform`; only that file's bytes change (assert via the write
   report, not mtimes). Change a level resource; only `level.ron` changes. Delete an entity; its file and an empty
   shard directory vanish. Inject a write failure (read-only directory) on the third file: nothing is deleted and
   the error lists the two written paths.
5. **Validation.** Each of the read-time reasons (`entity_file_mismatch`, `duplicate_entity`, `mixed_scene_versions`,
   `ambiguous_level_form`, junk file in `entities/`) produces its reason and no panic. A fuzz target feeds random
   bytes as an entity file (`cargo fuzz`, parser must return `Err`, never panic).
6. **Layers.** Initial states apply at load; `Absent → Parked → Live → Parked → Absent` spawn, hide, show,
   hide, despawn exactly the expected entities (assert entity counts and the `LayerParked` marker); recursion;
   child clamped by parent; multi-layer entity follows the lowest effective state; same input sequence twice gives
   identical entity ids and `LayerChanged` order; `peers`: a `ServerOnly` layer written with `Side::Client` is a
   no-op returning `false`.
7. **Placed levels.** Inline: derived guids are stable across loads and distinct across two instances of one level;
   level resources ignored with one diagnostic; filter include/exclude by layer; transform applies to contained
   entities (world positions asserted); a cycle, depth 9 and a missing level produce their reasons and leave the
   host level loading. Streamed: contained entities appear only after the instance entity, `PlacedLevelLoaded` names the
   owner, despawn unloads. Break: guids kept, no `PlacedMember`, source asset untouched.
8. **Agent protocol (rule 8).** Every method in § 8: success, missing parameter, each reason it can return; the
   no-plugin degradation answers an empty list. Dogfood script: `regia_agent` toggles a layer in play mode and
   `play.wait_until` sees the expected entity count.
9. **Android/Windows.** `regia_scene` tests run on Linux and Windows; the Android build only needs the
   `cargo ndk … check`.

Acceptance, as the lead reads it: `cargo test -p regia_scene -p regia_asset -p regia_agent -p regia_cli`;
`cargo xtask check-formats --update` then `check-formats`; `cargo xtask manual`; `cargo xtask agent-coverage`;
`regia level split` / `merge` round trip on DO27-shaped fixtures is not required (DO27 is out of scope).

## 11. Performance, determinism, platforms

- Directory read, desktop SSD, warm cache: 10,000 entity files < 400 ms with the IO pool (single-file budget of 50
  ms applies to the **cooked** scene, which is what ships). Dirty-only save of 10 changed entities < 50 ms.
- `request` applying to a layer of 1,000 members: `Parked ↔ Live` < 0.5 ms; `Absent ↔ Parked` bounded by
  the existing spawn budget (prefab instantiation target, `specs/14` A3). `tracing` spans: `level_dir_read`,
  `level_dir_write`, `layer_switch`, `placed_spawn`.
- Determinism: entity order, id allocation, derived guids and event order are functions of file content only.
- Platforms: the directory form is authoring only (Linux/Windows editor, CI). The shipped game on Android
  aarch64 (and every platform) reads the cooked scene, so no Android file-system code is added; layers and
  instances run on all targets. Filenames are 32 hex characters plus `.entity.ron` (≤ 45 bytes), safe on every
  filesystem including case-insensitive ones (lowercase only).

## 12. Out of scope

Source-control integration (changelist decoding, status icons, pre-submit viewers) — the files are plain text, use
the project's VCS. Hierarchical LOD proxies of a level. Per-instance property overrides. Networking replication of
layer state beyond the `peers` field. A "reduced" alternative file layout. Partial-region *runtime* streaming. A
conversion of existing legacy layer systems (none exists). Editor panel layout is left to the editor lane (it needs:
a layers panel with visibility/loaded toggles, a current-set indicator, and an instance edit banner).

### Provenance

Public user documentation of per-object level storage, switchable layers and placed sub-levels, read for behavior
only (ADR-1526); set areas `runtime-worldgrid-external-actors`, `runtime-worldgrid-data-layers`,
`runtime-worldgrid-level-instances` (overview for context). Behavior: authoring-only per-entity files folded at cook;
the level file untouched by entity edits; per-entity dirty tracking; a definition/binding split with per-level
initial states; three runtime states with requested vs effective reads; parent clamping; a placed level as one
transformed object with inline vs streamed modes, inherited membership, edit/commit/discard and break. Regia's own
choices: the directory layout and sharding, guid-derived placement ids (blake3), the lowest-state intersection rule
for multi-layer entities, `min(parent)` clamping, the limits, `peers` as data, and every type, state and reason name
(renamed during review so no set literal remains: states `Absent/Parked/Live`, `LayerMembership`, `PlacedLevel`,
`PlacedMode::{Inline, Streamed}`). Row in `docs/provenance.md` (ADR-1578); this section is the spec's own record.

#### Public-page citations

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

## 13. Decisions and owner questions

- Decision (ADR-1585): the intersection rule stays (an entity is present at the lowest effective state of its
  layers); a union cannot express "only during night AND quest A".
- Decision (ADR-1585): `LevelLoaded` keeps its shape; `PlacedLevelLoaded` and `LayerChanged` are new messages.
- Decision (ADR-1585): `LayerParked` is a marker component, not `bevy_ecs` `Disabled`. Parked entities must not
  step: the builder adds `Without<LayerParked>` to the physics body sync, script run, AI tick, picking and render
  visibility queries (and hides them with `Visibility::Hidden`), and lists every touched system in the PR.
  Parked members' rigid bodies are not inserted into the physics world until `Live`. Built by ADR-1953; a
  `VehicleController` keeps its chassis, disabled, because the vehicle has no rebuild path. A terrain is drawn by its
  host, not by a query: the editor's terrain bridge skips a parked terrain, and a game host does the same (ADR-2161).
  As built (ADR-7200): in a game, `regia_terrain::parked_draw` withholds the renderer's shared terrain draw while
  every `Terrain` is parked or hidden and puts the same draw back when one goes live; a parked `CrashSequence`
  stops its effects and holds its clock (plumes and hover washes already did); every render extraction path also
  skips hidden entities. A `LayerMembership` on an entity without an `EntityGuid` is switched too (after the guid
  carriers, in entity order), and an `Absent` layer parks it, since it has no template record.
- Decision (ADR-1585): layer definitions are format `layers` v1 (`.layers.ron`), the directory form is format
  `level-dir` v1; the lead registers both with `cargo xtask check-formats --update` and writes
  `docs/formats/layers-ron.md` and `docs/formats/level-dir.md`. `LevelLayers`, `LayerMembership` and
  `PlacedLevel` are reflected components/resources stored in existing scene files, so no `SCENE_FORMAT_VERSION`
  bump is needed (new type paths are additive; unknown types are skipped by older builds with a warning).
- Decision (ADR-1585): no new dependency (blake3 is already a workspace dependency); no `unsafe`; no new crate.
- Decision (ADR-1585): `regia upgrade-project` gains a `level-dir` entry in its format table (it rewrites every file
  of a level directory at the latest versions, in the form found).
- Owner decisions: none.

## 14. Builder notes

Size L. Order: `level_dir` read/write and the `regia level split|merge` commands (with cook equivalence), layers
(definitions, membership, `LayerTable`, switch set), placed levels (inline first, then streamed), agent methods and
the book page `book/agents/levels-layers-instances.md` with COVERAGE rows, then the editor lane. Streaming spawn by
cell (ADR-1566's planner, spec 40 for content chunks) builds on the layer rules here. Spec 41's session harness can
drive `level.set_layer_state`.

As built (ADR-7792, v0.6.0 parity rows P-096/P-097): the editor locks layers (members not pickable in the viewport,
kept per level in `.regia/editor_view.ron`), stores a layer in its own file `<level>.<layer>.layer.ron` (format
`layer-file` v1, `docs/formats/layer-file.md`; every level reader folds it back in, `regia package` ships it folded),
and keeps several levels open at once (the host, levels opened beside it and its layer files, each a document with
its own unsaved state from tagged undo entries, its own Save and an active level for new entities). Agent page
`book/agents/levels-layers.md`.
