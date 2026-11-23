# Host look checklist

The look-dev harness renders DO27's content the way the parity sheets show it
(`docs/evidence/parity/2026-10-07/ferrite-do27-sheet.png`, made before the rename). This page lists every setting a host
(a game shell driving `FrameRenderer` directly) must make to get the same frame. It was written
from the harness's code: `samples/lookdev/src/look_r2.rs` (`render_shot`, `do27_level`), its
content loader `samples/lookdev/src/do27.rs` and the chase bench `samples/lookdev/src/chase.rs`
(`do27_chase`).

**Check a running host in one call.** `FrameRenderer::host_checklist_report(&frame_scene)`
returns a `HostChecklist`. Hosts that load a level call
`host_checklist_report_for_level(&frame_scene, &level_settings)`: it also checks the exposure band,
the sun strength and the sky body against the level. Print the report (`println!("{report}")`) once
per session after the first frame. Frame-breaking items print first as `ERROR` (`report.errors()`),
for example MSAA with terrain. Every other difference from the harness prints as `DIFF`.
`FrameRenderer::frame_problems()` (agent: `render.problems`) says what broke the last frame drawn.
It also carries the device's **capability fallbacks** (`fallback_*` codes, one per optional feature
the adapter lacks — no timestamps, no BC decompression, RGBA16F HDR, CPU indirect counts, …): those
are informational (the frame is correct, a named fast path is off) and appear in the checklist as
the `device capability fallbacks` item, never as `ERROR`. The harness prints its
own report with `LOOK_R2_CHECKLIST=1` (`look_r2` and `do27_chase`), so a shell can diff the two
texts.

## 1. Profile and frame quality

`RenderSettings::default()` (Desktop High on a desktop; on a Steam Deck it is `Deck`, so use
`RenderSettings::from_profile(RenderProfilePreset::DesktopHigh)` there), then `settings.quality()`
(`FrameQuality::from_profile`). Do not edit the result. The report checks each field:

| Field | Value |
|---|---|
| `msaa_samples` | `1` (anti-aliasing is TAA in post) |
| `hdr_target` | `Rgba16f` |
| `render_scale_permille` | `1000` (native) |
| `taau` | `true` |
| `physical_atmosphere`, `volumetric_fog`, `volumetric_clouds` | `true` |
| `paged_sun_shadows` | `true`; `csm_cascade_count` `4`; `csm_resolution` `2048`; `shadow_filter` `PcssLite` |
| `stable_sun_cascades` | `true` (cascades on a fixed texel lattice, so sun shadows do not shimmer; see [shadow stability](../agents/shadow-stability.md)) |
| `ssao` | `GtaoHalfRes` |
| `gi` / `screen_space_gi` / `ssr` / `reflection_probes` | `Probes` / `true` / `true` / `true` |
| `parallax_occlusion`, `gpu_driven` | `true` |
| `terrain_budget` | `TerrainBudget::OFF` (the default look) |

## 2. HDR, post and exposure

- `let mut post = settings.resolve_post();` then `post.auto_exposure = true;`.
- Apply the map's look with `regia_render::apply_level_look(&mut post, &level.look)`. That sets
  the bias, the EV100 clamp band, the meter, contrast, saturation and grade.
- If `level.look.lut` is set, load it with `ColorLut::load(path)` into `post.lut`.
- Pass the post settings in the frame (`FrameScene::post = Some(&post)`; `LabScene::post` in the
  lab types). The frame renders to the profile's linear HDR target and the post stack applies
  exposure, bloom, AgX (`post.tonemap = AgX`), the grade and the LUT once, at the end.
- Target: `render_into(device, &frame, FrameTarget::full(&view, Rgba8UnormSrgb, w, h))`, or
  `render_offscreen` (`Rgba8Unorm`).
- DO27 content exposure bands (`do27_level`) are EV100 11.1–11.3 for the Switchbacks and 11.9–12.1
  for Bore Twelve. Put them in the level's `look.min_ev100` / `look.max_ev100`
  ([Map look](../agents/look.md)).

## 3. Frame scene

| Field | Value |
|---|---|
| `near` / `far` | `0.1` / `12000.0` (the sun cascades stop at 4 km by themselves) |
| `depth_prepass` | `settings.profile.depth_prepass` (`true`) |
| `directional_shadows` | `true` |
| `ibl` | `true` |
| `fog` | `FogSettings::default()` (**disabled**; the planet sky and the froxel haze do the aerial perspective) |
| `directional` | `DirectionalLightUniform::new(travel, color, lux)` with physical lux (DO27: 45 000 Mars, 18 000 Europa) |
| `ambient_lux` | `Environment::default().ambient_lux` (only used without a planet sky) |
| `double_sided` | `false` |
| `meshes` | set pieces, scatter rocks (section 6) and vehicles, as `FrameMesh`es |

The sun direction is the map's authored light. With a level clock (`atmosphere.time_of_day`),
the renderer replaces it with the atmosphere's transmitted sun.

## 4. Level: atmosphere, sky body, fog

- Load the `.level.ron` with `SceneFile::from_ron_str` then `regia_scene::format::migrate_to_latest`.
  Read `LevelSettings` from the `"regia_scene::level::LevelSettings"` resource
  (`LevelSettings::from_ron`).
- Build the outdoor state with `let mut outdoor = OutdoorParams { fog: VolumetricFogSettings::default(), ..Default::default() };`
  then `outdoor.apply_level(&level.atmosphere, false)`, then `renderer.set_outdoor(outdoor)`.
- `atmosphere.preset` is `Mars` or `Airless`. `sun_illuminance_lux` is the map's.
  `haze` is the Switchbacks dust (`density 1.2e-5`, `scale_height_m 600`; it becomes the froxel
  fog) ([Atmosphere](../agents/atmosphere.md), [Volumetric fog](../agents/volumetric-fog.md)).
- Bore Twelve: `atmosphere.sky_body = Some(SkyBody { direction, fill_lux: Some(1600.0), ..SkyBody::jupiter_from_europa() })`
  and `moons = vec![SkyBody::io_from_europa() at its direction]`.
  - `fill_lux` is the Jupiter-shine, a shadowless fill on meshes, terrain and scatter.
  - Do not draw a Jupiter card of your own.
- Textured moons (alpha rc2): the sky bodies above stay tint-only unless the shell passes
  albedo maps. Load each moon's equirectangular image (longitude across, north pole at the top,
  sRGB RGBA8) into a `SkyBodyTexture` and set `outdoor.moon_albedo[i] = Some(Arc::new(tex))`
  before `renderer.set_outdoor` — `moon_albedo[i]` textures `moons[i]`, `None` keeps the tint
  byte-identical to today. The body's map is `outdoor.sky_body_albedo` (same format). Do not
  set a map without the matching `moons`/`sky_body` entry; an unset slot is the shipped look.
- Tunnels and interiors: `renderer.set_interior_scene(Some(EnclosureScene))` and
  `OutdoorParams::interiors` ([Tunnels](../agents/tunnels.md)).

## 5. Terrain: the shared draw and the renderer's three hooks

<!-- ignore-reason: requires a GPU device and queue to create the terrain draw and contact map; names `wgpu::TextureFormat`, and wgpu is not a dependency of the book's snippet crate -->
```rust,ignore
let mut draw = TerrainSharedDraw::new(device, map, &layers, CdlodConfig::default(), root_level)?;
draw.set_material_cooked(&cooked, params, macro_tiles, &[])?; // the cooked BC7/BC5/BC4 pack
draw.set_relief(TerrainRelief::default());                     // strata, near/mid relief
let contact = ContactMap::new(&device.device);
draw.set_contact_map(Some(&contact));
draw.prewarm(wgpu::TextureFormat::Rgba16Float);
let surface = draw.surface();                                   // for scatter (section 6)
renderer.set_shared_draw(Some(Box::new(draw)));
```

- Use a block-compressed device for cooked terrain (`regia_terrain::gpu::headless_bcn_device`
  headless, or request `TEXTURE_COMPRESSION_BC` on your device).
- Root level: 7 for a 1280 m map at 4096² (0.31 m cells at the default 32-cell grid).
- The renderer's three hooks are `set_shared_draw`, `set_outdoor` and `set_interior_scene`.
  - The renderer then calls the draw's `prepare`, `set_engine_csm`, `set_engine_ao` and
    `finish_prepare` itself.
  - The terrain receives the engine's sun cascades, SSAO, hemisphere sky ambient, planet-shine
    and canyon sky occlusion with no further host work.

See [Photoreal terrain](terrain-photoreal.md) and [Terrain data](terrain-data.md).

## 6. Scatter

Set-up, once:

```rust,wrap
# use std::sync::Arc;
# use regia_terrain::{
#     DensityMaps, RenderedSurface, ScatterField, ScatterLibrary, ScatterSet, ScatterSource, TerrainLayers, TerrainMap,
# };
# let (text, map, layers, library): (String, TerrainMap, TerrainLayers, ScatterLibrary) = todo!();
# let surface: RenderedSurface = todo!();
let set = ScatterSet::from_ron(&text, Some(&layers))?;
let source = ScatterSource { map: Arc::new(map.clone()), holes: None, layers: Some(layers.clone()), density_maps: DensityMaps::new() };
let mut field = ScatterField::new(&set, source, &library, set.seed)?;
field.set_rendered_surface(Some(surface)); // rocks sit on the drawn CDLOD surface
```

Per frame, before `render_into`:

```rust,wrap
# use regia_render::{FrameMesh, FrameScene, RenderDevice};
# use regia_terrain::{ContactMap, ScatterField, ScatterView};
# let device: &RenderDevice = todo!();
# let mut field: ScatterField = todo!();
# let mut contact: ContactMap = todo!();
# let (eye, target): ([f32; 3], [f32; 3]) = todo!();
# let (fov_y, aspect, near, far): (f32, f32, f32, f32) = todo!();
# let mut rocks: Vec<FrameMesh<'_>> = Vec::new();
# let mut frame: FrameScene<'_> = todo!();
field.update(eye);                          // tile streaming
contact.update(&device.queue, &field, eye); // rock contact pools the terrain samples
let view = ScatterView::look_at(eye, target, fov_y, aspect, near, far);
field.collect(&view, &mut rocks);           // culled, LOD-picked rock FrameMeshes
frame.meshes.extend(rocks);
```

See [Terrain scatter](../agents/terrain-scatter.md).

## 7. Materials and shadows

- **Set pieces (glTF).** Map them as follows (`do27::load_gltf`):
  - `baseColorTexture` → `LabMaterial::albedo`, uploaded as sRGB bytes, not linearised.
  - `normalTexture` → `normal_map`.
  - `occlusionTexture` (ORM R) → `occlusion_map`.
  - `baseColorFactor` → `base_color`; the metallic and roughness factors carry over.
  - Compute tangents from the UVs.
- **Terrain** takes its material only from the cooked pack and `MaterialParams`. Do not tint it in
  the host.
- **Shadow casting** (`FrameMesh::shadow`):
  - Set pieces and vehicles: `ShadowCast::All`.
  - Rocks: whatever `ScatterField::collect` sets. That is `ShadowCast::Local` within the class's
    `shadow_distance` and `shadow_max_lod` when the class casts shadows, otherwise no shadow.
  - `ShadowCast::Local { .. }` is only for short props.

## 8. Do not

- **Do not use MSAA** (`msaa_samples ≥ 4`). On an MSAA frame the renderer skips the shared terrain
  draw and the planet sky (sky bodies, moons, airless sky). Auto exposure then meters a black frame
  and blows every mesh out. The renderer warns once when this happens.
- **Do not pre-expose.** Lights are physical lux and materials are albedo. Do not scale the sun, the
  ambient or the materials by an exposure. Exposure happens once, in post (the report flags a sun
  under 1000 lux).
- **Do not draw a host sky, sky dome, skybox, Jupiter card or forward fog on top.** The planet LUT
  sky, the aerial composite and the froxel haze already draw them. `FrameScene::fog.enabled` must be
  `false`.
- **Do not ship lab settings.** That means `FrameQuality::LAB` / `EDITOR`, `post.enabled = false`, a
  fixed `exposure_ev` without the map's band, or a `render_scale` below native.
- **Do not set a level clock** (`time_of_day`) if the map authors its sun. The clock replaces the
  authored light.
- **Do not hash texture bytes every frame.** Build the frame scene once and update only the camera
  and meshes (`do27_chase` does this).

## 9. Characters, vehicles and other dynamic meshes

Skinned and rigid meshes are lit by the same forward path as every set piece. No extra call is
needed beyond the items above. That path is:

- the sun with its cascades;
- the planet's sky irradiance, whose lower half is the planet's ground lit by the sun;
- the sky body's planet-shine fill;
- the terrain's wall bounce (`GroundBounce`);
- the terrain's horizon, which takes the sky hidden by canyon walls and mesas out of a mesh's sky
  light, as the terrain does for itself.

`lookdev dyn_light` measures this against the reference path tracer at the same pose. It renders
a skinned 0.18 gray mannequin, a 0.05 charcoal one and a hull at 1, 3 and 5 m on both maps.

The lighting holds only when the host frame has all of the following:

- **`ibl: true`, `msaa_samples` 1 and the HDR post stack.** The atmosphere IBL, which carries the
  ground bounce onto a mesh's shaded side and underside, exists only on single-sample linear-HDR
  frames. With MSAA the terrain and the sky are not drawn at all (`ERROR`, `render.problems`
  `msaa_skips_terrain`).
- **The sky body on airless maps.** DO27's exported Bore Twelve `level.ron` has
  `sky_body: None`. Set `atmosphere.sky_body = Some(SkyBody { direction, fill_lux: Some(1600.0),
  ..SkyBody::jupiter_from_europa() })` before `outdoor.apply_level`. Without it, a mesh's shaded
  side gets only the ice's bounce: 2.4 klx instead of 3.8 klx. The gray mannequin then prints at
  55/255 instead of 82, the charcoal one at 28 instead of 44.
- **One sun.** `directional.illuminance` must equal `outdoor.planet.sun_illuminance` (the level's
  `sun_illuminance_lux`: 18 000 Europa, 45 000 Mars). The sky and ground light on the shaded side
  come from the planet's value, and the sunlit side from the directional light.
- **The level's exposure band** (`apply_level_look`). DO27's raw look prints the same frame darker
  than the harness's band.
- **Skinned palettes in world space with an identity model matrix**, or a model matrix with
  palettes in model space, never both. The renderer skins normals with the same palette.

**Large meshes need a proxy.** A parked hull, crate or vehicle shades its own surroundings in a
way the sky light above cannot know about:

- the ground under its skirt lies in its own shadow (without a proxy, down-facing faces print
  about 2× the tracer);
- the ground in its shadow sees the hull instead of the sky: its bounce, and on Mars a darker
  contact under it.

Add `OcclusionProxy::default()` next to the hull's `MeshRenderer` (albedo from the material,
ground at the bounds' lowest corner; set `ground_y` when the box starts above the ground). A host
that drives `FrameRenderer` itself calls `set_mesh_occluders` with `MeshOccluder::from_positions`
or `from_local_bounds` each frame. Up to 32 proxies, nearest to the eye first, get the full shade;
frames without proxies are unchanged. With more hulls (a 32-40 player match plus AI units and
structures), set `OcclusionProxy { far_band: true, .. }` (or `MeshOccluder::with_far_band(true)`):
the next 128 opted-in proxies each get a coarse ground-contact blob (sky and fill darkened under
the footprint, no sun shadow), and a hull near the 32nd fades from one to the other over 5 m, so
nothing pops. The band costs 2.5 KiB of the occluder uniform and a short sorted-list walk per
pixel; it is off by default, and frames with 32 proxies or fewer are byte-identical either way.
`render.mesh_occluders` lists them, with `bands { near, far, dropped }`. Skip characters: a
mannequin's shadow is too thin to matter.

On airless maps, consider `TerrainSharedDraw::set_fill_canyon_scope` (ADR-8412): where it applies,
the sky body's fill on the ground takes canyon-scale and contact occlusion only, as meshes take it.
With the default occlusion stack, Europa's shadowed ground reads 0.57–0.67× the tracer; with it,
0.81–0.94×. `FillCanyonScope::Off` (the default) applies it nowhere. `MapWide` (the old
`set_fill_canyon_visibility(true)`) applies it to every fill-lit pixel, so it lifts every trench and
canyon floor on the map too. `NearOccluders` applies it only in the sun shade of the mesh occluders
(the hull boxes above): 1 inside a box's sun-shadow polygon, fading out a quarter of the polygon's
radius past its edge and one to two box heights above or below its ground. The ground under and
beside the hulls then gets the fill's color, and the rest of the map keeps the full stack. For
Europa, use `NearOccluders` with hull proxies. With the sun under about 3°, a box casts no polygon,
and `NearOccluders` then lifts nothing. `terrain.get_ground_look` reports the scope as
`fill_canyon_scope` when the host has inserted `TerrainGroundLookControl`.

What the engine cannot fix is albedo: a 0.05 charcoal suit is 28–44/255 on Europa even when its
irradiance matches the tracer within 3 %. Mars light is orange. The sun is authored
1.0/0.72/0.48 and the ground is red, so a neutral suit there reads brown in the tracer too
(shaded side rgb 1.65/0.89/0.45). Push it back with the grade, not the light.

## 10. Baked GI (ships with v0.2.0)

A map baked with `regia bake-gi` ships a `.fgiv` volume (`docs/formats/baked-gi.md`). At level
load, read it (`regia_asset::baked_gi::BakedGiVolume::from_bytes` on the pak entry, or `read` on
a file) and install it with `RuntimeRenderer::set_baked_gi(Some(Arc::new(volume)))`; install `None`
when the next map has no bake. Do this once per map, like `set_gi_field`. A stand-in renderer for a
warm worker gets it too. The volume uploads on the next frame. Its size is printed in the log and in
`render.get_gi_mode`.

While it lights the frame, it replaces DDGI. That frame needs linear HDR, the single-sample depth
prepass and a desktop build. SSGI is off, and full-resolution GTAO drops to half resolution. The sun,
its shadows and the paged cache stay as they are. Every preset uses it, Low included. Only a player
who chose `LightingMode::Balanced` keeps the analytic terms.

- **Light levels:** the sun-bounce term follows the frame's sun (illuminance × color over the baked
  sun). The fill term follows the frame's sky-body fill. Sky and emissive take
  `BakedGiSettings::{sky_scale, emissive_scale}`, relative to the bake. Change those with
  `set_baked_gi_settings` when `apply_level_look` changes the sky. The bake is valid for its sun
  direction only. A sun more than 3° off logs a warning once.
- **Fill:** the bake holds the sky-body fill shadowed, so where the volume covers it replaces the
  forward and terrain passes' live, unshadowed fill. Set `replace_live_fill: false` to go back to
  the live fill (the baked fill is then dropped).
- **Coverage:** outside the volume (more than one column spacing beyond the outermost columns, or
  more than twice the top level above the ground), the frame keeps its analytic terms.
- **Check:** `render.get_gi_mode` → `baked.active` is `true` and `baked.scales` are near 1 at the
  baked look; `sun_mismatch_deg` is near 0.

Related agent pages: [Renderer](../agents/renderer.md), [Map look](../agents/look.md),
[Atmosphere](../agents/atmosphere.md), [Look-dev](../agents/lookdev.md), [Baked GI](../agents/baked-gi.md).
