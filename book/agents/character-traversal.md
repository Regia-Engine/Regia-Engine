# Character traversal: swim, ladders, mantle and vault

`regia_framework::traversal` (`docs/specs/traversal.md`, ADR-2971) gives a character swimming, ladders, mantles and vaults, and
lets AI use the same moves. This page covers what an agent can read and how to verify a move in a running app. Distances are
meters, speeds m/s, angles radians.

A character opts in with the `TraversalSettings` component. One without it steps exactly as before.

## Read the state

`physics.character_traversal` lists every character with `TraversalSettings` in entity order; `entity` limits it to one.

```json
{ "method": "physics.character_traversal", "params": {} }
→ { "count": 1, "characters": [ {
      "entity": 4294967310, "kind": "ladder", "mode": "climbing", "phase": "surface", "depth": 0.0,
      "progress": 0.0, "ladder_id": 77, "last_refusal": "ladder_exit_blocked", "position": [4.6, 3.2, 0.0] } ] }
```

| Field | Meaning |
|---|---|
| `kind` | `none`, `swim`, `ladder`, `ladder_enter`, `ladder_exit`, `mantle`, `vault`, `jump` |
| `mode` | The movement mode: `swimming`, `climbing` (on a ladder), `traversing` (on a curve), `walking`, `falling`, ... |
| `phase`, `depth` | While swimming: `surface` or `underwater`, and the meters under the float line |
| `progress` | How far a curve has come, 0 to 1 |
| `ladder_id` | The ladder's `user_id`, while on one |
| `last_refusal` | The last refusal code; it stays until a later move starts |
| `entity`, `position` | The character's entity bits and its capsule center |

## Ask what a capsule could use

`physics.character_traversal_probe` answers without changing anything, at the character's place and facing, or at `position`
looking along `forward`.

```json
{ "method": "physics.character_traversal_probe", "params": { "entity": 4294967310, "forward": [1.0, 0.0, 0.0] } }
→ { "entity": 4294967310,
    "ledge": { "available": true, "kind": "mantle", "height": 1.5, "edge": [2.0, 1.5, 0.0],
               "stand": [2.37, 2.72, 0.0], "normal": [0.0, 1.0, 0.0], "reason": null },
    "ladder": { "available": false, "reason": "ladder_none_in_reach" },
    "water": { "available": false, "surface": null, "chest_under": false } }
```

A ladder's `attach` is `bottom`, `side` or `top`, and `entity` is the ladder's entity bits. An unavailable ledge or ladder carries
only `available` and `reason`. The probe asks for no input direction: a real attach also needs the character to
move into the ladder (or back over its edge at the top).

## The moves

- **Swim.** Enters where the chest (`swim.chest_offset` above the capsule center) is under the water and the character is not rising.
  It floats with the chest on the surface, moves at `swim.speed`, dives while `crouch` is held or with a downward `move_dir.y`, and
  returns to the line when released. It walks again once it is grounded with the chest `swim.exit_margin` above the water.
- **Ladder.** Attaches when the input moves into a `Ladder` in reach, at the bottom or from the side, or back over its edge at the
  top. Stick into the ladder climbs; `crouch` or stick away descends; `jump` leaves backward with a short cooldown. The facing is
  locked. At the top it needs room, a clear way and a floor, or it stays on and reports why.
- **Mantle and vault.** `jump` while moving, or by itself while falling and moving (`mantle.auto_in_air`, silent on refusal). A
  refused jump still jumps. The capsule follows a curve; `TraversalEvent::Started` gives the host a montage slot and warp targets.
- **AI.** With `TraversalNavPlugin` a `Ladder` or `TraversalJumpPoint` publishes a nav link, and a crowd agent taking off on a link
  sends one `TraversalRequest`. A character tied to the agent with `TraversalCrowdAgent` makes the move by itself.

## Verify

Drive the input with `scene.set_property` on `CharacterInput`, step with `play.step`, and read the state:

```json
{ "method": "scene.set_property", "params": { "entity": 4294967310, "component": "CharacterInput", "path": "move_dir", "value": [1.0, 0.0, 0.0] } }
{ "method": "play.step", "params": { "frames": 120 } }
{ "method": "physics.character_traversal", "params": { "entity": 4294967310 } }
```

## Refusal codes

These appear in `last_refusal` and `ledge.reason` or `ladder.reason`.

| Code | Meaning |
|---|---|
| `ledge_no_wall` | Nothing to climb in reach. |
| `ledge_too_low`, `ledge_too_high` | The top is outside `[mantle.min_height, mantle.max_height]` above the feet. |
| `ledge_too_steep` | The top is steeper than `mantle.max_slope`. |
| `ledge_too_thin` | The top is shallower than `mantle.min_depth` and no vault applies. |
| `ledge_no_headroom` | The capsule does not fit on the ledge or on the way there. |
| `vault_no_landing` | A vaultable wall has no floor or room behind it. |
| `ladder_none_in_reach` | No ladder is in reach (a probe or a request). |
| `ladder_no_room` | The capsule does not fit on the climb line. |
| `ladder_exit_blocked`, `ladder_exit_no_floor` | The top exit has no room or no floor; the character stays on. |
| `traversal_interrupted` | Something entered the path of a running curve, or moved the character off a curve or a ladder; the character falls. |
| `traversal_no_physics` | No physics world: ledge and ladder moves cannot run. |
| `traversal_bad_settings` | `TraversalSettings` holds a non-finite or out-of-range value: no moves run, default movement does. |
| `traversal_bad_request` | A request holds a non-finite point. |
| `traversal_busy` | A request arrived during a ladder climb or a curve, and was dropped. |
| `traversal_disabled` | A request or a probe asked for a move the character's settings turn off. |

`Ladder::validate` and `TraversalJumpPoint::validate` return `ladder_bad_params` and `jump_point_bad_params` for non-finite values; such a
ladder attaches nobody and publishes no link.

## Errors

| Code | Cause |
|---|---|
| `character_not_found` | The entity has no `CharacterMovement` (`F0003`). |
| `traversal_not_enabled` | The character has no `TraversalSettings` (`F0003`). |
| `traversal_bad_param` | `position` or `forward` is not three finite numbers, or `forward` has no horizontal length (`F0002`). |

## Networking

`CharacterTraversal` is predicted and replicated with the `net` feature. Moves run in the predicted schedule from the existing input
command, a re-run after a correction gives the same result, and a corrected capsule that the next sweep finds blocked falls rather than
being pushed into geometry. Both sides need the same `TraversalSettings`. A swimmer on a moving swell can see small corrections, since
the water clock does not rewind in a re-run.
