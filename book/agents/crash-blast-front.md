# Crash blast front: the dust wall and the gust line

`regia_vfx::crash` (ADR-1497) gives a crash's blast front one clock: the front is
`343 m/s × t` from the crash point `t` seconds after the detonation. Everything that marks the
front reads that clock (`blast_front_radius`, `blast_arrival_seconds`, `blast_front_reached`,
`blast_arrival_step`):

- the camera shake (`BlastArrived`, and `regia_framework`'s shock shake, whose sound speed is the
  same constant);
- the dust wall rings, which spawn on the front's schedule;
- the gust puffs, which fire on the frame the front reaches each site;
- the grass gust;
- the blast wind (`BlastGusts`).

At shot 4's camera, 3308.9 m from the 250 t impact, the shake, the gust and the front all land
579 frames (9.65 s at 60 Hz) after the detonation.

| Part | What it is | Measured at 250 t (CPU probe) | Target |
|---|---|---|---|
| Blast front skirt (`blast_front` in `crash_dust_<ground>@<t>t`) | a thin band of kicked-up snow and dust running at the front, out to 2 km | 340 m/s, within 2 % of `343 t` beyond 150 m | `343 t ± 10 %` beyond 150 m |
| Dust wall (`front_wall_<k>`, 17 rings on full, 9 on lite) | snow and dust lifted along the front's path: each ring's puffs leave on the front's schedule, settle behind it within two frames, then rise and drift with the wind | last ring at about 440 m (full) / 424 m (lite), inside `wall_reach` 464 m; edge at most one ring ratio (1.075 full, 1.15 lite) plus a frame behind the front, never ahead | lifted along the path to 300–500 m |
| Base surge (`surge`, unchanged) | the slow dust ring behind the front | 400–700 m and 50–100 m high by 15 s | same |
| Gust line (`crash_gust_snow`, grass gust) | past the wall the front shows only as wind: grass leans away from the crash, and loose snow sheds from tree crowns and banks | 4.7 m/s for about 0.3 s at 3.3 km; grass tips bend about 0.39 of the blade height | flicks as the front passes, arriving on the shake frame |

The wind behind the front comes from public blast physics:

- the Kinney–Graham peak overpressure at the scaled distance `R / W^(1/3)`, with a surface burst
  taken as 1.8 times the charge;
- the Rankine–Hugoniot particle speed behind the shock;
- a Friedlander pulse over the positive phase, which swings slightly back in the suction phase.

`CrashScale::blast_wind(distance)` returns the overpressure, the peak wind and the positive
phase there.

## Grass and trees

- **Grass.** The VFX plugin's `publish_blast_gusts` lists the fronts. Then
  `regia_framework::blast_gust::BlastGustGrassPlugin` hands the strongest one near the camera to
  the terrain grass (`TerrainGrassHandle::set_gust`). `ShockShakePlugin` installs that plugin;
  a host without the shock shake adds it itself. Behind the front the blades lean away
  from the crash with the pulse, and their motion vectors follow the front. `terrain.grass`
  reports the gust in effect (`gust`).
- **Trees.** The tree scatter has no sway path, so trees shed snow instead: give the crash
  `gust_sites`, for example tree crowns near the camera. The 48 sites nearest the camera each
  fire a `crash_gust_snow` puff on the frame the front reaches them. The puff is a short burst
  of white powder blown along the gust, and it sinks and fades within 2–3 s.
  `regia_vfx::crash::crash_gust_sites` scatters sites on the ground around a point for a host
  without a tree list.

## Schedule it

```json
{"jsonrpc":"2.0","id":1,"method":"vfx.crash","params":{"position":[700.0,105.7,5.0],"ground":"snow","yield_t":250.0,"wind":[3.0,0.0,1.0],"impact_in":7.0,"gust_sites":[[-1700.0,350.0,-2150.0],[-1780.0,356.0,-2240.0]]}}
```

| Param | Default | Notes |
|---|---|---|
| `gust` | true | Fire the gust puffs. |
| `gust_sites` | none | Up to 256 `[x, y, z]` world points. |

The result adds:

| Field | What it is |
|---|---|
| `wall_reach`, `wall_rings` | How far the dust wall runs and how many rings it has. |
| `gust`, `gust_sites` | Whether gust puffs fire, and how many sites will. |
| `camera_distance` | The `VfxView` camera's distance from the crash. |
| `camera_arrival_in` | Seconds from now until the front reaches the camera. |
| `camera_wind` | The peak wind there. |

`vfx.crashes` adds:

- `gust_puffs` (fired so far) and `gust_sites` per crash;
- `camera_wind`, the blast wind at the camera now.

## Check it

```json
{"jsonrpc":"2.0","id":2,"method":"vfx.blast_gust","params":{"position":[-1752.0,353.2,-2203.0]}}
```

`vfx.blast_gust` returns the blast wind at a point this frame (`wind`, `speed`). The default
point is the `VfxView` camera. For every scheduled crash it also returns, in `fronts`:

| Field | What it is |
|---|---|
| `distance` | The point's distance from the crash. |
| `detonated`, `front_radius` | Whether the crash has detonated, and the front's radius now. |
| `arrival_s` | `distance / 343`. |
| `arrival_in` | Seconds until the front reaches the point. |
| `reached` | Whether the front has reached the point. |
| `peak_wind`, `overpressure_pa`, `positive_phase` | The blast wind at the point. |

To check the timing:

1. Step the clock frame by frame until `reached` turns true.
2. On that frame `vfx.crashes` shows `arrived`, and `camera.shock_shake` shows the shake firing.

`vfx.crash_profile` on `crash_dust_<ground>` reports the wall's numbers in its summary:

| Field | What it is |
|---|---|
| `wall_reach` | p90 of the outermost ring, metres. |
| `wall_lag` | Largest lag of the wall's edge behind `343 t` between 150 m and the last ring. |
| `wall_leads_front` | Must be false. |
| `wall_height` | How tall the wall stands, metres. |

Errors: `F0002` for a bad `position`, for a missing one when there is no `VfxView`, and for bad
`gust_sites` (not `[x, y, z]` points, or more than 256).

## The body's approach

The final fall (approach angle, speed, tumble and the damage-smoke trail) is the crash-final-fall
work (ADR-1500). This page only takes the body's travel direction:
`CrashBurst::contact_downrange` (`None` = straight down) tilts the contact puff downrange by half
the travel's angle from the vertical.
