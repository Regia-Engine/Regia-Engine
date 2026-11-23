# Distance and height fog

Outdoor haze on the runtime path: fog gets stronger with distance and with depth below a
base height. It runs in linear color, before AgX.

## Formula

Optical depth for the flat-color mode (`sky_inscatter = false`) is evaluated at the point.
`regia::fog::fog_factor` returns the fog amount `1 − exp(−τ)`, not `τ` itself:

```text
τ = distance · density · exp(−height_falloff · (y − base_height))
amount = 1 − exp(−τ)
color = lerp(scene, fog_colour, amount)
```

That is the exponential height fog from Hoffman & Preetham, "Rendering Outdoor Light Scattering"
(GDC 2002). `regia_render::fog` is the CPU copy of this point formula, and the flat branch of
`lab_forward.wgsl` and the terrain shaders (`terrain.wgsl`, scatter, water) use it.

The default aerial mode (`sky_inscatter = true`) instead integrates along the camera-to-surface
segment (`height_fog_optical_depth` in `aerial.wgsl`):

```text
start = density · exp(−height_falloff · (camera.y − base_height))
dh    = surface.y − camera.y
τ     = start · distance · (1 − exp(−height_falloff · dh)) / (height_falloff · dh)
```

(the last factor is 1 when `height_falloff · dh` is near 0). A hand-computed golden for a mesh
must use this form.

## Runtime

Put settings on `RenderSettings.fog` (`FogSettings`). When the LUT sky runs, the frame forces
`FogSettings` off for meshes and terrain and the atmosphere's aerial LUT supplies the haze; use
`VolumetricFogSettings` there. On Earth the LUT sky is `physical_atmosphere` (Desktop High and
Ultra, and only when no studio environment owns the frame). On Mars or an airless world an HDR
single-sample frame uses the LUT sky on every preset. An airless world also forces `FogSettings`
off. On Earth, `FogSettings` applies when that LUT sky is off (Desktop Low, Deck, Mobile, lab).
Extraction copies them onto
`ExtractedFrame` and `FrameScene`, so `MeshRenderer` + `Camera` see them. The default
(`RenderSettings::default().fog`) is `FogSettings::aerial()`: aerial perspective that
in-scatters the horizon sky radiance (`sky_inscatter = true`, optical depth integrated along
the ray; see [The default look](../user-guide/default-look.md)). `FogSettings::default()` is
off, and lab/golden scenes built from `LabScene` stay fog-free. `FogSettings::outdoor_haze()`
is a dense blue-gray flat-color preset.

```rust
use regia::prelude::*;
use regia_render::{FogSettings, RenderProfilePreset, RenderSettings};

fn configure(app: &mut App) {
    // Desktop Low keeps the analytic fog. Desktop High's LUT sky forces `FogSettings` off;
    // use `VolumetricFogSettings` on that preset.
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopLow);
    settings.fog = FogSettings::outdoor_haze();
    app.insert_resource(settings);
}
```

`params_vec4()` packs `(density, height_falloff, base_height, enabled)` for the GPU.

## What a good frame looks like

Far terrain shifts toward the fog color more than near terrain. A uniform tint is not fog.
Tests reject a near-black frame, a blown-out frame, and a frame where far pixels did not move
more than near ones. Evidence for the heightfield (scatter, one sun cascade, three sun angles,
fog on and off) lives in `docs/evidence/do27-gaps/`. The noon fog-on PNG is the lavapipe golden.
