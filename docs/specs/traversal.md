# Traversal: swim, ladders, mantle and vault, nav bridge

Status: proposed (ADR-2971). Unnumbered, like `lightmaps.md`. Crate: `regia_framework::traversal`, on top of the character
step (`specs/14` § B4, `specs/93`), the prediction rules (`specs/73` §§ 6, 9), the off-mesh links (`specs/92`) and the warp
targets (`specs/34`).

No reviewed public-spec set was used. Sources: Regia's own docs and the public `regia_physics` queries. The ledge test is an
original design from three plain questions (is there a wall, where is its top, does the capsule fit); see `docs/provenance.md`.

## 1. Purpose

A third-person game needs its character to swim, use ladders and climb over ledges, and its AI needs to use the same moves.
Before this spec `MovementMode::Swimming` had no movement code, no ladder or mantle existed, and an off-mesh link moved a crowd
point but nothing a character could use.

**Opt-in by component.** A character gains the moves by having [`TraversalSettings`]. A character without it runs the
exact code path it ran before: the only new work in `step_character` is one component lookup. No system joins
`CharacterPlugin` (the determinism pins move with the schedule), and nothing is added to entities that did not ask.

## 2. Components

| Item | What |
|---|---|
| `TraversalSettings` | Saved tuning (`traversal-settings` v1): `swim`, `ladder`, `mantle` groups, `montage_slot`. Requires `CharacterTraversal`. |
| `CharacterTraversal` | The state of the current move, predicted and replicated (§ 8). `last_refusal` keeps the last refusal. |
| `Ladder` | A saved volume (`traversal-ladder` v1) at an entity: rungs from the entity's origin up local +Y for `height`, climbed from local +Z facing local -Z, so `into` is the entity's -Z. `half_width`, `climb_speed`, `standoff`, `exit_forward`, `user_id`, `nav_link`, `enabled`. |
| `TraversalJumpPoint` | A saved authored jump (`traversal-jump-point` v1): takes off at the entity, lands at local `to`. `bidirectional`, `user_id`, `arc_height`, `enabled`. |
| `TraversalCrowdAgent` | Ties a character to the crowd agent that steers it (§ 7). |

Two reserved `MovementMode::Custom` ids, as constants: `MovementMode::CLIMBING` (on a ladder) and `MovementMode::TRAVERSING` (a
scripted curve: mantle, vault, ladder step on or off, authored jump). Custom ids at or above `0xFFFF_0000` are reserved for the
engine. Neither has gravity, and no exhaustive match breaks. `Swimming` keeps its meaning and its old `has_gravity`: a character
without `TraversalSettings` that a host puts in `Swimming` steps as it always did.

`TraversalKind`: `None`, `Swim`, `Ladder`, `LadderEnter`, `LadderExit`, `Mantle`, `Vault`, `Jump`.

## 3. Swim

**Water.** The surface height comes from `regia_terrain::water_body::query::surface_at`, the function `WaterQuery` wraps (same
highest-wet-body rule, shore switch and floating origin), on `client` builds, where `regia_terrain` is present. A `TraversalWater`
resource (`Box<dyn WaterSource>`) overrides it. A plain or server build has no terrain crate: it compiles, answers "no water"
and swims only with a `TraversalWater` installed.

**Lines.** `chest = y + swim.chest_offset`, `float_y = surface - swim.chest_offset` (the chest on the surface). `depth` is
how far the capsule center is under `float_y`.

**Enter.** From `Walking` or `Falling`, when `chest <= surface` and the character is not rising (`velocity.y <= 0`).

**Each step.** Horizontal target velocity = input (x, z) x `swim.speed` (x `sprint_multiplier` while sprinting). Vertical target
velocity, first match:
1. `crouch` held: `-swim.dive_speed` (held dive);
2. `move_dir.y` not zero: `move_dir.y x swim.dive_speed` (3D swim input);
3. otherwise rise or settle on the float line: `depth x swim.float_rate`, limited to `swim.rise_speed`.
A dive stops at `swim.max_dive_depth` under the float line. Velocity follows the target with
`v += (target - v) x (1 - exp(-swim.drag x dt))`, then the capsule moves through `CollisionQuery::move_shape` with no snap and the
character's step height. Velocity reads the motion achieved when the move was blocked. Phase: `Surface` while within
`swim.surface_band` of the float line, else `Underwater`.

**Leave.** Grounded with `chest >= surface + swim.exit_margin`: `Walking` (depth-based: the gap to the enter line is the hysteresis,
so a character cannot flip on one spot). `jump` at the surface: first a mantle attempt, else a hop (`swim.jump_velocity`) into
`Falling`. `chest > surface + swim.leave_margin` with no ground, or no water at the point: `Falling`.

## 4. Ladder

**Attach.** A character in `Walking`, `Falling` or `Swim`, with no ladder cooldown, attaches to the nearest enabled valid ladder
whose attach zone holds it (ties: lower `user_id`, then lower position bits, so entity order never decides):
- *bottom or side*: lateral offset within `half_width + ladder.attach_margin`, in front of the plane by at most
  `standoff + ladder.attach_reach`, feet below the top and head above the base, and moving into the ladder
  (`move_dir . into >= ladder.intent`);
- *top*: feet within `step_height` of the top, lateral as above, within `exit_forward + standoff + ladder.attach_reach` beyond the
  ladder, and moving back over the edge (`move_dir . -into >= ladder.intent`).
A capsule that would not fit on the climb line refuses with `ladder_no_room` (moving into a ladder is an attempt: the refusal is
recorded once, with a `Refused` event, not every step). The top zone allows the feet `step_height + 5 cm` from the top, and the
climb line stands at least the capsule's radius plus its skin off the plane, whatever `standoff` says. Attaching runs a short
`LadderEnter` curve (no pop) onto the climb line; on the top it runs at the height where the feet are at the top.

**Climb.** `climb = (move_dir . into + move_dir.y)`, clamped to 1, or -1 while `crouch` is held; an AI request sets `auto_climb`,
which replaces the input. The capsule moves on the climb line (`origin + (+Z) x standoff`) at `climb x climb_speed`, swept, between
`bottom_y` (feet at the base) and `top_y` (feet at the top plus the skin). Facing is locked to `into` whatever the rotation mode.

**Leave.**
- Moved off the line by someone else (more than 5 cm; a respawn, a correction): `Falling` with `traversal_interrupted`, as with a curve.
- Top: at `top_y` moving up, the exit is the stand point `exit_forward` past the plane. The capsule must fit there, the straight
  path must be clear, and a floor must lie within `step_height` under it. Otherwise it stays on, holds at `top_y` and sets
  `last_refusal` to `ladder_exit_blocked` or `ladder_exit_no_floor`; the exit runs as soon as the space clears. A clear exit runs
  the `LadderExit` curve and ends in `Walking`. A climb held under something before the top reports `ladder_exit_blocked` too, so a
  self-driven climb (no player to notice) says why it stopped.
- Bottom: at `bottom_y` or against the ground moving down: a sweep with snap settles it, `Walking`.
- `jump`: leaves with `-into x jump_off_speed + Y x jump_off_up`, `Falling`, and `ladder.cooldown` seconds before any re-attach.

## 5. Mantle and vault

**Trigger.** `jump` while moving (`move_dir` horizontal length at least 0.3), in `Walking`, `Falling` or `Swim`; while `Falling`
and moving, automatically when `mantle.auto_in_air` (silent on refusal); or a request (§ 7). A refused jump still jumps.

**The probe** (`probe_ledge`, a pure read of the world), from the capsule's feet, in the direction of intent:
1. **Wall.** A capsule cast forward up to `mantle.reach`, lifted over `step_height`. No hit, a surface walkable by
   `max_slope_angle`, or a wall that does not face the intent (cosine between the intent and the wall's inward normal under 0.3):
   `ledge_no_wall`.
2. **Top.** A row scan up the column just past the contact (`mantle.edge_inset`), starting at the contact, finds the first free height,
   so a ledge under an overhang is found; no free height up to `max_height`: `ledge_too_high`. A ray down from there reads the top.
3. **Slope and range.** The top's normal steeper than `mantle.max_slope`: `ledge_too_steep` (first: on a steep face the "top" is only
   where the column met the face). Then a top above `max_height`: `ledge_too_high`, or below `min_height`: `ledge_too_low`.
4. **Depth.** A second ray `mantle.min_depth` further in must find the top within `step_height`. If not, the wall is thin: a vault
   if allowed (`height <= vault_max_height`, thickness at most `vault_max_thickness`, a floor walkable by `max_slope_angle` within
   `vault_max_drop` of the feet, and clear space to stand there), else `ledge_too_thin`, or `vault_no_landing` when a vaultable wall
   has nowhere to land.
5. **Room.** The capsule must fit at the stand point (`radius + mantle.stand_inset` past the edge) and be clear along the rise and
   across (`ledge_no_headroom`).

Height ranges are stated in meters above the feet: mantle `[min_height, max_height]`, default `[0.5, 2.0]`.

**The move.** The path is four control points (`CharacterTraversal::path`): start, straight above the start at the pass height
(`clearance` over the stand height), straight above the stand point, stand point; a vault ends at the landing instead. The capsule
rises clear of the face before it moves across, so the rise and the crossing are the swept room checks of step 5. The inner
corners are rounded by a quadratic blend of at most 4 cm (`clearance` is at least that), so a rounded corner never drops under
the stand height. Travel is by arc length with a smoothstep ease, over
`ceil(length / speed / dt)` whole fixed steps. Each step first checks that the character is where the curve left it (within 5 cm):
someone who moved it (a shove, a correction from the server) ends the move in `Falling` with `traversal_interrupted`. Pulling it
back would erase the move in one step, so it would never reach the owning client. Then the step sweeps the capsule to the curve's next
point, and a hit ends the move the same way, so a capsule is never pushed into geometry. The end runs one snap sweep and leaves the
character `Walking` on the ground.

**Host hooks.** `TraversalEvent::Started` carries the kind, `settings.montage_slot`, a montage name (`mantle`, `vault`,
`ladder_enter`, `ladder_exit`, `jump`), the duration in seconds and the warp targets (`regia_animation::WarpTarget`): `ledge_edge`
and `ledge_stand` for a mantle, `vault_over` and `vault_land` for a vault, both with a facing. No animation asset is needed:
the capsule follows the curve whether or not a host plays anything. Root motion is drained and dropped during every move.

## 6. Refusals and parameter errors

Every code is lowercase protocol text: `TraversalRefusal::code()`. The last one stays in `CharacterTraversal::last_refusal` until a
later move starts. Explicit attempts (a jump, a request, moving into a ladder, climbing to its top) also send
`TraversalEvent::Refused`; the automatic ledge grab in the air does not. `traversal_interrupted` always sends it.

**Events balance.** Every `Started` is followed by one `Finished` of the same kind before the next `Started`, however the move ends. A
ladder is `LadderEnter`, then `Ladder`, then `LadderExit` (or a jump off, or the bottom).

| Code | Meaning |
|---|---|
| `ledge_no_wall` | Nothing to climb in reach (a request only; a jump just jumps). |
| `ledge_too_low`, `ledge_too_high` | The top is outside `[min_height, max_height]`. |
| `ledge_too_steep` | The top is steeper than `mantle.max_slope`. |
| `ledge_too_thin` | The top is shallower than `mantle.min_depth` and no vault applies. |
| `ledge_no_headroom` | The capsule does not fit on the ledge or on the way there. |
| `vault_no_landing` | A vaultable wall with no floor or room behind it. |
| `ladder_none_in_reach` | A ladder request found no ladder. |
| `ladder_no_room` | The capsule does not fit on the climb line. |
| `ladder_exit_blocked`, `ladder_exit_no_floor` | The top exit has no room or no floor; the character stays on. |
| `traversal_interrupted` | Something entered the path of a running curve, or moved the character off it. |
| `traversal_no_physics` | No `PhysicsWorld`: ledge and ladder moves cannot run. |
| `traversal_bad_settings` | `TraversalSettings` holds a non-finite or out-of-range value: no moves run, default movement does. |
| `traversal_bad_request` | A request holds a non-finite point. |
| `traversal_busy` | A request arrived during a ladder climb or a curve, and was dropped. |
| `traversal_disabled` | A request or a probe asked for a move the character's settings turn off. |
| `ladder_bad_params`, `jump_point_bad_params` | `Ladder::validate` / `TraversalJumpPoint::validate` failed: it is skipped and publishes no link. |

## 7. Nav bridge (`TraversalNavPlugin`)

- A `Ladder` with `nav_link` publishes a `NavLinkAnchor` on its entity: `kind: Climb`, bidirectional, `user_id` of the ladder, landing
  `(0, height, -(exit_forward))` locally, `duration = height / climb_speed`. A `TraversalJumpPoint` publishes `kind: Jump`. They
  are kept equal to the source each fixed step, before `NavAuthoredLinksPlugin` reads them; an invalid source removes its anchor.
- After `NavCrowdStep`, each `NavLinkPhase::Started` of kind `Climb`, `Jump` or `Drop` writes one `TraversalRequest { agent, entity,
  kind, user_id, start, end, arc_height }` message; `entity` is the character whose `TraversalCrowdAgent` names the agent, and
  `arc_height` is the `TraversalJumpPoint`'s with that `user_id` for a jump, else 0.5 m.
- `TraversalPlugin` puts requests on the named character (`CharacterTraversal::pending`). A climb finds the ladder by `user_id`
  (else the nearest to `start`) and climbs toward `end` by itself. A jump or drop runs the `Jump` curve to `end` plus the stand height.

## 8. Networking

`step_character` runs the moves, so `net_character_step` (server, predicting owner, re-run) runs them with no other change.
- `CharacterTraversal` has a `ReplicateCodec`; `install_net_character` registers and predicts it, with a tolerance
  (`traversal_matches`): same kind, phase, `auto_climb`, `cooldown_steps`, `last_refusal` and `pending`; during a curve the same step
  and step count with its points within `TOLERANCE_POSITION_M`; on a ladder the same ladder id with its line within that tolerance.
- The state holds everything a re-run needs: the curve's points and step counts and the ladder's line, so no ladder entity
  crosses the wire and a level's ladders need no `NetId`.
- The state carries `frame`, the floating origin its points are written in. Each step moves them into its own frame first, which covers
  a local rebase and a peer whose origin differs; the tolerance compares in the shared world frame.
- **Known gap outside this module:** `regia_net` does not yet convert a predicted root's `Transform` to a client's own origin, so any
  predicted character on a client whose origin differs from the server's corrects constantly, with or without traversal. The ignored
  test `a_client_with_another_origin_predicts_a_mantle_with_no_correction` turns on when that is fixed.
- A state no step could write (non-finite points, a step past the end, a line upside down) is refused by the decoder. The step checks
  again, for one written another way, and ends the move with `traversal_interrupted`.
- `TraversalSettings` is not replicated: both sides hold the same tuning, like `CharacterMovement`.
- Input is `CharacterNetInput` unchanged: stick into the ladder climbs, `crouch` dives or descends, `jump` is the attempt.
- Events are first-run only (dropped while `NetResimulating`).
- A level water plane re-runs exactly. A swell moves with `WaterClock`, which a re-run does not rewind, so a swimmer on a swell
  can see small corrections (known limit; § 12).

## 9. Agent protocol (`book/agents/character-traversal.md`)

- `physics.character_traversal { entity? }`: for every character with `TraversalSettings`, entity order: `kind`, `mode`, `phase`,
  `depth`, `progress`, `ladder_id`, `last_refusal`, `position`.
- `physics.character_traversal_probe { entity, position?, forward? }`: what this capsule could use now: `ledge` (`available`, `kind`,
  `reason`, `height`, `edge`, `stand`, `normal`), `ladder` (`available`, `user_id`, `entity`, `attach`, `reason`), `water`
  (`available`, `surface`, `chest_under`). An unavailable ledge or ladder carries only `available` and `reason`.
- Errors: `character_not_found` (F0003), `traversal_not_enabled` (F0003), `traversal_bad_param` (F0002: `position` or `forward` is
  not three finite numbers, `forward` has no length).

## 10. Determinism

Fixed steps only: durations count whole steps, nothing reads a wall clock. Candidate ladders are ordered by distance, then
`user_id`, then position bits; ray and cast queries return the nearest hit. The moves read `Transform`s, never `GlobalTransform`
(`nav_world::world_pose`).

## 11. Tests and acceptance

`crates/regia_framework/tests/traversal.rs` (rapier backend, fixed steps, no wall clock):
1. Walk into a pool, float within 5 cm of the line, swim across, walk out. A held dive reaches depth and returns.
2. Ladder enter, climb, exit on top on the upper floor. Blocked top refuses and stays on, and exits once clear. Jump off with no
   instant re-attach. Rotation lock. Top attach climbs back down.
3. Mantles inside the range succeed, just outside fail with the stated reason; no headroom, too steep and too thin each report their
   code; a vault lands on the far side.
4. A character with no settings is bit-identical to a run of the same inputs with traversal code absent from its path (a recorded
   comparison against a twin with `TraversalSettings` and no reachable volumes). Existing pins and `character_polish` stay green.
5. Non-finite parameters refuse with their code and fall back to default movement.

`crates/regia_framework/tests/traversal_net.rs`: a predicted mantle with no false corrections, then a forced correction during a
mantle: the client converges with the server and the capsule is never embedded. `crates/regia_framework/tests/traversal_nav.rs`:
a crowd agent crosses a ladder link and gets one request; its bound character climbs. `samples/physics_lab`: the course, a two-run
digest equality, and an agent dogfood test.

## 12. Limits and out of scope

Ladders are static (a moved ladder applies at the next attach); no moving-platform ledges during a curve; no lateral movement on a
ladder; no breath, no wading speed change, no crouch-mantle; swell water under prediction (§ 8); animation assets and IK.

## 13. Platforms and budget

Pure CPU Rust with no platform seam: Linux, Windows and Android aarch64 build the same code. Per character per step: a settings lookup
when off. When on: one water lookup (a few bodies), a ladder scan only with move intent, and during a jump or an in-air move one capsule
cast; only when it finds a wall, the top scan (one short ray per 10 cm row up to `max_height`, about 20 at the defaults), two down
rays, and the fit and sweep checks of step 5 (a vault adds one ray per 5 cm of thickness). A running curve or a climb casts once.
Tracing span `character_traversal` on the step.
