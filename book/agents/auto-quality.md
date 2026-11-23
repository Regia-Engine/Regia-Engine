# Auto quality (agents)

On a first launch the engine picks the quality tier from the GPU, can confirm it with a ~3 s probe
of the first scene after the splash (off by default since v0.1.0, ADR-173), and saves it in `GameSettings.graphics`. The full description is the
[Automatic quality detection](../user-guide/auto-quality.md) page. Guide topic:
`agent.guide {"topic": "quality"}`.

## Methods

- `quality.get_detected` returns:
  - `setting`: `{ preset, source }`. `source` is `unset` (a first launch before detection),
    `detected` or `player`.
  - `detected`: the saved record, `null` before any detection. It holds `tier` (after the probe),
    `detected_tier` (the table's), `reason`, `rule` (the table row or `fallback`), `evidence`
    (`measured`, `by_class`, `guess` or `fallback`), `gpu`, `pci_id`, `vram_mb`, `driver`, and `probe`
    (`{ p50_ms, p95_ms, budget_ms, frames, clock, size, stepped_down, verdict, monitor_size,
    window_mode, size_mismatch }`). Since v0.5.1 `monitor_size` is the physical size of the
    monitor the window was on (null when unknown), `window_mode` the mode the host asked for
    (`windowed`, `borderless_fullscreen`, empty without a window), and `size_mismatch` is true when
    fullscreen was asked for but the probe drew smaller than the monitor (the window had left
    fullscreen or was not there yet): its numbers are not the monitor's, so it steps nothing down
    (`stepped_down` false, the preset and `tier` unchanged, no re-warm) and the probe at the monitor
    size runs from the same tier.
  - `step_down` (v0.5.1, ADR-7882): why the saved tier is below the table's pick or below `Low`,
    `null` when it is neither: `{ from, to, by, why, budget_ms, below_low }`. `by` is `probe` (the
    confirmation probe stepped down; `why` is its verdict, such as "p95 75.0 ms > 33.3 ms budget:
    Low misses 30 fps, Low → VeryLow (Desktop Low at 33 % render scale, SGSR upscale)", and
    `budget_ms` the budget it missed) or `table` (a GPU table row picked `VeryLow`; `why` is the
    detection's reason, `budget_ms` null). `below_low` is whether the tier is `VeryLow`.
  - `this_launch`: `detections` (0 when the saved preset was read), `probing`, `redetect_pending`,
    `reprobe_pending` (v0.5.1: a `size_mismatch` probe runs again on the first frame the window
    fills the monitor),
    `reason`, `verdict`, and the configuration (`enabled`, `confirm`, `target_fps`,
    `probe_seconds`).
  - `render_tier`: the tier the live `RenderSettings` match, `null` when the game customized them.
  - `probe_active` and `adapter`: the live adapter profile (name, vendor, device, class,
    `vram_mb`, limits).
  - `host`: the app's own detection when it picks its tier without `GameSettings`
    (`regia_agent::HostQualityDetection`), `null` otherwise. The Regia Forest Demo answers
    `{ tier, source, detection }` there, the same as `demo.state`'s `quality` (ADR-5131).
- `quality.detect` without `adapter` re-detects the running GPU on the next frame, like the
  options menu's Auto-detect. It replaces `graphics.preset` (even the player's), saves it, and
  runs the probe when `confirm` is on. It returns the tier and reason it computes now, plus
  `applied: "next_frame"`, `replaces` (the old `{ preset, source }`) and `probe`. Mutating;
  permission `edit_scene`.
- `quality.detect {"adapter": {...}}` is a dry run for any GPU: `{ name*, vendor, device, class,
  vram_mb, android }`, where `class` is `discrete` (default), `integrated`, `virtual`, `cpu` or
  `other`. It returns `{ tier, reason, rule, evidence, note, caps, adapter, applied: false }` and
  changes nothing.

```json
{"method": "quality.detect", "params": {"adapter": {"name": "NVIDIA GeForce RTX 4070", "vendor": 4318, "vram_mb": 12282}}}
```

## Checking a launch

1. Delete the settings file (`user://settings.ron`) and launch. Detection runs on the first frame
   with a device. The confirmation probe is off by default (ADR-173). When a host opts in with
   `with_confirm(true)`, wait for it: it draws about 3–4 s of the first scene offscreen after the
   splash (`quality.get_detected` → `probe_active` false).
2. `quality.get_detected`: `setting.source` should be `detected`, and `detected.reason` should name
   the GPU and tier. `detected.probe.verdict` says whether the probe stepped down.
3. `render.get_profile` shows the preset in effect. `Medium` shows `preset: null`, because it is
   Desktop High at `render_scale` 0.67. `VeryLow` shows `preset: "DesktopLow"` with
   `render_scale` 0.333 and `spatial_upscaler` SGSR (ADR-7882): it is Low drawn cheaper, so systems
   that pick their tier by preset stay on Low's. `render_tier` in `quality.get_detected` names it.
   On Linux, on a non-AMD discrete GPU with 6 GB or more, `Low` still shows `preset: "DesktopLow"` even though it also
   turns on the terrain virtual texture (v0.1.2). `terrain.vt` should then report the texture as on,
   with `vt hit rate` near 100 %. On `Medium`, `High` and `Ultra` it reports the texture as off,
   unless the host turned it on itself or opted High in through `quality.set_terrain_vt_policy`
   ([terrain virtual texture tiers](terrain-vt-tiers.md)).
4. `play.hitch_report {"seconds": 10}` confirms the frame time at that tier.
5. Relaunch: `this_launch.detections` must be 0, because detection runs once.
6. To check that the player's choice is kept, run `settings.set {"path": "graphics.preset",
   "value": "Ultra"}`. `quality.get_detected` then shows `source: "player"`, and a relaunch keeps
   Ultra.

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
quality.get_detected {}         # setting.source, detected.reason, step_down, probe_active
quality.detect {"adapter": {"name": "Intel(R) Graphics", "vendor": 32902, "device": 18132, "class": "integrated"}}
                                # dry run: the N150 class -> VeryLow (row intel_n_series)
```

### More `quality.detect` parameters

The schema (`agent.schema`) also lists these optional parameters:

- `apply` (boolean): Re-detect and replace the saved preset (default true without `adapter`).

## Minimum-spec profile explanation

`renderer.minspec_explain {"profile":"pascal"}` or `{"profile":"mobile"}` returns the named
renderer profile and two reason lists. `features` names optional wgpu features omitted from that
profile's mask; `limits` names each clamped limit field and the cap applied by the existing
minimum-spec clamp. The explanation is generated from the profile's mask and clamp, so it follows
those values directly. Unknown names return `minspec_profile_unknown` (JSON-RPC invalid params);
use the lowercase profile names `pascal` and `mobile`.

## Packaged profile provenance (v0.5.1, ADR-7893)

`quality.get_detected.configuration` reports the configured starting profile's source,
`packaged_profile`, metadata `location`, and any metadata `error`, with the same shape as
[render.get_profile](render-profile.md#packaged-starting-profile-v051-adr-7893).
`setting` continues to describe the saved player/Auto record; it can differ from the live profile
when `configuration.source` is `packaged`. A package suppresses automatic detection, including a
saved detected preset. Player settings win; explicit `quality.detect` overrides the package for
this launch. A new launch uses the package again unless the player saved a preset.
