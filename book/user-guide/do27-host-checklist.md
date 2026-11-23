# DO27 host checklist

The base frame (Desktop High, the planet sky, the shared terrain draw, exposure, scatter) is
[Host look checklist](host-look-checklist.md). This page is the rest of what the DO27 shell opts
into or copies into a level. Each section is one feature: what it does, the field or call that
exists in the engine, the setting to ship, and the agent call that checks it.

Numbers are from the integrated tree (ADR-109 and later) and the DO27 plates. A difference the
host checklist prints as `DIFF` is a choice; an `ERROR` breaks the frame. `render.problems`
(`FrameRenderer::frame_problems`) is the last frame's errors.

The look grades below are the values `do27_level` writes onto DO27's own `level.ron` (cooked
albedos, 45 klx Mars / 18 klx Europa). That function is crate-private in `lookdev::look_r2`;
`MarsGrade` and `LOOK_R2_MARS_GRADE` exist only in that harness. Copy the fields into
`LevelSettings.look`. The runtime applies them
(`apply_level_look` inside extract). A host that builds `PostSettings` itself calls
`regia_render::apply_level_look(&mut post, &level.look)`.

## Boot: branding under 1 s, first game frame without a hitch

Alpha smoke #1 wants boot → branding < 1 s, and no frame may compile a pipeline after warm-up
(`specs/27` § 2.4). A cold first game frame compiles everything its configuration uses — post
stack, shadow cascades, sky and clouds, volumetric fog, SSAO/SSGI/SSR, TAAU/TSR, material graphs:
about 5.5 s on the RTX 5080 for a DO27 map. The engine moves that compile onto warm workers while
the branding frame is on screen.

**Windowed runtime (`regia::run`, `RenderPlugin`): one resource, one call.**

1. Insert a `regia_render::BootSplash` before the app runs (logo, text, background). It is
   presented from the lifecycle event that creates the window, before any game frame (≈ 20 ms after
   the device on the 5080).
2. Call `splash.finish()` once the first scene is spawned (camera, level meshes, lights, the
   `RenderSettings` you ship). That is the warm trigger: `GameFrameWarm` (on by default) moves the
   `RuntimeRenderer` onto a `PipelineWarmQueue` worker, renders the extracted frame there at
   64×64 in the window's draw format (and once at the window's size, so the first visible frame
   allocates nothing), and gives it back; the splash holds until then, but never past 1.5 s after
   process start (next section), and fades into a first game frame that compiles nothing.
3. Each later level load: `world.resource_mut::<regia_render::GameFrameWarm>().request()` once the
   level is spawned. Until the warm is back the window shows the clear color and the UI overlay
   (the loading screen), not the scene.

**A map behind a UI-only menu warms by itself (v0.1.8, ADR-190).** When a window that drew no
scene on its last tick (the UI-only main menu, a lobby, a loading screen) gets a frame with at least
32 MiB or 256 meshes the runtime renderer has not uploaded, the engine warms that frame on a worker
before drawing it, exactly like `request()`. The window keeps presenting the clear color and the
UI meanwhile, so the menu stays interactive instead of freezing for seconds while the frame thread
compiles the map's pipelines and uploads it (v0.1.6 testers: a 4.7 s frame, 72 pipelines, 1 GB).
The map then appears and its first frame compiles nothing. This needs no call from DO27. It
runs at most once per scene-less stretch, never hides a scene already on screen, and is bounded by
`timeout`. Knobs: `GameFrameWarm::on_new_scene` (default `true`; `REGIA_SCENE_WARM=0`),
`new_scene_min_bytes`, `new_scene_min_meshes`; `scene_warms()` counts them, and the boot mark
`scene_warm_request` marks each one. A level swap under a scene that stays visible still needs
`request()` behind its loading screen (step 3).

**First-frame compiles overlap (v0.1.8, ADR-190).** A frame that builds its forward pipelines also
compiles the GPU-driven forward variants in parallel with them, instead of after them: about a
quarter less wall time for a cold map warm (RTX 5080). No call from DO27; `REGIA_FORWARD_OVERLAP=0`
turns it off for A/B timing. For a slow first run on a tester's machine, the log has
`wgpu instance flags: ..` after the `GPU:` line and, after `pipeline cache saved`, a line
`pipeline creation this run: N pipelines, X ms on the creating threads, Y ms each`: the per-pipeline
cost on that machine.

Configure the renderer before the trigger: while it is on the worker, systems that touch
`RuntimeRenderer` see a stand-in. The stand-in starts with copies of the renderer's VFX layers, GI
field and reflection probes. Translucent layers (VFX) and the shared pass draw (terrain) set on the
stand-in are moved over when the warm comes back. Since v0.1.8 so are a GI field, reflection
probes, an interior scene, DDGI settings and switch, and `reserve_instances` set on it (the last
call wins), because the engine can now start a warm by itself (previous paragraph).

Call order cannot blank or hang the window (ADR-166). The splash's warm runs only while the splash
still covers the window. A `finish()` before any scene exists queues the request; if the splash has
faded before a frame with a camera and a mesh arrives, the request is dropped and the scene draws
unwarmed, with no blank frames (a large scene on a scene-less window takes the new-scene warm
above instead; it shows nothing less than the window already showed). Only an explicit
`GameFrameWarm::request()` may hold a renderer whose scene is on screen, so call it behind a
loading screen. A request with no renderable frame expires after
`request_grace` (10 s). A watchdog (`GameFrameWarm::timeout`, 15 s by default,
`REGIA_GAME_WARM_TIMEOUT_S` overrides it) gives up on a warm that has not come back in that time.
If the job is still queued, it is canceled and the renderer comes back unwarmed. If it is running
and still building pipelines (one within `progress_grace`, 8 s), it is slow, not hung: since
v0.1.10 the watchdog lets it finish, up to `max_timeout` (30 s, `REGIA_GAME_WARM_MAX_S`). A
running warm that has built nothing for `progress_grace` past its timeout, or reaches
`max_timeout`, is abandoned and the stand-in takes over, compiling what it needs on its first frames. It already
has the GI field and reflection probes; the terrain shared draw comes back to it when the abandoned
job returns, unless you set or cleared the shared draw on it in the meantime. Both cases
log a `warn` with the warm queue's state. A host that replaces the
`RenderDevice` resource must also recreate the `PipelineWarmQueue`; otherwise the game warm is
skipped with a warning, because the queue's workers stay on the first device.

The persistent pipeline cache is on by default in the windowed runtime (`cache://pipelines`,
keyed by adapter, driver, wgpu and shader set); a second boot creates its pipelines from it.
`REGIA_PIPELINE_CACHE_DIR` moves it, `REGIA_PIPELINE_CACHE=0` turns it off.

**Ship pipeline caches for the test GPUs.** Before `regia package`, run each render profile you ship once on
every test machine (RTX 5090, RTX 5080, RTX 3080 Ti, GTX 1080) with `REGIA_PIPELINE_CACHE_DIR=<dir>` and copy
the `pipelines-vulkan-*.fpc` files into `<project>/pipeline-caches/`; `regia package --platform linux|windows`
(or `--pipeline-caches <dir>`) then ships them in `content/pipelines/`. A tester with the same GPU and driver then
starts from a filled cache (first launch ≈ a second launch); anyone else compiles behind the splash as before.
Redo it after a shader change or a driver update on a test machine (the key changes; a stale file is ignored,
never used). Book: Instant load → Shipped pipeline caches.

**A shell that drives `FrameRenderer` itself** does the same in two calls:

```rust,ignore
// Before the device: where the persistent cache lives.
regia_render::pipeline_cache::set_directory(Some(cache_dir));
// After the device: present the splash every frame (`BootSplash::record` into the swapchain view),
// then move the renderer onto a worker with the first frame it will draw.
let queue = regia_render::PipelineWarmQueue::default();
queue.start(&device);
let handle = queue.warm_renderer(WarmTier::Map, "map:switchbacks", renderer, move |r, device| {
    let scene = level.frame_scene(width, height); // owned data, built on the worker
    r.warm_scene(device, &scene, surface_format, false).map_err(|e| e.to_string())
});
// Each frame: keep presenting the splash until the renderer is back.
if let Some(done) = handle.take() { renderer = done.renderer.unwrap_or_else(FrameRenderer::new); }
```

A quit during a warm stops it between compiles: `warm_scene`, `warm_extracted` and `prewarm`
return an error once the queue shuts down. A warm job of your own that builds many pipelines in a
loop should check `regia_render::warm_cancelled()` between them and return early, or the quit
waits for the loop (up to 30 s, then the exit hook's 30 s; 250 ms after a device loss, ADR-276).

`warm_scene` alone (synchronous) is what `do27_chase` and `cargo xtask smoke` use inside their
prewarm span. Check: agent `render.pipeline_warm` → `game_frame.first_frame_ms` and
`pipelines_outside` unchanged after the `game_warm_done` mark (`render.boot_timeline`). Since
v0.5.1 (ADR-7883) do not gate the game on these counters: call `SceneReadiness::prepare(map_id)`
once the map is spawned and wait for the `SceneReady` message (below, "Scene ready").

**Effects, decals and glass (v0.1.5).** A map warm `frame_job` renders on a throwaway renderer
with no effect layer, so it cannot build what the first explosion, scorch decal or canopy needs.
Those pipelines live on your own objects; warm them there, next to the map's frame jobs:

```rust,ignore
// The VfxGpu you gave to `translucent_layer` (or `VfxGpuHandle::warm_job` on the ECS path).
// Billboards (haze, additive, sorted alpha, soft smoke), mesh particles, decals, trails and the
// soft-layer composite, for the window format and the HDR pass (`Rgba16Float`).
queue.push(WarmTier::Map, "vfx", regia_vfx::vfx_warm_job(vfx.clone(), VfxGpu::warm_formats(surface_format)));
// Your FrameRenderer, in the warm closure that already renders the map (`warm_renderer`):
r.warm_scene(device, &scene, surface_format, false)?;
r.prewarm_decals(device)?;           // the decal receiver's stamp variants
r.prewarm_glass_instances(device)?;  // GLASS sets for every target the warm drew (own canopy, cracks)
```

`vfx_warm_job` compiles outside the `VfxGpu` lock (frames keep drawing), stops between compiles
when the queue shuts down (the job then ends with `canceled at shutdown`), and goes through the
installed and shipped pipeline caches. `VfxGpu::warm(&formats)` does the same in place behind a
loading screen. The engine's own game-frame warm (`GameFrameWarm`, `warm_extracted`) now also
builds the decal receiver when translucent layers are installed and the glass-instance sets when
the frame has a transmissive standard material, and its frame already draws the effect layer, so
an ECS host needs no extra call. Check: after the map warm, `render.pipeline_warm` →
`outside_sites` lists no `regia_vfx/src/gpu/*`, `decal_receiver.rs` or `glass_gpu.rs` site when
the first effects, decals and canopies appear.

**Warm coverage (v0.1.10).** The v0.1.8 smoke still logged pipelines "created outside the warm
queue" after START: terrain sets, material graphs, forward sets, the translucent shadow and effects.
What the engine now does by itself, and the calls that are new:

- **Terrain pipelines are the device's** (`RenderDevice::shared_pipelines`). A terrain built on
  your thread (a map loaded behind the menu, the next map's terrain), its `prewarm` and its first
  hole cut compile nothing once any terrain or the terrain warm job built them. `TerrainPlugin`
  queues that job (`regia_terrain::terrain_warm_job`, map tier) once the device exists; turn it
  off (`app.world_mut().resource_mut::<TerrainWarm>().enabled = false`) only if you queue it yourself. Without
  `TerrainPlugin`, queue it at boot:
  `queue.push(WarmTier::Map, "terrain", terrain_warm_job(terrain_warm_formats(&profile, &device)))`.
  It warms the HDR format of your `RenderSettings`; apply saved settings before the first `Update`
  if you can (a later format change queues the new format too, but the first job was wasted).
- **Hole cuts mid-play:** `TerrainSharedDraw::prewarm` and the game-frame warm now also build the
  hole-cut sets (`SharedPassDraw::warm_pipelines`), so a crater or a tunnel mouth cut after START
  compiles nothing.
- **Effects (ECS):** `VfxRuntimePlugin` builds the `VfxGpu` (simulation, sort and radix-sort compute
  pipelines) and its render sets for the window and HDR formats on a map-tier warm worker
  (`VfxGpu::new_warmed`) and installs the layer when it is back (a few frames after boot).
  A host that owns its `VfxGpu`: build it with `VfxGpu::new_warmed` inside a warm job (it returns
  `Ok(None)` when the app quit mid-build), and call `FrameRenderer::set_warm_decals(true)` if a
  game-frame warm can run before its layer is installed.
- **Material graphs:** hand over every graph the level can draw, spawned units included, before the
  warm: `RuntimeRenderer::set_level_material_graphs(graphs)` (new). The game-frame warm starts
  their sets for the targets it drew. The graph worker threads now count as warm builders (map
  tier), not "outside": they never ran on the frame thread.
- **Double-sided meshes:** the game-frame warm builds the cull-none twin of every forward set it
  drew (`prewarm_double_sided`), so the first wreck or foliage card compiles nothing.

Check: `render.pipeline_warm` → `pipelines_outside` does not grow after `first_game_frame`.
Still compiled on the frame thread (unchanged, ADR-166): a window's first scene that the engine
does not warm (the splash's warm expired and the scene is under the new-scene thresholds). Prepare
the scene once the map is spawned, behind the loading screen, to cover it (next paragraph).

**Prepare maps at first launch (v0.5.1, ADR-8453): a cold driver cache never costs play time.**

| Do | Instead of |
|---|---|
| At launch, before the menu: `regia_render::prepare::host_status(&request, &device.adapter.get_info())` with one `PrepareMap` per map (from your loader's `ExtractedFrame`) and your render targets. When `needed`, show a "Preparing shaders" page and run `prepare::run_on(&device, &request, ..)` on a background thread (bar: `fraction()`, `eta`). Or run `regia prepare --cache-dir <cache> --scene <map>...` from the launcher or installer step; `regia package --prepare-scene <map>` records the list in `build_info.json` (`prepare_maps`). | Shipping driver caches for every GPU and driver, or letting the first match compile its pipelines (tester B: 225 pipelines, 83 s of thread time on a cold cache). |

**Scene ready (v0.5.1, ADR-7883): one event for the loading page, audio and player control.**

| Do | Instead of |
|---|---|
| Behind the loading page, once the map is spawned: `world.resource_mut::<SceneReadiness>().prepare("switchbacks")`. Keep the loading page, game audio and player input off until a `SceneReady` message arrives (`MessageReader<SceneReady>`, or `SceneReadiness::is_ready()`); then drop the page, unmute and hand over control. | Polling `render.pipeline_warm` counters, `BootBackground::warm_done()` or the `first_game_frame` mark: those say "no warm pending" or give a CPU time, not "the map is resident and on screen". |

From the prepare until `SceneReady` the window keeps presenting your UI and does not draw the scene;
the engine uploads every mesh and texture the frame references and creates the pipelines of its
draw keys on a warm worker, and warms again when more of the map spawns while it runs (tester A1's
340 MB second batch, tester A2's map that arrived after a warm of an empty frame). `SceneReady` is
sent once per prepare, after the present of the first frame that drew the map (frame generation:
after its present thread presented it). A warm the watchdog cancels or abandons still ends in
`SceneReady { timed_out: true }`: never wait without it. Agents: `render.scene_ready` (wait until
`state` is `presented`). Telemetry: `scene_load_end` has `map` and `complete`, and `scene_ready`
has `first_present_ms`.

**Warm coverage (v0.1.11).** The v0.1.10 smoke still logged ~14 pipelines "outside the warm queue"
per map at the start of play. All but one came from `do27.apply_lighting`'s
`capture_reflection_probe`, which ran on the main thread before the game-frame warm. A probe face
has no post stack, so it draws the forward set and the terrain (plain and hole-cut) into the
`Rgba8Unorm` readback target. No game frame and no warm used that target. The last one was the
glass sets. They compiled on the frame thread for the first glass canopy, a transmissive mesh with a
`GlassInstance` that was not in the warm's frame (its vehicle spawned after the warm started).

- **Glass:** the game-frame warm now always builds the glass sets. Nothing to call.
- **Probe captures after the warm** compile nothing. A frame warm renders one probe face when the
  renderer has reflection probes installed, has captured one, or was told to with
  `RuntimeRenderer::set_warm_probe_capture(true)` (new). The terrain's warm then builds its plain
  and hole-cut sets for every format it was drawn into, so a hole cut after a capture is covered.
- **The first capture of a map** still compiles where you call it if it runs before the
  game-frame warm, which is DO27's order today: about 14 pipelines on the main thread behind the
  loading screen, before `first_game_frame`. To move them onto the warm worker, call
  `set_warm_probe_capture(true)` at level load and capture once the warm is done
  (`GameFrameWarm::is_pending()` and `is_warming()` both false, with `last_report()` set).
  Otherwise leave the order as it is. Nothing compiles after `first_game_frame` either way.
- Still worth doing: `set_level_material_graphs`. Without it, graph sets for units spawned
  mid-play build on the graph workers. That is not "outside", but those units draw the standard
  surface until the build lands; the v0.1.10 smoke showed `graph forward` builds 45 s into play.

**Warm coverage (v0.1.15).** Rehearsal B on v0.1.11 still logged pipelines outside the warm queue
after `first_game_frame`. The engine side is fixed, and there is nothing new to call.
- **Renderer pipelines built by a stand-in:** the cluster build, the page clear and the translucent
  casters (`frame.rs`, `translucent_shadow.rs`) are now built once per device. A stand-in renderer
  (what `GameFrameWarm` leaves while the warm runs; a graph material that resolves then is
  prewarmed on it) or a second renderer compiles none of them.
- **TSR and the depth match:** the game-frame warm now builds TSR and the depth match whatever
  upscaler and scale the warm frame used. Applying the 0.67 + TSR lever after the preset's
  `SettingsChanged` (§ FSR 3, TSR, SGSR and 4K render scale), a fullscreen toggle and switching
  back to TAAU compile nothing. So does the half-resolution SSGI upsample at exactly 0.67.
- **A second map's sky:** the warm builds the sky, aerial perspective, froxel fog and cloud
  pipelines for its HDR target even when the warm frame's map has no clouds or fog.
- Still request `GameFrameWarm::request()` for each map, behind its loading screen (DO27's
  WARM_PER_MAP). The engine covers the switches above, but a new map's own forward sets, terrain
  formats and probe faces come from that map's warm frame.
- MSAA switched on mid-play, a render-to-texture camera or a second window: register each target's format and
  sample count with `pipeline_warm::register_warm_target(format, samples)` (or `PipelineWarmQueue::warm_target`)
  before the game-frame warm, and the warm builds them (ADR-7885; for a target made later, register it and call
  `GameFrameWarm::request()`). A late pipeline in your log now names its format, sample count, thread and the key axis
  that was new ([Targets and late pipelines](../agents/instant-load.md#targets-and-late-pipelines)).
- Not covered: FSR 3 switched on mid-play.

## Splash and lobby: the splash is gone by 1.5 s

The splash leads into the lobby, and gameplay is seconds to minutes away, so since v0.1.0
(ADR-173) the splash is gone **1.5 s after process start at the latest**, fade-out included.
Whatever the engine has not finished by then carries on in the background behind the lobby.

What DO27 does:

1. Call `splash.finish()` as soon as the lobby can draw. DO27 already does this at sim boot. The
   cap only cuts engine holds (a running game-frame warm, `min_duration`). It never fades a splash
   the host has not finished, because `finish()` is the host's "my first screen can draw" signal.
2. Keep the default cap, or set it: `BootSplash::with_max_duration(Some(Duration::from_millis(1500)))`
   (`None` = no cap, the pre-v0.1.0 behavior). `REGIA_SPLASH_MAX_MS=<ms>` overrides it for a
   test run (`0` = no cap).
3. The lobby's UI draws from its first frame. The overlay layers (runtime UI, quick draw, 2D) are
   the first job in the warm queue's boot tier, so their pipelines are built before the cap.
4. If the lobby has a 3D backdrop, spawn it before `finish()`, so the splash's game-frame warm
   covers it. If the warm is still running when the splash goes (a cold first launch: 2–4 s on an
   RTX 5080 without a matching shipped cache), the window shows the clear color plus the UI until
   the renderer is back. The backdrop then appears, and nothing compiles on the frame path. A
   backdrop spawned after the splash has gone draws unwarmed (ADR-166).
5. Entering a match: call `GameFrameWarm::request()` once the map is spawned, behind the loading
   screen, as for any level load (§ Boot, step 3). If the splash's warm is still running, the
   map's warm starts as soon as it is back. Until then the window shows the loading path (clear
   color + UI), never a frame that compiles.

What tells DO27 the background work is done: the `regia_render::BootBackground` resource,
refreshed every `PreUpdate`:

```rust,ignore
fn lobby_ready(bg: Res<regia_render::BootBackground>) {
    if bg.is_changed() && bg.warm_done() { /* game-frame warm back: the scene draws warmed */ }
    if bg.probe_done() { /* first-launch quality probe finished (or never ran) */ }
    // bg.splash_done, bg.splash_capped, bg.game_warm_running, bg.deferred_pending, bg.is_idle()
}
```

`GameFrameWarm::is_pending()` / `last_report()` and the boot marks `game_warm_done`,
`splash_capped` and `boot_background_idle` (`render.boot_timeline`) say the same thing.

**First launch is table-only in v0.1.0; the player's preset menu is the correction path.** DO27's first
launch takes the GPU table's pick and saves it as `Detected`: measured rows for the RTX 5090/5080 (Ultra),
RTX 3080 Ti (High) and GTX 1080 (Low), estimated rows for the rest of the Steam survey's top 40. A GPU
in no row gets the fallback: integrated, virtual and software GPUs get Low, an unknown discrete GPU with
15 GB or more gets Medium, anything less (or unknown VRAM on DX12) gets Low. It never picks Ultra, nor
High for an unknown or integrated GPU. A player who finds the pick too high or too low changes the preset
in the options menu; that choice is saved as the player's and never overridden. The menu's
Auto-detect button runs the table again. The confirmation probe is off by default, because its GPU
timings were not trustworthy on the RTX 5080 when v0.1.0 shipped (ADR-173).
`AutoQuality::with_confirm(true)` opts in. When on, it no longer runs behind the splash. Once the splash is gone and
the game-frame warm is back, a probe renderer on a warm worker draws the lobby's scene offscreen at
the window's size, back to back, for about 3–4 s (30+ frames of warm-up, then GPU timestamps), so the
GPU is at its clocks. The visible lobby frames are not touched; each can wait at most one probe frame
on the GPU. A step-down applies in the lobby, before the first match. A match
started before the probe has finished aborts it, and the table's pick (ADR-157) stays. A lobby
with no 3D scene gives it nothing to draw: after 15 s it ends with no frames and keeps the table
pick. It gets the runtime renderer's VFX layers, GI field and reflection probes, but not
the terrain shared draw. A saved preset skips the probe. It never steps up, never steps down on the wall clock or
when the GPU was busy under half the time, and needs the median near the budget too, not just
the p95.

**The `SettingsChanged` contract is unchanged.** Detection on the first `Update`, and a probe
step-down later in the lobby, each change `graphics.preset` and send one `SettingsChanged`. The
preset re-seeds `RenderSettings.profile`, so DO27 re-applies its 4K render scale 0.67 + TSR on
that message (§ FSR 3, TSR, SGSR and 4K render scale), exactly as before. The only difference is
when it can arrive: a step-down now comes a few seconds into the lobby, not under the splash.

## Performance overlay: on (Compact) for the alpha, option in the settings menu

Alpha testers report performance from the in-game overlay ([Performance overlay](performance-overlay.md)).
It is a top-left panel showing FPS, a 5 s frame-time graph, renderer CPU against GPU time, the top GPU
passes, texture memory against the budget, stutters, creates after warm-up, preset, GI mode and
resolution. **Detailed** adds the full per-pass GPU table. `DefaultPlugins` already includes it.

1. **Add the Performance overlay option to your settings menu.** Use
   `regia_framework::perf_overlay::spawn_settings_entry(world, Some(graphics_page))` for a
   ready-made label and Off / Compact / Detailed dropdown. Or build your own control from
   `settings_menu_entry()` and write the choice with `set_player_mode`. The choice is
   `GameSettings.performance_overlay`, saved with the other settings.
2. **Ship it on for the alpha.** Insert
   `PerformanceOverlay::default_mode(PerformanceOverlayMode::Compact)`. That is the host
   default; the engine default is Off. A player's saved choice (`Some(Off)` included) always
   wins over the host default, and `None` follows it.
3. **No default key.** DO27 uses the F-keys, so the overlay binds none. To add a key, declare
   and bind the `"PerformanceOverlay"` action (`PERF_OVERLAY_ACTION`) in your `InputConfig`.
   Each press cycles the mode.

Check: `perf_overlay.get` shows `mode: Compact, source: host_default` on a fresh install, and
`source: player` once the menu was used. Showing the overlay costs ≤ 0.1 ms CPU and ≤ 0.1 ms
GPU, and the toggle creates no pipelines. Guide: [Performance overlay (agents)](../agents/perf-overlay.md).

## Mars look: `ue`, `nasa`, `nasa_dark`

One grade on the Switchbacks `look` block. `ue` matches the UE v13 plates. `nasa` matches NASA's
white-balanced rover photographs. `nasa_dark` is that grade 0.35 EV darker, the darkest step that
still keeps shadow detail on a dim laptop.

All three share the meter and the band. Haze is `LevelAtmosphere.haze` (`LevelHaze`): `density`
`1.2e-5`, `scale_height_m` `600`, `base_height_m` `0`.

| Field | `ue` | `nasa` | `nasa_dark` (ship this) |
|---|---|---|---|
| `exposure_bias_ev` | `0.7` | `0.8` | `0.45` |
| `min_ev100` / `max_ev100` | `9.0` / `12.0` | same | same |
| `meter.low_percentile` | `0.35` | same | same |
| `meter.high_percentile` | `0.97` | same | same |
| `meter.center_weight` | `0.4` | same | same |
| `contrast` | `1.5` | `1.65` | `1.65` |
| `saturation` | `1.5` | `1.15` | `1.15` |
| `grade.white_balance_kelvin` | `6200` | `5200` | `5200` |
| `grade.tint` | `0.3` | `0.3` | `0.3` |
| `grade.lift` | `[0.010, 0.003, 0.0]` | `[0.0, 0.03, 0.02]` | `[0.0, 0.03, 0.02]` |
| `grade.gamma` / `grade.gain` | `[1, 1, 1]` / `[1, 1, 1]` | same | same |

`nasa_dark`'s bias is `switchbacks_nasa_grade`'s `0.8` plus `NASA_DARK_EV` (`-0.35`).

Ship `nasa_dark`. On DO27 content at 960×540, sunlit landscape saturation is 0.26–0.30 against
0.23 on the white-balanced Mastcam set, while `ue` is 0.43–0.65 (the rust-brown complaint).
`switchbacks_close` luma p50 is 158 on `nasa` and 154 on `ue` (UE 155). Darkening `nasa` by 0.5 EV
drops shadow-band steps on `DisplayModel::DIM_LAPTOP` to 81 % (`switchbacks_mid`) and 83 %
(`tunnel_interior`). −0.35 EV keeps every shot at or above 85 % (mid 89 %, tunnel 85 %, close 96 %,
wide 97 %; those were measured before the green-blue lift, which takes `switchbacks_mid` to 97 %). Shade stays warm; the natural-color photographs do too.

Check: `level.get_look`, then `seeing.screenshot` (`offscreen`) after a few frames so the meter
settles. `level.set_look` deep-merges a partial object if you are tuning live. Guide:
[Map look](../agents/look.md).

## Europa grade

Bore Twelve. Neutral ice is the path tracer; the blue-white print is this grade. Snow compensation
is on: a frame of sunlit ice (from 250 cd/m²) takes up to +1 EV.

| Field | Value |
|---|---|
| `exposure_bias_ev` | `0.0` |
| `min_ev100` / `max_ev100` | `10.5` / `13.6` |
| `meter.low_percentile` / `high_percentile` / `center_weight` | `0.45` / `0.98` / `0.3` |
| `meter.bright_threshold_cd_m2` | `250` |
| `meter.bright_bias_ev` | `1.0` |
| `contrast` | `1.05` |
| `saturation` | `1.15` |
| `grade.white_balance_kelvin` | `6200` |
| `grade.tint` | `0.1` |
| `grade.lift` | `[0, 0, 0]` |
| `grade.gamma` | `[0.87, 0.88, 0.95]` |
| `grade.gain` | `[1.15, 1.15, 1.15]` |
| `PostSettings::band_bounds_final_exposure` (host, not the level file) | `true` |

Turn `band_bounds_final_exposure` on (in code, or `render.set_exposure_settings
{"band_bounds_final_exposure": true}`) when the band is meant as the range the frame may print at:
the snow bias and the compensation then apply before the band clamp, so DO27's `min_ev100` /
`max_ev100` is the final exposure and sunlit ice no longer settles 1 EV under the floor (ADR-7969;
a level look keeps the switch, and the level file gains the field with the v0.6 scene-format bump).

The cool gamma (blue above red and green) moves lit ice from about 214° to 224–228° hue without
lifting the black sky. Contrast 1.05 opens trench shadows: visible shadow steps on
`bore_twelve_wide` go from 42 % to about 65 % on the dim laptop, and `set_piece_borehole` p50 stays
near the UE plate (about 139 vs 135).

Jupiter and Io stay as in the host look checklist (`SkyBody::jupiter_from_europa`,
`SkyBody::io_from_europa`, the map's `fill_lux`). Do not add a second Jupiter card.

Check: `level.get_look` shows `bright_bias_ev` `1` and the 6200 K grade, then
`seeing.screenshot`. An interior or a frame of fire should not take the snow bias (coverage under
25 % adds none).

## Sky: engine sky, not a host dome

Use the engine's planet sky and sky bodies for the planet and the moons: on the level's
`atmosphere`, set `sky_body = Some(SkyBody::jupiter_from_europa())` and
`moons = vec![SkyBody::io_from_europa()]`. Do **not** draw a host sky dome, skybox, or any huge
opaque mesh around the camera: every `MeshRenderer` casts shadows (`ShadowCast::All`), so a
1.9 km dome shadows the whole map. If a host mesh must enclose the camera (a canopy shell, an
interior backdrop), add `regia_render::NotShadowCaster` to its entity. Extraction then gives
it `ShadowCast::Off`: it casts no shadow (sun or local lights) but still receives them. Meshes
without the marker cast shadows exactly as before.

## Reflection probes

From v0.1.5:
- **Planet sky in the capture.** `capture_reflection_probe` lights its faces with the level's planet. That means the planet's sun, its sky bake and its sky as the background. Before v0.1.5 a Mars probe held Earth's blue sky, which stochastic SSR put on glossy armor, rifles, rails, berms and pads.
- **Probes stay near their box.** On Desktop High and Ultra (stochastic SSR), a probe reflects only inside its box and fades out over a margin past it. An interior probe left installed outdoors therefore no longer tints the plain; past every box, pixels keep the frame's own sky. Size each outdoor probe's box to cover the ground it serves: an outdoor probe with a box that is too small stops reflecting a few metres past its edge.
- **Less disturbance to the next frame.** A capture no longer leaves camera motion (a motion-blur smear) or a TAA exposure step on the frame that follows it, and the sun's cascade placement is restored. The paged shadow caches are not restored: the faces still advance them like any other frame.

`set_reflection_probes` replaces the whole set. After an interior exit, install the outdoor set again (or an empty set). Re-capture outdoor probes when the sun moves far from the time they were captured.

## Texture budgets and `render.problems`

Every game preset streams material textures under a resident budget
(`TextureStreaming::for_preset`, eager: a new material uploads up to its cap while the budget has
room). Over the budget, the least-recently-seen materials drop top mips. `FrameQuality::LAB` and
`EDITOR` stay unstreamed.

| Preset | Cap | Budget |
|---|---|---|
| Mobile Low | 1 K | 256 MiB |
| Mobile High | 2 K | 512 MiB |
| Desktop Low, Deck | 2 K | 1 GiB |
| Desktop High | 4 K | 2 GiB |
| Desktop Ultra | authored | 4 GiB |

Leave the preset policy alone. DO27's drop is 3 798 PNGs and decodes to 191 GiB of RGBA8 plus
mips; three instances at about 16.6 GB each OOM'd a rig when streaming was off. The path under
3 GB RSS is a cooked pak (`regia cook`), not PNGs the shell decodes itself. Name host-built
materials with `regia_render::name_material_textures` so the report can name them.

`render.problems` lists both (`ok` is false while either is present). The host checklist
prints the same two as differences, not errors (the frame still draws):

- `texture_budget_exceeded` — resident material textures over the tier budget (or 2 GiB when
  streaming is off).
- `uncompressed_textures` — more than `UNCOMPRESSED_TEXTURE_LIMIT` (64) RGBA8 material textures.

Each message names the five largest offenders. `assets.texture_memory` lists every texture
(`budget_bytes`, `resident_bytes`, `over_budget`, `uncompressed`).

Check: after the level is resident, `render.problems` has `ok: true` and
`assets.texture_memory` has `over_budget: false`. A large `uncompressed` count means the shell is
still uploading PNG/JPEG. Guide: [Assets](../agents/assets.md),
[Map look](../agents/look.md).

## Material texture residency

Graph slots and the packed detail, height, and curvature maps can carry a cooked chain beside
RGBA8. A paint scan the host puts in one `SharedImage` uploads once, however many materials
clone it. The MFD stays one GPU texture and one bind group across refreshes. RGBA8
`LabTexture` fields are unchanged. Identical cloned pixel buffers are still uploaded once each:
the identity is the `Arc`, not a hash of the bytes. The renderer keeps that `Arc` while the
GPU texture is resident and pairs the address with a generation, so a recycled address cannot
come back as the old texture.

Cook a graph color slot once and clone the handle into every material that shows that scan:

```rust,ignore
let slot = regia_asset_pipeline::cook_graph_texture(
    width, height, &rgba,
    regia_asset::TextureCookUsage::ColorSrgb,
    true, "linux", regia_asset_pipeline::TextureCookQuality::High,
)?;
graph.textures[0] = Some(slot.clone());
```

`cook_shared_image` is the same image for a fixed slot (`LabMaterial::images`). Pass
`TextureCookUsage::ColorSrgb` and `srgb: true` for color (BC7 sRGB, the sampler decodes it).
A graph normal the shader reads as XYZ is `ColorLinear` with `srgb: false` (BC7 linear). Use
`NormalMap` (BC5) only when the shader rebuilds Z. A single channel is `Mask` (BC4).

Build the slot with `GraphTexture::rgba`, `GraphTexture::from_shared`, or
`GraphTexture::from_dynamic`. A struct literal from an older revision does not compile:
`GraphTexture` gained `compressed`, `shared`, and `dynamic`. `..Default::default()` fills
those with nothing, and `GraphTexture::rgba` is what the graph builder calls.

Packed detail, height, and curvature go through `cook_surface_maps`. Clone the returned
`CompressedMaps` `Arc` onto each material's `compressed` field. Detail albedo and the detail
normal are linear BC7: the shader reads the normal's XYZ and the mask in A, and does not
rebuild Z. Height (R) and curvature (G) are BC5.

The MFD (1024×640 RGBA8) keeps one material:

```rust,ignore
let mfd = regia_render::DynamicTexture::new(1024, 640, true);
mfd.write(&pixels)?; // or mfd.write_with_queue(&device.queue, &pixels)?;
material.images.emissive = Some(regia_render::MaterialImage::Dynamic(mfd.clone()));
```

A graph material does not read `LabMaterial::images`. Bind the MFD with
`GraphTexture::from_dynamic` on a graph slot instead. `write` and `write_with_queue` store
the bytes on the handle. The renderer that draws the material copies them into one GPU
texture before the draw, with `queue.write_texture`. That copy does not require a row to be
a multiple of 256 bytes (a buffer-to-texture copy does; `write_texture` does not). The handle
holds no `wgpu` texture: drop the renderer whenever you drop it, the handle will not keep the
device alive, and a second renderer creates its own texture on its own device and flushes the
same bytes. The bind group is created on the first draw and kept across writes.

Check: `FrameRenderer::texture_memory` has one row per shared image (`compressed: true` for a
cooked chain) and `uncompressed` / `uncompressed_bytes` count only the RGBA8 ones.
`render.problems` still lists `uncompressed_textures` above 64 RGBA8 maps.

## Camera cuts

`FrameRenderer::camera_cut` (and `camera_cut_with(CutExposure::Fade)`) drops temporal history
before the next frame: TAAU/TSR/FSR3, froxel fog, the static-screen SSGI/SSR cache, stochastic
SSR, sun cascades and pages, and (by default) the exposure meter. `CutExposure::Snap` is the
default. `CameraCutDetection` is on by default: eye jump over 50 m, turn over 90°, or a scene
change under 25 % of objects kept both ways (`set_camera_cut_detection(None)` disables it;
`set_camera_cut_detection(Some(detection))` retunes `max_eye_jump_m`, `max_turn_degrees`,
`max_kept_share`).

Leave detection on. Call `camera_cut()` on a scripted cut (cockpit to chase, replay, menu to map)
even though detection would catch most of them: the explicit cut is `reason: requested` and does
not depend on the 50 m threshold. A vehicle that teleports more than 50 m without a cut raises
`max_eye_jump_m`. Frame 1 after a cut matches a fresh renderer within 1 per channel (Desktop High
and Deck, TAA on and off).

`render.camera_cut` requests a cut (`exposure`: `snap` or `fade`) or, with `{"cut": false}`, only
reads `last_cut` (`eye_jump`, `turn`, `scene_change`, `requested`). Then `seeing.screenshot`: the
image carries nothing from the previous shot. An unexpected `eye_jump` on a fast chase means the
threshold is too low. Guide: [Camera cuts](../agents/camera-cuts.md).

## Contact shadows

Screen-space sun shadows for casters the cascades miss (rocks, tire contact, crust, ledges).
`RenderProfile::contact_shadows` is on for Desktop Low, High, Ultra and Deck, and off for Mobile
Low, Mobile High, `FrameQuality::LAB` and `EDITOR`. It runs only with a depth prepass, one sample
and sun shadows. Off binds a white texture, so off frames match the old image.

Leave it on for the desktop and Deck tiers. On the DO27 plates it is local: pixels darkened by
more than 8/255 are 0.16 % (`bore_twelve_mid`), 0.47 % (`set_piece_borehole`) and 0.37 % (cockpit).
Rocks gain a dark crescent where they meet the ground; `switchbacks_close` is unchanged because
the sun is behind the camera and the cascades already resolve those rocks. Budget written for the
pass: at most 0.3 ms at 2560×1440. `GpuTimings::contact_shadows_ms` is the measurement.

`render.set_contact_shadows {"enabled": false}` then `play.step` and `seeing.screenshot`, and
`seeing.compare_images` against the shot with it on. `active` is false when the profile has no
depth prepass or uses MSAA. Guide: [Contact shadows](../agents/contact-shadows.md).

## Mesh occluders

A parked hull's underside reads sunlit ground unless the frame has a box proxy. On a mesh with
`MeshRenderer`, add `OcclusionProxy::default()` (albedo from the material, ground at the bounds'
lowest corner; set `ground_y` when the box starts above the ground). The render plugin runs
`extract_mesh_occluders`. A host that drives `FrameRenderer` calls `set_mesh_occluders` each frame
with `MeshOccluder::from_positions` or `from_local_bounds` (optional `with_ground`). Up to 32,
nearest first. Skip characters. Frames with no proxies are unchanged. For matches with more hulls,
set `far_band: true` on the proxies: the next 128 get a ground-contact blob, faded in over 5 m at
the boundary (host look checklist § 9, ADR-7970).

On the dyn_light hull plate the skirt and the Mars ground under the hull land within ±25 % of the
path tracer. Twenty hulls at 1440p on an RTX 5090 cost +0.15 ms (Bore Twelve) and +0.25 ms
(Switchbacks) in the forward pass. On Europa, `TerrainSharedDraw::set_fill_canyon_visibility(true)`
lifts shadowed ground from 0.57–0.67× the tracer to 0.81–0.94×. It is off by default because it
lifts every fill-lit shadow on the map. The Mars hull under Depth is a separate problem
([GI mode](#gi-mode-depth-default-on-desktop-balanced-option)).

`render.mesh_occluders` lists the boxes (`in_use` is 0 when the frame is not lit by the planet
IBL). A hull whose belly matches the sunlit ground is missing its proxy. Guide:
[Map look](../agents/look.md), host look checklist § 9.

## GI mode: Depth default on desktop, Balanced option

`GiLook::Balanced` is the analytic sky, wall bounce and mesh ground bounce. `GiLook::Depth` is the
GPU irradiance probes. `RenderSettings::gi_look: None` follows the profile: since gi-probes round 4
(ADR-133) Desktop High and Ultra default to Depth, every other preset (and Android) to Balanced.
`GameSettings.graphics.lighting` (`LightingMode::Balanced`, `LightingMode::Depth`, or `None`) is
the menu value; the settings plugin copies it onto `RenderSettings::gi_look`.

Ship `None` (Depth on Desktop High/Ultra). The Mars hull on the dyn_light plate passes under Depth
(every class within ±25 % of the path tracer on Mars and Europa; Mars shaded 0.95–1.02). Depth
costs about +0.50–0.57 ms over Balanced on the 5090. Call
`TerrainSharedDraw::bake_gi_albedo_map` once at load or the probes see one albedo for the map.
`GiLook::depth_available()` is false on Android, and `render.set_gi_mode {"mode": "depth"}` is
refused there.

`render.get_gi_mode` returns `mode`, `setting` and `preset_default` (on a default Desktop High
frame `mode` and `preset_default` are `depth` and `setting` is `default`, since `gi_look` is
`None`). To compare: `play.pause`, `render.set_gi_mode {"mode": "balanced"}`, `play.step` 30,
`seeing.screenshot`. Probes need about 30 frames. `gi_ms` is the GPU cost (about 0.26–0.28 ms on
the Mars plates, RTX 5090 at 1440p, clocks held up).
Guide: [Lighting option](../agents/gi-probes.md).

## DDGI brick pool

`DdgiSettings::sdf_pool_bytes` is the signed-distance brick pool. The default is `SDF_POOL_BYTES`
(32 MB), the size the pool had when the field did not exist. A change recreates the pool and the
bricks rebake. `DdgiSettings::sdf_evict_frames` (default `SDF_EVICT_FRAMES`, 300 probe updates,
about 5 s at 60 fps with one view) drops a brick whose mesh has not been drawn for that many
updates. When a new brick needs the room, the least-recently-drawn eligible brick goes first. A
mesh drawn every update keeps its brick and its offset, so a default scene does not move.

A mesh that stays undrawn for that window loses its brick. The next time it is drawn, the brick
is baked again, and the probes do not trace it until that bake finishes (a short pop). Several
rendered views per present tick the window faster than one update per displayed frame. Lower or
raise `sdf_evict_frames` to trade pool turnover against that pop. While the pool is full, a mesh
that does not fit is remembered until a brick is freed or `sdf_pool_bytes` changes. Those frames
do not walk its vertices again.

`MeshRenderer::gi_proxy` defaults to true. Set it false on a small dynamic part: that mesh gets no
brick and the probes do not trace it, and it still casts shadows. `NotShadowCaster` is separate and
still means no shadow. The brick is per mesh, so one instance with `gi_proxy` false opts out every
instance of that mesh. This field is new: a `MeshRenderer` struct literal in the shell that names
its fields must set `gi_proxy` (use `true` to keep the old behavior). The window, texture views,
hidden screenshots and the pipeline warm pass the opted-out keys every frame. A caller that drives
`FrameRenderer` itself calls `FrameRenderer::set_gi_proxy_disabled` with those
`FrameMesh::mesh_key` values every frame, including an empty list; the runtime path overwrites that
set every frame, so a game sets `MeshRenderer::gi_proxy` instead. The editor viewport and the agent
reference export do not forward the opt-out. Those paths are not the player frame. A host that
renders an `ExtractedFrame` on its own renderer calls `set_gi_proxy_disabled` itself.

`min_proxy_radius` stays 1 m in the engine. The shell's 4 m workaround can stay or come back down;
it is independent of `gi_proxy`.

When the pool cannot fit a new brick and nothing is old enough to evict, that mesh is skipped for
the update and retried the next time it is drawn. The log line is still
`ddgi: signed-distance brick pool full; later meshes are not traced`, once, until a brick is stored.

## Translucent shadows

Not shipped. There is no switch.

The shadow pass skips blended draws. DO27 cockpit and canopy glass is `alphaMode: BLEND` at alpha
0.9, so interiors behind it are fully sunlit. A path tracer attenuates by opacity and those
interiors stay dark. On the hero references, `M_Glass` reads 2–66× the Cycles reference on
`scavenger_a52`, `turret`, `constructor` (`_a52`), `foundry_*_a52` and `remora`. The fix that is
not in the tree is to attenuate the sun by `1 − alpha`.

Do not darken interiors with an extra light or a fake shadow card to hide this. `GlassView` and
`GlassDamage` change refraction and cracks; they do not cast a tinted shadow. Check with
`seeing.screenshot` through a canopy: the sun on the interior is unattenuated. When a shadow API
lands, this section will name it.

## Terrain virtual texture

`TerrainSharedDraw::set_virtual_texture(Some(VirtualTextureSettings::default()))` caches the
photoreal surface (`VtCache::Surface` is that default). A pixel whose pages are missing shades in
full, so a miss costs time and does not show a hole. Off by default. `cache` `lighting` or
`relief` keeps more of the per-pixel surface and saves less.

Turn Surface on for 4K, and for the Switchbacks at 1440p. On the RTX 5080 bench
(`cargo xtask bench-do27`, 2026-10-09, High, GPU frame p50, Bore Twelve / Switchbacks):

| | 1440p | 4K | SSIM vs the default frame |
|---|---|---|---|
| default | 4.54 / 6.15 ms | 9.46 / 12.25 ms | 1 |
| Surface VT | 3.98 / 4.76 (−12 / −23 %) | 8.45 / 10.57 (−11 / −14 %) | 0.985–0.991 |

The frame is GPU-bound (CPU render is 1.5–3 ms). Bore Twelve at 1440p saves less (−12 %); leave
VT off there if that ground must stay as sharp as the uncached plate. No mode hits both SSIM
0.998 and a 40 % cut. `Lighting` is the mode that keeps the frame (SSIM 0.998–1.0 on the earlier
5090 plates) and barely saves.

`terrain.set_vt {"enabled": true, "settings": {"cache": "surface"}}`, then poll `terrain.vt`
until `pending` is false. `stats.missing` may be non-zero for a few frames after a cut; the image
still shades those pixels. Debug view 96 is the bake's geometry error. Guide:
[Terrain virtual texture](../agents/terrain-vt.md).

## Impostors

Far scatter (past each class's mesh fade, out to `ImpostorSettings::far_end_m`, default 2 km)
draws as octahedral cards. Off by default, and an unbound layer does not change the frame.

At load:

```rust,ignore
field.set_impostors(Some(ImpostorSettings::default()));
let atlas = field.bake_impostors(None); // 0.04–0.12 s, 4.9–7 MB, DO27's prototypes
let mut layer = ImpostorLayer::new(&device.device, &device.queue, &atlas);
draw.set_impostors(Some(&layer));
```

After `field.update`, call `layer.update(&queue, &field)`. `ScatterControl.impostors = true` is
only the request: `ScatterControl::apply_impostors(&mut field, Some(&mut layer), settings)` is
what pushes it, and nothing in the frame loop calls that for you. Without a layer,
`apply_impostors` forces the field off.

Turn it on for wide views. On the RTX 5090 at 2560×1440 the chase moved 2.81 → 2.84 ms
(Switchbacks) and 2.375 → 2.402 ms (Bore Twelve). Held plates added 0.02–0.07 ms. Wide plates
gain the far rocks (`switchbacks_wide` 2.6 % of pixels, `bore_twelve_wide` 3.3 %); the close
plates stay identical. Far cards cast no shadow.

`terrain.set_scatter {"impostors": true}` writes the same flag (the handler accepts it; the
schema parameter list does not name it yet). `terrain.scatter` returns `impostors`. The harness
switch is `REGIA_IMPOSTORS=1`. Confirm with `seeing.screenshot` on a wide vista: rocks continue
to the rim. Guide: [Terrain scatter](../agents/terrain-scatter.md).

## Quality preset on first launch: automatic

Nothing to call in the default runtime. `regia::DefaultPlugins` includes `AutoQualityPlugin`; with
`regia_framework::settings::SettingsPlugin` added (DO27's options menu persists there), a first
launch reads the GPU, picks a tier from the engine's table (RTX 5080/5090 → Ultra, RTX 3080 Ti →
High, GTX 1080 → Low: measured; the rest estimated, integrated → Low, Deck → Deck), optionally
(off by default in v0.1.0, § Splash and lobby) probes the lobby's scene offscreen after the splash
and steps down one tier when it misses 60 fps, and saves `graphics.preset` with `preset_source: Detected`. A preset the player picked is
never overridden; the menu's Auto-detect button calls `AutoQuality::request_redetect()`. Apply the
4K render scale and TSR (next section) after a `SettingsChanged` of `graphics.preset`, since the
preset re-seeds `RenderSettings.profile`. A shell driving `FrameRenderer` itself calls
`regia_render::quality::detect(&AdapterProfile::from_device(&device), &GpuTierTable::builtin())`
and `tier.apply(&mut settings)`. Check: `quality.get_detected` (`detected.reason`, e.g.
"NVIDIA GeForce RTX 3080 Ti, 12 GB → High"). Page: [Automatic quality detection](auto-quality.md).

## FSR 3, TSR, SGSR and 4K render scale

Desktop High and Ultra ship native (`render_scale` 1, `TemporalUpscaler::Taau`). The shell sets
the scale on `RenderSettings.profile.render_scale` and the upscaler on
`RenderSettings.profile.temporal_upscaler` (extract copies both;
`FrameRenderer::set_temporal_upscaler` is the direct call). `GameSettings.graphics.render_scale`
is stored and not applied by the settings plugin. At exactly 0.67 with a 2048² cascade, Desktop
High also takes half-resolution SSGI, 6+8 PCSS taps and no full-resolution SSR
(`FrameQuality::ssgi_half_res`, `pcss_taps`). The host checklist prints a non-native scale as
`DIFF`, not `ERROR`.

At 1440p leave the scale at 1. The High GPU frame p50 over the path is 4.54 / 6.12 ms
(Bore Twelve / Switchbacks). The Surface-VT row above measured its own default run at 6.15 ms.

At 4K ship `render_scale` `0.67` and `TemporalUpscaler::Tsr`. On the lever run (High, GPU frame
p50, same card) that is 9.46 / 12.25 ms native and 5.23 / 6.75 ms at 0.67 + TSR (−45 %), SSIM
0.976–0.982. The whole-path summary lists Switchbacks 4K at 12.21 ms; compare a lever against
the lever row. `TemporalUpscaler::Fsr3` is the comparison baseline (FidelityFX SDK v1.1.4), not the
default: on the 1080p plates SSIM ties TSR, FSR 3.1 keeps more detail (82–98 % of native gradient
energy against TSR's 68–84 %) and shimmers more (rotation flicker 0.75 / 0.93 against 0.48 /
0.72). Its upscale pass is 0.28 ms at 1440p against TSR's 0.22 ms (+0.065 ms). Switch it with
`set_temporal_upscaler(TemporalUpscaler::Fsr3)` or `do27_bench --upscaler fsr3` when you want that
A/B. `FrameRenderer::set_fsr3_settings` takes `Fsr3Settings` (SDK defaults, RCAS 0.8).

`SpatialUpscaler::Sgsr` is the mobile spatial upscaler, already selected on Mobile Low and Mobile
High. Leave Mobile High at `render_scale` 0.85: 0.75 with SGSR lost to the previous preset on
every DO27 plate (ADR-102). Desktop spatial upscale stays `Bilinear`.

Check a running shell with `render.get_profile` (`render_scale`, and the preset name) and
`play.hitch_report`. The profile object the agent returns does not include the temporal upscaler;
read that from the shell's `RenderSettings` or from a `do27_bench` run with `--scale 0.67
--upscaler tsr`. Guide: [Render profile](../agents/render-profile.md),
[Rendering](rendering.md).

## Hover washes, weapon hits, muzzles, tracers, plumes

Add `VfxPlugin` and `VfxRuntimePlugin`. They are not part of `DefaultPlugins`. The level's air
selects the variant (`@mars`, `@airless`; Earth is the base name).

**Hover.** `HoverWash::new(HoverSurface::Auto, radius_m)` on the hull. `Auto` is ice on an airless
level and dust on Mars and Earth. Set `ground`, `ground_normal` and `height` each frame.
Intensity is `(1 − h / 2R)²`; scale is `R / 3` (`HOVER_REFERENCE_RADIUS`). The air wash is a
radial sheet that rolls up (Mars dust lingers 3.5–6 s). Airless stays ballistic grains. Since
v0.5.1 the sheet and billows draw soft puffs, not streaks up to 2.7 m long, and a respawn jump does not
fling the wash (ADR-8026). A host that plays its own dust skirt because the built-in wash
streaked can switch back to `hover_wash_*`.

**Weapons.** `regia_vfx::combat::DO27_WEAPONS` (`weapon_fx`) maps `rifle`, `sidearm`, `support`,
`sniper`, `chaincannon`, `spikegun`, `ricochet`, `thumpgun`, `arcmortar`, `quakeshell`,
`heatseeker`, `frag`, `cutterbeam`, `sentry` and `guntower` to a muzzle, a tracer and a hit.
Queue `WeaponHit::by_weapon(id, position, normal, surface)` (it is a `Command`). Muzzles are
`muzzle_flash`, `muzzle_cannon`, `muzzle_launcher`. Tracers are `tracer` and `tracer_heavy` (no
air variant: they burn their own oxidiser). Hits are `hit_metal`, `hit_concrete`, `hit_glass`.
Explosive rows (`thumpgun` 0.5, `arcmortar` 0.8, `quakeshell` 1.0, `heatseeker` 0.7, `frag` 0.6)
also spawn the atmosphere-aware `explosion`. The readable muzzle and tracer cores draw over the
scene, so they stay orange on Europa ice instead of clipping to white.

**Plumes.** `ThrusterPlume::new(PlumeKind::Duct)` or `PlumeKind::Jetpack` on a nozzle entity.
`throttle` is 0 (idle) to 1, clamped at 2. Exhaust is local −Y unless you set `exhaust`.
`DO27_THRUSTERS` names the sockets: gleaner `socket_duct_l0/l1/r0/r1` (exhaust down),
`gleaner_a52` two ducts (exhaust −Z), `kraken` ducts with no sockets (place them yourself),
pilot `socket_jetpack`. Air: shimmer, a faint column, nozzle glow. Mars: no shimmer, a thinner
wider plume. Airless: glow only, and it lights the ground when the unit is low.

A firefight of six washes, a chaincannon, two rifles, 30 hits per second, 12 ducts and 2 jetpacks
is 0.28 ms on Europa and about 0.45 ms on Mars at 2560×1440 on an RTX 5090 (`vfx_lab
--combat-bench`). Mars draw rose from 0.06 ms to 0.20 ms when the wash became overlapping billows.

`vfx.weapons` lists the table. `vfx.hover_washes` / `vfx.set_hover_wash` and `vfx.plumes` /
`vfx.set_plume` read and spawn. `vfx.hit` plays one impact (`surface`: `metal`, `concrete`,
`glass`; `weapon` or `calibre`). `play.wait_until` until the list shows `playing`, then
`seeing.screenshot`.
Guide: [VFX](../agents/vfx.md).

## Glass: view, damage, breach

On the glass entity:

- The pilot's own canopy: `GlassView::viewer_inside()` (`GlassViewMode::ViewerInside`,
  `inside_specular` 0.15). That pane does not refract. Glass seen through it still does.
- A canopy's cracks: `GlassDamage` with `set: "laminate"`. Laminate never sheds shards and never
  emits `GlassBreach`.
- A building pane: `set: "pressure_pane"`. At level 3 the VFX plugin emits `GlassBreach`
  (`glass_breach_airless` or `glass_breach_mars`; Earth plays nothing) and `glass_chunks`.
  `GlassPressure` sets the vent. `GlassBreachAudioPlugin` puts a ground-coupled sound on the
  airless breach.

`WeaponHit` with `surface: HitSurface::Glass` and `target: Some(entity)` calls
`GlassDamage::impact` in the mesh frame. Refraction itself is on for Desktop High and Ultra only.

`render.set_glass_view {"entity", "mode": "viewer_inside"}` and `render.glass_damage` with
`stamp.point` in world space. `seeing.screenshot`: laminate cracks are frosted lines; a
`pressure_pane` hole is cut out. `vfx.medium` does not list the breach; the breach effect name is
on `GlassBreachFired`. Guide: [Glass](../agents/glass.md).

## Atmosphere-aware explosions

`VfxPlugin` builds `VfxMedium::from_level` from `LevelSettings` (atmosphere preset and gravity
length) and plays `explosion@airless` or `explosion@mars` when the library has no override of
that name. Spawn the base name `explosion` (`SpawnEffect { effect: "explosion".into(), .. }` or
`VfxScene::spawn`). Do not spawn a Mars explosion on Europa by hand.

Airless (Europa, `EUROPA_GRAVITY` 1.315 m/s²): flash, a gas fireball gone by about 0.35 s, no
fire light after 0.5 s, ballistic ejecta, no smoke column. Mars (`MARS_GRAVITY` 3.71): a short
fireball and butterscotch dust that lingers 10–16 s. Earth is the previous showcase, byte for
byte. A game `.vfx.ron` named `explosion@airless` replaces the built-in.

`vfx.medium` returns `atmosphere`, `gravity` and `variants` (`explosion` → `explosion@airless` on
Bore Twelve). `seeing.screenshot` at 0.5 s: Europa has no fireball left; Mars still has dust.
Guide: [VFX](../agents/vfx.md), [Atmosphere](../agents/atmosphere.md).

## Live asset reload

`regia run --dev` cooks in the CLI process and runs the game with `dev-assets`. Saving a file
under `assets/` re-cooks that source (120 ms debounce) and reloads it in place. `regia run
--dev --level path/to/map.level.ron` opens that level after the game's `Startup` and again when
the file changes. A cook error keeps the previous asset; the log line is `cook error in <path>:
…`. Shipping `regia package` builds do not watch.

Use it for look and content iteration. Do not ship a watcher. `assets.watch_status` returns
`watching`, `reloads`, `errors` and `recent` (`ok`, `error`). Poll it after a save, then
`seeing.screenshot`. Guide: [Live asset reload](live-asset-reload.md).

## Skinned cooked meshes (cooked mesh v2)

Cooked meshes now keep the glTF skin (joints as cooked skeleton ids, `unorm16` weights, up to 8
influences), `COLOR_0` and sparse morph targets (ADR-226; format reference: `docs/formats/cooked-mesh.md`).
Existing paks and caches load unchanged: a v1 mesh has no skin and the same vertices.

- **Re-cook once.** The glTF importer is 1.8.0, so `regia cook` rebuilds every glTF. Until then a
  pilot's cooked mesh has `skin: None`. `regia upgrade-project --write` re-encodes v1 `.fckd` meshes
  at v2 (backups as `.fckd.bak`); it does not add a skin, only a re-cook does.
- **Skinned draws.** Build the render mesh with `regia_render::CpuMesh::from_cooked(&mesh)?` and
  feed it the palette of the skeleton cooked from the same glTF (`skin.joint_names` equals
  `skeleton.joint_names`). Pilots are well under the 256-joint cap; a skin above it is an error
  (`from_cooked_rigid` draws it unskinned). `load_cooked_model` still draws parts rigid.
- **Mesh node transform.** Any armature or mesh node transform works (Blender's X-90° and 0.01
  scale included): a skinned primitive bakes its skin's matrix, the skeleton's inverse bind
  matrices undo it, and root joints carry the armature transform. Root joints of such skeletons
  therefore have a rest rotation/scale and root motion comes out in scene metres. Re-cook the
  skeleton and clips together with the mesh; a skeleton cooked before this fix does not match a
  mesh cooked after it (and the other way round).
- **Check a pose.** `assets.skin_check { mesh, skeleton, clip?, time? }` skins the cooked mesh with
  the cooked skeleton on the CPU: `bind_offset_max` ≈ 0 means mesh and skeleton agree, `bounds`
  is the posed extent (a 1.8 m pilot should be about 1.8 m tall), `missing_joints` lists skin
  joints the skeleton lacks.
- **Struct literals.** `regia_asset::CpuMesh` gained `skin`, `colors` and `morph_targets`: add
  `..Default::default()` to literals. `CookedMesh` literals list `format_version`, `skin`,
  `stream_color0` and `morph_targets`. New error variants on `MeshValidationError`,
  `CookedMeshValidationError` and `CookedMeshLoadError`.
- **Check it.** `assets.load` answers `mesh.skin.joints` / `influences`, `colors` and
  `morph_targets` for a cooked mesh.

## `bench-do27`

`cargo xtask bench-do27` release-builds `lookdev`'s `do27_bench` and runs it.
`REGIA_DO27_CONTENT` must point at the content drop. The run that produced the numbers above:

```text
cargo xtask bench-do27 -- --out <dir> --png
```

Defaults: both maps, Desktop High and Ultra, 2560×1440 and 3840×2160, segments `spawn`,
`flyover`, `fight`, 600 frames after 120 warm-up. `--baseline <summary.csv> --tolerance 0.1`
exits 1 when wall, GPU frame, CPU `render_into` or host scatter p50/p95 regress. Levers that
match this page: `--vt`, `--scale 0.67`, `--upscaler tsr` (or `fsr3`). The 2026-10-09 RTX 5080
tables are the baseline to diff against; that card is not a 5090, so compare a machine with its
own previous CSV.

There is no agent method that runs the bench. On a running shell, `play.hitch_report` is the
frame-time check after a lever change. Guide: [Performance and benches](perf-benches.md).

## Zero-copy pak loads (after v0.1.26): `CompressedTexture.levels` and `CookedTexture.ktx2` are views

Since the release after v0.1.26, textures loaded from a pak point into the pak (or into its one decompressed buffer) and are not
copied (ADR-177). Two public fields changed type: `CompressedTexture.levels` is now `Vec<PakPayload>` and
`CookedTexture.ktx2` is now `PakPayload`. `PakPayload` derefs to `[u8]`, so reads compile unchanged. Code that
mutates levels as `Vec<u8>` (DO27's mesh bank `clear_levels`/`fill_levels`) needs two one-line changes:

```rust,ignore
// clear_levels: drop each level's share of its buffer (was: discard_vec(level))
for level in &mut tex.levels { *level = regia_asset::PakPayload::default(); }
// fill_levels: assign the Vec<u8> (moved, no copy) (was: *dst = src)
*dst = src.into();
```

Is park/fill still worth it? Clearing a texture's levels frees heap only when nothing else holds its buffer.
The `AssetServer` keeps the loaded `CookedTexture` (it shares the same buffer), so parking a texture that came from
the server frees nothing now. Before this change it freed only the second copy, while the server's full copy stayed. A
level served from a raw (uncompressed) pak entry is page cache, not heap, and parking it saves nothing. Keep
park/fill for levels the bank built itself (spilled or generated chains). For server-loaded textures, drop the
`AssetServer` handle as well, or skip parking.

Check: after a map load, `regia_asset::PakPayload::is_mapped()` is true for levels of raw-stored textures once the
pak is verified (below). Peak RSS during a map load is roughly half of v0.1.x's on texture-heavy maps (ADR-177 evidence).

## Faster map loads (after v0.1.26): GPU-ready meshes, parallel decode, verify before mapping

ADR-285. What changes for the host:

- **Desktop paks can store meshes GPU-ready (opt-in).** With `REGIA_PAK_GPU_MESHES=1` at package time, `regia package` / `regia cook` (never Android) write each
  mesh entry in `regia_asset::gpu_mesh` layout v1 (the renderer's vertices with tangents, `u32` indices) instead
  of the compact `CookedMesh`. Code that decodes mesh entries itself with `CookedMesh::from_postcard` fails on them:
  use `regia_asset::load_mesh_bytes` / `AssetServer::load_mesh` (both forms, `CpuMesh` with tangents) or
  `regia_render::cooked_model::load_cooked_model`. Android paks and the cooked cache keep the compact form.
  Without the variable paks stay compact and nothing changes for the host. Such paks grow by roughly the mesh
  vertex bytes (96 B per vertex before lz4 instead of 24 B). A mesh with a skin or morph targets
  (cooked mesh format v2) stayed compact until GPU mesh layout v3; since v0.7 it converts too and `load_mesh`
  returns its skin and morph targets from the payload's trailing block (ADR-10080; see "GPU mesh v3" below); a mesh
  with vertex colors converts and `load_mesh` returns `COLOR_0` from the vertex color slot (v0.5.1, ADR-7919); `pak_migrate`
  (`regia_asset_pipeline`) converts an existing desktop pak the same way without a re-cook.
- **Public type changes (compile-time, from ADR-177 and this work):** `UploadBytes::slice` is generic; `CookedTexture.ktx2` and `CompressedTexture.levels` are `PakPayload`; `regia_terrain::TileSource::load` returns `PakPayload`; `residency()` returns `PakPayload` instead of `Arc<Vec<u8>>`; `AssetLoadError` has a new `GpuMesh` variant (match with a wildcard arm). Build `shell-ferrite` against this release before taking it.
- **`load_cooked_model` decodes meshes on the compute pool** while it loads the materials, and no longer keeps
  a `CpuMesh` copy of each mesh in `AssetServer::meshes()` (the `LabMesh` is the only CPU copy;
  `AssetServer::source_of` / `state` still report them). Hosts that read those meshes back from the server after
  `load_cooked_model` must call `load_mesh` themselves.
- **First launch after an update:** a pak this build has not verified yet serves copies (read through the file
  handle) until the background `pak_verify` check passes; after that, and on every later launch, loads are
  zero-copy again. Nothing to do; `REGIA_PAK_VERIFY=0` skips the check and keeps zero-copy from the start.
- **New `AssetLoadError::GpuMesh` variant.** A `match` over `AssetLoadError` without a wildcard arm needs it.
- Do not overwrite or truncate a mounted pak in place while the game runs (patchers: write a new file and rename it over;
  on Windows replace paks only while the game is closed). The verify gate covers only the first launch after an update,
  later reads are views of the mapping. Each read also checks the open file's length and modification time: a pak
  that shrank or was re-stamped logs `pak changed on disk` once (target `regia_asset`) and is read through the file
  from then on, so a cut entry is a load error (`PakError::Corrupt`), not a crash. Views served before the change was
  noticed stay exposed.
- **Agent check:** `assets.status` reports `paks: { mounted, zero_copy }`; after the first launch's verify pass
  `zero_copy` equals `mounted`.
- **Upload ring:** the frame's staging belt writes into persistently mapped buffers. `REGIA_UPLOAD_RING=0` restores
  the previous `Vec` + `write_buffer` belt (same pixels) if a GPU shows upload hitches.
- `REGIA_UPLOAD_RING=0` turns the renderer's mapped upload ring back into the `write_buffer` belt (diagnostics only).

## Pak errors

`AssetServer` mounts every `*.fpak` in its content directory at startup (sorted path; a later pak
overrides the same manifest path). `PakReader::open` logs `failed to open pak` with the
`PakError` (`BadMagic`, `UnsupportedVersion`, `TocDecode`, `HashMismatch`, `TocOutOfBounds`,
`Io`, …) and skips that file. A bad entry logs `pak decompress failed` with the pak name, the
asset id and the error. `PakError::Corrupt` names `path`, `id`, `offset` and `reason`, and
`read_decompressed` returns exactly `size` bytes. Compression is the block table
(`block_table_offset != 0`), not `size == compressed_size`: that equality used to return the
block table as if it were the asset, and the cooked loader then failed with `Cooked(BadMagic)`.

Ship paks from `regia cook`. When a mesh is missing, read the `regia_asset` warn before
treating the id as absent. `assets.status {"asset": "<path>"}` returns `state` and `error`.
`assets.load_progress` counts `failed` items in a load set. `assets.describe` is the manifest
row. A level of loose PNGs shows up as `uncompressed_textures` (see
[Texture budgets](#texture-budgets-and-renderproblems)), which is the usual pak mistake.

**Integrity check (automatic).** After mounting, `AssetServer` checks each pak's whole-file
`content_hash` in a detached background thread. It starts 60 s after the mount and reads at most
64 MiB/s on every platform, so loading never waits for it. Its priority is per platform: nice 19
plus the idle IO class on Linux (the IO class only helps under the BFQ scheduler; NVMe's default
ignores it), background mode on Windows. It runs only on the first launch after a pak or the game
binary changed: verified paks are recorded in `cache://pak-verified.txt` (`~/.cache/RegiaEngine/<exe>/`
on Linux) and skipped after that. With no cache directory (Android) it does not run. A mismatch
logs an `ERROR` (`game data file … is corrupt …: verify the game files or reinstall the game`) and a
`pak_corrupt` telemetry note. The game keeps running. If you surface errors to the player, match
that message. `REGIA_PAK_VERIFY=0` turns the check off, and `=force` re-hashes now. Support can
run `regia doctor --verify-paks <game>/content` on a tester's install. Paks whose hash field is
all zero are skipped (tools that do not hash). `regia cook` always hashes.

## Android profiles

`RenderSettings::default()` uses `RenderProfilePreset::runtime_default()`: `MobileHigh` on
Android, `Deck` on a Steam Deck (v0.2.16, ADR-3289), `DesktopHigh` everywhere else. Do not force Desktop High on a phone. Mobile High is
`render_scale` 0.85, SMAA, `SpatialUpscaler::Sgsr`, no depth prepass, contact shadows off, probes
off, texture budget 512 MiB at a 2 K cap. `render.set_gi_mode {"mode": "depth"}` is refused.
Devices without BC texture compression decode KTX2/BCn to RGBA8 at upload (4–8× the memory); the
budget is what keeps that from exhausting a phone.

`render.get_profile` on device must show `preset: "MobileHigh"` and `platform_default` the same.
`render.set_profile {"preset": "MobileHigh"}` puts it back if something else selected a desktop
tier. `regia install-android` installs, grants the debug `INTERNET` permission and attaches the
agent; then `seeing.screenshot` and `play.hitch_report`. Guide:
[Render profile](../agents/render-profile.md).

## Feedback and crash bundles

Add `regia_framework::feedback::FeedbackPlugin::default()`. **No key is bound by default**, so
the F-keys stay DO27's. Put a "Send feedback" entry (`FEEDBACK_MENU_ITEM`) in the pause or
settings menu and call `feedback::open_prompt(world)` when it is picked, or spawn
`spawn_feedback_button`. To give it a key as well, bind the `"Feedback"` input action. Skip
gameplay input while `Feedback::is_prompt_open()`; the match and its networking keep running.
When the match id, map or player count changes, set them on `FeedbackHost`. Use `set_provider`
for the position. Crash bundles are automatic, and the next launch shows the player the path.
**Native crashes** (segfaults, GPU driver crashes; Linux and Windows) need
`regia_framework::feedback::run_crash_monitor_if_requested()` as the **first line of `main`**:
the game re-launches itself as a crash monitor, and this call turns that process into the
monitor. Without the call, native capture stays off and nothing else changes. Opt out with
`FeedbackConfig::native_crashes = false`; players can use `REGIA_NO_NATIVE_CRASH=1`. **On Windows it is
opt-in** until it has run on real Windows: set `FeedbackConfig::native_crashes_windows = true` or run with
`REGIA_NATIVE_CRASH=1`. Check that
`feedback::native_crash_status()` is `Armed` a moment after startup. Archive every release's
`dist/<platform>/symbols/` directory: `cargo xtask minidump <bundle> --symbols <that dir>` needs it
to name the crashing functions.
Nothing is uploaded; to collect bundles, read `FeedbackWritten` / `pending_bundles` and upload
them yourself after asking the player. Check: `feedback.capture {"category": "bug"}`, then
`feedback.list`. Guide: [Feedback bundles](../agents/feedback.md),
[Feedback and bug reports](feedback.md).

## Telemetry and the alpha log

`regia_telemetry` is in `DefaultPlugins`. It emits structured events under `regia::*`: the
session header, a frame summary every 10 s, hitches, GPU errors and device loss, surface errors,
`render.problems` changes, and streaming events. The shell decides where they go:

- **A host that builds its own subscriber must add the log ring.** `regia::run` / `run_with`
  install the engine logger (`regia_core::log::init`), which includes the in-memory log ring
  (`regia::core::log::ring::RingLayer`). The agent's `logs.read` and the feedback/crash
  `log_tail` read that ring. If you set the global `tracing` subscriber yourself, the engine's
  logger cannot install, so add the ring layer **in addition to** the JSON layer, or those two
  come back empty even though the JSONL file and the console log work. Give the ring a filter
  too, or it records every `trace` event of every crate:

  ```rust,ignore
  tracing_subscriber::registry()
      .with(regia::core::log::ring::RingLayer.with_filter(EnvFilter::new(regia::core::log::DEFAULT_FILTER)))
      .with(json.with_filter(EnvFilter::new(regia::telemetry::RECOMMENDED_FILTER)))
      .init();
  ```
- **JSONL file.** Use `let (json, _guard) = regia::telemetry::json_layer(dir.join(format!("do27-{session}.jsonl")), Default::default())?;`
  and add `json` to the subscriber next to the human log. Keep `_guard` until exit.
  - The writer is non-blocking, bounded and lossy. It applies a 64 MB cap (warnings and errors
    only past it), fsyncs at most 1 Hz (and on a warning or error), and collapses repeats
    within 5 s.
  - Put `_guard.stats().dropped` in your own summary.
- **Console (stderr).** `regia::run_with` already makes stderr non-blocking when it is not a
  terminal. If the shell builds its own subscriber with a stderr `fmt` layer, do not use
  `with_writer(std::io::stderr)` under a launcher. Steam or a wrapper that never reads the pipe
  fills it after about 64 KiB, and the next log line then blocks the frame thread forever. Use
  `regia_core::log::NonBlockingConsole::spawn(std::io::stderr(), regia_core::log::console::CONSOLE_QUEUE_LINES)`
  as the writer when `!std::io::stderr().is_terminal()`. It is bounded and lossy (`dropped()`
  counts what was lost). Call `flush_timeout` before exit; it never waits longer than you ask.
  This covers `tracing` output only. `eprintln!`, `println!` to an unread pipe and Rust's
  default panic message still block on a stuck stderr, so do not print from the frame thread.
- **Filter (required).** Give the JSON layer its own filter:
  `json.with_filter(EnvFilter::new(regia::telemetry::RECOMMENDED_FILTER))`. Without one,
  naga's `log` output (about 140 k lines per level load) costs about 1 ms of CPU per frame and
  fills the cap. Keep the human log at `info`.
- **Hitch attribution.** Add `.with(regia::frame_profiler())` to the subscriber. Hitch events
  then name their top CPU spans.
- **Telemetry fields (v0.1.4).** Every `regia::frame_summary` now splits the frame by itself;
  forward its `data` object (or `regia_telemetry::last_summary_json()`) into your log as is.
  - `frame_breakdown_ms`: mean main-thread milliseconds per frame by phase. `outside_update`
    (event loop, OS), `host_schedules` (schedules you ran with `App::run_schedule`), `pacing`
    (frame-cap wait), `first`, `pre_update`, `state_transition`, `fixed_update`, `update`,
    `post_update`, `extract`, `render`, `last`, `clear_trackers`. `render_split` breaks `render`
    into `acquire` (swapchain wait), `render_into`, `tail` (overlay, submit), `present` and
    `other`. `sum_ms` is their sum, `frame_ms` the mean frame interval, and `unaccounted_ms` the
    difference, shown explicitly.
  - `top_systems`: the 10 costliest ECS systems by mean ms per frame, with `p95_ms`, `max_ms`
    and `runs_per_frame`. This needs `.with(regia::frame_profiler())` in the subscriber (the
    same layer as hitch attribution). Your filter must also let `bevy_ecs` `info` spans through,
    which `info` and `RECOMMENDED_FILTER` both do. Without the layer, `timed` is 0.
  - `churn`: `created_by_label` (buffers, textures, bind groups, pipelines by descriptor
    label), `pipelines_by_label`, `upload_bytes` by source (`belt` per-frame data, `texture`,
    `mesh` cache uploads, `other_buffer`), `mesh_buffers_created`, and the renderer's `cache`
    sizes with their growth over the window.
  - `counts`: `mesh_instances`, `drawn`, `per_object_draws`, `local_lights`, `shadow_casters`,
    `culled` (`{mean, max}`), plus `entities` and `entity_slots` sampled at the summary.
  - `host_spans`: your own sections per window. Until you set prefixes, every span whose target
    starts with `do27::` or with this executable's name (`arena` → `arena::`) is timed by span name.
    A span opened on the thread that runs `App::update` goes under `main`; one opened on any other
    thread goes under `other_threads`.
    `main.unattributed_ms` is the main-thread time outside all of your sections, shown explicitly.
    `regia::core::profiling::set_host_span_prefixes(["do27::", …])` replaces both of those defaults
    (pass `do27::` again if you still want it). Call it once per process, before the first span is
    created; a second call returns `false` and changes nothing. Keep host sections synchronous:
    a span held across an `.await` and exited on another thread leaves the main-thread depth counter
    stuck, so `top_level_ms` / `unattributed_ms` go wrong. This also needs `regia::frame_profiler()`.
  - Hitch events add `frame_breakdown_ms` for the slow frame, `top_systems` (that frame's 5
    costliest systems) and `host_spans` (its 8 largest sections).
- **Level end.** Call `regia_telemetry::request_summary(world, "level_end")`, and do the same
  at match end and return to menu, so a summary closes each level.
- **`logging.level`.** For `normal`, set `JsonLayerOptions { level: TelemetryLevel::Normal, .. }`,
  and `TelemetryOptions::level` through `TelemetryPlugin` or `REGIA_TELEMETRY=normal`.
- **Session header extras.** Before the first frame, put the game version, git hash and content
  pack hashes into `TelemetryHost::set(key, value)`. They appear under `host` in
  `regia::session`.
- **Crash and stall reports.** Add `regia_telemetry::last_summary_json()`, which can be called
  from any thread.
- **Do not swallow wgpu errors.** Telemetry installs the device's uncaptured-error and
  device-lost callbacks. Do not replace them. A release build logs a validation error and keeps
  running.

## GPU device lost, GPU errors, VRAM pressure

Nothing to wire beyond `FeedbackPlugin` (ADR-171). What the host sees:

- **Device lost** (a driver reset or crash, a GPU hang, an unplugged eGPU). The game stops
  drawing at once, writes a crash bundle, logs one error (`device_lost: ...`) and exits with
  **code 3** within about 2 s, through a deliberate exit (ADR-276): the bundle is written first, then the process ends
  without running process-exit handlers. No frozen black window and no
  segfault. The next launch shows "the game closed unexpectedly" with the bundle's path, like any
  crash. In the bundle, `crash.txt` starts with `device_lost:` and holds the wgpu error count and
  the last telemetry frame summary; `host.json` has `crash_reason = device_lost`. A launcher or
  Steam wrapper can treat exit code 3 as "GPU lost". **What to do:** ask the player to update or
  reinstall the GPU driver and to lower the preset if it repeats. Collect the bundle.
- **No window surface** (the GPU cannot present to the window: a driver/display mismatch, for
  example the NVIDIA 580 driver under Xvfb, "... cannot present to this window"). Surface
  creation is retried once; then the game logs one error, writes the same kind of bundle with
  reason `surface_unavailable` and exits with **code 4**. A surface that stays lost for 120
  frames does the same. Before, the game kept running at ~1600 fps behind a blank window.
  **What to do:** check the driver and the display server (Wayland/X11, no remote or virtual
  display without GPU presentation), or pick another GPU with `REGIA_GPU_NAME` or `REGIA_GPU_INDEX`.
- **GPU errors.** When wgpu reports errors, the performance overlay shows a red
  "GPU errors: N (see log)" line, `render.problems` lists `gpu_errors`, and every feedback bundle
  has `gpu_errors` in `feedback.json`. Frames may be wrong; the log names the pass. Send the
  bundle with the log.
- **VRAM pressure.** When the GPU memory in live allocations passes 85 % of the card's
  (checked every 10 s; never on cards with under 2 GiB of dedicated VRAM), the renderer frees
  idle GPU memory (up to ~0.5 GiB at 4K, re-allocated when needed); textures are not touched.
  Only real out-of-memory errors lower the texture streaming budget: since v0.1.16 the first one
  halves it, and so does each later one within a minute of the previous one (at most three times,
  never below 256 MiB), and textures may get slightly softer. See
  [Low-memory GPUs](#low-memory-gpus-v0116).
  `render.problems` lists `vram_pressure` either way. **What to do:** suggest a lower preset or
  render scale on that machine.

Check: the overlay line appears only when N > 0, and `render.problems` stays `ok` on a healthy
machine. Guide: [Look: frame problems](../agents/look.md#frame-problems).

Check: `telemetry.get_last_summary {"session": true}` after 10 s of play. `summary.frames` > 0,
`session.adapter` names the GPU, and `session.fallbacks` lists what the machine lacks. Guide:
[Telemetry](telemetry.md), [Telemetry (agent)](../agents/telemetry.md).

## Cache residency and one-joint skins (v0.1.5)

Nothing to wire for the defaults (ADR-183). What changed and what the host can use:

- **Units that leave the view no longer re-upload.** Meshes and materials no frame draws stay
  resident while the renderer's caches fit their budget: the texture streaming budget (4 GiB on
  Ultra, 2 GiB on High) plus 1 GiB. Before, anything undrawn for 600 frames (~10 s) was freed and
  uploaded again when it came back. Over the budget, the least recently drawn entries idle for
  at least 600 frames go first. Anything idle for 36 000 frames (~10 min) goes anyway.
  The derived budget is at most half the GPU's VRAM (so a 4 GB card keeps at most 2 GiB idle),
  and on Android 768 MiB unbudgeted, at most 1 GiB. (Since v0.1.8 the default splits it into separate
  material and mesh budgets, see [Budgets, terrain holes and emissive probes](#budgets-terrain-holes-and-emissive-probes-v018).) Memory pressure (the OS low-memory event,
  the telemetry VRAM probe at 85 %, or `signal_memory_pressure()` from the host) evicts every
  unpinned entry not drawn for ~0.5 s at the end of the next frame, whatever the budget.
  `FrameRenderer::set_cache_residency(CacheResidency { budget_bytes, min_idle_frames,
  max_idle_frames })` overrides these limits; `budget_bytes: 0` keeps the derived budget.
- **Pinning without `prewarm`.** `FrameRenderer::pin_keys(keys)` takes `FrameMesh::mesh_key` and
  `material_key` values and keeps them resident whatever the age or budget. It uploads nothing and
  builds no pipeline: a key uploads the first time a frame draws it (or `prewarm` does) and then
  stays. Pin the match's unit and weapon sets at match start. Call `unpin_keys` or
  `clear_pinned_keys` when the match ends. `prewarm` still pins what it uploads.
- **Watching it.** `FrameRenderer::cache_stats()` returns resident meshes/materials, pinned keys,
  resident bytes against the budget (as of the last sweep, every 256 frames; `resident_bytes` is
  0 until the first sweep at frame 256, which does not mean empty), and running counters:
  `mesh_uploads`, `material_uploads`, `meshes_evicted`, `materials_evicted`. A steady match should
  show both upload counters flat once every unit has been seen.
- **One-joint rigid skins draw instanced.** A part whose joint palette has exactly one joint and
  whose vertices are all weighted 1.0 to joint 0 (canopies, fans, doors) is drawn as a rigid mesh
  with `model × joint`: instanced, frustum-culled, in the visibility buffer, with motion vectors.
  A still frame matches the old per-object path pixel for pixel. A moving part now also writes
  motion vectors (the skinned path wrote none until v0.1.15), so TAA/TSR smear it less. `DrawStats::rigid_skin_folded` counts these meshes;
  `per_object` drops by the same number. Decal layers, GI proxies and emissive lights still treat
  the part as skinned. Parts with blended weights or several joints keep the skinned path. The
  check runs once per `mesh_key` and vertex count, so a key must name its vertex content (as for
  the mesh cache). To turn it off: `FrameRenderer::set_rigid_skin_fold(false)` (TAA output of
  moving parts then matches v0.1.4 again; switching at runtime is safe).

Check: after a minute of play, `cache_stats().mesh_uploads` stays flat while units leave and
re-enter the view, and `draw_stats().per_object` is lower than in v0.1.3 in a scene with
canopies and fans.

## Skinned motion vectors (v0.1.15)

Nothing to wire (ADR-221). Skinned meshes with several joints (pilots, Steve, anything animated)
now write motion vectors, so TAA and TSR keep moving characters as sharp as rigid hulls; before,
they wrote none and smeared on every tier. The renderer keeps last frame's palette per
`FrameMesh::object_key` itself: keep submitting only the current palette, and keep a character's
`object_key` stable from frame to frame (as for rigid motion). A new key, a character that did not
draw last frame, a camera cut, or a changed joint count gives that frame no limb motion (the model
matrix still moves it). Motion blur, when on, now streaks moving limbs too.

Check: walk the pilot in 3P with TSR on (any tier) past a fixed camera: the pilot stays as sharp as
with `DO27_QUALITY_OFF=taau` (minus the aliasing), and no trail follows the legs.

## Low-memory GPUs (v0.1.16)

Nothing to wire. What changed for a machine like the tester's Radeon 610M (integrated, 5.7 GB of RAM), ADR-223:

- **Integrated GPUs on desktop** (not the Steam Deck, not Android) get a texture streaming budget of at most
  384 MiB, never more than the device-local heap or a quarter of system RAM, with low mips first (no eager first
  use). The new-scene warm on a scene-less window is off there by default. `REGIA_INTEGRATED_LIMITS=0` restores the
  v0.1.15 behaviour; `REGIA_SCENE_WARM=1` keeps only the warm.
- **A streamed texture that cannot be allocated** no longer reaches the uncaptured-error handler: it is retried one mip
  smaller (first use) or dropped (growth), and streaming pauses for 5 s (up to 60 s on repeats). The first
  out-of-memory error halves the streaming budget. One less than a minute after a trim holds the streamed textures a
  mip lower for a minute (softer textures, then they sharpen again).
- **Still fatal:** an out-of-memory error in a `write_buffer` upload, a submit or a mesh upload still loses the device
  (wgpu); the game exits with code 3 and a crash bundle, as before. (Streamed-texture `write_texture` uploads and
  their bind group rebuilds are inside the error scope since the hardening patch: see
  `docs/releases/v0.1.21-oom-hardening.md`.)
- **Large integrated GPUs keep the presets** (Apple M-series, big APUs: 15,000 MiB of RAM or 8 GiB of device-local heap
  or more). `REGIA_INTEGRATED_LIMITS=1` forces the defaults, `0` turns them off. The OOM test hooks need the
  `regia_render` feature `test-hooks`.
- `local_shadow_max_lights: 0` (DO27's Low) no longer allocates the 64 MiB local shadow atlas.
- **The integrated-GPU floor row** (v0.7, ADR-8878): with ULTRA LOW selected as `VeryLow`, admit loads against
  `MemoryBudget::for_tier(BudgetTier::IntegratedLow)` (2.5 GiB RAM, 1.5 GiB GPU memory; agent `render.memory_budget
  {"tier": "integrated_low"}`). Very Low streams textures with at most 384 MiB and lazy first use on any RAM size, so a
  16 GB integrated laptop no longer gets Desktop Low's 1 GiB there. Measured row for the forest demo scene on the N150
  min-spec laptop (`docs/perf-log.md` "N150 integrated_low row", 2026-11-20): Very Low at 1280×720 runs frame p50
  31.8 ms on Vulkan and 20.0 ms on DX12, peak working set 1779 / 1438 MiB, peak GPU process memory 676 / 661 MiB,
  no growth over 10 minutes. The row for your maps (spec 13 § 3a S2) is yours to run.

**What DO27 should do on its side** (from the analysis, not engine changes): free the previous map
(`release_meshes`, drop its materials) before the menu preloads the next one, and wait for the memory to drop
(`render.memory_pressure` `categories`, or the summary's `memory.categories`); keep `REGIA_SCENE_WARM` unset on
integrated GPUs; cook material textures to BC7/BC5.

Check: `render.memory_pressure` on the target machine. `integrated_limits` is `true` on an integrated laptop and
`false` on a discrete card and on the Deck; `stream.budget_bytes` is 402653184 (384 MiB) or less on the laptop;
`oom_uncaught` stays 0 through a map change. After 10 s of play `telemetry.get_last_summary` has
`memory.categories` with non-zero `meshes` and `textures`. Guide: [Low-memory GPUs (agent)](../agents/low-memory-gpus.md).

## Object keys (v0.1.15)

`FrameMesh::object_key` is the identity the renderer pairs a mesh with last frame's model matrix (and
joint palette) by, for motion vectors under TAA, TSR and FSR 3. Rules:

- Give every moving object a stable non-zero key: `key = (entity_id << 8) | part_index`, entity id under
  2^48, one key per (instance, part).
- Never use a key whose top byte is `0xFE` or `0xFF` (`0xFE` is the renderer's ephemeral range).
- Keep a key stable for the object's life and never reuse it for another object within a match.
- Key `0` is ephemeral: the renderer keys the mesh by its index in the frame's mesh list. A spawn, despawn
  or reorder then pairs meshes with another object's previous matrix, and the shifted ones smear under
  temporal AA. Scatter fields that never move are fine with key `0`.

The renderer logs one `tracing` warning (target `regia_render`) per run when a temporal-AA frame has at
least `ZERO_KEY_WARN_MIN` (4) zero-key meshes whose matrix changed at their list index, and
`DrawStats::zero_key_moving_meshes` counts them every frame; the telemetry window reports its maximum as
`draws.zero_key_moving_meshes`. Host change: none required; `DrawStats` gained a field, so a host that
builds it with a struct literal (rare, it is output only) adds `..Default::default()`.

Check: after loading a match, `draws.zero_key_moving_meshes` in the alpha log is 0.

## Texture views (v0.1.6)

Cockpit screens, the target feed and the map are cameras with `RenderTarget::Texture { asset_id }`.
Before v0.1.6 they rendered through the window's view state and cost the window its targets,
history, paged shadows and terrain uploads every frame (ADR-184). Now each texture view keeps its
own state, renders a reduced feature set, and the window's frame is the same with or without
them. What to wire:

- **Behavior change for existing texture cameras.** Every texture camera now renders the reduced
  feature set below, including cameras that have no `TextureViewSettings` (mirrors, UI previews,
  screens set up before v0.1.6). Such a camera loses temporal AA, SSAO, SSR, SSGI, contact shadows,
  fog, clouds and probe lighting, and uses two sun cascades. It no longer looks like the window.
  This is intended: a texture camera at full window quality was what cost DO27 60–87 ms. Check
  each texture camera by eye after updating.
- **Settings per camera.** Add `regia_render::TextureViewSettings` to the camera entity
  (without it: 512², every frame, the defaults below).
  - `size: [w, h]` (default 512×512).
  - `update_every: u32`: render on one frame in N (1 = every frame). Use 2–6 for screens and the
    map (10–30 Hz at 60 fps). `paused: true` keeps the last image and costs nothing.
  - `fxaa`, `bloom` (default on), `sun_cascades` (default 2, 0 = no sun shadows),
    `shadow_resolution` (default 1024), `local_shadows` (default on, ≤ 1024² atlas, 4 lights).
  - A view redraws its cascades whole on every refresh, so its shadow cost grows with the
    casters the cascades cover. For the top-down map use `sun_cascades: 1` (or 0).
  - Always off in a view: temporal AA and upscaling, SSAO, contact shadows, SSR, SSGI, probe
    updates (views take ambient light), volumetric fog and clouds, refraction, motion blur, depth
    of field, VFX layers, gizmos.
  - A view draws meshes through the same GPU-driven batches as the window (frustum-culled with
    the view's own camera, no Hi-Z occlusion), so a unit spawning in a view's sight allocates
    nothing (v0.1.6 fix; before it, each new object cost the view a uniform buffer and a bind
    group).
- **Show it on a surface (GPU, no CPU copy).** Create the settings with a material texture and
  bind that texture in the screen's material:

  ```rust,ignore
  let view = regia_render::TextureViewSettings::with_texture([512, 512]).update_every(3);
  let screen = view.texture.clone().expect("texture");
  // Graph material slot:
  let slot = regia_render::GraphTexture::from_dynamic(screen.clone());
  // or a standard material: images.emissive / images.albedo = Some(MaterialImage::Dynamic(screen))
  commands.spawn((camera_with_texture_target, transform, view));
  ```

  Each new view image is copied into that material's texture on the GPU at the end of the view's
  frame. The texture is created with the view, so a screen shows the view from its first frame.
  Texture views render before the window in the same frame, so the screen shows this frame's
  image. A view that sees its own screen shows its previous image. Do not `write` to that
  `DynamicTexture` from the host.
- **Show it in the UI.** `ui_renderer.add_image_view(&device, image_id,
  &runtime.0.texture_target_view(asset_id)?)` once the view has rendered, then use `image_id`
  like any UI image. Register again if you change the view's size.
- **Read it on the CPU without stalling.** `runtime.0.request_texture_readback(&device, asset_id)`,
  then on a later frame `runtime.0.take_texture_readback(asset_id)` returns
  `Some(Ok(ForwardResult))` (RGBA8, sRGB) once the copy is mapped. It never waits. Keep one request in
  flight per view. **`read_texture_target` is blocking** (it waits for the whole GPU queue, 85 ms in
  the v0.1.4 log): use it only in tools and tests.
- **Hosts that drive `FrameRenderer` themselves** call `render_texture_view(&device, asset_id,
  frame_scene, &settings)` per view before the main view's `render_into`.
  `render_into_texture` still renders as the main view (for benches). `drop_texture_view(asset_id)`
  frees a view: its targets, its material texture if no material binds it, and every cached bind
  group over its buffers. The runtime does this 120 frames after the camera stops being extracted.
- **Resizes return their memory.** Before v0.1.6, every window size, fullscreen toggle and
  dynamic-resolution step left its render targets allocated (DO27 on an RX 7900: 2.7 → 17.5 GB in
  20 s, still allocated after the churn stopped). Old targets are now released within 4 frames of
  their last use. Check: toggle fullscreen ten times, wait a second, and `vram_allocated_bytes` in
  `frame_summary` is back where it started.

Check: with the screens and the map on, `telemetry.get_last_summary` shows
`texture_views.views_mean` = number of texture cameras, `renders_per_frame` ≈ Σ 1 / `update_every`,
and `per_view[].gpu_ms_mean` per view (GPU timing on). `churn.created_by_label` stays empty during
play, and `gpu_pass_ms` and `frame_ms` of the window match a session with the views off plus the
views' own GPU time.

## Audio (v0.1.6)

Fixes the "audio drops out, then stays muffled" report (ADR-185). What changes for a host:

- **Suspend ends every voice, and nothing stays "playing".** After `suspend_output` (or the
  engine's own pause on a hidden or throttled window) every voice is gone: no duck sticks, no
  voice-budget slot leaks. Restart your loops (and `start_voice_loopback`, which also ends)
  after the resume, as before. Music stems and
  spawned patches are not restored either: `despawn_patch` and `spawn_patch` again after a resume.
- **Resume is idempotent.** `resume_output` on a live output does nothing and returns `true`;
  `resume_count()` counts only real re-opens, so `resume_count() == suspend_count()` while the
  output is live, and `output_suspended()` says whether it is down. If you re-open the stream
  yourself in `Update`, the engine's gate no longer resumes it a second time in `PostUpdate`,
  so the loops you restarted keep playing. `false` means no device yet: call it again later.
- **Your bus volumes and mutes survive.** The rebuilt buses get your `set_bus_volume` /
  `set_bus_mute` / `apply_user_settings` values and the active snapshot's filter cutoffs, not
  the layout defaults. You no longer need to rebuild a fresh `Audio` to recover.
- **Patch `filter_curves` work.** A filtered layer's loops now play through its low-pass
  (a child track under the patch bus), so the hover/engine cutoff retune is audible.
- **New: `Audio::start_asset(&asset, params) -> SoundInstanceId`.** Starts at once (no queue)
  and returns the id for `stop_voice`, `pause_voice`, `set_voice_volume_db`, ... `play_asset`
  is unchanged.

Check: play a looping Voice-bus line, minimize the window (or suspend) and restore it. Within
1 s of the resume Music/Sfx/Ambience are back at the player's settings, and
`real_voice_count() + virtual_voice_count()` is back to what your restarted loops account for.

## Shader warm threads (v0.1.8)

`PipelineWarmQueue` now has two thread budgets, so the pre-menu shader warm uses the whole machine:

| `WarmMode` | Workers | When |
|---|---|---|
| `Boot` (the default) | all cores but two, 1..=12; at most 4 on Android and under 8 GiB RAM on Linux (`boot_threads()`) | engine boot and `--prepare` |
| `Play` | a quarter of the cores, 1..=4 (`default_threads()`) | while the game plays |

- `PipelineWarmQueue::default()` starts in `Boot`. The engine switches to `Play` when the first game frame is
  presented (the same moment `GameFrameWarm::first_frame_ms` is set), or, with no `GameFrameWarm`, at the first
  drawn scene frame (window, offscreen texture view). Nothing to do for a normal host. A host that draws only
  through `FrameRenderer` itself (no ECS render systems) calls `queue.set_mode(WarmMode::Play)` once booted.
- **After that first frame the queue stays at `Play`.** Nothing returns it to `Boot`. To warm fast around a later map
  load or loading screen, the host does it (two lines):
  `queue.set_mode(WarmMode::Boot);` before the load, `queue.set_mode(WarmMode::Play);` when it is done.
- `queue.set_mode(WarmMode::Play)` or `queue.set_threads(n)` switches a running queue without dropping a queued job:
  growing spawns workers at once; extra workers retire after the job each is running.
- A host's `--prepare` (warm everything, then exit) never presents a game frame, so it stays in `Boot`: no call needed.
  The engine command for that is `regia prepare` (next section). A host that wants fewer threads there passes
  `--threads N` or calls `set_threads(n)` first.
- `queue.threads()` is the wanted count; `queue.live_workers()` counts workers still alive, including ones finishing a job.
- `shutdown()` is unchanged: it joins every worker, retired ones included.

## Install prepare (batch v020)

`regia prepare` warms the shipped pipeline units on a headless device, then writes a stamp the launcher can
trust. It is the "Preparing shaders…" step: run it from the game's install or update, or from the game binary
before Play, and exit without presenting a frame. It uses the same adapter order as the game
(`instance_backends`: Vulkan, and on Windows DX12 as well; wgpu tries Vulkan first) and the Boot thread budget.
It never calls `note_scene_frame`, so the workers stay at Boot.

The cache directory must be the game's, not the `regia` tool's (`regia` would otherwise use
`cache://pipelines` under its own executable name). Pass `--cache-dir` or set `REGIA_PIPELINE_CACHE_DIR`
(the env var wins, same as the runtime). `--shipped-dir` is the package's `content/pipelines`. `--salt` is the
content salt (`pipeline_cache::set_content_salt`); change it when a build ships new material graphs.

```
regia prepare --cache-dir <game cache://pipelines> [--shipped-dir content/pipelines] [--salt S]
    [--profile DesktopHigh] [--threads N] [--timeout-secs 480] [--no-nice] [--force]
regia prepare --cache-dir <dir> --check     # exit 0 current, exit 2 still needed; does not warm
regia prepare --cache-dir <dir> --status    # one JSON object; does not open a device
```

A game binary can call `regia_render::prepare::run` (or `regia::render::prepare::run`) with a
`PrepareRequest` and then exit. Same rule: do not present, and do not call `note_scene_frame`.

Stdout is one JSON object per line (`PrepareProgress::to_json`: `event`, `state`, `phase`, `done`, `total`,
`fraction`, `pipelines`, `threads`, `backend`, `message`, `error`, `skipped`). Human text is on stderr.
`state` is `starting`, `running`, `ready`, `failed`, or `stale`. `--check` prints `ready` when the stamp
matches and `starting` with phase `check` when it does not; the exit code is what a launcher should branch on.
`--status` prints `absent` when the file is missing.

Two files land in the cache directory:

- `prepare-stamp.json` — written only after the warm and the cache save both succeed. A later run skips when
  the version is 1, `ok` is true, and the cache key and the unit list both match. The unit list includes the
  `--profile` frames, so a later run with a different profile does not skip. `--force` deletes the stamp
  before the warm, so a crash mid-warm cannot skip onto a half-written cache.
- `prepare-status.json` — rewritten about five times a second. A `running` file whose `updated_unix` is more
  than 45 s old reads as `stale` (no pid check; it is the same on Windows).

The warm is the splash (`Bgra8UnormSrgb`) and the canonical lit frame in `Bgra8UnormSrgb` and
`Rgba8UnormSrgb`, plus the same frame at the named profile's post stack when `--profile` is set. The main
thread and each warm worker drop to background priority (nice 19 on Linux and Android, background thread mode
on Windows) unless `--no-nice`. `REGIA_PIPELINE_CACHE=0` is an error: there is nowhere to stamp.

Not in this step: carrying a pipeline-cache blob across a driver change, saving the cache concurrently with
the last compiles, and checking packaged textures.

## Audio (v0.1.7)

Fixes "the audio drops out during hitches" on real GPUs (ADR-187). In v0.1.6 the engine paused
the audio output after 5 frames in a row of 50 ms or more, on any adapter. On the RX 7900 XTX,
ordinary hitches (map preload, texture eviction, the downed/revive camera) tripped it 14 times in
a 13-minute match. Each pause ended every voice, so your loops restarted.

- **New resource: `regia_audio::AudioThrottle { Auto, On, Off }`.** It controls only the
  slow-frame pause. `Auto` (the default) arms it only when frames are rendered by a software
  adapter (lavapipe, llvmpipe, WARP: the Xvfb soak case it exists for). `On` is the v0.1.6
  behavior. `Off` never pauses for slow frames.
- **The hidden-window pause is unchanged in every mode.** Minimizing (or every window hidden)
  still suspends and resumes the output, so keep your restart-loops-after-resume code.
- **DO27 can insert `AudioThrottle::Off`** (`app.insert_resource(AudioThrottle::Off)`, before or
  after `AudioPlugin`), but you don't need to: `Auto` already never pauses on a real GPU.
- **The fact `Auto` reads:** `regia_platform::SoftwareRenderer(bool)`, published by the
  renderer when its device is created. It's absent before the first device, which counts as
  "not software".
- **Telemetry.** Every engine pause logs its reason: `audio output paused: every window is
  hidden` (info), or `audio output paused: N consecutive frames slower than 50 ms` (warn, with
  the streak and the frame time). `Audio::throttle_suspend_count()` counts the slow-frame
  pauses. It's a subset of `suspend_count()`. Your "AUDIO: output suspended ... frames were
  slower than 50 ms or the window was hidden" line can now say which one by comparing the two.

Check: on the 7900 XTX, play a match and grep the log for `audio output paused`. Only
hidden-window lines should appear, and `throttle_suspend_count()` should stay 0.

## Quit (v0.1.8)

Fixes "quitting hard-locks the PC" (Windows, RTX 5080, fullscreen, Immediate, HDR) and "the
desktop freezes for a minute after quit" (Hyprland, RX 7900 XTX) (ADR-188). Every quit now runs
one ordered, time-boxed GPU shutdown before `regia::run` returns:

1. stop the pipeline warm queue;
2. leave fullscreen on every window;
3. wait for the GPU to go idle (at most `gpu_idle_timeout`, 1 s);
4. drop the surfaces (swapchains): `Suspended`, then `WillTerminate`, as before;
5. drop the renderers and every GPU object (the world is cleared);
6. drop the device (waited for at most `device_drop_timeout`, 1 s);
7. write the pipeline cache.

What changes for a host:

- **Quit with `regia::request_clean_exit(world)`** (or write `AppExit`, or close the window:
  same path). Then let `main` return. Don't call `_exit` / `TerminateProcess` from a system:
  that skips steps 2 to 6, and that's what locked the PC.
- **Keep the 3 s deadline as a watchdog, not as the quit.** Insert
  `CleanExitSettings { watchdog: Some(Duration::from_secs(3)), ..Default::default() }` (or call
  `regia::platform::arm_exit_watchdog(Duration::from_secs(3), 0)` when you start quitting). If
  the process is still alive at the deadline, during the sequence or in the exit handlers after
  `main`, it ends with `_exit` / `TerminateProcess`. It's off by default.
- **Your `WillTerminate` systems still run** (save settings, flush logs), at step 4, with the
  world intact. After step 5 the world is empty: don't keep work for later.
- **Hiding the window first is fine** but no longer needed; the engine leaves fullscreen itself.
- **The log tells you where time went**: `regia exit: <step> took <ms> ms` per step, a warning
  when a step is over budget (the sequence goes on), `regia exit: GPU device destroyed; no wgpu
  objects left`, and `regia exit: done in <ms> ms`. A typical quit takes well under a second.
- **Android and alt-tab are unchanged.** A suspend still drops only the surfaces and keeps the
  device.

Check: quit from fullscreen (Immediate, HDR on) on the Windows box and on Hyprland. The desktop
comes back at once, and the log ends with `regia exit: done in ...` and no over-budget warning.

## Quit timing and the game-warm watchdog (v0.1.10)

Nothing to wire (ADR-196). From the v0.1.8 integration smoke on a loaded box: "runtime renderer
dropped in" took 0.36–1.73 s, and Bore Twelve 4K finished the sequence in 2.95 s of the 3 s
watchdog; a game warm still compiling was abandoned at 20.6 s.

- **Step 5 no longer waits for the renderer.** The runtime renderer (and any warmed renderer the
  game warm left behind) is freed on a helper thread while the world clears; step 6 waits for it
  within `device_drop_timeout` (1 s), then drops the device. A free that hangs in the driver can't
  hold the sequence longer than that.
- **The log names what was slow:** `regia exit: runtime renderer dropped in <ms> ms (off the
  event-loop thread; freed N buffers, N textures, …); slowest: <part> <ms>, …` and, for step 6,
  `(renderer wait <ms>, pipeline cache read <ms>, device drop <ms>)`. Send those lines with any
  slow-quit report.
- **Set the watchdog to 8 s, not 3 s.** The bounded steps alone allow about 5.5 s in the worst
  case: warm-queue stop 2 s (only when a warm job is mid-compile), GPU idle 1 s, device step
  1.1 s, plus surfaces, world clear and the cache write (each well under 0.5 s seen so far). Add
  your own flush (0.4–1.2 s in the smoke). A typical quit is under 0.5 s on a quiet machine and
  about 2 s on the loaded farm box; the watchdog is only for a driver that never returns.
- **The game warm on a slow machine** (a loaded box, lavapipe) is no longer abandoned while it
  is still building pipelines: the loading screen stays up until it is done (at most
  `max_timeout`, 30 s), and the first game frame compiles nothing. The log says
  `game frame warm still building after … s (N pipelines so far …)` once; an abandon says why
  (`no pipeline built for … s` or `reached GameFrameWarm::max_timeout`) and how to change it.
  `GameFrameWarm::timeout`, `progress_grace` and `max_timeout` are plain fields on the resource;
  `progress_grace = Duration::ZERO` restores the v0.1.9 behavior.

## Budgets, terrain holes and emissive probes (v0.1.8)

Nothing to wire (ADR-189). What changed, from the first tester logs (RTX 4080, Switchbacks CPU-bound at 41–54 ms):

- **Terrain holes cost nothing per frame.** Each terrain node's hole state is computed once and kept until
  `set_holes` changes the mask (it was recomputed for every pass list every frame, copying every hole shape each
  time: ~29 ms a frame on Switchbacks). Calling `set_holes` every frame with the same mask is fine (compared, not
  copied). `TerrainSharedDraw::stats().hole_tests()` counts the nodes classified by the last frame: 0 on a held view.
- **Texture budget follows the card.** On a discrete GPU the streaming budget is at least a quarter of its VRAM: a
  16 GB card at High gets 4 GiB (was 2), 24 GB at Ultra 6 GiB. Integrated GPUs, Android, the Deck and DX12 (VRAM
  unknown) keep the preset's. `FrameRenderer::set_vram_scaled_budgets(false)` restores the preset's budget.
- **Textures never evict meshes.** With the default `CacheResidency` (`budget_bytes: 0`) meshes and materials have
  their own budgets: materials = the streaming budget + 1 GiB (at most half the VRAM), meshes = an eighth of the VRAM,
  1–4 GiB. Nothing drawn in the last 20 s is evicted by a budget (memory pressure still trims it), and a cache
  over its budget is swept down to 7/8 of it. A host-set `budget_bytes` keeps the v0.1.5 single budget.
  `FrameRenderer::cache_budgets()` shows the bytes against each budget.
- **Emissive lights don't hitch.** Fitted emissive lights and their probes are kept ~10 min (was 300 frames, so units back in
  view were fitted and traced again). The probe BVH holds only reachable triangles. After the load frame, new
  emitters are traced nearest first and whole: the first one even when it holds more than 16 lights, a mesh with no
  cache key always, and any further emitter only while the frame stays within 16 lights. A wave of separate emissive
  units lights up over a few frames instead of stalling one. `FrameRenderer::emissive_lights().probes_deferred`
  counts the lights still waiting.

Check: on Switchbacks the `terrain_upload` span stays under a millisecond. With units leaving and re-entering view,
`cache_stats().mesh_uploads` stays flat, and `cache_budgets()` shows `material_bytes` under `material_budget_bytes`
on a 16 GB card. No `emissive_lights` hitch over a few ms when a wave of units appears.

## Terrain filtering (v0.1.8)

Fixes "the terrain is extremely low res, like LOD 3" at 4K (request 20, ADR-192). Ground textures now
filter anisotropically, and the terrain takes a texture mip bias under TAAU.

- **Nothing to call.** Your terrain wrapper calls `TerrainGpu::upload_shared` with the frame's
  `SharedFrameInfo`; that call now applies both settings. `TerrainSharedDraw` hosts get them too.
- **The level comes from the profile.** It's `RenderProfile::terrain_budget.anisotropy`. Desktop Ultra
  is 16, Desktop High 8, Desktop Low and Deck 4, Mobile High 2, Mobile Low 0. 0 is the v0.1.7 look,
  bit for bit. To override the preset: `settings.profile.terrain_budget.anisotropy = 8;` (any value
  0–16).
- **What it covers.** Layer albedo, normal, height and ORM, triplanar cliff sides, the macro tint and
  normal, and the per-pixel heightfield normal (and the terrain virtual texture, if you turn it on).
- **The TAAU mip bias.** On a TAA frame that upscales, the terrain picks the mip the output resolution
  would: `log2(render / output) − 0.1`, the same rule TSR frames use (ADR-101). At the tester's 0.58
  that's −0.89. `SharedFrameInfo::terrain_texture_lod_bias` carries it. Mesh materials are unchanged
  under TAAU.
- **Your own path (no `upload_shared`):** call `TerrainGpu::set_anisotropy(level)` and
  `set_texture_lod_bias(bias)` yourself.

Check: stand on foot on the Switchbacks valley floor at 4K and look along the ground.
`render.get_profile` reports `profile.terrain_anisotropy`. The crust and grit should now reach
50 m and more, instead of smooth relief from about 25 m. At 1440p 87% on a 4K panel the ground is
still softer than native, but it keeps its grain.

## Android thermal governor and clean exit (batch v020)

- **Nothing to call.** `regia::DefaultPlugins` adds `ThermalGovernorPlugin`. On Android it lowers
  `profile.render_scale` (× 0.85 / 0.7 / 0.55) when the phone nears the OS's severe thermal status,
  and restores it 30 s per level after it cools. Your preset's scale stays the base.
- **Turn it off** with `app.insert_resource(ThermalGovernor { enabled: false, ..default() })`
  (`regia_framework::thermal`), or set `enabled` at run time. Read `ThermalState` (a
  `regia_platform` resource) for your own HUD or policy.
- **Quit on Android.** The generated activity reports `onDestroy`; the engine then runs the same clean
  exit as a desktop quit. A shell with a custom activity must keep `RegiaActivity`'s `regiaState()`.
- `render.get_profile` has a `thermal` object. Guide: [Android](android.md).

## In-repo player sweep host (batch v020)

Nothing to wire in the game shell (ADR-199). `regia::run` is still how DO27 opens its window.

- **`do27_chase --window`** is the in-repo test host. It loads `REGIA_DO27_CONTENT` and presents
  the chase into a desktop window titled `regia.do27_chase`. On a machine with no display, run it
  under `xvfb-run -a`. It is not built for Android.
- **`do27_chase --summary <path>`** writes spec 28 `summary.json` for that process (`exit.clean`,
  frame percentiles, pipeline-cache hit or miss). `--crash-test` writes `exit.panic: true` and then
  panics, so a sweep can see a crash without a map load.
- **`cargo xtask player-sweep`** is the nightly run: cold first launch, a 10-minute replay
  (`--replay-seconds`, default 600), five quits, and the crash test. Each child writes
  `summary.json`. The table lands in `maintainer-scripts/reports/sweep-<date>.md`. The GTX 1080
  row runs only when `REGIA_156_STABLE=1`. Other machines that do not answer are skipped, not
  passed.
- **`cargo xtask smoke --scenes`** is rows 19–21 only (ultrawide, black sky, many units).

Check: `do27_chase --window --map switchbacks --frames 2 --summary /tmp/summary.json` leaves a
schema-1 file with `host.app` = `do27_chase` and `exit.clean` true. `do27_chase --crash-test
--summary /tmp/crash.json` leaves `exit.panic` true and a non-zero exit.

## Windows adapter pick and DX12 fallback (batch v020)
- On Windows, with no `REGIA_GPU_*` variable, the engine now picks the best Vulkan hardware adapter that can present, and uses DX12
  only when Vulkan has none (software Vulkan such as lavapipe does not count). Linux and Android are unchanged.
- To force DX12 (or any adapter), run `list_adapters()` (each entry shows its backend) and set `REGIA_GPU_INDEX=<n>` or
  `REGIA_GPU_NAME=<substring>`.
- New, additive API: `ResourceBindingTier`, `AdapterSummary::resource_binding_tier` (a new public field: hosts that build
  `AdapterSummary` by struct literal must add it), `regia_platform::paths::saved_games_dir()`, and the `static-dxc` cargo feature
  on `regia` and `regia_render`. The tier is logged at device creation; the `v010_detect` tool is not in this repo.
- Not yet run on real Windows hardware: the DX12 path, the FXC compile of `lab_forward`, and native crash capture (opt-in with
  `REGIA_NATIVE_CRASH=1`).

## HUD in render-out (batch v020)

Whole sequences (any size and rate, supersampled, PNG or a movie through ffmpeg) come from
`render.out_start`, see [Render-out](../agents/render-out.md); it draws this HUD for every frame.

The game UI is part of a rendered frame, laid out for the output, not the window.

- **Agent / CLI.** `seeing.screenshot` with `target: "offscreen"` now draws the runtime UI by
  default (`hud: true`; pass `hud: false` for the old HUD-free image). `layout` is `"auto"` (by
  output orientation), `"landscape"` (16:9) or `"portrait"` (native 9:16). `regia render
  --offscreen --width 1080 --height 1920 --layout portrait` does the same from the shell.
- **Your own video loop.** Call `regia_ui::begin_render_out(world, &RenderOutSpec::new(width, height,
  layout))` (or the `regia_render::RenderOutHud` resource's `begin`), draw the window overlay's
  `"ui"` layer into your output texture, then `end_render_out`. `begin` lays the UI out at the output
  size and fills the draw list; `end` puts the window layout back.
- **One root per layout.** Tag a `UiRoot` with `UiVariant(UiLayoutVariant::Portrait)` (or
  `Landscape`); it is laid out and drawn only in that layout. Roots without the tag show in both. A
  portrait phone window picks the portrait roots by itself.
- **Safe areas.** Roots with `SafeAreaPadding` are inset during a render-out by `default_safe_area`:
  5 % per edge for landscape (broadcast title-safe), 12 % top / 20 % bottom / 5 % left / 10 % right
  for portrait (clear of the controls short-video apps draw). Set `UiOutput::safe_area` to override.
  Windowed play keeps using the platform `SafeArea`.

## Temporal clouds (batch v020)

Volumetric clouds (Desktop High and Ultra, only when the level or a `CloudSettings` resource sets
coverage above 0) now march one low-resolution texel in 16 per frame and reproject the rest
(ADR-205).

- **Nothing to call.** The presets turn it on (`RenderProfile::cloud_reprojection`). A camera cut
  marches the whole layer for one frame: keep calling `FrameRenderer::camera_cut` on level loads
  and teleports, as for TAA.
- **Advance `CloudSettings::time_s` smoothly** if you animate wind. The reprojection removes the
  drift `wind × time` between frames; a jump in `time_s` or a change of `wind_m_per_s` is caught by
  the neighbourhood clamp and settles over 16 frames.
- **Ultra marches 96 steps** (`RenderProfile::cloud_step_percent` 150 of `CloudSettings::steps`).
  Set it to 100 for the v0.1 step count, or set `cloud_reprojection = false` to march every texel
  every frame (the v0.1 cost and per-frame noise).
- **Host struct literals** of `RenderProfile` or `FrameQuality` must add the two fields; hosts that
  start from `RenderProfile::from_preset` / `FrameQuality::from_profile` need no change.

Check: `render.get_profile` reports `profile.cloud_reprojection` and `profile.cloud_step_percent`.
On a cloudy map the "clouds" GPU timing drops (RTX 5080, 1440p, coverage 0.5: High 0.98 → 0.35 ms,
Ultra 1.37 → 0.45 ms at 96 steps), and a held camera shows still clouds instead of a fine shimmer.
Clouds soften slightly during fast turns and sharpen again within 16 frames of the camera stopping.

## Cloud lighting (batch v020)

Volumetric clouds are lit with a cone of samples toward the sun, darker thin edges and bases, curl-swirled
wisps, the sun as it reaches the layer's own height, and the sky's aerial perspective in front of them
(ADR-217, cloud lighting).

- **Nothing to call.** Desktop High and Ultra turn the cone light on (`RenderProfile::cloud_cone_light`).
  The sun at altitude and the aerial perspective apply whenever clouds draw with a level clock and the
  physical sky.
- **Sunsets change.** The layer now keeps the sun for a few minutes after the sun sets for the ground
  camera, warm at the tops. Re-check maps that set an evening `TimeOfDay`.
- **Distant clouds take the sky's haze** (the aerial LUT) instead of a fixed 45 km fade, so a far bank is
  bluer at noon and warmer at sunset.
- **Host struct literals** of `RenderProfile` or `FrameQuality` add `cloud_cone_light`; hosts that start
  from the presets need no change. Set it to `false` for the v0.1 light march (slightly cheaper).

Check: `render.get_profile` reports `profile.cloud_cone_light`. Cost: the cone takes as many cloud density samples as
the v0.1 light march (six, three with detail), plus a small curl fetch; RTX timings are still to be measured (the
`cloud_temporal_cost_at_1440p` bench has cone on and off rows).

## Lamps, muzzle flashes and explosions on the ground (batch v020)

Point and spot lights now light the shared terrain the way they light a mesh of the same albedo and roughness:
the same froxel lists, falloff, spot cone, local-light shadows and BRDF (ADR-211, terrain local lights). A
hull lamp under the Remora lights the pad under it; a muzzle flash or explosion lights the crater floor. Nothing
to call: `FrameRenderer` hands the lights to any `SharedPassDraw` (`set_engine_lights`), and
`TerrainSharedDraw` uses them on every device with room for the bindings: 6 storage buffers per fragment stage
without the terrain virtual texture, 8 with it (desktop, Pascal and the Mobile tier fit). A shadow-casting light (`cast_shadows: true`) shadows the ground behind meshes;
the terrain itself does not cast into the local-light atlas.

- **Ship:** nothing; it is on. A light that should stay off the ground (a cockpit lamp) needs a radius that
  ends before the ground.
- **Off:** `TerrainSharedDraw::set_local_lights_enabled(false)` (the v0.1 look and the v0.1 cost: the colour
  pass is built without the light loop; lookdev `LOOK_R2_NO_TERRAIN_LIGHTS=1`). The terrain warm job and
  `prewarm` build both variants, so switching at any time after the warm compiles nothing.
- **Check:** `seeing.screenshot` with a lamp 1–3 m above the ground: a lit pool under it. `cargo test -p
  regia_terrain --test local_lights` is the parity test (grey mesh plane and grey terrain under one lamp).

## Local exposure (batch v020, scene format v9)

`LevelSettings.look.local_exposure` lifts dark regions and holds bright ones region by region, keeping their
texture (a bilateral grid in post, before bloom and the tonemap; ADR-212, local exposure). Off by default. DO27's
UE look asks for `local_exposure_shadows 0.75`; the Regia Engine spelling is

```ron
look: (
    // ...
    local_exposure: Some((shadow_contrast: 0.75)),
),
```

`shadow_contrast` 0.75 lifts a region 4 EV under middle grey by 1 EV (the hull's shadow side beside sunlit snow);
`highlight_contrast` below 1 pulls bright regions down; `detail_strength` above 1 sharpens texture. All three
default to 1 (no change). A host that builds `PostSettings` itself sets `PostSettings::local_exposure` or calls
`apply_level_look`. Scene files are v9: v8 levels load and migrate (`local_exposure: None`).

- **Host source change:** `PostSettings` and `LevelLook` gained `local_exposure`; a struct literal without
  `..Default::default()` adds `local_exposure: None`.
- **Check:** `level.set_look {"look": {"local_exposure": {"shadow_contrast": 0.75}}}`, then `seeing.screenshot`
  side by side with the UE plate. lookdev: `LOOK_R2_LOCAL_EXPOSURE=0.75`.

## Steam Deck and controller UI (batch v020)

Additive; nothing here changes a frame on desktop.

- **Gamepad UI navigation.** `UiPlugin` now turns the standard gamepad into `UiNavActions` (D-pad and
  left stick with repeat, A submit, B cancel, LB/RB previous/next). The calibration screen's own
  gamepad mapping still works; both write the same flags.
- **First navigation press.** With nothing focused, the first Up/Down/Left/Right/Tab press now only
  focuses the `DefaultFocus` node (before, it moved one step from it). A screen needs a `DefaultFocus`.
- **Tab, LB and RB order** is reading order (top to bottom, then left to right). It used to follow
  the query's archetype order, which was not stable.
- **Sliders.** Left/right on a focused slider changes its value (add `ManualValueNav` to a slider you step yourself, as the calibration screen does); submit no longer jumps the slider to
  the pointer.
- **`UiInputMode`** (`Pointer` or `Navigation`) follows the last input. Draw the focus ring and
  button prompts in `Navigation`.
- **`UiDevicePolicy`** (text floor, title-safe margin; defaults from `SteamDeck=1`) and
  **`ScaleMode::HANDHELD`**. A host that keeps its own small fonts is affected on the Deck only.
- **Frame cap.** On a Deck, `regia_steam` applies `DeckFrameCapSetting` to `FramePacing`.
  Fill `display_refresh_hz` from the monitor so a lower SteamOS limiter wins.
- **Sleep.** On Linux the runner detects system sleep and runs `Suspended` then `Resumed`; the
  `Lifecycle` schedule and `LifecycleEvent` messages fire on a desktop for the first time. Systems that
  react to `Resumed` (save games on `Suspended`, for one) now run after a sleep.

## Mesh identity and mesh arenas (batch v020)

Nothing to wire on the `MeshRenderer` path; additive API for hosts that drive `FrameRenderer` themselves.

- **Mesh keys are generation ids, not `Arc` addresses.** The runtime keys each `Arc<CpuMesh>` by an
  id it gives that `Arc` the first time it is drawn (`regia_render::mesh_key(&arc)`; clones share
  it). A new `Arc` never gets an old key, even at the same address with the same byte size, so a
  replaced mesh always uploads its own vertices. Before, a new `Arc` that landed on a dropped one's
  address with equal sizes drew the old mesh's buffers. Building a new `Arc` every frame still
  uploads every frame (the id is per `Arc`); keep the `Arc` when the content has not changed.
- **No mesh buffer per mesh.** Cached meshes are ranges in shared vertex and index arena buffers.
  When the last `Arc` of a mesh drops, the next frame frees its range, and the next mesh reuses it
  without creating a buffer. `frame_path_counters` shows `mesh vertex arena` / `mesh index arena`
  only when a chunk is added (instead of `mesh vb` / `mesh ib` per mesh). Meshes larger than half a
  chunk (32 MiB on desktop, 8 MiB on Android) keep their own buffers, and so does a mesh drawn with a
  material graph that reads the second UV set (`mesh vb (uv1)` beside `mesh uv1`).
- **A host that renders `ExtractedFrame` on its own renderer** calls
  `FrameRenderer::release_meshes(&extracted.released_mesh_keys)` every frame, next to
  `set_gi_proxy_disabled`. Without it, dropped meshes are freed by the idle eviction instead (later,
  not wrong). A host that builds `FrameMesh` from `Arc<CpuMesh>` itself can use `mesh_key(&arc)` and
  `take_released_mesh_keys()` the same way; any other stable key keeps working as before.
- **Watching it.** `FrameRenderer::mesh_arena_stats()` returns `(vertices, indices)` chunk count,
  bytes, free bytes and free ranges. `cache_stats().meshes_released` counts entries dropped by
  `release_meshes`.
- **API change.** `MeshSlice` gained `vertex_count` (a struct literal must set it). `MeshArena`
  had no users.

Check: in a match where units come and go, `upload_split().mesh_buffers` stays flat after warm-up
and `mesh_arena_stats().0.chunks` stops growing.

## SpaceMouse (batch v020)

Nothing to wire for a game (spec 29). `InputPlugin` installs `SpaceMousePlugin` on desktop; with no
device it only rescans for one every second on a background thread, and gameplay never reads it.

- **No new behaviour in the game.** The merged state (`SpaceMouseState`) exists but nothing consumes
  it unless the shell opts in.
- **Turn it off** with `InputConfig.spacemouse.enabled = false` (the `spacemouse` section of the
  input RON): no device is opened.
- **New public field.** `InputConfig` gained `spacemouse: SpaceMouseConfig` (serde default, omitted
  from the RON while default). A shell that builds `InputConfig` by struct literal must add it.
- **Viewport tools** (editor, Tincal, Red-Rouge) call `regia_input::spacemouse::nav::step` and
  apply the `NavDelta`; the editor viewport is the reference. Guide: [SpaceMouse](spacemouse.md).
- **Linux:** when the `spacenavd` daemon runs, Regia Engine reads its socket (`/run/spnav.sock`) and does
  not open the HID node; no udev rule is needed then. A shell that links its own spacenavd client
  should switch Regia Engine's off (`enabled = false`) or both will see the motion.

## Render overrides and surface picks (batch v020, Tincal F5/F7)

Additive; a frame with no `RenderOverride` component is unchanged (goldens pass untouched).

- **`RenderOverride`** (tint, emissive, ghost alpha, x-ray, unlit) rides the two unused lanes of the
  instance's object block (`skin.zw` in `lab_forward.wgsl`), so `ExtractedMesh`, `FrameMesh` and
  `FrameScene` keep their fields and no struct literal changes. `RenderPlugin` fills
  `ExtractedRenderOverrides` after `extract_frame`; `render_windows` and `render_texture_views`
  apply it.
- **Host source change (only if you render an `ExtractedFrame` yourself):** call
  `renderer.set_render_overrides(overrides.keys.iter().copied())` every frame next to
  `set_gi_proxy_disabled`, or overrides do not show. DO27 does not use overrides today: nothing to do.
- **Custom shaders:** a WGSL file that declares the forward `VertexOutput` by constructor (as
  `vis_resolve.wgsl` does) adds the new `overrides: vec2<u32>` argument after `lod_dither`.
- **`editor.pick_hit`** is editor-only (no runtime cost).
- **Check:** `editor.set_render_override {"entity": <bits>, "tint": [1, 0.2, 0.2, 1]}`, then
  `editor.capture_viewport`; clear it and capture again: the frames match.

## Reference image in a viewport pane (batch v020)

Editor-side (Tincal); nothing for DO27's shell.

- `regia_editor::render::PaneRequest` gained `reference: Option<ReferenceImage>`. A hosting app
  that builds `PaneRequest` as a struct literal must add `reference: None` (or a
  `ReferenceImage { texture: Arc<egui::ColorImage>, opacity, mode, rect_uv }`).
- The live panel and the agent's captures read the `ViewportReferences` world resource (pane
  index to reference); `editor.set_reference` writes it.

## Editor theme tokens (`regia_editor::theme`)

Additive; no host struct changed and nothing changes unless a host sets tokens.

- **Nothing to do** for a host that never touches the theme: `ThemeTokens::regia_default()` is today's look, and the
  old `pub const` overlay colors are unchanged.
- To re-skin the editor: insert a `ThemeTokens` (`ThemeTokens::from_json`) as a world resource, or call
  `theme::apply(&ctx, &tokens)` on your own egui context. Read colors with `theme::tokens(world)` or
  `theme::ui_tokens(ctx)`.
- `overlays::paint_grid` and `viewport::measure_viewport` keep their signatures; use `paint_grid_with` and
  `measure_viewport_with` when the overlay colors are themed. A host that hard-codes `OVERLAY_COLORS` to find
  overlay pixels must use `OverlayTokens::colors()` instead after retheming.
- Fonts: set `fonts.*.file` to a font on disk; none ships with Regia Engine.
- Agent: `editor.get_theme`, `editor.set_theme` (guide: `agents/theme.md`).

## Independent viewports (batch v020, Tincal F2)

Additive; a world that never draws a `ViewportInstance` renders exactly as before.

- **New:** `ViewportInstance`, `CameraLinkGroup`, `viewport_panel_with`, `render_instance_pane`,
  `capture_instance_pane`, `capture_instance`, the `ViewportInstances` registry and five `editor.viewport_*`
  agent methods. `viewport_panel` keeps its signature and behavior (it now draws through the same body).
- **Changed types (additive):** `EditorCamera` derives `PartialEq`; `ViewportPanelState` gains `toolbar: bool`
  (built with `Default`, never literally). No struct literal changes. `ExtractedMesh`, `PaneRequest` and the
  render types are untouched.
- **Host source change:** none. DO27's runtime does not use the editor viewport.
- **Behavior note:** once a world has instances, TAAU is off while more than one view draws through a shared
  `RuntimeRenderer` in a frame (no shared temporal history). Editor-only.
- **Check:** `editor.viewport_instance_set {"id": 1, "create": true, "yaw": 0.6}`,
  `editor.viewport_instance_set {"id": 2, "create": true, "yaw": 2.4}`, then `editor.capture_viewport_instance`
  for each: two different eyes, and `editor.capture_viewport` unchanged.

## Timeline widget and `sample_into` (batch v020)

Additive; nothing changes unless you call them. No host struct changed, so no host edit is needed.

- `regia_animation::sample_into(world, rig, &clip, time, SampleMode)` poses a rig without the animation schedule. It writes
  only the joints the clip animates (not every joint like the schedule) and clamps `time` (wrap it with `AnimationClip::advance`
  to loop). Do not call it on a rig an `Animator` also drives in the same frame: the schedule's apply system overwrites it.
- `regia_editor::anim::timeline::Timeline` is for editor-side tools (desktop only, like the editor).

## Scalar overlay (batch v020, Tincal F10)

Additive; a frame with no `ScalarOverlay` component is unchanged (no pass, no allocation).

- **`ScalarOverlay`** is a component; `ExtractedMesh`, `FrameMesh` and `FrameScene` keep their fields and
  no struct literal changes. `RenderPlugin` fills `ExtractedScalarOverlays` after `extract_frame`;
  `render_windows` and `render_texture_views` hand it to the renderer. The overlay is its own pass
  (`scalar_overlay.wgsl`), recorded in `render_into` right before the gizmos, so `lab_forward.wgsl` and
  the goldens are untouched. Texture-view and reflection-probe renders do not draw it.
- **Host source change (only if you render an `ExtractedFrame` yourself):** call
  `renderer.set_scalar_overlays(overlays.entries.iter().cloned())` every frame next to
  `set_gi_proxy_disabled`, or overlays do not show. `render_extracted_into` and the offscreen helper take
  the renderer you prepared, so set them on it first. DO27 does not use overlays today: nothing to do.
- **Check:** `editor.set_scalar_overlay {"entity": <bits>, "axis": "y", "range": [-0.5, 0.5]}`, then
  `editor.capture_viewport`; clear it and capture again: the frames match.

## Mask recolor and decal slots (batch v020)

Faction colors, chest numbers, callsigns, faction marks and visor tint per pilot, from instance
data instead of baked per-faction textures ([Mask recolor and decal slots](mask-decals.md)).

- **Insert `MaskDecals` on each mesh entity** that should change look: one shared
  `Arc<MaskDecalTextures>` per asset (mask with `srgb = false`, atlas with `srgb = true`, the atlas
  grid), a `MaskPalette` per faction, and the decal slots for that part's UV layout. It is not
  inherited from a parent entity.
- **A host that renders `ExtractedFrame` on its own renderer** calls
  `renderer.set_mask_decals(&extracted.mask_decals)` every frame next to `set_glass`. A host that
  builds `FrameMesh` lists itself builds `MaskDecalInstance { object_key, decals }` with the draw's
  `object_key`.
- **API change.** `ExtractedFrame` gained `mask_decals` (it derives `Default`; a struct literal
  must add the field or use `..Default::default()`). `ExtractedMesh`, `LabMaterial` and `FrameMesh`
  are unchanged.
- **Which materials.** Standard opaque, alpha-tested and alpha-blended. Glass (transmissive),
  material graphs, premultiplied and additive materials draw without it; the agent's
  `render.mask_decals` counts them as `unsupported_draws`.
- **Migrating the baked textures.** Replace the per-faction albedo set with one albedo whose faction
  areas are light neutral grey, plus a mask (R primary, G secondary, B trim, A emissive). Check the
  four looks side by side with `render.set_mask_decals` and `seeing.screenshot`.

Check: `render.mask_decals` shows `fallback_draws` 0 after the first frames of a match and
`blocks_created` equal to the number of masked parts that have drawn (it does not grow per frame).

## Model map and node report (batch v020, Tincal F14)

- **New, additive, editor-only:** `regia_editor::model_report::{ModelReport, ModelClass, BudgetLimits, ...}`
  and the agent method `editor.model_report`. Nothing in a runtime crate changed and no existing struct
  gained a field, so no host struct literal breaks.
- **Host source change:** none. DO27's shell does not link `regia_editor`.
- **Check:** `editor.model_report {"path": "models/tank.glb", "class": "vehicle", "text": true}`; the reply's
  `report.flags` is empty when the model fits its class budget.

## Batch thumbnails (batch v020, Tincal F8)

Editor-only and additive (`regia_editor::thumbnails`); nothing in the runtime frame path changed
and no public struct gained a field. The renderer owns a `DeviceFrameRenderer`, so a game that
embeds it renders thumbnails beside its own `FrameRenderer` without sharing state. No host change.

## Spring bones (batch v020, C3)

Additive and opt-in; a rig without `SpringBones` behaves exactly as before.

- **Add `SpringBones::new(setup)` to a rig** that has a `SkinnedMesh` and an `Animator`, with a
  `SpringBoneSetup` (from a `.spring.ron`, see `docs/formats/spring-ron.md`). The solver runs inside
  `apply_animation` (after the ragdoll blend, before the joint `Transform`s and `SkinPalette`) and writes joint
  rotations only. The rig entity needs a `Transform` (its world transform); without one the identity is used.
- **Optional LOD.** Insert `AnimLodCamera` (already used by `UpdateMode::Lod`) and set `lod` in the setup to drop the
  step rate or freeze the chains at a distance. Call `SpringBones::reset()` (or `anim.spring_reset`) after teleporting a
  rig.
- **Units are not VRM's.** `stiffness` is a per-step blend (0..1) and `gravity_power` is m/s². A VRM importer must convert.
- No host struct changed; `ExtractedMesh` and friends are untouched.
## Character shading models (batch v020, Tincal B3/B4/B6)

Additive; a frame without a `MaterialShading` component is unchanged (goldens pass untouched) and
no pipeline is built for shaded materials ([Shading models](shading-models.md)).

- **Nothing to change on the `MeshRenderer` path.** Insert `MaterialShading::skin(..)`,
  `::eye(..)` or `::sheen(..)` on a mesh entity to use it. The model belongs to the entity's
  material `Arc`: parts that share one material share its model (the first entity wins; the
  agent's `material.shading {}` reports `conflicts`).
- **A host that renders `ExtractedFrame` on its own renderer** calls
  `renderer.set_material_shading(shading.entries())` every frame next to
  `set_render_overrides`, with `shading` the `ExtractedMaterialShading` resource
  (`RenderPlugin` fills it after `extract_frame`). A host that builds `FrameMesh` lists itself
  passes `(material_key, MaterialShading)` pairs for its own material keys. DO27 uses none of the
  models today: nothing to do.
- **No struct literal changes.** `LabMaterial`, `ExtractedMesh`, `ExtractedFrame` and `FrameMesh`
  keep their fields.
- **Custom shaders** that declare the material uniform: it grows by five vec4s after `transmit`
  (`shading`, `shading_a`, `shading_b`, `shading_c`, `sheen`; 272 bytes). A graph's parameter
  block now starts at vec4 17 (`GRAPH_STANDARD_VEC4S`; 22 once the hair block below is added); the engine's shadow and velocity shaders
  are updated.
- **Pilots.** DO27's pilots are armored: the sheen suits undersuit fabric, and skin only shows on
  an exposed face. The skin model needs real-scale geometry (curvature in millimetres; set
  `mm_per_unit` for non-metre units) and softens the terminator only where the surface curves
  tightly.

Check: `material.set_shading {"entity": <bits>, "sheen": {"color": [0.6, 0.55, 0.5]}}`, then
`seeing.screenshot`; remove it and capture again: the frames match.
## Character rig systems (batch v020, Tincal C4/C5/C7)

Additive; a rig without a `RigDrivers` component animates exactly as before.

- **Nothing changes in `ExtractedMesh` or any host struct.** `RigDrivers` is a new component; `apply_animation` takes one more
  optional query item. DO27 does not use it: nothing to do.
- **Hosts that attach drivers:** compile the `.drivers.ron` against the skeleton and the host's morph names (`DriverSet::compile`,
  names via the `MorphNames` trait) and insert `RigDrivers`. Until the morph-target lane gives `MorphWeights` names, the host passes
  its own list in bank order.
- **New content formats** `drivers` v1 and `skelprofile` v1 (`cargo xtask check-formats` knows them). No migrators needed yet.
- **Check:** `anim.rig_attach { dry_run: true }` on the rig; `anim.profile_check { profile: "generic-humanoid" }` on the skeleton.
## Morph targets, `SkinnedMesh` and morph weights (batch v020)

Faces, visemes and correctives drawn on the GPU per instance, a skinning bridge in the engine, and
morph weights from clips, expression layers and the agent
([Morph targets and skinned meshes](morph-targets.md)).

- **`SkeletonBinding` is now `SkinnedMesh`.** Same fields; `SkeletonBinding` is a deprecated alias of
  the same type, so literals, queries and `insert` calls compile, with a deprecation warning. Under
  `-D warnings`, rename them (`SkeletonBinding {` → `SkinnedMesh {`, `Query<&SkeletonBinding>` →
  `Query<&SkinnedMesh>`) or allow `deprecated` where they are. `spawn_skeleton` returns `SkinnedMesh`.
- **Skinning bridge (opt-in).** `regia_framework::skinning::SkinnedMeshPlugin` fills `SkinJoints`
  from the animator's `SkinPalette` (or, without an `Animator`, from the joint entities' world
  transforms). A shell with its own `SkinPalette` → `SkinJoints` copy can drop it for the plugin. A
  shell that fills `SkinJoints` from its own poses (the sim) must **not** add the plugin to those
  entities' app, or the plugin overwrites them.
- **`MorphWeights` moved** to `regia_scene::components` (with the new `MorphTargetNames`).
  `regia_animation::MorphWeights` is a re-export of the same type: nothing to change. It now
  derives `PartialEq`.
- **API change.** `ExtractedFrame` gained `morphs` (it derives `Default`; a struct literal must add the
  field or use `..Default::default()`). `ExtractedMesh`, `FrameMesh`, `CpuMesh`, `MeshRenderer` and
  `LabMaterial` are unchanged.
- **A host that renders `ExtractedFrame` on its own renderer** calls
  `renderer.set_morph_instances(&extracted.morphs)` every frame next to `set_glass`. A host that
  builds `FrameMesh` lists itself builds `MorphInstance { object_key, targets, weights, mode }` with
  the draw's `object_key`, and must not use mesh keys whose top byte is `0x6D`
  (`MORPH_MESH_KEY_TAG`, the morphed copies' keys).
- **Drawing cooked morph targets.** `MorphTargets::from_cooked(&render_mesh, &cooked)` once per mesh,
  then `MeshMorphTargets::new(targets)` + `MorphWeights` on each entity. Without `MorphWeights` the
  mesh draws undeformed (no cost).
- **Warm.** A level warm whose frame has the morphed instances builds the morph pass; otherwise the
  first frame that needs it draws the mesh undeformed while it compiles in the background.

Check: `anim.morph_weights` (no entity) shows `waiting_pipeline` 0 after the first frames,
`dispatched` 0 while weights hold still, and `buffers_created` flat during a match.

## Telemetry and knobs (v0.1.14)

Additive: new summary fields, one renamed meaning (`hitch_abs_ms`), one render knob. No host
struct gained a field, so no struct literal in the shell needs a change.

- **Hitches.** A frame is a hitch only when it stands out from the rolling median: over 2 × the
  median, or more than `hitch_abs_ms` (50 ms) above it. A steady 66 ms no longer logs a hitch
  every 2 s; a 117 ms spike in it still does. If the shell sets `TelemetryOptions::hitch_abs_ms`,
  it is now an excess over the median, not an absolute bound.
- **CPU-bound or GPU-bound.** Read `frame_bound.class` from each `regia::frame_summary`:
  `gpu_bound` (mean GPU time at least 90 % of the mean frame interval: a lower render scale
  helps), `cpu_bound` (75 % or less: the GPU waits, a lower render scale does not help),
  `balanced`, `unknown` (no GPU timing). Thresholds: `regia_telemetry::GPU_BOUND_SHARE`,
  `CPU_BOUND_SHARE`. Per frame: `Telemetry::live().bound` (a `FrameBound`). The old `bound`
  field (renderer CPU against GPU) is unchanged; `cpu_ms` everywhere covers `render_into` only.
- **Dynamic resolution.** `render_scale_permille` (`last`, `mean`, `min`, `max`) is the scale
  the renderer drew at; `Telemetry::live().render_scale_permille` is the newest frame's. The
  engine does not persist it.
- **Waits on the GPU.** `wait_acquire_ms`, `wait_present_ms`, `wait_readback_ms` and
  `cpu_wait_on_gpu_ms` (`{mean, max}`) split the frame's CPU wait on the GPU. With
  `frame_breakdown_ms.outside_update` they tell a compositor gate from GPU backpressure (the
  pipelined-rendering plan's step S0). Measurement only: the runner is unchanged.
- **Hitch records** add `frame_ms_wall`, `update_ms_wall`, `bound`, `render_scale_permille` and
  `wait_ms {acquire, present, readback}`.
- **Perf overlay (Detailed).** The GPU memory report no longer runs on the frame thread; the
  GPU memory and RSS lines appear a refresh after the page opens.
- **SSR roughness cutoff.** `regia::render::set_sssr_roughness_cutoff(0.7)` (default 0.55,
  clamped to 0.15..1) keeps traced reflections on rougher surfaces; set it at start if you
  offer it as a setting (it is not persisted). `REGIA_SSSR_ROUGHNESS_CUTOFF=0.7` overrides it
  for a test run.

Check: in a steady scene the summaries show `hitches: 0`; `frame_bound.class` matches what the
overlay's "CPU-bound"/"GPU-bound" says when one side fills the frame; `telemetry.live` over the
agent protocol shows the same numbers.

## `regia_editor` sphere-pick rename (batch v020)

- `regia_editor::viewport::PickHit` (the old sphere-pick result of `pick_entities`) is now
  `SpherePickHit`, so it no longer shares a name with the surface hit
  `regia_editor::picking::PickHit`. The old path stays as a `#[deprecated]` type alias for one
  release: existing code compiles with a warning. **Host change (optional, editor hosts only):**
  rename `viewport::PickHit` to `viewport::SpherePickHit`. DO27's runtime does not use it.

## Eye gaze and per-instance bind poses (batch v020, Tincal C6/C9)

Additive; a rig with neither `Gaze` nor `InstanceBindPose` is unchanged.

- **No host source change.** No struct literal changes: `ExtractedMesh`, `FrameMesh`, `SkinPalette` (render), `Animator`
  and `SkinnedMesh` keep their fields. DO27 does not use either component today: nothing to do.
- `Gaze` and `InstanceBindPose` are ordinary components on the rig entity; `apply_animation` solves the gaze after the
  ragdoll blend, rig drivers and spring bones, then retargets onto the instance bind pose before the joint writes.
- To drive morph targets from the eyes, fill `Gaze::morphs` (`LidMorphMap::from_arkit_names(&morph_names)`); the
  weights arrive in the rig's `MorphWeights` like animation morphs.
- An `InstanceBindPose` must have the rig's joint count, or it is ignored. A baked body skinned in its own frame passes
  its inverse binds with `with_inverse_binds`. Root motion is not scaled: a body with longer legs and the same
  character speed shows a little foot slide that `foot_ik` corrects.
- **Check:** `anim.gaze_set {"entity": <bits>, "target": [0, 1.7, -2]}` then `anim.gaze_get`;
  `anim.bind_pose_set {"entity": <bits>, "scale": 1.15}` then `anim.bind_pose_get` and a screenshot.

## Hair cards (batch v020, Tincal B5)

Additive; a material without `hair` shades as before and goldens pass untouched.

- **New public field:** `LabMaterial` (and so `CpuMaterial`) gained `hair: HairFactors`. A shell that
  builds `LabMaterial` by struct literal without `..LabMaterial::default()` must add
  `hair: HairFactors::OFF`. `FrameMesh`, `ExtractedMesh` and `FrameScene` are unchanged. DO27 uses no
  hair material today: the only change is that one line, if it builds a `LabMaterial` literally.
- **Uniform:** `GpuMaterial` grows from 272 to 352 bytes (`GRAPH_STANDARD_VEC4S` 17 → 22): the five
  hair `vec4`s (`hair_a`..`hair_e`) come after the shading model block (vec4s 17-21). A custom shader
  that declares the material block (the shadow and velocity hosts do, and are updated) must declare
  22 `vec4`s ahead of a graph's parameters; no WGSL from the host changes.
- **Alpha:** `HairAlpha::Hashed` needs the TSR hashed-alpha setting for stochastic coverage (without
  it the cutoff is hard); `Sorted` draws in the blend pass; `Coverage` draws as `Hashed` in this build.
- **Shadows:** hair casts through the translucency maps (every preset but Mobile Low), never as an
  opaque card outline. A foliage card marked `hair` loses its square shadow by design.
- **Check:** `render.hair_set {"entity": <bits>, "tip_alpha": 0.3}`, then `seeing.screenshot`; set
  `"enabled": false` and capture again: the frames match.

## Compute skinning, dual quaternions, eight influences (batch v020, Tincal B2)

Opt-in. With no `SkinningConfig` resource and no `SkinningOptions` component, frames do not
change. The warm-up builds one more compute pipeline and two more velocity pipelines.

- **`SkinningConfig { compute: true }`** puts every skinned mesh on the compute path. Each
  instance is skinned once per frame, and the shadow cascades, depth prepass, velocity and
  colour passes all draw that result. **`SkinningOptions`** does the same for one entity and its
  children, and adds `method: SkinningMethod::DualQuaternion` or `influences8`
  (`SkinInfluences8::from_cpu_skin`). `ExtractedMesh`, `FrameMesh`, `CpuMesh` and `LabVertex`
  keep their fields, so no struct literal changes.
- **Host source change (only if you render an `ExtractedFrame` yourself):** call
  `renderer.set_compute_skinning(skinning.compute_all)` and
  `renderer.set_skinning_options(skinning.keys.iter().cloned())` every frame, next to
  `set_render_overrides`. DO27 does not opt in today, so there is nothing to do. To try it on
  pilots, insert `SkinningConfig { compute: true }`.
- **Reserved keys:** mesh keys whose top byte is `0x5B` (`SKIN_TWIN_KEY_TAG`) are the renderer's
  own, like `0xFE` and `0xFF`.
- **Not verified:** GPU timings (the shadow and main pass savings per character) and Android.
  Measure with `render.skinning` plus the GPU pass timings before shipping it on.
- **Check:** `render.set_skinning {"compute_all": true}`, then `render.skinning`.
  `last_frame.instances` should count the pilots, and a capture should match the vertex path.

## Terrain holes file, picking, probes and path tracer (batch v020, ask 4)

Tunnels v2: the holes a map cuts are a file it ships, and picks, rays, probes and the path
tracer see them ([Tunnels](../agents/tunnels.md), `docs/formats/terrain.md` § 5).

- **Ship `<map>.holes.ron` next to the map** (`maps/switchbacks.level.ron` →
  `maps/switchbacks.holes.ron`, `regia_terrain::holes_ron_path`). It is a serialized `HolesMask`
  with an optional `version` (format v1): `ron::to_string(&mask)` output loads as is, or write it
  with `HolesMask::to_holes_ron`. At most 64 shapes in all (ellipses included); rasters at most
  2048 texels a side. A file from a newer engine is refused.
- **Load it** after installing the terrain:
  `let mask = regia_terrain::load_holes_ron(&holes_ron_path(map))?;` (`Ok(None)` when the map has
  none), then `draw.set_holes(mask.as_ref())` and `mask.cut_heights(...)` for the collider, as
  before. The draw's mask now also feeds the GPU irradiance probes and the path tracer.
- **Rays.** `regia_terrain::pick_terrain(world, origin, direction, max)` and
  `raycast_with_holes(map, holes, ...)` pass through a hole, like the physics heightfield (below
  the surface there is no rock: test the tunnel meshes yourself). `raycast_terrain` is
  unchanged (no hole test). The editor's picks call the hole-aware ones.
- **No struct literal changes.** `HolesMask`, `HoleShape`, `Terrain`, `ExtractedMesh` and
  `ReferenceHeightfield` keep their fields; `HolesError` is unchanged (file errors are the new
  `HolesFileError`). `SharedPassDraw` gains `gi_holes` with a default (`None`), so a host's own
  draw compiles unchanged.
- **Behavior changes to check.** `ReferenceHeightfield::heights` is NaN inside the holes (a tool
  that reads it skips non-finite samples). `PtHeightfield::height_at` is `None` in a cut cell. A
  `regia bake-gi` description's terrain takes `holes: Some("map.holes.ron")`. Probes in a mouth
  are no longer dead, so lighting in and near a mouth changes (a map without holes renders as before).

Check: `terrain.load_holes` with `path` set to the map, then `terrain.holes` shows the shapes and
`pending: false`; `terrain.raycast` straight down a mouth returns `hit: null`.

## Draw integrity and skinned bounds (batch v020, W2.2)

The v0.1.13.1 hotfix skipped a draw whose mesh, object or material entry was gone instead of
panicking a render task. Batch v020 (v0.1.19) makes the lifecycle explicit ([Draw integrity and skinned
bounds](../agents/draw-integrity.md)): every entry a frame's draws refer to stays until the frame's
last pass (removals during collection wait for the frame's end; the sweep only takes entries idle
before the frame), a missing object entry is rebuilt on demand, and the skip remains the last
resort, now counted.

- **Host source change:** none. Additive API only: `FrameRenderer::draw_integrity() ->
  DrawIntegrity` (a `#[non_exhaustive]` read-only struct), `FrameRenderer::set_skinned_bounds(bool)`
  and `FrameRenderer::skinned_bounds()`. No existing struct gained a field; `ExtractedMesh`,
  `FrameMesh`, `DrawStats` and `CacheStats` are unchanged.
- **Behavior change: skinned meshes are frustum-culled.** Characters and mechs used to draw every
  frame whatever the view. They are now culled from the color passes when a conservative bound from
  their joint palette is outside the view; they still cast shadows (skinned shadow casters are
  unchanged: every cascade, as before). The bound covers any pose the palette produces, so a mesh
  must never vanish while on screen; if one does, `render.set_skinned_bounds {"enabled": false}`
  (or `set_skinned_bounds(false)`) restores the old behavior while you report it. Meshes whose
  weights cannot be bounded (negative weights, a weight sum off by more than 1 %, joint indices past
  the palette) keep drawing every frame.
- **Telemetry.** Summaries gained `draws.skipped` (window total, 0 in a healthy run) and
  `draws.skinned_culled_mean`.

Check: after several minutes of a match with AI wingmen, `render.draw_integrity` shows
`healthy: true` (`skipped_draws_total` and `objects_rebuilt_total` 0), `skinned.unbounded` 0 for
your characters, and `skinned.culled` > 0 when wingmen are behind the camera.

## Review fixes (batch v020, v020q)

Fixes from the reviews of hair cards, compute skinning, eye gaze, terrain holes and the draw
lifecycle. No struct gained a field.

- **Possible source change: `HolesError::BadRotation(f32)`.** A shape whose `rotation_radians` is
  NaN or infinite is refused. An exhaustive `match` on `HolesError` needs the new arm.
- **Possible source change: `regia_terrain::terrain_holes` returns `Option<TerrainHolesRef>`**
  (it derefs to `HolesMask`; use `.as_deref()` where you passed `.as_ref()`). It prefers the mask
  the terrain draw applied over `Terrain::holes` once a draw applied one.
- **`<map>.holes.ron`.** A misspelled top-level key (`shape:` for `shapes:`) is now an error
  instead of an empty mask. A file whose version cannot be read is a parse error. A dotted map name
  keeps its dots: `maps/v1.2.level.ron` → `maps/v1.2.holes.ron` (it was `maps/v1.holes.ron`).
  `switchbacks.level.ron` → `switchbacks.holes.ron` as before.
- **Eye gaze order.** Gaze (and head follow) now runs before the rig drivers and spring bones, so
  twist and corrective drivers and head-rooted springs follow the gazed head. A rig with an
  `InstanceBindPose` aims its eyes from its own eye positions. `InstanceBindPose::uniform` and
  `from_bone_scales` refuse a factor of 0 or less (`BindPoseError::BadValue`).
- **Spring bones on fast rigs.** A chain on a rig moving faster than the chain's length per frame
  keeps its lag; only a jump away from the rig's motion (a teleport, a cut) reseeds it.
  `anim.spring_reset` still forces a reseed.
- **Hair.** `render.hair_set {"enabled": false}` restores the alpha mode the material had before
  hair was turned on through the agent. A hair material left `ALPHA_OPAQUE` (no `apply_alpha_mode`)
  no longer takes the visibility buffer or counts as an opaque GI proxy or emissive light.
- **Compute skinning.** `FrameRenderer::skinning_gpu_bytes()` reports what the twins hold; these
  bytes are outside the mesh cache budget. `trim_memory` (memory pressure) now also frees twins
  not drawn on the previous frame. Dual quaternion signs align to the heaviest influence: an
  eight-influence table with an unused slot 0 now skins correctly.
- **Morph targets.** An object drawn under two mesh keys keeps a deformed copy per key: both draws
  are deformed and nothing is uploaded again every frame.

Check: `render.skinning` shows `gpu_bytes`; `render.draw_integrity` stays `healthy: true` with a
morphed character drawn under two primitives; `terrain.load_holes` on your map loads the same
shape count as before.

## Own canopy glass (viewer-inside) (v0.1.17)

A visible change, on by default. Glass with `GlassView::viewer_inside()` (the pilot's own canopy)
now adds its own light with a cheap shader: the environment reflection with Fresnel, the sun
(specular and diffuse, one cascade-0 shadow compare), the planet-shine fill, crack shading and the
material's emission. Tint and absorption of what is seen through it are unchanged (the multiply
pass is the same). It skips the clustered local lights, the fitted emitters (emissive panels),
probe GI, fog and the terrain and large-mesh occlusion terms. What you see: cockpit lights,
nav lights and emissive panels no longer put highlights on the inside of the own canopy. All
other glass (windows, other ships' canopies, exterior panes) keeps the full shader.

Why: the cockpit view cost 8 to 13 ms of forward against about 2 ms on foot at the same render
scale, about 4 ms per internal megapixel. Each canopy layer ran the full light loop over the
cockpit's densest clusters. Real-GPU numbers come after the lead's A/B.

No host struct gained a field, so no struct literal in the shell changes. New API (all optional):

- `regia::render::set_canopy_glass(CanopyGlass { shading: CanopyShading::Full,
  ..CanopyGlass::default() })` restores the old look for every renderer (process-wide, not
  persisted). Build `CanopyGlass` with `..CanopyGlass::default()`: later versions may add fields.
- `FrameRenderer::set_canopy_glass(Some(..))` overrides one renderer.
- `CanopyGlass::nearest_layer_only` (off by default): on frames with screen-space refraction (the
  glass front layer), a canopy layer behind the nearest one adds no light of its own. Try it if the
  canopy is an inner and an outer shell. Its tint still applies.
- `REGIA_CANOPY_FULL=1` in the environment forces the full shader for a test run, over every
  setting.
- Agent: `render.canopy_glass`, `render.set_canopy_glass {"shading": "full"}`.
- Telemetry: `gpu_pass_ms.forward_glass` in `regia::frame_summary`, the forward pass's
  translucent section (every glass pane, the own canopy included, and the other blended draws).
  It is 0 on GPUs without in-pass timestamps. `GpuTimings` gained `forward_glass_ms`; a struct
  literal of `GpuTimings` (none expected in the shell) needs `..Default::default()`.

Check: in a cockpit, A/B `forward_glass` and `forward` with `render.set_canopy_glass` cheap and
full at a fixed render scale, and take a `seeing.screenshot` of each. The canopy should look the
same except for the highlights of cockpit lights and emissive panels on the glass. Pipelines: the
cheap variant is part of the glass sets the game-frame warm builds (`prewarm_glass_instances`), so
switching between cheap and full mid-play compiles nothing.

Since the canopy hardening (ADR-262), `viewer_inside` is checked against the camera. The cheap shader
draws the glass only while the camera is inside the glass mesh's bounding sphere or within
`regia_render::CANOPY_EYE_MARGIN_M` (1 m) of it. A chase, replay or texture-view camera farther out
gets the full shader (fog, every sun cascade) even if the shell leaves `GlassView::viewer_inside()` on.
No shell change is needed. Still, switch the canopy back to `GlassViewMode::Exterior` when the
camera leaves the cockpit: an inside instance never refracts, whichever shader draws it. If the
canopy glass is a separate windscreen mesh whose bounding sphere ends more than a meter short of
the pilot's eye, `render.canopy_glass` counts it under `last_frame.eye_outside` from the cockpit.
It then pays for the full shader: give the glass mesh bounds that hold the eye (one canopy mesh,
not a lone front pane).

## Audio zones: tunnels (batch v020)

`AudioReverbZone` has three new fields (`preset: ReverbPreset`, `distance_scale: f32`, `cavity: bool`) and
`ReverbZoneShape` a `Capsule { radius, half_length }` variant (axis = local Y). A struct literal without
`..Default::default()` needs the fields; an exhaustive `match` on the shape needs the arm. The aliases for the
asked names: `wet` = `send_db`, `decay` = `feedback`, `blend_distance` = `fade_distance`.

For a tunnel: one `AudioReverbZone::tube(ReverbZoneShape::Capsule { .. })` per section (rotate the entity so
the capsule's Y runs along the tunnel), one `AudioReverbZone::open(ReverbZoneShape::Global)` for the surface
(the blend between them is the mouth), and the mine galleries as `mine_gallery`. Two things only the shell can
supply:

- **Rock.** Install `AudioRockField` with the ground height at `(x, z)` and `None` inside a terrain hole:
  `terrain.map.sample_bilinear(x, z)` and `terrain.holes` (`HolesMask::contains`). Without it no rock occlusion
  happens (the zones still scale distance and reverberate).
- **Mouths.** `AudioZonePortals { portals: vec![ZonePortal { position (zone-local), radius }] }` on a cavity zone,
  from the map bake. Without it the engine is silent on the surface everywhere (rock) instead of audible near
  the mouths.

Check: `audio.probe_zones` with the listener on the surface above, at the mouth and inside the tube
([Audio zones](../agents/audio-zones.md)).

## Render-side raycast and map layers (batch v020, DO27 ask 12)

- **New crate `regia_pick`** (runtime). Add it to the shell's `Cargo.toml` to call `world_pick`; nothing else changes
  for a host that does not. It depends on `regia_render`, `regia_scene` and `regia_terrain`.
- **`MapLayer(u8)`** (`regia_scene::prelude::MapLayer`, re-exported by `regia_pick`) is a reflected component. The
  bake writes it per entity: terrain is layer 0, an entity without one counts as 0, and a mesh child under a labeled
  parent inherits the parent's layer. It is a new component, not a field of an existing struct: no struct literal
  changes.
- **Holes.** `world_pick` uses the mask of the linked terrain draw (`world.insert_resource(draw.holes_handle())`, as
  `terrain.set_holes` needs) and falls back to `Terrain::holes`. A host that cuts holes only in its own draw without
  linking the handle gets no pass-through.
- **No struct changes** to `ExtractedMesh`, `ExtractedFrame`, `MeshRenderer` or `CpuMesh`.
- **Editor.** `regia_editor::picking` re-exports the casts from `regia_pick` (same names); `pick_terrain` now passes
  through holes.
- **Check:** `world.pick { origin, direction, all_layers: true }` over the stacked floors lists one hit per layer.

## glTF extensions (batch v020)

Import reads `KHR_materials_clearcoat` and `KHR_materials_anisotropy` into the cooked material (the renderer
shades both), keeps sheen, specular and `KHR_texture_transform` in `GltfImportResult::extensions`, and keeps any
other extension as raw JSON with one warning per file ([glTF extensions](gltf-extensions.md)).

- **API change.** `GltfImportResult` gained `extensions` (an output type: nothing to change unless a host builds
  one). `import_gltf_path` and `import_gltf_glb_bytes` keep their signatures; the `_with_handlers` forms take a
  `GltfExtensionRegistry`. `ExtractedMesh`, `CpuMesh`, `LabMaterial` and the cooked formats are unchanged.
- **Re-cook.** A glTF with clearcoat or anisotropy cooks a material that now carries them; cooked materials from
  before this version stay valid.
- **Sheen is the host's call.** Attach `result.extensions.material(i)?.sheen?.fabric_sheen()` as
  `MaterialShading::sheen` on the entity. The renderer ignores `KHR_materials_specular` and texture transforms.

Check: `editor.model_extensions { path }` lists what a file uses; `unknown_names` empty means every extension is read.

## Steam: one shared client (batch v020, ask 13)

- **Do not init steamworks yourself; use `Steam::client()`.** Add `SteamPlugin` (`regia` feature
  `steam`, plus `regia_steam/steamworks` for the real backend) and take the shared handle with
  `res.client()` (`Option<steamworks::Client>`). Calling `steamworks::Client::init` next to the
  plugin double-initializes the SDK.
- **Match the `steamworks` version** of `regia_steam` (0.13.x; `docs/03` § 1) in the shell's
  `Cargo.toml`, so `Steam::client()` returns the type the shell names.
- **Do not pump callbacks.** The engine runs `run_callbacks` once per frame in `PreUpdate`. Register
  your own callbacks on the shared client; they fire from that pump.
- **Overlay:** read `SteamStatus::overlay_open` or the `OverlayOpened`/`OverlayClosed` messages to
  pause shell-owned input. Gameplay contexts the engine knows are suspended already.
- **Rich presence:** `Steam::set_presence` / `clear_presence`. **Shutdown:** the engine does it on
  exit; drop any client clones you hold before then.
- **API:** additive only. `SteamBackend` gained defaulted methods (`logged_on`, `shutdown`,
  `raw_client`); `Steam`, `SteamState` and the existing messages are unchanged. `SteamPlugin` now
  also registers the `steam.status` agent method (`regia_steam` depends on `regia_agent`).


## Radial menu (batch v020, DO27 ask 16)

New in `regia_ui`; nothing existing changed, no struct gained a field.

- **Add** `RadialMenu` (component), `spawn_radial_menu(commands, parent, menu)`, the `RadialEvent` message,
  `RadialSticks` (resource) and `RadialGeometry` (pure math). `UiPlugin` registers them; a host that builds its own
  `UiPlugin` system set must schedule `radial::radial_stick_input` and `radial::radial_menu_update` in `Update`
  after `ui_gamepad_navigation` and before `ui_focus_navigation`.
- **Open control.** Either name a `VirtualButtons` action (`with_open_action("WeaponWheel")`, which a
  `VirtualButton` node or the input layer writes) or call `set_held(bool)` / `toggle()` / `open()` from game code.
  Sticks come from `GamepadAxisChanged`; a host that reads them elsewhere writes `RadialSticks`.
- **Events.** `RadialEvent { target, kind }`: `Select { id, .. }` carries the sector id you gave it. While a wheel
  is open it consumes the nav actions and sets `UiPointer::consumed_pointer`; gameplay input should check that.
- **Look.** Sectors are rounded chips from the theme colours, not arc shapes (`RadialStyle` overrides). An arc
  shader is not needed; if DO27 wants true wedges later it is a renderer change.
- **Check:** in `ui_gallery` press `4`, hold the button, point with the mouse.

## Schedule order checks (batch v020, developer tooling)

Additive and optional. `regia_app::App::shuffle_schedules(seed)` and `App::ambiguity_report()` (the `debug_names`
feature, on by default) let a host test run its own app under a seeded system order and list unordered conflicting
system pairs; nothing changes unless called. `regia_app` gains the `debug_names` feature (forwards to
`regia_ecs/debug_names`); a host that disables default features of `regia_app` loses `shuffle_schedules` only.
`regia_test_utils::shuffle::{snapshot, shuffle_check}` compare a name-keyed state snapshot across seeds (see
`agents/schedule-shuffle.md`). No host struct, component or schedule changed.

## Dynamic resolution: key on GPU time (batch v020)

The engine now has a pure controller, `regia_render::DynResController`
([Dynamic resolution](dynamic-resolution.md)). It is not wired into the frame. DO27's shell can
adopt it or keep its own controller. Either way the decision keys on **GPU time against the wall
frame**, not on GPU time against the budget alone.

A CPU-bound scene must hold or raise the scale. The measured failure: `render_scale` fell from
1000 to 500 permille over a few minutes while fps stayed near 15, with GPU time about 31 ms
inside a 66 ms wall frame. That frame is CPU-bound (31/66 is under 60%). The budget (16.6 ms)
was already exceeded, and a controller that only watched the budget kept stepping down.

- **Clocks.** `gpu_ms` is `GpuTimings::frame_ms` (two frames late; `0` means "no sample" and is
  ignored). `frame_ms` is the present interval. `FrameCpuTimings::total_ms` is only `render_into`.
- **Down** only when the rolling median GPU time is over the budget's upper band and the current
  frame is GPU-bound (GPU time over 90% of the wall frame). Balanced and CPU-bound frames hold.
- **Up** when the median is under the lower band, including while CPU-bound, so a lowered scale
  recovers once the GPU work drops.
- **Do not persist the permille per step.** Apply it to `FrameQuality::render_scale_permille` for
  the frame you are drawing. Leave `RenderSettings` and `user://settings.ron` alone: a settings
  write fsyncs on the main thread. The player's saved scale stays the base they picked.
- **Thermal governor** (`ThermalGovernor` in `regia_framework`) is a separate policy. Do not
  confuse a heat step with a frame-time step. If both run, combine them in memory for the frame.
- **Telemetry** the controller already exposes: `permille()`, `bound()` (`"cpu"` / `"gpu"` /
  `"balanced"`) and `median_gpu_ms()`. Nothing in `regia_telemetry` changed for this.

Check: a CPU-bound soak (GPU time under 60% of the wall frame, GPU time over 16.6 ms) holds its
starting permille. A GPU-bound overload steps down by 50 and stops at 500.


## Exposure zones (math only)

Volumes that carry a target EV range, and the pure functions that blend them and step the eye
([Exposure zones](../agents/exposure-zones.md)). This section is data and math only. The renderer
lane will hook it into the frame. Until that hook lands, exposure stays on the post stack.

- **Component.** `ExposureZone` on an entity that already has a `GlobalTransform`. Shape is an
  oriented box (`half_extents` on local X, Y, Z) or a capsule along local Y (`radius`,
  `half_height` of the cylindrical segment). `ev_min` and `ev_max` are the range the zone wants
  (the eye chases the midpoint). `blend_distance` is meters. `adapt_up_speed` and
  `adapt_down_speed` are EV per second. `priority` is an `i32`. No serde derive, same as
  `InteriorVolume`. A 25 m blend sits *outside* the mouth: weight 0 at 25 m out, weight 1 on the
  face and everywhere inside.
- **No frame change.** `ExtractedFrame`, `PostSettings`, and the exposure post pass are unchanged.
  No host struct gained a field. Spawning a zone changes no pixel. Do not sample the zones into
  your own frame copy; the renderer lane will call `zone_weight`, `combine_zones` and `adapt_ev`.
- **Authoring a tube.** One zone is enough when the sun is the default range the combiner receives.
  Tunnel numbers in the acceptance drive: half extents 8 × 4 × 200 m, blend 25 m, tube EV 8.5,
  sun EV 14.5, 2.0 EV/s brightening, 1.5 EV/s darkening. Overlapping zones: the higher `priority`
  wins; equal priorities weight-blend.
- **Check.** There is no agent call and no frame stat yet. The unit test
  `scripted_tunnel_drive_has_no_flash_or_blackout` is the curve: 30 fps, 40 s, 20 m/s, 1200 log
  samples, every EV inside [8.5, 14.5], each frame within 0.5 EV of the previous one.

## Tunnel shading fixes: fitted interior volumes, lights at the camera (batch v020)

Two forward-shading defects the tunnel acceptance scene (`regia_terrain/tests/tunnel_scene`) found
on lavapipe. Both are shader-only: no host struct, component or setting changed.

- **Fitted `InteriorVolume`.** A volume whose faces coincide with the tunnel's walls, floor and roof
  used to shade those surfaces pixel by pixel as either inside (40 lux floor) or outside (sky IBL):
  one-pixel stripes along the bore that changed with every view, a flicker while driving. Surfaces
  on a face, or up to 2 cm outside it, now count as inside. If you padded your volumes to avoid
  the stripes, the padding still works; you can fit them to the inner faces instead.
- **Headlight or flashlight at the eye.** A point or spot light placed exactly at the camera
  printed black (NaN) pixels across its cone, on meshes and terrain, at any intensity. The Fresnel
  term is clamped now; no offset from the camera is needed.
- **Check.** `regia_render` tests `interior_faces` (no one-pixel stripes in a room fitted to its
  volume) and `light_at_eye` (no blackened pixel with a point or spot light at the eye, lab and
  Desktop High); `regia_terrain` test `tunnel_scene` (no flash, no black frame, no crack along
  the cut's lip on the 20 m/s drive).

## Emissive fits by content (v0.1.18)

Fixes the 60–598 ms combat frames inside `emissive_lights` > `emissive_probes` (ADR-225).

- **Nothing to change in the shell.** The runtime lends each frame's emissive meshes to the renderer
  (`FrameRenderer::hold_mesh_identities`). A shell path that builds `FrameScene`s itself may call it with its emissive
  `Arc<CpuMesh>`es; without it the cache checks each key's buffers every frame instead.
- **The same mesh again costs nothing.** A mesh landed under a new `Arc` with the same content (positions, indices,
  emissive material) reuses its fit, and on the same entity its probes. Keep the entity when swapping a mesh (a
  damage step), so the probes carry over and the strips stay lit while a changed mesh is fitted.
- **New meshes fit in the background.** After the first frame, a genuinely new emissive mesh is fitted and traced on
  a worker; its strips appear a few frames later. A mesh that replaced another on the same entity keeps the old
  strips meanwhile. `REGIA_EMISSIVE_SYNC=1` restores the old synchronous behavior for an A/B.

Check: shoot hulls for a minute. In `regia::frame_summary`, `emissive.frame_ms_max` stays around a millisecond and
`emissive.cache_hits` climbs while `emissive.fits_done` stays low (each distinct damage stage fits once). No hitch
logs inside `emissive_lights`. `render.emissive_lights` shows the same under `fit_cache`.

## Mesh LOD tiers and authored LOD thresholds (v0.1.20)

Photoreal close-ups on the top tiers ([Mesh LOD tiers](../agents/mesh-lod-tiers.md), ADR-260).

- **Only chained meshes change.** The tier scales act in `select_mesh_lods`, on entities that carry a
  `MeshLodChain` (build it with `MeshLodChain::from_cooked`). A hull spawned without one draws its
  full mesh on every tier, as today; attach the chain to get both the savings and the close-up hold.
- **Tier values.** Your HIGH (`DesktopHigh`) gets `mesh_lod_distance_scale` 2 and
  `mesh_lod_shadow_scale` 0.35; MAX (`DesktopUltra`, also your `ultra` alias) 6 and 0.15; LOW
  (`DesktopLow`) and MEDIUM (`Deck`) keep 1 and 0.5, which select exactly as before. ULTRA LOW is the
  engine's Very Low tier: select it by name with `VeryLow` (`render.set_profile {"preset": "VeryLow"}`,
  `render_profile = "VeryLow"` in `Regia.toml`, `--render-profile VeryLow`; v0.7, ADR-8844). It keeps
  Desktop Low's 1 and 0.5 (or lower the distance scale below 1 to drop detail earlier).
- **Overriding.** Set `RenderSettings.profile.mesh_lod_distance_scale` / `mesh_lod_shadow_scale` after
  choosing the preset (a settings menu slider, or a per-map cap). `RenderSettings::preset()` then
  reports `None`, as for any customized field. If you build `RenderProfile` with a struct literal, add
  the two fields or spread `..RenderProfile::from_preset(..)`.
- **Budget.** More hulls draw LOD 0 on HIGH and MAX. Check with the unit-budget bench's `ultra` and
  `close20` rows, or live: `render.mesh_lod {"radius": <hull radius>, "thresholds": [...]}` gives the
  switch distances, `perf_overlay.get` the cost.
- **Authored chains.** Deliver `Stem_LOD0..n` nodes with `"extras": {"lod_screen_size": x}` for
  per-asset values, and/or keep a `lod_classes.toml` next to `assets/` with your classes (`light`,
  `battle`, `heavy`, `walker`, `structure`, `interior`, `cockpit`) and `[[assign]]` globs; a root node
  `"extras": {"lod_class": "battle"}` beats the globs. Node extras beat the class, the class beats the
  importer default. `kit.json` is not read. Run `regia cook` and read its warnings for table problems.
- **Check.** `render.get_profile` shows the scales; `render.mesh_lod` the distances per preset; a fly-in
  on a hull at the printed distances shows the cross-fade at the right range.

## Open kit (v0.1.21, editor only)

- **Nothing to change in the shell.** The kit viewer is editor-only (`regia_editor::kit`,
  `regia_editor::kit_view`); no runtime crate, component or host struct changed.
- **If you match `regia_editor::shell::Panel`** exhaustively, add a `Panel::Kit` arm.
- **Kits for the kit-test build:** the editor reads the same folder layout as DO27's kit-test loader
  (`kit.json`, `<kit_id>.glb`, `cockpit_<faction>_<class>.glb`, `rig.json`, `sockets.json`,
  `SHA256SUMS`) and reports problems instead of refusing the kit, so a modder can fix them before the
  kit reaches the game. It does not check the kit-test contract (scale, budgets, names).
- **Check:** `editor.open_kit {"path": "<kit folder>"}`; `checksums.verdict` is `verified` and
  `warnings` is empty for a clean Tincal export; `cockpit.attached_to` is `socket_cockpit`.

## End of the process: GPU workers (v0.1.27)

Nothing to wire if the shell uses `regia::run_with` (ADR-287). Engine threads that may be inside the GPU driver (the
telemetry probe, shader-warm and material-graph compiles, the exit's renderer and device drops, the encode workers)
are now tracked. After the last frame and the clean exit, the engine waits for them before it returns from `main`, so
the driver's own exit handlers never run under one of them (the v0.1.23 to v0.1.25 exit SIGSEGV on NVIDIA under load).

- **Own `main` loop:** keep calling `regia_platform::finish_deliberate_exit(|| log.flush())` last, after the app and
  every renderer are dropped. It now also does the wait, with exit code 0 if it has to end the process. Better: call
  `regia_platform::finish_exit(code, || log.flush())` with the code you are about to return, so that code is kept.
- **What a quit looks like:** all workers end, so the call returns and `main` returns as before. If one is still
  running 30 s after the quit started (`REGIA_EXIT_WORKER_WAIT_MS` changes that; it only bounds a hung driver), the
  log says `regia exit: N GPU worker(s) still running after <ms> ms: <names>` and the process ends at once with your
  exit code, without exit handlers. The 8 s watchdog still fires first if it is armed. A wait of 100 ms or more logs
  `regia exit: waited <ms> ms for GPU worker(s) <names>`.
- **Your own GPU threads** (if the shell starts any that hold a wgpu device): start them with
  `regia_platform::GpuWorkerBuilder::new("name").spawn(|worker| ...)` so the exit waits for them too.
- **Don't keep a renderer alive past that call** (in a static or a leaked box): its workers would hold the end of the
  process for the full budget.

Check: quit 20 times from a loaded map (a build running beside it). Every quit ends with the expected code, the log has
no `GPU worker(s) still running` line, and no crash bundle is written.

## Baked GI from the level (v0.2.0)

- **Name the bake in the map.** Put `baked_gi: Some((volume: "maps/<map>.fgiv"))` in the level's
  `look` (scene format v10; older levels migrate with `None`). `DefaultPlugins` installs the volume
  when the level loads and removes it when the level unloads, so the shell's own `set_baked_gi` call
  can go. If you keep it, the level's volume replaces yours while a level names one.
- **If you build `LevelLook` with a struct literal,** add `baked_gi: None` or spread
  `..LevelLook::default()`. A host that does not use `DefaultPlugins` adds
  `regia_framework::baked_gi_level::BakedGiLevelPlugin`.
- **Ship the file.** `regia cook` takes `.fgiv` under `assets/` as an asset, and a build profile's
  closure follows the level's path to it.
- **Check:** `level.get_baked_gi` shows `source: "level"` and `error: null`; a missing or wrong-version
  file shows `error_code` and the frame keeps its other GI.

## Runtime UI: events, theme, input layers, `ui.*` (v0.1.27)

Everything here is in `regia_ui` (ADR-289 to ADR-293). A shell that draws its HUD with its own widgets and no `UiPlugin`
changes nothing. A shell on `regia_ui` makes these edits.

- **`UiEvent` struct literals.** `UiEvent` has new public fields `button`, `modifiers` and `click_count`. Replace
  `UiEvent { target, kind }` with `UiEvent::new(target, kind)`; `with_button` sets the button.
- **Exhaustive `match` on `UiEventKind`.** New variants `DoubleClick`, `ContextClick`, `MiddleClick`, `Scroll`, `LongPress`
  and `Repeat`. Add a wildcard arm (`_ => {}`). `LongPress` and `Repeat` only fire for nodes that carry `LongPressable` or
  `RepeatOnHold`.
- **`UiPointer` and `UiInputState` literals.** Both have new public fields; build them with `..Default::default()`.
- **`UiThemes` and `Themed` literals.** `UiThemes` has a new field `set: ThemeSet`; `Themed` has `generation`, `anim_from`
  and `anim_to`. Add `..Default::default()`.
- **The default look changes.** Classed widgets resolve from the active `.theme.ron` (default `regia-dark`); the design
  palette replaces the v0.1.22 palette, and a disabled widget draws at 0.45 opacity. To keep the old disabled look:

  ```rust,wrap
  # let ui_themes: &mut regia_ui::UiThemes = todo!();
  ui_themes.disabled_opacity = 1.0;
  ```

  To keep your own palette, ship a theme that extends the default and sets your tokens (see
  [UI themes](../agents/ui-theme.md)), then select it at startup:

  ```rust,wrap
  # let world: &mut regia::prelude::World = todo!();
  *world.resource_mut::<regia_ui::ActiveTheme>() = regia_ui::ActiveTheme::named("my-game");
  ```

  UI goldens that show classed widgets change; re-bless them with the reason in the commit message.
- **Modals block input.** An open `Modal` is now an input layer: nodes behind it get no hover, click or focus. A shell
  that relied on clicking through a modal must close it first.
- **`ui.*` agent methods.** `DefaultPlugins` inserts the bridge when the `agent` and `ui` features are on. A shell with its own
  app inserts it itself, otherwise `ui.*` answers `ui_not_installed`:

  ```rust,wrap
  # use regia::prelude::*;
  # let app: &mut App = todo!();
  app.insert_resource(regia::agent::UiAgentHost::new(regia::ui::agent_bridge::host_call));
  ```

- **Check:** `ui.query {"type": "Button"}` lists the shell's buttons; `ui.click {"name": "start"}` then `play.step` fires
  the button's `UiEvent::Click`; `ui.layers {}` shows `depth: 0` with no modal open. Pages: [Runtime UI](ui.md),
  [Runtime UI (agents)](../agents/ui.md), [UI input](../agents/ui-input.md), [UI themes](../agents/ui-theme.md).

## Morph weights per node (v0.1.27)

ADR-297. A glTF `weights` channel now keeps the node it targets, so two morphing meshes in one clip no longer share
weights.

- **Struct literals.** `MorphTrack`, `CompressedMorphTrack`, `GltfAnimationImport` and `FinishedEval` have new fields.
  Replace `MorphTrack { target, weights }` with `MorphTrack::new(target, weights)` (or `MorphTrack::for_node(node,
  target, weights)` to bind a node).
- **`sample_morphs`.** It now returns only unbound (old-style) tracks. Code that sampled morph weights from a clip with
  per-node tracks also calls `sample_node_morphs`.
- **Cooked clips are format v2** (magic `FCLP`). A pak built earlier still loads (v1 clips load unbound and play on
  every morph mesh as before). To rewrite the dev cache: `regia upgrade-project --write`.
- **Not done by the hierarchy spawn:** a model with no skin that has morph clips needs a `NodeMorphPlayer` on the spawned
  root; a skinned rig's `Animator` drives the tracks.
- **Check:** `anim.morph_weights {"entity": <mesh>}` shows `source_node` and `node_driver`; `anim.get_state` lists
  `morph_tracks` with a `node` each. Page: [Morph weights](../agents/morph-weights.md).

## Procedural loops (v0.1.27)

ADR-299, ADR-300. Optional: nothing to change unless the shell wants a loop made from a file instead of a recording. A
`.loopgen.ron` (`docs/formats/loopgen-ron.md`) is a small graph of noise, oscillators, filters and gain that renders the same
samples on every platform.

```rust,ignore
// a patch layer whose `sound` ends in `.loopgen.ron`
let patch = regia_audio::AudioPatch::from_def_with_loopgen(def, &loopgen_cache, read_text, resolve_sound)?;
// or register one by name
sound_assets.add_loopgen("wind", ron_text)?;      // or add_loopgen_file("wind", path)
```

- **Check:** `audio.loopgen_render {"path": "audio/wind.loopgen.ron"}` answers `rms`, `peak` and `seam_jump` without playing;
  `audio.list_emitters` has a `loopgens` object. Loop generators do not hot-reload in v1. Page:
  [Audio (agents)](../agents/audio.md).

## Agent protocol hardening (v0.1.27)

ADR-340. Nothing to change for a shell that does not use the agent, or that only reads.

- **`render.out_start` output folders.** `out` must lie under the project root, the system temp dir, the working dir, or a
  directory listed in `.regia/agent.toml`. A DO27 capture script that writes elsewhere (a render farm mount, a second
  disk) adds it:

  ```toml
  output_roots = ["/srv/capture"]
  ```

  `..` and a symlink that leaves every root are `invalid_params`. Answers show paths relative to the root.
- **A broken `.regia/agent.toml` grants `read` only** (it used to fall back to the default profile). A missing file still
  gets the default profile. Check `agent.toml` parses before a capture run.
- **Errors carry no absolute paths** from `assets.*`, `terrain.load_holes` and `render.out_*`; read the log for the full text.
- **Limits:** 256 shots per sequence, 4096 keys per track, 4096 `cuts`, 16 connections, 32 MiB per WebSocket message,
  `replay.load` regular files up to 512 MiB, `seeing.compare_images` PNGs of 1 to 4096 px a side.
- **Check:** `render.out_start` to a folder under no root answers `invalid_params`; after adding it to `output_roots` it
  starts. Pages: [Safety](../agents/safety.md), [Render-out](../agents/render-out.md), [Protocol](../agents/protocol.md).

## Baked light groundwork (v0.1.27)

Baked light is not usable end to end in this release (no scene to `.bake.ron` exporter yet); do not plan a map around it.
The edits that keep a shell compiling:

- **`SCENE_FORMAT_VERSION` is 10.** Run `regia upgrade-project --write` to rewrite v9 levels.
- **Struct literals** of `LevelLook` (add `baked_gi: None`), `DetailBrick` (`flags: 0`), `BakedGiStatus`, `GiBakeDescription`,
  `LayoutDesc`, `GiBakeJob` and `BakeGiArgs` need the new fields or `..Default::default()`.
- **`.fgiv` v2** adds interior bricks; v1 files load as before, but an older build refuses a v2 file. `CookError` has a new
  variant `BakedGi(String)`.
- **`.bake.ron` v1:** v0 files load unchanged; `regia upgrade-project --write` stamps `format_version: 1` and keeps a `.bak`.
- **Check without a GPU:** `regia bake-gi <scene> --dry-run` validates and prints the layout. Pages:
  [Baking GI from meshes](../agents/baked-gi-bake.md), [Baked GI](../agents/baked-gi.md),
  [Baked GI from the level](../agents/baked-gi-maps.md).

## Windows checks (v0.1.27)

ADR-295, ADR-296. Host-visible only as a tool: `cargo xtask windows-pass` runs the Windows test subset, pipeline compile
times and start-and-quit runs on a Windows machine, and `--read <summary.json>` prints the verdict. Windows is not verified on
hardware since 2026-10-14. Page: [Windows](windows.md).
## Exposure zones drive exposure (v0.1.28, DO27 ask 5)

The zones from "Exposure zones (math only)" above now reach the frame (ADR-322). That section's
"No frame change" bullet no longer holds: a spawned zone changes the main view's exposure.

- **Nothing to wire with `DefaultPlugins`.** `ExposureZonesPlugin` weighs every `ExposureZone` at
  the active camera (the window camera with the lowest `order`) each frame and calls
  `RuntimeRenderer::set_exposure_target`. A host without `DefaultPlugins` adds
  `regia_framework::exposure_zones::ExposureZonesPlugin`, or calls
  `set_exposure_target` and `set_exposure_dt` on its `FrameRenderer` before `render_into`.
- **The tunnel.** One zone per bore, centered on the tube, local Z down the bore:

  ```rust,ignore
  use regia_render::exposure_zones::{ExposureZone, ExposureZoneShape};

  commands.spawn((
      Transform::from_translation(tube_center).with_rotation(tube_rotation),
      ExposureZone {
          shape: ExposureZoneShape::Box { half_extents: [8.0, 4.0, 200.0] },
          ev_min: 8.5,
          ev_max: 8.5,
          blend_distance: 25.0, // meters outside each portal
          priority: 1,
          ..ExposureZone::default() // 2.0 EV/s up, 1.5 EV/s down
      },
  ));
  ```

  Outside the band the frame meters as before. Across the 25 m mouth the allowed range narrows
  from the open band (EV100 −2 to 15) to 8.5. Inside, a dark bore that meters at EV 5 holds the eye
  at 8.5, and a lit window cannot pull it out. A range (`ev_min` 8, `ev_max` 9.5) lets the meter
  move inside it.
- **What changes and what does not.** Auto exposure inside a zone adapts with `adapt_ev` at the
  zone's speeds (from about EV 14.5 to 8.5 in 96 frames at 30 fps). Exposure compensation and the
  level's bias still apply on top. Manual exposure (auto exposure off, a camera `exposure_ev`)
  ignores zones. Texture views are not zoned. A map with no zones renders the same pixels as
  before. No host struct gained a field.
- **Check:** `render.get_exposure` in the tube shows `mode: "zone"`, `zone_count: 1`,
  `zone_target.target_ev: 8.5` and, once arrived, `settled: true`. For the scripted drive:
  `render.exposure_log {"action": "start"}`, drive through with `play.step`, then
  `render.exposure_log {"action": "get"}` and assert on `samples` (`time`, `ev`, `target`);
  `wait_until {"predicate": "exposure_settled"}` waits for the eye. See
  [Exposure zones](../agents/exposure-zones.md) for the assertions and the error reasons.

## Shadow LOD hint, collision nodes, eight skin influences (DO-6, ADR-1622)

- **Shadow LOD hint.** For an object whose shadow should not drop to a coarse LOD (a vehicle
  hull close to the camera), insert `regia_render::ShadowLodHint::cap(0)` (or `cap(1)`) next to
  its `MeshLodChain`. For a busy prop whose fine shadow is wasted, `ShadowLodHint::fixed(2)`.
  Without the component nothing changes. Check: `render.shadow_lod_hints`; set one live with
  `render.set_shadow_lod_hint { entity, max_lod }` ([Shadow LOD hint](../agents/shadow-lod-hint.md)).
- **Collision nodes.** `collision_*` glTF nodes are never drawn: the cook skipped them already,
  and now the editor's `load_gltf_model` and `spawn_gltf_hierarchy` do too (the hierarchy keeps
  the node entity with `GltfCollisionNode`). Nothing to do on the DO27 side; Tincal's kits keep
  naming hulls `collision_<body>_<n>`.
- **Eight skin influences** are opt-in per asset: add `settings: Some("(skin_influences: 8)")`
  to the character's `.meta`, export `JOINTS_1`/`WEIGHTS_1`, and on spawn insert
  `SkinningOptions::for_cooked_skin(&skin)` on the mesh entity when it returns `Some`. Every
  other asset stays at four and draws as before. Cooked meshes are v3; `regia upgrade-project
  --write` rewrites older cooked files, and paks load unchanged. Check: `assets.skin_check`
  answers `draw_influences: 8` for the opted-in mesh.

## Thin film (ADR-1977)

A glTF with `KHR_materials_iridescence` now cooks a thin film, and the forward pass draws it ([Thin film](../agents/thin-film.md)).

- **Re-cook only for a film.** `.material.ron` is v4 and the cooked material is v5. Paks with a v4 cooked material
  still load, as no film, so a glTF must be cooked again for its film to reach the material. Source `.material.ron`
  files load as they are; `regia upgrade-project` rewrites them.
- **API change.** `LabMaterial` gained `thin_film` and `thin_film_map`, `AuthoredSurfaceMaps` gained
  `thin_film_coverage` and `thin_film_thickness`, and `MaterialAssetRon` gained `thin_film`. A literal without
  `..LabMaterial::EMPTY` or `..Default::default()` needs them. `ForwardReason` gained `ThinFilm`.
- **No code needed for a material without a film:** its pipelines and material block do not change.

Check: `material.thin_film { entity }`: `active` true, and `renderer.drawn` above 0 once a frame has run.


## Presentation physics next to the sim (DO27 phase 2-3)

Debris, ragdolls and explosions on the engine's physics, with the sim's units as kinematic bodies
and the terrain collider built from the same map and holes the draw uses. The engine plugin is
`regia_framework::sim_bridge::presentation::PresentationPhysicsPlugin` (ADR-8356), together with
`PresentationGround::from_terrain` and `DebrisBurst`. A shell that still has its own copy of that
composition can call the plugin instead. The plugin is not on the lockstep closure and does not
feed the sim. See [Presentation physics](../systems/sim-bridge.md#presentation-physics). The
windowed sample remains `samples/sim_bridge` (`--presentation-physics`).

- **Check:** `cargo test -p regia_framework --test presentation_physics` (a chunk rests on a
  bridged hull and rides with it, a chunk over a hole falls through, the bridge never writes a
  non-sim `Transform`, and a dynamic body with a sim id is refused). In a running shell,
  `sim_bridge.presentation_stats` reports hulls, debris and refused writes;
  `physics.stats` shows the debris as awake dynamic bodies and `sim.counters` is unchanged by them.

## Frame caps for menus, loading and pause; dynamic resolution held outside play (v0.5.1, ADR-7889)

Testers saw the loading page and menus run uncapped at 300-430 fps, and the render scale climb to 1.0 on menu and
pause frames, then step down again in play ([Frame pacing](../agents/frame-pacing.md)).

- **Per-state caps.** Insert `regia_app::PacingCaps` once at start-up. Recommended values for DO27:
  `PacingCaps { menu: Some(60.0), loading: Some(60.0), paused: Some(30.0), play: None }` (play stays at the
  player's `display.frame_cap`). The engine default is `None` everywhere, so a host that sets nothing runs as before.
- **The state.** With `GameFlowPlugin` the flow sets `regia_app::PacingState` on every transition (MainMenu menu,
  Boot and Loading loading, Paused paused, Playing play). Without it, set `PacingState` on your own state changes.
- **Drop the workaround.** DO27 rewrote `FramePacing::target_fps` on its state changes; the settings apply then
  overwrote it. Stop doing that: the settings apply now writes only the player's cap and the runner paces at the
  tighter of the two, so the menu cap survives every settings change.
- **Dynamic resolution.** If your governor feeds `DynResController`, call `hold()` when you leave play and `resume()`
  when play starts again (or keep it in `regia_render::DynResGovernor` and call `feed(state, gpu_ms, frame_ms)`,
  which does both). Resume clears the rolling window, so the first play frame keeps the scale play settled at.
- **Frame generation.** The cap paces rendered frames. A 60 fps menu cap presents about 120 frames per second with
  frame generation on; a 30 fps pause cap is under frame generation's 55 fps floor (`PacingConfig::min_base_fps`),
  so the pause presents its 30 rendered frames only.

Check: `app.pacing` in the main menu answers `effective_fps` 60 and `measured_fps` about 60; `render.dynres`
answers `held: true` in the pause menu.

## Per-frame load budget (v0.5.1, ADR-7884)

What the scene warm does not cover (units spawned after it, a streaming LOD swap, a warm the watchdog abandoned) is
uploaded and compiled on the frame that first draws it. A tester's frame took 213 ms for 338 MB of uploads, and
another took 480 ms for 79 pipelines. The opt-in load budget bounds that frame
([Load budget](../agents/instant-load.md#load-budget)):

<!-- ignore-reason: statements in the host's start-up code; `world` is the host's World -->
```rust,ignore
use regia_render::{FrameLoadBudget, RuntimeRenderer};
// Once, after the renderer exists (it survives the scene warm's renderer swap):
world.resource_mut::<RuntimeRenderer>().0.set_load_budget(FrameLoadBudget::RECOMMENDED_60FPS);
```

- **Recommended for a 60 fps target:** `upload_bytes_per_frame: Some(64 << 20)` (64 MiB),
  `new_pipelines_per_frame: Some(8)`, `late: LateDraw::Defer` (= `FrameLoadBudget::RECOMMENDED_60FPS`).
  At 30 fps, 128 MiB is about the same share of a frame.
- **The trade:** a late object appears a few frames later (300 MiB at 64 MiB per frame is resident on the sixth
  frame, nearest first) instead of the frame stalling 200-500 ms. A material graph built past the pipeline cap
  appears when its worker build lands.
- **Off by default.** The default is `LateDraw::Block`, no caps. It changes nothing, and no preset sets a budget.
- **Still do the warm.** The budget bounds what the warm missed. `GameFrameWarm::request()` once the whole map has
  spawned still keeps the first frame free of uploads and compiles.
- **Not covered:** a new render target format or sample count still builds its forward set on the frame that needs
  it (without the set the frame draws none of its scene, so there is nothing to defer). Cover it with the warm:
  `SceneReadiness::prepare` behind the loading page, and `pipeline_warm::register_warm_target` for every other
  target you render into (ADR-8122). A frame with several views budgets each view.

Check: `render.load_budget` shows `budget.active: true`, and in `last_frame`, `deferred_draws` drops to 0 a few
frames after a spawn burst. `render.set_load_budget {recommended: true}` sets it from an agent.

## Host memory budget: retire or shrink the mesh spill (v0.5.1, ADR-7891)

| Item | What to do |
|---|---|
| Mesh copies | Keep map meshes in a `regia_render::host_memory::HostMeshStore` (`insert_pak` from the mounted pak) and draw them through `prepare` / `frame_mesh` / `after_frame`. Over the tier's `RenderProfile::host_memory.cpu_mesh_bytes`, the store drops the CPU copies of GPU-resident meshes (least recently drawn first) and re-reads them from the mapped pak when needed. With this on, the game's own mesh parking (`parked_mesh`, the spill file) can be retired or reduced: DO27 decides. |
| CPU consumers | `pin` every mesh a collider build, picking or CPU skinning reads; `unpin` when done. |
| What stays | Procedural meshes (`insert_owned`), pinned meshes, and every mesh on frames with the visibility buffer, DDGI probes, the traced sun, ReSTIR GI or lightmap receivers on (they read CPU vertices later; counted as over-budget ticks). Block-compressed texture payloads are counted (`set_cpu_texture_bytes`), not dropped yet: keep `parked_bcn` for now. |
| Watch | `render.memory_budget` `host` block, `render.set_host_budget` to try a cap live, telemetry `memory.host` every 10 s. |

## `regia_net`: multiplayer for DO27

DO27 builds its multiplayer on `regia_net` (owner decision 2026-11-15, ADR-7906; spec 25 § 5). What the engine
does and what stays the game's, with the page that has the full rules. The how-to is [Networking](networking.md).

- **Modes.** `NetMode` is `Offline` (the default), `Client`, `ListenServer` (hosts and has local players) or
  `DedicatedServer` (no local players, no rendering, no audio). A dedicated server is a `server` build of the game:
  `regia server` in development, `regia package --server --platform linux|windows` to ship, `server.toml` to
  configure. See [Net dedicated server (agents)](../agents/net-dedicated-server.md).
- **No ECS world needed.** `NetServer<G, S>` and `NetClient<G, S>` are generic over the game's `NetGame` (its
  message, per-tick input and join data types, each a `NetCodec`, plus `GAME_ID`, `PROTOCOL_VERSION` and
  `content_hash`) and over a datagram socket. The game steps them with its own millisecond clock (`receive`,
  `poll_event`, `advance_tick`, `flush`); the crate's own example runs with no world. DO27's simulation never enters
  an engine world for this. The crate still links the engine's ECS crates at build time.
- **Lockstep-style stepping: `TickDriver`.** `server.advance_tick(|tick, inputs| ..)` hands the game the inputs that
  arrived for that tick, in peer-id order, and runs the game's own step; a missing input is absent from the map, and
  the game's rules cover it. `server.driver().all_ready_for(tick)` is true once every registered player's input for
  `tick` is in, so a game that must not step without every input waits on it. Clients stamp inputs a few ticks ahead
  of `estimated_server_tick` and resend the last `ClientConfig::input_redundancy` (8) inputs in every packet.
- **Game-owned state: `SnapshotEncoder`.** For state that lives in DO27's sim rather than in ECS components, build a
  `WorldSnapshot` of `EntitySnapshot`s against one `SnapshotSchema`, set each entity's `priority` from the game's
  own rules (`EntitySnapshot::with_priority`; higher is packed first), and call `encode_with_report(tick, &world,
  ack, budget_bytes)`. What did not fit the byte budget is listed in `EncodeReport::omitted` and follows in a later
  packet; a lost packet costs nothing because deltas are against the last acknowledged snapshot. The client reads it
  with `SnapshotDecoder`. This layer does no relevance filtering and no interpolation: the game picks priorities
  and interpolates in its sim. See [Snapshot delta codec](../systems/snapshot-delta.md).
- **Handshake and encryption.** A join checks `GAME_ID`, `PROTOCOL_VERSION`, the protocol hash and the content
  manifest (`protocol_mismatch`, `version_mismatch`, `content_mismatch`). Traffic to every non-loopback peer is
  encrypted and authenticated with a Noise NX handshake by default (ADR-1901). A client that sets
  `ClientConfig::server_key` refuses any other key, which stops an attacker in the middle; without it the link stops
  a listener only. See [Net transport encryption](../agents/net-crypto.md).
- **Steam.** `regia_steam::SteamSocket` carries the same datagrams over Steam's relay network and
  `regia_steam::SteamLobbyPlugin` backs `Lobbies` with Steam lobbies, both on the engine's one Steam client: the
  shell never calls `Client::init` ([interface contract § 7](do27-interface-contract.md)).
  See [Net lobbies (agents)](../agents/net-lobbies.md).
- **The relay's limit.** `relay::RelayServer` (join codes, hole-punch introductions, forwarding) serves one relayed
  client per host address today. A host with more than one player behind NATs that do not punch needs Steam sockets,
  port mapping (`port_map::try_port_mapping`) or an open UDP port.
- **Host migration is the game's.** The engine does not move a session to a new host. Reconnect grace resumes a
  dropped client on the same host only ([Net session (agents)](../agents/net-session.md)). See
  [Networking](networking.md).
- **Hostile input and readiness.** What the server refuses from a peer, when it disconnects one, and how to ask
  whether a chosen set of players has input for a tick are on [Networking, Server](networking.md#server) and
  [Net model (agents)](../agents/net-model.md); this page does not restate them.
- **Check:** `net.status` and `net.peers` on the host, `session.state` on a client; offline,
  `cargo run -p regia_net --example net_duel` runs two players over the in-memory loopback with the `wan`
  conditioner profile. See [Networking (agents)](../agents/net.md).

## Hand-built pilots (ADR-4577, ADR-8028)

`unit_tasks.rs::seed_t_action` spawns a sim pilot because a hand-built one crashed the renderer on v0.1.13
("object entry", 10-16). The cause: one pilot mesh drawn rigid for the hand-built pilot and skinned for another pilot
in the same frame. The skinned visit cleared the mesh's cull bound after the rigid draw was collected, so the GPU
cull dropped that draw and the colour pass found no object entry for it ([Draw integrity and skinned
bounds](../agents/draw-integrity.md)). ADR-4577 (in v0.5.0) keeps the bound. ADR-8028 draws any draw the cull still
misses per object instead of skipping it.

- **Host source change:** none required. `DrawIntegrity` gained `cull_missed_draws` and `cull_missed_draws_total`
  (the struct is `#[non_exhaustive]`). `render.draw_integrity` returns them, and `healthy` needs the total to be 0.
- **Optional:** the hand-built pilot path can come back (`seed_t_action` with a hand-built body instead of
  `spawn_proof_steve`). A body with no skin palette draws rigid. If it shares its mesh with a skinned pilot, it now
  draws in either submission order.

Check: run the t-action scenario with a hand-built pilot beside a sim-spawned pilot for a minute. The pilot is drawn
and `render.draw_integrity` shows `healthy: true` (`skipped_draws_total`, `objects_rebuilt_total` and
`cull_missed_draws_total` 0).

## Runtime glTF cook without the optional importers (ADR-7972)

A player build that cooks glTF at run time does not need the `https://` cook-cache client, its TLS stack, or the FBX,
USD, SVG and Vorbis importers. They are `regia_asset_pipeline` features, all on by default (the editor, the CLI and a
host that names no features build exactly as before). For the player build:

```toml
regia_asset_pipeline = { version = "0.5", default-features = false }
```

| Feature | Brings | Without it |
|---|---|---|
| `http_cache` | ureq + rustls + ring: `https://` shared cook caches | opening an `https://` cache fails with `cook_cache_https_disabled`; directory, `file://` and `http://` caches work |
| `fbx_import` | the ufbx C library | `.fbx` import and cook: `importer_disabled` |
| `usd_import` | the `openusd` reader | `.usd`, `.usda`, `.usdc`, `.usdz`: `importer_disabled` |
| `svg_import` | resvg, usvg, tiny-skia | `.svg` and atlas sprites that name an `.svg`: `importer_disabled` |
| `audio_transcode` | libvorbis (`vorbis_rs`), kira's decoder | a sound whose plan is `vorbis` fails its cook with `audio_transcode_disabled`; `keep` (the default) cooks |

glTF, images, textures, meshes, OBJ, PLY, STL, PSD and the pak writer are always built. A disabled importer is never a
panic and never a silent skip: `regia_asset_pipeline::ImportError::ImporterDisabled { importer, feature }`
(`code()` = `importer_disabled`) names the feature that brings it back; `disabled_importer(ext)` answers the same
before a call, and `enabled_features()` lists what the build carries. The three error codes are lowercase protocol codes
(`importer_disabled`, `cook_cache_https_disabled`, `audio_transcode_disabled`); the editor's agent protocol always
builds the defaults, so they reach a host's own logs, not an agent method.

`cargo tree -p regia_asset_pipeline -e normal` on v0.5.1 (`--locked`, host target, unique packages in parentheses):

| Build | v0.5.0 | v0.5.1 |
|---|---|---|
| default | 1,487 lines (481) | 1,474 lines (472; `rcgen` is a dev-dependency now) |
| `--no-default-features` | 1,487 lines (481; no features existed) | 1,339 lines (423) |

The slim tree holds none of ureq, rustls, rcgen, ring, ufbx, openusd, resvg, usvg and vorbis_rs; `cargo xtask
check-layers` fails if one comes back. kira and tiny-skia stay in the tree through `regia_audio` and the window stack.
The smallest host that cooks a glTF project through `cook_project` (the cook's full importer dispatch), the example
`runtime_gltf_cook`, release profile, Linux x86_64, both builds on one host:

| `cargo build --release -p regia_asset_pipeline --example runtime_gltf_cook` | default | `--no-default-features` |
|---|---|---|
| stripped binary | 43.34 MB | 33.83 MB (−9.51 MB, −21.9 %) |
| `.text` | 32.15 MB | 25.48 MB (−20.7 %) |
| `libregia_asset_pipeline` `.rlib` | 80.3 MB | 76.3 MB |

A host that only calls `import_gltf` (never `cook_project`) already lost the other importers at link time: there the
two builds are the same size (6.50 MB stripped). The gain is for a host that runs the project cook, as DO27's player
does. Feature unification: if any crate in the player's
build graph depends on `regia_asset_pipeline` with default features, the defaults come back; check with
`cargo tree -p <player> -e features -i regia_asset_pipeline`.

## GPU mesh v3: skinned and morphing meshes GPU-ready (v0.7, ADR-10080)

- **Re-cook the desktop paks.** `regia cook --ship` (or `--gpu-meshes`) now stores skinned and morphing meshes
  GPU-ready (`gpu-mesh` layout v3: the packed skin with both influence sets, the eight-influence opt-in and the sparse
  morph targets after the indices). Paks cooked by v0.6 still load unchanged (their skinned and morphing meshes stay
  compact and keep the `compact_meshes` warning until re-cooked). A v0.6 runtime refuses a v3 entry
  (`GPU-ready mesh layout version 3 is not supported (this runtime reads 2)`): ship the v0.7 runtime with v0.7 paks.
- **No MikkTSpace at load.** `regia_render::CpuMesh::from_cooked` (and `from_cooked_rigid`, the counted uploads)
  draws a mesh's stored tangents when they are real, so a `CpuMesh` from `AssetServer::load_mesh` of a GPU-ready
  entry builds its render vertices without a tangent pass (bit-identical to the pass). Check:
  `regia_render::tangent_passes_on_this_thread()` does not move around your `from_cooked` call. A `CpuMesh` your
  code fills with its own valid tangents is now drawn with them.
- **API:** `GpuMeshView` has a new `extras` field (no host builds that struct by literal that we know of);
  `GpuMeshExtras`, `GpuMeshParts`, `encode_gpu_mesh_parts` and `encode_gpu_mesh_file_parts` write the block.
