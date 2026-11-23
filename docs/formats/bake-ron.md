# `.bake.ron`: GI bake description (format v3)

Reader: `regia_pt::bake_desc::GiBakeDescription` (`parse_versioned`, `build_with`). Writer: hand-written or
an exporter (DO27's `static_meshes.json` exporter, lane B-01). Used by `regia bake-gi <file.bake.ron>`, which
bakes a `.fgiv` (`docs/formats/baked-gi.md`). ADR-279 (format, meshes, automatic bricks) and ADR-280 (DO27
bundle files); ADR-1095 (v2: HDRI sky and area emitters), ADR-1096 (v2: bake quality controls) and ADR-7818 (v3: a list of
suns). Registered as
`bake` in `cargo xtask check-formats` and `regia upgrade-project`.

A description names the terrain, the static meshes, the interiors, the lights and the probe layout. The bake
traces the meshes as occluders and bounce surfaces with their base color; emissive parts become lights.

## Versions

| Version | Shape | Migration |
|---|---|---|
| 0 | The provisional pre-v1 file: no `format_version`; terrain, sun, sky, sky bodies, layout (terrain layer and explicit bricks), bake settings. | Every v0 file is a valid v1 file without the new fields. The loader reads it as v0 and bumps it in memory. `regia upgrade-project --write` adds `format_version: 1` after the root `(` and keeps the rest of the file (comments too) byte for byte. |
| 1 | Adds `format_version`, `root`, `terrain_mesh`, `meshes`, `static_meshes`, `interiors`, `interiors_file`, `manifest`, `default_albedo` and `layout.auto_bricks`. | Every v1 file is a valid v2 file without the new fields. The loader bumps it in memory. `regia upgrade-project --write` rewrites the root's `format_version` number and keeps the rest of the file byte for byte. |
| 2 | Adds `sky_hdri` (an equirect HDRI sky, optional sun extraction), `sky_atmosphere` (the runtime's atmosphere sky) and `emitters` (area emitters: quads, discs, spheres) (ADR-1095), and the bake quality controls to `bake`: `rr_start`, `clamp_direct`, `clamp_indirect`, `sample_groups`, `firefly_threshold` (ADR-1096). The defaults bake as v1 did. `max_bounces` is now checked (1..=64) instead of clamped. | `sun` becomes a list (v3). |
| 3 | Replaces `sun: Option<SunDesc>` with `suns: [SunDesc]`, at most four (ADR-7818). The first is the scene's sun; the others light the probe and lightmap bakes the same way (`PtScene::extra_suns`). | The loader moves a v0-v2 `sun: Some(s)` to `suns: [s]` and `sun: None` to `suns: []` in memory. `regia upgrade-project --write` rewrites the root's `sun` key and value the same way (`Some(` → `[`, its `)` → `]`; the sun's own text and comments stay), then the version number, and keeps the rest of the file byte for byte. A one-sun file bakes the same bytes before and after. Current. |

A file with a version above 3 is refused with `bake_desc_newer`. Unknown fields are refused (`bake_desc_parse`). A
v3 file that sets `sun`, or a v0-v2 file that sets `suns`, is refused with `bake_desc_invalid` naming the key.

## Paths

- `root` is relative to the description. It defaults to the description's directory.
- `meshes[].source`, `terrain_mesh.source`, `terrain_mesh.holes`, `static_meshes`, `interiors_file`, `manifest`
  and `sky_hdri.source` are relative to `root`.
- `terrain.heights` and `terrain.holes` stay relative to the description, as in v0.

## Example

```ron
(
    format_version: 3,
    // The DO27 bundle, read-only. Everything below except `terrain` is relative to it.
    root: Some("<build-dir>"),
    manifest: Some("MANIFEST.sha256"),

    // The terrain grid mesh becomes the traced heightfield, sampled at its own 1.25 m spacing.
    // Uncovered samples (the open borehole mouth) are holes.
    terrain_mesh: Some((source: "terrain/terrain.gltf", spacing: 1.25, albedo: (0.32, 0.30, 0.28))),

    // DO27's mesh list. The entry whose source is the terrain mesh is not placed twice.
    static_meshes: Some("static_meshes.json"),
    interiors_file: Some("interiors.json"),

    // Meshes added by hand: a cooked mesh or a glTF, with a column-major 4x4 transform.
    meshes: [
        (
            id: Some("beacon"),
            source: "assets_ferrite/props/beacon/beacon.gltf",
            transform: (1.0, 0.0, 0.0, 0.0,  0.0, 1.0, 0.0, 0.0,  0.0, 0.0, 1.0, 0.0,  12.0, 3.0, -40.0, 1.0),
            layer: Some("setpiece"),
            material: Some((albedo: Some((0.4, 0.4, 0.42)), emissive: Some((0.0, 0.0, 0.0)))),
        ),
    ],
    // Textured parts without an albedo override bake with this albedo (times the base color factor).
    default_albedo: (0.5, 0.5, 0.5),

    // The sun, then a second, dimmer sun (v3: up to four).
    suns: [
        (toward: (0.3, 0.5, 0.2), illuminance: 50000.0, angular_radius: 0.00465),
        (toward: (-0.6, 0.3, -0.4), illuminance: 8000.0, color: (1.0, 0.75, 0.55), angular_radius: 0.002),
    ],
    sky: None, // airless
    sky_bodies: [(direction: (-0.4, 0.6, 0.7), angular_radius: 0.12, radiance: (900.0, 820.0, 700.0))],
    // A 1500 lm lamp panel facing down over the beacon (v2).
    emitters: [
        (id: Some("beacon_lamp"),
         shape: Quad(corner: (11.5, 5.0, -40.5), edge_u: (0.0, 0.0, 1.0), edge_v: (1.0, 0.0, 0.0)),
         power: Some(1500.0), color: (1.0, 0.85, 0.7)),
    ],

    layout: (
        terrain_spacing: Some((8.0, 8.0)),
        levels: [1.0, 2.0, 4.0, 8.0, 16.0],
        bricks: [],
        // One brick over each interior, then one per cluster of set-piece placements.
        auto_bricks: Some((spacing: 1.0, margin: 2.0, max_probes: 262144, layers: ["setpiece"])),
        moment_resolution: 4,
        max_distance: None,
    ),
    bake: (rays_per_probe: 256, samples: 16, max_bounces: 8, seed: 1, probes_per_batch: 4096,
           relocation_iterations: 2, filter: true,
           // v2: quality controls (book/agents/baked-gi-quality.md).
           rr_start: 3, clamp_direct: 0.0, clamp_indirect: 0.0, sample_groups: 8, firefly_threshold: 6.0),
)
```

## Fields

### Root

| Field | Type | Default | Meaning |
|---|---|---|---|
| `format_version` | u32 | absent = 0 | 3 for this version. |
| `root` | path | description directory | Base of the bundle paths. |
| `terrain` | `TerrainDesc` | none | A raw 16-bit heightfield (v0 field, unchanged). |
| `terrain_mesh` | `TerrainMeshDesc` | none | The terrain as a grid mesh. Use it instead of `terrain`, not with it. |
| `suns` | list of `SunDesc` | `[]` | The suns (v3), at most four: the scene's sun and up to three more (the extra directional lights every bake path takes, `regia_render::MAX_EXTRA_DIRECTIONAL`). Every probe and lightmap bake integrates each one like the sun: its bounce lands in the sun component, and with `direct_sun` a lightmap bakes its direct light too. A fifth is refused (`bake_desc_invalid` at `suns`). |
| `sun` | `SunDesc` | none | v0-v2 only: the one sun. Read as a one-element `suns`. |
| `sky`, `sky_bodies` | | none | As in v0. |
| `sky_hdri` | `SkyHdriDesc` | none | An equirect HDRI sky (v2). Use it instead of `sky`, not with it. |
| `sky_atmosphere` | `SkyAtmosphereDesc` | none | The runtime's atmosphere sky for the first sun's direction (v2). Needs a sun; not with `sky` or `sky_hdri`. |
| `emitters` | list of `EmitterDesc` | `[]` | Area emitters (v2). |
| `meshes` | list of `MeshDesc` | `[]` | Static meshes. |
| `static_meshes` | path | none | A DO27 `static_meshes.json` (version 1). Its entries are added after `meshes`. |
| `interiors` | list of `InteriorDesc` | `[]` | Enclosed volumes. |
| `interiors_file` | path | none | A DO27 `interiors.json` (`do27-bake-interiors/1`). Its entries are added after `interiors`. |
| `manifest` | path | none | A `MANIFEST.sha256` (`<sha256>  <path>` per line, paths relative to `root`). Every file the bake reads under `root` must be listed with a matching hash. For a `.gltf` this includes its buffers and images. Files outside `root` are not checked. |
| `default_albedo` | RGB 0..1 | `(0.5, 0.5, 0.5)` | Albedo of textured parts without an override. The bake does not sample textures. |
| `layout` | `LayoutDesc` | required | Probe layout. |
| `bake` | `BakeDesc` | defaults | Bake settings (below). |

### `SunDesc`

| Field | Default | Meaning |
|---|---|---|
| `toward` | required | Vector toward the sun (normalized by the bake). |
| `illuminance` | required | Illuminance at normal incidence, lux. |
| `color` | `(1.0, 1.0, 1.0)` | Linear color. |
| `angular_radius` | 0 | Angular radius, radians (0 = a delta direction). |

The baked volume records the first sun as its baked sun (`BakeLights`, `docs/formats/baked-gi.md`): with
`follow_sun` the runtime scales the whole sun component, the other suns' bounce included, by the frame's sun over
that first sun. Bake several suns for a scene that keeps them at the baked levels (ADR-7818).

### `BakeDesc`

| Field | Default | Meaning |
|---|---|---|
| `rays_per_probe` | 256 | Rays per probe per sample (one rotated spherical Fibonacci set). Both tracers trace `clamp(n, 1, 4096)`. A larger value still validates, and the bake summary prints the stored value. |
| `samples` | 16 | Samples per probe. |
| `max_bounces` | 8 | Path vertices after the probe, 1..=64 (v2 checks the range; v1 clamped it). |
| `seed` | 1 | Seed: the same scene, settings and device give the same volume. |
| `probes_per_batch` | 4096 | Probes per GPU batch. |
| `relocation_iterations` | 2 | Relocation rounds before the main trace. |
| `filter` | `true` | Run the 3×3×3 visibility-weighted filter. |
| `rr_start` | 3 | v2. Path vertex from which Russian roulette may end a path, at least 1. |
| `clamp_direct` | 0 | v2. Luminance cap, cd/m², on light seen without scattering (sky, bodies, emitters at the first hit). 0 = off. |
| `clamp_indirect` | 0 | v2. Luminance cap, cd/m², on each contribution after a scattering event. 0 = off. |
| `sample_groups` | 1 | v2. Sample groups read back separately, 1..=`samples` (at most 64). 2+ give a noise estimate, 3+ allow firefly suppression. |
| `firefly_threshold` | 0 | v2. Scale a sample group above this many times the median group down to it. 0 = off, otherwise above 1, with 3+ groups. |

What the v2 controls do and how to pick them: `book/agents/baked-gi-bake-quality.md`. A bad value fails with
`bake_desc_invalid` at `bake.<field>`.

### `SkyHdriDesc` (v2)

| Field | Default | Meaning |
|---|---|---|
| `source` | required | The map: Radiance RGBE (`.hdr`, `.pic`; flat or run-length scanlines, `-Y h +X w` or `+Y h +X w`) or an uncompressed `.exr` (`R`, `G`, `B`, or the first layer that has them). The `EXPOSURE` header is not applied. |
| `rotation` | 0 | Turn about +Y, radians. A positive angle turns +Z toward +X. |
| `scale` | 1 | Map values to cd/m². Not with `irradiance`. |
| `irradiance` | none | Instead of `scale`: the map is scaled so a surface facing up receives this many lux from it, the extracted sun included. |
| `max_width` | 2048 | A wider map is box-filtered down by an integer factor (solid-angle weighted, light kept). At least 16. The tracer holds at most 2048 × 1024 texels. |
| `extract_sun` | `false` | Lift the sun out of the map into the bake's sun (below). Not with any `suns` entry. |
| `sun_threshold` | 100 | Sun extraction cut: texels brighter than this × the map's mean luminance. Above 1. |
| `sun_max_solid_angle` | 0.03 | A region above the cut larger than this (steradians), or whose directions are not a compact disc, is not a sun: the bake fails with `bake_hdri_sun` and the map is left unchanged. |

Mapping: row 0 is the zenith (+Y), the last row the nadir. The image's horizontal centre looks along −Z, and
`u` grows toward +X: seen from inside, the map is a panorama with +Y up and the right half of the image on the
right. Lookups are bilinear, U wrapped, V clamped. The tracer importance-samples the map (a CDF up to
1024 × 512 cells).

Sun extraction: the 8-connected region of texels above the cut around the brightest texel is lifted out. Its
texels take the mean of the ring of texels around it, and the excess becomes a sun disc: direction from the
excess-weighted centroid, illuminance at normal incidence from the excess, angular radius from the region's solid
angle. Map plus sun deliver the light the map alone did. A region that is not a compact disc is refused with
`bake_hdri_sun` before the fill is written: a full-width latitude band has a small solid angle, but its direction
vectors cancel, so the centroid is not a direction the texels face. The bake then treats a real disc like any sun:
the direct sun stays live at runtime and its bounce is baked into the sun component. A fixed probe ray set rarely
hits a sun-sized hot spot, so an HDRI that holds the sun bakes far less noisily with `extract_sun`.

### `SkyAtmosphereDesc` (v2)

| Field | Default | Meaning |
|---|---|---|
| `planet` | required | `Earth`, `Mars` or `Airless`: the runtime's `AtmosphereParameters` presets. |
| `height` | 2.0 | Height of the viewpoint above the ground, metres. |
| `scale` | 1 | Multiplier on the table. |

The sky is the planet's sky-view table for the description's sun direction, the table the raster frame draws and
lights with (`regia_pt::extract::atmosphere_sky`). Its level is the planet's own sun at the top of the
atmosphere (`AtmosphereParameters::sun_illuminance`), whatever the `sun` entry's `illuminance`; use `scale` to
follow another sun level. The planet's air as a participating medium is not part of the bake. `plan.lights.sky`
reports `kind: "table"` and `source: "atmosphere:<planet>"`.

### `EmitterDesc` (v2)

| Field | Default | Meaning |
|---|---|---|
| `id` | `emitters[<index>]` | Name in reports and errors. |
| `shape` | required | `Quad(corner, edge_u, edge_v)`: the parallelogram `corner + s·edge_u + t·edge_v`, front toward `edge_u × edge_v`. `Disc(center, normal, radius, segments: 64)`: a fan of 3..=4096 triangles facing `normal`. `Sphere(center, radius, segments: 32)`: a UV sphere of 4..=1024 segments, emitting outward. World space, metres. |
| `radiance` | none | Emitted radiance, linear cd/m². Exactly one of `radiance` and `power`. |
| `power` | none | Luminous power, lumens, over the emitting sides: `radiance = color · power / (π · area · sides)`, the area of the traced triangles. |
| `color` | `(1, 1, 1)` | Colour of a `power` emitter, normalized to luminance 1. |
| `two_sided` | `false` | Quads and discs emit from both faces. Spheres emit outward only. |

Emitters are traced as black emissive triangles: probes see them directly and through bounces, and next-event
estimation samples them, so their light lands in the emissive component. They reflect nothing and occlude like
any surface.

### `plan.lights`

`regia bake-gi --dry-run` (and the bake summary) prints what lights the bake under `plan.lights`: the `sun`
(`toward`, `illuminance`, `color`, `angular_radius`, `from_hdri`), `extra_suns` (the suns after the first, same
fields; absent with one sun or none, v3), the `sky` (`kind`: `none`, `uniform`, `table`
or `equirect`; `source` (the HDRI file, or `atmosphere:<planet>`), `source_size`, `size`, `downsample`, `scale`, `rotation`, `upward_irradiance` in lux per
channel, `mean_luminance`, `sun_extraction`), `sky_bodies`, each emitter (`id`, `shape`, `area`, `radiance`,
`power_lm`, `triangles`, `two_sided`), `emitter_power_lm` and `emissive_materials`. Check these before spending a
bake. For example, an `upward_irradiance` of 200 lux for a clear-day map that should give about 20 000 lux means
the map needs a `scale` (or an `irradiance` target).

### `TerrainMeshDesc`

| Field | Default | Meaning |
|---|---|---|
| `source` | required | A glTF or cooked mesh. All parts are rasterized top-down (highest surface per sample). |
| `spacing` | required | Sample spacing, metres. At the mesh's vertex spacing, the samples are the vertices. |
| `transform` | identity | Column-major 4x4, applied before rasterizing. |
| `albedo` | `(0.3, 0.3, 0.3)` | Linear albedo. |
| `holes` | none | A `<map>.holes.ron` (relative to `root`), cut as for `terrain`. |

### `MeshDesc` (one `static_meshes.json` entry)

| Field | Default | Meaning |
|---|---|---|
| `id` | `meshes[<index>]` | Name in reports and errors. |
| `source` | required | A cooked mesh (`.fckd`) or a glTF (`.gltf`, `.glb`). All primitives load, with their node transforms, LOD0 only. |
| `transform` | identity | Column-major 4x4, object to world. A negative determinant (a mirror, DO27's `mirror_x`) flips the triangle winding. The normals stay consistent, so faces stay front-facing. |
| `instances` | `None` | Column-major 4x4 per instance, each placed as `transform × instance`. `None` = one placement at `transform`; `Some([])` = no placement. |
| `layer` | `None` | Tag (`terrain`, `setpiece`, `scatter`, `structure`, `dynamic`). `structure` and `dynamic` are never baked. |
| `static` | `true` | `false` = not baked. |
| `material` | `None` | `MaterialOverride`: `albedo`, `emissive` (cd/m²), `roughness`, `metallic`, each optional. It applies to every part of the mesh. |
| `approximate` | `false` | The placement is a coarse stand-in. It is baked like any other entry. |

### `InteriorDesc`

| Field | Default | Meaning |
|---|---|---|
| `id` | required | Name. |
| `min`, `max` | required | World box, metres. |
| `sky_open` | `false` | Open to the sky (a shaft). Its bricks are tagged with it but are not INTERIOR. They keep the mesh-aware leak control (relocation, validity). |
| `openings` | `false` | Has openings (a vehicle bay). Still INTERIOR as a whole in v0.2.0, so no light enters through the opening. |
| `spacing` | auto spacing | Probe spacing of the brick `auto_bricks` places over it. |

A brick whose box lies inside an interior's box (first interior in order) is tagged with it. The tag asks for
the brick's INTERIOR flag unless the interior is `sky_open`. The tag is in the bake plan (`--dry-run`, the bake
summary) and written into the brick's flags, so the `.fgiv` (v2) stores it (ADR-357). The test is the whole brick
box inside the interior, not its centre: a cluster brick that reaches out of the room stays outdoor.

### `LayoutDesc`

As in v0 (`terrain_spacing`, `levels`, `bricks`, `moment_resolution`, `max_distance`), plus:

| Field | Default | Meaning |
|---|---|---|
| `auto_bricks` | `None` | `AutoBricksDesc`: automatic detail bricks. |

`AutoBricksDesc`:

| Field | Default | Meaning |
|---|---|---|
| `spacing` | required | Probe spacing, metres. |
| `margin` | 2.0 | Each placement's world box grows by this. Overlapping or touching boxes merge until no two overlap. Each cluster gets one brick. |
| `max_probes` | 262144 | Most probes in all the automatic bricks (interiors and clusters). If they hold more, the cluster bricks' spacing grows by one factor until they fit in what the interiors leave, so the interiors keep their spacing. Only when the interiors alone leave fewer than 8 probes per cluster does every automatic brick coarsen by one common factor (ADR-357). Every box stays covered. `plan.auto_bricks.scale` (clusters) and `plan.auto_bricks.interior_scale` report the factors. Fewer than 8 per brick is an error. |
| `layers` | `[]` | Layers whose placements get bricks. Empty = every layer but `terrain`. |
| `interiors` | `true` | Also place one brick over each interior, at the interior's `spacing` (default: `spacing`). These bricks share `max_probes` with the clusters, which coarsen first. |

Brick order in the volume: explicit `bricks`, then interior bricks in description order, then cluster bricks
sorted by box. The order does not depend on the order of the meshes.

The renderer uploads each volume into one GPU storage binding: per probe 8 bytes of meta, 64 of SH and 4 per
moment texel (`moment_resolution²`), plus 4 bytes per terrain column and 48 per brick. `plan.volume_bytes` reports
the total. A layout above 128 MiB (wgpu's default `max_storage_buffer_binding_size`, and the renderer's mobile
minimum-spec limit, ADR-357) bakes as a split volume of up to four parts, each at most 128 MiB, cut in X and Z along
brick boundaries with a one-cell overlap band (`plan.split_parts` lists the bytes per part; part `k` is written to
`<stem>.part<k>.fgiv`; ADR-2618, `docs/formats/baked-gi.md`). Only a layout no split fits (a single brick above
128 MiB, or more than four parts) fails with `bake_volume_too_large`.

## DO27 `static_meshes.json` (version 1)

The bake reads `header` (`version: 1`, `units: "metres"`, `up: "+Y"`, `handedness: "right"`) and per entry `id`,
`transform`, `static` and `layer` (required), plus `source` (the glTF, required for a placed entry), `instances`
and `approximate`. It never reads `mesh` (the shell's cooked `.gltc`). Other keys are ignored (`cooked_lods`,
`mirror_x`, `mesh_variants`, `sources`, `placement`, `size_m`, `note`, ...). Skipped entries: `static: false`, a
`structure` or `dynamic` layer, and `instances: null` with a `placement` (scatter that Regia Engine generates at runtime,
dynamic for GI). An error names the field, for example `static_meshes.json: meshes[3].layer`.

## DO27 `interiors.json` (`do27-bake-interiors/1`)

The root is an object with `format` (checked) and an `interiors` list. Each entry has `id`, `aabb_min`, `aabb_max`,
and optionally `sky_open` (bool) and `openings` (any value that is not null or false counts as openings). Other
keys are ignored.
