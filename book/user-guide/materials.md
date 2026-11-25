# Materials, textures and import

<!-- index: Material -->
<!-- index: Standard shader -->
<!-- index: ORM map -->
<!-- index: Subsurface scattering -->
<!-- index: Transmission -->

Materials are `shader + parameters`, stored as `.material.ron`. Meshes, textures and materials are
authored in neutral formats (glTF, PNG, WAV) and **cooked** to platform data by the import pipeline.
The task procedure (a material, streaming, a graph, skin and eyes, a skeleton) is
[Materials](../how-to/picture/materials.md). Shading models and skinning stay on their own pages.

## A material

A `.material.ron` is `shader + parameters`, optionally layered. The sample below is the frozen v1
document `docs/formats/material.example.ron`: a `format_version`, a `shader`, a nested `base` PBR
block, and up to four masked `layers`. The loader accepts `shader: Standard` and bare optionals
(`name: "…"`, `clearcoat: (…)`).

```ron,material
(
    format_version: 1,
    name: "LayeredPaint",
    shader: Standard,
    base: (
        base_color: (0.45, 0.42, 0.38, 1.0),
        metallic: 0.0,
        roughness: 0.85,
        base_color_map: "textures/concrete_albedo.png",
        normal_map: "textures/concrete_normal.png",
        orm_map: "textures/concrete_orm.png",
        emissive: (0.0, 0.0, 0.0),
    ),
    layers: [
        (
            name: "dust",
            mask: "textures/dust_mask.png",
            base_color: (0.7, 0.68, 0.6, 1.0),
            roughness: 0.95,
            metallic: 0.0,
            triplanar: false,
        ),
        (
            name: "wetness",
            mask: "textures/wet_mask.png",
            base_color: (0.2, 0.22, 0.25, 0.6),
            roughness: 0.15,
            metallic: 0.0,
            triplanar: true,
        ),
    ],
    clearcoat: (factor: 0.35, roughness: 0.08),
    render_state: (alpha_mode: opaque, queue_offset: 0),
)
```

Materials are layered (`docs/specs/26` § 4); the schema is `docs/formats/material.ron.schema.json`.
Texture slots are bare strings by project path (a GUID string is an already-imported asset);
the cooker emits the runtime texture (KTX2 for larger maps, RGBA8 for small ones).

## Hard surfaces (format version 2)

Version 2 adds an optional `hard_surface` block for vehicles and structures. A v1 file loads
as v2 with multi-scatter and specular occlusion turned on and every other factor off.
`docs/formats/material-v2.example.ron` is a full block. The four masked layers above stay
authored data; this block does not shade them.

```ron,material
(
    format_version: 2,
    name: "HullPaint",
    shader: Standard,
    base: (
        base_color: (0.42, 0.45, 0.48, 1.0),
        metallic: 0.0,
        roughness: 0.55,
        base_color_map: "textures/hull_albedo.png",
        normal_map: "textures/hull_normal.png",
        orm_map: "textures/hull_orm.png",
        emissive: (0.0, 0.0, 0.0),
    ),
    clearcoat: (factor: 0.4, roughness: 0.06),
    anisotropy: (strength: 0.0, direction: (1.0, 0.0)),
    hard_surface: (
        detail: (
            albedo: "textures/hull_detail_albedo.png",
            normal: "textures/hull_detail_normal.png",
            roughness: "textures/hull_detail_roughness.png",
            mask: "textures/hull_detail_mask.png",
            uv_scale: (4.0, 4.0),
            albedo_strength: 1.0,
            normal_strength: 1.0,
            roughness_strength: 0.6,
            fade_start: 2.0,
            fade_end: 18.0,
        ),
        parallax: (
            height_map: "textures/hull_height.png",
            scale: 0.04,
            min_steps: 4,
            max_steps: 12,
        ),
        edge_wear: (
            source: curvature,
            curvature_map: "textures/hull_curvature.png",
            intensity: 0.85,
            roughness_bias: -0.15,
            metallic_bias: 0.7,
            tint: (0.75, 0.76, 0.78),
            bias: 0.5,
            contrast: 4.0,
        ),
        multi_scatter: true,
        specular_occlusion: true,
    ),
    render_state: (alpha_mode: opaque, queue_offset: 0),
)
```

Detail albedo is a multiply about mid-gray (0.5 leaves the base alone) and fades out between
`fade_start` and `fade_end`. `fade_end <= fade_start` means no fade. Parallax reads
`LabMaterial::height_map` (1 = the geometric surface) and runs only on Desktop High and
Desktop Ultra, and only when `scale` is above zero. The profile gate is `LabFrame.pcss.z`.
`quality.w` stays the shadow-noise index. Edge wear reads
`LabMaterial::curvature_map` (`source: curvature`, 0.5 is flat, above 0.5 is a convex edge)
or the ambient-occlusion texture (`source: ao`). The paths in the file are stored for the
cooker. `LabMaterial::from_authored` copies the numbers and leaves the texture slots empty.
`LabMaterial::from_authored_with_maps` also copies resolved images, and the mesh bind group
uploads them. Clearcoat and anisotropy stay on the existing top-level fields.

## Glass, IOR and emission strength (format version 3)

Version 3 adds the glTF material extensions DO27's hero assets use: `emissive_strength`,
`orm_packed` and `ior` on `base`, an optional `transmission` block (`KHR_materials_transmission`
and `KHR_materials_volume`) and `alpha_cutoff` on `render_state`. A v2 file loads as v3 with
emissive strength 1, packed ORM, IOR 1.5 (F0 0.04, as before), no transmission and cutoff 0.5.
`docs/formats/material-v3.example.ron` is a canopy:

```ron,material
(
    format_version: 3,
    name: "M_Canopy",
    shader: Standard,
    base: (
        base_color: (1.0, 1.0, 1.0, 1.0),
        roughness: 0.15,
        emissive: (0.0, 0.0, 0.0),
        ior: 1.52,
    ),
    transmission: (
        factor: 0.92,
        thickness: 0.012,
        attenuation_color: (0.75, 0.88, 0.84),
        attenuation_distance: 0.08,
    ),
    render_state: (alpha_mode: opaque, cull_mode: none, queue_offset: 0, alpha_cutoff: 0.5),
)
```

A transmissive surface draws over what is behind it in two blends: the frame behind is multiplied
by `factor · (1 − F) · base color · volume` (Schlick Fresnel at the IOR; Beer–Lambert through
`thickness`, longer at grazing views), then the surface's reflection and remaining diffuse are
added. Clear glass with a tint is a white base color plus an attenuation color. The glTF import
writes these fields (`docs/formats/gltf-conventions.md`).

A material graph has no transmission block, but a host can set `LabMaterial::transmission` (and
the IOR and volume fields) on a graph material. It then draws in the same two blends, and the
multiply uses the graph's own base color, clamped to 0..1 (ADR-347). A transmissive graph pane has
no refraction pass and is not part of the glass front layer described below, and its translucent
shadow does not take the graph's color yet.

Refraction is opt-in: set `RenderProfile::screen_space_refraction` (or
`FrameQuality::screen_space_refraction`) on an HDR profile without MSAA (Desktop High, Ultra). The
forward pass then copies the opaque frame before the blended draws, and glass with a `thickness`
shows that copy bent by its normal across the view × thickness × (IOR − 1). A bend that leaves the
screen, or lands on something in front of the glass, shows the unbent background. A thin wall
(thickness 0) bends nothing, and with the flag off every frame is unchanged. Thickness is in the
mesh's units: DO27's 12 mm X12 canopy bends the view by millimeters, a glass ball by a lot.

With the refraction copy, panes seen through other panes are composited in depth order: a small
pass records the nearest transmissive pane's depth and transmittance per pixel, and a pane behind
it adds its reflection through that transmittance (exact for two layers, approximate beyond). The
far side of a dark canopy, or the inner sheet of a two-sheet pane, no longer shows its reflection
over the near side at full strength. Without the copy (Desktop Low, Deck, mobile) each pane adds
its reflection right after its own multiply, as before.

Roughness is floored at perceptual 0.045 (GGX α 0.002025, as in Filament): a polished pane or
lacquer keeps a tight sun highlight instead of the perceptual-0.21 lobe the old α ≥ 0.045 floor gave.

`regia upgrade-project` rewrites v1 and v2 materials. The agent page is
[Hard surfaces](../agents/hard-surface.md).


## Thin film (format version 4)

Version 4 adds an optional `thin_film` block (`KHR_materials_iridescence`): a thin dielectric film
over the base specular, whose reflection shifts color with the view angle and the film's thickness. A
v3 file loads as v4 with no film. `docs/formats/material-v4.example.ron` is a feather:

```ron,material
(
    format_version: 4,
    name: "M_Feather",
    shader: Standard,
    base: (
        base_color: (0.02, 0.03, 0.05, 1.0),
        roughness: 0.35,
        emissive: (0.0, 0.0, 0.0),
    ),
    thin_film: (
        factor: 0.9,
        ior: 1.56,
        thickness_min: 180.0,
        thickness_max: 520.0,
    ),
)
```

`factor` is the share of the surface the film covers (`factor_map`'s R multiplies it). The film is
`mix(thickness_min, thickness_max, thickness_map.g)` nanometers thick, or `thickness_max` without a
map. A block that omits a field takes the extension's default: factor 0, IOR 1.3, 100 to 400 nm.
[Thin film](../agents/thin-film.md) has the details and what the film does not combine with.

## Weather response (format version 5)

<!-- index: Material weather -->

Version 5 adds an optional `weather` block (spec 99): how rain and snow change this material. A
v4 file loads as v5 with the default response. Leaving the block out is that default: porosity
derived from roughness and metallic, wetness and snow at full strength, no puddles. A bare
`weather: ()` is the same response. The frozen sample is `docs/formats/material-v5.example.ron`.

| Field | Default | Effect |
|---|---|---|
| `porosity` | derived | `0..=1`. How much a wet surface darkens. Absent derives `(1 - metallic) · clamp((roughness - 0.25) / 0.5, 0, 1)`. |
| `wet` | 1 | Scales wetness. 0 means rain never wets the surface. |
| `snow` | 1 | Scales snow cover. 0 means snow never settles. |
| `puddles` | false | Flat, upward parts may hold puddles. |

Every authored number has to be finite and in `0..=1`.

## Subsurface scattering (format version 6)

Version 6 adds an optional `subsurface` block: the diffuse light of skin or leaves spreads a few
millimeters under the surface, which softens shadow edges and tints them with the profile's color. A
v5 file loads as v6 with no scattering. `docs/formats/material-v6.example.ron` is skin:

```ron,material
(
    format_version: 6,
    name: "M_Skin",
    shader: Standard,
    base: (
        base_color: (0.72, 0.53, 0.45, 1.0),
        roughness: 0.45,
        emissive: (0.0, 0.0, 0.0),
    ),
    subsurface: (
        profile: skin,
        radius: 6.0,
        tint: (1.0, 0.95, 0.9),
    ),
)
```

`profile` is `skin` or `foliage`. `radius` is the reach in millimeters on a world in meters; `0` takes
the profile's own (about 8 mm for skin). `tint` is the share of the light that scatters per channel:
black is no scattering. The render profile must also ask for it, and no preset does yet.
[Subsurface scattering](../agents/subsurface.md) has the details and what it does not combine with.

## Rough transmission (format version 7)

Version 7 adds an optional `transmission_roughness`: how far a transmissive surface spreads what it
shows behind it, from 0 (clear glass) to 1. Frosted glass, a scuffed visor and ice use it. A v6 file
loads as v7 with clear glass. `base.roughness` keeps shaping the reflection alone. A glTF file's
roughness does not set it. `docs/formats/material-v7.example.ron` is a frosted pane:

```ron,material
(
    format_version: 7,
    name: "M_FrostedPane",
    shader: Standard,
    base: (
        base_color: (0.95, 0.97, 1.0, 1.0),
        roughness: 0.2,
        emissive: (0.0, 0.0, 0.0),
    ),
    transmission: (factor: 0.9),
    transmission_roughness: 0.35,
)
```

[Rough transmission](../agents/rough-transmission.md) has the details and what it does not combine with.

## Hair cards

A runtime `LabMaterial` can switch on strand shading for cards of hair, brows, lashes or fur
(`LabMaterial::hair`): two shifted highlight lobes along the mesh tangent, light through the
strands, a root-to-tip colour and alpha ramp, hashed or sorted alpha, and alpha-tested shadows.
It is not part of the `.material.ron` format yet. See [Hair cards](../agents/hair-cards.md).

## glTF import

Drop `.glb`/`.gltf` under `assets/` and it cooks to a mesh (and skeleton/clips, materials, textures).
Conventions for LOD groups (`Stem_LOD0…2`), sockets (`socket_<id>` empties), ORM texture packing and
`Rem`/`A52` material variants are in `docs/formats/gltf-conventions.md` and the
[agent guide](../agents/gltf-conventions.md).

```console
$ regia preview models/DamagedHelmet.glb --project my_game
```

`regia preview` cooks (incrementally) and shows the mesh in a window — the fastest way to check an
import before it lands in a level.

## Cook

```console
$ regia cook --project my_game --platform linux
```

`cook` converts source assets into the cooked cache; `--clean` rebuilds from scratch, `--quality
fast|final` turns pak lz4 compression off or on. Packaging runs the cook automatically (see
[Packaging](packaging.md)).

## Cooked textures at runtime

A shipped game reads its materials from the pak, never from the glTF. The cook writes, per glTF,
one mesh per primitive, one `.material.ron` v3 payload per material, every image as a KTX2 chain, and
a `model` part list naming each primitive's material. `regia_render::cooked_model::load_cooked_model(&mut
asset_server, gltf_id)` rebuilds the draws from those (LOD 0, MikkTSpace tangents):

| Slot | Desktop cook | Android cook | Sampled as |
|---|---|---|---|
| base color, emissive | BC7 sRGB | ASTC 4×4 sRGB | sRGB (decoded by the GPU) |
| packed ORM | BC7 linear | ASTC 4×4 | linear |
| normal | BC5 (X, Y; Z rebuilt) | ASTC 4×4 normal mode (X in RGB, Y in A) | linear |

The full mip chain is uploaded as it is (about a quarter of the RGBA8 memory); with texture
streaming on (`RenderProfile::texture_streaming`) the cooked chain streams by need like an RGBA8
map, a few block rows a frame. A color slot is always
viewed as sRGB and a data slot as linear, whatever the file says. On a GPU that cannot sample the
pak's block format the renderer decodes it to RGBA8 on the CPU (BCn, ASTC and ETC2 alike; 4× the
memory or more, one warning), so cook the pak for the platform you ship
([Android texture compression](../agents/texture-compression.md)). Power-of-two maps
cook to ASTC 4×4 (or 8×8 for masks) because the GPU needs whole blocks.

glTF `doubleSided` (`cull_mode: none`) is per material: that material's draws, shadows and
motion vectors skip back-face culling, including a material graph. The forward shader flips
the shading normal on a visible back face: it shades with the front's tangent frame negated, so
its normal-mapped normal is the front's reversed, as for the other side of a thin sheet. `LabScene::double_sided` still turns culling off
for the whole frame. A `.matgraph` does not carry the flag; set `LabMaterial::double_sided`.
The cull mode is part of the GPU-driven batch key, so double-sided meshes batch and instance in
the depth prepass, shadow maps, forward pass and visibility buffer like single-sided ones, and
`FrameRenderer::prewarm` builds their cull-none pipelines when a level mesh needs them.

Most exported hulls are flagged `doubleSided` although they are closed, where the back faces only
show from inside. The glTF importer can drop the flag on materials used only by closed,
consistently wound, opaque meshes: `.meta` settings `(drop_double_sided_on_closed: true)`. It is
off by default, because a camera inside the volume (a cockpit inside its hull) would see the
inside culled. `cargo run --release -p regia_asset_pipeline --example closed_double_sided --
<dir> [--list]` reports what the setting would change in a content tree.

`cargo run -p lookdev --bin cooked_runtime -- --asset hero/buildings/silo` cooks a DO27 hero
asset from `REGIA_DO27_CONTENT`, mounts the pak without the source and prints the cooked/direct
frame difference and texture memory.

## Provenance and `.meta`

Every imported asset has a `.meta` sidecar (schema `docs/formats/meta.schema.json`) recording import
settings, format versions, and — for agent-authored content — provenance (`tool`, `client`,
`session`, `reviewer`). The provenance policy is in `Regia.toml` `[provenance]`; `cook --quality
final` can refuse assets that have not been reviewed (specs/23 § 5).

## Texture sets: a folder of maps becomes a material

Artists exporting PBR maps named `<base>_<suffix>.png` can turn a folder into materials
without hand-writing RON. The detector groups maps by folder and base name using one shared
suffix table (`regia_asset::texture_sets::TEXTURE_SUFFIX_TABLE`): `BaseColor`/`Albedo`/
`Diffuse`/`Color`, `Normal`/`N`/`Nrm`/`NormalGL`/`NormalDX`, `Roughness`, `Metallic`,
`AO`, `ORM`/`ARM`/`RMA`/`MRA` (channel orders documented there), `Height`/`Displacement`,
`Emissive`/`Emission`, `Opacity`/`Alpha`/`Mask`.

```sh
regia assets texture-sets assets/textures/rocks          # dry run: what would be written
regia assets texture-sets assets/textures/rocks --write  # write .meta and .material.ron
```

`--recursive` descends into sub-folders (a set never spans folders); `--json` prints the report
for tools. With `--write` each set becomes `<base>.material.ron` in its folder, naming the set's
texture ids; an existing material is never overwritten (`material_exists` in the report).
Members' `.meta` settings are set so cooks agree: base colour and emissive sRGB, everything
else linear; a `_NormalDX` map records `normal_green: directx` and the cook flips its green
channel (at cook, never on disk); an `_RMA`/`_MRA` map records its channel order and the cook
swizzles it to `ORM`. An opacity map switches the material to alpha cutout.

The report never guesses: two files claiming one slot are a reported conflict, files with an
unknown or ambiguous suffix (e.g. `_RM`) are reported as leftovers, and maps the standard
material has no slot for yet (separate roughness/metallic until the packed-ORM packer lands;
a separate opacity map, whose alpha the base colour map carries) are listed unreferenced with
the reason. The agent surface is [`assets.texture_sets`](../agents/assets.md). The project's
suffix table and default green convention extend in `Regia.toml`:

```toml
[import.texture_sets]
normal_green = "directx"        # project default for normals without a GL/DX suffix
[import.texture_sets.suffixes]
base_color = ["diff"]           # extra words per slot, merged with the defaults
```
