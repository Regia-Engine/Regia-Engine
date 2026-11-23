# Crash fireball: seen size, soot and toroid

`regia_vfx::crash::crash_fireball` (ADR-1498) is the fireball layer of a crash
([crash effects](crash-fx.md)), sized by the same yield (`yield_t`, TNT-equivalent tonnes). This
page covers how big the ball *looks*, how it turns to soot and rolls into a toroid, and how to check
it without a GPU through `vfx.crash_fireball`.

## What it is

| Emitter | What it is |
|---|---|
| `flash` | a smooth glowing disc of the young ball for 50–200 ms (no flame flicks), with the light that lights the land out to `80 R` |
| `fireball` | the body: 36 (full) / 14 (lite) fireball-sheet cards on a shell `0.3–0.65 R` around the centre, so the ball is a volume of layered cards with a lumpy silhouette; carries the ball's light |
| `soot` | carbon-black soot laid on the ball's shell from `0.22 τ`, wrapping the flame by `τ` |
| `soot_ring` | soot thrown outward on a ring above the centre from `0.27 τ`: the rising mass becomes a toroid |
| `billows` (full) | orange billows rolling out on the ring under the soot |
| `heat_haze` (full) | hot air over the ball bends the scene behind it |

`R` is `1.75 W^(1/3)` m (the luminous diameter `3.5 W^(1/3)` for `W` kg TNT), `τ = √(R/g)`. The ball
is spawned `0.55 R` above the crash point (`FIREBALL_SPAWN_HEIGHT`), so its lights sit in the ball.
Both lights reach `80 R` and so have the ball as their sphere source: on the ground and on lit puffs
(the dust skirt under the ball) the light is at most `I / R²`, so the skirt is not lit past white
and there is no horizontal glare band.

## Seen size: the footprint model

`fireball_footprint(def, times)` plays the definition with the CPU probe and lays every visible
card down back to front as a disc of its opaque part on a `R / 60` grid in a vertical plane: a fire
card's flame part is `SHEET_FLAME_RADIUS` (0.27) of its width (a test measures this on the real
fireball sheet), a soot puff's body `PUFF_BODY_RADIUS` (0.35). A cell is flame where the accumulated
flame coverage is at least one half, soot where it is opaque otherwise. The flash glow, haze and
meshes are left out. `fireball_peak_radius` and `fireball_peak_time` of `vfx.crash_profile` come
from this model since ADR-1498 (before it they were the cards' extent, which counted the cards'
transparent margins: a "90 m radius" ball looked about 60–90 m across in the shot-4 frames).

## `vfx.crash_fireball`

```json
{"jsonrpc":"2.0","id":1,"method":"vfx.crash_fireball","params":{"yield_t":250.0,"times":[1.0,1.25,4.0]}}
```

Params: `yield_t` (0.05–1000, default 1), `times` (1–32 values in 0..=30 s; default 0.1, 0.25,
0.5, 1, 1.25, 1.5, 2, 3, 4, 5), `tier` (`full` or `lite`; default from the VFX profile), `wind`
(`[x, y, z]` m/s). No scene or GPU is needed. Returns:

`{ effect, tier, yield_t, target_diameter, spawn_height, time_scale, peak: { time, flame_diameter, width }, black_at, frames: [{ time, flame_area, flame_diameter, soot_area, width, height, top, centre_height, soot_ring_radius, soot_hole, soot_aspect }] }`

- `target_diameter` is `3.5 W^(1/3)`; `peak.flame_diameter` is the largest equivalent diameter of
  the flame area over `0..1.6 τ`, and `peak.time` when.
- `black_at`: the first time after the peak with the flame under a tenth of its peak area.
- `soot_ring_radius`: median distance of the soot puffs from their vertical axis; `soot_hole`:
  its 25th percentile over the median (near 1 for a thin ring); `soot_aspect`: twice the ring
  radius over the soot's height spread (above 1: wider than tall).

Errors: invalid params only (no `F` codes).

## Numbers at 250 t (CPU footprint and probe)

| Check | Target (showcase X1) | Full / lite |
|---|---|---|
| seen peak (equivalent diameter) | 150–250 m | 195 / 188 m |
| peak time | 1–1.5 s | 1.45 / 1.35 s |
| flame under a tenth of its peak | by 3–5 s | by 4 s (none at 4 s) |
| rise (body centroid, `0.5 τ` to `τ`) | 20–40 m/s | 25.1 / 25.4 m/s |
| soot toroid at 4 s | ring at least `0.5 R`, hole at least 0.55, wider than tall | ring 81 / 85 m, hole 0.79 / 0.74, aspect 1.53 / 1.63 |
| orange from 1.2 km at 1.25 s (lavapipe, real shader) | 150–250 m | 213 / 193 m |
| shot 4 (3.3 km, 4K Cinematic, 5090; full) | 150–250 m | 168 m peak at 1.25–1.5 s, about 225 m wide; 33 m left at 4.75 s |

Every yield's seen peak is 0.88 of `3.5 W^(1/3)` (1 t: 31 m at 0.6 s; 1 kt: 309 m at 1.85 s).

## How to check it in a frame

1. `vfx.crash_fireball { "yield_t": 250, "times": [1.0, 1.25, 1.5, 3.0, 4.5] }` for the expected
   numbers.
2. Fire the crash (`vfx.crash`), render the frames at 1.0–1.5 s after the detonation from the shot
   camera and count the orange pixels in a box around the ball (HSV hue 8–50°, saturation 0.35 and
   up, value 0.3 and up). With `m` metres per pixel at the crash's distance the equivalent diameter
   is `2 m √(px / π)`; it should land within the footprint's peak. At 3.3 km and 4K / 32° one pixel
   is 0.48 m.
3. At 4–5 s the orange is gone and a dark, wider-than-tall mass rises at 20–40 m/s.
