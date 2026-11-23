# Studio environment (gradient or HDR image)

A showroom or look-dev frame lights its materials from an environment. The two-tone studio
(`FrameRenderer::set_studio_sky`) is one sky colour above the horizon and one ground colour below
it, so a metal sphere reflects a flat sky. The studio environment adds two richer sources that
feed the **same** image-based light, the diffuse irradiance and the specular prefilter:

- **gradient**: radiance ramps from the horizon colour to the zenith colour, with a flat
  ground colour below the horizon and no sun disc (add the sun as a directional light);
- **equirect**: a caller-supplied HDR equirectangular image.

The sky draw shows the same environment as the background unless a separate background is set
(below). On frames without the physical atmosphere (Desktop Low and Medium, Mobile, or
`physical_atmosphere` off) an installed studio environment is the frame's sky and image light. On
Desktop High and Ultra the atmosphere's sky and bake are drawn over it by default, as in v0.2.8 and
earlier; the environment authority below opts into the environment alone. The two-tone call is
unchanged.

## Which draws the sky: the environment authority

A frame on Desktop High or Ultra (or on a planet other than Earth) has the physical atmosphere. With a
studio environment installed as well, `EnvironmentAuthority` says which of the two is the sky and the
image light:

| `EnvironmentAuthority` | Agent `authority` | The frame |
|---|---|---|
| `Atmosphere` (default) | `"atmosphere"` | The atmosphere's LUT sky is drawn and its IBL bake is written over the environment's maps: the frame is lit by a mix of the two. The frames of v0.2.8 and earlier, kept so an exposure fitted on them holds. |
| `Studio` (opt-in) | `"studio"` | The environment is the sky and the image light; the atmosphere draws neither. A studio background (`set_studio_background`) shows. |

```rust,wrap
# use regia_render::{FrameRenderer, RenderDevice};
# let renderer: &mut FrameRenderer = todo!();
# let device: &RenderDevice = todo!();
# let gradient = regia_render::StudioGradient::new([0.0; 3], [0.0; 3], [0.0; 3]);
use regia_render::EnvironmentAuthority;

renderer.set_environment_authority(EnvironmentAuthority::Studio); // before the install
renderer.set_studio_gradient(&device, Some(&gradient))?;
```

Frames without the atmosphere, or without a studio environment, are the same under both. Set the
authority before installing the environment: under the atmosphere its bake writes into the installed
maps, so after switching to `Studio` once such frames were drawn, set the environment again (the same
parameters rebuild it). The agent's `authority` parameter is applied before the call installs the
environment, so one call does both. On the pinned sunless studio frame (lavapipe, 8-bit, Desktop High
at its defaults, `crates/regia_render/tests/studio_frame_pin.rs`) the default reads a horizon of
[231, 237, 242] and a frame mean of [171, 191, 217]; `Studio` reads [158, 163, 168] and [159, 175, 200].

## From a host

```rust,wrap
# use regia_render::{FrameRenderer, RenderDevice};
# let renderer: &mut FrameRenderer = todo!();
# let device: &RenderDevice = todo!();
# let (width, height): (u32, u32) = todo!();
# let texels: Vec<[f32; 3]> = todo!();
use regia_render::{StudioEquirect, StudioGradient};

let gradient = StudioGradient::new(
    [2400.0, 4200.0, 8000.0], // zenith, cd/m²
    [2600.0, 2800.0, 3000.0], // horizon
    [600.0, 600.0, 600.0],    // ground
);
let rebuilt = renderer.set_studio_gradient(&device, Some(&gradient))?; // Ok(true)
let again = renderer.set_studio_gradient(&device, Some(&gradient))?; // Ok(false): kept

let image = StudioEquirect::new(width, height, texels)?; // linear cd/m², top row up
renderer.set_studio_equirect(&device, Some(&image));
renderer.set_studio_gradient(&device, None)?; // the default IBL again
```

`StudioGradient::falloff` shapes the ramp: the blend toward the zenith colour is
`sin(elevation) ^ falloff` (default 1; above 1 holds the horizon colour longer, below 1 reaches the
zenith colour sooner; 0.05 to 16). A radiance above `SKY_IBL_SCALE` (9000 cd/m²) is kept, not
clipped. The maps are baked in linear radiance and stored as half floats, so a smooth ramp does not
band in a mirror. The bake is deterministic. It runs only when the parameters differ from the
installed environment's: `StudioGradient::cache_key` and `StudioEquirect::cache_key` are 64-bit
keys of the parameters (the image's size and every texel), and `FrameRenderer::studio_environment_key`
reports the installed one (`None` for any other IBL, including the two-tone studio). A rebuild is
about three million environment samples and no per-frame cost.

The equirect layout: column `x` of `width` is azimuth `atan2(z, x)` from -π to π (the image centre
looks along +X); row 0 is straight up. Texels are linear radiance. Non-finite or negative texels are
refused by `StudioEquirect::new`.

## `render.set_studio_environment`

| Parameter | For | Meaning |
|---|---|---|
| `kind` | all | `"gradient"`, `"equirect"` or `"none"` |
| `zenith`, `horizon`, `ground` | gradient | `[r, g, b]` radiance, cd/m², each >= 0 |
| `falloff` | gradient | 0.05 to 16, default 1 |
| `path` | equirect | an OpenEXR file; relative paths join the project root |
| `asset` | equirect | a cooked HDR texture (a cooked `.exr` or `.hdr`, BC6H, RGBA16F or ASTC HDR) by project-relative path or asset id, in place of `path` (not both). A cube map is resampled to an equirect four faces wide. |
| `scale` | equirect | multiplies every texel, default 1; must remain finite and positive as `f32` |
| `authority` | all | `"atmosphere"` (the default) or `"studio"`: see the environment authority above; applied before the install |

Returns `{ kind, rebuilt, key, base_width, authority, width?, height?, replaced_samples? }`. `rebuilt` is `false` when the
installed environment already had these parameters. `key` is the cache key as 16 hex digits, `null`
for `none`. `replaced_samples` counts non-finite or negative image samples read as 0, including
products that overflow after scaling. A cube's resampled image is checked against the 8192 × 4096
texel cap before resampling; at four faces wide and two high, a cube face edge may be at most 2048 texels.
Errors: `F0002`
(a missing or malformed parameter, an unreadable file or an image that is too large, `path` and `asset` together, or an
`asset` that is not a cooked HDR texture: the message starts with `hdr_texture_ktx2`, `hdr_texture_format`,
`hdr_texture_shape` or `hdr_texture_data`), `F0003` (no `RenderDevice`, `RuntimeRenderer` or, for `asset`, `AssetServer` in
this app), `F0016` (no manifest entry for the `asset`). A cooked asset gives the same environment as its raw file within the
cook's block-compression error (BC6H: 6 % of a texel's brightest channel; the diffuse irradiance, which averages many texels,
agrees far more closely).

## Check it

1. `render.set_studio_environment { "kind": "gradient", "zenith": [2400, 4200, 8000], "horizon": [2600, 2800, 3000], "ground": [600, 600, 600] }`
2. `play.step`, then `seeing.screenshot` with a polished metal sphere in view.
3. Look for: the sphere's upper half getting brighter and bluer toward the top, a flat darker lower
   half, the horizon line across its middle. A flat upper half means the two-tone studio is
   still installed (`key` is `null` in a later call's result).
4. Repeat the call with the same parameters: `rebuilt` is `false`.

## Sharper mirrors: the environment's base resolution

The mirror mip of a studio environment is 256 × 128 by default. A hot source smaller than a texel
(a sun disc, a small softbox in an HDR image) averages down with what is around it, so a mirror shows
it several stops too dim. Keep a larger base when a frame's reflections matter:

```rust,wrap
# use regia_render::{FrameRenderer, RenderDevice};
# let renderer: &mut FrameRenderer = todo!();
# let device: &RenderDevice = todo!();
# let image: regia_render::StudioEquirect = todo!();
use regia_render::StudioEnvResolution;

renderer.set_studio_env_resolution(StudioEnvResolution::Studio); // 1024 × 512
renderer.set_studio_equirect(&device, Some(&image));
let info = renderer.studio_env_info().expect("installed");
assert_eq!((info.base_width, info.base_height), (1024, 512));
```

| `StudioEnvResolution` | Base width |
|---|---|
| `Standard` (default) | 256 |
| `Studio` | 1024, or the image's width when that is smaller |
| `Source` | the image's width (1024 for a gradient) |
| `Width(n)` | `n`, no wider than the image |

The result is rounded down to a power of two, held to the budget (`set_studio_env_budget`, default 4096,
1024 on Android) and to the device's texture limit; a smaller source than 256 wide is enlarged to 256 as
before. The chain halves down to 32 wide, one mip per roughness step, so a 1024 base has six mips where
256 has four, and each step of roughness is narrower. Every mip is filtered from a solid-angle-weighted
pyramid of the image, reading each sample at the level it covers (filtered importance sampling), so a
hot disc spreads over a rough lobe without losing energy and without speckle.

| Base | Mirror mip | Chain, GPU bytes with the irradiance maps |
|---|---|---|
| 256 | 256 × 128 | about 0.4 MB |
| 1024 | 1024 × 512 | about 5.6 MB |
| 2048 | 2048 × 1024 | about 22 MB |
| 4096 | 4096 × 2048 | about 89 MB |

The setting applies to the next `set_studio_gradient` or `set_studio_equirect`; setting the same
environment again rebuilds it at the new base. The bake runs on the caller's thread and scoped worker
threads; it costs nothing per frame. On the agent protocol pass `resolution` (`"standard"`, `"studio"`,
`"source"`) or `base_width`, and `budget`, to `render.set_studio_environment`.

`render.get_studio_environment` reads the effective resolution:

<!-- not-a-request -->
```json
{
  "key": "8c1f3b6a00d4e711",
  "requested": { "resolution": "studio", "width": null, "budget": 4096 },
  "effective": { "width": 1024, "height": 512, "mips": 6 },
  "source": [4096, 2048],
  "gpu_bytes": 5611520,
  "ground": { "plane": null, "active": false },
  "background": "environment",
  "authority": "atmosphere"
}
```

`effective` is `null` before the first frame on the default environment, and reports the IBL in use
whatever it is (the default procedural sky is 64 × 32). `source` and `gpu_bytes` are `null` for an IBL
that is not a gradient or equirect environment.

## The image light's ground plane

An image is a whole sphere of light, so an object lit by one alone takes the image's lower half on
its underside and far sides, where a real set has a floor. `render.set_environment_ground` gives the
image light that floor:

```rust,wrap
# use regia_render::{FrameRenderer, RenderDevice};
# let renderer: &mut FrameRenderer = todo!();
# let device: &RenderDevice = todo!();
use regia_render::EnvGroundPlane;

renderer.set_environment_ground(Some(EnvGroundPlane {
    height: 0.0,
    radius: None, // Some(meters) for a disc
    center: [0.0, 0.0],
    albedo: [0.18, 0.18, 0.18],
    sun_bounce: true,
}))?;
```

| Parameter | Meaning | Default |
|---|---|---|
| `enabled` | `false` removes the plane | on |
| `height` | world height of the plane | 0 |
| `radius` | disc radius, meters; omit for an unbounded plane | unbounded |
| `center` | `[x, z]` of the disc | `[0, 0]` |
| `albedo` | `[r, g, b]` linear, 0..1 | 0.18 gray |
| `sun_bounce` | the plane also shows the frame's sun | `true` |

On every surface above it, the part of the image the plane hides gives way to the plane's own radiance
(its albedo over the light the upper half of the image and, with `sun_bounce`, the sun send onto it): in
the diffuse term through an analytic form factor, and in reflections where the reflected ray lands on
the plane. The plane costs a few instructions per pixel and no new map.

- It changes **only the image light**. The sun's direct light and the local lights are not touched.
- It is not drawn. A floor the camera should see is a mesh of the scene.
- It takes no shadows. The plane under an object stays lit, so its own shadow on the floor does not
  darken the underside; screen-space AO covers the contact.
- It applies to `set_studio_gradient`, `set_studio_equirect`, `set_uniform_sky` and `set_studio_sky`.
  The default procedural sky and the planet atmosphere's IBL carry no lower-hemisphere map: a plane set on
  them changes nothing and `render.get_studio_environment` reports `ground.active: false`.
- A surface below the plane sees none of it.
- A frame with a plane, or with an image-light debug view, draws the full forward shader on a Mobile tier
  that asks for the lite path: the lite shader has no such term (`tier` in `render.get_forward_lite` shows it).
- On a profile with `gi_probes` (Desktop High and Ultra) the probes hold the plane themselves: a probe ray that
  meets no surface and lands on the plane reads the plane, not the image's lower half (ADR-5716). A ground mesh
  the probes trace hides the plane behind it, so a scene with both is lit by the mesh, once. Setting, moving or
  removing the plane restarts the probes' light (it settles again within about a second). A probe that
  stands on the plane or under it reads the plane below it too, so the underside of an object resting on the
  plane takes the plane's light and not the image's lower half. The probes'
  reflection lobe (probe specular) reads the same probes, so a metal object reflects the plane's light where it
  reflects straight down. A baked volume keeps the form-factor swap of ADR-2000.

## Seeing what the environment lights

Two debug views show this light apart from the rest of the frame. `render.set_debug_view` sets them
on the runtime (the editor's `viewport.render` takes the same names as `debug_view`), and
`render.get_debug_view` lists the modes.

| Mode | Shows |
|---|---|
| `image_light` | Only the image-based light, diffuse and specular with their occlusion, through the frame's exposure: no sun, no local lights, no emission |
| `image_light_occlusion` | What occludes it: **red** diffuse visibility, **green** specular visibility, **blue** the share of the image the ground plane replaced |

Both draw through the frame's exposure and tonemap, so the occlusion view's channels read as shades,
not exact values; `FrameRenderer::request_hdr_capture` returns them over the exposure. Probe GI and
baked GI replace the sky term where they cover, and the view shows what replaced it. `DebugViewMode`
has both as `ImageLight` and `ImageLightOcclusion`; a host sets the `DebugView` resource, or
`FrameRenderer::set_image_light_view` when it drives the renderer itself.

`render.set_debug_view { mode, tile_m?, target_px_per_m?, tolerance? }` sets every other mode of
`render.get_debug_view` the same way. The optional numbers apply to two modes only: `tile_m` to
`uv_checker` (meters per checker cell), and `target_px_per_m` and `tolerance` to `texel_density` (the
target density and the allowed relative deviation); the other modes ignore them. Their defaults and
colors are in [glTF hierarchy](gltf-hierarchy.md). An unknown `mode` is `invalid_params` with the list of
names.

## Lighting from one image, showing another

The sky draw's background can be chosen apart from the lighting: a solid color for a product shot, a
gradient, or a second image behind a subject lit by an HDR image.

```rust,wrap
# use regia_render::{FrameRenderer, RenderDevice};
# let renderer: &mut FrameRenderer = todo!();
# let device: &RenderDevice = todo!();
# let hdr_light: regia_render::StudioEquirect = todo!();
use regia_render::StudioBackground;

renderer.set_studio_equirect(&device, Some(&hdr_light));
renderer.set_studio_background(&device, StudioBackground::Solid([40.0, 40.0, 44.0]))?;
```

Over the agent protocol it is `render.set_studio_background { kind, ... }` with the parameters below, for
example `{ "kind": "solid", "color": [40.0, 40.0, 44.0] }`; `render.get_studio_environment` reports the
background in force.

| `kind` | Parameters | Shows |
|---|---|---|
| `environment` | none | the lighting environment (the default) |
| `solid` | `color` | one radiance, cd/m² |
| `gradient` | `zenith`, `horizon`, `ground`, `falloff?` | the gradient of `render.set_studio_environment`, over the view direction |
| `image` | `path`, `scale?` | a second OpenEXR equirect image, up to the environment budget wide |

The lighting never reads the background: the diffuse term, reflections and local captures use the
environment. Reflections of the sky stay the environment, as a chrome ball on a backdrop shows the room,
not the backdrop. The background goes through the sky draw, so glass refracts it, and it has the exposure
of the frame; auto exposure measures the whole frame, background included, so use a fixed exposure for
frames compared with each other. It needs a frame with the image light on (`FrameScene::ibl`). On a
frame with the physical atmosphere (Desktop High, Ultra) the atmosphere's sky covers it unless the
environment authority is `Studio` (see above).

## Errors of the methods above

`F0002`: a missing or malformed parameter (an unknown `kind` or `mode`, a bad value). `F0003`: no
`RuntimeRenderer`, `RenderDevice` or `DebugView` in this app.
