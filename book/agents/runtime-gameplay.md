# Runtime gameplay: prefab spawn, levels, rules, timers, tweens

The runtime gameplay surface (`docs/specs/14-scene-and-framework.md` § A3/A4/B3/B6): spawning
prefab instances while the game runs, opening and unloading levels, reading the active game
rules, and listing the scheduled timers and active tweens. These methods work in a running game
(the editor's `editor.spawn_prefab` / PIE level tools are the editor-side counterparts); every
call degrades gracefully — when the plugin a call reads is not installed it answers `F0003`
with a hint naming the plugin, rather than pretending.

## Spawning a prefab at runtime

`scene.spawn_prefab` instantiates a prefab the way scripts do (`world.spawn_prefab`, spec 14 §
A3). The root entity carries `PrefabInstance`:

```json
scene.spawn_prefab { "prefab": "asset:67e55044-10b1-426f-9247-bb680e5fe0c8",
                     "transform": { "translation": [3.0, 4.0, 5.0] } }
→ { "entity": 4294967307, "prefab": "asset:67e55044-10b1-426f-9247-bb680e5fe0c8",
    "pending": false }
```

- `prefab` (required): the prefab asset id, a guid or `asset:<guid>`.
- `transform` (optional): `{translation?, rotation?, scale?}`; omitted fields take the defaults
  (identity rotation, unit scale). Vectors are `[x, y, z]` (or `{x, y, z}`), rotation a
  quaternion `[x, y, z, w]`.
- `root_guid` (optional): the instance root's `EntityGuid`. Omitting it draws a fresh random
  guid. A lockstep game passes its sim RNG's next guid so peers derive identical
  prefab-internal guids.

When the template is not in the `PrefabLibrary` yet, the root spawns immediately with
`PendingPrefab` and resolves on a later frame (`pending: true`); the returned entity id is
stable across the resolve. The instance is not part of any level (no `LevelMember`), so a
level unload never despawns it.

Refusals are `F0002` with a `data.reason`: `bad_guid` (a guid parameter does not parse) and
`spawn_refused` (a nil or duplicate `root_guid`, or a template that fails validation). Without
the scene plugin (`regia_scene::ScenePlugin`) the call answers `F0003`.

## Opening and unloading levels

`level.open` drives the world's `LevelManager` (spec 14 § A4):

```json
level.open { "level": "asset:67e55044-10b1-426f-9247-bb680e5fe0c8", "mode": "additive" }
→ { "level": "asset:67e55044-10b1-426f-9247-bb680e5fe0c8", "mode": "additive",
    "current": "asset:…", "state": "active" }
```

`mode` is `"replace"` (the default; unloads the current level first) or `"additive"` (loads
alongside it and only becomes the primary level when none is open). The open is synchronous
when the level's data is already in the `LevelLibrary`; otherwise the level is recorded
`Loading` and completes when its data arrives (`state: "loading"`; watch for the `LevelLoaded`
message, then poll again). `current` is the primary level's id after the call, or `null` when
the world is level-less.

`level.unload` fires `LevelUnloading` and despawns the level's non-persistent entities
(`Persistent` ones survive and are adopted when the level reopens):

```json
level.unload { "level": "asset:67e55044-10b1-426f-9247-bb680e5fe0c8" }
→ { "level": "asset:67e55044-10b1-426f-9247-bb680e5fe0c8", "unloaded": true }
```

Unloading the primary level promotes the remaining active level with the lowest id. A level
that is not loaded answers `{ "unloaded": false }` — a graceful no-op, not an error.
Refusals are `F0002` with `data.reason` `bad_guid` or `level_not_found` (the level is neither
in the `LevelLibrary` nor tracked by the `LevelManager`). Without `regia_scene::ScenePlugin`
both calls answer `F0003`.

## Rules, timers, tweens

`rules.get` reads the active `GameRules` (the `GameRulesResource`, installed by
`regia_framework::rules::RulesPlugin`):

```json
rules.get {}
→ { "rules": "my_game::rules::ArenaRules", "match_over": false, "level_rules": "arena",
    "match_session": false }
```

`rules` is the concrete rules type's path (its `GameRules::type_path`); `match_over` is the
rules' `is_match_over(world)`; `level_rules` is the name the current level asked for
(`LevelSettings.game_rules`, `null` with no level open); `match_session` is `true` while
`regia_framework::rules::MatchSessionPlugin` steps the hosted match session from ECS
([Match modes and match flow](match-rules.md), "Using the core from ECS"). Without the resource
the call answers `F0003`.

`timers.list` reports every scheduled timer in the `TimerHub` (spec 14 § B6), in scheduling
order. A missing hub answers an empty list, never an error (scripts create the hub on demand):

```json
timers.list {}
→ { "timers": [
    { "remaining_secs": 1.5, "interval_secs": null, "owner": 4294967307,
      "action": { "kind": "callback" } },
    { "remaining_secs": 0.25, "interval_secs": 0.25, "owner": null,
      "action": { "kind": "callback" } },
    { "remaining_secs": 2.0, "interval_secs": null, "owner": null,
      "action": { "kind": "message", "target": 4294967307,
                  "message_type": "my_game::messages::Ding" } }
  ], "count": 3 }
```

`owner` is the entity whose despawn cancels the timer (`null` for unbound timers);
`interval_secs` is the repeat interval (`null` for one-shots). Callback timers are reported by
kind only — the closure itself is not observable (and does not survive a hot reload; message
timers do).

`tweens.list` reports every active `Tween` component — the reflected `f32` leaf each one
animates and its progress:

```json
tweens.list {}
→ { "tweens": [
    { "entity": 4294967307, "component": "Transform", "path": ".translation.y",
      "from": 0.0, "to": 10.0, "duration_secs": 2.0, "elapsed_secs": 1.0,
      "easing": "Linear", "t": 0.5 }
  ], "count": 1 }
```

`t` is the normalised progress after the tween's own easing (`0.0`..=`1.0`). A tween whose
target leaf stops being a single `f32` removes itself, so it disappears from the next list.

## Verifying with play.expect

All three reads compose with `play.wait_until` / `play.expect` for deterministic checks, e.g.
wait until `tweens.list` is empty (the tween finished) or until `rules.get` reports
`match_over: true`. `play.step` advances exact frames while paused; timers tick in
`FixedUpdate`, so two `play.step` calls with fixed time advance a one-shot timer by exactly
its step.

## Rules-selected character prefabs (v0.5.1)

A game's `GameRules::on_player_joined` may return `SpawnRequest.prefab` as a
project-relative `.prefab.ron` source path or a registered `asset:<guid>`.
Local and remote joins use scene instantiation and possess the authored root
under the prefab instance wrapper. The root keeps its authored components;
the request supplies its transform and team (`team: None` clears any authored team). Missing character
components receive defaults. Missing, malformed or uninstantiable prefabs
warn and fall back to the default character without a pending instance.
Nested prefab dependencies must already be registered in `PrefabLibrary`.

Use `rules.get` to identify the active rules, `scene.query_entities` to find
`PlayerController` and the authored character's name, and `scene.get_entity`
to inspect possession, `Transform` and `Team`. The wrapper carries
`PrefabInstance`; authored entities carry `PrefabLink`. The existing
`scene.spawn_prefab` method retains its own asynchronous pending behavior.
There is no new agent method or error code.

## Friendly fire

`FriendlyFire` is a resource the damage system reads when both the source and the target carry
`TeamId` and the id is the same and not `0`. `On` (the default) applies the hit in full. `Off`
drops it. `Scaled(factor)` multiplies the amount; a non-finite factor drops the hit. `Reflect(factor)`
leaves the target alone and applies `amount * factor` to the source once, and a kill's `DiedEvent`
names the original target. Two neutrals are not teammates. A missing resource is `On`.

`SelfDamage` is separate. It defaults to on. A hit whose source is the target itself follows that
switch and is not also friendly fire.

`LedgerSlot` with `TeamId` is the seat a positive finite hit is recorded against. When `MatchRules`
is present, a killing hit credits that seat's ledger (kills, deaths, and assists inside the session's
assist window). Team `0` and an entity with no slot are not credited. The rules core itself does not
store the friendly-fire factor.

## Spawn points and policies

A `SpawnPoint` component beside a `PlayerStart` (ADR-8647) gives the start a side (`team`, `0` serves
every team), `tags`, a `priority` and an `enabled` flag. A start without one is side 0, untagged,
priority 0 and enabled, so a level with no `SpawnPoint` behaves as before. `PlayerStart` itself did not
change shape.

`DefaultRules::choose_spawn` calls `regia_framework::spawn_select::choose_spawn`, which walks the starts
by priority (higher first), then spawn order, and applies the hosted session's spawn policy
(`MatchRules.session.respawn.spawn`; `first` when there is no `MatchRules`):

Spawn order (ADR-8704) is the order the `PlayerStart`s arrived in when the world tracks it.
`PlayerPlugin` installs that tracking (`SpawnOrder<PlayerStart>`); add it before anything spawns a
start. Without tracking (a bare `World`, or a game that took `PlayerStart`'s component hooks) the order
is the start's translation: x, then y, then z. Neither order uses entity indices, which Bevy recycles,
and `default_spawn_point` uses the same order.

| Policy | Picks |
|---|---|
| `first` (default) | the first eligible start (S-14's behavior) |
| `round_robin` | the next eligible start after the last one given, wrapping |
| `random` | a draw from the session's seeded `DetRng` stream (`respawn.seed`), part of the session state |
| `scored` | the best score: `enemy` points per meter to the nearest enemy (capped at `enemy_cap`), minus `los` per enemy that sees the start, plus `team` per teammate within `team_radius`, minus `recent` per use within `recent_ticks`, plus `priority` per priority unit; ties go to the earlier start |

A start is eligible when it is enabled, its side is `0` or the side the asking team plays on now (after
a side swap, `PhaseState::side_of`), it carries every tag the ask requires, and it is not blocked. The
blocked test runs only with a `SpawnShape { radius, half_height, eye_height, block_test: true }`
resource: an overlap query of the pawn's capsule against the physics world, or (with no physics world)
any living contestant within two radii. Sight lines are physics raycasts between eye points when the
world has physics; without physics nothing is occluded. Contestants are entities with `TeamId` and a
`Transform`; one carrying `Died` is not alive. The asking team is the `TeamId` on the player, else on its
possessed pawn.

Lockstep sims call the plain-data core instead: `regia_rules::spawn::pick_spawn(points, ask, contestants,
policy, state, occluded)`, or `MatchSession::pick_spawn`, which fills the side and tick from the session.
`occluded(a, b)` must answer from map data every peer shares.

### Spawn protection

A `SpawnProtection { ticks, ends_on_fire }` value inserts `SpawnProtected` on a pawn
(`SpawnProtection::protect`). While `ticks_left > 0` the damage step ignores hits on it (heals still
apply), counts it down once per fixed step and removes it at zero. With `ends_on_fire`, the pawn's first
shot ends it: a `ShotFired { shooter }` message, or a `DamageEvent` the pawn is the source of (that hit
lands). Nothing is protected unless the game inserts the component.

### `rules.spawn_preview`

`rules.spawn_preview { player }` (read-only) answers `{ policy, order, team, side, tick, pick, points,
respawn, respawns }`. `order` is `arrival` when the walk follows `PlayerStart` arrival and `position` when
it follows the translation. `points` lists every start in walk order with `entity`, `position`, `team`,
`tags`, `priority`, `enabled`, `eligible`, `refused` (`disabled`, `wrong_team`, `missing_tag`,
`blocked` or `null`), `nearest_enemy_cm`, `enemies_in_sight`, `teammates_near`, `recent_uses` and
`score`. The score terms use the session's scored weights, or the default weights under another
policy, so an editor overlay can show them. `pick` is the start a pick would choose now; the preview
does not move the session's round-robin cursor, recent uses or generator. `respawn` is the session's
respawn settings and `respawns` the running match's pending respawns, lives and eliminated seats.

| Code | When |
|---|---|
| `spawn_player_missing` | `player` is not a live entity id |
| `F0002` | `player` is absent or not an integer |

```json
{ "method": "rules.spawn_preview", "params": { "player": 4294967297 } }
```
