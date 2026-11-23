# Terrain debug views

Use these views to find out why terrain looks too dark or too bright: which term is wrong (albedo, sun visibility, the
sun's angle, the sky light, occlusion), measured on the frame the game actually renders. The shared terrain pass
(`TerrainSharedDraw`) has 17 views. Two agent methods switch them on a running frame. The host inserts
`regia_terrain::TerrainDebugViewControl(draw.debug_view_handle())` with its terrain draw (the showcase host does). The
draw reads the view at its next prepare. Without that resource both methods answer `F0003`. An unknown view is `F0002`.

## `terrain.get_debug_view`

`{}` → `{ view: { id, name, channels }, views: [{ id, name, channels }] }`. `view` is the one in effect (`0` / `off` is
the lit terrain). A number the table does not name has `name: null`.

## `terrain.set_debug_view`

`{ "view": 8 }` or `{ "view": "sun_split" }` → `{ before, after }`. Names are case-insensitive. `off` restores the
lit terrain.

| id | name | channels |
|---|---|---|
| 0 | `off` | the lit terrain |
| 1 | `lod` | the lit terrain tinted by CDLOD level |
| 2 | `n_dot_l` | grey: N·L of the shading normal |
| 3 | `albedo` | rgb: the blended albedo before lighting |
| 4 | `layer_weights` | rgb: weights of the first three layers |
| 5 | `slope` | grey: slope, 90° = 1 |
| 6 | `height` | grey: normalised height |
| 7 | `sim_agreement` | green/blue agree with the physics heightfield, red/yellow do not |
| 8 | `sun_split` | R cascade shadow, G heightfield shadow × micro shadow, B N·L, each × the views' scale. R×G is the lit visibility only when one shadow is 0 or 1 |
| 9 | `indirect_split` | unscaled. Per unit albedo, irradiance / π: R sky and ground ambient, G planet-shine, B terrain bounce before probes replace it |
| 10 | `near_field` | R near-field strength, G icy (crust), B frost |
| 11 | `cascade_shadow` | grey: the cascade shadow alone |
| 12 | `cascade_occluder` | R metres the cascade depth sits in front (÷ 2), G cascade index ÷ 3, B metres behind (÷ 2) |
| 13 | `far_sun` | R heightfield sun visibility to ~2 km, G distance of the first occluder ÷ 2 km |
| 14 | `occlusion_relief` | R heightfield cavity, G baked macro AO, B irradiance-probe coverage |
| 15 | `occlusion_texture` | R layers' ORM occlusion, G screen-space AO, B 1 |
| 16 | `normal` | rgb: shading normal as 0.5 + 0.5·n |

## Reading the numbers

On the HDR target, a value of 1 prints like a white Lambert surface in full sun (`E_sun / π`, the scale `dbg_k`). The
frame's exposure is applied on top, but the HDR pre-exposure is not. Read values from a float dump, not the PNG:
`seeing.dump_resource { "label": "exposed", "format": "exr" }` after each view. Within one view, compare channels
directly. Across views, divide by `dbg_k`, which is the B channel of view 15 (always 1 · `dbg_k`):

- albedo = view 3 / `dbg_k`; N·L = view 8 B / `dbg_k`.
- View 8 R is the cascade shadow and view 8 G is the heightfield self-shadow times the micro shadow, each
  times `dbg_k`. The lit sun visibility is `min(cascade, heightfield) × micro`. That equals view 8 R × G /
  `dbg_k²` when one of those two shadows is 0 or 1. When both sit between 0 and 1, the product is smaller
  than the lit visibility.
- View 9 is stored without `dbg_k`. R is sky and ground ambient after occlusion, G is planet-shine fill, and
  B is the terrain-bounce estimate before irradiance probes replace it. The lit frame multiplies that bounce
  by `1 − probe coverage` (view 14 B). Where view 14 B is 1, skip view 9 B.
- Where the two sun shadows are not both partial and probe coverage is 0, lit radiance is about
  albedo × (`dbg_k` · R·G·B of view 8 / `dbg_k³` + view 9 R + view 9 B), up to the HDR pre-exposure. That
  comparison is not a ±5 % match on ground where both shadows are partial, or where probes cover the point.

To compare with the sky, normalise each frame by the median of its own sky pixels; that cancels each frame's auto
exposure. Pin the exposure (`render.set_exposure_settings { auto_exposure: false, exposure_ev }`) for absolute values:
`exposed / exposure_from_ev100(ev)` is cd/m².

## Worked example

Look round 2 found the showcase ground about 4× darker than the sky (ADR-1447). On open ground in shot 1 the views gave:
albedo 0.196, cascade shadow 1, N·L 0.35 (sin 20.5°), cavity, macro AO, ORM AO and SSAO all 0.99–1.0, probe coverage 1,
and sky light 0.20 of the sun. Every terrain term was as designed. The cause was upstream: the atmosphere's sun was
0.81× its own sky table value (fixed in ADR-1447), plus a bright horizon band in the sky model.
