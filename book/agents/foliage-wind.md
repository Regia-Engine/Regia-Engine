# Foliage wind and dynamic shadow casters

Plants that sway in the wind are drawn by a material graph whose vertex stage reads the
[material clock](material-clock.md). Two things make that look right in a running game:

- **One scene wind.** The runtime renderer writes one wind vector, metres per second on world X
  and Z, into every time-reading graph next to the clock: parameter slot 0 is
  `[time, previous time, wind x, wind z]`. Every plant bends along the same wind, and a gust
  front or a storm is one call.
- **Shadows that sway with the plant.** Sun cascades on Desktop and the Deck are *paged*: a
  shadow page is redrawn only when a caster's bounds or pose change. A swaying plant does not
  change either (the sway happens in the vertex shader), so its cached shadow would hold the pose
  it was first drawn with. Casters whose vertex stage reads the clock form the **dynamic shadow
  class**: in the nearest cascades their pages are redrawn every frame (spec 88, ADR-2028).

## The ready-made sway

`regia_render::FoliageSway` is a vertex stage you can put on any surface graph:

```rust,wrap
# let renderer: &mut regia_render::FrameRenderer = todo!();
# let my_surface_wgsl: &str = todo!();
use regia_render::FoliageSway;

let sway = FoliageSway {
    stiffness: 0.03,        // metres of bend per m/s of wind
    height_m: 2.0,          // full bend at 2 m above the object's origin
    flutter_m: 0.02,        // leaf flutter along the normal at vertex colour red = 1
    gust_period_s: 4.0,     // divides 3600 s: loops across the clock's hourly wrap
    max_wind_m_per_s: 20.0, // the culling bound grows by the offset this allows
    ..FoliageSway::default() // `style`: the opt-in natural wind (ADR-2446)
};
let graph = sway.graph(my_surface_wgsl); // a SurfaceGraph with the stage installed
renderer.set_foliage_wind(Some([6.0, 2.0])); // or `render.set_foliage_wind`
```

The stage bends every vertex above the origin downwind by `amp · h²` and lowers it by the arc's
drop, with `h` the vertex's height over `height_m` (0 at the root, 1 at the top) and `amp` the
stiffness times the wind speed times a gust between 0.4 and 1.0 whose phase comes from the
object's origin (neighbours do not move in step). It is the same in the forward, depth, shadow and
velocity passes, so the shadow and the motion vectors follow the drawn plant.
`FoliageSway::offset` is its CPU mirror (tools, tests, gameplay that needs to know where a branch
is). A material graph can read the wind directly too: slot 0 `zw` is the vector.

## Read

`render.foliage_wind` (no parameters):

```json
{}
→ { "wind_m_per_s": [6.0, 2.0], "speed_m_per_s": 6.3245554, "max_m_per_s": 40.0,
    "dynamic_casters": { "mode": "tier", "cascades": null, "local_lights": null },
    "last_frame": { "paged": true, "cascade_mask": 3, "refreshed_cascades": 2,
                    "local_lights": true, "animated_casters": 148, "skinned_casters": 20,
                    "shared_boxes": 9,
                    "dynamic_pages": 11, "dynamic_atlas_tiles": 1 } }
```

- `wind_m_per_s` is `null` until a wind is set; then graphs keep what their material carries in
  slot 0 `zw` (labs and goldens).
- `last_frame` is what the dynamic class did on the last frame: `paged` false means the frame's
  sun shadows were not cached (both Mobile tiers), so every caster was drawn anyway;
  `cascade_mask` (bit `i` = cascade `i`) and `refreshed_cascades` say where animated casters were
  redrawn; `animated_casters` counts the shadow casters in the class (a time-reading vertex
  stage, or a bounded skinned pose); `skinned_casters` how many of those are skinned;
  `dynamic_pages` the sun pages and `dynamic_atlas_tiles` the local-light tiles they made the frame
  redraw. `shared_boxes` counts the boxes of swaying shared-draw content (the terrain grass,
  `SharedPassDraw::dynamic_shadow_boxes`) that joined the class.

## Skinned casters

A vertex-shader-skinned mesh (a character, a walker, a crowd unit) is in the class too (ADR-5418).
The page caches bound it by its joint-palette box ([draw integrity](draw-integrity.md), ADR-248),
the same box the colour pass culls it with, and page it like a rigid mesh:

- A pose that moves the box (a step, a reach, a turn of the whole unit) redraws the pages the
  unit covered and covers, in every cascade, with the class in any mode.
- A pose that changes inside the box (an arm swinging in front of the chest, a turret turning
  inside its hull's box) moves nothing the cache can see. On the frames a unit's palette changes,
  the class redraws its pages in the refreshed cascades, as it does for a swaying plant. With the
  class `off`, or beyond the refreshed cascades, that shadow holds the pose it was drawn with
  until the box or the unit's model matrix moves.
- A held pose (the same palette as last frame: a paused or frozen unit, or the in-between frames
  of an animation updated at half rate) refreshes nothing, even with the class `on`: its pages
  already hold that pose.
- A skinned mesh without a palette bound (bounds switched off, object key 0, weights the bound
  does not cover) and a compute-skinned unit ([skinning](skinning.md)) have no bound: every
  frame with one of them in view redraws whole cascades. That is right whatever the class says,
  but it gives up the page cache; `skinned.unbounded` in `render.draw_integrity` finds them.

`skinned_casters` counts the bounded skinned casters in the class on the last frame (those whose
palette changed). Skinned instancing
([draw counters](skinned-instancing.md)) does not change their shadows.

## Grass

The terrain grass field joins the class on its own. Once the grass clock runs
(`TerrainGrassHandle::set_time`), its shadow in the refreshed cascades it draws into is the
swayed blade, and its shadow patches join the class as boxes (one per 16 m cell): `shared_boxes`
here, `shadow_sway_cascades` and `dynamic_boxes` in `terrain.grass`. The boxes refresh only the
cascades the grass draws into (one on the High grass tier, two on Ultra), never a local-light
tile: terrain and grass do not cast into the atlas. A still clock keeps the
rest-pose shadows. With a scene wind set, the blades sway along it, and their sway grows with
the speed (×1 at 5 m/s, up to ×2; calm air stops them). Without one, the blades follow
`terrain.set_wind` ([terrain-wind](terrain-wind.md)). With both, the scene wind replaces that
wind's direction and multiplies its strength; its frequency and gust still apply, and strength 0
stays still. Meshes read only the scene wind. With the class `off`, grass shadows hold the rest
pose.

## Set

`render.set_foliage_wind`:

```json
{ "wind_m_per_s": [4.0, 1.5] }
→ { "before": { "wind_m_per_s": null, ... }, "after": { "wind_m_per_s": [4.0, 1.5], ... } }
```

A vector longer than `max_m_per_s` (40 m/s) is shortened to it; `null` stops writing the slot.
When the app has a `SceneWind` (the weather's wind), the call sets it too (`Manual` with the
value, `Off` for `null`), so the weather does not overwrite it next frame. A refused call
(`F0002`, `F0003`) changes neither.

`render.set_dynamic_caster_shadows`:

```json
{ "mode": "on", "cascades": 2, "local_lights": true }
→ { "before": { "mode": "tier", ... }, "after": { "mode": "on", "cascades": 2, "local_lights": true } }
```

| `mode` | What happens |
|---|---|
| `tier` (default) | Desktop High, Ultra and Cinematic: cascades 0 and 1 and the local atlas. Desktop Low and the Deck: cascade 0. Mobile: nothing to do (unpaged). |
| `off` | Animated casters cache like static ones: their shadows hold one pose. |
| `on` | Their pages in the first `cascades` (1–4, default 2) sun cascades, and with `local_lights` (default true) their atlas tiles, are redrawn every frame. |

Beyond the refreshed cascades an animated caster's pages are cached: they hold the pose the page
was last drawn with. At those distances a sway is a fraction of a shadow texel.

Errors: `F0002` for bad parameters (the message names the field), `F0003` without a
`RuntimeRenderer` (the hint says what to add).

## Sway on any material: trees, imports and scatter

Most plants arrive as plain textured materials (a glTF import, a scatter mesh), not as graphs.
`FoliageSway::adopt` gives such a material the sway (spec 88 § 2.5, ADR-2304):

```rust,wrap
# let tree_material: regia_render::LabMaterial = todo!();
use regia_render::FoliageSway;

let sway = FoliageSway { height_m: 9.0, stiffness: 0.035, ..FoliageSway::default() };
// A plain material becomes the standard surface written as a graph (FOLIAGE_SURFACE) with the
// sway's vertex stage; its four maps move into graph slots 0..3. Base colour, metallic,
// roughness, normal scale, occlusion strength and emissive are still read from the material,
// so a tint applied later (the scatter field's class and variant tint) still applies.
let swaying = sway.adopt(&tree_material)?;
assert_eq!(FoliageSway::of_material(&swaying), Some(sway));
let at_rest = FoliageSway::still(&swaying); // same surface, no vertex stage
```

- A graph material without a vertex stage keeps its surface and gains the stage. A graph that
  has its own vertex stage is refused (`sway_vertex_stage_taken`), and so is one whose code reads
  parameter slot 14 or 15, where the sway keeps its values (`sway_slots_taken`). Use at most 13
  parameters on a graph you want to sway.
- Adopting a swaying material again keeps its code (same pipelines) and takes the new values.
- Nothing changes for a material nobody adopts: sway is opt-in per material.
- In a material graph file, the `Wind` node (`.matgraph` v3) reads the same wind
  ([Material graphs](material-graph.md#wind-v3)).

**Scatter.** Give a `ScatterMeshAsset`'s materials (and its parts' materials) a sway and every
seeded instance of that mesh sways. A painted instance follows the wind too, unless it was painted
with the brush's wind off (`ScatterBrush::wind = false`, or `terrain.scatter_stroke { "wind":
false }`): those carry `PAINTED_FLAG_STILL` in their saved flags and draw the same surface at rest
(a dead tree in a swaying wood). Older `.scatterpaint.bin` files hold zero flags, so their
instances follow the wind. The still copy is made once per mesh and tint variant.

**The sample.** `cargo run -p terrain_lab -- --foliage-wind <dir>` renders a wood of swaying
pines, an adopted "imported" tree and a still painted patch in gusting wind, and prints the
class's report per shot; `--bench --size 2560x1440` prints the GPU shadow time with the class on
and off. The film host takes the same adoption for a scene's glTF trees with
`showcase <scene_dir> --foliage-wind` ([Showcase](showcase.md)).

### Per-material sway on the protocol

`material.sway` reads a mesh entity's sway:

```json
{ "entity": 4294967301 }
→ { "entity": 4294967301, "sway": null, "graph": false, "vertex_stage": false, "animated": false }
```

`material.set_sway` gives it one, changes it (missing fields keep the current sway's, else the
default shrub's) or removes it with `null` (the surface stays a graph):

```json
{ "entity": 4294967301, "sway": { "height_m": 9.0, "stiffness": 0.035 } }
→ { "entity": 4294967301,
    "before": { "sway": null, "graph": false, "vertex_stage": false, "animated": false },
    "after": { "sway": { "stiffness": 0.035, "height_m": 9.0, "flutter_m": 0.02,
                         "gust_period_s": 4.0, "max_wind_m_per_s": 20.0, "max_offset_m": 0.72 },
               "graph": true, "vertex_stage": true, "animated": true } }
```

The entity gets its own copy of the material, so entities that shared the old one keep theirs.
`animated` is the dynamic shadow class's test. Field ranges: `stiffness` 0..1, `height_m` above 0
up to 200, `flutter_m` 0..1, `gust_period_s` 0.1..3600, `max_wind_m_per_s` 0..40. Errors: `F0002`
for a bad or unknown field, `F0003` for an entity without a mesh, `sway_vertex_stage_taken` and
`sway_slots_taken` as above.

## On the visibility path

An adopted plant takes the cluster path with the same motion: see [Swaying plants](visbuffer.md#swaying-plants).

## What it costs

A refreshed page redraws every caster and the terrain under it, so the cost grows with how much
of the near cascades the swaying plants cover, not with how many plants there are. A field of
grass cards in front of the camera can make most of cascade 0 dynamic. Watch `dynamic_pages` and
`render.stats` while tuning, and turn the class `off` for a view where the shadows of plants do not
matter. Mobile pays nothing: its shadows are redrawn every frame already.

## Checks for an agent

- Set a wind, step a few frames (`loop.set`), and read `render.foliage_wind`: on Desktop High with
  plants in view `animated_casters` > 0 and `dynamic_pages` > 0 every frame.
- With animated characters in view, `skinned_casters` > 0. If it is 0 while
  `render.draw_integrity` shows `skinned.unbounded` > 0, the units have no palette bound and every
  frame redraws whole cascades for them.
- With `mode: "off"` the same frames show `dynamic_pages` 0, and a screenshot of a plant's
  shadow stops moving while the plant still sways.
- `wind_m_per_s: [0, 0]` stops every `FoliageSway` plant (no offset in calm air).
- `material.set_sway { entity, sway: {} }` on a plant, then `render.foliage_wind`: `animated_casters`
  grows by one; `material.set_sway { entity, sway: null }` takes it back.

## Natural sway style

`FoliageSway { style: WindStyle::Natural, .. }` (or `material.set_sway {"entity": id, "sway": {"style": "natural"}}`,
ADR-2446) replaces the periodic gust by the same travelling gust field the grass uses
([Terrain wind](terrain-wind.md#natural-style)): calm spells where the plant stands still, a sway that is slower for
a taller plant (`height_m`), a phase that varies across space, and a flutter band picked per plant by its stiffness
and height. `classic` is the default and the stage's classic branch is unchanged. `material.sway` reports `style`;
a wrong name answers F0002. The material clock wraps every hour (ADR-1238), and the natural field is not
periodic, so it jumps once at the wrap.
