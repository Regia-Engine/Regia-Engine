# Spec 14 — Scene (`regia_scene`) and Gameplay Framework (`regia_framework`, `regia_macros`)

## Part A — Scene

### A1. Transforms and hierarchy
- `Transform { translation: Vec3, rotation: Quat, scale: Vec3 }` (local, relative to parent).
- `GlobalTransform(Affine3A)` computed in `PostUpdate/TransformPropagate` by one exclusive system that walks
  only the dirty subtrees from each root in turn (change detection is a per-entity snapshot diff, so a direct
  call works too; ADR-5613). `PreviousGlobalTransform` kept for motion vectors/interpolation.
- Hierarchy via bevy_ecs relationships (`ChildOf(parent)` component; `Children` maintained
  automatically). API helpers: `commands.entity(e).set_parent(p)`, `remove_parent`, `despawn_recursive`
  semantics, and `set_parent_keep_world_transform` / `remove_parent_keep_world_transform` to keep the world
  transform when reparenting. `despawn_recursive` and
  `hierarchy::despawn_tree(world, root)` walk the subtree with an explicit stack and despawn it leaves
  first, so no depth overflows the stack (bevy's own `World::despawn` recurses per level; ADR-2306).
- `Name`, `EntityGuid`, `Tags(SmallVec<[Name; 4]>)`, `Layers(u32)` (one stored and saved bit set; no
  subsystem reads it yet, and physics filters by its own `CollisionLayer`, spec 15 § 3), `Static` marker
  (ADR-5704, ADR-5906): `extract_frame` copies it into the frame (`FrameMesh::is_static`). It does
  not change drawing or caching: the renderer already caches by content (unchanged instance bytes
  are not re-uploaded, a held shadow caster keeps its pages), so static and dynamic instances of
  one mesh and material share one GPU batch and a static caster caches like any other.
  `GpuBatch::is_static`/`DrawStats::static_batches` and `ShadowCacheReport::static_casters` count
  them as diagnostics (`regia_render::static_batch_eligible`: Static, rigid, drawn whole). The
  editor warns, play world only and once per entity, when a `Static` entity's transform moves at
  runtime and counts every move (`regia_editor::static_moves`, agent method
  `editor.static_move_warnings`). Merging static meshes into fewer draws is not implemented.
- `Visibility { Inherited, Visible, Hidden }` → `InheritedVisibility(bool)` → `ViewVisibility`. As built
  (ADR-7200): every render extraction path skips an entity whose `InheritedVisibility` is false (one shared
  filter, `regia_render::is_drawn`), `MeshRenderer` requires `InheritedVisibility` and `ViewVisibility` so a mesh
  child of a hidden parent is hidden (ADR-7549: so do `DirectionalLight` and `AmbientVolume`; an entity that leaves
  propagation's tracked set, losing its last `ChildOf`, `Transform` and `Visibility`, is reset to visible), and
  culling writes `ViewVisibility` each frame (true when a draw of the
  entity passed some view's layer mask and frustum). As built (ADR-7255): skinned draws are tested at their posed
  bound (the joint palette moves each joint's bind-space box, ADR-248; dual-quaternion and eight-influence skins
  pass whole), and with one view the two-phase occlusion result folds in: the depth prepass cull's commands are
  read back without a stall (`regia_render::OcclusionFeedback`), so `ViewVisibility` reads last frame's
  occlusion, a one-frame lag when the GPU keeps up (longer under a deeper queue, never newer): a mesh uncovered
  on frame N reads true from frame N + 1. Draws the cull does not test (blended, visibility-buffer clusters,
  frames without the cull or with several views) stay frustum-only. As built (ADR-7286): each window view has its
  own readback (split screen, `FrameRenderer::set_occlusion_view`), and an entity is visible when any view sees
  it: a view without the cull (a texture camera) is frustum-only for that view only. Visibility-buffer clusters
  fold in per instance from the cluster cull's visibility bits, read back the same way (an instance none of
  whose bits is set drew nothing); only blended draws, persistent instance sets and frames without a cull stay
  frustum-only. `regia_render::ViewVisibilityPerView` and `render.visible_entities` `views` report each view.
  Each window view culls against its own history (ADR-7329: a Hi-Z pyramid and cluster bits per view), so an
  occluder only one split-screen view sees hides what is behind it in that view. A view's cluster LOD history is
  its own too, and a view undrawn for 16 rounds gives its history back on its own (ADR-8620). A hidden terrain leaves the editor viewport
  too, and a hidden (not parked) static decal stops drawing while its decal stays in the vfx pool (filtered at
  draw time). Agent: `render.visible_entities` (with the occlusion count).

### A2. Scene data format (`.level.ron`, `.prefab.ron`)
```ron
SceneFile(
  format_version: 1,
  entities: [
    (guid: "8f1c...", name: "Door", parent: Some("2ab0..."),
     components: {
       "regia_scene::transform::Transform": (translation: (0.0, 0.0, -3.0), rotation: (0,0,0,1), scale: (1,1,1)),
       "regia_render::MeshRenderer": (mesh: "asset:6b2e...", materials: ["asset:91d0..."]),
       "my_game::Door": (open_angle: 1.57),
     }),
    (guid: "c0aa...", prefab_instance: (
       prefab: "asset:44fe...",
       overrides: [ (target: "5d2e0c41-7a63-4c1e-9b0f-3a1c8e6d2f77", component: "regia_render::PointLight", path: ".intensity", value: 800.0) ],
       added_components: [], removed_components: [], added_children: [], removed_children: [] )),
  ],
  resources: { "regia_scene::level::LevelSettings": (...) },
)
```
- Prefab-internal entities are addressed by **stable local ids** (guid within the prefab), not names,
  in `target`; the editor displays names.
- Nested prefabs: a prefab can contain prefab instances; overrides compose outer-wins.
- Cooked form: postcard with type paths hashed + a string table; load speed target: 10,000 entities
  < 50 ms on desktop.

### A3. Prefabs
- `PrefabInstance { prefab: u128 (the prefab's asset guid), overrides: ... }` component on the instance root; child
  entities carry `PrefabLink { local_id }`.
- Spawning: `world.spawn_prefab(guid, Option<Transform>, &SpawnOptions, root_guid) -> Result<Entity, SceneSpawnError>` (async if not
  loaded: returns the entity immediately with `PendingPrefab`, fills when loaded); the caller supplies the instance
  root's `EntityGuid` (a lockstep game draws it from the sim RNG, so peers derive identical prefab-internal guids;
  nil or already-used guids are refused) and `instantiate_prefab` refuses a root without one. The `Commands` form takes
  the same four
  arguments, returns `&mut Self` and only logs a `tracing` warning on error.
- Editor: when a prefab asset changes, live instances are rebuilt preserving overrides (the stored instance
  record is re-applied). Revert override. Apply override writes the prefab's `.prefab.ron` when the open
  project has it (atomic, current format; a nested prefab's entity becomes an outer-wins override on the
  nested record, the inner file untouched); other instances take the value unless they override it, one undo
  (ADR-7203). On an instance of a prefab variant, Apply moves the override into the variant's
  `.variant.ron` (atomic, current format; the base file untouched; other instances of the variant take the
  value in place; agent `editor.prefab_apply_override`, ADR-7263). A template with no file keeps the
  session-only Apply. Break link removes `PrefabInstance` /
  `PrefabLink` and keeps the entities with their values, one undo step (`editor.prefab_break_link`, ADR-7203).
- Agent: `scene.spawn_prefab` spawns an instance at runtime (book/agents/runtime-gameplay.md).
- Runtime cost: prefab instantiation = clone of a pre-baked `PrefabTemplate` (component values stored
  reflected → `ReflectComponent::insert`); target 1,000 instantiations of a 20-entity prefab < 16 ms (not
  measured yet).
- Names: a prefab entity's record `name` becomes its `Name` on every instance, inserted before the baked
  components, so an explicit `Name` component and `Name` overrides still win (ADR-4928). The instance root
  keeps its own record's name.

### A4. Levels and world
- `Level` asset = scene file + `LevelSettings` (environment, sky (`atmosphere`), gravity, `game_rules` (a name
  registered with `GameRulesRegistry`), default post settings (`look`), ambient audio, nav settings (Tier 2,
  not built), streaming (Tier 2, `streaming_enabled`)).
- `LevelManager` resource: `open_level(world, id, LoadMode::Replace|Additive)`, `unload_level(world, id)`,
  `current()`. Loading is async (states `Loading` → `Active`); the load progress `0..1` is
  `regia_framework::level_progress::level_load_progress` (the `level:<guid>` load set while loading, `1` when
  active) and agent `level.status` (ADR-7203); `Replace` mode shows the project's loading-screen UI
  prefab while loading (as built, ADR-7263: `Regia.toml` `[project] loading_screen` = a `.prefab.ron` under
  `assets/` or `asset:<guid>`, read by `regia_framework::loading_screen::LoadingScreenPlugin` into
  `LevelManager::loading_screen`; the manager spawns one instance, root marked `LoadingScreen { level, progress }`,
  when a `Replace` open waits for its data, and despawns it when no `Replace` open waits any more: completed,
  failed or unloaded; `progress` follows the load and never goes down; `level.status` names the entity; an
  `Additive` open or one whose data is already present shows none), then despawns the old level's entities (all entities carry `LevelMember(id)`;
  one also marked `Persistent` is kept, and adopted when its level is reopened).
- Events: `LevelLoading`, `LevelLoaded`, `LevelUnloading` (before the level's entities despawn). Startup level
  from `Regia.toml`: the editor opens it; at runtime `StartLevelPlugin` opens `[project] startup_level` when the
  game's own `Startup` opened no level, and `regia run --level` overrides both (ADR-7203).
- Streaming volumes / world streaming: Tier 2 (`specs/22` T2.11).
- Agent: `level.open` / `level.unload` drive the LevelManager at runtime (book/agents/runtime-gameplay.md).

## Part B — Gameplay framework

### B1. Game plugin entry
```rust
pub trait GamePlugin: Send + Sync + 'static {
    fn register(&self, app: &mut App);               // types, scripts, systems, rules, assets
    fn on_unload(&self, _world: &mut World) {}       // joins game threads, writes their state; runs before the snapshot
    fn on_load(&self, _world: &mut World) {}         // restarts those threads after restore (specs/20 § 3)
}
// in regia_framework, reached as regia::framework::export_game!
#[macro_export] macro_rules! export_game { ($t:expr) => { /* regia_game_entry + REGIA_ABI_HASH (specs/20 § 2) */ } }
```

### B2. Scripts
```rust
#[derive(Script, Reflect, Default, Clone)]
#[reflect(Default)]
pub struct Door {
    #[reflect(@Range(0.0..=3.14))] pub open_angle: f32,
    pub opens_for: AssetRef<Prefab>,
    #[reflect(ignore)] t: f32,
}
impl Script for Door {
    fn start(&mut self, ctx: &mut ScriptCtx) {}
    fn update(&mut self, ctx: &mut ScriptCtx) { let dt = ctx.delta_secs(); ... }  // or ctx.time().delta_secs
    fn fixed_update(&mut self, ctx: &mut ScriptCtx) {}
    fn on_event(&mut self, ctx: &mut ScriptCtx, ev: &ScriptEvent) {
        if let ScriptEvent::TriggerEnter { other, .. } = ev { ... }
    }
    fn on_message(&mut self, ctx: &mut ScriptCtx, msg: &dyn Reflect) {}
    fn on_destroy(&mut self, ctx: &mut ScriptCtx) {}
}
```
- Storage: component `Scripts(SmallVec<[ScriptSlot; 2]>)`, where `ScriptSlot { type_path, order: i32, instance: Option<Box<dyn Script>>, started: bool, enabled: bool }`.
- Execution: one exclusive system per schedule (`Update`, `FixedUpdate`, plus a late-update variant in
  `PostUpdate` before transform propagation; sets `ScriptsSet::{FixedUpdate, Update, Late}`). For each entity with scripts: **take** the boxed script out
  of its slot (pointer move, no archetype change), call it with `ScriptCtx { world: &mut World, entity }`,
  put it back. A script whose entity was despawned during its own call is dropped after `on_destroy`.
- `ScriptCtx` methods: `entity()`, `world()` (`&mut World`), `get::<C>()`, `get_mut::<C>()`,
  `insert`, `remove`, `transform_mut()`, `spawn(bundle)`, `despawn()` (this entity),
  `find_by_name`, `with_script::<S>(entity, f)`, `send_message`, `broadcast`, `time()`,
  `delta_secs()`, `action2d(name)`, `action_pressed(name)` (both read the global
  `regia_input::Actions` resource, not a per-player map), `after`, `every`, `start_coroutine`,
  `send_message_in`, `rand_f32`, `rand_range`, `commands()`, `physics()` (spec 15 § 3 queries), `teleport()` (spec 15
  § 1, ADR-7233).
- `ScriptEvent` sources shipped today: physics only (`CollisionStarted`, `CollisionEnded`,
  `TriggerEnter`, `TriggerExit`, `ContactForce` with `total_force`, ADR-7233), queued by `ScriptPlugin` and delivered on `Update`. Animation
  notifies are `AnimNotify` messages, not script events (spec 17 § 3). UI, input, possess, damage
  and level events are not `ScriptEvent` variants.
- Ordering: scripts run in **execution order** groups (`#[script(order = -100)]`), then entity spawn
  order. Deterministic.
- Editor: scripts appear as components in the inspector; "Add Script" lists registered script types.
- Plain ECS systems remain fully supported for hot paths; scripts and systems can query each other's
  data (systems can `Query<&Scripts>` and downcast).

### B3. Game rules, players, controllers
- `GameRules` trait object resource chosen by `LevelSettings.game_rules` (default `DefaultRules`):
  `on_level_start`, `on_player_joined(player) -> spawn point/prefab`, `on_player_left`,
  `choose_spawn(player)`, `on_character_died`, `tick(ctx)`, `is_match_over(&World)`. Implemented with a
  `RulesCtx` context. The framework calls `on_player_joined`, `on_player_left` and `choose_spawn`; the game
  calls `on_level_start`, `tick` and `on_character_died` from its own systems (ADR-5613).
- `Player { index: u8, kind: Local|Remote(Tier2), controller: Entity }` entities. `PlayerManager`
  handles local join (press button on new gamepad) and split-screen camera layout; the client build
  writes each player's rect onto the cameras tagged `SplitScreenCamera { player }` (ADR-7880). The
  `MaxLocalPlayers` resource (default 4) is copied onto `PlayerManager.max_local` when it is first
  seen and is not the default, or whenever its value changes after that. A manager cap written
  directly is left alone while the resource stays at 4.
- `PlayerController` component: owns the input context stack for its player, `possess(entity)` /
  `unpossess()`, camera view target. `AiController` (Tier 1: just a possession target driven by scripts;
  Tier 2 adds behaviour trees).
- `PlayerStart` marker entity. Default spawn takes the first one in entity order and ignores tags.
  Custom `GameRules` may filter by tag.
- Agent: `rules.get` reports the active rules, the match-over flag and the level's rules name
  (book/agents/runtime-gameplay.md).

### B4. Character movement
- `Character` component + `CharacterMovement` settings: capsule (radius, cylindrical half-length;
  tip-to-tip height is `2 * (half_height + radius)`), max walk speed, acceleration, braking
  deceleration (this is what stops a grounded character with no input), ground friction (stored
  and sanitized, not applied by the step), air control, jump velocity, max jumps, coyote time,
  jump buffer time, gravity scale, max slope angle, step height, snap-to-ground distance,
  crouch height + speed, mass for pushing dynamic bodies, rotation mode (`OrientToMovement` with turn
  rate | `UseControllerYaw`), movement modes `Walking | Falling | Flying | Swimming | Custom(id)`.
  `Flying` has no gravity and still resolves the capsule. `Swimming` with `TraversalSettings` swims;
  without those settings it keeps gravity and walk speed.
- Input: `CharacterInput { move_dir: Vec3 (world), jump: bool, crouch: bool, sprint: bool }` written by
  controllers; movement runs in `FixedUpdate` using physics character controller (`specs/15`).
- Outputs for animation: `CharacterState { velocity, grounded, mode, time_in_air, landed_this_frame }`.
- Root-motion mode: when an animation provides root motion, it drives displacement (`specs/17`).
- Network prediction hooks (Tier 2): the movement step is
  `step_character(&mut World, &dyn CollisionQuery, Entity, &CharacterInput, dt)`, deterministic for the same
  world, input and step, but not a pure function of a state value (ADR-5613).

### B5. Cameras
- `CameraRig` component with modes (as built, ADR-5613: `ThirdPerson`, `Overhead` (top-down), `FirstPerson
  { eye_offset, bob }` and `Free`; a fixed or orbit view is `Free`, or `ThirdPerson` with zero lag):
  `ThirdPerson { boom_length, socket_offset, target_offset,
  probe_radius, lag_speed, rotation_lag, min/max pitch }` (boom shortens on collision via sphere cast —
  from M20, when physics queries exist),
  `FirstPerson { eye_height, bob }`, `TopDown`, `Fixed`, `Orbit` (inspection), `Spline` (Tier 2: built as the `Rail` and `Crane` nodes of a data-driven camera rig, spec 35 § 17, ADR-3231).
- `CameraDirector` resource (the shared one) plus one per local player on `PlayerManager`
  (`camera_director_mut`, moving `PlayerCamera(n)` cameras; ADR-7203): current view target, camera shakes
  (`Shake { amplitude, frequency_hz, duration_secs, falloff }`, smoothed value noise), FOV kick (stored and
  decayed; the game reads `fov_kick_value`). `blend_to(target, duration, curve)` blends to a pose or an entity
  along an `Easing`, reaching the target exactly at `duration` (ADR-7203). As built (ADR-7263): each player
  director owns its shakes; a `PlayerCamera(n)` camera feels only player `n`'s director (through its sensor,
  which it has by default like a rig), other cameras the shared director; agent `camera.shake { player }`
  routes a noise shake to one player and `camera.get` reports `players`.

### B5a. Camera shake sensors (ADR-1619)
- Sources and receivers are separate. `ShakeSensor` component on a camera: `channels` (u32 bitmask; it
  feels an emit when the masks share a bit), `gain` (0..=100), `combine` (`Add` sums the shakes, `Max`
  plays only the one with the largest envelope now), `axes` (`World` or `Camera` axes for the offset;
  rotations are always camera-relative), `clock` (`Gameplay`: pause and time scale apply; `Unscaled`:
  the engine's unscaled frame delta, for pause menus). Never a wall-clock read.
- Cameras without a sensor do not shake. Backward-compatible defaults: a `CameraRig` camera and the
  camera the shot player drives feel `ShakeSensor::default()` (channel 1, gain 1, `Add`, `World`,
  `Gameplay`); UI and picture-in-picture cameras (plain `Camera`) opt in by adding a sensor.
  `ShakeSensor::off()` silences a camera.
- `ShakeEmit` message: `origin` (optional), `channels`, `preset` (`Noise(Shake)`,
  `Blast(ShockShake)`, `Ground(GroundShock)`), `falloff_radius_m` (strength `(1 − d/r)²`, none by
  default), `travel_speed_m_s` (optional). The blast and ground laws (ADR-1423, ADR-1457) are presets
  with unchanged numbers. Each listening camera measures its own distance; delays are counted in fixed
  steps with the shock-shake rounding (never early). The director's shakes are channel 1, no position.
- `ContactShake` component on a physics body (opt-in): a `ContactForce` at or above `min_force_n`
  emits its noise shake scaled by `force / reference_force_n` (capped), filtered by the other body's
  collision layer bitmask, with a per-body cooldown in fixed steps.
- Order in `PostUpdate`: restore the unshaken pose (`ShakeRestoreSet`, only when the camera still
  holds the shaken pose) → rigs (`CameraSet::Update`) → shot drive (`ShotDriveSet`) → emits, arrivals
  and the shake on top (`CameraSet::Director`). A shake shows under a playing shot and never
  accumulates into a rig's eased pose.
- Agent: `camera.shake`, `camera.shake_sensor`, `camera.set_shake_sensor` (book/agents/camera-shake.md).

### B6. Utilities
- Timers (`TimerHub`): `after(secs, callback-or-message)`, repeating, owner-bound (cancel on despawn).
- Agent: `timers.list` reports every scheduled timer (book/agents/runtime-gameplay.md).
- `Tween` component: animate one reflected `f32` leaf (`.translation.x`, a color channel, UI alpha)
  with easing. A whole `Vec3` or color is not a leaf: the tween is removed and the value stays.
- Agent: `tweens.list` reports every active tween and its progress (book/agents/runtime-gameplay.md).
- Messages: typed `ctx.send_message(target, MyMsg{..})` → `on_message` (must be `Reflect`).
  Delivery runs once after every `update` call of the `Update` pass. A message sent from
  `on_message`, `on_event`, or `late_update` waits until the next `Update`.
- Pools: `PrefabPool` for frequent spawns (projectiles).
- Damage helper module (optional): `Health`, `DamageEvent`, `Died` — sample-level convenience.

### B7. Save games and settings
- `SaveGame` API: `save(&SaveSlot, &mut World, &SaveSpec)` / `load(&SaveSlot, &mut World, &SaveSpec)`; `SaveSpec` selects entities via
  `Saveable` marker (+ which components) and resources implementing `SaveableResource`. Format:
  header (game version, engine version, timestamp, screenshot thumbnail optional; not written yet) + postcard
  reflected data. Migration hook: `fn(&MigrationCtx, &mut SaveFileRaw)` registered by the game per from-version.
  Writes are atomic (`write_atomic`). Autosave on Android `Suspended`.
- `GameSettings` (graphics profile overrides, resolution, vsync, frame cap, audio bus volumes, input
  overrides (look sensitivity and invert-y; key bindings live in `regia_input` rebinding), language, accessibility basics like subtitles/text scale) persisted to `user://settings.ron`,
  applied live (resolution, fullscreen and vsync to the primary window, the size clamped to the monitor and
  vsync kept on Android, ADR-7880).
- **Game pause** (`game_pause`, ADR-10104): off until the game sets `[game] pause = true` in `Regia.toml`
  (`docs/formats/regia-toml-game.md`) or adds `GamePausePlugin` with `GamePauseConfig::enabled()`. The rebindable
  `TogglePause` action (Pause/Break by default, a gamepad button opt-in) in the consuming `Pause` context toggles it.
  A pause sets `Time::paused` (zero gameplay delta, no fixed step; `Unscaled` clocks keep running), reduces every
  player's input contexts to `Pause` plus `keep_contexts`, sets `CursorRelease`, shows the pause screen
  (`default`, `none`, a `.ui.ron` template, or `PauseScreen::Custom`) and sends `GamePaused`; resume restores the
  contexts, the cursor and (unless something else froze time first) `Time::paused`, and sends `GameResumed`. The
  default screen is a localized, focusable dialog with Resume and Quit-to-menu buttons (UI commands `pause.resume`,
  `pause.quit_to_menu` → `PauseScreenEvent`). A networked session refuses the pause (`networked_session`). Agent:
  `game.pause`, `game.resume`, `game.pause_state`.

## Part C — `regia_macros`
- `#[derive(Script)]` (registers `ReflectScript`, implements boilerplate), `#[script(order = N)]`.
- `#[derive(Asset)]` for custom data assets (`.data.ron`): implements `regia_asset::data_asset::Asset`; the type
  derives `Reflect` and `Default` and is registered with `register_data_asset` or `#[reflect(Asset)]`. The cook keeps
  the text; `DataAssets::load_path` loads it through the asset server (ADR-7232). The editor's data asset editor is
  not built yet (spec 19).
- `#[derive(InputAction)]` typed action enums (planned, not implemented).
- `export_game!` / `android_main!` macros (declarative macros in `regia`, not proc macros, where
  possible).

## Acceptance
- M6 (scene): round-trip tests for levels with prefabs/overrides/nested prefabs; 10k-entity level load
  budget; reparenting preserves world transform when requested.
- **M9 (framework core — what DO27 needs):** B1 game plugin entry, B2 scripts, B5 cameras, B6 utilities,
  B7 settings (not save games), plus the sim bridge (`specs/25` § 3). Acceptance: `samples/sim_bridge`
  uses scripts + `CameraRig` (chase with lag/lean, overhead, free) + timers/tweens + live settings changes;
  script execution order and take-run-reinsert semantics covered by tests; all reachable through the agent
  protocol.
- **M20 (framework gameplay — general games):** B3 game rules/players/controllers, B4 character movement,
  B7 save games. Acceptance: `samples/third_person` (with M12's animation graph) playable: move, jump,
  camera boom with collision, collectibles via trigger scripts, HUD counter, pause menu, save/load restores
  collected state; two local players split-screen on desktop.
- **`samples/arena` (M20)** — a small first-person arena shooter against script-driven bots. It covers
  what `third_person` does not: `GameRules` match flow (warm-up → rounds → match over, score limit and
  timer, respawn at `PlayerStart`s), `AiController` bots (steering plus line-of-sight raycasts; no
  navmesh, which is Tier 2), the first-person `CameraRig`, hitscan and pooled projectile weapons
  (`PrefabPool`, physics queries), the damage helper, dynamic physics props with joints and explosion
  impulses, script-driven VFX, decals and spatial audio, and a HUD and menus usable with gamepad and with
  touch controls. No networking. The First Person template is its stripped-down starting point.
  Acceptance: a recorded 3-minute bot match replays to the same final score (`InputRecording`, `06` § 6)
  and passes its agent-protocol `expect` checks; 8 bots, 200 active rigid bodies and 20,000 live
  particles stay within the `06` § 4 frame budgets on the desktop and Android reference devices; a
  30-minute soak with random bot input finishes without errors. (As built: the replay and agent checks pass;
  `samples/arena/tests/budget.rs` checks the counts, with timings only printed under `--bench`, and the soak is
  not built yet; ADR-5613.)
