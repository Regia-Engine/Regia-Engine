# samples/third_person: the third-person demo

A small playable third-person demo built only on engine APIs (M20 slice, then ADR-2596):

- A rapier-driven character (walk, sprint, double jump) with a boom camera that shortens at walls.
- The Khronos Fox (CC0, fetched and cooked at first run) animated from the character state, with
  foot IK on the stairs and the slope. Without the download the sample plays capsule-only.
- Eight coins collected through trigger scripts, a HUD, a pause menu, split screen (F2), a car (E).
- **Enemies**: two melee grunts and a ranged caster. Each runs a behavior tree
  (`assets/ai/*.bt.ron`) over a navmesh crowd: patrol, notice (sight cone and line of sight,
  hearing footsteps and swings, being hit), chase, attack, give up.
- **Combat**: health, hits and hit reactions through abilities, attributes and effects
  (`assets/combat/`). The player's three-hit combo is a montage (`combo.montage.ron`) on a sword
  arm; hits are physics shape queries. Damage numbers on the HUD.
- **Death**: fighters fall as ragdolls; the player respawns at the last checkpoint (blue pads).
- **Title screen and settings**: Play, Continue, Settings (master, music and effects volume, mouse
  sensitivity, invert Y, graphics preset), Quit. Settings persist.
- **Sound**: footsteps, swings, hits and falls are synthesized; the wind bed and the music pad are
  `.loopgen.ron` graphs (`assets/audio/`). No audio files ship.
- **Saves** through the save flow (`SaveFlowPlugin`): F5 / F9, the pause menu, Continue.
- **A guide to talk to** (gold block west of the start): `T` starts a short conversation
  (`assets/conversations/guide.conv.ron`: greeting, question, one choice, a reply) with subtitles
  through the caption board; `1` / `2` answer; `L` switches between English and German
  (`assets/localization/{en,de}`).
- **Cloth and soft body**: a cape on every player character (an 11 x 6 cloth sheet pinned to a
  one-joint mount skeleton, colliding with a torso capsule) and a pressure soft ball that drops
  near the guide.
- **Demo recording**: `F6` records the run and saves it on the second press, `F7` plays the last
  recording and pauses / resumes it, `[` and `]` scrub one second back and forward, `F8` opens the
  scrub bar.

## Run

```console
$ cargo run -p third_person                  # title screen
$ cargo run -p third_person -- --skip-title  # straight into play
$ cargo run -p third_person -- --split       # two local players
```

Flags for automated runs: `--exit-after-frames N`, `--screenshot PATH`, `--skip-title`.

## Controls

WASD move · Space jump (twice for a double jump) · Shift sprint · mouse look · F or left click
attack (press again during a swing for the next hit of the combo) · E car · F2 second player ·
F5 save · F9 load · Esc pause · T talk · 1 / 2 answer · L language · F6 record · F7 play / pause demo · [ ] scrub · F8 scrub bar.

## Files

| Path | What |
|---|---|
| `src/enemy.rs` | Enemy kinds, roster, the behavior-tree leaves (`Sense`, `Patrol`, `Chase`, `MeleeAttack`, `RangedAttack`, `Hold`), the navmesh crowd, bolts |
| `src/combat.rs` | Combat data, `deal_damage`, deaths, respawns, `CombatSummary` |
| `src/melee.rs` | The sword-arm rig, the combo montage, hit windows, the swing's sphere query |
| `src/ragdoll_fx.rs` | Death ragdolls on a procedural humanoid |
| `src/checkpoint.rs` | Checkpoint pads and the saveable `CombatProgress` |
| `src/menu.rs` | Title and settings panels |
| `src/sound.rs` | Synthesized one-shots, loop beds, footsteps |
| `src/npc.rs` | The guide NPC: talk key, choice keys, locale switch, `TalkState` |
| `src/cloth_fx.rs` | The cape (cloth bound to a mount skeleton) and the soft ball |
| `src/demo_fx.rs` | Demo record, play and scrub keys; tracks each character as demo state |
| `src/save.rs` | The save flow, save shape v2 and the v1 migration |
| `assets/ai/` | `grunt.bt.ron`, `ranged.bt.ron` |
| `assets/combat/` | Attributes, effects, abilities, `combo.montage.ron` |
| `assets/conversations/` | `guide.conv.ron`, `speakers.ron` |
| `assets/localization/` | `en/dialogue.ftl`, `de/dialogue.ftl` |
| `assets/audio/` | `ambient.loopgen.ron`, `music.loopgen.ron` |

## Saves and settings

Both live in `.regia/` next to this file (`saves/Chapter1.save`, `settings.ron`); a host can move
them with the `SampleDataDir` resource. Saves are shape v2 (coins plus checkpoint and defeated
enemies, stamped `third_person.save_shape` in `SaveBlobs`); a v1 save from before combat loads,
keeps its coins and starts the combat progress from the beginning.

## Tests

`cargo test -p third_person`: `tests/combat.rs` (direct, fixed 16 ms frames) and
`tests/combat_agent.rs` (the same checks through the agent protocol), plus the M20 tests. Agent
guide: `book/agents/third-person-demo.md`.
