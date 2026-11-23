# Third-person demo: combat, menus and saves (agents)

`samples/third_person` is a small playable demo (ADR-2596): the M20 slice (character, boom camera,
Fox with foot IK, coins, split screen, a car) plus two enemy kinds, a melee combo, ragdoll deaths,
checkpoint respawns, a title screen, a settings menu, procedural sound and saves through the save
flow. It adds **no agent methods** (its systems below are driven with the existing conversation, cloth and demo methods): everything is driven and checked with the existing protocol
(`play.*`, `scene.*`, `state.*`, `settings.*`, `save.*`) on reflected components and resources.
`samples/third_person/tests/combat_agent.rs` is the worked script.

## Driving it

| Goal | Call |
|---|---|
| Leave the title screen | `state.set { "state": "GameFlow", "value": "Loading" }` (the flow goes on to `Playing`) |
| Move, jump, sprint | `play.inject_input` keys `KeyW` `KeyA` `KeyS` `KeyD`, `Space`, `ShiftLeft` |
| Attack (combo: press again while the swing's combo window is open) | `play.inject_input` key `KeyF` (or the left mouse button) |
| Place the player | `scene.set_property` on the `Character` entity's `Transform.translation.*` (zero `CharacterState.velocity.*` first) |
| Settings | `settings.set { "path": "audio.master" \| "audio.music" \| "audio.sfx" \| "input.sensitivity" \| "input.invert_y" \| "graphics.preset", "value": ... }` |
| Save and load | `save.save` / `save.load { "slot": "Chapter1" }`; F5 / F9 in game go through the save flow (`SaveRequest`) |
| Fixed steps | `play.pause`, then `play.step { "frames": n }` (16 ms frames in the tests, 60 Hz fixed step) |

Entities have names: `enemy-0` (grunt), `enemy-1` (ranged caster), `enemy-2` (grunt),
`checkpoint-0..2`, `corpse-N`, `hud-health`, `hud-damage-numbers`, `hud-banner`, `title-menu`,
`settings-menu`, and the menu buttons (`menu-play`, `menu-continue`, `menu-settings`, `menu-quit`,
`settings-master-up`, `settings-invert-y`, `settings-preset-next`, ...), which `ui.click` can press in a
windowed run.

## What to read

| Component / resource | Where | Fields |
|---|---|---|
| `CombatSummary` | the `sample-state` entity | `enemies_killed`, `enemies_alive`, `player_deaths`, `respawns`, `player_hits`, `enemy_hits`, `ragdolls`, `checkpoint`, `defeated`, `player_health`, `footsteps`, `hit_sounds`, `ambient_playing`, `saves`, `loads`, `migrated` |
| `EnemyBrain` | each enemy | `state` (`patrol`, `chase`, `attack`, `recover`, `hold`, `give_up`), `alert`, `noticed`, `noticed_by` (`sight`, `hearing`, `hurt`), `gave_up`, `strikes`, `target_distance` |
| `Enemy` | each enemy | `id`, `kind` (`Grunt`, `Ranged`), `route`, `yaw` |
| `BehaviorTreeStatus`, `Blackboard` | each enemy | the tree's last tick per node; `disabled`, `alert`, `in_range`, `target_pos` |
| `MeleeState` | each player character | `combo_step`, `swings`, `hits`, `hit_window`, `combo_window`, `damage`, `refused` |
| `Combatant`, `Downed` | fighters | `team` (0 player, 1 enemy), `attack_grant`; `respawn_in` while a player is down |
| `Corpse` | each ragdoll rig | `team`, `bodies`, `limp`, `settled` |
| `DamageNumbers`, `HudLabel` | `hud-damage-numbers`; `hud-health`, `hud-banner` | the HUD's damage numbers (`entries`, `total`); the health and banner strings |
| `Attributes`, effects, grants | fighters | through `abilities.inspect` / `abilities.check` ([abilities](abilities.md)): `health`, `health_max`, `Status.Staggered`, `Cooldown.*` |
| `GameSettings` | resource | `play.expect { "resource": "GameSettings", ... }` |
| `GameFlow` | state | `play.expect { "state": "GameFlow", ... }` |

`CombatProgress` (checkpoint, defeated ids), `CombatStats`, `SoundStats`, `SaveStatus` and
`MenuState` are reflected resources too (`resources.list`); `CombatSummary` mirrors them on an
entity so `play.expect` and `play.wait_until` can test them.

## How it works

- **Enemies.** `assets/ai/grunt.bt.ron` and `ranged.bt.ron` (`.bt.ron` v1): a `sense` service over a
  selector that holds while staggered or down, fights while alert (attack in range, else chase) and
  patrols otherwise. `sense` uses `regia_ai::in_sight_cone` + `line_of_sight`, `can_hear` (the
  player's footsteps and swings are `HearingEvent`s addressed to the enemy team) and being hit. An
  alert enemy gives up after 4 s without perceiving the player or 20 m from its post. Movement is a
  `regia_nav::NavCrowd` over a navmesh baked from the level's static boxes at startup.
- **Health and hits.** Attributes from `assets/combat/*.attrs.ron`; a hit is the instant
  `combat/hit.effect.ron` (`Damage.Amount` parameter) plus `combat/stagger.effect.ron`
  (`Status.Staggered` for 0.45 s: no movement, no attack). `HealthBridge` mirrors `health` into
  `Health` and raises the death.
- **Melee.** `assets/combat/combo.montage.ron` plays on a procedural sword-arm rig: three sections,
  `hit` windows (payload = damage) and `combo` windows. While a hit window is open a sphere overlap
  (physics shape query) in front of the player damages each enemy once. Each swing is the `slash`
  ability (cooldown 0.15 s, refused while staggered).
- **Death.** A limp `regia_framework::ragdoll::Ragdoll` on a procedural humanoid replaces the
  fighter (box meshes on its bodies). A player is `Downed` for 2 s, then respawns at the active
  checkpoint with full health (`combat/heal_full.effect.ron`).
- **Saves.** `SaveFlowPlugin`; shape v2 stamps `third_person.save_shape` = 2 in `SaveBlobs`. A v1
  save (coins only) loads and migrates: combat progress resets to the start, `SaveStatus.migrated`
  counts it. Settings persist in `<data dir>/settings.ron` (`SettingsPlugin`).

## Checks agents run

- An enemy notices and reaches the player: place the player 6 m in front of `enemy-0`, step until
  `EnemyBrain.alert`, then until `{ "a": { "name": "enemy-0" }, "b": { "component": "Character" },
  "op": "within", "meters": 2.0 }`.
- Attacks kill: `KeyF` pressed / released every 10 frames until `CombatSummary.enemies_killed >= 1`;
  a `Corpse` with `bodies >= 8` follows.
- Death and respawn: step until `CombatSummary.player_deaths >= 1`, then `respawns >= 1`, and the
  `Character` is within 1.8 m of the active `checkpoint-N` with `player_health` 100.
- Persistence: `settings.set`, `save.save`, quit (a new process on the same data directory),
  `GameSettings` reads back, `save.load` restores `CombatSummary.checkpoint` and `defeated`.

There are no error codes of its own: the protocol's errors apply.

## Guide, cape, soft ball and demo recording

Three framework systems that no sample used before are shown in the level. All of them are driven and checked with
existing methods; `samples/third_person/tests/systems_agent.rs` is the worked script.

| Part | Keys | What an agent reads |
|---|---|---|
| The guide (`npc-guide`, west of the start) | `T` talk (within 4 m), `1` / `2` answer, `L` English / German | `TalkState` on `npc-guide`: `talking`, `choice_pending`, `options`, `lines_seen` (`guide#greet`, `guide#ask`, then `guide#yes` or `guide#no`), `answer` (-1 until chosen), `last_event` (`quest_given`), `finished`, `locale`; `captions.state` for the subtitles on screen |
| The cape (`cape-0`) and the ball (`soft-ball`) | none | `physics.cloth_inspect { "target": { "entity": E } }` (`kind`, `particles`, `pinned`, `bounds_min` / `bounds_max`, `max_stretch`, `volume`, `contacts`, `digest`); `physics.cloth_wind`, `cloth_step`, `cloth_reset` move and replay them |
| Demo recording | `F6` record / stop (saves `<data dir>/demos/run.frec`), `F7` play / pause, `[` `]` scrub one second, `F8` scrub bar | `demo.status` (`mode`, `tick`, `steps`, `keyframes`, `last_seek`, `diverged_at`); `demo.record_start`, `demo.record_stop`, `demo.play`, `demo.pause`, `demo.seek` |

- **Conversation.** `guide.conv.ron` has four lines, one `Choice` and an `Event`. The run is started with the framework
  runner (`ConversationPlugin`); the lines take their words from `assets/localization/{en,de}/dialogue.ftl`, so `L`
  changes the next run's language and the subtitle text with it. Error codes are the conversation ones
  ([dialogue and subtitles](dialogue-subtitles.md)): talking out of reach starts nothing and answers no error.
- **Cape.** An 11 x 6 cloth cooked in memory, its top row (`pinned` 11) bound to a one-joint skeleton mounted on the
  character at shoulder height; one torso capsule proxy (`colliders` 1). Wind from `physics.cloth_wind` moves it; with
  the wind cleared it settles back (bounds drift under 1 cm over half a second).
- **Soft ball.** A pressure octahedron dropped from 2.5 m; it rests on the ground with its volume near the rest volume.
- **Demo.** Each player character is demo state (`DemoTracked`, named `player-N`); its input is part of it. Seeking
  restores the nearest keyframe (every 60 fixed steps for the keys) and replays, so a run recorded with the same keys
  held scrubs back to the recorded positions. The sample test pins the position at ticks 0, 30, 45, 75, 100 and 120 of
  a walk. Keep the simulation in `FixedUpdate` and use `enable_fixed_step` in a headless host for exact seeks.
