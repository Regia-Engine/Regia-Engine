# 99 — Weather: precipitation, surface wetness and snow, weather-driven scene wind

Status: proposed (ADR-1847). Plan items: `versions.md` v0.2.x candidates ("weather such as rain, snow and wind", v0.2.x to
v0.3.x); gap audit 2026-10-30 rows "Rain and snow drawn; weather-to-wind coupling" (NOT-STARTED, "no spec") and E5
`weather-precip`. Crates: `regia_scene` (three data resources), `regia_vfx` (the weather model, the precipitation draw),
`regia_render` (the precipitation occlusion view, the surface response on lab materials, the scene-wind sink),
`regia_terrain` (the surface response on terrain and grass), `regia_asset` (phase B5 overrides), `regia_agent` (surface).
No reviewed public-spec exists for this item: the Regia docs and the papers in § 15 are the sources.

Builders read this file, `88-foliage-wind.md` (the scene wind this spec drives; lane w2, ADR-1830), `book/agents/weather.md`
and `regia_vfx/src/weather.rs` (ADR-429, ADR-1408: the weather resource as built), `regia_terrain/src/ground_look.rs`
(ADR-1408, ADR-1452: the static snow and wet-mud terms and the marker rule for terrain shader code),
`26-terrain-vfx-highend.md` § 3 (VFX), `12-assets.md` § 4a (format freeze rules) and `23-agent-native.md`. The proposed
builder phases are in § 14 and in `docs/notes/weather-precipitation-lanes.md`.

## 1. Purpose and scope

The engine has a weather (`regia_vfx::Weather`): cloud coverage, cloud type, density, a wind at cloud height and a
`precipitation` value in `0..1` that drives the clouds and that games read. Nothing draws the precipitation, nothing gets
wet, no snow settles, and the wind that carries the clouds does not reach the plants (spec 88's scene wind is a value a
host copies in by hand). This spec closes that gap:

1. **A weather model that knows what falls** (§ 3): a temperature and a precipitation kind on the weather, rain and snow
   rates in physical units, a drop size distribution and fall speeds, and a visibility term for the fog.
2. **Weather-driven scene wind** (§ 3.3): a `SceneWind` resource the weather fills with the wind near the ground (a
   height profile from the cloud wind plus deterministic gusts). It feeds spec 88's foliage wind and the grass, slants the
   rain and drifts the snow.
3. **Rain and snow drawn through `regia_vfx`** (§ 5): stateless GPU particles in a camera-centered wrapped volume (instanced
   streaks for rain, flakes for snow, splashes on the surfaces the rain hits), no simulation buffer and no CPU work per
   drop. They are hidden under roofs by a **precipitation occlusion view** (§ 5.6), a small depth map seen from the sky.
4. **Surface weather** (§ 3.4, § 6): a `SurfaceWeather` resource the weather integrates over game time (wetness,
   puddles, settled snow depth and cover, melting and drying). Lab materials and the terrain respond to it: wet
   surfaces darken by porosity and turn glossy, puddles form on flat ground with rain ripples, and snow covers
   up-facing surfaces. Surfaces under cover stay dry, using the same occlusion view.

**Opt-in per project.** The three effects (precipitation drawn, surface weather, weather-driven scene wind) are off by
default, so an existing host that already has a `Weather` resource (for its clouds) renders the same frames as before
this spec. That includes a map that sits in `overcast`, whose `precipitation` of 0.2 would otherwise start to drizzle.
A project turns them on in `config/weather.ron` `effects` (§ 7.1). A host can also insert the resources itself, and an
agent can turn them on with the `render.set_*` methods (§ 8). Projects made by `regia new` get a config file with all
three on. Turned on, they still do nothing until there is a `Weather` with precipitation, a wind, or a non-zero surface
state. With `SurfaceWeather::DRY`, no precipitation and no `SceneWind`, every frame renders the same bytes as before.
That is a tested requirement (§ 11).

## 2. Built baseline

- `regia_vfx::weather` (ADR-429, ADR-1408): `WeatherState { coverage, cloud_type, density, wind_m_per_s, precipitation,
  genus }`, nine presets, eased transitions, the weather clock and the integrated cloud drift, `WeatherPlugin` writing
  `CloudSettings` every `Update`; agent `render.get_weather` / `render.set_weather` (`book/agents/weather.md`).
- `regia_vfx::showcase::weather()`: a `.vfx.ron`-style effect (rain streaks in a 14 m box plus a ground mist) a game spawns
  at a point. It stays as it is: a local effect, not the scene's weather.
- `regia_vfx::gpu::VfxGpu` and the `TranslucentLayer` hook (`regia_render::translucent`): the pass the precipitation
  draws in, with the scene depth bound read-only, the post exposure, the sun and sky lighting rows and the
  half-resolution soft-particle path.
- Spec 88 (lane w2, unmerged at the time of writing; this spec depends on it): `FrameRenderer::set_foliage_wind`,
  `SharedFrameInfo::foliage_wind`, the grass reading the scene wind, and `render.foliage_wind` / `render.set_foliage_wind`.
- Terrain: `TerrainFrame::wetness` (darkens and glosses the ground; the shared runtime path hard-codes 0), the ground
  look's static snow, melt fringe and wet mud (ADR-1408) inside the `@ground-look` markers (ADR-1452: the terrain color
  pass sits at a compiler cliff, so new code there goes inside markers with a variant compiled without it).
- Lab forward pass (`lab_forward.wgsl`): the local-light shadow atlas (`shadow_atlas`, binding 9) and its tile table
  (`shadow_tiles`, binding 10) are bound in every permutation.
- `FogSettings` (base `density` in 1/m with a height falloff), `TimeOfDay` and `sun_position`, `VfxMedium` (Earth, Mars,
  airless).

## 3. The weather model

### 3.1 Weather state additions (`regia_vfx::weather`)

```rust,ignore
pub enum PrecipitationKind { Auto /* default */, Rain, Snow, Sleet }

pub struct WeatherState {
    // existing: coverage, cloud_type, density, wind_m_per_s, precipitation, genus
    pub temperature_c: f32,                   // air temperature near the ground, °C
    pub precipitation_kind: PrecipitationKind,
    pub gustiness: f32,                       // 0..1, the gust amplitude of the scene wind (§ 3.3)
}
```

- `sanitized` clamps `temperature_c` to `-60..60` (non-finite: the fair value) and `gustiness` to `0..1`.
- `lerp` blends `temperature_c` and `gustiness` linearly. The kind is `b`'s once `t` reaches 1, else `a`'s.
- **Rates during a transition** (`Weather::rates(&config)`): every consumer reads the rates from the weather, not from
  `current().rates()`. While a transition runs, they are the from-state's and the to-state's rates (§ 3.2) blended by
  the eased progress, field by field. `Λ` and the visibility are then recomputed from the blended rain rate and the
  blended extinction. A `Rain` → `Snow`, `Rain` → `Auto` or rain → snow-by-temperature transition therefore changes
  the snow fraction and both rates continuously, and settles exactly on the target's rates.
- `precipitation` keeps its meaning (`0..1`, now the input of § 3.2) and its existing presets' values. The existing presets
  keep every existing field bit for bit and gain: `clear` 15 °C, gust 0.15; `fair` 15 °C, 0.2; `cloudy` 12 °C, 0.25;
  `overcast` 8 °C, 0.2; `storm` 10 °C, 0.45; the four genera 12 °C, 0.2. All are `Auto`.
- New presets (appended to `WEATHER_PRESETS`, which becomes `[&str; 12]`):

| Preset | coverage | cloud_type | density | wind (m/s) | precipitation | temperature | gust |
|---|---|---|---|---|---|---|---|
| `rain` | 0.9 | 0.3 | 0.07 | [9, 3] | 0.35 (about 6 mm/h, moderate) | 9 °C | 0.3 |
| `snow` | 0.95 | 0.15 | 0.06 | [5, 2] | 0.5 (about 1.2 cm/h of snow) | -3 °C | 0.2 |
| `blizzard` | 1.0 | 0.2 | 0.08 | [20, 8] | 1.0 (about 5 cm/h of snow) | -8 °C | 0.6 |

### 3.2 What falls: rates, sizes, speeds

All in `regia_vfx::weather`, pure functions of a `WeatherState` and the project's `WeatherConfig` (§ 7.1):

```rust,ignore
pub struct PrecipitationRates {
    pub snow_fraction: f32,        // 0 rain .. 1 snow
    pub rain_mm_per_h: f32,        // liquid reaching the ground
    pub snow_we_mm_per_h: f32,     // snow as water equivalent
    pub snow_depth_mm_per_h: f32,  // fresh snow depth (water equivalent × snow ratio)
    pub drop_lambda_per_mm: f32,   // the rain's size distribution slope (0 when no rain)
    pub visibility_m: f32,         // horizontal visibility from precipitation alone (f32::INFINITY when none; JSON null)
}
impl WeatherState { pub fn rates(&self, config: &WeatherConfig) -> PrecipitationRates; }
pub fn rain_class_diameters_mm(rates: &PrecipitationRates, config, out: &mut [f32; RAIN_CLASSES]);
pub fn rain_fall_speed_m_per_s(diameter_mm: f32) -> f32;
pub const RAIN_CLASSES: usize = 8;
pub const SNOW_CLASSES: usize = 4;
```

- **Snow fraction.** `Rain` → 0, `Snow` → 1, `Sleet` → 0.5. `Auto`: 1 at or below `snow_all_below_c` (default 0 °C), 0 at
  or above `rain_all_above_c` (default 2.5 °C), a smoothstep between. With the defaults the 50 % point is 1.25 °C, near
  the observed rain-snow transition over land (Dai 2008, about 1-2 °C).
- **Rates.** With `p = precipitation`: `rain = max_rain_mm_per_h · p² · (1 - s)` (default max 50 mm/h) and
  `snow_we = max_snow_we_mm_per_h · p² · s` (default 5 mm/h). Fresh snow depth is `snow_we · snow_ratio` (default 10,
  the conventional snow-to-liquid ratio; Roebber et al. 2003 discuss its spread). With the defaults, `overcast` (0.2)
  drizzles at 2 mm/h (light: below 2.5 mm/h in the AMS Glossary), `rain` is moderate, and `storm` is 50 mm/h.
- **Drop sizes.** Exponential (Marshall and Palmer 1948): `N(D) = N0 · exp(-Λ D)`, `N0 = 8000 m⁻³ mm⁻¹`,
  `Λ = 4.1 · R^-0.21 mm⁻¹` (R in mm/h). Only drops above `visible_drop_mm` (default 1 mm) are drawn. The drawn drops are
  split into `RAIN_CLASSES` equal-count quantile classes of the tail above it: class `k` has diameter
  `D_vis + (-ln(1 - (k + 0.5) / K)) / Λ`, capped at 6 mm.
- **Fall speeds.** Rain: `v(D) = 9.65 - 10.3 · exp(-0.6 D)` m/s (Atlas, Srivastava and Sekhon 1973, a fit to Gunn and
  Kinzer 1949), clamped at 0. Snow: four classes at 0.6, 0.85, 1.1 and 1.4 m/s (aggregates fall at about 0.5-1.5 m/s;
  Locatelli and Hobbs 1974). Every drop is also carried by the scene wind horizontally (§ 3.3).
- **Drawn count.** The number of drops drawn scales with the number of drops above `D_vis` per unit volume,
  `(N0 / Λ) · exp(-Λ D_vis)`, normalized to 1 at `max_rain_mm_per_h`. That gives about 0.09 at 2 mm/h, 0.24 at the `rain` preset's 6.1 mm/h,
  0.41 at 12.5 mm/h and 1 at 50 mm/h. Snow scales linearly with `snow_we / max_snow_we`. Both are multiplied by the tier
  budget (§ 5.7) and `density_scale`.
- **Visibility** (Koschmieder: `V = 3.912 / σ` at a 2 % contrast threshold). The precipitation extinction is
  `σ_p = 3.912 · p² · ((1 - s) / rain_visibility_m + s / snow_visibility_m)` 1/m, with defaults of 3000 m for the heaviest
  rain and 400 m for the heaviest snow. While `effects.precipitation` and `visibility.couple_fog` (its default) are on,
  `drive_weather_air` writes `σ_p` into `regia_scene::weather::WeatherAir`. (The renderer cannot read `Weather`, which
  lives in `regia_vfx`.) The extraction passes it to the frame as its own term: a constant, height-independent
  extinction `exp(-σ_p d)` over the view distance `d`, applied wherever the frame fog is applied (lab forward, the shared
  terrain pass, the sky) with the fog's in-scatter color. It is not folded into `RenderSettings::fog`'s `density`. That
  would put the height falloff on the rain (with the `outdoor_haze` falloff of 0.06 it would vanish 50 m up) and, when
  the host's fog is off, turning it on would bring in the host's stored density. The host's `RenderSettings::fog` is
  never modified. The term exists whether or not the host's fog is enabled. Without the resource nothing is added and
  the shaders skip it on a uniform branch. Builder phase B4 (it touches the fog shading).

### 3.3 Scene wind (`regia_scene::weather::SceneWind`, driven from `regia_vfx::weather`)

```rust,ignore
// regia_scene::weather (data only; read by render, physics, audio, vfx without depending on regia_vfx)
#[derive(Resource, Reflect, Clone, Copy, Debug, PartialEq)]
pub struct SceneWind {
    pub mode: SceneWindMode,          // Weather | Manual | Off
    pub velocity_m_per_s: [f32; 2],   // now, gusts included: what consumers read (world X, Z)
    pub mean_m_per_s: [f32; 2],       // without gusts
}
pub enum SceneWindMode { Weather, Manual, Off }
impl SceneWind {
    pub const CALM: Self;                      // Manual, zero
    pub fn manual(wind_m_per_s: [f32; 2]) -> Self;
    pub fn speed(&self) -> f32;
    pub fn velocity3(&self) -> [f32; 3];       // [x, 0, z]
}
```

`drive_scene_wind` (`Update`, after `drive_cloud_weather`) inserts it with the config's `wind.mode` the first time a
`Weather` exists and `effects.scene_wind` is on (a host or an agent may insert it at any time instead), and writes it
every frame when the mode is `Weather` and a `Weather` exists:

- **Height profile.** The weather's wind is at cloud height. Near the ground (10 m) it is
  `mean = cloud_wind · f`, with `f = (10 / h)^(1/7)` (the wind profile power law with the open-terrain exponent; Peterson
  and Hennessey 1978) and `h` the cloud layer's `altitude_bottom` (1500 m by default, so `f ≈ 0.49`).
  `wind.surface_factor` in the config overrides `f`. `wind.veer_deg` (default 0) turns the surface wind relative to the
  wind aloft. Friction backs the surface wind by roughly 10-30° over land; the default 0 keeps the presets' directions.
- **Gusts.** `velocity = mean · g(t)` rotated by `θ(t)`, with `t` the weather clock and `γ = min(gustiness · wind.gust_scale, 1)`:
  `g = 1 + γ (0.55 sin(2π t / 11.3) + 0.3 sin(2π t / 4.7 + 1.3) + 0.15 sin(2π t / 1.9 + 2.9))` and
  `θ = 0.17 γ sin(2π t / 17.1 + 0.4)` rad. So `g ≥ 0` (the gusts never reverse the wind), `|g - 1| ≤ γ`, the gusts move the wind by at most
  `γ (1.203 + 0.0625 (1 + γ)) |mean| dt ≤ 1.33 γ |mean| dt` a frame (on top of any change of the mean), and a paused
  weather holds the wind. This is Regia's own deterministic signal, not a turbulence model. Spec 88's per-plant
  gust still runs on top of it.
- `Manual` holds the stored values (no gusts). `Off` tells consumers there is no scene wind.

**Consumers** (each reads the resource when it exists, else behaves as today):

| Consumer | Reads | Effect |
|---|---|---|
| Renderer (`extract_frame`) | `velocity_m_per_s` | `FrameRenderer::set_foliage_wind(Some(v))` every frame; `Off` → `None` (spec 88 § 2.1) |
| Grass | the terrain wind row below | `SharedFrameInfo::foliage_wind` stays `None` while a `SceneWind` sets the foliage wind, so grass does not take the wind twice (ADR-2043). Without a `SceneWind`, a direct `set_foliage_wind` reaches it as in spec 88 § 2.4 |
| Terrain wind (`regia_terrain::TerrainWind`, ADR-1853, on main) | `mean_m_per_s` | `couple_scene_wind`: heading in whole degrees, strength = speed / 5 m/s in 1/32 steps (≤ 4); written only when that changes, so gusts never redraw grass shadows (ADR-2032) |
| Precipitation (§ 5) | `velocity3()` (without a `SceneWind`: the mean of `surface_wind`, no gusts) | rain slant, snow drift, wind drift integration |
| Occlusion view (§ 5.6) | mean fall direction | the view's tilt |
| Surface weather (§ 3.4) | `speed()` of `mean` | faster drying |

While a `SceneWind` resource exists, the extraction sets the renderer's foliage wind every frame, so a host that calls
`FrameRenderer::set_foliage_wind` directly is overridden: such a host sets `SceneWind` instead (`Manual`), or removes it.
Changed behavior of spec 88's `render.set_foliage_wind` (§ 8): when a `SceneWind` resource exists, the call writes it
(`Manual` with the value, `Off` for `null`) instead of the renderer field, so the weather does not overwrite it the next
frame. Without the resource it acts as in spec 88. Audio wind beds, water sea state (gap audit E6) and
VFX particle drift are future consumers (§ 16). Cloth (spec 55) reads it since ADR-2884: each `ClothSim` solver's
global wind term gets `velocity3() · wind_scale` every fixed step (`regia_framework::cloth::feed_scene_wind_to_cloth`;
`Off` or no resource leaves cloth as before).

### 3.4 Surface weather (`regia_scene::weather::SurfaceWeather`, driven from `regia_vfx::weather`)

```rust,ignore
#[derive(Resource, Reflect, Clone, Copy, Debug, PartialEq)]
pub struct SurfaceWeather {
    pub mode: SurfaceWeatherMode,   // Simulated | Manual
    pub wetness: f32,               // 0..1, the water film on exposed surfaces
    pub puddles: f32,               // 0..1, how full the hollows are
    pub snow_we_mm: f32,            // settled snow as water equivalent (the conserved quantity)
    pub snow_depth_m: f32,          // snow_we_mm · snow_ratio / 1000 while Simulated
    pub snow_cover: f32,            // 0..1, the visual cover (from depth while Simulated)
    pub rain_mm_per_h: f32,         // falling now (splashes)
    pub snow_we_mm_per_h: f32,      // falling now
    pub ripple: f32,                // rain_mm_per_h / max_rain_mm_per_h, 0..1 (the renderer cannot read the config)
}
impl SurfaceWeather { pub const DRY: Self; pub fn is_dry(&self) -> bool; }
pub enum SurfaceWeatherMode { Simulated, Manual }
pub fn step_surface_weather(state: &mut SurfaceWeather, inputs: &SurfaceInputs, config: &SurfaceConfig, dt: f32);
pub struct SurfaceInputs { pub rates: PrecipitationRates, pub temperature_c: f32, pub sun_elevation_sin: f32,
                           pub cloud_coverage: f32, pub wind_speed_m_per_s: f32 }
```

`drive_surface_weather` (`Update`, after `drive_scene_wind`) steps it by the game's frame time while the weather is not
paused. It does nothing in `Manual` mode, without a `Weather`, or without the resource. It inserts
`SurfaceWeather::DRY` in `Simulated` mode the first time a `Weather` exists while `effects.surface` is on (a host or an
agent may insert it at any time instead). The model is Regia's
own: simple rates with the physical quantities that set them, tuned with config numbers. It is not a hydrology model.
With `R` the rain rate, `M` the meltwater rate (both mm/h), `T` the temperature, `e` the sun elevation sine (clamped to
`0..1`), `c` the cloud coverage and `u` the surface wind speed:

- **Drying factor** `F = (0.25 + 0.75 e (1 - 0.7 c)) · (1 + u / 5) · clamp(1 + T / 20, 0.1, 2)`. Evaporation grows with
  radiation and wind (the two terms of Penman 1948, used here only qualitatively).
- **Wetness.** `dw/dt = (R + M) / (wet_mm · 3600)` while `R + M > 0` (default `wet_mm` 0.2: 5 mm/h wets a surface fully in
  2.4 minutes), else `-F / dry_time_s` (default 1800 s). Clamped to `0..1`.
- **Puddles.** Fill only once `w ≥ 0.95`: `dq/dt = (R + M - drain_mm_per_h) / (puddle_mm · 3600)` (defaults 2.5 mm/h,
  the upper bound of light rain, and 5 mm, so drizzle wets but does not pool; `overcast`'s 2 mm/h stays below the drain
  even after `f32` rounding). When `R + M < drain_mm_per_h`: `dq/dt = -F / puddle_dry_time_s` (default
  3600 s). The film between puddles keeps drying on its own clock.
- **Snow** is kept as water equivalent, so mass is conserved. It accumulates at `snow_we_mm_per_h · a(T) / 3600` mm/s,
  with `a = 1` at or below 0 °C and falling to 0 at 2 °C (wet flakes melt on contact). It melts with a degree-day factor
  (Hock 2003; default `melt_mm_per_c_day` 4, in the published 3-6 range for snow) raised by the sun:
  `M = melt_mm_per_c_day · melt_scale · max(T, 0) · (1 + 0.5 e) / 24` mm/h of water, taken from `snow_we_mm` (never
  more than is left). `M` also wets the surface (above) while snow remains. Depth is `snow_we_mm · snow_ratio / 1000`
  m (the fresh-snow density; settling is not modeled).
- **Cover.** `snow_cover = smoothstep(0, full_cover_m, snow_depth_m)` (default 3 cm hides the ground) while Simulated.
- `rain_mm_per_h`, `snow_we_mm_per_h` and `ripple` copy the rates (they drive ripples and splashes).
- In `Manual` mode, setting `snow_depth_m` sets `snow_we_mm = snow_depth_m · 1000 / snow_ratio`.

`Manual` holds whatever the host or the agent sets (a "wet street after rain" scene with no weather clock). A level can
start wet by setting the state before the first frame, in either mode. `Weather::set_now(STORM)` does not wet the
ground at once: the surfaces get wet at the rates above.

Game time can run faster than real time for weather. The rates are per second of game time, and `melt_scale` exists
because a realistic melt takes days.

### 3.5 Media

The level's medium (`VfxMedium::from_level(&LevelSettings)`, the same value `VfxScene::medium()` gives; it is not a
resource) when airless: no precipitation is drawn, the surface integrator holds (nothing dries without air), and the
consumers treat the scene wind as off. The stored `SceneWind` mode is kept, so it applies again when the medium changes. Mars and Earth draw as configured. A game that wants no rain on its Mars map
keeps `precipitation` at 0.

## 4. API shape

### 4.1 `regia_scene::weather` (new module, data only)

`SceneWind`, `SceneWindMode`, `SurfaceWeather`, `SurfaceWeatherMode` (§ 3.3, § 3.4), and
`WeatherAir { extinction_per_m: f32 }` (§ 3.2, the fog term), registered for reflection. No systems live here. They sit
in `regia_scene` because `regia_render`, `regia_physics` and `regia_audio` already depend on it and none of them may
depend on `regia_vfx`.

### 4.2 `regia_vfx::weather` (extended)

§ 3.1-§ 3.4 types and functions, plus:

```rust,ignore
#[derive(Resource, Clone, Debug, PartialEq)]
pub struct WeatherConfig { /* § 7.1, with Default */ }
impl WeatherConfig {
    pub fn from_ron(text: &str) -> Result<Self, WeatherConfigError>;
    pub fn to_ron(&self) -> Result<String, WeatherConfigError>;
    pub fn load(vfs: &Vfs) -> Result<Option<Self>, WeatherConfigError>;  // `config://weather.ron`; missing → Ok(None)
    pub fn preset(&self, name: &str) -> Option<WeatherState>;              // config presets, then built-ins
}
pub fn drive_scene_wind(/* Time, Weather, WeatherConfig, CloudSettings, LevelSettings, Option<ResMut<SceneWind>> */);
pub fn drive_surface_weather(/* Time, Weather, WeatherConfig, TimeOfDay, SceneWind, LevelSettings, Option<ResMut<SurfaceWeather>> */);
pub fn drive_weather_air(/* Weather, WeatherConfig, LevelSettings, Option<ResMut<WeatherAir>> */); // runs while effects.precipitation is on
pub fn surface_wind(state: &WeatherState, clock_s: f64, cloud_base_m: f32, config: &WindCoupling) -> SceneWind;
```

`WeatherPlugin` adds the three systems after `drive_cloud_weather`. At `Startup`, unless the host has inserted a
`WeatherConfig`, it reads `config://weather.ron` through the `Vfs` resource (`<assets>/config/weather.ron` on desktop,
the APK's `assets/config` on Android, the same mount as `config/input.ron`). A missing file, or an app without a `Vfs` resource (a test app), inserts the defaults without an error. A file
that fails to parse or validate is logged once, keeps the defaults, and leaves its error for `render.get_weather`
(`config.error`). The validated config is not re-read during a run. Hot reload is a follow-up.
`weather_config_migration_sources()` and `weather_config_schema_fingerprint()` serve `check-formats`, as the voice chat
config's do.

### 4.3 `regia_vfx::precip` (new module)

```rust,ignore
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct PrecipitationSettings {
    pub enabled: bool,                  // from `effects.precipitation` (default false); then draws whenever it falls
    pub density_scale: f32,             // 0..4, default 1
    pub max_drops: Option<u32>,         // None = the tier budget (§ 5.7)
    pub splashes: bool,                 // default true (tiers with a splash budget)
    pub occlusion: bool,                // default true: ask the renderer for the occlusion view
    pub streak_exposure_s: f32,         // default 1/60
}
pub struct PrecipBudget { pub rain: u32, pub snow: u32, pub splashes: u32, pub half_extent_m: f32, pub height_m: f32,
                          pub occlusion: Option<(u32, f32)> /* texels, extent m */ }
impl PrecipBudget { pub fn for_quality(tier: QualityTier) -> Self; } // § 5.7

/// The per-frame inputs, built on the CPU (no per-drop data).
pub struct PrecipFrame {
    pub eye: [f64; 3], pub eye_velocity_m_per_s: [f32; 3], pub clock_s: f64,
    pub rates: PrecipitationRates, pub wind_m_per_s: [f32; 3],
    pub rain_offsets_m: [f32; RAIN_CLASSES],   // integrated fall, wrapped (§ 5.2)
    pub snow_offsets_m: [f32; SNOW_CLASSES],
    pub drift_m: [f32; 2],                     // integrated wind drift, wrapped
    pub counts: PrecipCounts, pub quality: QualityTier,
}
pub struct PrecipIntegrator { /* f64 offsets per class, wrapped per step */ }
impl PrecipIntegrator { pub fn step(&mut self, rates, wind, dt: f32); pub fn frame(&self, ...) -> PrecipFrame; }

pub struct PrecipitationGpu { /* pipelines (lazy per color format), one uniform buffer, the occlusion binding */ }
impl PrecipitationGpu {
    pub fn new(device: &RenderDevice, profile: &VfxProfile) -> Result<Self, VfxError>;
    pub fn prepare(&mut self, queue: &wgpu::Queue, frame: &PrecipFrame, camera: &VfxCamera, lighting: &VfxLighting);
    pub fn bind_occlusion(&mut self, occlusion: Option<&PrecipOcclusionBinding>);
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat);
    pub fn draw_hdr(&self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat);
}
#[derive(Resource, Clone, Copy, Debug, PartialEq)] // no Default: QualityTier has none
pub struct PrecipitationReport { pub quality: QualityTier, pub rain_drawn: u32, pub snow_drawn: u32, pub splashes: u32,
                                 pub budget: PrecipBudget, pub occlusion: Option<PrecipOcclusionInfo> }

/// CPU mirror of the vertex stage, for tests and agent checks.
pub fn rain_drop(index: u32, frame: &PrecipFrame) -> DropSample;   // eye-relative f32 position (eye is f64), velocity,
                                                                    // diameter, alpha
pub fn snow_flake(index: u32, frame: &PrecipFrame) -> DropSample;
pub fn splash(index: u32, frame: &PrecipFrame) -> SplashSample;    // xz, phase, alpha (height comes from the map)
```

`VfxRuntimePlugin` owns one `PrecipitationGpu` next to its `VfxGpu`. Its existing `TranslucentLayer` implementation
draws the precipitation in the same pass, before `draw_particles`: rain and snow are unsorted, low-alpha and thin, so
sorted effects (smoke, fire) are composited over them. On HDR frames it draws in `draw_hdr`. No second layer is
registered.

### 4.4 `regia_render` (extended)

```rust,ignore
// Surface weather the frame shades with (extracted from regia_scene::weather::SurfaceWeather).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SurfaceWeatherFrame { pub wetness: f32, pub puddles: f32, pub snow_cover: f32,
                                 pub ripple: f32 /* rain rate / max, 0..1 */, pub clock_s: f32 }
impl SurfaceWeatherFrame { pub const DRY: Self; pub fn is_dry(&self) -> bool; }
impl FrameRenderer {
    pub fn set_surface_weather(&mut self, weather: SurfaceWeatherFrame);
    pub fn surface_weather(&self) -> SurfaceWeatherFrame;
    pub fn set_precipitation_occlusion(&mut self, request: Option<PrecipOcclusionRequest>);
    pub fn precipitation_occlusion(&self) -> Option<PrecipOcclusionInfo>;
}
pub struct PrecipOcclusionRequest { pub fall_dir: [f32; 3] }        // unit, pointing down
pub struct PrecipOcclusionInfo { pub size_texels: u32, pub extent_m: f32, pub origin: [f64; 2], pub tilt_deg: f32,
                                 pub redraws: u64, pub last_redraw_frame: u64, pub tile: u32 }
pub struct PrecipOcclusionBinding<'a> { pub atlas: &'a wgpu::TextureView, pub view_proj: [[f32; 4]; 4],
                                        pub uv_rect: [f32; 4], pub extent_m: f32 }

pub struct WeatherResponse { pub porosity: Option<f32>, pub wet: f32, pub snow: f32, pub puddles: bool }
impl WeatherResponse { pub const DEFAULT: Self; /* derived porosity, wet 1, snow 1, no puddles */ pub const NONE: Self; }
// LabMaterial gains `pub weather: WeatherResponse` (DEFAULT).
// SharedFrameInfo gains `pub surface_weather: SurfaceWeatherFrame` and
//                      `pub precip_occlusion: Option<PrecipOcclusionView>` (matrix, uv rect, extent).
// TranslucentLayer gains a provided method:
//   fn bind_precipitation_occlusion(&self, _binding: Option<&PrecipOcclusionBinding<'_>>) {}
//   and fn precipitation_occlusion_request(&self) -> Option<PrecipOcclusionRequest> { None }
pub fn apply_surface_weather(input: SurfaceSample, response: &ResolvedWeatherResponse,
                             weather: &SurfaceWeatherFrame, exposure: f32) -> SurfaceSample; // CPU mirror of § 6
```

`extract_frame` already has 16 parameters (the system limit): `SceneWind`, `SurfaceWeather` and `WeatherAir` join the
existing `(level, aerosol)` tuple (or the builder bundles that tuple into a `SystemParam` struct). The parameter count
stays 16. `regia_render` reads only these `regia_scene` resources, never the
`regia_vfx` weather.

### 4.5 `regia_terrain`

The shared color pass reads `SharedFrameInfo::surface_weather` and `precip_occlusion` (§ 6.4). `TerrainSharedDraw`
gains `surface_weather_report(&self) -> TerrainSurfaceWeatherReport { variant_on: bool, frames_wet: u64 }` (beside its
`layer_albedo_report`), which `render.surface_weather` reads as `terrain_variant_on`.
`TerrainFrame::wetness` and its existing shading stay as they are for hosts that set them directly.

## 5. Drawing precipitation

### 5.1 The volume

Each kind fills an axis-aligned box around the eye: half-extent `H` horizontally, `V` vertically (§ 5.7), with its
center pushed `0.35 H` forward along the horizontal view direction so few drops are spent behind the camera. Drops are
defined in world space and wrapped toroidally into the box (Tariq 2007 describes the wrapped particle box; Wang and Wade
2004 a camera-attached volume). Drop `i` of class `k`:

```text
k        = i mod K                                     // the drop's size class (K = RAIN_CLASSES or SNOW_CLASSES)
seed_i   = hash4(i)                                    // in [0,1)^4, Regia's integer hash (no state); w: size, phase
offset   = (drift.x, -fall_k, drift.z)                 // § 5.2; fall_k grows downward
c        = eye + center_shift                          // the box center
local    = B · fract((B · seed_i.xyz + offset - c) / B) - B / 2      // B = (2H, 2V, 2H)
world    = c + local
```

`world = X - B · floor((X - c) / B) - B / 2` with `X = B · seed + offset`: a drop keeps its world position when the box
moves and only the wrap term changes. It only reappears on
the far side when it crosses a face of the box. Alpha fades over the outer 10 % of each half-extent, so drops crossing a
face are invisible. A camera cut or teleport shows a full, correct field on its first frame: there is no history to
warm up.

### 5.2 Continuity

Integrated offsets, not `speed × time`, so changing rates and winds never make drops jump (the cloud drift rule of
ADR-429):

- `PrecipIntegrator` keeps one `f64` fall distance per class and kind and one `f64` horizontal drift. Each `Update` it
  adds `speed_k · dt` and `wind · dt` (trapezoid over the step, as `Weather::step` does) and wraps each modulo the box size
  in `f64`. Only the wrapped values are uploaded as `f32`, together with `eye mod B` computed in `f64`, so precision does
  not degrade at 100 km from the origin.
- A class's speed changes when the rain rate changes `Λ`. Its offset keeps integrating, so it never jumps.
- **Count changes:** drop `i` is drawn with alpha `1 - smoothstep(0.95 N, N, i)` (N = the drawn count, § 3.2; the
  instance count drawn is `ceil(N)`). A rising
  rate fades drops in and a falling rate fades them out.
- **Kind changes:** rain and snow are two draws, each with its own count from its own rate (§ 3.2) and its own instance
  range, so there is no per-drop kind weight. As the snow fraction rises, the rain count falls and the snow count rises,
  and the count fade above fades drops out of one draw and into the other. Sleet draws both at their rates.
- Snow flakes add a bounded sway `0.15 m · (sin(2π t / P_i + φ_i), cos(...))` with a per-flake period of 1.5-3 s. The
  sway is continuous and its bound is used for culling.

### 5.3 Rain streaks

- Velocity `v = wind3 + (0, -v_k, 0)` (the class fall speed). The streak follows the drop's velocity relative to the
  camera, `v_rel = v - eye_velocity`. The eye velocity comes from the frame's eye delta over `dt`, is clamped to 50 m/s
  and is zeroed on a camera cut. The streak's length is `|v_rel| · streak_exposure_s` (at least 2 cm) and its width is the
  drop diameter. The appearance follows the exposure-time streak model of Garg and Nayar 2006: a drop seen through a
  shutter is a motion-blurred line. No streak database is used.
- **Thin-line anti-aliasing.** A streak narrower than one pixel is widened to one pixel and its alpha is multiplied by
  `true width / pixel width` (the coverage-preserving thin-line technique described by Persson 2012). Far drops therefore
  fade rather than shimmer, and the count can stay modest.
- **Lighting.** A drop reflects and refracts its surroundings. The approximation: the mean of the two sky light rows
  already given to `VfxGpu` (`set_sky_light`: the light from the sky above and from the ground below; when both are zero,
  as in the offscreen harness, `VfxLighting::ambient` instead) times 0.6, plus the sun times a forward-scattering lobe
  (Henyey-Greenstein, g = 0.75, a stand-in for a sphere's forward glint) times the cloud transmittance factor
  `1 - 0.8 coverage`. Brought into the frame's range with the post exposure exactly as `VfxGpu::set_exposure` does for
  other effects. Base alpha 0.25, scaled by `D / 2 mm`, clamped to
  0.1-0.5. Local lights do not light the rain in this spec (§ 16).
- Soft fade against the scene depth (the depth `VfxGpu` already binds), so streaks do not cut into the ground.

### 5.4 Snow flakes

Camera-facing quads of 3-10 mm (per-flake from the seed). They are lit like VFX lit particles: Lambert with the sun
(times the cloud factor) plus the sky rows, albedo 0.85. They are drawn opaque-looking (alpha 0.9) with a soft round
sprite and the same thin-line coverage rule for subpixel flakes. The flake velocity (wind plus fall plus sway) and
`streak_exposure_s` stretch the flake a little at high speeds (`blizzard`).

### 5.5 Splashes

Rain only, on the tiers with a splash budget, and only with the occlusion view. `splashes` instances live in a disk of
radius `0.5 H` around the eye. Each runs a fixed life of 0.3 s on a per-instance phase. At every life cycle `c` it takes a
new position `hash(i, c)` in the disk, snapped to world space (it does not slide with the camera), and maps it to the
occlusion tile's texel there. Its world position is rebuilt from that texel's uv and stored depth through the inverse of
the view's matrix (`textureLoad` on the depth tile in the vertex stage), which is the top surface the rain hits there and
stays exact when the view is tilted. That puts
splashes on roofs, cars and the ground, never in a sheltered spot. The shape is a small upward crown billboard that
widens and fades over its life. The rate scales with `rain_mm_per_h / max_rain`. A splash whose point falls outside the
map is not drawn.

### 5.6 The precipitation occlusion view (`regia_render`)

A depth map of what stands between the sky and a point, so precipitation stops under roofs, bridges and trees, and the
surfaces below them stay dry (Tatarchuk and Isidoro 2006 use the same occlusion idea for rain in a city scene).

- **When:** a layer asks for it (`precipitation_occlusion_request`), or the frame's `SurfaceWeatherFrame` is not dry and
  the tier has one (§ 5.7). Otherwise it is not drawn and costs nothing.
- **View:** orthographic, looking along the mean fall direction (straight down plus the wind slant of the heaviest rain
  class, tilt clamped to 45°; straight down below 10° of tilt). The tilt is quantized to 5° steps. The square covers
  `extent_m` around the eye, snapped to `extent_m / 8` in world space, at `size_texels`² (§ 5.7).
- **Storage:** a reserved tile of the local-light shadow atlas (`shadow_atlas` and a `shadow_tiles` entry). The lab
  forward pass needs no new binding: it finds the tile by an index in the frame uniform. A frame whose tier has no atlas
  allocates the smallest atlas that holds the tile while the view is on.
- **Content:** the shadow-caster draw list (opaque and masked casters, the terrain through its shared shadow-region draw),
  rendered depth-only.
- **Caching:** the tile is one cached page under the paged-shadow rules (`paged_shadow.rs`). It is redrawn when the snapped
  origin moves, the tilt bucket changes, or a caster whose bounds touch the region changes bounds or pose. Spec 88's
  dynamic casters (swaying plants) do not redraw it: a plant's rest pose is a fine shelter. On frames without paged
  shadows (MobileHigh, any unpaged tier) the tile keeps its own cache key instead (snapped origin, tilt bucket, and a hash
  of the bounds and poses of the casters touching the region) and is redrawn only when the key changes. `redraws` and
  `last_redraw_frame` are reported.
- **Atlas space:** the tile is reserved before local lights. Reserving it never drops a local light's tile: when the
  atlas has no room, it grows by the tile's size (one allocation per size change, not per frame).
- **Lookup** (`precip_exposure(p, n)`): the point is first moved 1.5 texels along its geometric normal (a normal offset,
  so a wall is not covered by its own top and wind-driven rain wets facades unless an eave shelters them). Then a 3×3
  PCF comparison against the stored depth with a receiver-plane slope-scaled bias (`regia::shadows::receiver_plane_slope`,
  as the atlas lookups already do) plus a 2 cm constant, so roofs and slopes do not shadow themselves. It returns 1 for
  open sky and 0 under cover, with a soft edge one texel wide (the drip line). Outside the map's area it returns 0 inside
  an `InteriorVolume` (the extraction already gathers them) and 1 elsewhere, so far floors inside buildings stay dry
  and bare.
- **Readback for agents:** `render.surface_exposure` (§ 8) asks for a one-shot copy of the tile into a mapped buffer; the
  answer is available after the next frame. There is no per-frame readback.

### 5.7 Tiers and budgets

| `QualityTier` | rain | snow | splashes | H / V (m) | occlusion view |
|---|---|---|---|---|---|
| MobileLow | 2,000 | 1,500 | 0 | 12 / 10 | off (exposure 1) |
| MobileHigh | 6,000 | 4,000 | 256 | 12 / 10 | 256² over 32 m |
| Deck | 8,000 | 5,000 | 256 | 14 / 10 | 256² over 48 m |
| Low | 12,000 | 8,000 | 512 | 16 / 12 | 512² over 64 m |
| Medium | 24,000 | 16,000 | 1,024 | 18 / 12 | 512² over 64 m |
| High | 48,000 | 30,000 | 2,048 | 18 / 12 | 512² over 64 m |
| Ultra | 96,000 | 60,000 | 4,096 | 22 / 14 | 1024² over 96 m |

Mobile tiers draw the precipitation in `VfxGpu`'s half-resolution soft pass where it is on, the same as alpha VFX. These
numbers are starting points: the builder records the measured cost (§ 12) and the lead may retune them by ADR.

## 6. Surface response

### 6.1 Inputs and exposure

Per pixel, from the frame: `w = wetness`, `q = puddles`, `σ = snow_cover`, `r = ripple`, and `x = precip_exposure(p)` (1
when the frame has no occlusion view). The effective values are `w·x`, `q·x` and `σ·x`, so covered surfaces stay dry and
bare. A sheltered surface next to open ground gets the soft drip edge.

### 6.2 Response per material

`WeatherResponse` (lab) or the terrain layer (§ 6.4) gives:

- **Porosity** `φ`. Default `(1 - metallic) · clamp((roughness - 0.25) / 0.5, 0, 1)`, so concrete, soil and rough stone
  come out porous, while polished, painted and metal surfaces do not.
- **wet** and **snow** scales (`0..1`), and **puddles** (whether flat parts of this material may pool).

### 6.3 The shading rule (applied after the surface stage, before lighting)

Applied to the albedo, roughness, metallic and normal the material (or its `.matgraph` surface function) produced:

1. **Wetness** (`w_e = w · x · wet`). A water film darkens a rough surface through total internal reflection (Lekner and
   Dorf 1988; Jensen et al. 1999) and smooths its specular response:
   `albedo *= 1 - 0.5 φ w_e`; `film = w_e (1 - φ) + 0.6 φ smoothstep(0.7, 1, w_e)`;
   `roughness = mix(roughness, 0.08, 0.9 film)`; normal-map strength `*= 1 - 0.5 film`. Metal albedo is unchanged
   (`φ = 0`), but it still gets the film. The split between porous darkening and the film on sealed surfaces follows the
   public discussion in Lagarde 2012.
2. **Puddles** (only with `puddles` and a geometric normal within about 14° of up, `n.y > 0.97`). The puddle key is
   world-space value noise at 1.5 m and 6 m, plus on terrain a concavity term (§ 6.4). With the key normalized to `0..1`, a pixel is in a puddle
   where the key exceeds `1 - 0.6 q·x`, with a 0.05 soft edge: the lowest spots fill first, and full puddles cover at most
   about 60 % of flat ground. In a puddle: `albedo *= 1 - 0.5 φ`,
   roughness 0.02, metallic 0, and the normal becomes the geometric up normal plus ripples.
3. **Ripples** (in puddles, while `r > 0`): two offset grids of 0.4 m cells. Each cell, in each cycle of
   0.7 ± 0.2 s (hashed per cell and cycle), is active with probability `r` and holds one expanding ring:
   `h = sin(2π (d - ρ(t)) / 0.06) · (1 - a)² · window(d - ρ)`, with `ρ` growing to 0.15 m over the cycle age `a`. The
   gradient is analytic. Loops are fixed at 2 × 4 cells with no data-dependent bounds (FXC-safe, ADR-201). This is
   Regia's own procedural formulation of the ring ripples in the public rain write-ups (Tatarchuk and Isidoro 2006;
   Lagarde 2012).
4. **Snow** (`σ_e = σ · x · snow`). Snow settles on surfaces that face up (Fearing 2000; Ohlsson and Seipel 2004 weight
   accumulation by the surface inclination and an exposure map):
   `m = σ_e · smoothstep(0.5, 0.85, n_geo.y + 0.15 (noise(p / 0.7 m) - 0.5))`. Where `m > 0`: albedo → 0.82
   (fresh snow's visible albedo is about 0.8-0.9; Warren and Wiscombe 1980), roughness → 0.8, metallic → 0, normal →
   `normalize(mix(n, n_geo_up_bent, 0.5 m))`, and the material's normal-map strength `*= 1 - m`. A thin cover
   (`σ < 0.5`) shows the ground through near the noise threshold, as the ground look's patches do. Snow suppresses
   puddles and ripples under it.

Zero weather is an identity, bit for bit: the CPU mirror `apply_surface_weather` returns its input unchanged when
`SurfaceWeatherFrame::is_dry()`, and the shaders skip the whole block on a uniform branch (§ 6.5).

### 6.4 Terrain and grass

- **Terrain color pass.** The rule of § 6.3 is added inside new `@surface-weather-begin` / `@surface-weather-end`
  markers, compiled out (`without_surface_weather`, the ADR-1452 mechanism) while the frame's weather rows are zero, so
  the dry frame keeps today's pipeline and bytes. A wet or snowy frame compiles the variant once. The project's warm list
  gets it when `config/weather.ron` has `effects.surface` on. Porosity per layer comes from the blended layer roughness by
  the § 6.2 formula, unless the layer stack sets one (§ 7.3). The puddle key adds a concavity term: the height minus the
  mean of four taps at 2 m and at 8 m, scaled so a 5 cm hollow over 2 m counts as fully concave. Puddles then sit in ruts
  and dips before they appear on open flats. The ground look's static snow (ADR-1408) stays: the two snow masks combine
  by `max`. The ground look's local wetness combines by `max` with the weather's.
- **Occlusion on terrain.** The shared color pass binds the atlas view and the tile in its engine group (group 3, a new
  binding next to `engine_cloud`) only in the surface-weather variant.
- **Grass** (stage 3, optional for the builder). Blades darken and gloss with `w·x` like the ground (φ = 0.6), and
  their upper third tints toward the snow albedo by `σ·x · 0.7`. Inside the same markers in `grass.wgsl`.
- **Water bodies** do not get puddles or ripples from this spec (§ 16).

### 6.5 Lab forward and the other shading paths

`lab_forward.wgsl` gains `fn apply_surface_weather(...)` right after the surface stage (built-in or graph) and before
lighting, behind `if (frame.weather.x + frame.weather.y + frame.weather.z > 0.0)`: a dynamically uniform branch, with
no new permutation. If the measured off-cost on the 5080 exceeds the budget (§ 12), the builder moves it behind a
permutation bit under the permutation policy (master plan § 5.5a, lane D5) and records that in an ADR. The `LabFrame`
uniform gains `weather: vec4` (wetness, puddles, snow cover, ripple) and `weather2: vec4` (clock, occlusion tile index
or -1, the occlusion extent, 0). Every other path that shades `LabMaterial`s (the visibility-buffer resolve, spec 98)
calls the same function from a shared WGSL module. The path tracer does not: reference comparisons keep the weather dry
(§ 16).

## 7. Files and formats

### 7.1 `config/weather.ron` (new, `format_version` 1)

Optional, per project, at `assets/config/weather.ron` (read as `config://weather.ron`; packaged with the rest of
`config/**`). A missing file means every default below. The loader rejects a newer `format_version`
(`weather_config_version`) and keeps the defaults, logging once. Unknown fields are an error (`weather_config_invalid`,
naming the field). A config preset whose name clashes with a built-in preset is an error too.

```ron
(
    format_version: 1,
    // Named weathers beside the built-in ones. Each starts from `base` and replaces the given fields.
    presets: [
        (name: "monsoon", base: "storm", temperature_c: Some(27.0), precipitation: Some(1.0), gustiness: Some(0.5)),
    ],
    // Which effects run (§ 1). Each defaults to false when absent, so a file that omits `effects` changes nothing
    // that a project without the file would not do; `regia new` writes all three as true.
    effects: (precipitation: true, surface: true, scene_wind: true),
    // A preset to start in when a level opens without a Weather resource (None: leave the sky alone, as today).
    start: None,
    precipitation: (
        max_rain_mm_per_h: 50.0,
        max_snow_we_mm_per_h: 5.0,
        snow_all_below_c: 0.0,
        rain_all_above_c: 2.5,
        snow_ratio: 10.0,
        visible_drop_mm: 1.0,
        density_scale: 1.0,
        streak_exposure_s: 0.0167,
        splashes: true,
    ),
    wind: (
        mode: Weather,          // Weather | Manual | Off: the SceneWind mode the plugin inserts
        surface_factor: None,   // None: (10 m / cloud base)^(1/7)
        veer_deg: 0.0,
        gust_scale: 1.0,
    ),
    surface: (
        wet_mm: 0.2,
        dry_time_s: 1800.0,
        drain_mm_per_h: 2.5,
        puddle_mm: 5.0,
        puddle_dry_time_s: 3600.0,
        full_cover_m: 0.03,
        melt_mm_per_c_day: 4.0,
        melt_scale: 1.0,
    ),
    visibility: (couple_fog: true, rain_visibility_m: 3000.0, snow_visibility_m: 400.0),
)
```

Every field has a serde default, so a file can name only what it changes. Ranges are validated: every rate, time and
size is positive and finite, `snow_all_below_c < rain_all_above_c`, `gust_scale` and `density_scale` are in `0..=4`.
Documented in `docs/formats/weather-ron.md` with a JSON Schema from reflection. Registered in `xtask` `check-formats`
(`WEATHER_CONFIG_FORMAT_VERSION`, schema fingerprint, migration sources: none at v1). Row in `12-assets.md` § 4a: frozen
on merge. Migration note: new format, nothing to migrate; `regia upgrade-project` learns the format so later versions can
migrate it.

### 7.2 No scene format change

The weather stays host state (a resource), as in ADR-429. A level that wants its own weather names a preset through
`config/weather.ron` `start`, or a game system sets it. A per-level weather field in `.level.ron` is a later,
separate decision (§ 17).

### 7.3 Phase B5: per-material and per-layer overrides (format bumps)

The defaults of § 6.2 work without data. Overrides need two frozen-format bumps, each with a migrator and
`upgrade-project` support, and the builder does them last:

- `.material.ron` v3 → v4: an optional `weather: Some((porosity: Some(0.6), wet: 1.0, snow: 1.0, puddles: true))`. The
  migrator rewrites the version only (absent = `WeatherResponse::DEFAULT`). The cooked material goes from v4 to v5 and
  carries the resolved response. `docs/formats/material-v4.example.ron`.
- `.terrain.layers.ron` v3 → v4: optional per-layer `porosity` and `snow` (scale), and a stack-level `puddles`
  (default true). The migrator rewrites the version only.
- `.terrain.layers.ron` v4 → v5 (ADR-3367): the per-layer block's `porosity_from_orm` (default false) reads each
  texel's porosity from the blue channel of the layer's ORM map, where the block names no `porosity`. Opt-in, because
  an ORM map's blue channel is glTF metallic and existing maps must not be reinterpreted; terrain is never metallic,
  so the channel is otherwise unread. The migrator rewrites the version only.

Both follow `12-assets.md` § 4a (DO27 content is produced in parallel: the migrators are tested on a v3 sample of each).

## 8. Agent-protocol surface

All under `render.*`, beside the existing weather methods. Errors: `F0002` for invalid parameters (the message names the
field), `F0003` when no `RuntimeRenderer` exists (for the methods that need one; the hint says what to add). Domain codes
(lowercase): `occlusion_off`, `weather_config_invalid`, `weather_config_version`.

Result additions are additive JSON fields. `render.get_weather` answers with them, and with the longer preset list,
for every host, effects on or off. That is the only visible change for a host with the effects off.

- `render.get_weather` (extended): `current` and `target` gain `temperature_c`, `precipitation_kind`, `gustiness`. New
  `rates: { snow_fraction, rain_mm_per_h, snow_we_mm_per_h, snow_depth_mm_per_h, drop_lambda_per_mm, visibility_m }`
  and `config: { path, format_version, error }`.
- `render.set_weather` (extended): also accepts `temperature_c`, `precipitation_kind: "auto" | "rain" | "snow" | "sleet"`
  and `gustiness`. Presets now include `rain`, `snow`, `blizzard` and the config's presets.
- `render.scene_wind` (read): `{ present, mode, velocity_m_per_s, mean_m_per_s, speed_m_per_s, gust,
  surface_factor, renderer_foliage_wind }`.
- `render.set_scene_wind { mode: "weather" | "manual" | "off", wind_m_per_s?: [x, z] }` → `{ before, after }`. It creates
  the resource when absent. `manual` requires `wind_m_per_s` (two finite numbers, length clamped to 40 m/s as in spec 88).
- `render.precipitation` (read): `{ enabled, quality_tier, kind: { rain, snow }, rates, drawn: { rain, snow, splashes },
  budget: { rain, snow, splashes, half_extent_m, height_m }, density_scale, streak_exposure_s,
  occlusion: null | { size_texels, extent_m, origin, tilt_deg, redraws, last_redraw_frame }, fog_extinction_per_m }`.
- `render.set_precipitation { enabled?, density_scale?, max_drops?, splashes?, occlusion?, streak_exposure_s? }` →
  `{ before, after }`.
- `render.surface_weather` (read): `{ present, mode, wetness, puddles, snow_we_mm, snow_depth_m, snow_cover, ripple, rain_mm_per_h,
  snow_we_mm_per_h, drying_factor, frame: { wetness, puddles, snow_cover, ripple }, terrain_variant_on }`.
- `render.set_surface_weather { mode?, wetness?, puddles?, snow_depth_m?, snow_cover? }` → `{ before, after }`. It
  creates the resource when absent (turning the effect on). Values
  are clamped to their ranges. A value given without `mode` keeps the mode (in `simulated` it sets the integrator's state).
- `render.surface_exposure { points: [[x, y, z], ...] }` (at most 256 points) →
  `{ ready, map_frame, points: [{ p, exposure, cover_y }] }`. `cover_y` is the height of the first occluder above the
  point (null under open sky). The first call schedules a readback and answers `ready: false`; the next frame's call
  answers. With the view off: `occlusion_off`.
- `render.set_foliage_wind` (spec 88, changed): with a `SceneWind` resource it writes `SceneWind` (§ 3.3); the result
  shape is unchanged.

Each new method gets its schema, contract, dispatch and coverage entries, an `agent.guide` topic `weather-precipitation`,
and a `book/COVERAGE.md` row (as do the new format and the new presets).

## 9. Guide page

`book/agents/weather-precipitation.md` (new; `book/agents/weather.md` links to it and loses its "not drawn" sentence). It
covers: the model in one table (preset → rates → what you see), the scene wind and who reads it, the surface state and
how fast it changes, the occlusion view and how to check shelter (`render.surface_exposure`), tiers and budgets, the
config file, and the checks an agent can run:

- Set `rain`, step 300 frames at 1/60 s, read `render.surface_weather`: `wetness` rose by `R · 5 s / (wet_mm · 3600)`.
  Take a screenshot.
- Set `snow` with `paused: false`, step, and watch `snow_cover` rise. A screenshot shows white on flat ground and not on
  walls.
- Put a roof over a point and compare `render.surface_exposure` under and beside it.
- Set `render.set_scene_wind { mode: "off" }` and read `render.foliage_wind`: the wind is null.

## 10. Determinism

- The weather, scene wind and surface weather are pure functions of the transitions a host makes and the sequence of
  `dt`s (`f32`/`f64` arithmetic, no wall clock, no randomness). Two runs of the same build on the same platform with the
  same inputs give the same bits. Across operating systems, `powf`, `exp` and `sin` come from each platform's math
  library and may differ in the last bit. The visual consumers do not care. If the lead wants bit equality across
  platforms (for example, to hash weather state in a networked game), the builder routes the integrators' transcendental
  calls through the `libm` crate (MIT/Apache-2.0, already in `Cargo.lock` as a transitive dependency; a direct use
  needs its `docs/03` row).
- The drawn precipitation is a pure function of the drop index, the uploaded `PrecipFrame` (the CPU's integrated,
  wrapped offsets) and the camera. The same frames draw the same drops. The CPU mirror `rain_drop` / `snow_flake` uses
  the same formulas in `f32`. It is bit-identical across runs on one machine, but not across GPU vendors.
- The occlusion view's redraw decisions depend only on the caster list, the snapped origin and the tilt bucket.
- Networked games replicate the weather commands (transitions), not the per-frame values. Each peer steps the same
  model with the same tick `dt` and arrives at the same state, within the tolerance above (spec 70's message rules).

## 11. Tests and acceptance

**Unit (no GPU)** — `regia_vfx::weather`, `regia_vfx::precip`, `regia_scene::weather`, `regia_render` (CPU mirror):

1. Presets: all twelve in range. The old presets' old fields are unchanged (a regression table). `snow` has snow fraction
   1, `rain` is 0.
2. Snow fraction: forced kinds 0 / 1 / 0.5. `Auto` is 1 at or below 0 °C, 0 at or above 2.5 °C, and monotone between. A
   rain-to-snow transition through `Auto` crosses the band smoothly.
3. Rates: zero at `p = 0`, the maxima at `p = 1`. `overcast` < 2.5 mm/h, `rain` in 2.5-7.6 mm/h, `storm` = 50 mm/h.
   `Λ(R) = 4.1 R^-0.21` to 1e-5. Class diameters increase with `k` and with `R`. Fall speeds match the Atlas fit and lie
   in `[0, 9.65)`. The drawn-count factor is monotone and equals 1 at the maximum. Visibility is
   `3.912 / σ_p`, infinite when dry.
4. Scene wind: the factor at 1500 m is `(10/1500)^(1/7)` to 1e-4. Over 10⁵ frames, `|v| / |mean|` stays in
   `[1 - γ, 1 + γ]`, the per-frame change of a steady mean stays within `1.35 γ |mean| dt + ε`, the same clock gives the same bits, `Manual`
   holds, `Off` makes the renderer's foliage wind `None`, a paused weather holds, and a wind transition carries on
   without a jump.
5. Surface integrator: dry stays dry. Wetting at 5 mm/h reaches 1 in `wet_mm / 5 · 3600` s to within one frame. Drying is
   slower under full overcast than under an overhead sun, and faster with wind. Puddles stay at 0 under drizzle and fill
   only once wet. Snow accumulates at -3 °C and not at +3 °C, melts above 0 °C (faster with sun), and meltwater wets.
   `snow_cover` follows the depth. `Manual` and pause hold. `dt` of 0, negative or NaN is ignored. 10⁶ steps stay in
   range. The same `dt` sequence gives the same bits.
6. Fog coupling: `WeatherAir` holds `σ_p` only while `effects.precipitation` and `couple_fog` are on; the extracted
   frame carries it as its own term; `RenderSettings::fog` is never modified; with the host's fog off the term still
   applies and the host's stored density does not appear.
7. Config: the default round-trips, `effects` absent means all three off (and no file means the same), a newer version gives `weather_config_version`, an unknown field or a preset name
   clash gives `weather_config_invalid`, a missing file gives the defaults, a config preset resolves through `base`, and
   the schema fingerprint is registered.
8. Drop mirror: every drop lies inside its box. A drop's world position at a fixed clock is the same from two eye
   positions that both contain it. Between consecutive frames a drop moves by at most `|v| dt + ε`, except across a box
   face, where its alpha is 0. Count and kind changes fade continuously (alpha is continuous in `N` and `s`). After a
   teleport of 10 km, the occupancy histogram over the box matches a steady frame. At 100 km from the origin, positions
   keep millimeter precision in the eye-relative `f32` position `DropSample` returns (`f64` wrap; the eye is `f64`).
9. Tier table: `PrecipBudget::for_quality` for every `QualityTier` matches § 5.7, and `max_drops` caps it.
10. Surface rule (`apply_surface_weather`): zero weather is a bitwise identity. φ = 0 keeps albedo and φ = 1 at
    `w = 1` halves it. Metal albedo is unchanged. Puddles never form on a wall and need `puddles`. Snow covers an
    up-facing plane and not a vertical one. Exposure 0 is an identity. The ripple normal stays unit length with a
    bounded tilt, and fixed inputs give fixed bits.
11. Agent: every method's defaults; set/clamp/clear; `F0002` naming the field for each bad parameter; `F0003` without a
    renderer; `render.set_foliage_wind` writes `SceneWind` when it exists; `surface_exposure` answers `occlusion_off`,
    then `ready: false`, then values.

**Integration (lavapipe; no goldens needed, the lead decides whether any become goldens):**

- Opt-in: an app with `WeatherPlugin`, the `overcast` weather and no config file inserts no `SceneWind`,
  `SurfaceWeather` or enabled `PrecipitationSettings`, and its frame equals the frame before this spec.
- `regia_vfx/tests/precipitation.rs` (on `VfxHarness`): precipitation 0 records no draw and the frame bytes equal a run
  without the module. `storm` rain changes the frame: coverage in a stated band and not blown out (`measure.rs`
  metrics). `blizzard` snow brightens it. Two harness runs at the same clock give the same bytes. A teleported camera's
  first frame has coverage statistics within 10 % of a steady frame's. A rain → snow transition has no frame whose
  coverage jumps more than a stated bound.
- `regia_render/tests/precip_occlusion.rs`: a slab roof over half the view; drops and splashes draw no pixels in the
  screen region under the roof (a geometric mask). The tile is redrawn on an origin snap and not on a held frame. A
  swaying spec 88 caster does not redraw it. `surface_exposure` reads 0 under and 1 beside the roof.
- `regia_render/tests/surface_weather.rs`: a lab scene (concrete plane, metal sphere, a wall, a roofed patch) with dry
  weather renders bytes equal to the scene without the feature. `wetness = 1` lowers the concrete's mean luminance by
  35-55 % and the metal's by less than 5 %. `snow_cover = 1` brightens the plane and not the wall. The roofed patch keeps
  its dry statistics. Puddles plus ripples change the frame at another clock.
- `regia_terrain/tests/terrain_surface_weather.rs`: rows zero → the variant is off (`variant_on` false) and the bytes
  equal; wet → darker; snow → brighter on flat ground than on a 50° slope; concave ruts pool before flats.
- Ignored benches print the CPU frame time with the module off and at the tier budget (lavapipe numbers are for
  correctness only).

**Acceptance (lead, real GPU; owner by eye):**

- On the 5080 at 1440p Desktop High: the budget rows of § 12 for `storm` rain, `blizzard` snow, the occlusion redraw and
  the surface response on and off.
- Captures for the owner, compared by eye with the owner's reference plates:
  - daytime `rain` over the showcase terrain with a wet road, puddles and ripples;
  - `storm` at dusk;
  - 10 minutes of accelerated `snow` covering the terrain while walls stay dark;
  - a porch that stays dry.
- Pixel 6 at MobileHigh: the § 12 mobile row.

## 12. Performance budgets

- **CPU:** weather drivers and the integrator ≤ 0.02 ms a frame; the precipitation prepare (one uniform) ≤ 0.02 ms. No
  per-drop CPU work, no per-frame allocation.
- **GPU, 5080, 1440p, Desktop High:** `storm` rain at the High budget (48k streaks, 2k splashes) ≤ 0.6 ms; `blizzard`
  (30k flakes) ≤ 0.5 ms; occlusion view redraw ≤ 0.3 ms on the frames it redraws (expected once every several seconds
  for a walking camera; a car at 20 m/s snaps every 0.4 s); surface response on ≤ 0.15 ms over lab and terrain; surface
  response present but dry ≤ 0.02 ms (the uniform branch in lab forward, nothing in terrain).
- **Proposed, not decided (ADR-2557; the owner decides):** the "surface response on ≤ 0.15 ms" row counts the surface
  rule's shading only. The reflection pass gets its own wet row: stochastic SSR on a wet frame ≤ 0.15 ms over the
  same dry frame on the 5080 at 1440p Desktop High, with the SSR cost bound (`SssrCostBound::WET`) on, so a wet
  frame's surface weather costs at most 0.30 ms in all. Measured with the `wet` row of `xtask gpu-bench`, with and
  without `REGIA_SSSR_COST_BOUND=wet`. Until the owner decides, the row above is read as before.
- **Pixel 6, MobileHigh:** rain ≤ 1.0 ms at 6k streaks in the half-resolution pass; surface response ≤ 0.3 ms.
- **Memory:** one uniform buffer per frame in flight and no per-drop buffers. The occlusion tile lives in the atlas (1 MB
  at 512² depth, 4 MB at 1024²).
- Lavapipe measures correctness, counts and redraws only. The GPU rows need the lead's run (commands in the builder's
  report). The results go into `docs/perf-log.md`.

## 13. Platform notes

- Linux, Windows and Android aarch64 compile the same code; nothing is platform-specific.
- The precipitation uses no compute: instanced vertex work only. It runs on every tier and on devices without
  storage-buffer writes in the vertex stage.
- WGSL: no derivatives in the vertex stage, fixed loop bounds (ripples 2 × 4), `textureLoad` on the depth tile in the
  vertex stage for splashes, and no storage writes (FXC-safe under ADR-201).
- Android surface loss: the module holds pipelines and a uniform only. Nothing needs recreating beyond what `VfxGpu`
  already recreates. Tile-based GPUs pay for alpha overdraw, which is why the mobile tiers use fewer drops in the
  half-resolution pass.
- Steam Deck: the Deck row. macOS builds as before (testing only).

## 14. Builder phases

One builder lane (size L) in this order. Each phase ends green and pushed. `docs/notes/weather-precipitation-lanes.md`
has the dependencies and the calendar estimate.

| Phase | Content | Needs | Verifiable on |
|---|---|---|---|
| B1 | § 3 model, `regia_scene::weather`, `config/weather.ron` v1 (and the `regia new` template's file), the three drivers, scene wind → renderer, agent `get/set_weather` extension, `scene_wind`, `set_scene_wind`, `surface_weather`, `set_surface_weather`, the `set_foliage_wind` redirect, guide page | spec 88 (w2) merged | CPU tests, the build server |
| B2 | § 5.1-§ 5.5, § 5.7 precipitation draw without occlusion (exposure 1, no splashes), `render.precipitation` / `set_precipitation` | B1 | lavapipe |
| B3 | § 5.6 occlusion view, drops masked, splashes, `render.surface_exposure` | B2; the `frame.rs` and paged-shadow owner's slot (W-A) | lavapipe |
| B4 | § 6 lab forward and terrain (markers) response, grass stage 3, the § 3.2 fog term in the shaders | B3; the `lab_forward.wgsl` slot; spec 98 (visibility buffer, lane r2) merged or its resolve call left as a follow-up | lavapipe, then lead GPU |
| B5 | § 7.3 `.material.ron` v4, cooked material v5, `.terrain.layers.ron` v4, migrators, `upgrade-project` | B4 | CPU tests |

## 15. Provenance

Public papers and public write-ups only. The formulas above are Regia's combinations of them; the code will be original.

- Drop size distribution: J. S. Marshall and W. M. Palmer, "The distribution of raindrops with size", J. Meteorology 5
  (1948).
- Fall speeds: D. Atlas, R. C. Srivastava and R. S. Sekhon, "Doppler radar characteristics of precipitation at vertical
  incidence", Rev. Geophys. Space Phys. 11 (1973); R. Gunn and G. D. Kinzer, "The terminal velocity of fall for water
  droplets in stagnant air", J. Meteorology 6 (1949); J. D. Locatelli and P. V. Hobbs, "Fall speeds and masses of solid
  precipitation particles", J. Geophys. Res. 79 (1974).
- Rain-snow transition: A. Dai, "Temperature and pressure dependence of the rain-snow phase transition over land and
  ocean", Geophys. Res. Lett. 35 (2008).
- Rain intensity classes: the American Meteorological Society's Glossary of Meteorology ("rain").
- Snow ratio: P. J. Roebber et al., "Improving snowfall forecasting by diagnosing snow density", Weather and
  Forecasting 18 (2003).
- Snow melt: R. Hock, "Temperature index melt modelling in mountain areas", J. Hydrology 282 (2003).
- Visibility: H. Koschmieder (1924), as given in W. E. K. Middleton, "Vision through the Atmosphere" (1952).
- Wind profile: E. W. Peterson and J. P. Hennessey, "On the use of power laws for estimates of wind power potential",
  J. Applied Meteorology 17 (1978). Evaporation drivers: H. L. Penman, "Natural evaporation from open water, bare soil
  and grass", Proc. R. Soc. A 193 (1948).
- Drawing rain and snow: S. Tariq, "Rain", NVIDIA Direct3D 10 SDK whitepaper (2007); N. Tatarchuk and J. Isidoro,
  "Artist-directable real-time rain rendering in city environments", Eurographics Workshop on Natural Phenomena (2006);
  N. Wang and B. Wade, "Rendering falling rain and snow", SIGGRAPH 2004 sketch; K. Garg and S. K. Nayar,
  "Photorealistic rendering of rain streaks", ACM TOG 25(3) (2006); E. Persson, "Phone-wire AA", public article (2012).
- Wet surfaces: J. Lekner and M. C. Dorf, "Why some things are darker when wet", Applied Optics 27(7) (1988);
  H. W. Jensen, J. Legakis and J. Dorsey, "Rendering of wet materials", Eurographics Workshop on Rendering (1999);
  S. Lagarde, "Water drop" blog series, parts 2a/2b, "Dynamic rain and its effects" (2012).
- Snow cover: P. Fearing, "Computer modelling of fallen snow", SIGGRAPH 2000; P. Ohlsson and S. Seipel, "Real-time
  rendering of accumulated snow", SIGRAD 2004; S. G. Warren and W. J. Wiscombe, "A model for the spectral albedo of
  snow", J. Atmos. Sci. 37 (1980).

Rows in `docs/provenance.md` (ADR-1847).

## 16. Out of scope

- Lightning (bolts, flashes and the sky light they cast).
- Rain on the camera lens and screen droplets.
- Rain streaks running down walls and windows.
- Snow deformation (footprints, tire tracks) and snow geometry (displaced drifts).
- Hail.
- Rain shafts under individual clouds.
- Regional weather zones and weather volumes: the weather is global.
- Rain and ripples on water bodies, and the sea state from the weather (gap audit E6).
- Local lights lighting the rain and snow.
- VFX particles drifting in the scene wind (a `.vfx.ron` v6 module with a migrator).
- Cloth reading `SceneWind` (spec 55's wind is a host value today; a small follow-up for the cloth lane).
- An audio rain/wind bed (games read the rates). Built since: `regia_audio::AmbienceBed` reads `SceneWind` and
  `SurfaceWeather` (ADR-2442, spec 16 § 3e).
- A per-level weather field in `.level.ron`.
- Weather in the path tracer and in the baked GI (bakes are dry; wet albedo does not change the probes).
- Reactive-mask hints to the temporal upscalers for thin streaks (ADR on the upscaler seam if TAA smearing shows up
  in review).

## 17. Questions for the lead and the owner

1. **Look sign-off.** The wet, puddle and snow looks need the owner's by-eye review against the reference plates before
   their defaults freeze (phase B4).
2. **Per-level weather.** Is the project `config/weather.ron` `start` enough for DO27-style maps, or should a level carry
   its weather (a `.level.ron` bump)? The spec assumes the former until asked.
3. **Fog coupling default.** On by default (rain and snow thicken the air). A game that hand-tunes its fog per weather
   turns it off in the config.

## 18. As built (ADR-1920)

Built by lane `sprint-weather-build`. Where the code refined the sections above:

- **§ 3.4.** `SurfaceInputs` also carries `snow_ratio` and `max_rain_mm_per_h`, which `SurfaceConfig` does not hold.
- **§ 3.3, § 4.4.** `OutdoorParams` carries the surface state, the extinction and the scene wind, so every host of
  `FrameRenderer::set_outdoor` gets them and the held-camera cache sees a change. `OutdoorParams::scene_wind` holds what the
  renderer's foliage wind is set to (`None` without a `SceneWind`, `Some(None)` for `Off` or no air), and
  `SceneWind::set_explicitly` is `render.set_foliage_wind`'s write. Spec 88 was not on the builder's base: the two call
  sites go in with its merge.
- **§ 4.3.** `PrecipitationGpu::prepare` takes a `PrecipView` (camera, lighting, target), `bind_depth` binds the scene
  depth, and one `draw` serves the display and HDR passes. `translucent_layer_with_precipitation` is the layer for a host
  that owns its `VfxGpu`. `PrecipFrame` also carries the class speeds and diameters, `eye mod B`, the budget and the
  shutter, which the shader needs.
- **§ 4.4.** `PrecipOcclusionBinding` also carries the atlas and tile edges. `FrameRenderer` gains
  `set_precipitation_extinction`, `set_precipitation_occlusion_budget` (the VFX plugin hands it the tier's row, since the
  renderer cannot read `PrecipBudget`), `request_precip_occlusion_readback` and `take_precip_occlusion_readback`
  (`PrecipOcclusionMap`). `SharedFrameInfo` also carries `precip_extinction_per_m`. `SharedPassDraw` gains
  `draw_precip_occlusion` and `set_engine_precip_occlusion`.
- **§ 5.6.** The tile sits in a strip at the right of the atlas, which grows by its edge. A tier whose atlas plus tile
  passes the device's texture limit has no view. One content key decides the redraw on paged and unpaged frames. The
  interior rule beyond the map reads the interior volume that holds the eye (the frame uniform holds one).
  A layer may also ask for the tile aimed along the frame's sun (`TranslucentLayer::sun_occlusion_request`, ADR-3024). That
  is the tile's lowest claim, drawn only on a dry frame, with its own bind hook (`bind_sun_occlusion`) and accessor
  (`FrameRenderer::sun_occlusion`), so rain and `render.surface_exposure` never read it.
- **§ 5.3.** Thin-line coverage stops at 0.5 of a pixel: below that a far drop leaves the 8-bit range. A look default.
- **§ 5.5.** A splash's disk is centered on the eye snapped to 1 m in world space.
- **§ 5.7.** Mobile tiers draw at full resolution for now: the half-resolution pass is a follow-up.
- **§ 6.2.** `LabMaterial::weather` packs into the bits above the flags of the material's `alpha.w`, so the record does
  not grow. Defaults pack to 0.
- **§ 6.3.** The puddle key is `smoothstep(0.2, 0.8, 0.35 n(p / 1.5) + 0.65 n(p / 6))` so its share above a threshold
  follows the threshold. A pool's film keeps the snow rule's order: snow is decided first and suppresses the pool.
- **§ 6.4.** The terrain's rows sit on its material uniform. `terrain.wgsl` holds the text of `surface_weather_wgsl()`,
  and a test keeps the two equal. The ground reaches the tile through a fifth shadow slot.
- **§ 3.2.** The term applies in lab forward, the shared terrain pass and the lab sky. The atmosphere sky has no frame
  fog and takes no term.

### 18.1 Second pass (ADR-2032)

- **§ 3.3.** On main the coupling runs through ADR-1853's `TerrainWind`: `couple_scene_wind` writes its heading and
  strength from the gust-free mean, rounded, and only on a change. The renderer's foliage wind follows when spec 88's
  renderer side merges.
- **§ 6.5.** The Mobile lite forward path shades the rule in a composed variant (`LITE_SURFACE_WEATHER`), not a uniform
  branch, so a dry frame keeps the lite pipelines. The High tier reads the occlusion view; the Low tier has none.
  `set_warm_surface_weather` (§ 6.4's warm switch) also has a dry lite frame build the wet variant of its key.
- **§ 7.3.** `.material.ron` is v5 (the current version was already 4 when this was built), cooked v6, which reads v5
  and v4 cooks. `.terrain.layers.ron` v4 carries the per-layer fields in one optional `weather: Some((porosity,
  snow))` block per layer, as the material does, plus the stack's `puddles`. The terrain blends the blocks by the
  layers' band weights times the splat packs' (the photoreal path) or by the band weights alone (the legacy path),
  inside the weather markers and only when a layer overrides the default; the height blend and the mottle are not
  repeated. Without an override, the response is the one before v4, bit for bit. `render.surface_weather` reports
  each terrain's resolved response (`terrain_layers`).
- **§ 7.1.** The JSON Schema is hand-written, as the material's is, and a test keeps it equal to the serde fields.

### 18.2 Third pass (ADR-3249)

- **§ 6.4, grass stage 3.** Built. The blades take the ground layer's response, not a fixed φ = 0.6: the layers'
  roughness blended by the ground's band weights (times the splat packs on the photoreal path) at the ground
  normal's slope, through `terrain_weather_response`, so a layer's porosity and snow overrides reach its grass.
  The layers' textures are not read for the blades. Snow whitens the upper third by `0.7 σ·x` times the layer's
  snow scale, gated by the ground normal like the ground's own snow. Only the colour pipeline has a weather
  variant (`grass.wgsl`'s `@surface-weather` blocks); depth, velocity and shadow pipelines never shade. It is
  built on the first wet frame or by the terrain's weather warm.
- **§ 5.7.** The precipitation resolution is a setting: `PrecipBudget::resolution` per tier (`Full` on every
  tier until a device measurement moves one by ADR) and `PrecipitationSettings::resolution` at run time. `Half`
  and `Quarter` do not use `VfxGpu`'s soft pass: the precipitation owns its target, draws into it with the scene
  depth tested in the fragment stage (the soft fade), and composites it over the frame.
- **§ 12.** "No per-frame allocation": the VFX layer's own share of a steady rainy frame is 0 (a counting test).
  While only rain or snow keeps the runtime layer live, it leaves out the effects context's passes and draws.
  The renderer's share (its passes for an active translucent layer and the occlusion view) is listed in
  ADR-3249, not changed by it.
- **§ 3.3.** The precipitation's slant already followed `SceneWind`; a test now holds it. Cloth, the fluid
  simulation and the far impostors do not follow it yet.

### 18.3 Conformance audit (ADR-5823)

Recorded by the v0.5.0 spec-conformance lane, where the code already refined the sections above:

- **§ 5.6.** The occlusion view also rounds its azimuth (the horizontal heading of the fall direction) to 15° steps
  (`AZIMUTH_STEP_DEG`), so a gust that turns the wind does not turn the view; only the tilt was quantized above.
- **§ 6.5.** The full lab forward shader leaves the weather block out on Adreno (`FORWARD_TRIM`, ADR-3145), so its
  surfaces stay dry there; the lite shader's `LITE_SURFACE_WEATHER` variant (§ 18.1) still shades the rule.
- **§ 6.5, spec 98.** The visibility-buffer resolve (`vis_resolve.wgsl`) shades through `shade_surface`, which is the
  lab forward body under `VISBUFFER`, so the weather block and its uniform branch are shared and not copied.
  `regia_render/tests/visbuffer_surface_weather.rs` holds it on lavapipe: a dry resolve equals the resolve without the
  feature byte for byte, `wetness = 1` darkens the concrete by the forward path's amount and not the metal, and snow
  settles on the floor and not the wall.
- **§ 11, opt-in.** The opt-in check is split in two: `with_no_config_nothing_turns_on` holds the resources, and
  `with_no_config_the_renderer_gets_the_frame_without_the_feature` holds the extracted frame (dry surface bit for bit,
  no precipitation extinction, no scene-wind override, nothing drawn); the dry-frame bytes are held by the two
  `regia_render` surface-weather suites. A whole-app frame comparison is left to v0.5.x.
- **§ 8.** `render.precipitation` `drawn.splashes` counts a splash as drawn only while the renderer reports an
  occlusion view (§ 5.5: the splash shader culls every splash without it); `wanted.splashes` keeps the weather's ask.
