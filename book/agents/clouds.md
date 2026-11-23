# Volumetric clouds

Desktop High and Ultra raymarch one cloud layer on Earth and Mars maps (an airless preset has
none). The method follows Schneider's public talks (SIGGRAPH 2015, and "Nubis", SIGGRAPH 2017):

- A **weather map**: R is coverage, G is cloud type. `CloudPattern::Coverage` thresholds a
  tiling cellular noise at the coverage percentile, so exactly that fraction of the map is at
  least half covered, with soft edges.
- A **shape** volume (32³ Perlin–Worley plus Worley FBM) eroded by a smaller **detail** volume.
  Both tile and are built once per process.
- A **height profile** per type: stratus (thin, low), cumulus, towering cumulus.
- **Light**: a six-step march toward the sun, Beer's law, the Beer–Powder edge term, a dual-lobe
  Henyey–Greenstein phase and three multiple-scattering octaves; ambient from the sky dome.
- **Cone light** (batch v020, `FrameQuality::cloud_cone_light`, on for Desktop High and Ultra, the
  presets with clouds): the light march takes five taps spread inside a ~10° cone toward the
  sun over half the layer thickness (the first three eroded by the detail noise, as in the
  straight march), rotated per sample, plus one long tap that catches shadow
  from cloud farther toward the sun (Schneider 2015). The two multiple-scattering octaves are
  weighted by an in-scatter probability (Schneider & Vos 2017) that is low in thin wisps and
  near the base and high in dense cores, so edges and undersides read darker and cores glow.
  The detail erosion is displaced by a tileable 2D curl-noise field, strongest at the bases,
  for swirled wisps. Off keeps the v0.1 straight march, octaves and detail.
- **Sun at altitude** (batch v020, always): the layer is lit by the sun as transmitted to its base
  and top (the atmosphere's transmittance table), not to the camera. At sunset the tops take
  a warmer, brighter sun than the bases, and the layer stays lit for a while after the sun has
  set for a camera on the ground (the horizon dips ~1.8° at 3 km).
- **Aerial perspective** (batch v020, with the physical sky): the air between the camera and the
  cloud comes from the same aerial LUT the terrain uses, at the cloud's scatter distance, so
  distant clouds take the haze colour of the sky around them. Past the table's 32 km they also
  fade out over the old 45 km scale.
- **Cost**: the march runs at half resolution (`resolution_divisor`) with jittered start
  positions. A big step is four times the small one; a hit steps back and walks small,
  and eight empty small steps return to the big step. The small step grows with
  distance. Inside the layer it starts at 20 m and grows so the 67.5 km fade span
  finishes in the step budget. The result is upsampled by nearest scene distance
  over the whole frame (tied distances blend bilinearly), so a cloud in front of a
  mountain draws over it and a closer peak stays in front. View rays use the unjittered
  camera; depth is still unprojected with the jittered matrix. Clouds fade into the sky
  with distance. Froxel fog still integrates through the same band.
- **Temporal** (batch v020, `FrameQuality::cloud_reprojection`, on for every preset): each frame
  marches one texel of every 4×4 block of the half-resolution target, in Bayer order, so a
  texel is refreshed every 16 frames. The other 15 are reprojected from the previous result:
  the texel's world point at its scatter distance, minus the wind drift since that frame, is
  projected with the previous unjittered view-projection and fetched with a Catmull-Rom
  filter, then clamped to the min/max of the 3×3 fresh texels around it. Off screen, or where
  the scene distance no longer matches the history (a disocclusion), the texel is filled from
  the fresh texels on its surface and keeps blending in new fills until it is marched. The
  clamp only applies where the block's fresh sample shows the history is stale, so a still
  sky keeps detail finer than a block. The marched texel blends half and half with its
  history when still, and replaces it when the texel moved or was resampled (a fast pan
  softens clouds by about a low-resolution texel until then). RTX 5080, 1440p, coverage 0.5:
  High 0.98 → 0.35 ms, Ultra 1.37 → 0.45 ms. A camera cut (`FrameRenderer::camera_cut`, or a detected one), the first
  frame and a resize march every texel. Desktop Ultra spends the time saved on 96 steps
  (`cloud_step_percent` 150). Texture views keep clouds off and never touch the history.

The composite leaves a surface alone when it is nearer than the point where its view ray enters the
layer (ADR-5792): no cloud can be in front of it, whatever the low-resolution texels around it
hold. Before that rule, trees and rooftops against a cloudy sky carried texel-sized specks of
cloud radiance, which the history fetch had spread from sky texels over the surface texels beside
them.

The same weather column, projected along the sun, darkens the terrain (cloud shadows). With the
cloud shadow map on (below), the 3D clouds themselves shadow the sun instead.

## Cloud shadow map, god rays and overcast sky light (batch B, ADR-320, ADR-321)

Three switches on `CloudSettings`, **all off by default** (frames with them off are unchanged):

- `shadow_map`: a top-down **cloud transmittance map** darkens the **sun term only** of meshes
  (`lab_forward.wgsl`) and of the terrain (the shared colour pass); sky and ambient light are
  untouched, so a surface under a cloud keeps its sky light. The old screen-space pass that
  multiplied the whole pixel with a 0.35 floor is not drawn while the map is on.
- `fog_coupling`: the froxel fog's sun in-scatter reads the same map, so sunlit air shows god
  rays through gaps in the layer. It builds the map even with `shadow_map` off.
- `overcast_ibl`: the sky light (the atmosphere IBL, the terrain's hemisphere sky and the fog's
  sky ambient) greys and dims with the layer's mean coverage and column depth, by up to 60 %
  under a full, thick overcast. Earth skies only: airless and Mars skies never change.

The map is `R16Float`, `shadow_map_size` texels a side (512 by default) over
`shadow_map_extent_m` (20 km, about 39 m a texel) around the camera. Each texel holds the sun's
transmittance along the line through where it crosses the layer base: 16 samples of the same 3D
density the clouds pass draws. It is world-anchored (a texel keeps its cell while the camera
moves) and snapped to whole texels, so shadows do not swim. Each frame marches one sixteenth of
it (a band of rows) plus the strips a camera move brings in; a camera cut, a weather cut, a new
sun, a changed layer shape (altitudes, noise scales, weather tile, resolution, seed or pattern) or
a wind-drift jump of a texel marches it whole. Coverage, type, density and wind changes smaller
than a weather cut are followed by the band refresh within 16 frames (ADR-429). It fades to unshadowed over the outer 15 % of the
window, and nothing reads it with the sun under about 0.6°. Texture views keep clouds off.

```rust
use regia::prelude::*;
use regia::render::CloudSettings;

fn configure(app: &mut App) {
    app.insert_resource(CloudSettings {
        shadow_map: true,
        fog_coupling: true,
        overcast_ibl: true,
        ..CloudSettings::coverage_layer(0.45)
    });
}
```

### The air under the layer (ADR-5944)

With `shadow_map` on, the air between the eye and a surface is shadowed with the surface. No extra
switch: it follows `shadow_map`, and a frame with `shadow_map` off draws its air as before, byte
for byte.

- **Aerial perspective** (the atmosphere tables, Desktop High and Ultra). The tables are built for
  a clear sky. They now keep the sun's single scattering apart (`AtmosphereLuts::aerial_sun`), and
  the composite takes that share out of the stretches of each view ray that lie in the layer's
  shadow: the cloud transmittance map is read at 8, 4, then 2 points per 2 km of the ray (nearest
  first), with the lookup the surfaces use, so one transmittance dims the sun on a surface and in
  the air in front of it. Under a clear part of the sky nothing is removed.
- **The rest of the in-scatter** is multiple scattering: the sky's light and the lit ground's
  bounce, scattered again (a quarter to more than half of the in-scatter, by the tables). It
  follows the light on the ground under the layer: where the ray's air is shadowed it loses the
  sun's share of the clear ground light, and with `overcast_ibl` on the sky's share grays and dims
  by the same rule as the sky light on surfaces. Under one cover the air and the ground behind it
  then fall by the same factor.
- **Analytic haze** (`FogSettings` aerial mode, on meshes, terrain, impostors and grass): the sun
  glow, and the horizon sky term down to a floor of 0.12 (the sky's share of the clear ground
  light), fall with the map's mean at 4 points of the view ray. Today this rule shows only
  through rain and snow (below): a frame that draws the atmosphere tables switches the forward
  fog off, so the fog's own haze and the cloud map do not meet.

Why it matters: before, the haze kept the full sun over ground that had lost it. From eye height
that hardly shows; from high up, under a closed cover, the exposure rises for the darker ground and
the whole view washed out to blue-gray (ADR-5792 measured it on the forest demo). Now a cover
leaves the contrast of a high view where a clear sky has it.

The planet's own surface beyond the terrain (the land a sky pixel shows where its ray meets the
planet) is under the layer too: it loses the sun's share of its light by the map's value where the
ray meets it, and the air in front of it is shadowed like the air in front of the terrain.

The image light follows as well when `overcast_ibl` is on too: below the horizon it shows the
ground, and that ground is then the shadowed one (the sun's share of its light times the layer's
mean transmittance, the sky's share under the overcast rule). A wall, a tree or a hillside under
the cover no longer takes the bounce of a sunlit ground. The terrain's hemisphere ambient (the
bounce its slopes, its grass and its tree impostors take) uses the same ground. With `shadow_map`
alone the image light is unchanged (a texture view lit by it renders as with the map off), so steep
faces keep that bounce: switch the overcast sky light on with the map.

The GI probes (the Depth lighting option, the default of Desktop High and Ultra) are under the
layer too, with `shadow_map` alone. A probe ray's hit is lit by the sun through the tracer's own
shadow rays, which meet the terrain and the meshes and not the clouds; with the map on, the hits
keep the share of the sun the layer leaves on the ground on average (one minus its mean cover).
Before, the probes kept a sunlit ground's bounce under any cover, and what they light (a tree, a
wall, a hillside) stood pale against the shadowed ground: on High and Ultra, not on the tiers
without probes. To check it, set `render.set_gi_mode {"mode": "depth"}` under a closed cover with
the three cloud switches on: a wall with its back to the sun should be no brighter than the
ground at its foot.

Without the probes (the Balanced option) the terrain estimates the bounce a sloped face takes from
the ground before it; that ground's sunlit share falls with the cloud map too, so a cliff under a
cover no longer outshines the ground it stands on.

Limits: the map fades to lit beyond about 8.5 km of the camera (its window), so far air and far
land keep their sun; the image light's ground and the probes' sun take the layer's mean cover, not
the cover over each place (in the shadow of one cloud of a broken layer the probes keep the mean
share of the sunlit bounce); a baked GI volume and the terrain's bounce grid for meshes (the
Balanced option) keep the frame's sun, and so do the effect layer's particles; the cloud layer's
own aerial perspective is not shadowed; the froxel fog needs `fog_coupling`, as before.

Rain and snow are under the layer too (frames without the table air). Each surface mixes toward
the precipitation's in-scatter over its distance; with `shadow_map` on that in-scatter takes the
map's mean along the view ray, by the haze's rule: in the fog's sky in-scatter mode the sun glow
goes and the sky term falls to its 0.12 floor, and in the flat mode the fog's color falls to 0.12
of its clear-day radiance under a closed cover (meshes, terrain and grass). The flat color on
meshes is display-referred at the scene's exposure setting, so this assumes the final exposure
adapts on its own (the auto exposure); a host that sets the exposure by hand for the cover gets a
weaker veil on meshes. The floor is a
constant: with `overcast_ibl` on as well the ground dims further than the veil, so the far ground
through rain stays paler than on a clear day. The table air's veil (`aerial_from_tables`,
ADR-3294) keeps its own deck rule.

`render.get_clouds` reports it under `shadow_map.air`: `shadowed` (the air reads the map: the
`shadow_map` switch), `steps` (`[8, 4, 2]`), `haze_steps` (4) and `overcast` (true when
`overcast_ibl` also dims the sky's share of the light the air's multiple scattering follows). In Rust,
`FrameRenderer::cloud_air_frames()` counts the frames whose aerial perspective read the map.

```json
{ "method": "render.set_clouds", "params": { "shadow_map": true, "overcast_ibl": true } }
{ "method": "render.get_clouds" }
```

The second call answers with `"shadow_map": { "enabled": true, ..., "air": { "shadowed": true,
"steps": [8, 4, 2], "haze_steps": 4, "overcast": true } }`.

Proposed preset defaults (ADR-320; not on yet): Desktop High and Ultra on, every other preset
off.

### Agent methods

- `render.get_clouds` → `{ source: "resource" | "level" | "default", coverage, cloud_type,
  density, altitude_bottom, altitude_top, wind_m_per_s, time_s, shadow_map: { enabled, built,
  size, extent_m, texel_m, refresh_fraction, steps, air: { shadowed, steps, haze_steps, overcast }
  }, fog_coupling, overcast_ibl }`. `built` is
  true when any switch that needs the map is on; `refresh_fraction` is 1/16; `air` is the air
  under the layer (ADR-5944, above).
- `render.set_clouds { coverage?, cloud_type?, density?, shadow_map?, shadow_map_size?,
  shadow_map_extent_m?, fog_coupling?, overcast_ibl? }` → `{ before, after }`. Changes only the
  fields given and writes the app's `CloudSettings` resource (starting from the level's clouds
  when there is no resource), which then wins over the level. An airless planet still forces the
  frame's coverage to 0 and skips the pass, including a cirrus sheet on the resource. `get_clouds`
  returns the stored resource and does not apply that zero. `shadow_map_size` is clamped to
  64..2048 and `shadow_map_extent_m` to 1000..200000. A wrong type (a string for a number, a
  number for a switch) answers error `F0002` (invalid parameters) and changes nothing. A number that
  is not finite, or that does not fit in `f32`, is the same `F0002` and changes nothing (it is not
  stored as a non-finite look override).
- `render.set_clouds` also takes the genus and look fields, documented on their pages: `genus`,
  `diffusion`, `erosion`, `breakup`, `breakup_scale_m`, `cirrus_coverage`, `cirrus_altitude_m`,
  `cirrus_optical_depth`, `cirrus_scale_m` and `cirrus_stretch` ([cloud types](cloud-types.md));
  `cirrus_fiber`, `cirrus_fiber_scale_m`, `cirrus_shear_deg`, `cirrus_shear`, `cirrus_breakup` and
  `cirrus_hook` ([fibrous cirrus](cirrus-fibrous.md)); `cirrus_crisp` ([crisp cirrus](cirrus-crisp.md));
  and `clear_look_override` (below). Values are clamped to each field's range.
- **Look override** (ADR-1492). When a `Weather` drives the layer (a showcase scene always has one),
  the weather step rewrites the genus layer's look every fixed step. The look fields given to
  `render.set_clouds` (`diffusion`, `erosion`, `breakup`, `breakup_scale_m`, `cirrus_*`) are therefore
  also kept in the weather as an override applied over the genus look every step, so
  `render.get_clouds` right after returns what was set and it stays. `render.get_clouds` and
  `set_clouds` report `look_override: { active, fields }` (only the pinned fields). Later calls merge
  into the pins; `clear_look_override: true` drops them all (given together with look fields, the
  fields are pinned afresh) and the genus look returns on the next step. A scene can pin the same
  values with `cloud_look` in `sky.ron` ([showcase](showcase.md)). Without a `Weather` nothing
  overwrites the resource and `look_override` is inactive. `genus`, `coverage`, `cloud_type` and
  `density` are still the weather's to set (`render.set_weather`).
- Debug view `cloud_shadow_map` (`DebugViewMode::CloudShadowMap` on the `DebugView` resource, or
  the editor viewport's `debug_view: "cloud_shadow_map"`): the frame is replaced by the map, grey
  by the sun's transmittance at every surface (white lit, black shadowed) and, on the sky, where
  the view ray meets the layer base. It builds the map even with the switches off.

```json
{ "method": "render.set_clouds", "params": { "shadow_map": true, "fog_coupling": true } }
{ "method": "render.get_clouds" }
```

`CloudSettings` is a resource; coverage zero skips the pass. Defaults are a cumulus layer from
1.5 km to 4 km. A map can instead set `cloud_coverage` / `cloud_type` in
`LevelSettings.atmosphere`; an explicit `CloudSettings` resource wins.

```rust
use regia::prelude::*;
use regia::render::CloudSettings;

fn configure(app: &mut App) {
    app.insert_resource(CloudSettings::coverage_layer(0.45));
}
```

Wind is `wind_m_per_s` times `time_s`, plus `drift_offset_m`; it moves the weather and the noise.
Nothing advances `time_s` on its own: a game does, or the [`Weather`](weather.md) resource does
(it also integrates the drift, so a wind change does not jump the clouds). The map is re-packed
only when coverage, type, tile size, resolution, seed or pattern change, and a coverage or type
change re-thresholds a cached noise field into the same texture (ADR-429).

## Checks

- `cloud_coverage_matches_the_setting_within_ten_percent`: a top-down march of the full 3D
  density covers 25 %, 50 % and 75 % of the tile within ±10 % of the setting.
- `clouds_are_clear_at_zero_coverage_and_shaped_by_density`: coverage 0 leaves the sky
  byte-identical; moving the cloud field (wind) changes the sky, so cloud brightness comes from
  the density, not from the view direction; local contrast is far above the clear sky's.
- `clouds_horizon_shows_structure_along_the_shell`, `clouds_inside_the_layer_are_lit`,
  `clouds_above_the_layer_leave_gaps`, `clouds_occlude_a_far_peak_and_leave_a_near_one`:
  horizon curvature, a lit in-layer view, gaps when looking down, and occlusion of a far
  peak with a near peak left in front.
- `shell_matches_f64_on_a_planet_sphere`: the curved shell against the same expression in f64.
- `cloud_temporal.rs` (regia_test_utils): held-camera flicker at most half the full
  march's, converging to it, with nothing allocated per steady frame; a 45°/s pan at 120 m/s
  and a 40 m/s wind (60 fps) leave the clouds undisplaced, with an error a small part of the
  motion's own image change, and the pan settles back within 16 held frames; the frame
  after a camera cut has SSIM ≥ 0.95 against a full march; a texture
  view renders the same with and without clouds and leaves the main view's clouds unchanged.
  `cloud_temporal_cost_at_1440p` (ignored) times the pass continuously, full vs reprojected,
  64 and 96 steps, cone light on and off.
- `clouds_at_sunset_are_warm_and_lit` (golden `clouds_sunset`, the sun 2° up, found by
  bisection on the solar model, looking away from the sun): at least an eighth of the cloud
  pixels are warm (R > 1.15 B), and those sunlit faces are brighter than the shaded rest.
  `clouds_stay_lit_after_the_camera_sunset`: with the sun just below the camera's table horizon
  (found by bisection on the camera's table sun, ~−1.7° at 3 m), the camera has no sun, the
  layer top still has some, and far cloud rims toward the sun are redder than the clear sky
  (a smoke check: the soft table horizon leaves only the upper layer lit). `cone_light_changes_the_clouds_but_keeps_the_layer`: cone
  on vs off differ but keep the layer (SSIM > 0.6). The ignored
  `cloud_lighting_lookdev_dump` writes 640×360 noon, sunset and dusk views, cone on and off.
- `a_raised_layer_keeps_the_sun_after_the_camera_sunset` (`transmitted_sun_at`) and
  `curl_field_tiles_and_is_divergence_free` (unit tests).
- Cloud shadow map (ADR-320, unit tests in `regia_render::clouds`):
  `cloud_shadow_march_matches_an_f64_reference` (the f32 map march equals the same 16-step march
  in f64 within 5e-3 of transmittance near the origin and 75 km out, and a 4096-step f64
  integral within 0.02), `cloud_shadow_window_snaps_to_texels` (sub-texel camera moves keep the
  window and a world point's texel; a whole-texel move shifts the window one cell and the point
  keeps its texel), `cloud_shadow_refresh_covers_the_map_in_16_frames` (16 frames march every
  texel exactly once, counted; moved windows march the strips they bring in),
  `cloud_shadow_lookup_follows_the_sun_line` and `cloud_shadow_switches_are_off_by_default`.
- Overcast (ADR-321): `overcast_factor_grows_with_coverage`,
  `overcast_applies_only_to_earth_with_the_switch` (Mars, airless and switch-off measure 0),
  `overcast_zero_leaves_the_ibl_unchanged` (factor 0 is the clear bake bit for bit).
- Shaders: `cloud_shadow_lookup_composes_and_is_gated` (the lookup composes and validates; the
  forward and fog shaders read the map only under its switch; all composed variants validate),
  `terrain_cloud_lookup_matches_the_engine` (the terrain splices the same lookup text).
- Frames (`cloud_shadow.rs`, regia_test_utils): `switches_off_leave_the_frame_unchanged`,
  `shadow_map_shadows_the_sun_term_only` (shadowed ground darker, not black),
  `fog_coupling_shadows_the_sunlit_air` (the fog only loses light),
  `overcast_ibl_dims_earth_and_leaves_mars_and_airless`, `debug_view_shows_the_map`; ignored
  bench `cloud_shadow_cost_at_1440p` (map in `shadows_ms`, coupling in `fog_ms`).
- The air under the layer (ADR-5944). Frames (`cloud_shadow_haze.rs`, regia_test_utils):
  `the_air_under_the_cloudy_half_loses_its_sun` (over a black ground the frame is the air and
  little else: the half under a split sky's layer loses its sun, on the side where the ground is
  shadowed, and the clear half keeps what it has with no layer), `a_high_view_keeps_its_contrast_under_a_closed_cover` (dark and
  bright ground patches from 1.2 km up: under a closed cover, exposed to the clear frame's level,
  they keep at least 0.85 of the clear sky's contrast, with the map alone and with the overcast
  sky light), `the_cover_shadows_all_of_the_ground_in_view`,
  `the_planets_surface_beyond_the_terrain_is_shadowed_too`,
  `a_wall_under_the_cover_takes_no_sunlit_grounds_bounce`,
  `rain_under_the_cover_scatters_the_covers_light` (forward surfaces through rain in the fog's sky
  in-scatter mode keep at least 0.85 of the clear sky's contrast under the map),
  `rain_in_the_flat_fog_mode_under_the_cover_keeps_the_contrast` (the same in the flat mode, the
  final exposure adapting away from the scene's setting),
  `the_probes_under_the_cover_gather_no_sunlit_bounce` (a wall with its back to the sun on open
  ground under a closed cover: with the GI probes it reads at most 1.6 times what the image light
  alone gives it, and under a clear sky the probes' wall is more than twice as bright), and on the
  terrain
  `rain_under_a_closed_cover_scatters_the_covers_light` (`terrain_surface_weather.rs`,
  regia_terrain: exposed for the near ground, the far ground through rain is no brighter under the
  cover than under a clear sky) and `a_cliff_under_a_closed_cover_takes_no_sunlit_grounds_bounce`
  (the same file: a cliff's face is no brighter than the flat ground before it),
  `the_air_reads_the_map_only_with_the_switch_on` (the renderer's count of such frames: none with
  the switch off, with the fog's map or the overcast sky light alone, with no layer, or on an
  airless world). Unit tests:
  `the_sun_table_is_a_share_of_the_aerial_table`,
  `a_clear_sky_keeps_the_table_and_a_closed_one_takes_the_suns_share`,
  `a_shadowed_stretch_loses_its_own_share_of_the_sun`,
  `under_a_cover_the_air_follows_the_ground_light`,
  `the_haze_floor_is_within_the_tables_sky_share_of_the_ground_light` (`regia_render::atmosphere`),
  `cloud_air_span_is_where_the_lookup_can_shadow`, `cloud_shadow_on_ray_is_the_mean_of_the_lookup`
  (`regia_render::clouds`), `the_image_lights_ground_follows_the_cover_when_the_map_shadows_it`,
  `the_ground_cover_needs_the_map_and_the_overcast_sky_light`,
  `the_hemisphere_ambients_ground_follows_the_cover_with_the_image_lights`,
  `the_probes_sun_follows_the_layers_cover_under_the_map` (`regia_render::outdoor`) and `the_air_reads_the_cloud_map_only_under_its_switch` (shaders).
