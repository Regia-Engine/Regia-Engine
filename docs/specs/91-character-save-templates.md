# 91 — Character and save framework, third-person and top-down templates

Status: proposed (ADR-1691). Plan item: `master-plan.md` § 3 v0.3.x (b) general-engine completion ("save framework with Steam
cloud backend, First/Third Person templates, samples and guides"), coverage row J4 ("save/load covering ECS, script state and
game blobs; quicksave"). Crates: `regia_framework` (`save_flow`, `character_control`), `regia_steam` (the cloud store),
`regia_agent` (methods), `regia_templates` + `regia_cli` + `regia_editor` (the `top_down` template).

Sources: the Regia docs only (`specs/14` § B4/B5/B7, `specs/24` § 2, `specs/21` § 3, `docs/formats/save-format.md`). No
reviewed public spec exists for this item. Nothing here is taken from another engine.

Built baseline (2026-10-29, `main` 28fb09309): `SaveGame` (save, load, encode/decode byte seam, migrations, autosave on
`Suspended`, `Saveable`, `SaveableResource`), `regia_steam::{SaveBackend, LocalSaveBackend, SteamRemoteStorage}`, the character
movement system with rapier backend, the camera rig with `ThirdPerson` and `Overhead` modes, the `blank`, `first_person`,
`third_person` and `quick` templates, `samples/third_person` (save/load F5/F9, pause menu) and agent methods `save.save`,
`save.load`, `save.list_slots`.

## 1. Purpose and scope

What a game developer still has to write by hand today, and what this spec moves into the engine:

1. **Where saves go.** `SaveGame::save` always writes `SaveDirectory/<slot>.save`; a game that wants Steam Remote Storage
   calls `encode`, stores the bytes itself and loses the agent surface (the agent still writes files). A `SaveStore`
   resource (a `SaveStorage` trait object) makes the destination one switch for the game *and* the agent.
2. **Saving from a system.** `SaveGame` needs `&mut World`; ordinary gameplay systems cannot call it. A `SaveRequest`
   message, processed once per frame at a fixed point, and a `SaveOutcome` message for HUD toasts close that gap.
3. **Quicksave / quickload.** One slot (`Quick` by default), two input actions, the same request path.
4. **Game blobs and slot info.** `SaveBlobs` stores opaque bytes by key (a game's own serialized state, script state a game
   chooses to persist); `SaveInfo` carries display name, level, play time and save count for a load menu. Both are
   ordinary saveable resources inside the existing format.
5. **Move-to for characters.** `MoveTo` drives `CharacterInput` toward a point (point-and-click, AI, agent tests) and reports
   arrival; `camera_relative_move` is the one shared helper both templates use to map a stick to world space.
6. **Top-down template.** `regia new --template top_down`: overhead camera, screen-relative movement, quicksave on F5/F9.
   The `third_person` template gains the same quicksave keys.

## 2. API shape

### 2.1 `regia_framework::save_flow`

```rust
/// Where save-file bytes are kept. The bytes are exactly `SaveGame::encode`'s container.
pub trait SaveStorage: Send + Sync + 'static {
    fn kind(&self) -> &'static str;                         // "file", "steam_remote", ... (agent `save.status`)
    fn write(&self, slot: &str, bytes: &[u8]) -> Result<(), SaveError>;   // atomic
    fn read(&self, slot: &str) -> Result<Option<Vec<u8>>, SaveError>;     // Ok(None) = no such slot
    fn delete(&self, slot: &str) -> Result<bool, SaveError>;              // Ok(false) = was missing
    fn list(&self) -> Result<Vec<String>, SaveError>;                     // sorted slot names
}
pub struct FileSaveStorage { pub dir: PathBuf }            // `<dir>/<slot>.save`, same path as `SaveGame::slot`
#[derive(Resource, Clone)] pub struct SaveStore(pub Arc<dyn SaveStorage>);  // default: FileSaveStorage(SaveDirectory)

#[derive(Resource, Reflect, Default, Clone, Debug, PartialEq)]
pub struct SaveBlobs { pub entries: BTreeMap<String, Vec<u8>> }   // set/get/remove/keys; set_value/get_value (postcard)
#[derive(Resource, Reflect, Default, Clone, Debug, PartialEq)]
pub struct SaveInfo { pub display_name: String, pub level: String, pub play_time_secs: f64, pub save_count: u32 }

#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub enum SaveRequest { Save { slot: String }, Load { slot: String }, Delete { slot: String }, QuickSave, QuickLoad }
#[derive(Message, Clone, Debug, PartialEq)]
pub struct SaveOutcome { pub op: SaveOp, pub slot: String, pub quick: bool, pub result: Result<SaveResult, String> }

#[derive(Resource, Clone, Debug)] pub struct QuickSaveConfig { pub slot: String, pub save_action: String, pub load_action: String }
pub struct SaveFlowPlugin;   // needs SavePlugin; registers the two resources as saveable, the messages, the systems

pub fn save_to_store(world: &mut World, slot: &str) -> Result<SaveResult, SaveError>;
pub fn load_from_store(world: &mut World, slot: &str) -> Result<SaveResult, SaveError>;
pub fn delete_from_store(world: &World, slot: &str) -> Result<bool, SaveError>;
pub fn list_store(world: &World) -> Result<Vec<SlotListing>, SaveError>;  // name, header, has_previous (ADR-3102)
pub fn header_from_bytes(bytes: &[u8]) -> Option<SaveHeader>;
pub fn run_save_request(world: &mut World, request: &SaveRequest) -> SaveOutcome;  // what the drain runs per request
pub fn sanitize_slot_name(name: &str) -> String;
pub fn active_storage(world: &World) -> Arc<dyn SaveStorage>;   // SaveStore, else FileSaveStorage(SaveDirectory)
```

The build returns `SaveGame`'s existing `SaveResult` (entity and resource counts, bytes, and since ADR-3102 which
copy loaded) where this section first said `SaveSummary`; `SaveOutcome::quick` says whether a quick request produced it
(ADR-5657). `SaveStorage` also has default methods for the previous generation (`read_header`, `read_previous`,
`read_previous_header`, `has_previous`; ADR-3102).

Rules:

- `save_to_store` bumps `SaveInfo::save_count` *before* encoding, so the file holds the count it was written as.
- Slot names go through the same sanitizer as `SaveGame::slot` (ASCII alphanumerics, `-`, `_`; others become `_`), so a
  name means the same file on every store. An empty name is refused with `SaveError::Io` of kind `InvalidInput` (no new
  error variant, so existing matches on `SaveError` keep compiling).
- Requests are drained in `Last`, in arrival order, by one exclusive system (`process_save_requests`, tracing span
  `save_flow_requests`). A save and a load in the same frame run in that order. Each request writes exactly one
  `SaveOutcome`.
- The quick actions are read from `regia_input::Actions` when that resource exists (`just_pressed`), in `Update`. Without an
  input plugin the actions are ignored; `SaveRequest::QuickSave` still works.
- Play time accumulates `Time::delta_secs` in `Update` while `SaveInfo` exists (virtual time: a paused game does not count).

### 2.2 `regia_steam` cloud store

`SteamRemoteStorage` and `LocalSaveBackend` implement `regia_framework::save_flow::SaveStorage` (`kind` `"steam_remote"` and
`"user_dir"`), so a game installs Steam cloud saves with one line:

```rust
app.insert_resource(SaveStore::new(SteamRemoteStorage::new(&steam)));
```

A Steam `SaveError` maps to the framework's `SaveError::Io` (`Unavailable` → kind `NotConnected`, `InvalidSlot` → kind
`InvalidInput`, `Io` unchanged) with the path `saves/<slot>.frts`. `regia_steam` gains a normal dependency on `regia_framework` (same layer; `xtask` `SAME_LAYER_ALLOWED` row).

### 2.3 `regia_framework::character_control`

```rust
pub fn camera_relative_move(yaw: f32, axes: Vec2) -> Vec3;   // stick (x right, y forward) → world XZ, length ≤ 1
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
pub struct MoveTo { pub target: Vec3, pub accept_radius: f32, pub slow_radius: f32, pub sprint: bool }
#[derive(Message, Clone, Debug, PartialEq)] pub struct MoveToArrived { pub entity: Entity, pub target: Vec3 }
pub struct CharacterControlPlugin;   // `drive_move_to` in Update (feeds the next frame's movement step)
```

`drive_move_to` runs in `Update`, which the main schedule runs after `FixedUpdate`: the input it writes is what the next
frame's fixed movement steps consume, as for any input written in `Update` (ADR-5657). It writes `CharacterInput::move_dir` = horizontal direction to the target, scaled by
`clamp(distance / slow_radius, 0.15, 1)` inside `slow_radius`. Within `accept_radius` (horizontal distance) it writes zero,
removes `MoveTo` and sends `MoveToArrived`. Vertical distance is ignored (the character cannot fly to a point).

### 2.4 Templates

`crates/regia_templates/templates/top_down/` has game modules `lib.rs`, `level.rs`, `combat.rs`,
`enemy.rs` and `hud.rs`, plus its agent smoke test, desktop editor/launcher, Android entry,
starter level, `assets/config/weather.ron`, config/generator placeholders, manifests and README. It is registered as `TOP_DOWN` and
`template_files("top_down")`; `regia new` and the editor's project browser accept it. The third-person and top-down game
crates add `SaveFlowPlugin` and bind the `QuickSave` and `QuickLoad` actions to F5 and F9 (gamepad bindings are left to the
game). The top-down game uses `CameraMode::Overhead` with a tilted pitch, `RotationMode::OrientToMovement`, and
`camera_relative_move` with the rig's yaw.

## 3. Files and formats

- `.save` stays at **format version 1** (`SAVE_FORMAT_VERSION`, `docs/formats/save-format.md`). `SaveBlobs` and `SaveInfo`
  are saveable resources, stored as resource entries keyed by their type path (`regia_framework::save_flow::SaveBlobs`,
  `regia_framework::save_flow::SaveInfo`). **Migration note:** no migrator is needed. A v1 file written before this change
  has no such entries and loads with both resources untouched; a file written with them and opened by an older engine logs
  "resource ... is not registered as saveable; skipping" for each and loads the rest. The two type paths are now part of the
  save contract: renaming or moving either type needs a `SaveMigrations` hook (the resource map is keyed by path). Adding a
  field to `SaveInfo` follows the binary loading rules (missing trailing fields take defaults).
- Bytes in a non-file store are byte-identical to the file container (magic `FRTS`, version, postcard payload, CRC-32).
  Steam Remote Storage names them `saves/<slot>.frts` (unchanged, `specs/24` § 2).
- Template files are text; nothing frozen changes. `check-formats` fingerprints are unchanged.

## 4. Agent-protocol surface

| Method | Params | Result | Errors |
|---|---|---|---|
| `save.quick_save` | — | `{ slot, entity_count, resource_count, bytes, store, copy }` | `F0003` no save plugin; `F0001` write failed |
| `save.quick_load` | — | same | `F0003` no quick slot yet |
| `save.delete` | `slot` | `{ slot, deleted }` | `F0002` missing `slot` |
| `save.status` | — | `{ store, quick_slot, slots, slots_with_previous, last_load, info: { display_name, level, play_time_secs, save_count }, blob_keys }` (ADR-3102) | `F0003` no save plugin |
| `character.move_to` | `entity`, `x`,`y`,`z`, `accept_radius?` (default 0.25), `sprint?` | `{ entity, target, accept_radius }` | `F0003` not a character or no `CharacterControlPlugin`; `F0002` params |

`save.save`, `save.load` and `save.list_slots` route through `SaveStore` when it is installed (results unchanged for the file
default). Guide topic `gameplay` mentions the new methods; page `book/agents/save-flow.md`; `book/COVERAGE.md` rows.

## 5. Determinism

`SaveBlobs` is a `BTreeMap` (stable key order, stable bytes). Requests are processed in arrival order at one schedule point.
`MoveTo` is a pure function of transform and target per frame. The save payload still holds a wall-clock timestamp, so two
saves of the same world differ in those 8 bytes only; tests compare decoded worlds, not file bytes.

## 6. Tests and acceptance

- `save_flow`: file store write/read/delete/list; a custom in-memory store receives the exact `encode` bytes; request
  save → load round trip restores components and resources; quicksave/quickload through requests and through the actions;
  `SaveInfo::save_count` survives the round trip; `SaveBlobs` typed values round trip; delete of a missing slot reports
  `deleted: false`; empty slot name is an error outcome, not a panic; a v1 file without the new resources loads.
- `regia_steam`: `SaveStore` over `SteamRemoteStorage` (mock) save → load round trip.
- `character_control`: `camera_relative_move` at yaw 0 and π/2; `MoveTo` drives a character on `FlatPlaneQuery` to the
  target within a fixed number of fixed steps and sends one `MoveToArrived`; a character saved with `Saveable` restores
  its transform and movement settings.
- Agent: every new method in `coverage_calls`, schema rows, a round-trip test (`save.quick_save` → `save.status` →
  `save.quick_load` → `save.delete`), `character.move_to` parameter errors.
- Templates: `top_down` writes a full project with no `{{` left; the editor accepts it; the generated `top_down` and
  `third_person` projects **compile** against the checkout (`cargo check` on the build server, recorded in the lane report).

### 6.1 As built (conformance audit 2026-11-10, ADR-5657)

| Item | Test |
|---|---|
| file store write/read/delete/list, missing slot `deleted: false` | `save_flow::tests::file_storage_writes_reads_lists_and_deletes`, `bad_requests_answer_with_errors_not_panics` |
| custom store gets the exact `encode` bytes | `a_custom_store_receives_the_encode_container_and_lists_headers` |
| request save → load restores components and resources; arrival order | `save_and_load_through_requests_restore_the_world`, `requests_of_one_frame_run_in_arrival_order` |
| quicksave/quickload through requests and both actions | `quick_save_and_quick_load_use_the_quick_slot`, `quick_actions_send_requests`, `the_quick_load_action_loads_the_quick_slot` |
| `save_count` and play time survive; paused time does not count | `save_info_counts_saves_and_play_time_survives_the_round_trip`, `a_failed_save_does_not_advance_the_save_count`, `play_time_stops_while_paused_and_follows_the_time_scale` |
| `SaveBlobs` typed and raw round trip | `blobs_round_trip_typed_and_raw` |
| empty slot name: error outcome, `InvalidInput` on every operation | `bad_requests_answer_with_errors_not_panics`, `an_empty_slot_name_is_invalid_input_everywhere` |
| a v1 file without the new resources loads | `a_file_without_the_new_resources_loads_and_keeps_them` |
| Steam store round trip and error mapping | `regia_steam` `tests/cloud_save_store.rs` (`quicksave_and_quickload_go_through_steam_remote_storage` and four failure tests), `cloud_store::tests::errors_keep_their_meaning`, `local_backend_is_a_framework_store` |
| `camera_relative_move`, `MoveTo`, saved character | `character_control::tests::camera_relative_move_follows_the_camera_yaw`, `steer_slows_inside_the_slow_radius_and_stops_inside_the_accept_radius`, `move_to_walks_a_character_to_the_target_and_reports_once`, `a_saved_character_restores_its_transform_and_movement_settings` |
| agent round trip, `character.move_to` errors | `regia_agent` `save_flow::tests::quick_save_status_quick_load_delete_round_trip`, `move_to_validates_and_inserts_the_component`, `save_methods_need_the_save_plugin`, `save_save_load_and_list_follow_an_installed_store` |
| templates: no `{{`, quicksave keys, editor accepts `top_down` | `regia_cli` `new_project::tests::top_down_template_writes_a_playable_starter`, `regia_templates` `character_templates_quicksave_on_f5_and_f9`, `regia_editor` `scaffold` tests |
| generated projects compile | `cargo xtask templates-check` (builds `blank`, `first_person`, `third_person`, `top_down`) |

## 7. Platform notes

Linux, Windows, Android aarch64: pure Rust, no `cfg` in the new code. On Android `SaveDirectory`'s default is relative
(`saves`), unchanged; the store follows it. Steam is desktop-only (`regia_steam` is not built for Android), so the cloud
store is too. macOS builds as the rest of the framework does.

## 8. Performance budget

Per frame with no requests: two empty message reads and one `Time` add (< 0.01 ms). A save or load is a one-frame hitch
proportional to world size (same cost as `SaveGame::save`); the span `save_flow_requests` shows it. `drive_move_to`: one
query over `MoveTo` characters (< 0.05 ms for 200 NPCs on the desktop reference). No GPU work; nothing touches the
renderer.

## 9. Out of scope

Stable entity remapping for `Entity` fields inside saved components (Tier 2, `specs/22`; since built, § 10); async/background saves;
thumbnails captured from the frame; multiple rotating quick slots; nav-mesh paths for `MoveTo` (straight line only; AI
uses `regia_nav` steering); Steam conflict resolution UI; the `templates` CI job (`06` § 5; separate item).

## 10. Stable entity references in saves (ADR-3239)

Lifts the § 9 exclusion "stable entity remapping for `Entity` fields inside saved components". An AI target, a carried item,
a vehicle seat or an owner link now survives save and load and points at the right entity of the loaded world.

### 10.1 Save ids

Every saved entity gets a **save id**: its position in `SaveFileRaw::entities`. The file also carries
`SaveFileRaw::entity_ids: Vec<SaveEntityId>`, one entry per saved entity in the same order, then one entry per *external*
entity (a level entity a saved component points at but that is not itself saved):

```rust
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveEntityId {
    Runtime(u32),                       // a runtime-spawned saved entity: ordinal among the runtime entities
    Level { guid: u128, level: u128 },  // EntityGuid, and LevelMember's level (0 without one)
    Lost,                               // a reference that could not be kept (converted from format 1)
}
```

- **Order** (deterministic, so two saves of one world are byte-identical apart from the timestamp): entities with an
  `EntityGuid` first, by `(level, guid)`, then runtime entities by entity index and generation (spawn-slot order).
  Externals follow, sorted by `(level, guid)`.
- A saved entity with an `EntityGuid` is `Level`; one without is `Runtime(n)`.

### 10.2 Writing references

A component type *holds references* when reflection reaches an `Entity` through its fields: struct and tuple fields,
`Option`, other enums, `Vec`/lists, arrays, map values. The type scan is cached per type.

- The type must register `ReflectMapEntities` (`#[reflect(Component, MapEntities)]`, as for scenes); bevy's `ChildOf` is
  accepted as is. `Children` is not saved (each child's `ChildOf` rebuilds it). A type that holds references and does not
  register it is **not saved**, with the warning `save_entity_ref_unmapped`. So is a type with an `Entity` in a map key or a
  set element (`save_entity_ref_unmapped`, field `<map key>`), which reflection cannot rewrite in place.
- Every `Entity` reached by reflection is written as a marker `Entity` whose index is the target's save id (generation 1);
  `Entity::PLACEHOLDER` stays `PLACEHOLDER` ("no entity"). Fields marked or not marked `#[entities]` are treated alike.
- A target that is not saved and has no `EntityGuid` (or is dead): inside an `Option`, the innermost `Option` is written as
  `None` (note `save_entity_ref_cleared`); with no `Option` around it the component is not saved and the warning
  `save_entity_ref_unsaved` names the entity's save id, the component and the field path (`riders[1]`, `seat.rider`).

### 10.3 Loading references

1. Every component is decoded (as before), then every reference is checked against the table: an index past the table or a
   generation other than 1 refuses the load before the world changes (`SaveError::Corrupt`, message starting
   `save_entity_ref_out_of_range`); a table shorter than `entities` (but not empty) or a `Runtime` entry among the
   externals refuses it with `save_entity_ids_invalid`.
2. The current `Saveable` entities are despawned and **every saved entity is spawned first** (empty, with `Saveable`).
   A load removes only what the save records (ADR-7601): a non-`Saveable` child of a `Saveable` entity is detached first
   (keeping its world pose) and, after step 5, re-parented to the respawned entity with its old parent's `EntityGuid` and
   level (its local transform restored); without such an entity (a runtime parent) it stays a root where it was.
3. Externals resolve against the live world: the entity whose `EntityGuid` and level match (lowest entity id on a tie).
   `Lost` and unmatched externals are *missing*.
4. Every reference is remapped in a second pass. A missing target inside an `Option` becomes `None`
   (`save_entity_ref_cleared`); a plain one skips the component with the warning `save_entity_ref_missing`. Nothing is
   inserted that points at a dead or foreign entity, and nothing panics.
5. Components are inserted (a `ChildOf` finds its parent already spawned).

The notes of the last save or load are in the resource `SaveEntityRefReport` (`op`, `notes: [{ code, entity, component,
field }]`) and in `tracing` (`warn` for `_unsaved`, `_missing`, `_unmapped`; `debug` for `_cleared`).

### 10.4 Format 2 and migration

`SAVE_FORMAT_VERSION` 1 → 2: `entity_ids` is a new trailing field of `SaveFileRaw` (postcard is positional, so format 1
payloads are decoded with the format 1 layout). An **empty** `entity_ids` means the references were never converted (a format
1 file, or one `regia upgrade-project` rewrote without the game's types): the loader converts them first, with the app's
type registry: every non-placeholder `Entity` in a registered component becomes `Lost` (so it loads as `None` or as a
skipped, reported component, as format 1 never kept them). A file with no references converts to nothing, so format 1 saves
load unchanged. The engine handles the 1 → 2 step itself; a game's own `SaveMigrations` hook for version 1 still runs after
it. `regia upgrade-project` rewrites `.save` v1 files to v2 with an empty table. `SaveHeader::from_bytes` reads the shared
prefix of both layouts.

### 10.5 Agent

`save.inspect { slot }` → `{ slot, version, current_version, entities: [{ id, key, components, refs: [{ component, field, target, target_key }] }],
externals: [{ id, key }], legacy_refs, last_report: { op, notes } | null }` (`key`: `runtime:3`, `level:<guid>`, `lost`;
`Entity::PLACEHOLDER` is "no entity" and is not listed). Errors: `F0003` no save plugin, `NOT_FOUND` missing slot, `F0001` unreadable file. Page
`book/agents/save-entity-refs.md`.

### 10.6 Tests

`crates/regia_framework/tests/save_entity_refs.rs` (CPU, `unique_temp_dir`): pawn target, carried item, vehicle with seated
riders, a three-link chain; a reference to an unsaved entity (`Option` → `None`; plain → reported, component skipped); a
missing level entity at load (reported, nothing dangles); a format 1 save through the converter; two saves of one world
byte-identical apart from the timestamp; a hostile out-of-range id refused with its code; an unmapped type reported.
`format_compat`: a v2 fixture with references and a v3 "future" fixture.

### 10.7 Out of scope

`Entity` inside opaque reflected values (a type reflected as one opaque value hides its fields) is written as before, raw:
such a type must not hold entity references in a save. `Entity` as a map key or set element is refused (10.2). References
held by saveable *resources* are not remapped (resources load after entities; a later step).
