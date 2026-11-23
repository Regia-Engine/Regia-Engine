# Contact sounds from physics

Physics objects sound when they hit, slide and roll. `regia_framework::physics_audio::PhysicsAudioPlugin` (feature
`client`, opt-in: add it after `PhysicsPlugin` and `AudioPlugin`) reads each fixed step's `ContactForce` messages and
plays the `ImpactBank`'s sounds at the contact through the emitter pipeline (distance gain, panning, the medium).
Nothing plays until an `ImpactBank` resource exists. ADR-3270 holds the curves and rules; the file format is
`docs/formats/impactbank.md`.

Opt a collider in the way camera shake does: `Collider::active_events.contact_force = true` and a
`contact_force_threshold` (0 reports every touching step). Name its material with `PhysicsMaterialRef`; a collider
without one falls through to the other material, then the bank's `default_material`.

## What plays

- **Impacts.** The impulse and the approach speed at the contact *before* the step give the level and pitch. The
  impulse is the momentum that stops the approach (the reduced mass times the approach speed, plus any bounce), at
  least the reported force × fixed step (the physics step reports only its last substep's force). Below `min_impulse_ns` or `min_normal_speed_m_s` nothing plays, so a resting box never
  retriggers. A dynamic body that just played an impact waits `cooldown_steps`. At most `max_new_impacts_per_step`
  play per step: the loudest; the rest are dropped and counted. A play beyond the emitter's range (or with an
  unregistered sound) starts no voice and is logged with `started: false`; it does not count as played, nor does it
  spend the per-step cap, but it does start the body cooldown (an out-of-range rattle is planned and logged once per
  cooldown, not on every step). A scrape/roll voice whose resolved clip changes (a bank edit or a
  material change under a sliding pair) restarts on the new clip.
- **Scrapes and rolls.** A sliding pair plays its scrape loop (level from slip speed × normal force), a rolling pair its
  roll loop (angular speed × radius; it rolls while the slip is at most `roll_slip_ratio` of that). One loop per pair;
  it fades to silence over `fade_steps` fixed steps after the motion stops and then stops.
- **Breaks.** A destruction break (`DestructionFxMessage`) plays one break sound per destructible per step, its
  `fx_tag` mapped to a material by `destruction_materials`.
- **Footstep surfaces.** With `PhysicsFootstepSurfaces` inserted and `install_physics_footstep_probe(&mut app)`
  called, footsteps take the surface from the physics material under the foot (`surfaces` maps material → surface;
  an unmapped material is its own surface name). The downward ray from 0.25 m above the foot skips the walker's whole
  body: the walker entity, the nearest non-static rigid body among the walker and its ancestors, and every
  descendant of either. A character whose capsule `Collider` sits on the walker, on a child of it, or on the body a
  child "feet" walker hangs under still reads the floor under it.

The set a contact uses: the pair's entry (either order), then the faster body's material, then the other's, then
`default_material`.

## Read the bank and the live pairs

```json
{ "method": "audio.impacts" }
→ { "bank": { "format_version": 1, "materials": { "wood": { "impacts": ["impact/wood_1", ...], "scrape": "impact/wood_scrape",
      "roll": "impact/wood_roll", "breaks": [], "volume_db": 0.0, "pitch": 1.0, "missing_sounds": [] } }, "pairs": [],
      "default_material": "default", "destruction_materials": {}, "curves": { ... }, "budget": { "cooldown_steps": 6,
      "max_new_impacts_per_step": 8, "fade_steps": 6, "max_loop_voices": 16 }, "bus": "World", "max_distance_m": 40.0,
      "played": 12, "breaks_played": 1, "recent": [ { "kind": "impact", "materials": ["wood", "concrete"], "source": "first",
      "variant": 2, "sound": "impact/wood_3", "curve_db": -6.1, "volume_db": -5.4, "pitch": 0.97, "impulse_ns": 3.1,
      "normal_speed_m_s": 6.2, "position": [0, 0, 0], "started": true } ] },
    "state": { "step": 600, "contacts": 980, "impacts": 12, "dropped_by_budget": 192, "dropped_last_step": 0,
      "max_impacts_in_a_step": 8, "cooled_down": 3, "loops_started": 2, "loops_stopped": 1, "loops_over_budget": 0,
      "breaks": 1, "pairs": [ { "a": 4294967301, "b": 4294967298, "kind": "roll", "level_db": 3.5, "gain": 1.49,
      "pitch": 1.2, "last_active_step": 600, "position": [3.1, 0, 0], "voice": true, "voiced": true } ] } }
```

`bank` is null until a bank exists; `state` is null without `PhysicsAudioPlugin`. `wait_until` on
`state.dropped_by_budget` or a pair's `gain` checks the budget and the fades.

## Probe the curves offline

`audio.impact_probe` answers what one contact would do, on a copy of the bank (or of the `ron` text given): nothing
plays and the live history does not move.

```json
{ "method": "audio.impact_probe", "params": { "a": "wood", "b": "concrete", "impulse_ns": 10.0, "normal_speed_m_s": 5.0,
  "slip_m_s": 0.0, "rolling_speed_m_s": 2.0, "seed": 7 } }
→ { "source": "first", "impact_db": 0.0, "impact_pitch": 1.0, "impact": { "variant": 1, "sound": "impact/wood_2", ... },
    "loop": { "kind": "roll", "level_db": 0.0, "pitch": 1.0, "sound": "impact/wood_roll" } }
```

## Set the bank

`audio.impacts_set { ron }` replaces the bank with a validated `.impactbank.ron` text, keeping its play log;
`seed` reseeds an existing bank (the same seed picks the same variant sequence; a seed without a bank is
`impacts_no_bank`); `enabled: false` removes it (any supplied `ron`/`seed` is still validated first).

## Error codes

| Code | When |
|---|---|
| `F0013` | No `Audio` in the app (`audio.impacts`, `audio.impacts_set`). |
| `impacts_bad_param` | A number outside its range or a value of the wrong type. Ranges: `impulse_ns` 0..=1e6, `normal_speed_m_s`, `slip_m_s`, `rolling_speed_m_s` 0..=1e4, `normal_force_n` 0..=1e9; `seed` a non-negative integer. |
| `impacts_no_bank` | No `ImpactBank` and no `ron` given. |
| `impacts_no_set` | Neither material, their pair nor `default_material` has a set (`audio.impact_probe`). |
| `impactbank_parse` | The `ron` text does not parse. |
| `impactbank_version` | Its `format_version` is not 1. |
| `impactbank_name` | An empty material, pair, tag, default material or bus name. |
| `impactbank_value` | A curve, budget or set number out of range or not finite. |
| `impactbank_sounds` | An empty sound name or more than 64 variants in a list. |

## Placeholders and limits

The sounds in `crates/regia_framework/tests/physics_audio.rs` and the agent tests are **placeholders generated in
code** (a decaying click and plain tones), not recordings; a game supplies its own. A loop voice stays where its
contact began (voices have no position update); the contact point is reported in `pairs`. Debris chunks spawned by
destruction do not raise contact-force events yet, so they make no impact sounds (ADR-3270 § 7).
