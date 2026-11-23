# Render profile (agents)

The render profile is the quality tier the renderer runs (`specs/13` § 3): render scale, AA,
upscaler, shadows, post. A game picks one through `RenderSettings`; without one the platform
default runs: **Mobile High on Android**, Desktop High elsewhere. Guide topic:
`agent.guide {"topic": "profile"}`.

## Methods

- `render.get_profile` returns `{ preset, tier, profile, post_enabled, platform_default, runtime_default,
  presets, profile_names, offline_presets, thermal }` (the full shape, every `profile` field included, is in
  `agent.schema`; the fields below are the ones most hosts read).
  `preset` is the preset name (`MobileLow`, `MobileHigh`, `DesktopLow`, `DesktopHigh`,
  `DesktopUltra`, `Deck`), or `null` when the game customized the quality fields. Runtime
  `temporal_upscaler` and `render_scale` choices preserve the preset and its gameplay caps
  (v0.5.1); Very Low still reports `DesktopLow`. Other quality overrides, including AA,
  can make the profile custom. `tier` (v0.7, ADR-8844) is the quality-ladder tier the profile is
  (`QualityTier::of_settings`: `VeryLow`, `Low`, `Medium`, `High`, `Ultra`, `Deck`, `MobileLow`,
  `MobileHigh`), or `null` for a profile no tier produces; it is how Very Low is told apart from
  Desktop Low, since both report `preset: "DesktopLow"`. `profile_names` lists every name
  `render.set_profile` takes (see [Profile names](#profile-names-v07)). `profile` holds
  `{ render_scale, anti_aliasing, spatial_upscaler, depth_prepass, bloom, auto_exposure,
  terrain_anisotropy, cloud_reprojection, cloud_step_percent, cloud_cone_light, mesh_lod_bias,
  mesh_lod_distance_scale, mesh_lod_shadow_scale }`. The three `mesh_lod_*` values are the mesh LOD
  tier (v0.1.20; see [Mesh LOD tiers](mesh-lod-tiers.md)).
  `terrain_anisotropy` is the terrain's anisotropic filtering level (`TerrainBudget::anisotropy`: 0 = off, Mobile High 2,
  Desktop Low and Deck 4, Desktop High 8, Desktop Ultra 16; v0.1.8). `cloud_reprojection` is
  whether volumetric clouds march one texel in 16 per frame and reproject the rest (on for every
  preset), and `cloud_step_percent` scales the cloud view-march steps (Desktop Ultra 150, so
  64 → 96 steps; the others 100; batch v020). `cloud_cone_light` is the cone light march with
  dark edges and curl detail (Desktop High and Ultra; off on the presets without clouds;
  batch v020). See `agents/clouds.md`.
  `thermal` (batch v020) is `{ status, headroom, governor_enabled, governor_level, base_render_scale }`:
  Android's thermal status (`None` … `Shutdown`), the 10-second headroom forecast (1.0 = severe, `null`
  when the device does not report it), and the thermal governor's state. While `governor_level` is above
  0 the governor holds `profile.render_scale` below `base_render_scale`; this scale change
  preserves `preset`, so ragdoll and vehicle-FX limits continue to follow the selected tier.
  `platform_default` is the build's default preset (Mobile High on Android, Desktop High elsewhere); cooks
  and permutation lists use it. `runtime_default` (v0.2.16, ADR-3289) is what `RenderSettings::default()`
  picks on this device when the game names no preset: `Deck` on a Steam Deck (`SteamDeck=1`, as Steam sets
  it, or a Valve `Jupiter`/`Galileo` board), else the same as `platform_default`.
- `render.set_profile` switches to a preset on the next frame. It re-seeds the toggles the preset
  owns (bloom, AA, auto exposure) and keeps the rest of the post stack and the fog. `post`
  (optional boolean) turns the post stack on or off: samples that render forward-only for
  lavapipe screenshots need `"post": true` for SMAA and the SGSR upscale to run.
  `mesh_lod_distance_scale` and `mesh_lod_shadow_scale` (optional numbers, v0.1.20) override the
  preset's mesh LOD tier scales, clamped to 0.25..16 and 0.05..1; with one of them, `preset` may be
  left out. `preset` also takes the tier names of [Profile names](#profile-names-v07). The switch
  is not persisted. Returns `{ before, after }` (each with `preset` and `tier`), plus `gate` and
  `requested` (the canonical name asked for) when a preset was named. Mutating; permission
  `edit_scene`. `render.mesh_lod` prints the switch distances the scales give (see
  [Mesh LOD tiers](mesh-lod-tiers.md)).

```json
{"method": "render.set_profile", "params": {"preset": "MobileLow", "post": true}}
```

## Profile names (v0.7)

Wherever a render profile is named (`render.set_profile`, `--render-profile`,
`REGIA_RENDER_PROFILE`, a scene's `render_profile.txt`, `Regia.toml` `[platforms.*]
render_profile`, the editor's Project Settings), the name is a `ProfileName` (ADR-7977, ADR-8844):

- a preset row: `MobileLow`, `MobileHigh`, `DesktopLow`, `DesktopHigh`, `DesktopUltra`, `Deck`,
  `Cinematic`; or
- a quality-ladder tier that has no row of its own: `VeryLow` (the integrated-GPU floor, Desktop
  Low drawn at a third with SGSR, `specs/13` § 3a) and `Medium` (Desktop High at 0.67).

Spelling is free as for presets (`VeryLow`, `very_low` and `very-low` all match); the canonical
name is what is written back. `Low`, `High` and `Ultra` stay unknown names, because those tiers
are rows already (`DesktopLow`, `DesktopHigh`, `DesktopUltra`) and no row gets a second name. A
tier applies as auto quality applies it, with the live adapter deciding the GPU levers. Very Low
gates as Desktop Low (it is never gated) and reports `preset: "DesktopLow"`, `tier: "VeryLow"`, so
water, foliage and the other systems that pick their tier by preset stay on Low's.

```json
{"method": "render.set_profile", "params": {"preset": "VeryLow"}}
```

Errors: an unknown name is `invalid_params`; its message names the bad name and lists
`profile_names`. The command-line, environment and scene-file readers (`choose_profile`,
`take_profile_arg`) report `render_profile_unknown`, `render_profile_missing_value` or
`render_profile_unreadable` as the preset readers do; every command that reads `Regia.toml`
(`regia package` among them) refuses an unknown `render_profile` with `render_profile_unknown`.

## Offline Cinematic profile (v0.2.0)

`render.set_profile {"preset": "Cinematic"}` switches to the offline profile above Desktop Ultra
(ADR-1403): every cloud texel marched every frame at full resolution with 4 stratified samples,
8192² sun cascades, the light caps at their maximum, mesh LOD toward LOD0. Android and GPU tiers
below High get their own preset instead unless `"force": true`; the result's `gate` says what ran.
`cloud_samples`, `cloud_step_percent`, `cloud_reprojection` and `cloud_max_divisor` override the
cloud march on any preset. See [Cinematic profile](cinematic.md).

## Stochastic SSR roughness cutoff (v0.1.14)

Stochastic SSR (Desktop High and Ultra) traces reflections only on surfaces smoother than a
perceptual roughness cutoff, and fades them out over the 0.15 below it. The default is 0.55. A
higher cutoff keeps sharp reflections on rougher surfaces (wet asphalt, worn metal) and traces
more pixels.

- `render.get_sssr_cutoff` returns `{ cutoff, default, env_override, env, range, fade_band }`.
  `cutoff` is the value in effect.
- `render.set_sssr_cutoff {"cutoff": 0.7}` sets it for every renderer from the next frame,
  clamped to `range` (0.15..1); 0.55 (`default`) restores the shipped look. Not persisted. Returns
  `{ before, after, stored }`. Mutating; permission `edit_scene`.
- `REGIA_SSSR_ROUGHNESS_CUTOFF=0.7` in the environment beats the setting (read once at the
  first frame); `env_override` shows it.

To judge a value, `seeing.screenshot` the same view at two cutoffs and compare the GPU `ssr` pass
in `play.hitch_report`.

```json
{"method": "render.set_sssr_cutoff", "params": {"cutoff": 0.7}}
```

## Comparing tiers on a phone

1. `regia install-android --project <game>` installs, launches and attaches the agent
   protocol through `adb forward` (`--attach` re-attaches to a running app).
2. For each preset: `render.set_profile`, wait a second, `play.hitch_report {"seconds": 10}` for
   frame time p50/p99, and `seeing.screenshot` for the look.
3. `seeing.memory_report` gives GPU memory by category and the process RSS.

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
render.get_profile {}           # { preset, tier, profile, post_enabled, platform_default, runtime_default, presets, profile_names }
render.set_profile {"preset": "VeryLow"}   # after: { preset: "DesktopLow", tier: "VeryLow" }
```

## Packaged starting profile (v0.5.1, ADR-7893)

`render.get_profile.configuration` is `{ source, packaged_profile, location, error }`.
`source` is `player`, `packaged`, `detected`, or `runtime_or_host`. `packaged_profile` is the
exact profile name from `build_info.json` (a preset, or `VeryLow`/`Medium` since v0.7), or null; `location` is its desktop path or
`apk:assets/build_info.json`, and `error` is null or a diagnostic for ignored metadata.
This describes the configured starting profile; a host or `render.set_profile` can customize
the live profile afterward, which the existing `preset` and `profile` fields show.

Startup precedence is player settings > packaged profile > saved or first-launch Auto > runtime
default. An explicit `quality.detect` overrides the package for this launch; a subsequent launch
uses the package again unless the player saved a preset. `quality.get_detected.configuration`
returns the same provenance. See [Packaging](../user-guide/packaging.md).
