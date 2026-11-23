# Spec 35: Data-driven gameplay camera framework

Status: proposed (ADR-1577). Plan item G4 (`BUILD-PLAN-1027.md` § 3), rung v0.3.x (b). Behavior written from the
public-sources spec set (ADR-1526); set area `runtime-gameplay` (cameras). This page stands alone: the legacy camera
conventions it relies on are restated in § 1 and § 4.2.
Reviewed: integ/specs (ADR-1585), 2026-10-28. **Built** (ADR-1637; load and hot reload ADR-2030; rail and crane
ADR-3231, format v2; pinned math ADR-4754). ADR-1637 and ADR-5778 record where the build differs from the text below;
§ 18 lists the as-built notes and the open gaps.

## 1. Purpose

**Baseline (implemented, this spec adds the data-driven layer).** `regia_framework::camera` (`camera.rs`, plus the
submodule `camera/ortho.rs`) has: the `CameraRig` component (`mode: CameraMode`, `pitch`, `yaw`) with
`CameraMode::{ThirdPerson, Overhead, FirstPerson, Free}` (structs of the same names; `ThirdPerson` has `boom_length`,
`socket_offset`, `target_offset`, `lag_speed`, `rotation_lag`, `min_pitch`, `max_pitch`, `max_lean`, `lean_gain`,
`lean_speed`, `probe_radius`, and a sphere-cast probe that shortens the boom through `regia_physics`); `ChaseCamera`,
`FreeLookInput`; the `CameraDirector` resource (shake with `Shake`, `MountShock`, fov kick); `CameraSet::{Update,
Director}` ordered in `PostUpdate` before transform propagation; `CameraPlugin`; the agent methods `camera.get` and
`camera.set_mode` (`book/agents/framework.md`) and `camera.set_path`/`camera.raycast` (shot paths, ADR-268). Spec 14
§ B5 is the older design text; where it lists other modes the built code above wins. Cameras today are a closed set of
modes with a shake director and cinematic camera paths.
A game with several camera behaviors (exploration, aiming, vaulting, cinematic follow, a vehicle) must today write
Rust for each one and for switching and blending between them. This spec adds an **authored camera asset** that
describes camera behavior as data: *rigs* (ordered pipelines of small nodes), *transitions* between rigs, a
*director* that decides which rig runs, *variables* and *input slots* that gameplay feeds, and *layers* (overlays and additive rigs) for
temporary and additive effects. A **driver** evaluates it every frame into a pose that drives a camera entity.

Goals: new camera behaviors by editing a `.camera.ron` file; deterministic evaluation (same inputs and `dt` give the
same pose); parity with the four legacy modes (presets that reproduce them); a debug overlay and an agent
inspector. Non-goals: § 13.

## 2. Terms

| Term | Meaning |
|---|---|
| Camera asset | `<name>.camera.ron`: parameters, rigs, shared transitions, one director, input slots |
| Rig | One selectable behavior: a name, tags, an ordered node tree, its own transitions |
| Node | One step of a rig's evaluation; reads and writes the working `CameraPose` |
| Pose | Position, rotation (yaw, pitch, roll), vertical field of view, optional aim point, flags |
| Transition | A blend (or cut) between two rigs, with entry conditions and start-facing/hold/share-state policy |
| Director | The policy that chooses the active rigs each frame |
| Variable | A named bool, float, vec3 or yaw/pitch value that tunes nodes at run time |
| Input slot | A named accumulator for player look/move input that nodes read |
| Driver | The component that evaluates one asset for one camera entity |
| Layer | A rig evaluated on top of the director's choice: an *overlay* (replaces what is beneath, removes itself once fully blended out) or an *additive* layer (adds its delta, stays until released) |

## 3. Crate layout and files

- `regia_framework::camera::asset` (types, parse, validate, migrations), `::nodes` (evaluators), `::director`,
  `::driver` (component, systems, layers), `::debug` (gizmos), `::presets` (legacy-parity assets). The module
  tree extends the existing `camera.rs`; legacy `CameraRig` keeps working untouched.
- `regia_agent`: `camera.inspect` and friends (§ 9), registered beside `camera.get`. The gizmo group type
  `CameraRigGizmos` lives with the other groups in `regia_render::gizmos`; `regia_framework::camera::debug` draws into
  it (layer rule: framework may depend on render). Editor: the Cameras panel (asset page, rig graph, running drivers; ADR-2030).
- `::load` (ADR-2030): `.camera.ron` through the asset paths (source text, else the cooked entry), hot reload on
  `SourceAssetChanged`, and the reflected `CameraAssetSource` a level names a camera with.
- Format `.camera.ron` (format name `camera`, **v1**). Unknown fields or variants are load errors with line and
  column. Spec 12 § 4a applies from the first release: a later shape change needs a version bump, a migrator and
  `regia upgrade-project`; the lead registers the format in `check-formats`. No earlier version exists.

## 4. Asset shape

```ron
(
    format_version: 1,
    name: "third_person",
    params: { "boom_length": Float(4.5), "shoulder": Float(0.4), "aiming": Bool(false) },
    inputs: [ (name: "look", kind: Axis2, speed: (120.0, 90.0), pitch_clamp: (-80.0, 30.0), invert_pitch: false) ],
    rigs: [
        (name: "explore", tags: ["ground"], root: [
            Pivot(target: DriveTarget, offset: (0.0, 1.5, 0.0)),
            LookInput(slot: "look"),
            Boom(length: Param("boom_length"), side_offset: Param("shoulder")),
            Collision(radius: 0.25, layers: ["static"], close_speed: 20.0, open_speed: 4.0),
            Fov(degrees: Literal(70.0)),
            SmoothPosition(depth: 8.0, side: 8.0, height: 6.0),
        ], transitions: []),
        (name: "aim", tags: ["ground", "aim"], root: [ /* ... */ ], transitions: []),
    ],
    transitions: [
        (from: Any, to: Tag("aim"), blend: Smooth(0.25), start_facing: OwnSeed),
        (from: Any, to: Any, blend: Smooth(0.5), start_facing: OwnSeed),
    ],
    director: Rules(default: "explore", rules: [ (when: VarBool("aiming", true), rig: "aim") ]),
)
```

### 4.1 Values

`Value<T>` is `Literal(T)`, `Param("name")` (an asset parameter; a driver override wins over the default) or
`Var("name")` (a runtime variable, re-read every frame; the variable's blended value when several rigs are active).
Parameters are the public inputs of the asset; variables are the live ones. A name used but not declared is a
validation error (`camera_unknown_name`).

### 4.2 Pose and evaluation contract

`CameraPose { position: Vec3, yaw: f32, pitch: f32, roll: f32, fov_deg: f32, aim: Option<Vec3>, cut: bool }`.
Angles are radians, yaw about +Y, 0 faces −Z, positive yaw turns toward −X, positive pitch looks up (spec 14 § B5
conventions). Rotation is `Ry(yaw)·Rx(pitch)·Rz(roll)`. A rig's nodes run in listed order on a pose seeded from the
rig's previous-frame final pose (on the first frame: seeded from the camera entity's current transform and default
fov). A node may only read the pose, runtime state of its own instance, the world (read-only queries), variables and
input slots. Nodes never mutate the world.

### 4.3 Nodes (v1 set)

| Node | Parameters | Behavior |
|---|---|---|
| `Pivot` | `target: DriveTarget | Entity(tag/name) | Point(vec3)`, `offset`, `space: Target | World` | Sets `position` to the target's position plus offset (offset rotated by the target in `Target` space) and `aim` to the same point. A missing target keeps the pose unchanged and records a warning once |
| `LookInput` | `slot`, `additive: bool` | Sets yaw and pitch from the slot's accumulated angles (clamped by the slot); `additive` adds them to the current rotation instead of replacing it |
| `Boom` | `length`, `side_offset`, `up_offset`, `length_speed` | Moves `position` back along the view direction by `length` (eased toward the target length at `length_speed` per second, 0 = instant), then shifts by `side_offset` and `up_offset` in camera space; sets rotation to look along the boom toward `aim` |
| `Collision` | `radius`, `layers`, `close_speed`, `open_speed`, `anchor: Pivot | Target`, `reuse_last_frame: bool` | Sphere-casts from the anchor point (the pose position when the rig has no `Pivot`) to `position`; if blocked, the effective distance is the hit minus a 5 cm skin, clamped so the probe centre stays on the near side of the hit (it may be 0, and it may sit under the radius and under 0.1 m), closing at `close_speed` and reopening at `open_speed` (m/s). No physics world: no effect. `reuse_last_frame` uses the previous frame's cast result (cost control; one frame late). Generalizes the legacy `probe_radius` probe |
| `Fov` | `degrees` | Sets `fov_deg`, clamped to 1..=170 |
| `Offset` | `translation`, `rotation_deg`, `space: Camera | World` | Adds a fixed offset |
| `SmoothPosition` | `depth`, `side`, `height`, `space: Camera | World` | Per-axis exponential smoothing of the pose position toward the node's input position: `p += (in − p)·(1 − exp(−k·dt))` per axis in the chosen space (depth = along the view axis), where `k` is the axis value (1/s); `k <= 0` or absent = no smoothing on that axis. The legacy `lag_speed` maps to `k` directly |
| `SmoothRotation` | `pitch`, `yaw`, `roll` | Same smoothing of angles (shortest-path for yaw) |
| `Lean` | `gain`, `max`, `speed` | Roll proportional to the yaw velocity (legacy lean), relaxed at `speed` |
| `Frame` | `target: Context`, `dead_zone_deg: (h, v)`, `soft_zone_deg: (h, v)`, `speed` | Rotates the camera minimally so the target's direction stays inside the soft zone; inside the dead zone nothing moves; between, the correction is eased at `speed` |
| `Shake` | `amplitude`, `frequency`, `decay` | Adds the legacy director's smooth-noise shake (`Shake` in `camera.rs`), seeded from the driver seed |
| `Group` | `enabled: Value<bool>`, `nodes` | Runs `nodes` when enabled (the editor's enable switch is `enabled: Literal(false)`) |
| `When` | `cond`, `nodes` | Runs `nodes` when `cond` holds (§ 4.4) |

Damping is the only stateful smoothing; position-only and rotation-only are separate nodes so a rig can smooth one
without the other. A node with an invalid value at run time (NaN, negative length) is skipped for the frame and
counted in the instance's `skipped` counter (visible in the inspector); the pose stays finite.

### 4.4 Conditions

`Condition`: `Elapsed(f32)` (seconds since the rig instance started), `VarBool(name, bool)`, `VarCompare(name, Lt|Le|Eq|Ge|Gt, f32)`, `HasTag(tag)` (the rig being evaluated
or, in transitions, the incoming/outgoing rig), `RigIs(name)`, `All([..])`, `Any([..])`, `Not(..)`. An empty
condition list passes. Evaluation is pure and cheap (no allocation).

## 5. Variables, parameters and input slots

- **Parameters** (asset): `Bool | Int | Float | Vec3 | Name` defaults; the driver overrides by name
  (`set_param`); an override of the wrong type is `CameraError::BadParam`.
- **Variables** (runtime): the driver owns a table of named values (`Bool`, `Float`, `Vec3`, `YawPitch`) written by
  gameplay each frame (`set_var`). Values written during the frame are flagged so nodes can ask "was it written
  this frame". A variable marked `local_only` in the asset is never blended between rigs. When several rigs are active
  (a transition, a layer), each rig sees the table blended by its current weight: floats and vec3 linearly, bools
  by threshold 0.5 on the blended weight, yaw/pitch by shortest path.
- **Input slots:** `Axis1` or `Axis2`. Gameplay writes raw per-frame values (`set_input(slot, value)`; the camera
  system does not read devices). A slot integrates `value · speed · dt` into an accumulated angle per axis, then
  clamps (`pitch_clamp` degrees; yaw wraps), optionally normalizes the raw input to length ≤ 1 first, and inverts the
  vertical axis if asked. A slot can be reset (`reset_input`) and has `pull_toward_var: Option<String>` that mixes its
  accumulated angle toward a variable's value by a weight (for snap-behind-the-player).

## 6. Transitions and the director

### 6.1 Director kinds (v1)

- `Fixed(rig)`: always that rig.
- `Rules { default, rules: [(when, rig)] }`: the first rule whose condition holds wins; otherwise `default`.
- `Ranked([(rank: i32, director)])`: the sub-director with the highest `rank` whose own result is `Some` (a `Rules`
  director returns `None` when no rule matches and `default` is `None`; this lets a high-rank director act only on demand).
- `Hook(name)`: a Rust function `fn(&DirectorContext) -> Vec<RigRequest>` registered with the camera plugin by
  name (the escape hatch for a game's state machine; the asset stays data otherwise).

A director may request several rigs at once (for layered effects); the first request is the *primary* (drives
transitions), the others join the active set with weight 1 as additive layers. Directors are evaluated every frame
before rigs. They are read-only over the world.

### 6.2 Rig activation, transitions

The active set changes when the primary rig changes. Transition choice, in order: the *outgoing* rig's own
transitions, then the *incoming* rig's, then the asset's shared list; within a list, the first transition whose
`from`/`to` match and whose conditions all pass. `from`/`to`: `Any`, `Rig(name)`, `Tag(tag)`; a tag match passes when
the rig has the tag. If none matches the change is a cut. A transition has:

- `blend`: `Cut`, `Linear(t)`, `Smooth(t)` (smoothstep), `EaseIn(t)`, `EaseOut(t)`, with `t` seconds ≥ 0 (0 = cut).
- `start_facing`: how the incoming rig's rotation initializes when it starts: `OwnSeed` (its own seed), `TargetFacing`
  (the driver target's yaw and pitch), `CarryOver` (the outgoing rig's final yaw and pitch), `ContinueAim`
  (look at the outgoing rig's last `aim` point; falls back to `OwnSeed` when it had none).
- `hold_outgoing: bool`: while blending out, the outgoing rig stops updating (its last pose is held).
- `share_state: bool`: when two rigs share the same structure (same node kinds in the same order), their *node states* are
  merged (interpolated by weight) instead of cross-fading the final poses, so smoothing state does not pop.
  Rigs with different structure fall back to pose cross-fade; a `share_state` transition between them logs a warning
  once.
- `interruptible` (default true): a new primary rig while a blend is running starts a new blend from the current
  blended pose over the new transition's time; the old blend's outgoing rigs keep fading by their remaining weight.

Blend weights: the incoming weight `w = curve(elapsed / t)` ∈ 0..1; the outgoing rigs share `1 − w` in proportion
to their previous weights. Poses combine by weighted position/fov average and weighted quaternion nlerp (shorter
arc), roll and aim by weighted average, the `cut` flag true if any component has `cut` and weight ≥ 0.5. The final
weights sum to 1 before the layers are applied.

## 7. Layers: overlays and additive rigs

Rigs evaluated on top of the director's pose, addressed by `LayerId`:

- **Overlay:** `push_overlay(rig, OverlayOptions { transition: Option<Transition>, context: ContextRef, always_new:
  bool }) -> LayerId` evaluates the rig, blends it in over the transition, and blends everything beneath it out. An
  overlay removes itself once a later overlay (or the director's pose after a `release`) has fully blended it out; a
  self-ending overlay (e.g. a hit-reaction camera) uses the rig-local `Elapsed(seconds)` condition in a transition
  back. `hold_overlay(id)` stops an entry updating (its pose is held). If `always_new` is false and the same rig is
  already an overlay, `push_overlay` returns the existing id and restarts its blend-in. A cut-blend overlay switches
  with no interpolation.
- **Additive layer:** `add_additive(rig, order: i32, context, allow_same_order: bool) -> LayerId`; layers are sorted by
  `order` (ties by insertion; a duplicate `order` is refused with `camera_order_taken` unless `allow_same_order`);
  each adds its pose delta against the pose below it (position delta, rotation delta composed by quaternion, fov
  delta) and stays until released.
- `release(ReleaseBy::Id(id) | ReleaseBy::Rig(name, context), instant: bool)`: `instant` drops at once, otherwise the
  layer blends out over its rig's blend-out time (default 0.2 s). It works on both kinds.
- Events (`CameraLayerEvent { driver, id, rig, kind: Entered | Settled | Leaving | Gone | Cut }`) are written as
  messages in evaluation order.

## 8. Driver, evaluation order, actions

`CameraDriver` (component on the camera entity): `asset: Handle`, `target: Option<Entity>` (the `DriveTarget`),
`local_player: u8` (selects which local player's input/target when several exist), `seed:
u64`, `enabled: bool` (a driver starts evaluating as soon as it is enabled and has a valid asset). A driver on an entity disables legacy `CameraRig` updates for that entity.

Per frame in `PostUpdate`, `CameraSet::Update` (before transform propagation): 1) director → requests; 2) transition
state advance (`dt` from the camera clock, fixed steps allowed); 3) evaluate each active rig's nodes in listed order;
4) combine by weights; 5) apply the overlay and additive layers; 6) write `Transform` and `Projection` fov; 7)
`CameraSet::Director` shakes apply as before. The system never allocates in steady state (reused buffers).

**Actions** are small scoped effects on a live rig instance. `LookAt { rig, target: Context, tolerance_deg, hold:
bool }` overrides the `aim` for that instance. With `hold: false` it ends by itself once the target is within
tolerance; with `hold: true` it keeps framing every frame until the caller removes it (`clear_look_at(id)`). It runs
before the rig's nodes so a `Boom` or `Frame` sees the overridden `aim`.

**Build status.** `CameraBuildStatus`: `Ok`, `Warnings`, `Errors`, `Stale` (edited since the last validation; the
other three are the result of the last validation). Validation (`CameraAsset::validate`) reports issues with stable
codes: `camera_unknown_rig` (director or transition names a rig that does not exist), `camera_unknown_name`,
`camera_duplicate_rig`, `camera_bad_value` (non-finite or out of range), `camera_empty_rig` (warning),
`camera_no_default_rig`, `camera_share_structure` (warning), `camera_unknown_slot`, `camera_unused_param` (warning),
`camera_unknown_hook` (a `Hook` name not registered). Assets with errors do not load into a driver; the previous good
version keeps running in dev builds.

## 9. Debug and agent surface

- **Gizmo group** `CameraRigGizmos` (`"camera_rig"`, off by default; type in `regia_render::gizmos`): each active rig's pose frustum tinted by
  weight, boom line from pivot to camera, collision probe sphere (red when blocked), aim point, frame dead/soft
  zone, and the layer entries as stacked labels. Toggle with `debug.set_gizmos`.
- **Agent methods** (page `book/agents/camera-framework.md`, topic `camera-framework`, COVERAGE rows). The legacy
  `camera.get/set_mode` stay.

| Method | Permission | Params | Result |
|---|---|---|---|
| `camera.assets` | read | `{}` | loaded camera assets: name, path, rigs, params, build status, issue count |
| `camera.validate` | read | `{ asset }` | `{ status, issues: [{ code, severity, rig, node, message }] }` |
| `camera.inspect` | read | `{ entity }` | driver state: asset, active rigs with weights and node-by-node pose trace (position, yaw, pitch, roll, fov after each node), transition state, layer entries, variables, input slots, `skipped` counters, last events |
| `camera.set_var` | play | `{ entity, name, value }` | `{ ok }` |
| `camera.set_param` | play | `{ entity, name, value }` | `{ ok }` |
| `camera.set_input` | play | `{ entity, slot, value }` | `{ ok }` |
| `camera.reset_input` | play | `{ entity, slot }` | `{ ok }` |
| `camera.push_overlay` | play | `{ entity, rig, transition?, always_new? }` | `{ id }` |
| `camera.add_additive` | play | `{ entity, rig, order, allow_same_order? }` | `{ id }` |
| `camera.release_layer` | play | `{ entity, id?, rig?, instant? }` | `{ released }` |
| `camera.hold_overlay` | play | `{ entity, id }` | `{ ok }` |
| `camera.set_driver` | edit_scene | `{ entity, asset?, target?, enabled? }` | driver summary |

Errors: `camera_no_driver`, `camera_unknown_asset`, `camera_unknown_rig`, `camera_unknown_hook`, `camera_unknown_name`, `camera_bad_value`,
`camera_unknown_layer_id`, `camera_order_taken`, `camera_asset_invalid` (with the validation issues in `data`). All
lowercase ASCII/digits/underscore, documented in the book page.

## 10. Presets (legacy parity)

`presets::third_person()`, `overhead()`, `first_person()`, `free()` return `CameraAsset` values equivalent to the four
legacy modes with their default parameters, and `CameraRig`'s settings convert with `presets::from_legacy(&CameraRig)`.
These are code-built values, also shipped as `assets/camera/presets/*.camera.ron` in starter templates. They are the
parity oracle for the tests (§ 12): for a scripted target path and look input, the preset driver's pose equals the
legacy rig's pose within 1e-4 m / 1e-4 rad each frame (shake and fov kick excluded), including lag, lean and boom
collision shortening.

## 11. Determinism, performance, platforms

- Pure function of (asset, params, variables, input values, target transforms, physics query results, `dt`,
  `seed`, previous-frame state). No wall-clock, no thread-order dependence; physics queries read the state of the
  frame they run in. Replays that record variables, inputs and target transforms reproduce poses exactly.
- Budget (desktop, release): one driver with 2 active rigs of 8 nodes each, one collision probe: ≤ 60 µs per frame;
  ≤ 8 drivers ≤ 0.5 ms. `tracing` spans: `camera_driver{entity}`, `camera_rig{name}`, `camera_collision`.
- Linux, Windows, Android aarch64: pure Rust over existing systems. Touch-look sensitivity is a variable the game
  sets; nothing platform-specific.

## 12. Tests and acceptance

No golden image is needed; poses are compared as numbers.

1. Parse/validate: round-trip the example; each validation code produced by a minimal bad asset; unknown field gives
   line/column; `Stale` after an edit; errors prevent loading.
2. Node unit tests: each node's output for fixed inputs (Boom length/offsets, Fov clamp 1..170, damping `k` formula
   at dt 1/60 and 1/30 over 2 s converging to the same value within 1e-3 (frame-rate independence), yaw shortest
   path across ±π, Frame dead zone leaves the pose untouched, soft zone eases, Lean matches legacy).
3. Collision: with a wall between pivot and desired camera, the distance shortens to the hit minus the 5 cm skin
   (§ 4.3), pulls in at `close_speed`, pushes out at `open_speed`; no physics world is a no-op; `reuse_last_frame` is
   exactly one frame late.
4. Director: `Fixed`, `Rules` first-match-wins and default, `Ranked` highest wins, `Hook` registered by name;
   unknown hook name fails validation at load.
5. Transitions: choice order (outgoing own, incoming own, shared), tag and rig matching, conditions all-pass,
   no-match is a cut; blend curves hit 0 and 1 exactly and are monotonic; weights sum to 1 each frame; interrupting a
   blend starts from the current blended pose without a pop (pose continuity ≤ 1e-3 per frame at 60 Hz).
6. Start-facing policies: each of the four on a scripted switch.
7. `hold_outgoing` holds the outgoing pose; `share_state` with equal structure has no velocity discontinuity in damped
   position (compare against cross-fade, which does), unequal structure warns once and cross-fades.
8. Layers: `push_overlay` blends the layer in and out and removes it; `always_new` semantics; `hold_overlay`; `add_additive`
   order, duplicate order refused unless `allow_same_order`; `release` instant vs blended; cut-blend overlay is a cut; event order.
9. Variables: set/read, written-this-frame flag, blending by weight, `local_only` not blended; parameter override type
   check; input slot integration, clamp, invert, normalize, reset, `pull_toward_var`.
10. Actions: `LookAt` with `hold: false` finishes within tolerance; with `hold: true` it persists until cleared.
11. Parity: the four presets against the legacy rig on a 600-frame scripted path (§ 10).
12. Driver disables legacy `CameraRig` updates on its entity and not on others.
13. Determinism: two runs bit-identical; a recorded input/variable/target stream replays to the same poses.
14. Agent: every method round-trips; every error code is produced by a test; `camera.inspect` node trace is
    reproducible; `cargo xtask agent-coverage`, `book-check`, `check-layers` pass.
15. Performance counters (not wall-clock): steady-state frames allocate nothing (a counting allocator test or a
    buffer-reuse counter).

## 13. Out of scope

A visual rig-graph editor (the asset is RON; the editor shows the inspector and the gizmos; a graph panel is a
follow-up spec), state-machine directors authored as data (the `Hook` director covers games meanwhile; a state-tree
backed director can be added once spec 38 is built: the hook reads a `StateTreeStatus` and returns rig requests), occluder-fading materials (renderer work), IK-style multi-iteration aiming beyond `LookAt`, split
screen composition (several drivers already work; layout is a renderer concern), cinematic sequencing (the
sequencer owns shots; a sequencer track may later set a driver's variables), camera-mounted post volumes (ADR-1464
already ties post volumes to the camera entity).

## 14. Provenance

Public user documentation of gameplay-camera systems read for *what* they do (behavior only; ADR-1526). Evaluation
math is standard: exponential smoothing (frame-rate independent form `1 − exp(−k·dt)`), smoothstep easing, quaternion
nlerp. Set area: `runtime-gameplay` (cameras). Row in `docs/provenance.md` (ADR-1577); this section is the spec's own
record. Names, node set, field names, director kinds and parameterization are Regia's.

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

## 15. Decisions and owner questions

- Decision (ADR-1585): the format is `camera` v1 under `format_version: 1`; the lead registers it with
  `cargo xtask check-formats --update` and writes `docs/formats/camera-ron.md`.
- Decision (ADR-1585): the parity bound in § 10 (1e-4) stays, with the escape that the builder first checks the
  legacy integration order (exponential smoothing of position, then rotation, then lean); if it differs, the preset
  uses the same order and the bound stays; the tests must not be loosened without an ADR.
- Decision (ADR-1585): `Ranked` sub-directors may return `None`; kept as written.
- Decision (ADR-1585): no new dependency; no `unsafe`. New book page `book/agents/camera-framework.md` and COVERAGE
  rows for the eleven methods are part of the build.
- Owner decisions: none.

## 16. Builder notes

Size L. Order: asset types and validation, nodes and pose, driver and legacy parity presets, director and
transitions, layers, agent surface, gizmos. Spec 38 (state trees, built on `integ/ext2`) is only needed for the
optional state-tree director hook. No dependency on other unbuilt specs.

## 17. Rail and crane (ADR-3231)

Two node kinds added in `.camera.ron` v2 (the v1 → v2 migration only bumps the version; v1 files mean the same).
Code: `regia_framework::camera::{rail, crane, rail_keys}`; evaluation in `camera/nodes.rs`.

**`Rail { spline, drive, orient = Frame, offset = (0,0,0) }`.** `spline` is the `Name` of a spline entity
(`regia_scene::Spline`); the driver builds its world-space `SplineCurve` once per frame per name. The camera sits
at a distance `s` in meters along the curve. `s` is arc length (the curve's own table-driven arc-length
parameterization, spec 64 § 3), so a drive that adds `v · dt` per step moves at `v` m/s on any curve.

- `drive: Value(v)` — `s` is the node value (a literal, `Param` or `Var`; `Var("rail_position")` is what a sequence keys).
- `drive: Time(speed, start = 0)` — `s = start + speed · elapsed`, stateless.
- `drive: Follow(target, lead = 0, damping = 0)` — goal = distance of the target's closest point on the rail + `lead`;
  `s` eases to it with `s += (goal − s) · (1 − exp(−damping · dt))` (`damping 0` snaps). The ease never passes
  the goal, so on an open rail it never passes an end; on a closed rail it takes the shorter way round the seam.
  The first frame starts at the goal.
- Ends: an open spline clamps `s` to `0..=length`; a closed one wraps it. A rail shorter than 1 mm (or missing, or
  failing `Spline::validate`) skips the node (warning once) and leaves the pose as earlier nodes set it.
- `orient: Frame` — forward is the spline tangent, up the spline frame's up (after roll); `LookAt(target)` — turn
  toward the target (frame while it cannot be resolved); `Free` — keep the rotation earlier nodes set.
- `offset` — meters in the rail frame (x right, y up, z forward) from the rail point.

**`Crane { base, yaw = 0, pitch = 0, length, mount_yaw = 0, mount_pitch = 0, limits = () }`.** The base is
`Ground(point)` or `Rail { spline, position, heading = false }` (a rail point; with `heading` the arm yaw adds the
rail tangent's yaw, so the crane turns with a curving track). The arm leaves the base along yaw (about +Y; 0 faces
−Z, positive turns toward −X) and pitch (positive up) for `length` meters; the camera sits at the tip. The mount
adds `mount_yaw`/`mount_pitch` (degrees) to the arm's angles for the camera rotation (180° yaw looks back at the
base). Every axis can have a `[min, max]` limit; pitch is always held in ±89.9° and length at ≥ 0. Axes are
clamped before solving; the node reports both the requested and the clamped values. **The arm does not collide**
with anything: a crane that must not enter geometry needs limits authored for the set.

**Keying.** The sequencer properties `camera.rail.position` and `camera.crane.{yaw, pitch, length, mount_yaw,
mount_pitch}` write the Float driver variables `rail_position` and `crane_*` (declared in the asset), which the
nodes read with `Var(...)`. A track whose variable is missing reports an apply error once.

**Agent.** `camera.rail_state`, `camera.crane_state` (read the last evaluation), `camera.set_rail`,
`camera.set_crane` (write the variables). Book page `agents/camera-rail-crane.md`.

**Validation.** Empty spline names, non-finite numbers, a negative damping or arm length, and limits that are
non-finite or inverted are errors (`camera_bad_value`).

**Known limits.** The spline's world transform is read from `GlobalTransform` of the previous propagation, so a
spline that moves in the same frame lags one frame; the curve is rebuilt every frame per driver that has a rail node
(cost grows with the point count; cache it if a scene has hundreds of points).

## 18. As built and open gaps (ADR-5778)

Conformance audit 2026-11-10. The build records are ADR-1637 (build notes and parity), ADR-2030 (load, hot reload,
editor panel), ADR-3231 (§ 17), ADR-4754 (pinned math) and ADR-5778; where they differ from §§ 1-16, they win.

- `share_state` merges node states and still cross-fades the final poses; `HasTag` in a transition reads the
  incoming rig; the written-this-frame flag is API only (`VarTable::written_this_frame`); `local_player` reaches
  `DirectorContext` only; a `Pivot` with a missing target also counts as skipped every frame.
- Collision keeps a 5 cm skin and may place the probe under the radius and under 0.1 m (§ 4.3); ADR-1637's floor is
  superseded.
- The `camera_driver` span records `entity` (`DriverCore::set_entity_bits`); with no enabled driver the system
  returns before building its per-frame name index.
- Open for v0.5.x: a counting-allocator test for § 12.15, and removal of the per-frame allocations that remain
  while a driver is enabled (name index, rail map, spline clone, layer order, share-blend state copy); the `Frame`
  zone gizmo; presets in the starter templates.
- As built (ADR-7734, v0.6.0): the Cameras panel edits the asset: typed forms for every node, transition and rig
  field, add/remove/duplicate/reorder of nodes, rigs and transitions, rig rename with every reference rewritten, a
  rig still named elsewhere is not removed (`camera_panel_rig_in_use`), one undo history per asset, Save/Ctrl+S,
  Content ▸ New ▸ Camera rig, and the `camera` document autosave. Still no node-graph canvas (§ 13).
