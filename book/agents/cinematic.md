# Cinematic profile (agents)

`Cinematic` is an offline render profile above Desktop Ultra (v0.2.0, ADR-1403). It is meant for
`render.out` sequences, stills and trailers, where frame time does not matter and every frame has to
stand alone. Any game can use it. It is not in the in-game preset list (`RenderProfilePreset::ALL`),
so no options menu and no quality detector picks it. Guide topic: `agent.guide {"topic": "profile"}`.

## What it changes against Desktop Ultra

| Setting | Desktop Ultra | Cinematic |
|---|---|---|
| Cloud reprojection | on (one texel in 16 per frame) | off: every texel is marched every frame |
| Cloud view-march steps | 150 % (64 → 96, ceiling 96) | 300 % (64 → 192, ceiling 256) |
| Cloud march resolution | the layer's divisor (2 by default) | full resolution |
| Cloud marches per texel | 1 | 4, with stratified start jitter, averaged in linear light |
| Sun cascades | 4 × 4096² | 4 × 8192² |
| Near sun tile | 4096 | 4096 (unchanged, see below) |
| Lights uploaded / per cluster | 512 / 256 | 512 / 256 (the engine maximums) |
| Mesh LOD (bias × distance scale, shadow scale) | 1.15 × 6, 0.15 | 2 × 8 = 16 (the ceiling), 1 |
| Terrain anisotropy | 16 | 16 |
| Render scale, AA | native, TAA | native, TAA |

Everything else is Ultra's.

The near sun tile stays at 4096. It covers only the first 4 m of the view, and an 8192 tile would
take the whole 8192² local shadow atlas, so local lights would lose their shadows.

**Why the clouds are clean.** The real-time presets march each cloud texel once, from a start
offset that changes per pixel and per frame (interleaved gradient noise). TAA and the cloud
reprojection average that offset away over frames. Without a temporal resolve (a plain look with
AA off, or the first frame after a cut), the offset shows as dither at cloud edges and in thin
gaps.

Cinematic marches each texel `n` times instead (4 by default). March `k` starts in its own stratum,
`(offset + k) / n`, and casts its ray through point `k` of the R2 sequence inside the texel. The
average sits within `1/n` of a step of the exact integral, where one march can be off by up to a
whole step.

The result depends only on the frame index. Two runs of the same frames are identical with
reprojection off. With render-out subframe accumulation each subframe has its own frame index, so
the accumulation averages the cloud jitter further.

## Methods

`render.set_profile` (see [Render profile](render-profile.md)) takes the preset and these optional
fields:

- `preset: "Cinematic"`. Any spelling works (`cinematic`, `CINEMATIC`).
- `cloud_samples` (integer 1..16): cloud marches per texel. Works on any preset.
- `cloud_step_percent` (integer 1..400): percent of the layer's view-march steps. Giving it lifts
  the step ceiling from 96 to 256.
- `cloud_reprojection` (boolean).
- `cloud_max_divisor` (integer 1..8): the largest resolution divisor of the cloud march. 1 is full
  resolution (Cinematic); 8 keeps the cloud layer's own divisor (every real-time preset).
- `force` (boolean, default false): run Cinematic on a desktop GPU tier below High.

Without `preset`, the cloud fields change the live profile on their own, and the profile then
reads as customized (`preset: null`).

When a preset is given, the result carries
`gate: { requested, applied, honoured, reason, tier }`. Cinematic is gated by platform and by the
tier the quality detector gives the running GPU:

| Where | Applied | `reason` |
|---|---|---|
| Android (even with `force`) | `MobileHigh` | `cinematic_needs_desktop` |
| Tier Deck, Low, Medium (or a mobile tier on desktop) | that tier's preset (`Deck`, `DesktopLow`, `DesktopHigh`) | `cinematic_tier_too_low` |
| Same, with `"force": true` | `Cinematic` | `null` |
| Tier High or Ultra, or no adapter (headless tools) | `Cinematic` | `null` |

A software rasterizer (lavapipe) is the Low tier, so it needs `"force": true`.

`render.get_profile` reports the march as
`profile.cloud_march: { samples, max_steps, max_divisor }`, plus `profile.csm_resolution`,
`profile.near_sun_tile` and `profile.max_visible_lights`. `presets` lists every accepted name,
Cinematic included. `offline_presets` is `["Cinematic"]`.

Parameter errors are `F0002` (invalid params): `cloud_samples` outside 1..16, `cloud_step_percent`
outside 1..400, `cloud_max_divisor` outside 1..8, or a non-boolean `cloud_reprojection` or `force`.

```json
{"method": "render.set_profile", "params": {"preset": "Cinematic"}}
{"method": "render.set_profile", "params": {"preset": "Cinematic", "force": true}}
{"method": "render.set_profile", "params": {"cloud_samples": 8, "cloud_step_percent": 400}}
{"method": "render.set_profile", "params": {"cloud_samples": 4, "cloud_step_percent": 300, "cloud_reprojection": false, "cloud_max_divisor": 1}}
```

## Choosing the profile from the command line or a scene directory

Hosts call `regia_render::cinematic::take_preset_arg(args)`. It removes
`--render-profile NAME` (or `--render-profile=NAME`) from the arguments, so the host's own parser
never sees it. The host then calls
`choose_preset(cli, env::var("REGIA_RENDER_PROFILE").ok().as_deref(), Some(scene_dir), default)`
and `gate_preset(...)` before `RenderSettings::from_profile(...)`.

The first match wins: the command line, then `REGIA_RENDER_PROFILE`, then the scene directory's
`render_profile.txt`, then the host's default. `render_profile.txt` holds one preset name; blank
lines and `#` comments are ignored. `PresetChoice::source` reports which one won
(`command_line`, `environment`, `scene_dir`, `default`).

Error codes (`PresetChoiceError::code`):

| Code | Cause |
|---|---|
| `render_profile_missing_value` | the flag has no name after it |
| `render_profile_unknown` | the name is not a preset |
| `render_profile_unreadable` | `render_profile.txt` exists but cannot be read |

## Cost

Measured with the ignored bench `cinematic_cost_at_1440p_and_4k`
(`crates/regia_test_utils/tests/cinematic_clouds.rs`) on an RTX 5090, 2026-10-22. The scene is a
lab scene: a ground plane and a ridge under a broken layer (coverage 0.45), the eye moving every
frame. Each value is the median GPU time of 60 frames after 30 warm-up frames. The scene has no sun
shadows by default (`CINEMATIC_BENCH_SHADOWS` turns them on), so the 8192² cascades are not in
these numbers.

| Profile | 1440p frame | 1440p clouds | 4K frame | 4K clouds |
|---|---|---|---|---|
| Desktop Ultra | 1.16 ms | 0.45 ms | 1.98 ms | 0.65 ms |
| Cinematic, 1 sample | 7.11 ms | 6.39 ms | 14.21 ms | 12.86 ms |
| Cinematic, 2 samples | 13.41 ms | 12.68 ms | 26.84 ms | 25.50 ms |
| **Cinematic (4 samples)** | **25.84 ms** | **25.11 ms** | **51.80 ms** | **50.45 ms** |
| Cinematic, 8 samples | 50.10 ms | 49.36 ms | 101.83 ms | 100.48 ms |

With sun shadows on and the paging off, so that all four cascades are redrawn every frame, the
shadow pass on this light scene costs 0.10 ms at Ultra's 4096² and 0.22-0.25 ms at Cinematic's
8192². With paging on (every preset), the cached cascades cost about 0 ms while the camera creeps.

On lavapipe (llvmpipe, 2 timed frames) a 4K frame takes 1.06 s of GPU time for Ultra and 8.0 s for
Cinematic (wall time 1.8 s and 16.2 s).

The cloud pass grows linearly with the sample count and the pixel count. A 4K Cinematic frame
takes about 52 ms of GPU time, so a 120-frame warm-up plus one still is about 6 s on that card.
Supersampling (`render.out_start` `supersample`) multiplies the pixel count again.

To run the bench:

```sh
cargo test -p regia_test_utils --test cinematic_clouds -- --ignored --nocapture
```

Set `REGIA_DUMP_DIR` to also write the 4K plain-look frames and a crop of the cloud base.

## Example calls

```text
render.set_profile {"preset": "Cinematic"}                # offline profile (gated by tier)
render.set_profile {"preset": "Cinematic", "force": true} # also on a Low-tier desktop GPU
render.set_profile {"cloud_samples": 8}                   # more cloud samples on the live profile
render.get_profile {}                                     # profile.cloud_march, offline_presets
```
