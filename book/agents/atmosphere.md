# Atmosphere

Desktop High and Ultra sample a physically based sky. The tables are integrated on the CPU
once per sun elevation, camera height and planet, then uploaded. A change of azimuth only
rotates the sample. On Earth, Mobile, Desktop Low and Deck keep the analytic gradient; Mars and
airless maps use the tables on every HDR frame (there is no cheap stand-in for them).

## What is in the tables

- Transmittance to space.
- One multiple-scattering estimate (Hillaire 2020, building on Bruneton 2017).
- A sky-view table relative to the sun.
- Aerial perspective: in-scatter and transmittance over 32 km, sixteen bilinear slices.

The sun disk is drawn analytically, at least a pixel wide, with limb darkening. It is smaller
than one sky-view texel, so it is not baked into the table. Stars are added on top of the
lookup: at night on a planet with air, at any hour on an airless one. A sky body (Jupiter seen
from Europa, 14° in the DO27 preset) and up to two `moons` are spheres lit by the sun, so they show the right phase; `OutdoorParams::sky_body_albedo` takes an equirectangular albedo map for
the body, and `OutdoorParams::moon_albedo` (`[Option<Arc<SkyBodyTexture>>; 2]`, one per moon in
order) does the same for the moons. A `None` slot keeps that moon's tint, byte-identical to the
untextured look; a `Some` map is sampled on the same path (equirectangular, longitude across,
north pole at the top, sRGB RGBA8) and lit with the same sun-driven phase. Airless daylight stars carry their physical radiance, so they vanish next to sunlit ice and show at night exposure. Indirect light uses the
table without the disk, plus a ground bounce below the horizon, written into the existing IBL
textures, so a diffuse surface matches the sky; on an airless moon that ambient is the ground
bounce alone.

A view ray below the geometric horizon that no geometry covers (past a platform edge, past the
far edge of a map) shows the planet's surface: one Lambertian color behind the air in front of
it. That is on by default under air and off on an airless world, where nothing fades it into a
horizon and it would read as a flat band at the map's edge; `OutdoorParams::planet_surface`
(`Some(true)` / `Some(false)`) overrides it per sky (ADR-162).

## Planets

`AtmosphereParameters::{earth, mars, airless}` (or `from_preset`) set the medium; maps choose
one with `LevelSettings.atmosphere.preset` (`Earth`, `Mars`, `Airless`), see the
[rendering guide](../user-guide/rendering.md#planet-presets-per-map). In code,
`OutdoorParams { planet, sky_body, .. }` on `FrameRenderer::set_outdoor` does the same.

| Preset | Sky | Haze | Clouds | Ambient |
|---|---|---|---|---|
| Earth | blue, orange at sunset | Rayleigh + Mie | yes | sky + ground |
| Mars | butterscotch, blue around a low sun | dust | yes | dusty sky + ground |
| Airless | black, stars | none | none | ground bounce |

Agents: the protocol has no dedicated sky command. Read the level's settings with
`scene.serialize` (the `LevelSettings` resource) and change the planet by editing the
`.level.ron` (canonical RON, format v3); the map's exposure and grade are `level.get_look` / `level.set_look` ([look](look.md)). `render_lab planets` renders the reference shots.

## Time of day

Insert a `TimeOfDay` resource (`hour`, `day_of_year`, `latitude_deg`). When the physical
atmosphere is on, that sun replaces the directional light, including the transmitted color
and illuminance, before the shadow cascades are fitted. With no `TimeOfDay`, the authored
directional light still drives the sky.

```rust
use regia::prelude::*;
use regia::render::TimeOfDay;

fn configure(app: &mut App) {
    app.insert_resource(TimeOfDay::noon());
}
```

`TimeOfDay::dawn`, `dusk` and `night` are the equinox, 45°N samples the goldens use. A map
can set the clock instead with `LevelSettings.atmosphere.time_of_day`; a `TimeOfDay`
resource wins.

## Checks

`sky_four_times_of_day_have_the_right_hue` renders dawn and dusk as one fixed panorama: the
glow sits over the sun's azimuth (east at dawn, west at dusk) and a dusk frame fails the dawn
golden. `planet_presets_have_their_own_sky_haze_and_ambient`,
`mars_sunset_is_blue_around_the_sun` and `airless_sky_body_is_lit_on_the_sun_side` check the
presets on whole Desktop High frames.
