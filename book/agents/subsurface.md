# Subsurface scattering

Light that enters skin or a leaf leaves it a few millimeters away. Shadow edges soften and take on the
color of what the light passed through: red on skin, green in a leaf. Screen-space subsurface scattering
gets this by blurring a material's diffuse light on screen with the material's diffusion profile, then
putting the albedo back, so pores, freckles and veins stay sharp.

## Turning it on

Two switches, both off by default:

- **The render profile** asks for it: `RenderProfile::subsurface_scattering`, or `render.set_subsurface
  { enabled }`. No preset turns it on yet.
- **The material** has the factors: `LabMaterial::subsurface`, the `subsurface` block of a `.material.ron`
  (format version 6), or `material.set_subsurface`.

| Field | Meaning | Default |
|---|---|---|
| `profile` | `off`, `skin` (d'Eon and Luebke's measured six Gaussians; red reaches furthest) or `foliage` (an authored two-Gaussian mix; green reaches furthest). | `off` |
| `radius_mm` (`radius` in `.material.ron`) | How far the light reaches, in millimeters on a world in meters. `0` takes the profile's own: 8.2 mm for skin, 4.5 mm for foliage. | 0 |
| `tint` | Share of the light that scatters per channel, 0..=1, linear. The rest stays where it landed. Black is no scattering. | white |

## How it works

1. Scattering materials draw first, in their own pass with two more color targets (the `SUBSURFACE`
   forward variant): the HDR image gets their specular, emission and the part of the diffuse light the
   tint keeps; one target gets the light that scatters, without the albedo; one gets the albedo and the
   reach, so materials of different reach can sit side by side.
2. Two 1D blurs (17 taps each) spread that light. A tap off the scattering surface or across a depth
   step takes the center's light instead, so light does not cross a silhouette and none is lost. The
   second blur multiplies the albedo back in and adds the result to the HDR image.
3. The forward pass loads that image and draws everything else over it. Blended surfaces in front of the
   skin therefore see the finished skin. Registered passes at `after_opaque` see it too.

A material with the [skin shading model](shading-models.md) swaps its pre-integrated falloff and blurred
normals for the blur, so the light does not scatter twice. Its transmission and second lobe stay.

## What it costs

Nothing unless both switches are on and a scattering material is visible. Such a frame takes three pooled
targets the size of the HDR image and draws two blur passes inside the screen bounds of the scattering
draws (`covered_permille` in the report). Pixels in those bounds off the material discard after one texture
read. The draws go through the CPU color loop, never the GPU-driven batches or the visibility buffer, and
each pixel shades once.

A first use mid-play never stalls: the material draws without the blur while its pipelines build
(`fallback`, `reason: pipelines_building`). A game-frame warm that sees the material builds them first.

## Where it falls back

The frame draws the material without the blur when it cannot scatter, and says why in
`render.subsurface`'s `frame.reason`:

| Code | Meaning |
|---|---|
| `profile_off` | The profile does not ask. |
| `no_material` | No draw in the frame scatters. |
| `no_hdr_target` | The frame has no HDR image (no post stack and no temporal upscale). |
| `msaa` | The frame is multisampled. |
| `forward_lite` | A Mobile lite tier. |
| `pipelines_building` | The pipelines are still compiling. |
| `no_visible_draw` | The frame's scattering draws are all culled. The targets stay held, so one coming back creates nothing. |

These materials never scatter and count as `unsupported`: a material graph, a transmissive or blended
surface, an unlit one, hair, a thin film, a lightmap receiver, a masked-decal instance, an eye, and a draw
with a render override. `material.subsurface` names the material-side ones in `unsupported`; the
others count in `renderer.unsupported` when they draw.

Lightmap receivers (known limit, audit v0.2.12): a mesh with a `LightmapReceiver` draws its baked
lightmap path and never the scattering pass, whatever the profile says; turning
`subsurface_scattering` on only raises `renderer.unsupported`. The scattering pass has no lightmap
binding (no `LIGHTMAP` + `SUBSURFACE` variant is built), so the shader's "bake holds the sun, drop the
live sun diffuse" branch under `SUBSURFACE` is not reached yet. To scatter a statue, a plant or a
character, leave that mesh out of the lightmap bake: it is then lit by the probes and the live lights
and scatters normally. Check with `material.subsurface { entity }` (`active: false`) and
`render.subsurface` (`frame.unsupported` counts the receiver).

Limits of working in screen space:

- A surface the camera cannot see sends no light: a thin edge shows less scattering than it should, and
  the back of an ear stays dark (the skin model's transmission term is the way to light it).
- Light crosses a pixel grid: a kernel under a few pixels wide loses its fine shape, and a straight
  edge along a diagonal looks slightly squarer than along an axis.
- Light crossing between two scattering materials takes the receiver's kernel and profile: a seam
  between skin and a leaf is plausible, not measured.
- On the screen kernel (the default), a surface seen at a grazing angle blurs over more of itself
  along its slope than the profile says; only the depth test narrows it. The tangent-plane kernel
  below fixes that.

## The kernel: screen or tangent plane

`RenderProfile::subsurface_kernel` (or `render.set_subsurface { "kernel": ... }`, ADR-3170) picks
where the blur lays its kernel:

| Kernel | What it does |
|---|---|
| `screen` | The default in every preset. The reach is the same number of pixels along both screen axes, as before: frames with scattering on do not move unless a game asks. |
| `tangent` | The kernel lies on the surface's tangent plane. Each pass measures how much surface one pixel spans along its axis from the depth of the neighboring pixels on the scattering surface (a facing surface spans its pixel width, one at θ from the view `1 / cos θ` of it), shrinks the reach on screen by that, and measures each tap's depth step from the tangent plane instead of from the center's depth. A grazing cheek or ear blurs over as much of itself as a facing one. |

The tangent plane comes from the depth buffer, not from the shading normal: a normal map's bumps would
bend the kernel by texture detail, and the scattering follows the geometric surface. The neighbor
used for the slope is the nearer one in depth, on the scattering surface, so a silhouette does not
tilt it. Switching the kernel is a uniform: no pipeline and no target is created.

Measured on lavapipe (`crates/regia_render/tests/subsurface_kernel_frame.rs`), a skin plate with a
hard light edge tilted away from the view, against the profile's response on the surface: see
ADR-3170 for the numbers at 0, 45 and 70 degrees.

## Grass and scatter: the foliage profile

- **Terrain grass** takes the foliage profile with the render profile's switch: on, the light the sun
  sends through a blade is the profile's transmission through the blade's thickness along the sun ray
  (`regia_render::subsurface::slab_transmission`; `TerrainBudget::foliage_subsurface`, which
  `FrameQuality::from_profile` sets from `subsurface_scattering`). `terrain.grass` reports
  `subsurface`; `terrain.set_grass { "debug_view": 4 }` shows the transmission itself. See
  [terrain grass](terrain-grass.md). Off, the grass renders byte-identical to a build without it.
- **Terrain scatter** (props from a `.scatter.ron`) draws as meshes, so its leaves scatter like any mesh
  leaf once their materials carry the factors: `ScatterField::set_class_subsurface` (or
  `terrain.set_scatter { "class": ..., "class_subsurface": "foliage" }`) puts the foliage profile on
  every material of a class, `off` restores the materials the field was built with. Such a class's
  instances leave the GPU-driven batches for the CPU color loop while the profile scatters: a draw
  per visible instance.

## Why a material does not scatter

`render.subsurface` counts the scene's mesh entities whose material asks to scatter
(`materials.asking`), how many this path can scatter (`materials.scatterable`), and the rest by reason
(`materials.unsupported_by`, `{ code: count }`, only the codes that occur). The frame-side counts are
in `frame.unsupported` (with the frame hookup of ADR-3170, by reason in `frame.unsupported_by`).
The codes are stable:

| Code | Meaning |
|---|---|
| `eye` | The eye model keeps its own block. |
| `material_graph` | A material graph shades the surface. |
| `transmissive` | A transmissive surface. |
| `blended` | A blended surface. |
| `unlit` | An unlit material. |
| `hair` | The hair model. |
| `thin_film` | A thin film. |
| `masked` | An alpha-masked instance drawn through the mask path (frame only). |
| `lightmap` | A lightmap receiver (frame only). |
| `forward_lite` | A Mobile lite forward frame (frame only). |
| `render_override` | A render override, x-ray or highlight, is on the draw (frame only). |

## MSAA

A multisampled frame does not scatter (`reason: msaa`): the scattering pass would need multisampled
light and albedo targets and a resolve that keeps each sample's reach, and the blur would read a
multisampled depth. ADR-3170 records why this stays a fallback: no preset renders multisampled (only
a host that picks `AntiAliasing::Msaa4x` does), the Mobile tiers that once used it skip the scattering
pass anyway (`forward_lite`), and the desktop presets resolve edges with TAA or SMAA. The cost would buy
nothing a shipped profile uses. A test holds both: no preset is multisampled, and a multisampled frame
reports `msaa` and draws the bytes of the frame without the feature.

## Agent methods

| Method | What it does |
|---|---|
| `render.subsurface` | `{ enabled, kernel, materials: { asking, scatterable, unsupported_by }, frame: { requested, drawn, fallback, unsupported, passes, covered_permille, reason } \| null }` |
| `render.set_subsurface { enabled?, kernel? }` | Switches the active profile's knob and picks the kernel (`screen` or `tangent`); at least one. Returns `{ before, after, kernel: { before, after }, note }`. A preset change turns both back. |
| `material.subsurface { entity }` | `{ entity, enabled, active, unsupported, profile, radius_mm, reach_mm, tint, renderer }` |
| `material.set_subsurface { entity, profile?, radius_mm?, tint? }` | Edits the factors. Omitted fields keep their value, and the entity gets its own copy of the material. |

`F0002` for a value out of range, an unknown kernel, or a `render.set_subsurface` with neither field. `F0003` when the entity has no `MeshRenderer` or the app has no
`RenderSettings`. `render.graph` lists the `subsurface_draws` and `subsurface_blur` nodes ahead of `forward`.

## How it was checked

`crates/regia_pt/tests/subsurface_parity.rs` holds the frame to the full radial profile integrated in
world space over the light the path tracer computes, on a skin plate with a hard light edge and on a
nose-sized sphere across its terminator. That test extracts with the tracer's own subsurface off; the tracer's
Burley transport (ADR-2725, `ExtractOptions::subsurface`) is described in
[Material graphs and subsurface in the reference tracer](pt-material-graphs.md).
`crates/regia_render/tests/subsurface_frame.rs` holds the edge to the analytic response at every pixel and
checks that energy stays, albedo detail stays sharp, light stays off silhouettes and off non-scattering
neighbors, and that a frame that does not scatter is byte for byte the frame without the feature.
