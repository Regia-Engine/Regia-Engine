# .decal.ron — projected decal prefab v3

Spec: `docs/specs/26-terrain-vfx-highend.md` § 3. Example: `docs/formats/decal.example.ron`.

## Overview

`.decal.ron` is one named decal prefab: a box that projects albedo, and optionally normal, roughness
and emissive, onto receivers. The engine ships four names — `scorch`, `pad_marking`, `rut`, `grime`.
Levels add more by dropping `*.decal.ron` files in the VFX library directory (`VfxSettings.library_dir`).

The format is RON with `#[serde(deny_unknown_fields)]`. `version` is 3 after migration.
v2 (ADR-3255) added `metallic`, the metallic channel and `shading`. v3 (ADR-8121,
lead accepted 2026-11-16) adds optional `texture` and (ADR-8829, folded into the same
step) optional footstep `surface`. Both v1 and v2 migrate directly
to v3 by rewriting only the version; missing fields retain their defaults,
comments and formatting survive. `decal_migration_sources` lists `[1, 2]`.
`regia upgrade-project` (dry run by default, `--write` to apply with backup)
uses this migrator under the existing `decal` identity. `check-formats` pins its
schema in `format-hashes.ron`. v0.6.0 shipped v2; v3 is the `decal` step of the v0.7
format batch 2 (freeze 2026-11-28, ADR-8829). A v0.6.0 build refuses a v3 file with
`decal prefab has version 3, but this engine reads version 2`; this build refuses v4 the
same way.

Every number must be finite (RON spells `NaN` and `inf`): a file with a non-finite roughness, normal
strength, `normal_xy`, fade, edge softness or lifetime is refused, and `lifetime` must be at least 0
(ADR-2077).

`//` line comments are stripped only for the built-in example. Authored files should be plain RON.

## Schema

```
DecalPrefab {
    version: u32,                 // 3 (v1 and v2 files migrate)
    name: String,                 // Stable id (agent `prefab`, StaticDecal.prefab)
    half_size: (f32, f32, f32),   // (across, along, depth) metres; each > 0
    albedo: String = "#ffffff",  // sRGB hex #rrggbb or #rrggbbaa
    opacity: f32 = 1.0,           // 0..=1
    roughness: f32 = 1.0,         // 0 smooth, 1 rough
    normal_strength: f32 = 0.0,   // 0 keeps the geometric shade
    normal_xy: (f32, f32) = (0, 0),
    emissive: String = "#000000",
    angle_fade: (f32, f32) = (0.25, 0.6),   // smoothstep on |dot(normal, axis)|; end <= start disables
    distance_fade: (f32, f32) = (0, 0),     // metres from the camera; end <= start disables
    priority: i16 = 0,            // higher draws later
    receiver_mask: u32 = 3,       // bit 0 terrain, bit 1 static mesh, bit 2 character
    atlas: String = "solid",      // solid | scorch | pad | rut | grime
    lifetime: Option<f32>,        // None stays until the pool recycles it
    edge_softness: f32 = 1.0,     // 0 hard box, 1 feathered edge
    channels: String = "albedo",  // albedo, normal, roughness, emissive, metallic joined by '+'
    blend: String = "modulate",   // modulate | replace
    metallic: f32 = 0.0,          // v2: written with the metallic channel (forward decals only)
    shading: String = "auto",     // v2: auto | post
    texture: Option<String> = None, // v3: relative cooked RGBA8 image path
    surface: Option<String> = None, // v3 (ADR-8829): footstep surface; omitted from output when None
}
```

`shading` (v2, ADR-3255): `auto` shades the decal in the forward pass when the forward-decal switch
is on (`RenderProfile::forward_decals`) and the decal writes albedo, normal, roughness or metallic;
its normal and roughness are then lit like the surface's own. `post` keeps the projected post pass,
which relights the finished colour. The post pass ignores `metallic`.

`receiver_mask` 3 is terrain and static meshes, not characters. `0` or `u32::MAX` accepts every layer.
Unknown atlas, channel, blend, or hex is a hard error.

## Placement

- `DecalPrefab::instantiate(position, normal, yaw, lifetime)` builds a `Decal` whose local +Z is the
  surface normal, plus a `DecalSurface`.
- `place_decal` applies a `PlaceDecal` into the live pool immediately. The agent method
  `vfx.spawn_decal` writes the same slot itself and does not call `place_decal` again.
- `StaticDecal { prefab, yaw, receiver_mask }` on an entity with a `Transform` is ingested once.
  An identity rotation projects along world +Y; any other quaternion is the decal basis. Scale
  multiplies the half extents. A non-zero `receiver_mask` replaces the prefab mask.
- `DecalStrip` stamps the prefab's surface along a polyline (tyre ruts). `vfx.place_rut` is the
  agent entry; `curve: true` with three points samples a quadratic.

The forward renderer has no G-buffer. Decals draw in the translucent pass. A host that binds a
receiver view (R = layer/255, GBA = world normal encoded to 0..1) and a copy of the scene colour
gets the material composite. Without those views the same prefab draws as a premultiplied tint,
which is the M14 path.

## Custom color and coverage (v3)

`texture: Some("images/paint.fckd")` overrides `atlas` with an authored sRGB
color and straight-alpha coverage image. The image uses the existing cooked
`CookedImage` wrapper (use the texture importer setting `compress: false`,
not a compressed `CookedTexture`).
Parsing and migration do not open the filesystem. `DecalLibrary::load_dir` and
`load_file` resolve images relative to each prefab directory before placement;
missing, invalid, oversized (>64 MiB cooked file) or escaping paths fail the
load. Absolute paths, parent/dot components and symlink escapes are refused.
A failed directory load preserves the entire previous library and atlas.

The existing atlas has eight 64×64 layers. Four retain the built-ins byte for
byte; four are available for custom images. Images of other dimensions are
resampled by integer nearest-neighbor texel-center mapping; author at 64×64 for
exact texels. The fifth distinct custom image fails explicitly. Reusing a path
reuses its layer, and reloading its image updates live decals at that layer.
No layers are evicted or reordered while live decals may refer to them.
Custom images supply color and coverage only; normal and material values stay
on the prefab, without deriving a height map from the custom image's colors.

A host using an asset server can call `DecalLibrary::add_textured(prefab,
&CookedImage)` with already decoded image data. `add` alone leaves a textured
prefab unresolved; its `instantiate` returns an error, never a solid fallback.
Loaded prefabs carry private resolution state and are constructed through
`from_ron`, rather than struct literals. Serialization omits this state.

`VfxRuntimePlugin` automatically uploads the library's immutable atlas snapshot
when it changes and publishes the matching forward pages. Standalone users
call `VfxGpu::set_decal_texture_atlas` or `DecalScene::set_decal_texture_atlas`;
a host feeding forward decals directly uses `texture_atlas().forward_atlas()`.
The standard forward/post/terrain split, fades, masks, and priority are retained.
There is no per-frame filesystem access or image resampling. Reload the prefab
to reload its referenced image; image-only source-watch events do not reload it.

## Footstep surface (v3, ADR-8829)

`surface: Some("puddle")` names the footstep surface of the ground the decal covers: 1 to 32
bytes of lowercase ascii, digits and `_`; anything else is refused at load. Writing a prefab
without one emits no `surface` key, so a v3 file written before the fold keeps its bytes.
Every placement path records it on the `DecalPool` (`set_footstep_surface`):
`place_decal`, `StaticDecal` ingest and prefab reload, routed sim-event decals and
`vfx.spawn_decal`. `DecalPool::footstep_surface_at(point)` returns the surface of the
highest-priority, then newest, live decal whose box contains the point, or `None`, which
leaves the footstep to the receiver underneath. A pool interns at most
`DECAL_FOOTSTEP_NAMES_MAX` (1024) distinct names. The agent read `vfx.decal_surface_at`
answers the same question (`book/agents/custom-decal-textures.md`). It is presentation
only: navigation, collision and the simulation never read it, so a decal never changes a
path or a lockstep checksum.

## Damage atlas cells (ADR-8829)

The cook-side damage atlas (`generate_damage_decal_atlas`, ADR-8038) supplies custom
images: `damage_decal_cell_image(&atlas, cell)` returns one 256×256 cell as a
`CookedImage`, and `DecalLibrary::add_damage_cell(prefab, &atlas, cell)` registers a prefab
with it under `texture: "damage-atlas/cell-NN"`, so equal cells share a layer. The cell is
resampled to the 64×64 layer like any custom image.
