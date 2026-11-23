# IES light profiles and area lights (agents)

Spec 13 § 6 (Tier 2), ADR-2592. Three light additions, all opt-in: a light without them renders bit for bit as
before.

- **IES profiles.** A `LightProfile` component on a `PointLight` or `SpotLight` entity shapes the light with an
  IES LM-63 photometric profile. Profiles live in the `LightProfileLibrary` resource under a name (normally the
  `.ies` asset path). The asset pipeline cooks `.ies` files into a cooked profile (`FIES` v1,
  [docs/formats/ies-profile-v1.md](../../docs/formats/ies-profile-v1.md)).
- **Rect and tube lights.** `RectLight` and `TubeLight` components are authored area lights placed by the
  entity's transform, shaded with the GGX LTC tables (the same path that shades emissive meshes).
- **Sized spots in the path tracer.** A spot with a `LightSourceSize` is traced as a sphere limited by the spot's
  cone (`PtLight::SphereSpot`); before, the tracer treated it as a delta light.
- **LTC sphere lights.** A `LightSourceLtc` marker next to a `LightSourceSize` shades the sphere through the same
  LTC tables as a rect light, for both diffuse and specular, with the tracer's physical falloff.

## `render.set_light_profile`

Adds a profile to the library and, optionally, puts it on a light.

| Param | Type | Meaning |
|---|---|---|
| `name` | string, required | Library name. |
| `ies` | string | LM-63 text to parse and cook. |
| `path` | string | A project-relative `.ies` file to read instead of `ies` (no absolute path, no `..`). |
| `entity` | integer | A point or spot light entity that takes a `LightProfile` naming the profile. |
| `file_intensity` | boolean | Use the file's candela × `scale` as the light's intensity. |
| `scale` | number | Multiplier on the file's candela (default 1). |
| `rotation` | number | Turn of the profile's horizontal 0° about the nadir, radians. |

```json
{"method": "render.set_light_profile", "params": {"name": "lights/downlight.ies", "path": "assets/lights/downlight.ies", "entity": 4294967301}}
```

The result holds `name`, `peak_candela`, `flux_lm`, `vertical_range` (degrees), `profiles` (every name in the
library) and `assigned` (the entity's `LightProfile` fields, or `null`).

Errors:

- `F0002`: a missing or empty `name`, both `ies` and `path`, a `path` that is absolute or uses `..`, LM-63 text that does not parse (the message names the
  field, for example `ies: unexpected end of file while reading candela values`), a profile with no light, or a
  non-numeric `scale` / `rotation`.
- `F0003`: an unknown `name` with no `ies` or `path`, an unreadable `path`, an entity that does not exist or is
  not a point or spot light.

Without `entity`, the call only fills the library. Every light whose `LightProfile.profile` names an entry is
shaped from the next frame.

## `render.area_light_fallback` and `render.set_area_light_fallback`

ADR-3247. Rect and tube lights ride the emitter path, which has no budget on `DesktopLow`, `MobileLow` and
`MobileHigh` (`FrameQuality::emissive_lights` 0): there they emit nothing. `RenderProfile::area_light_fallback`
(off in every preset; `RenderSettings::preset` ignores it) makes the extraction stand them in as clustered lights
every tier shades, the lite (Mobile) shader included:

- each emitting face of a rect: a spot at the rect's centre along its normal, intensity `L · A` (= lumens / π), inner
  half-angle 0 and outer 90°, so the spot's ramp is exactly Lambert's `cos θ`;
- a tube: three cosine spots pointing out from its axis, 120° apart (the first toward the ground), each of
  `lumens / 3π` cd: together nothing along the axis and within −9 % / +5 % of the cylinder's `sin θ` across it;
- all with the equal-area source size (`√(A/π)` for a rect, `√(2rL/π)` for a tube), so the specular takes the
  sized lights' representative point.

`render.set_area_light_fallback {enabled}` sets the field (error `area_light_fallback_bad_enabled` for anything but a
boolean) and answers the state; `render.area_light_fallback` reads it:

| Field | Meaning |
|---|---|
| `enabled` | The profile's `area_light_fallback`. |
| `emitter_budget` | The tier's `FrameQuality::emissive_lights`. |
| `active` | `enabled` and the budget is 0: the stand-ins apply. |
| `stand_in_lights` | The last extracted frame's stand-in lights (the cost counter: each is one more clustered light). |
| `area_lights` | The last extracted frame's rect and tube lights. |
| `preset` | The preset the settings still match. |

```json
{"method": "render.set_area_light_fallback", "params": {"enabled": true}}
```

The stand-ins keep the punctual lights' 1 m falloff knee, so within about a metre of the light they read darker than
the rect; against the path tracer on a rough floor the floor's error is in `area_light_stand_ins_match_the_traced_rect_and_tube`
(`regia_pt/tests/lights_ies_area_frame.rs`; `REGIA_AREA_FALLBACK_DUMP=<dir>` writes trace | off | on plates). A tier
with an emitter budget never uses them. The default is the owner's call.

## `render.sized_light_ltc` and `render.set_sized_light_ltc`

ADR-3247. `RenderProfile::sized_light_ltc` (off in every preset; `RenderSettings::preset` ignores it) shades every
point and spot light that has a `LightSourceSize` as if it carried a `LightSourceLtc` marker: through the LTC tables
instead of the representative point (ADR-1367). A resolved glossy highlight then takes the sphere's flat-topped shape
(`an_ltc_sphere_follows_the_resolved_highlight_closer_than_the_representative_point` in
`regia_pt/tests/source_size_parity.rs`), and a sphere below a surface's horizon still lights it. The LTC form trades a
few percent on rough lobes for that (the roughness table below). Lights without a size, the Mobile lite shader and
the terrain pass are unchanged.

`render.set_sized_light_ltc {enabled}` sets it (error `sized_light_ltc_bad_enabled`); `render.sized_light_ltc` reads
`{ enabled, marked_lights, preset, applies_to }`, where `marked_lights` counts the last extracted frame's lights shaded
through the tables, markers included.

```json
{"method": "render.set_sized_light_ltc", "params": {"enabled": true}}
```

## The components

The components are reflected and saved in scenes. Agents add them with `scene.add_component` and edit their
fields with `editor.set_property`, like any other component.

| Component | Fields | Meaning |
|---|---|---|
| `LightProfile` | `profile`, `file_intensity`, `scale`, `rotation` | The library name. Without `file_intensity`, a spot's `intensity` (cd) is the profile's peak, and a point light keeps its lumens spread over the profile. A spot's nadir is its direction; a point light's is its rotated `-Y`. The spot cone still applies on top of the profile. |
| `RectLight` | `color`, `intensity` (lm per face), `width`, `height`, `two_sided`, `range` (0 = the emitters' cutoff), `cast_shadows` | A rectangle `width` along the entity's `X` and `height` along its `Y`, emitting toward its `-Z`. |
| `TubeLight` | `color`, `intensity` (lm), `length`, `radius`, `range`, `cast_shadows` | A Lambertian cylinder along the entity's `X`. |
| `LightSourceLtc` | none (a marker) | On a `PointLight` or `SpotLight` with a `LightSourceSize`: shade the sphere through the LTC tables. Add it with `scene.add_component`, remove it with `scene.remove_component`. Without a size it does nothing. |

## What the renderer does

- A profiled light's GPU record gets a fraction added to its kind (`kind + code / 4096`). The fraction names one of
  7 profile slots and the horizontal turn. Profiles are 256 × 32 bands at the bottom of the emitter probe atlas,
  so no new binding is needed. At most 7 distinct profiles fit in a frame; a light past that shades without its
  profile and logs a warning. The profile bands reduce the emitter tiles available: each band costs 32 tiles of
  the 512.
- Rect and tube lights join the emissive-mesh area lights after the fitted ones, in the same budget
  (`FrameQuality::emissive_lights`).
- **Shadows (an approximation).** A rect or tube light with `cast_shadows` traces the fitted emitters' distance
  probe once per placement: 16 × 16 octahedral first-hit distances from the light against the scene's meshes.
  That probe is a static, soft shadow map. Moving geometry does not update it, but moving the light does. This
  is the closest shadow the emitter path has. Area lights take no shadow-atlas tile.

- **LTC spheres.** A marked light's GPU record stores its source radius negated. The forward pass reads the sign
  and integrates a 12-gon of the sphere's silhouette disk through the clamped cosine and the GGX table. A sphere
  that dips below a surface's horizon lights it correctly, which the representative point does not.
  - Against a brute-force integral of the GGX specular over the sphere, across view angles and light positions, the
    mean relative error depends on roughness. LTC against the representative point: 1.2 % / 27 % at perceptual
    roughness 0.25, 2.2 % / 5.6 % at 0.4, 3.7 % / 1.6 % at 0.6, 4.6 % / 0.6 % at 0.8, 3.6 % / 0.2 % at 1. So the LTC
    form is for glossy surfaces and for spheres near a horizon. On rough surfaces far from the horizon the
    default form is closer.
  - The falloff is `1 / d²`, as in the path tracer. The punctual path's `1 / (d² + 1)` knee does not apply, so a
    marked light is brighter than the same unmarked light near it: by `(d² + 1) / d²`, twice at 1 m and 1 % at 10 m.
  - Under an angular radius of about 1.1° (`r / d` below 0.02), the polygon's edge sum is `f32` noise, so the
    light takes the representative point there with the same `1 / d²`.
  - Clearcoat, sheen, anisotropic, skin and hair materials also keep the representative point, which models their
    lobes. The terrain's shared pass keeps it too (it has no LTC binding). All of them use the magnitude of the
    radius, so they shade the light normally.

## Known limits

- **Rect and tube lights need an emitter budget.** They are shaded as area lights only on tiers where
  `FrameQuality::emissive_lights` is above 0: `DesktopHigh` (96) and `DesktopUltra` (160). On `DesktopLow`,
  `MobileLow` and `MobileHigh` the budget is 0, so they emit nothing and the renderer logs one warning, unless
  `render.set_area_light_fallback` turns on their point and spot stand-ins (above).
- Profiles shade on the forward pass of every tier (the Mobile lite shader too, ADR-3247), on the terrain's shared
  light pass (ADR-3247, once the frame hands the draw the atlas: `SharedPassDraw::set_engine_light_profiles`) and in
  the path tracer (`PtScene::light_profiles`, filled by `regia_pt::extract::push_light_profiles`). Volumetric fog
  reads the light without its profile.
- Photometric types A and B are resampled through their own angle frame, which is less tested than type C.
  `TILT=<file>` is rejected. `TILT=INCLUDE` data is applied (ADR-3247): every candela value takes the table's factor
  at the luminaire's tilt, 0° (as photometered) in the plain lookups and the cooked profile, another angle through
  `IesFile::candela_tilted` and `CookedIesProfile::from_file_tilted`. A file whose factor at 0° is not 1 therefore
  cooks a different peak than before.
- The Mobile forward has no sphere specular and ignores `LightSourceLtc`.
- **The LTC tables under-read a glossy lobe's far tail.** On a glossy floor (perceptual roughness 0.3, metal) lit by
  a marked sphere, the highlight is within 1.4 % of a brute-force integral and the floor's total reflected energy
  within 3.1 % (the representative point: 47 %), but points far off the highlight, under a tenth of its peak, read
  about 60 % low. The same tables shade emissive meshes and rect and tube lights, so their glossy tails do the same.
  Measured on the CPU (`ltc::sphere_study::the_glossy_frame_geometry_on_the_cpu`) and in the frame test.
- **Open: the glossy LTC sphere frame is further off than its CPU mirror.** On lavapipe the glossy metal floor is 12 %
  off the path tracer in energy and 11 % in the highlight (the representative point: 49 % and 50 %), against 3.1 % and
  1.4 % for the CPU mirror of the same scene. The dielectric floors match the tracer within 3 %. The glossy frame test
  is ignored until the gap is found (`an_ltc_sphere_on_glossy_metal_matches_the_traced_sphere`).
