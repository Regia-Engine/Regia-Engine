# Terrain scatter (rocks and props)

A map's rocks, rubble and debris come from its `.scatter.ron` (format v2,
`docs/formats/terrain.md` § 2): classes, each with weighted glTF meshes, density rules (density
map, noise, slope / altitude / layer mask, exclusion paths) and a look (tint, base blend, contact
AO, fade distances). Placement is seeded per cell, so the same file and map place the same rocks
on every client; the runtime streams 64 m tiles around the camera and hands the instances to the
frame renderer, which culls them on the GPU (frustum + Hi-Z) and draws them instanced.

## Check what is on screen

`terrain.scatter` returns the requested state and the last frame's counts:

```json
{ "enabled": true, "contact": true, "class_overrides": [],
  "report": { "installed": true, "resident": 21515, "tiles": 431, "tiles_visible": 96,
              "emitted": 4331, "per_lod": [14, 321, 3996, 0], "fading": 1779,
              "classes": [ { "name": "boulders", "enabled": true, "resident": 6120 }, … ] } }
```

`emitted` is what reached the frame before the GPU cull; `resident` is everything placed near
the camera. `installed: false` means the host has not recorded a field yet.

Instances placed by hand come from the brush, not the seed, and draw through the same field:
[painted scatter](terrain-scatter-paint.md). `resident` and `emitted` count both.

## Toggle and compare

`terrain.set_scatter` takes any of `enabled`, `contact`, or `class` + `class_enabled` and returns
`{ before, after }`. A/B a change the way a reviewer would:

1. `seeing.screenshot` (target `"offscreen"`) → `on.png`
2. `terrain.set_scatter { "enabled": false }`, `play.step`, `seeing.screenshot` → `off.png`
3. `seeing.compare_images` on the two; set `enabled` back to `true`.

`{ "contact": false }` removes only the soft occlusion pool under each rock (the ground
darkening and dust tint); `{ "class": "gravel", "class_enabled": false }` hides one class.

`{ "class": "ferns", "class_subsurface": "foliage" }` puts the foliage subsurface profile on every
material of one class (`"off"` restores the materials the field was built with, byte for byte); plain
materials then scatter like mesh leaves wherever the render profile asks (`render.set_subsurface`, off in
every preset; [subsurface scattering](subsurface.md), ADR-3170). A material graph does not scatter on this
path, for scatter as for any mesh, and a swaying class's materials are graphs: such a class keeps its look,
and `report.classes[].subsurface_unsupported` says `material_graph` (`null` when the class does not ask or
can scatter; the codes are in the [subsurface page](subsurface.md)). `terrain.scatter` lists the overrides
in `foliage_subsurface` and each class's profile in `report.classes[].subsurface`. `class` needs
`class_enabled`, `class_subsurface` or both; `class_subsurface` needs `class`. A refused call changes
nothing.

`{ "class": "woodland_trees", "class_caster": { "from_lod": 1, "leaf_keep_one_in": 4 } }` and
`{ "class": "woodland_trees", "class_season": { "kind": "broadleaf" } }` set a class's lower-detail shadow casters
and its season (each `null` to clear). Both need `class`, and both are in
[Streamed forests](scatter-forest.md).

## Far rocks as impostors

`{ "impostors": true }` turns on the octahedral impostor far field (opt-in, off by default)
where the host bound an impostor layer: rocks past their class's `fade_start` hand over to a
camera-facing card baked from the rock (no pop: the mesh dithers out across the fade band
while the card dithers in), and past `fade_end` — where nothing used to draw — cards carry
every class out to 2 km or until the rock is under a pixel. The report then shows
`"impostors": true` and `impostor_band` (meshes in the hand-over band this frame). A/B it the
same way: screenshot, `terrain.set_scatter { "impostors": false }`, `play.step`, screenshot,
`seeing.compare_images` — the difference is far rocks only. Without a layer the switch does
nothing (`report.impostors` stays false).

## Dense ground: detail limits

`{ "detail": true }` turns on the dense-scatter detail limits (opt-in, off by default; the host
supplies its `ScatterDetail` with its output height and field of view): every rock also shrinks
into the ground by its own projected size (a 5 cm pebble ends at a few tens of meters, a boulder
of the same class keeps its range; the shrink is the distance fade's, so nothing pops), classes
stream only as far as their largest rock stays above the limit, and stones under
`shadow_min_radius_m` cast no shadow-map shadow (the contact pool and contact shadows ground
them). The report shows `"detail": true` and `emitted` drops. A/B it like the other switches;
the difference is sub-pixel stones in the distance and the shadows of tiny stones.

## Host contract

The host owns the `ScatterField` and `ContactMap`. Each frame: `control.apply(&mut field,
Some(&mut contact))`, `field.update(eye)`, `contact.update(queue, &field, eye)`,
`field.collect(&view, &mut frame.meshes)`, then `control.record(&field, stats)`. The lookdev
harness (`samples/lookdev/src/scatter_shots.rs`) is the reference.

Impostors: bake once at load (`field.bake_impostors(Some(settings))`,
`ImpostorLayer::new(device, queue, &atlas)`), bind the layer with
`TerrainSharedDraw::set_impostors(Some(&layer))` before boxing the draw, call
`field.settle_streaming(eye)` at the load screen, and each frame
`control.apply_impostors(&mut field, Some(&mut layer), settings)` before `field.update(eye)`
and `layer.update(queue, &field)` after it.

Detail limits: each frame `control.apply_detail(&mut field, settings)` before
`field.update(eye)`, with `settings = ScatterDetail { reference_height_px, reference_fov_y, ..ScatterDetail::default() }`.
`cargo xtask bench-do27 -- --scatter scan --density 1,3,5,10 --detail` measures it. `REGIA_IMPOSTORS=1` turns it on in the lookdev
harnesses (`look_r2`, `do27_chase`).

`update` places tiles on the background task pool (the next ring is prefetched and stays
invisible until needed) and `collect` culls and seats tiles in parallel; both give exactly the
rocks the synchronous, serial path gives. `set_background_streaming(false)` and
`set_parallel_collect(false)` switch to those paths when a measurement needs them (a heavily
loaded machine can make the parallel path's worst frame slower than the serial one).
`terrain_lab --do27-chase --variants scatter` prints the scatter CPU per frame and the sun
shadow pages rocks dirty per cascade (`scatter-held`: a parked camera must dirty none).

Evidence: `docs/evidence/scatter-rocks/`, `docs/evidence/impostors/`.

## Shadow LOD hint

`terrain.set_scatter { "shadow_lod": { "max_lod": 1, "min_lod": 0 } }` applies a field-wide shadow LOD
hint to every scatter instance, the counterpart of `render.set_shadow_lod_hint` (which takes an entity
and so cannot reach scatter). Each instance casts from its asset's own shadow LOD (else its drawn LOD)
clamped to `min_lod..=max_lod` and to the asset's LOD chain; when that differs from the drawn LOD the
shadow moves to a shadow-only draw of it. `max_lod: null` (or missing) is no cap. `{ "shadow_lod": null }`
clears it; off is the default and leaves every frame unchanged. `terrain.scatter` reports the current
hint as `shadow_lod`.

Cluster-cast shadows (the meshlet path) do not use this hint: a cluster's shadow comes from the same
cluster DAG level the camera view selects, so a per-entity LOD cap has no coarser mesh to switch to.
`render.mesh_lod` reports what the cluster path selected.

## World-anchored grids and gameplay colliders

By default each class's cell grid starts at the map edge, so a map grown by a backdrop ring places
different rocks. `ScatterField::with_placement_origin(PlacementOrigin::World(..))` anchors the grid
at a world point instead. `terrain.scatter_colliders` bakes the gameplay colliders of a set under
either origin. See [scatter gameplay colliders](terrain-scatter-colliders.md) (ADR-8029).
