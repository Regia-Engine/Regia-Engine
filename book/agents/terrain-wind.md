# Terrain wind

One wind for everything that sways (ADR-1853). Today the grass reads it; foliage will read the same
values. Wind is visual only: it never reaches the simulation, and it is a function of position and
the sway clock, so two renders with the same clock sway the same.

## Read it

```json
{ "method": "terrain.wind" }
```

```json
{ "direction": [1.0, 0.3], "unit_direction": [0.958, 0.287], "strength": 1.0,
  "frequency_scale": 1.0, "gust": 0.35, "grass_linked": true, "weather_steered": false }
```

The default is the grass's original fixed wind, so a scene that never sets it looks as before.

## The weather steers it

When the project turns the weather's scene wind on (`config/weather.ron` `effects.scene_wind`, or a
host inserts `regia_scene::weather::SceneWind`), `weather_steered` is `true` and the weather's
gust-free mean wind sets `direction` (its heading, in whole degrees) and `strength` (speed / 5 m/s,
in steps of 1/32, at most 4). `frequency_scale` and `gust` stay yours. The weather writes only when
that result changes a step, so gusts never redraw the grass's cached shadows, and a
`terrain.set_wind` holds until the weather's mean wind changes. To pin the wind for good, set the
scene wind instead (`render.set_scene_wind`) or set its mode to `off`. See
[Rain, snow and wet surfaces](weather-precipitation.md).

## Set it

```json
{ "method": "terrain.set_wind",
  "params": { "direction": [-0.4, 1.0], "strength": 1.8, "gust": 0.6 } }
```

| Field | Range | Effect |
|---|---|---|
| `direction` | `[x, z]` of length `1e-6..=1e6` | the way the wind blows toward; need not be unit length |
| `strength` | 0..4 | multiplies every class's sway amplitude (0 = still air) |
| `frequency_scale` | 0.1..4 | multiplies every class's sway frequency |
| `gust` | 0..1 | weight of the faster second harmonic (gustiness); under `natural`, the weight of the flutter band |
| `style` | `classic` \| `natural` | `classic` (default) is the original sway; `natural` is described below |

Out-of-range values are clamped; the reply is the wind in effect. A zero or non-finite direction
returns `wind_bad_direction`. The grass picks the new wind up on its next frame; `terrain.grass`'s
`sway_scale` still multiplies on top. A scene wind (`render.set_foliage_wind`,
[foliage-wind](foliage-wind.md)) replaces the direction and multiplies the strength while it is set.

From Rust, insert or change the `regia_terrain::TerrainWind` resource; `TerrainPlugin` pushes a
changed resource into the grass handle. `TerrainWind::sway_offset` gives the same sway on the CPU.

## Natural style

`terrain.set_wind {"style": "natural"}` (ADR-2446) changes how the grass moves; `classic`, the default, is exactly
the sway described above and no frame of a scene that never sets `style` changes.

- **Gusts with calm spells.** A noise field of gusts travels along the wind. Where it is low the air is calm and
  the grass stands still; where it is high the blades lean downwind and swing. The field is two octaves moving at
  different speeds, so the pattern does not repeat.
- **Slower sway for taller blades.** The sway frequency falls with blade height (a factor 0.6 to 1.25 of the class
  frequency, times 0.55).
- **No regular wave.** The phase comes from two noise octaves of position and a per-blade draw, so there is no plane
  wave crossing the field.
- **A flutter band per species.** Each grass class has its own faster flutter (2.6 to 4.6 times its sway
  frequency), weighted by `gust` and by the gust field.

`terrain.wind` reports `style`. Error: `wind_bad_style` (not `classic` or `natural`).

## Check it

1. `terrain.set_wind {"strength": 0}` and take `seeing.screenshot` twice a second apart: identical.
2. `terrain.set_wind {"strength": 2, "direction": [0, 1]}` and screenshot twice: the blades lean
   and move along +Z.

3. `terrain.set_wind {"style": "natural"}`, then screenshot a few times ten seconds apart: some frames show the
   field still, others leaning and moving; no stripes of equal spacing travel across it.

The sun shadows of grass use the rest pose, so wind never re-renders the shadow cache.
