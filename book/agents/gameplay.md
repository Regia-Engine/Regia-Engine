# Gameplay: players, possession and save games

The M20 gameplay surface lets an agent see who is playing, switch what a controller drives, and
save or restore the running world. All of it works on the same entity ids `scene.query_entities`
reports.

## Players

`players.list` shows every active player — index, kind (`Local` until v0.4.0 networking), the
controller entity, and what it currently possesses:

```json
→ { "players": [{ "index": 0, "entity": 4294967296, "kind": "Local",
                  "controller": 4294967297, "possessed": 4294967304,
                  "viewport": [0.0, 0.0, 1.0, 1.0], "cameras": [] }],
    "count": 1, "max_local": 4 }
```

`viewport` is the player's split-screen rect (`null` for a player without a local view) and `cameras`
lists the cameras tagged `SplitScreenCamera` for that player, with the viewport each one has now. The
player plugin writes the rect onto those cameras ([display settings](display-settings.md), ADR-7880).

`players.get` takes either `index` or `entity` and adds the camera view target and the active
input context of the controller:

```json
{ "index": 0 }
→ { "entity": 4294967296, "index": 0, "kind": "Local", "controller": 4294967297,
    "possessed": 4294967304, "view_target": 4294967304, "context": "Gameplay" }
```

If the app has no `PlayerManager` (no `regia_framework::player::PlayerPlugin`), both calls answer
`F0003` with a hint naming the missing plugin.

## Possession

`scene.possess` makes a `PlayerController` drive an entity — the character the player's input and
camera should follow:

```json
{ "controller": 4294967297, "entity": 4294967304 }
→ { "controller": 4294967297, "before": 4294967304, "after": 4294967304 }
```

Both ids are required. The controller's `view_target` follows the possessed entity. Possessing a
new entity automatically unpossesses the old one; `before` reports the previous pawn.

This is the quickest way to test gameplay scripts on a specific character: spawn or find a
character, possess it, inject input (`play.inject_input`), and assert the results (`play.expect`).

## Save games

`save.save` serializes every entity marked `Saveable` and every resource the game registered as
saveable, then writes the slot atomically (temp file + rename, CRC-32 checked):

```json
{ "slot": "Chapter1" }
→ { "slot": "Chapter1", "entity_count": 42, "resource_count": 3, "bytes_written": 12345 }
```

`slot` defaults to `"Auto"`. `save.load` restores a slot into the running world — current
saveable entities are despawned, saved entities spawn back with their reflected component values,
and registered resources are replaced. The game's migration hooks run automatically when the file
is older than the current format. `save.load` needs `slot` (no default) and also reports which copy it
read (`"current"` or `"previous"`):

```json
{ "slot": "Chapter1" }
→ { "slot": "Chapter1", "entity_count": 42, "resource_count": 3, "bytes_written": 12345,
    "copy": "current" }
```

`save.list_slots` lists the slots on disk with their header metadata (versions, timestamp, whether
a screenshot thumbnail is embedded). Slots whose header fails validation (bad magic, CRC
mismatch) are listed with `null` metadata — treat them as damaged — unless their previous good
generation (`<slot>.prev`, ADR-3102) is valid: then the metadata is the previous copy's and
`has_previous` is `true`. `save.load` of such a slot loads the previous copy and reports
`"copy": "previous"` ([Player data](../user-guide/player-data.md)).

```json
→ { "slots": [{ "name": "Auto", "game_version": "0.1.0", "engine_version": "0.1.0",
                "timestamp": 1760000000.0, "has_thumbnail": false, "has_previous": true }], "count": 1 }
```

Errors: `F0003` when the slot does not exist (`save.load`) or the save subsystem is missing, and
`F0001` when serialization fails (details in `logs.read`, target `regia_framework`).

### What an agent can rely on

- **Components**: every reflected component on a `Saveable` entity is stored by short type name and
  restored by value. A component that is not in the type registry is skipped with no warning. A
  postcard encode that fails logs a warning and is skipped. An entity left with no serialisable
  component is omitted from the file; load despawns every current `Saveable` entity and spawns
  only the file's rows, so that entity is deleted.
- **Resources**: only types the game opted in via `register_saveable_resource` — an agent cannot
  discover them generically; `resources.list` shows which reflected resources exist.
- **Entity ids inside component values** (e.g. a pawn reference) are process-local: do not expect
  them to survive a save/load round trip until stable entity remapping ships.

### Testing a save round trip

```json
save.save   { "slot": "T" }
scene.set_property { "entity": 4294967301, "component": "Health", "path": "hp", "value": 0.0 }
save.load   { "slot": "T" }
play.expect { "assertions": [ { "condition": { "entity": 4294967301, "component": "Health",
              "path": "hp", "op": "eq", "value": 73.5 } } ] }
```

On every platform the engine autosaves to the `"Auto"` slot once when the app enters `Suspended` (the Lifecycle edge, not every frame of the suspension). `Resumed` arms the next edge.

## Driving `samples/third_person` (M20)

The M20 sample exposes its gameplay through the generic surface plus one reflected
component. Every acceptance feature has an `expect`-able anchor:

| Feature | How to drive and assert |
|---|---|
| Move + jump | `play.inject_input` (or set `CharacterInput.move_dir` via `scene.set_property`), then `play.expect` on the character `Transform.translation` having changed / `CharacterState.grounded` flipping to `true` on landing. |
| Boom camera | Query the camera rig entity (`scene.query_entities` for `CameraRig`), read `Camera/Transform` — near a wall the camera sits closer than `boom_length`; `play.expect` `translation` within bounds. |
| Collectibles | The singleton `third_person::SampleState` component holds `collected`/`total`. Walk the character into a coin (inject input towards it), then `play.expect` `collected` to be `>= 1`. |
| HUD counter | The entity named `hud-counter` carries reflected `HudLabel.text`, the same string the HUD draws (`Coins: 1 / 8`). `play.expect` that string. `SampleState.collected` is the counter's source, not the HUD. |
| Pause menu | Trigger the pause key (`play.inject_input`), assert `SampleState.paused` is `true` and `play.step` does not advance the character. The pause-menu node's `NodeStyle` display becomes `Flex` (the style type is not reflected; the headless test reads it directly). |
| Save/load | `save.save { "slot": "Chapter1" }`, collect or wait, `save.load`, assert `SampleState.collected` is the stored value and despawned coins stayed gone. |
| Foot IK | Walk onto the stairs (the fox's length along +X). `fox-rig-0` (player 1: `fox-rig-1`) carries reflected `SolvedFoot.span`: the world-Y gap between solved foot tips that sit within 8 cm of the ground probe. On the flight that span is at least one step (~0.2 m). It stays ~0 if the rig's `FootPlacement` (graph output, which plans with `FootPlanter`) or the graph's `TwoBoneIK` nodes are removed, because the span is read from `JointGlobals`, not from the weight parameter. |
| Split-screen | Join a second player (F2, or a gamepad button through `play.inject_input`), then `players.list` shows two players. Each camera's `Camera.viewport` is half the window; the reflected `third_person::camera::SplitViewport` (`x`, `y`, `w`, `h`) is the `play.expect` anchor because `Camera` itself is not a reflected type. |

The headless equivalents of these checks live in `samples/third_person/tests/gameplay.rs`
— run `cargo test -p third_person` for the same assertions without a window.

## The third-person car and gameplay tags

`samples/third_person` parks a ray-cast car (`regia_physics::vehicle`) at `(-12, 1, -10)`, west of
the start (ADR-2134). A player within 3.5 m of the chassis presses `E` to get in and `E` again to
get out on the driver's side. While a player drives, `W`/`S` are throttle and reverse, `A`/`D`
steer and `Space` brakes; the keys write the car's `VehicleInput` only when they change, so an
agent's `physics.vehicle_input` holds until the player touches them. The player's camera runs the
camera-driver asset `vehicle_chase` (`samples/third_person/assets/camera/vehicle.camera.ron`) while
driving; getting out removes the `CameraDriver`, and the on-foot boom rig (`CameraRig`, the
`CameraRuntime.boom` the M20 checks read) takes over again. The car's tuning is the `.vehicle.ron`
`samples/third_person/assets/car.vehicle.ron` (the default car; `docs/formats/vehicle.md`); check an
edit to it with `physics.vehicle_config_check` before running the game.

| What | How to drive and assert |
|---|---|
| Find the car | `scene.query_entities { "component": "Car" }` (marker on the chassis). |
| Get in / out | `play.inject_input` key `KeyE` (pressed, then released) next to the car. The character gets the reflected `Driving { car, keys }` component; `scene.get_entity` with `components: ["Driving"]` reads `car`. |
| Drive | `physics.vehicle_input { entity, throttle, steer, brake }`, `play.step`, then `physics.vehicles { entity }` (`forward_speed`, `grounded_wheels`). See [Vehicles](vehicles.md). |
| Chase camera | `camera.inspect { entity }` on the `PlayerCamera` entity names the `vehicle_chase` asset and its `chase` rig while driving; `camera.validate` checks the asset. See [Camera framework](camera-framework.md). |
| Tags | The project tag table (`samples/third_person/assets/game.tags.ron`) declares `Role.Player` (the characters), `Pickup.Coin` (the coins), `Vehicle.Car` (the car) and `State.Driving` (added while a character drives). `tags.list`, `tags.get { entity }`, `tags.query { entity, query: { "Has": "State.Driving" } }`. See [Gameplay tags](gameplay-tags.md). |

```json
{ "method": "play.inject_input", "params": { "events": [{ "type": "key", "key": "KeyE", "state": "pressed" }] } }
{ "method": "play.step", "params": { "frames": 2 } }
{ "method": "scene.get_entity", "params": { "entity": 4294967304, "components": ["Driving"] } }
{ "method": "physics.vehicle_input", "params": { "entity": 4294967157, "throttle": 1.0 } }
{ "method": "play.step", "params": { "frames": 120 } }
{ "method": "physics.vehicles", "params": { "entity": 4294967157 } }
```

`samples/third_person/tests/agent_expect.rs` (`agent_drives_the_car_and_reads_the_tags`) runs this
script; `tests/gameplay.rs` (`the_car_is_entered_driven_with_its_chase_camera_and_left`) checks the
same behavior without the protocol. `tests/car_render_agent.rs`
(`an_agent_drives_the_car_and_sees_it_through_the_chase_camera`) runs the sample with the headless
renderer: `play.wait_until` (level presented, character grounded), `play.expect` on `Driving.car`, and
an offscreen `seeing.screenshot` on foot and again through the chase camera after driving off (pixel
checks only, no golden; skipped with a message on a machine without any Vulkan device):

```json
{ "method": "seeing.screenshot", "params": { "target": "offscreen", "width": 320, "height": 180, "hud": false } }
→ { "width": 320, "height": 180, "mime": "image/png", "png_base64": "iVBORw0KGgo..." }
```

Out-of-reach `E` does nothing. A refused car spawn (no physics world) is logged and the level plays
without it. The Fox still plays the locomotion graph, not motion matching: its clips carry no root
motion, so a motion-matching database built from them has all-zero trajectory features.
