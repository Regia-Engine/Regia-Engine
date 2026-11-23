# Render lights: more suns, ambient volumes and debug views

A frame draws more than its sun and its point and spot lights (`specs/13` § 6, § 10, § 11, ADR-7211):

- **Up to four directional lights.** The sun is the lowest-entity `DirectionalLight`, as before. It keeps
  the cascaded shadow map when its `cast_shadows` is on. The next
  three, in entity order, shade with the sun's BRDF and are **not shadowed**. A fifth light is refused:
  it is counted, and the engine logs one warning per process.
- **Ambient volumes.** An `AmbientVolume` component is a box centered on the entity's transform and
  turned by its rotation: the box's own axes bound it, so a volume turned 45 degrees covers a diagonal
  band (ADR-7275). The transform's scale multiplies `half_extents` along those axes. Every surface
  inside the box gets `color × intensity` lux of extra uniform irradiance on its diffuse ambient term,
  scaled by the material's occlusion. The fill fades to nothing over `blend_distance` meters past the
  faces. A frame draws up to four volumes and refuses the rest, the same way as extra lights.
- **Twelve more debug views** through `render.set_debug_view` (see below).
- **Hidden lights light nothing** (ADR-7490, ADR-7549). A `DirectionalLight` or `AmbientVolume` that is
  hidden, by its own `Visibility::Hidden` or under a hidden group, is not extracted: it neither shades the
  frame nor counts as refused. Both components bring `InheritedVisibility` with them, so a sun parented
  under a hidden group needs no `Visibility` of its own. The editor's bake panel and `level.export_bake`
  skip a hidden sun too. Point, spot and area lights are not filtered yet.

The forward pass, the visibility-buffer resolve and the Mobile forward draw the extra lights and the
volumes. Since ADR-7254 the extra lights also reach the terrain's shared draw and its grass (Lambert
plus the GGX lobe, unshadowed), the probe GI update (the probes' hits take each light's Lambert term,
unshadowed), and `regia_pt`: a scene extracted from a frame or a frame export carries them as
`PtScene::extra_suns`, which the path tracer, the GPU and CPU probe bakes and both lightmap bakes
light like the sun (shadowed, in the sun component; the path tracer treats them as delta directions).
A scene with one sun traces and bakes the same bytes as before. Since `.bake.ron` v3 (ADR-7818) a
bake description lists up to four suns (`suns: [..]`), and bakes set up from it (`regia bake-gi`,
`regia bake-lightmaps`) take every one ([Bake light inputs](baked-gi-light-inputs.md)). Still
sun-only: the scatter impostors, water, and the two writers that pick one sun from a level: the
level export (`level.export_bake`) and the editor's bake panel. `render.lights` lists both sets.

## Methods

### `render.lights`

`{}` returns:

| Field | Meaning |
|---|---|
| `sun` | `{ direction, color, illuminance, shadows }` of the light with the cascades, or `null` |
| `extra_directional` | `[{ direction, color, illuminance }]`, up to three, in entity order |
| `refused_directional` | directional lights the last extraction refused past `max_directional` (4) |
| `ambient_volumes` | `[{ center, half_extents, irradiance, blend_distance, rotation }]`, up to four; `rotation` is the box-to-world quaternion `[x, y, z, w]` |
| `refused_ambient_volumes` | volumes refused past `max_ambient_volumes` (4) |
| `debug_view_code` | the forward shader's debug view (0 = lit), see the table below |
| `extra_directional_paths` | the paths that shade the extra lights (`forward`, `visibility_buffer`, `mobile_forward`, `terrain`, `grass`, `ddgi_probes`, `path_tracer`, `probe_bake_gpu`, `probe_bake_cpu`, `lightmap_bake_cpu`, `lightmap_bake_gpu`; ADR-7254; `bake_description`: a `.bake.ron` v3's `suns`, ADR-7818) |
| `sun_only_paths` | the paths that still see only the sun (`scatter_impostors`, `water`, `level_bake_export`, `editor_bake_panel`) |

Errors: `F0003` when the app has no render plugin (no extracted frame).

### `render.set_debug_view`

Since ADR-7211 the method also takes these modes, and `render.get_debug_view` lists them:

| Mode | Code | Shows |
|---|---|---|
| `unlit` | 1 | base color plus emission, no lighting |
| `normals` | 2 | the shading normal, `n × 0.5 + 0.5` |
| `base_color` | 3 | base color only |
| `roughness` | 4 | perceptual roughness as gray |
| `metallic` | 5 | metalness as gray |
| `ambient_occlusion` | 6 | the material's occlusion (map × strength) as gray |
| `light_complexity` | 7 | lights in the pixel's froxel list: dark blue for none, then blue → green → yellow → red at 16 or more |
| `shadow_cascades` | 8 | the sun cascade the pixel reads: red, green, blue, yellow for 0 to 3; gray past the last one or without cascades |
| `motion_vectors` | 9 | screen motion since the previous frame through the camera; (0.5, 0.5) is still |
| `mip_level` | 10 | the base-color texture's mip level, blue (0) to red (10 or more) |
| `overdraw` | – | every mesh drawn additively in one dim color, with no depth rejection between meshes: the brighter, the more layers |
| `lod_color` | – | each mesh in the color of its LOD level: green, yellow, orange, red for LOD 0 to 3+ |

The shader views (codes 1 to 10) draw display-referred colors divided by the frame's exposure, then
go through the post stack. As with the other debug views, a channel reads as a shade, not an exact
value. Overdraw and LOD coloring swap each mesh's material, like the UV checker. Materials that are
already unlit (glTF `KHR_materials_unlit`) keep their own look in the shader views. `mip_level`
reads the base-color texture's own size, on the bindless texture table path as well (ADR-7254).

The Mobile forward (`forward_mobile.wgsl`, the lite tiers of the Mobile profiles on tile GPUs) draws
all ten shader views too (ADR-7254). Its `light_complexity` counts the froxel's whole list, though the
lite light loop stops at 4 (Low) or 8 (High) lights; its `shadow_cascades` colors the one or two
cascades a Mobile profile plans. Overdraw and LOD coloring are material swaps and draw on every path.

## `DebugDraw` durations and labels

`DebugDraw::line_for(from, to, color, frames)` keeps a line for `frames` frames (0 and 1 both mean
the next frame only). `DebugDraw::text(position, text, size, color, frames)` adds a world-space label,
`size` meters tall, facing the primary camera. Each `DebugLine` and `DebugText` has a `duration`
field.

Labels draw from the UI font atlas when `regia_ui`'s `UiPlugin` is in the app (ADR-7254): the plugin
installs a glyph source into `regia_render::DebugLabelGlyphs`, the label is shaped with the UI's
embedded fonts, and the gizmo pass draws one quad per glyph that samples the atlas page, with the
same depth test and on-top flag as the lines. An app without `regia_ui` (or a label the source cannot
lay out) draws the gizmo stroke font, as before. A host can install its own source with
`regia_render::install_debug_label_glyphs`. Color glyphs (emoji) are skipped.

## Android storage-buffer floor

On Android, device creation refuses to start when the GPU has fewer than
`ANDROID_MIN_STORAGE_BUFFERS_PER_STAGE` (8) storage buffers per shader stage, or none in fragment
shaders. It logs the reason and returns `RenderError::GpuSelection` (`regia_render::storage_floor`).

## Recipe: check that a second sun lights the scene

1. Spawn two `DirectionalLight`s. Then `render.lights` should list one `sun` and one entry in
   `extra_directional`, with `refused_directional` 0.
2. Take `screenshot` with both lights on and again with the second light's `illuminance` at 0. A
   surface facing the second light is brighter in the first image. Shadows still come from the sun only.
