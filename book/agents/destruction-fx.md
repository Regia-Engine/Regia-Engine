# Destruction effects: sound, VFX, decals

`regia_framework::destruction_fx::DestructionFxPlugin` plays the sound, dust and marks of destruction
from one table (ADR-2734). Every `DestructionFxMessage` that `DestructionPlugin` writes (a bond broke,
a chunk was knocked loose, a section collapsed; each with a position, a magnitude, the `fx_tag` and the
break event's seed) is looked up by its `fx_tag` in a `.destructionfx.ron` table
([format](../../docs/formats/destructionfx-ron.md)), which names a sound cue, a VFX system and a decal.

| Method | Permission | What it does |
|---|---|---|
| `physics.destruction_fx_table` | play | Read the table; with `ron`, replace it first. |
| `physics.destruction_fx_stats` | read | Plays, drops by cap, sounds, VFX, decals, pooled-buffer growths, the last step's plays. |

Both need `DestructionFxPlugin`; without it they answer `F0003` (not found) naming the plugin.

<!-- ignore-reason: fragment of a game's setup: `app` and the table file come from the game -->
```rust,ignore
app.add_plugins((PhysicsPlugin, DestructionPlugin::default(), DestructionFxPlugin {
    table: Some(DestructionFxTable::from_ron(&std::fs::read_to_string("fx/site.destructionfx.ron")?)?),
}));
```

## What plays

Each fixed step, after the destruction step, the planner reads the step's messages in order:

1. The row is the tag's own, else the table's `fallback`, else nothing (`unmatched`).
2. A row's `kinds` (default all) and `min_magnitude` filter the moment (`filtered`).
3. The row's `max_per_step` (default 8) and the table's `max_per_step` (default 32) cap the plays;
   the rest are dropped and counted (`dropped_tag_cap`, `dropped_step_cap`). A blast that breaks a
   hundred bonds in one tick plays a handful of cracks, not a hundred.
4. Variation comes from the event's seed: the cue's pick and pitch (`Audio::play_cue_seeded`), the
   decal's size (`1 ± scale_jitter`) and spin. A server and its replicas see the same events with the
   same seeds, so they pick the same variations.

Plans go into buffers the player keeps from step to step (pooled playback): once a busy step has
sized them, the planner allocates nothing more. `buffer_growths` counts every time a buffer had to
grow, so a test can prove it stays flat after warm-up.

Planning and the counters run in every build, so a headless server reports what it would play. Playing
needs the `destruction-fx` feature (`regia/destruction-fx`, which turns on `client`; `regia_dylib`
forwards it): sound cues go to `regia_audio::Audio`, VFX systems to `regia_vfx::VfxScene`, decals to
`regia_vfx::DecalPool`. A missing resource skips that part; a cue the app never registered counts as
`sounds_refused`, a VFX system the scene does not know as `vfx_unknown`. `physics.destruction_fx_table`
reports `playback: false` in a build without the feature.

```json
{ "method": "physics.destruction_fx_stats", "params": {} }
→ { "stats": { "messages": 140, "planned": 36, "dropped_tag_cap": 96, "dropped_step_cap": 0,
     "sounds": 36, "vfx": 36, "decals": 36, "buffer_growths": 3, ... },
    "last_plays": [{ "row": 0, "kind": "BondBreak", "position": [-4.0, 1.2, 0.1], "scale": 1.12 }],
    "table": "concrete_site", "load_error": null }
```

## Errors

| Reason | When |
|---|---|
| `destruction_fx_parse` | The `ron` text does not parse. |
| `destruction_fx_version` | A `format_version` this engine cannot read. |
| `destruction_fx_invalid` | A rule fails: a repeated or empty tag, a `fallback` naming no row, a value out of range. |

A table handed to the plugin that fails validation leaves the plugin empty and puts its code in
`load_error`.

## The sample

`samples/destruction_lab` adds the plugin to the server and both clients with `lab_fx_table()`: every
tag falls back to one row (crack cue, dust system, a dark scorch decal, 12 plays per step). The repo
holds no sound or particle assets for the lab, so those count as refused or unknown; the decals are
plain tints. `lab.report` shows each process's `fx` counters.

## Limits

- One table per app; replace it with `physics.destruction_fx_table { ron }` or
  `DestructionFxState::player.set_table`.
- Sound cues play without a position (the cue's own spatial settings apply); VFX systems and decals
  play at the moment's position, decals facing down onto the ground.
