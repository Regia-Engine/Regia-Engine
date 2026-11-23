# Custom decal textures

A `.decal.ron` v3 prefab can name a cooked RGBA8 image:

```ron
(version: 3, name: "paint", half_size: (1.0, 1.0, 0.2),
 texture: Some("images/paint.fckd"), blend: "replace")
```

Cook the source image with its texture importer setting `compress: false` to
produce `CookedImage` data. Put the cooked file beside or
below the prefab directory and configure `VfxSettings.library_dir`. The library
loads it before placement. Hosts may instead use `DecalLibrary::add_textured`
with decoded image data. Custom images need no new agent method or permission.

Call `vfx.spawn_decal` with the loaded prefab name:

```json
{"prefab":"paint","position":[0,0,0],"normal":[0,1,0]}
```

The result includes `texture: "images/paint.fckd"` and `atlas_layer: 4` in
addition to the existing slot, generation, pool counts and normal. Layer 4 is
the first custom layer; later loaded paths occupy 5–7. Built-ins report
`texture: null` and their ordinary atlas index (solid is -1).
`vfx.place_rut` and entity `StaticDecal` placement use the same resolved prefab.

Use `vfx.decal_budget` to inspect the forward/post/split path. A custom image's
RGB multiplies the prefab tint and its alpha multiplies opacity and fades;
transparent texels leave the receiver unchanged. Custom color does not create
a normal map. Terrain continues through the projected pass. Both paths use the
same texture layer and respect receiver masks.

Failures use existing `INVALID_PARAMS` (JSON-RPC -32602, F0002) for an unknown
prefab or unresolved texture; `UNSUPPORTED` (-32004, F0016) means there is no decal pool.
This feature introduces no new error code.
A failed call places nothing. Library IO errors are reported by the host loader,
which keeps the previous library intact. Paths cannot escape the prefab directory.

The current atlas supports four custom images at 64×64. Other dimensions are
resampled with nearest-neighbor sampling; excess distinct paths are refused.
No live layers are evicted. Reloading a prefab reloads its image in place;
image-only file changes are not watched by this feature. See the
[format contract](../../docs/formats/decal-ron.md) for migration and host APIs.

Verification: `custom_decal_texture_draws` draws an opaque red half and a
transparent half on a receiver, then checks a character mask and unchanged
built-ins. `custom_texture_forward_draws` checks the same color and alpha on a
lit mesh. `custom_decal_texture_dispatch_reports_resolution_and_refuses_unloaded_images`
drives the dispatcher and checks the returned layer, texture and error path.
No image golden is created or changed.

## Damage atlas cells

The cook-side damage atlas (`vfx.damage_decal_atlas`,
[decal atlas and stair walkability](decal-atlas-walkability.md)) feeds this
path: `regia_vfx::damage_decal_cell_image(&atlas, cell)` cuts one 256×256 cell
(0–15) as a `CookedImage`, and `DecalLibrary::add_damage_cell(prefab, &atlas, cell)`
registers a prefab with it. The prefab's `texture` becomes
`damage-atlas/cell-NN`, so the same cell registered twice shares one layer.
A cell index of 16 or more, an atlas that is not 1024×1024 RGBA8, or a full
atlas is refused and changes nothing. `vfx.spawn_decal` then reports
`texture: "damage-atlas/cell-NN"` and the layer like any custom image.

## Footstep surfaces

A v3 prefab may also name the footstep surface of the ground it covers:

```ron
(version: 3, name: "puddle", half_size: (1.0, 1.0, 0.3), surface: Some("puddle"))
```

`surface` is 1 to 32 bytes of lowercase ascii, digits and `_` (the spelling
footstep banks use as surface keys). Every placement path (`vfx.spawn_decal`,
`PlaceDecal`, `StaticDecal`, routed sim events, prefab reload) records it on the
pool. `DecalPool::footstep_surface_at(point)` answers the surface at a world
point: among live decals with a surface whose box contains the point, the
highest priority wins, then the newest. `None` leaves the footstep to the
receiver underneath (for example the terrain layer through
`FootstepBank::layer_surfaces`); a host footstep probe passes the answer as the
`FootstepEvent` surface. It is presentation only: navigation, collision and the
simulation never read a decal's surface.

`vfx.decal_surface_at` asks the same question:

```json
{"point":[0.2,0.0,-0.1]}
```

returns `{ "surface": "puddle", "live": 3 }`, or `"surface": null` when no
surface decal covers the point. It is a read and needs no permission beyond
read. Errors: `UNSUPPORTED` (-32004, F0016) without a decal pool, and
`INVALID_PARAMS` (-32602, F0002) when `point` is not three numbers. No new
error code. `vfx.spawn_decal` adds `surface` (the prefab's, or null) to its
result.

Verification: `cargo test -p regia_vfx --lib decal::tests` covers the pool
query (box containment, priority, newest-wins, retirement and recycling), and
`cargo test -p regia_vfx --test custom_decal_textures` covers damage cells as
custom images and the agent answer.
