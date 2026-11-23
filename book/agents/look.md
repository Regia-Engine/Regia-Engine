# Map look: exposure meter and color grade

Every map can carry its own exposure and grade in `LevelSettings.look` (scene format v3). The
runtime applies it to every view's post stack; a field left `None` keeps the view's
`RenderSettings.post`, so a level that says nothing looks like the engine default.

| Field | Effect |
|---|---|
| `exposure_bias_ev` | Added to the metered (or fixed) exposure; positive brightens. |
| `min_ev100`, `max_ev100` | The auto-exposure clamp (the snow ceiling, the night floor). |
| `meter` | `low_percentile` / `high_percentile` trim the dark and bright ends of the histogram; `center_weight` 0 meters evenly, 1 weights the frame center up to 9×. |
| `contrast`, `saturation` | The display look after AgX (engine default 1.2 / 1.4). |
| `grade` | `white_balance_kelvin` (6500 neutral; higher warms the print), `tint`, then `lift` / `gamma` / `gain` per channel on the display image. |
| `lut` | A project-relative `.cube` (Adobe Cube LUT, 3D) or PNG strip (N slices of N×N side by side, x = r + b·N, y = g) applied after the grade; `None` is the identity. `\` is accepted as a separator. A `.cube` the asset manifest holds loads from the paks or the cooked cache first (a packaged Android client has nothing else), then a loose file under the project root. Loaded once per path (`LevelColorLut`); a failed load warns and keeps the identity. |
| `local_exposure` | Scene format v9, off by default. `shadow_contrast` / `highlight_contrast` (0–1, 1 = unchanged) pull dark / bright regions toward middle grey, `detail_strength` (1 = unchanged) scales the texture on top; an edge-aware bilateral grid in post, so a dark hull lifts as a whole beside sunlit snow. DO27's `local_exposure_shadows 0.75` is `{"shadow_contrast": 0.75}`. |

`LevelAtmosphere.haze` gives the map a dust or mist layer near the floor (`density` 1/m,
`scale_height_m`, `base_height_m`); on Mars it scatters like the planet's butterscotch dust.
`LevelAtmosphere.moons` adds up to two lit moons beside `sky_body`.

## Agent methods

- `level.get_look` returns `{ look }` for the open level.
- `level.set_look` deep-merges a partial object and returns `{ before, after }`:
  `{"look": {"exposure_bias_ev": 0.5, "grade": {"white_balance_kelvin": 6900}}}`. A `null`
  block (`meter`, `grade`) starts from its neutral value. The next frame uses the new look.

The view's own tonemap operator, auto-exposure switch, fixed EV, physical camera, bloom, vignette and
grain are `render.get_exposure_settings` / `render.set_exposure_settings` ([Exposure and tonemap](exposure.md)); the look
is applied on top of them.

Judge a change by eye: `seeing.screenshot` (target `offscreen`) after a few frames for auto
exposure to settle, side by side with the reference plate, then save the level.

## Reference presets

`samples/lookdev` (`lookdev::look_r2`) holds the DO27 presets tuned against the UE v13 plates:
`switchbacks_look` + `switchbacks_haze` (Mars: EV100 held at 10.25–10.75, contrast 1.8, 5800 K +
magenta tint 0.45, a thin 1.2e-5 /m haze) and `bore_twelve_look` (Europa: EV100 held at 11.5–11.7 for
≈ 0.5-reflectance ice, contrast 1.15). Render them with
`cargo run -p lookdev --bin look_r2 -- --out <dir>`.

Hold daylight exposure in a narrow `min_ev100`/`max_ev100` band rather than letting the meter
average the frame. An averaging meter darkens any frame with a bright sky in it by about 2 EV, and
it prints ice at the same brightness whatever its albedo. Put the sun ahead of and to the side of the
camera, not behind it: a sun behind the lens hides every shadow behind the object that casts it.

A warm white balance is not the same as a red print: for an orange (red > green > blue) the hue
angle is 60·(g − b)/(r − b), so cutting blue moves it toward yellow and olive. To push Mars rock
toward red-brown, add magenta `tint` (it removes green) rather than raising the kelvin.

## Frame problems

`render.problems` returns `{ ok, problems: [{ code, message }] }` for the last frame the runtime
renderer drew. `msaa_skips_terrain` means the quality asks for MSAA (`msaa_samples` ≥ 4) while a
shared terrain draw or a planet sky is set: the renderer draws neither on multisampled frames, and
auto exposure then meters what is left (often black, which blows every mesh out). Use
`msaa_samples` 1; anti-aliasing is TAA in post. A host that drives `FrameRenderer` itself reads
`FrameRenderer::frame_problems()`, and `host_checklist_report` lists the same item as an `ERROR`
([Host look checklist](../user-guide/host-look-checklist.md)).

`texture_budget_exceeded` (the material textures resident exceed the tier's texture budget, or
2 GiB with streaming off) and `uncompressed_textures` (more than 64 RGBA8 material textures: a
level of PNG/JPEG instead of cooked BC7/ASTC) name the five largest offenders; the host checklist
lists them as differences, not errors. `assets.texture_memory` has the per-texture detail
([Assets](assets.md#texture-memory-and-the-budget)).

Two GPU health codes (ADR-171) are process-wide, not about one frame:

- `gpu_errors`: wgpu reported uncaptured errors since start (validation, internal, out of
  memory). Release builds log them under `regia::gpu_error` and keep drawing, so a garbled
  frame may follow. The perf overlay shows "GPU errors: N" and `perf_overlay.get` returns
  `stats.gpu_errors`; feedback bundles carry `gpu_errors` in `feedback.json`. Read the log for
  the pass that failed.
- `vram_pressure`: GPU memory ran short (a wgpu out-of-memory error, live allocations above
  85 % of the adapter's VRAM in the telemetry probe, or the OS low-memory event). Every renderer
  freed its idle GPU memory (a dormant stochastic SSR pass's targets). Only out-of-memory errors
  lower the texture streaming budget: since v0.1.16 the first halves it, and each later one
  within 60 s of the previous one halves it again, at most three times and never below 256 MiB.
  One less than 60 s after a trim also holds the streamed textures a mip lower for a minute. The
  message gives the count of trims, budget steps and caught out-of-memory errors, and whether the
  mips are held; [`render.memory_pressure`](low-memory-gpus.md) has the detail.

A lost GPU device has no code: the game stops drawing, writes a crash bundle whose `crash.txt`
starts with `device_lost:`, and exits with code 3 within about 2 s. A window surface
that cannot be created (after one retry) or stays lost for 120 frames does the same with
`surface_unavailable` and exit code 4.

## Large-mesh proxies

`render.mesh_occluders` returns `{ in_use, bands: { near, far, dropped, near_max, far_max },
occluders: [{ center, axes, half_extents, albedo, ground_y, far_band }] }`: the oriented boxes of the entities with `OcclusionProxy` (parked hulls, vehicles,
containers). Each box's sun shadow darkens the ground that meshes beside and on it see, in both
diffuse light and reflections. Each box also replaces the sky it hides from the ground with its
own faces' light, and shadows the sky body's fill. `in_use` is how many the last frame shaded
with; it is 0 on frames not lit by the planet atmosphere's IBL. A hull whose skirt or belly glows
like sunlit ground, or that sits on its shadow without a dark contact, is missing its proxy:
add `OcclusionProxy::default()` to the entity
([Host look checklist](../user-guide/host-look-checklist.md) § 9).

`bands` splits the last frame's proxies (ADR-7970). `near` (at most `near_max`, 32) get the full
shade above. `far` (at most `far_max`, 128) are the next nearest proxies with `far_band` true
(`OcclusionProxy { far_band: true, .. }`): each is a coarse ground-contact blob that darkens the sky
and fill under its footprint, with no sun shadow. A near proxy whose distance comes within 5 m of
the nearest far one fades into its blob, so a hull crossing the boundary does not pop. `dropped`
are the rest: past 32 without `far_band`, past both bands, or with a non-finite box. All three are 0
until a frame lit by the planet IBL packs them. A match with hundreds of hulls and `dropped` above
0 on hulls near the camera needs more of them opted in, or fewer proxies per hull.

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
level.get_look {}
level.set_look {"look": {"exposure_bias_ev": 0.5, "grade": {"white_balance_kelvin": 6900}}}
render.problems {}               # { ok, problems: [{ code, message }] }
render.mesh_occluders {}         # { in_use, bands: { near, far, dropped, ... }, occluders: [...] }
```
