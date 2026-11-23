# Crash effects: from a vehicle crash to a large surface explosion

`regia_vfx::crash` (ADR-1398, rescaled by ADR-1414) is a crash or impact for any game, sized by
one number: the TNT-equivalent yield of its energy release, `yield_t` in tonnes, from 0.05 t (a
light vehicle) to 1000 t (1 kt). Every layer's `.vfx.ron` v5 definition is generated for the
yield (cube-root scaling for lengths, buoyancy scaling for the fireball's timing, the speed of
sound for the blast) and registered under a name that carries it, so a small and a large crash
can play in one scene. The showcase impact is 250 t (0.25 kt).

| Layer (effect name) | What it is | Measured at 250 t (CPU probe, full / lite) | Target at 0.2–0.5 kt |
|---|---|---|---|
| `crash_fireball@<t>t` | flash glow with a light out to `80 R`; the body as a volume of layered fireball-sheet cards (the ball's light, a sphere source of `R`); soot on its shell and on a rolling ring (toroid); billows; heat haze (ADR-1498, [crash fireball](crash-fireball.md)) | flash out by 0.12 s; seen flame (CPU footprint) 195 / 188 m at 1.45 / 1.35 s, gone by 4 s; 213 / 193 m of orange from 1.2 km at 1.25 s on lavapipe; rises at 25 m/s; soot toroid (ring 81 / 85 m at 4 s) | flash 50–200 ms lighting the land to 2–3 km; 150–250 m across, peak 1–1.5 s; sooty by 3–5 s; rising 20–40 m/s |
| `crash_shell@<t>t` | condensation shell (humid air only; off by default) | 194 m out, gone by 0.69 s | 150–300 m, gone within 1 s |
| `crash_dust_<ground>@<t>t` | blast front (a thin skirt of snow and dust kicked up at the front, fading with distance), the snow or dust the blast wind lifts behind it, base surge, inner billow (full) | front (p90) 340 m/s, within 1.8 % of `343 t` beyond 150 m, visible to 1.85 km; lifted cover 402 / 405 m, never ahead of the front; surge 558 / 547 m out and 85 / 96 m high at 15 s | 343 m/s ± 10 % beyond 150 m, 2 km at ~6 s; lifted cover to 300–500 m; surge 400–700 m, 50–100 m high by 15 s |
| `crash_ejecta_<ground>@<t>t` | dark fountain of earth, ballistic clods and rocks, slow fines (full) | clods 253 / 255 m up, landing 567 / 583 m out | 200–400 m high, landing 500–800 m out |
| `crash_column@<t>t` | mushroom cap (body, veil, wisps), stem, crater smoke, burning hardware in the crater | cap top 1216 / 1233 m and 616 / 676 m across at 60 s; 24 m/s over the first 30 s; 1428 m at 180 s (full; the lite cap lives 150–180 s) | cap 800–1500 m by 60 s, 400–800 m across, ~25 m/s, stable for > 3 min, drifts with the wind |
| `crash_contact_<ground>` | the dust puff and clods at first ground contact, sized for a 12–14 m body | 10 m out at 0.5 s, gone by 1.35 s | a few frames before the blast, short-lived |
| `crash_ground_fire_<size>` | `small`, `medium`, `large` fires with a 30 m light and a thin plume | burn 200 / 300 / 420 s | 30–80 fires within 300 m, ≥ 3 min |

`<ground>` is `mud`, `snow` or `grass` (dust colour, clods, how fast the lifted cover settles; the
ejecta is dark earth on every ground). `R` is the fireball radius, `1.75 W^(1/3)` m for `W` kg. At
250 t the crater (`CrashScale::crater`) is 50 m across, 14 m deep with a 4 m rim, its ejecta
blanket 151 m and the scorch zone 302 m; 55 fires are lit; the cap stops near 1.1 km.

**Look under a sky (ADR-1454).** Lit puffs take the sun through their neighbours, so a dense cap (`self_shadow`
0.88) is lit by the sky in its core and by the sun on its thin rim, a bright dust adapts the low sun's orange
away (snow powder reads white-grey), and the flash light is a sphere the size of the fireball (the ground under
it is bounded, not a clipped patch). `self_shadow` is therefore the extinction of the puffs around a puff:
`6 × self_shadow²` chords toward the sun.

**Column colour (ADR-1472).** The column is what an energy-store vent lifts over snow: condensed water vapour,
snow powder and dust, with little soot. The cap's albedo is 0.42 as it leaves the fireball and 0.62 by 20 s; the
stem is 0.22 at the crater and 0.6 under the cap; the fireball's soot (40 puffs on full, 18 on lite, on the
ball's shell and a rolling ring, ADR-1498) is carbon black through the first `τ` and greys to 0.3 as it mixes in. The dark ring of small puffs around
the ball in the first seconds is the ejecta fountain: soil from under the snow. The cap is three emitters: `cap` (the body, 0.4–0.85 R),
`cap_veil` (large thin puffs that join it into one mass) and `cap_wisps` (small fast puffs on a torn rim), with the
same puff total as one emitter had.

**Blast front (ADR-1497).** The front's position, the camera shake, a dust wall lifted along its
path out to about 440 m at 250 t, and the gust it drags across the grass and tree snow past that
all run on one `distance / 343` clock: see [Crash blast front](crash-blast-front.md)
(`vfx.blast_gust`, `gust_sites`).

**No tropes** (the showcase's physics check): debris has no flame trails and does not glow, only
the fires carry lights, nothing outruns the blast front, and a camera shakes only when the front
reaches it.

**Tiers.** `full` is the desktop look. `lite` is the fallback for Low, the Steam Deck and mobile:
fewer, larger puffs with the same size and timing envelope, no heat haze, fewer flame cards per
fire, and a cap that lives 150–180 s. The default comes from the VFX profile
(`CrashTier::for_profile`).

**Particle pool.** The GPU pool is a ring buffer: a long-lived puff is overwritten once the
pool's capacity of particles spawn after it. `crash_ring_slots(scale, tier, medium)` is what a
whole crash occupies over its longest lifetime (about 34 000 slots on full and 8 200 on lite at
250 t, against 100 000 on Desktop High and 10 000 on Mobile High); leave room for the scene's
other effects or the cap thins early.

**Distance.** LOD distances grow with the yield: at 250 t every layer keeps full detail out to
8 km, and the column stays drawn past 12 km (the 8 km scene never crosses its last band). LOD
only scales new spawns, so live puffs never pop. Set the camera's far plane past the column (at
least the camera distance plus 2 km).

## Schedule a crash

```json
{"jsonrpc":"2.0","id":1,"method":"vfx.crash","params":{"position":[700.0,105.7,5.0],"ground":"snow","yield_t":250.0,"wind":[3.0,0.0,1.0],"impact_in":1.0}}
```

Spawns an entity with a `CrashSequence`. The VFX plugin runs it on the frame clock (deterministic
under a fixed step): the contact puff fires `contact_lead` seconds (default 0.05, 3 frames at
60 Hz) before the detonation, the detonation fires `impact_in` seconds after the first frame
(default: `contact_lead`, so the puff fires at once), and the plugin sends `CrashEvent`s:
`Contact`, `Detonation`, `Crater(CraterSpec)` (with the detonation: the crater's size for a decal,
terrain or wreck system; the battered hull belongs at the crater point) and `BlastArrived {
distance }` when the front reaches the `VfxView` camera, with a camera shake that falls with
distance. Leave out layers with `"fireball"`, `"shell"` (default false), `"dust"`, `"ejecta"`,
`"column"`, `"fires"` or `"contact"`. With the detonation the sequence also adds a fire light
(`vfx.fire_lights`) at every visible ground fire: same positions, same seeds, so the lit area
follows the flames (ADR-1448); `"fire_lights": false` turns that off. `seed` (an unsigned integer,
default `CrashBurst::default().seed`) seeds the first layer; the other layers and the fires derive
theirs from it, so the same seed and inputs repeat the same crash. The wreck's glow is still
`vfx.spawn_crash_fires` (use `count` 0 next to `vfx.crash`). Returns `{ entity, tier, ground,
yield_t, impact_in, contact_lead, fire_lights, effects, fireball_radius, column_height, blast_reach, fire_count, crater }`.

Errors: `F0002` for a bad position, normal, ground, tier, yield (0.05–1000), `impact_in`
(0–600 s), `contact_lead` (0–1 s) or a non-boolean layer switch; `F0016` when the app has no VFX
scene (add `regia_vfx::VfxPlugin`).

`vfx.crashes` lists every scheduled crash: `clock`, `contact_fired`, `detonated`, `front_radius`,
`arrived` and the layers it fired (`instances`).

From Rust: `ensure_crash_effects(scene, scale, tier, wind)` then `spawn_crash(scene, &CrashBurst)`
fires the detonation at once (flat ground); `spawn_crash_on` takes a ground sampler for the fires;
`crash_fire_sites` gives the fire positions for a host that places them itself;
`CrashScale::crater` and `CrashScale::ejecta_launch` give the crater and the ejecta arcs to a
decal or mesh-debris system;
`blast_arrival_time(origin, listener)` is the shake and sound delay.

## Measure an effect

```json
{"jsonrpc":"2.0","id":2,"method":"vfx.crash_profile","params":{"effect":"crash_column","yield_t":250.0,"times":[30.0,60.0],"wind":[4.0,0.0,0.0]}}
```

Runs the CPU probe (`regia_vfx::probe`, no GPU and no scene needed). `effect` is a base name
(`crash_fireball`, `crash_shell`, `crash_column`, `crash_dust_<ground>`, `crash_ejecta_<ground>`,
scaled by `yield_t`), a tagged name (`crash_dust_snow@250t`), `crash_contact_<ground>` or
`crash_ground_fire_<size>`. It returns `summary` (`flash_end`, `fireball_peak_radius`,
`fireball_peak_time`, `fire_gone_at`, `fireball_rise`, `soot_time`, `soot_luma`, `soot_alpha`,
`shell_radius`, `shell_gone_at`, `front_speed`, `front_error`, `front_reach`, `lift_reach`,
`lift_leads_front`, `surge_time`, `surge_front`, `surge_height`, `ejecta_top`, `ejecta_landing`,
`column_top_60s`, `column_width_60s`, `column_rise_30s`, `column_drift_60s`, `column_top_180s`,
`contact_reach`, `contact_gone_at`; `null` where it does not apply) and, at each of `times`
(1–64 values, 0–240 s), every emitter's envelope. The probe leaves out curl noise and uses flat
ground at the effect's origin: an envelope in metres and seconds, not pixels. Errors: `F0002` for
an unknown effect, a non-canonical tag, a bad yield or bad times.

## Check it

1. `vfx.crash_profile` for each layer at the yield you use: every number of the table inside its
   target.
2. `vfx.crash` with `impact_in` of a second, then `seeing.screenshot` from the shot camera at
   1.2 s, 6 s, 15 s and 60 s (`wait_until` on the frame count): a fireball wrapped in black soot,
   a low surge spreading over the ground with dark ejecta over it, then a mushroom cap standing
   far above the ridge.
3. `vfx.crashes`: `arrived` turns true about `distance / 343` s after `detonated`.
4. `vfx.stats`: the crash instances stay inside their system budgets.

`samples/vfx_lab --crash-sheets <dir> [w h]` writes frames at 0.1–180 s and contact sheets for
both tiers from the showcase's shot-4 camera (3.3 km) and from 1.2 km; `--crash-bench [file]
[quick]` times the VFX passes at 1440p and 4K. The GPU test `crates/regia_vfx/tests/crash_fx.rs`
checks that two runs give identical frames, the fireball's screen extent from 1.2 km, no white
clipping, no fire left at 6 s, and the cap's height on screen from 3.3 km at 60 s.
