# Arena: driving and verifying the first-person bot arena

`samples/arena` is a first-person arena shooter against script-driven bots (`docs/specs/14` M20).
It covers what `third_person` does not: a `GameRules` match flow (warm-up → round → match over,
score limit + timer, respawn at `PlayerStart`s), `AiController` bots (a state tree with abilities,
a weapon chooser and resupply stations by default, or steering + line-of-sight raycasts; see
[Gameplay systems](#gameplay-systems-state-tree-bots-abilities-choosers-smart-objects-tags-spectator)), hitscan and pooled-projectile weapons, dynamic props with joints and explosion
impulses, VFX/decals/spatial audio, and a gamepad/touch HUD. No networking.

## Run it headless

The arena supports the offscreen capture path (`book/agents/headless-capture.md`): build the
sample with `MinimalPlugins` + `RenderPlugin { headless: true }` + `ArenaSample`, pin the fixed
step, and drive it with the agent protocol.

```
cargo run -p arena -- --exit-after-frames 300
```

Launcher flags (also used by `cargo xtask soak`):

| Flag | Meaning |
|---|---|
| `--seed N` | Match RNG seed (determinism) |
| `--bots N` | Number of AI bots (default 7, plus the local player) |
| `--no-local-player` | Pure bot-vs-bot match (soak hook: `--bots 16 --no-local-player` = 16 simulated players) |
| `--bench` | Budget probe (8 bots, 200 crates, ~20k particles) |
| `--brain state-tree\|steering` | Bot brain without a navmesh (default `state-tree`, ADR-2134) |

## What an agent can see

The arena registers its reflected types, so `scene.query_entities`, `resources.list` and
`play.expect` can inspect the match (`ArenaState` is a component, found with
`scene.query_entities`'s `component` filter):

| Type | Kind | Meaning |
|---|---|---|
| `Pawn` | component | `slot` (0 = local player, 1.. = bots) and `is_bot` |
| `Health` | component | `current` / `max` hit points |
| `ArenaState` | component (on the `"match"` entity) | `phase` (`Warmup`/`Round`/`MatchOver`), `scores` by slot, `match_secs` |

The bot brain (`BotBrain`) and pooled projectile (`Projectile`) are also reflected. The match
RNG is seeded from the launcher's `--seed` flag (`arena::MatchRng`), so two runs with the same
seed replay to the same final score.

## Assertions

`play.expect` conditions use the short component names `scene.get_entity` and `resources.list`
report. Health is a component on each pawn; the match state is a singleton `ArenaState` component
on the `"match"` entity (a component, not a resource, so the field atom can read it — the
engine's resource predicate is a closed set of typed handlers). `samples/arena/tests/agent_expect.rs`
executes exactly these assertions:

```json
{ "assertions": [
  { "name": "a bot is alive",
    "condition": { "entity": { "component": "Pawn" }, "component": "Health", "path": "current", "op": "gt", "value": 0.0 } },
  { "name": "the match is in the round",
    "condition": { "entity": { "component": "ArenaState" }, "component": "ArenaState", "path": "phase", "op": "eq", "value": "Round" } },
  { "name": "scores start at zero",
    "condition": { "entity": { "component": "ArenaState" }, "component": "ArenaState", "path": "scores", "op": "eq", "value": [0, 0, 0] } }
] }
```

A good headless smoke: pause, `play.step` 120 frames (two seconds), then assert every pawn is
above the floor and at least one bot has taken damage, and take a `seeing.screenshot` with
`target: "offscreen"` (the frame must not be black and must show bot bodies).

## Determinism

`samples/arena/tests/determinism.rs` records a three-minute bot match with `InputRecording`
(`regia_framework::input_recording`): the RNG seed plus every fixed-step action value.
Playback feeds each step's values through `InputInjection` before that frame's action evaluation
(`PreUpdate`), so a replayed step sees what the recorded step saw (ADR-537). The test asserts the
replayed `Actions` contain the recorded non-zero `Move`, and that `ArenaState.scores` matches.
`tests/repro_check.rs` goes further: 20 seeds of the match (steering and navmesh bots, a local player
on seeded input) are recorded and replayed, and every reflected component must hash the same at each
checkpoint ([Determinism](determinism.md)).

## Touch and pause

The HUD draws a virtual stick (`Move`) and a fire button (`Fire`). A bridge copies
`VirtualAxes` / `VirtualButtons` into `InputInjection`. Gamepad Start opens the pause menu
(`Display::Flex`); the Resume button's click sets it back to `Display::None`.

## Weapons

Bots fire a pooled rocket (`PrefabPool` key `rocket`) every fourth shot. An explosion overlap
writes `ExternalImpulse` on dynamic crates. A static wall between two bots blocks the
line-of-sight raycast, so the shot does not land until the wall is gone.

## Budget probe

```
cargo run -p arena -- --bench
```

Prints p50/p95/p99 over 300 frames and the live counts: 8 bots, at least 200 rigid bodies, at
least 20,000 particles (`VfxScene::estimated_live`). Lavapipe timings are not representative.

## Gameplay systems: state-tree bots, abilities, choosers, smart objects, tags, spectator

Without a navmesh the default bot brain is a state tree (`ArenaConfig::brain`,
`BotBrainKind::StateTree`, ADR-2134); `BotBrainKind::Steering` keeps the plain steering loop that
the determinism, replay, budget and evidence tests pin. The data ships in `samples/arena/assets/`
and is embedded in the binary, so the agent methods see it under these paths:

| Data | Path | System |
|---|---|---|
| Bot behavior | `ai/bot.statetree.ron` | `regia_ai` state tree: `Bot` → `Idle` (warm-up and match over), `Fight` (task `arena::brain::Engage`), `Resupply` (task `arena::brain::Restock`); ambient task `arena::brain::Sense`; `state_key: "state"`. See [State trees](state-tree.md). |
| Ammunition | `abilities/pawn.attrs.ron` | Attributes `ammo_max` 6, `ammo` 6. |
| Weapons | `abilities/rifle.ability.ron`, `abilities/rocket.ability.ron` | Abilities: rifle costs 1 round and starts a 0.6 s cadence (`Cooldown.Rifle`); rocket costs 4 rounds, cools 2.4 s (`Cooldown.Rocket`), chains the rifle cadence and waits for it. See [Abilities](abilities.md). |
| Effects | `effects/*.effect.ron` | Costs, cooldowns and `resupply` (instant refill, cue `Cue.Resupply`). |
| Weapon choice | `choosers/weapon.chooser.ron` | Chooser, mode `First`: `rocket` (ready, 5 to 30 m, 4+ rounds), `rifle` (ready, up to 30 m, 1+ round), catch-all `none`. See [Choosers](choosers.md). |
| Tags | `game.tags.ron` | `Team.Bot`, `Team.Player`, weapons, cooldowns, `Effect.Ammo.*`, `Cue.Resupply`. See [Gameplay tags](gameplay-tags.md). |
| Station choice | `ai/resupply.spatialquery.ron` | Spatial query `resupply_stations` (entity items, `NearbyEntities(tag: "resupply_slot")`, scored by planar distance to the bot, pick `All`). See [Spatial queries](spatial-queries.md). |

A bot fights while it has rounds: `Engage` closes to 6 m, and with a clear line of sight asks the
chooser (inputs `distance`, `ammo`, `rifle_ready`, `rocket_ready`; the readiness comes from the
ability activation check) and starts the picked ability. When `ammo` drops below 1 the tree enters
`Resupply`: `Restock` runs the station query for the bot (nearest slot first, ties to the lower
entity id) and claims the first free slot of role `resupply` in that ranking, on one of the two
yellow stations (smart objects at `(-6, 0, 3)` and `(6, 0, -3)`), walks there, uses it for 0.5 s, applies
the refill and succeeds once the new `ammo` reads back. A respawn also refills. A claim left behind
when the round ends is released by `Sense`. A bot-only match (`--no-local-player`) is watched
through the spectator camera following bot 1; `SpectatorFocus { enabled, slot }` on `main_camera`
switches the view and the followed slot.

| What | How to drive and assert |
|---|---|
| Which brain runs | `ArenaBrainStatus` on the `"match"` entity: `brain` (`state_tree`, `steering`, `navmesh`), `trees`, `stations`, `errors`. A tree that fails to compile falls back to steering and lists the error. |
| A bot's state | `StateTreeStatus.active_path` on the pawn (`["Bot", "Fight"]`), `trace`; `Blackboard` keys `in_round`, `ammo`, `has_target`, `target`, `target_pos`, `target_dist`, `state`. |
| Validate or swap the tree | `statetree.compile { text }`, `statetree.reload { entity, text }`, `statetree.control`, `statetree.debug`. |
| Ammunition and weapons | `abilities.list { entity }` (two grants), `abilities.check { entity, grant }` (`on_cooldown`, `cannot_afford`), `abilities.inspect`, `abilities.set_base { entity, attribute: "ammo", base }`, `abilities.cues` (`Cue.Resupply`). The reflected `Loadout { rifle, rocket }` holds the grant ids. |
| Weapon choice | `chooser.evaluate { path: "choosers/weapon.chooser.ron", context: { distance, ammo, rifle_ready, rocket_ready }, trace: "full" }` answers `row` and why every other row failed. |
| Stations | `smartobject.list` (two slots, role `resupply`, `claimed_by`), `smartobject.search`, `smartobject.set_enabled` (a disabled station sends a restocking bot to the other one). |
| Tags | `tags.get { entity }`, `tags.query { entity, query: { "Has": "Team.Bot" } }`. |
| Station ranking | `ai.query_list` (lists `resupply_stations`), `ai.query_run { template: "resupply_stations", owner, pick: "all" }` (both slots, nearest first, with `score` and the `near` contribution). Only the state-tree brain installs `SpatialQueryPlugin`; in the other modes the methods answer `ai_query_unavailable`. |
| Spectator | `spectator.get`, `spectator.set { mode, target }`, `spectator.step`; `scene.set_property` on `SpectatorFocus.enabled` / `.slot`. |

```json
{ "method": "abilities.set_base", "params": { "entity": 4294967310, "attribute": "ammo", "base": 0.0 } }
{ "method": "play.step", "params": { "frames": 15 } }
{ "method": "scene.get_entity", "params": { "entity": 4294967310, "components": ["StateTreeStatus"] } }
→ { "components": { "StateTreeStatus": { "value": { "active_path": ["Bot", "Resupply"], ... } } } }
```

```json
{ "method": "ai.query_run", "params": { "template": "resupply_stations", "owner": 4294967310, "pick": "all" } }
→ { "status": "success", "item_count": 2, "units": 4,
    "items": [ { "entity": 4294967340, "score": 0.0, "passed": true, "contributions": { "near": 0.0 }, ... },
               { "entity": 4294967339, "score": -1.0, "passed": true, "contributions": { "near": -1.0 }, ... } ],
    "tests": [ { "id": "near", "kind": "Distance", "in": 2, "out": 2 } ] }
```

`samples/arena/tests/wc_systems.rs` holds the headless checks and the agent script
(`an_agent_drives_the_gameplay_systems_of_the_arena`: compile, tags, abilities, chooser, smart
objects, the station query, an emptied magazine restocked at a station, the spectator). `tests/wc_render_agent.rs`
(`an_agent_watches_the_state_tree_match_through_the_spectator_camera`) runs the match with the
headless renderer: `play.wait_until` (a bot in `Fight`), `play.expect` (brain, trees, stations,
`SpectatorFocus`), an offscreen `seeing.screenshot` through the spectator camera, then
`scene.set_property` `SpectatorFocus.slot` to 3 and a second shot that must differ (pixel checks only,
no golden). `tests/repro_check.rs` records
and replays 20 seeds of the state-tree match (`arena_state_tree`) next to the steering and navmesh
scenes. The `agent` feature (default) installs the `abilities.*` and `chooser.*` methods; without it
they are absent.

## Evidence

`docs/evidence/m20/arena/arena_first_person.png` is the pawn's eye view.
`arena_overhead.png` looks down on the floor, crates and distinctly colored bots. Both
composite the HUD. `cargo test -p arena --test offscreen` regenerates them on lavapipe.
