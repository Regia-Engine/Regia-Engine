# Instant load (boot splash, pipeline warm-up, load sets, memory budget)

DO27's "feels instant" budget — branding < 1 s, menu interactive < 1 s, Ready → in control < 1 s with no
loading screen — is checked through four methods registered as read-only. Specs: `docs/specs/13-renderer.md` § 2b–2c,
`docs/specs/12-assets.md` § 5b; cooked content layout: `docs/formats/cooked-pack-guide.md`.

## Boot timeline

```json
{"jsonrpc":"2.0","id":1,"method":"render.boot_timeline","params":{}}
```

Returns `{ now_ms, marks: [{ mark, ms }] }`, milliseconds since the process started (Linux/Android read the
process start from `/proc`; elsewhere the clock starts at the first mark). Engine marks: `plugin_build`,
`resumed`, `device`, `surface`, `first_present`, `boot_tier_ready`, `menu_tier_ready`, `game_warm_start`,
`game_warm_done`, `first_game_frame`, `splash_done`, `splash_capped` (the 1.5 s cap ended the splash while a
game-frame warm still ran) and `boot_background_idle` (warm, deferred variants and quality probe all done).
Games add their own with `regia_render::boot::mark_named("menu_interactive")`. Check: `first_present` ≤ 500 ms
warm, `splash_done` ≤ 1500 ms on every launch (cold or warm, first launch included) when the host has called
`finish()` by then and `first_present` came before 1.25 s (the cap minus the 250 ms fade; a splash shown later
still fades in full).

## Splash cap and background boot (ADR-173)

The splash is gone 1.5 s after process start at the latest, fade-out included (`BootSplash::max_duration`,
default `regia_render::SPLASH_MAX_DURATION`; `REGIA_SPLASH_MAX_MS` overrides it, `0` = no cap). Once the host
has called `finish()`, the cap overrides every engine hold: a running game-frame warm, `min_duration`. It
never fades a splash the host has not finished. What is not done by then continues behind the host's first
scene:

- the game-frame warm stays on its warm worker. The window draws no scene until it is back, only the clear
  color plus the UI overlay (the loading path), so nothing compiles on the frame path;
- deferred variants build in the background after the first game frame (ADR-161); a draw whose variant is not
  ready is skipped;
- the splash's own pipeline is the boot tier's first job when the queue exists (`BootSplash::set_warm_queue`, which
  the renderer does itself, ADR-5131): the first present clears to the splash background and the image and text fade
  in once the job is done, so not even the splash compiles on the presenting thread. A host that calls
  `set_warm_queue(None)` keeps the splash pipeline on the presenting thread (ADR-5620);
- the overlay layers (lobby UI) are the boot tier's next job, so they are built before the cap; a layer installed
  after the queue started (the runtime UI sets its layer in `Startup`, after the first resume), or one that replaces
  a layer under the same name, gets its own boot-tier job (ADR-5131, ADR-5620);
- the first-launch quality probe runs on a warm worker after the splash and the warm: its own renderer draws
  the scene offscreen at the window's size back to back for ~3–4 s and holds nothing (a level load aborts it;
  `quality_probe_start` / `quality_probe_done` marks).

`regia_render::BootBackground` (resource, refreshed each `PreUpdate`) tells hosts where this stands:
`splash_done`, `splash_capped`, `game_warm_pending`, `game_warm_running`, `deferred_pending`, `probe_active`, and
`warm_done()`, `probe_done()`, `is_idle()`. Agents read the same under `render.pipeline_warm` →
`boot_background` (`splash_done`, `splash_capped`, `game_warm_pending`, `game_warm_running`, `deferred_pending`,
`probe_active`, `idle`).

## Pipeline warm-up and the persistent cache

```json
{"jsonrpc":"2.0","id":2,"method":"render.pipeline_warm","params":{}}
```

The method stays `Read` (`mutating: false`). On a headless host, `offscreen: { width, height }` starts a warm that moves the live renderer onto its own thread and leaves a stand-in in its place until the warm is put back. `render.out_start` answers `F0018` until then. A host that already has a window does not start that warm.

Per tier (`boot`, `menu`, `lobby`, `map`, `optional`): jobs done/total/failed, pipelines built, worker time and
`ready_after_ms`; `pipelines_outside` counts pipelines created outside the warm workers (the frame path, load
code) — after warm-up a steady frame adds none. `cache.load` is `missing` (first launch on this adapter/driver),
`loaded` with its bytes, `discarded` with the reason (driver update, shader change, corruption), or `seeded`
(no usable local file; a shipped cache for this exact GPU and driver was used, see below). `cache.shipped` says
what the package's caches gave when they were consulted: `no_match`, `seeded` with the file, or `discarded` with
the file and the reason. The cache is saved once on a background thread right after the first game frame, so a
first launch closed before the warm queue drains keeps the first frame's pipelines (ADR-5857), and again when the
queue drains (`cache.saves`, `saved_bytes`). `cache.skipped`
names a driver rule that left the device without the engine cache (`null` otherwise): Intel's Windows Vulkan driver
since v0.2.15 (ADR-3288; `platform.paths` shows `driver_skipped`). `deferred`
counts the rarely used pipeline variants waiting for a background build (`pending`), built so far (`built`),
and whether the first game frame released them (`released`).

## Install prepare

`regia prepare --cache-dir <game pipeline cache>` (or `regia_render::prepare::run`) warms the splash and the
canonical lit frame headless, on the Boot thread budget, and writes `prepare-stamp.json` next to the `.fpc`.
A later launch with the same cache key and the same unit list skips. Progress is one JSON object per line on
stdout; `prepare-status.json` is what a launcher polls (`running` older than 45 s is `stale`). The command
never presents, so the queue stays in Boot. `--profile` adds that preset's frame to the unit list. Background
priority is on unless `--no-nice`. See the DO27 host checklist, "Install prepare (batch v020)".

## One device holds the cache

One device per process holds the persistent cache. A headless device opened while a live device holds it compiles
without a cache and leaves the file alone; a window's device takes the cache over from a headless one, and the
headless device keeps working uncached. `pipeline_cache::status` reports the device that holds it.

## Shipped pipeline caches

A Vulkan pipeline cache only helps on the GPU and driver that wrote it, so a package carries one per GPU × driver
it was warmed on. Build them on each test machine by running the game (or `boot_gpu --game --profile <tier>`
for each render profile the game ships) with `REGIA_PIPELINE_CACHE_DIR=<dir>`: every tier accumulates in one
`pipelines-vulkan-<vendor>-<device>.fpc` per GPU. Collect the files into `<project>/pipeline-caches/` (or pass
`--pipeline-caches DIR`) and `regia package --platform linux|windows` validates each one (header, key,
checksum: a broken file fails the package), renames it to its key's shipped name
(`pipelines-vulkan-<vendor>-<device>-<key hash>.fpc`, so two drivers of one GPU sit side by side) and puts it in
`content/pipelines/`. `build_info.json` lists them (`pipeline_caches`: vendor, device, driver, wgpu, shader hash).

At runtime the shipped directory (`<exe dir>/content/pipelines`; `REGIA_SHIPPED_PIPELINE_CACHE_DIR` or
`pipeline_cache::set_shipped_directory` override it) is read only when the local cache is missing or unusable,
and only a file whose whole key matches (backend, vendor, device, driver name and version, wgpu, shader hash)
is used. It passes the same checks as a local file, then wgpu's header check (adapter, driver pipeline-cache
UUID) and the driver's own; any rejection means an empty cache and a normal compile behind the splash, never a
crash (`tests/shipped_pipeline_cache.rs` feeds another driver's key, garbage, and payloads with a corrupted
wgpu or driver header). The first save writes the local cache, so later launches do not read the shipped one.

The checksum catches corruption, not forgery: a cache payload goes to the GPU driver, so the local cache
directory and the shipped directory are trusted like the executable. Never point
`REGIA_SHIPPED_PIPELINE_CACHE_DIR` or `REGIA_PIPELINE_CACHE_DIR` at a directory that other users or
downloaded content can write.

Measured with `boot_gpu --game` (headless; the `boot_probe --game` frame warmed like `GameFrameWarm`, both
caches empty for cold), game-frame warm on the worker:

| GPU (driver) | before: cold / seeded / warm | after: cold / seeded / warm |
|---|---|---|
| RTX 5090 (615.71.09; loaded farm host, 48 niced cores) | 12.9–14.3 s / 3.4–4.2 s / 1.14–1.20 s | 6.1–7.6 s / 1.31–1.41 s / 1.01–1.18 s |
| RTX 3080 Ti (615.78.08) | 7.6–7.7 s / 2.08–2.11 s / 1.05–1.06 s | 3.71–3.74 s / 1.05–1.06 s / 0.96–0.98 s |
| GTX 1080 (580.173.02) | 8.5–8.9 s / 2.72–3.07 s / 1.42–1.49 s | 3.84–3.92 s / 1.37–1.40 s / 1.29–1.32 s |
| RTX 5080 (615.71.09) | 5.6–6.8 s / — / 0.63 s ¹ | 2.24–2.34 s / 0.60–0.63 s / 0.52–0.57 s |

¹ boot-fast's windowed `boot_probe --game` numbers (47 pipelines then); the rig's GPU queue was too long to rerun
the old code headless.

cold = first launch, both caches empty; seeded = first launch with the package's cache for this GPU (driver disk
cache still empty); warm = second launch. "Before" seeded is the shipped cache on the old pipeline set, where 20
uncached sites still compiled. Desktop High and Ultra build the same 59 pipelines for this frame. The first
1920×1080 frame after the warm creates 0 pipelines in every run; the deferred variants then build in the
background (1–4 s of worker time cold, none once cached).
No GPU in the set (an AMD card, another NVIDIA driver): the launch compiles, which the cold column covers.

## Cold compile: only what the game frame draws

`warm_site_costs(n)` (and `boot_gpu`'s `slowest_sites`) lists the creation sites a warm spent its time in. A cold
NVIDIA driver spends 0.4–1.4 s per variant of the forward uber shader, so:

- **Deferred variants.** The forward set's blend, premultiplied, additive and five glass transmission pipelines
  (per format and cull mode) are `pipeline_warm::DeferredPipeline`s. The warm (`warm_scene` / `warm_extracted`,
  hence `GameFrameWarm` and `BootSplash::finish()`) builds every variant a material of its frame can draw, visible
  or not, so the level's glass and blended materials are ready when the splash fades. Variants no loaded material
  uses build on the warm workers after the first game frame (`release_deferred`; `GameFrameWarm` and
  `RendererWarm::take` call it). A material that appears later, before its variant is ready, is skipped for that
  frame and its build jumps the queue (`DeferredPipeline::try_get`): the frame path never compiles or waits.
  `pipeline_warm::blocking_scope()` lets a loading screen wait instead. Without warm workers the variants are
  built eagerly.
- **Side by side.** Independent compiles run on helper threads (`pipeline_warm::join` / `join_all`, which keep
  the warm tier and the frame counters of the calling thread): the opaque forward pipeline beside depth, shadow and
  sky; the culled and cull-none forward sets; the GPU-driven forward variants; DDGI's eight and SSSR's seven compute
  pipelines.
- **Every pipeline is cached and counted.** Twenty creation sites (TAA sharpen, DDGI, SSSR, SMAA, depth pyramid,
  contact shadows, visibility-buffer shadows, lens, display stage, gizmos, immediate mode, terrain VT, VFX sort,
  2D) passed no pipeline cache, so a cache never covered them and `pipelines_outside` never saw them. They go
  through `pipeline_cache::cache_for` now.

`game_frame` is the runtime renderer's own warm (`regia_render::GameFrameWarm`): `pending`, `warming`,
`last { ms, pipelines, error }` and `first_frame_ms`, the CPU time of the first scene frame after it. With a
`BootSplash`, the splash's `finish()` requests it: the runtime renderer moves onto a warm worker, renders the
extracted frame at 64×64 and once at the window's size (so the first visible frame allocates no targets
either: the 28–36 ms first-frame hitch on an RTX 5080 moves onto the worker), and comes back while the splash
holds (up to the cap); the first game frame then creates no pipeline (`pipelines_outside` does not move). A later level load calls `GameFrameWarm::request()` once the level
is spawned. Check: `game_frame.first_frame_ms` < 50 and `pipelines_outside` unchanged after `game_warm_done`.

A large new scene on a window that showed none (a map spawned behind a UI-only menu: 32 MiB or 256 meshes the
runtime renderer has not uploaded) is warmed the same way without a call (v0.1.8, ADR-190): `game_frame.on_new_scene`
(default `true`, `REGIA_SCENE_WARM=0` turns it off) and `game_frame.scene_warms` (how many ran; boot mark
`scene_warm_request`). The window keeps presenting the clear color and the UI meanwhile. Check after a map loads
behind the menu: `scene_warms` moved, and `pipelines_outside` did not move on the map's first frame.
`boot_probe --exit --late-scene-s 3 [--map-mb 1024]` reproduces that order (its `late_scene` block: longest
main-loop interval after the spawn, intervals over 250 ms and over 5 s); `boot_gpu --late-map MB --mode inline|warm
--game` measures the same first frame headless (NVIDIA on a farm).

A game queues its own tiers (DO27: the menu backdrop, then both alpha maps behind the menu):

<!-- ignore-reason: names `wgpu::TextureFormat`, and wgpu is not a dependency of the book's snippet crate -->
```rust,ignore
let format = surfaces.primary_format().unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb);
queue.push(WarmTier::Map, "map:switchbacks frame", pipeline_warm::frame_job(map_frame.clone(), format));
```

## Load sets

```json
{"jsonrpc":"2.0","id":3,"method":"assets.load_progress","params":{"set":"map:switchbacks"}}
```

Returns `{ sets: [{ name, resident, items_resident, items_total, fraction, kinds: [...], failed }] }`. `resident`
is the answer to "can Ready switch to this map now?". Kinds: `texture`, `mesh`, `material`, `terrain_tile`,
`audio_bank`, `pipeline` (warm job labels), `other`. Omit `set` for every defined set.

## Memory budget and admission

```json
{"jsonrpc":"2.0","id":4,"method":"render.memory_budget","params":{"tier":"minimum","set":"map:bore_twelve"}}
```

Returns the tier's budget (`minimum` = RTX 2080 class: 6.5 GiB VRAM, 10 GiB RSS; `deck`; `desktop_high`;
`integrated_low` = the integrated-GPU floor Very Low is sized for: 1.5 GiB GPU memory, 2.5 GiB RSS, ADR-8878), the
measured GPU bytes by category and RSS, every load set's estimated and still-pending bytes, and with `set` the
admission check `{ fits, vram_after, ram_after, reason }` — whether that set can be preloaded now. A side
that is not measured (`use.vram_measured` false: GL, Metal and WebGPU have no allocator report; `use.ram` 0: no
resident-set sample on this OS) gives `fits: false` with a reason that names it, never an approval against an
empty baseline. Byte totals saturate at the largest u64 instead of wrapping.

Nothing picks the budget tier for you: `tier` defaults to `minimum`, and a host that runs the Very Low quality tier
asks with `{"tier":"integrated_low"}`. An unknown tier is an invalid-params error (`F0002`) whose message lists every
tier name.

## Probe

`cargo run -p boot_probe -- --exit [--game]` shows a splash, warms the boot tier, finishes the splash when its
scene is spawned (the engine then warms the runtime renderer on it), fades into the lit scene and, once the
splash is gone and the scene has drawn, prints `BOOT_PROBE {json}` (timeline, warm report, `game_warm`,
`first_game_frame_ms`, `max_frame_ms_after_splash`, `pipelines_outside_after_warm`, `splash_capped`). `--game` uses Desktop Ultra with sky, clouds and volumetric fog. Run it twice with `REGIA_PIPELINE_CACHE_DIR` set to
compare a cold and a warm pipeline cache; set `__GL_SHADER_DISK_CACHE=0` (NVIDIA) or
`MESA_SHADER_CACHE_DISABLE=true` (Mesa) for a cold driver cache.

On a GPU that cannot present to the machine's display (NVIDIA under a farm's Xvfb), `boot_gpu --cache-dir DIR
--game [--profile ultra|high|low|deck]` runs the same warm headless and prints `BOOT_GPU {json}`: `game { ms,
pipelines, after_warm_pipelines, first_frame_ms }` (the first 1920×1080 frame after the warm; `after_warm_pipelines`
must be 0), `deferred_ms`, `slowest_sites`, and `cache.shipped`.
`docs/evidence/perf/shipped-shader-cache-20261009/cold_warm.sh` runs cold, warm and seeded per profile and writes
the GPU's shipped cache (`SHIP_OUT`).

## Scene ready

`render.scene_ready` (ADR-7883) is the host's scene prepare: one state with a single ready edge, instead of
polling the warm counters. The host spawns its map behind its loading page and calls
`SceneReadiness::prepare(map_id)` (resource `regia_render::SceneReadiness`). From that call until ready the window
keeps presenting the host's UI and does not draw the scene, while `GameFrameWarm` warms the runtime renderer on the
extracted frame on a warm worker: every mesh and texture the frame references is uploaded (all of them, not the
first 32 MiB) and the pipelines of every draw key are created. When a warm round returns and the frame holds a mesh
or material the round did not see (a spawn that landed after it started), it warms again, up to
`SCENE_PREPARE_MAX_ROUNDS` (8) rounds; the new-scene warm's once-per-stretch lock-out does not apply while a
prepare is open. Hosts that never prepare keep the size-threshold warm. A warm that started with nothing to upload
(a backdrop already resident) no longer uses up the scene-less stretch either: the map that arrives while it runs is
warmed after it (DO27 tester A2's order).

```text
{ "method": "render.scene_ready", "params": {} }
→ { "state": "presented", "map_id": "switchbacks", "prepare": 1, "uploaded_mb": 340.2, "meshes": 183,
    "pipelines": 85, "warms": 2, "ms": 41012.5, "timed_out": false, "error": null, "complete": true,
    "first_present_frame": 912, "first_present_ms": 41230.1, "presented_to_window": true }
```

`state` walks `idle` (no prepare yet) → `preparing` (warming; the scene is held) → `resident` (warmed; the first
scene frame is being drawn and presented) → `presented`. `wait_until` on `render.scene_ready` with `state ==
"presented"` is the agent's "the map is on screen". At the `presented` edge the engine sends the ECS message
`regia_render::SceneReady` once per prepare: after `queue.present` returned for the first frame that drew the scene
(on the frame generation path, after its present thread presented that frame's rendered image; a suspend that drops
the image re-arms it), never on CPU time. Gate audio, player control and the loading page on it.

- `timed_out`: a warm round was cancelled or abandoned by the watchdog (`GameFrameWarm::timeout`), the warm queue ran
  on another device, or no renderable frame came within `GameFrameWarm::request_grace` (10 s). The prepare still
  ends in `presented` (late): a host never waits forever. `error` says why.
- `complete`: whether the first scene frame found every mesh resident and created no pipeline on the frame thread.
- `presented_to_window: false`: headless apps and hidden windows present nothing. A hidden window reaches `presented`
  on the first tick the scene is resident and renderable. A headless app with a render device and a started
  `PipelineWarmQueue` runs the same warm rounds as the window (a frame with no window view warms its render-to-texture
  views) and reaches `presented` once the frame thread submitted a texture-view frame of the resident scene (the tick
  after it turned resident), or right after its warm when the frame has no texture view (the warm drew and submitted
  it). Its texture views are held, like the window's scene, while the prepare warms. A resident headless prepare that
  sees no submitted frame within `request_grace` ends with `timed_out`. Without a render device nothing warms or
  draws: `presented` on the first renderable tick (ADR-8122).
- On the frame generation path only the present of the scene's own image counts: when the real image's acquire
  fails after the generated image was queued, the prepare stays `resident` and the next drawn frame is its first
  (ADR-8122).
- A new `prepare` before the last one was presented replaces it; only the newest sends `SceneReady`.
- Hosts that draw into their own targets (benchmarks) drive the same logic with `GameFrameWarm::host_tick` and
  `GameFrameWarm::host_presented`. `boot_gpu --late-map 340 --mode prepare --game` and `boot_gpu
  --late-map-split 40,300 --mode prepare|auto --game` measure the map's first frame (pipelines, upload MB) with and
  without the prepare.

Telemetry: `scene_load_end` gains `map` (the prepare's map id) and `complete` (false when the first scene frame after
the warm still uploaded or compiled, or another warm was needed); it is sent once that is known. A `scene_ready`
phase goes out at the `presented` edge with `first_present_ms`, `frame`, the warm totals, `complete`, `presented`
and `timed_out`.

Error codes: `F0011` (no scene readiness: the render plugin was not added).


## Load budget

A warm covers what the scene held when it ran. What lands after it (a late spawn, a streaming LOD swap, a warm the
watchdog gave up on) used to upload and compile on the frame that first drew it: one frame of 200-500 ms on a
tester's map (ADR-7884). `FrameRenderer::set_load_budget(FrameLoadBudget { .. })` bounds that frame.

| Field | Default | Under `late: Defer` |
|---|---|---|
| `upload_bytes_per_frame` | `None` | Meshes the frame references but has not uploaded are taken nearest first (squared distance from the eye to the instance's origin, ties in submission order) while the frame's mesh bytes are under the cap. The mesh that crosses it still lands, so a frame uploads at most the cap plus one mesh; the rest wait for the next frames and their draws are skipped meanwhile. |
| `new_pipelines_per_frame` | `None` | A material graph whose pipeline set would take the frame thread past this many pipelines in the frame builds on the material-graph worker threads; its draws are skipped until the set lands. The set size is the largest set built so far (32 before the first). |
| `late` | `Block` | `Block` ignores both caps: every frame uploads and builds what it draws, as before. |

`FrameLoadBudget::RECOMMENDED_60FPS` is 64 MiB, 8 pipelines, `Defer`. The trade: a late object appears a few frames
later instead of costing one long frame. Ephemeral meshes (`mesh_key` 0) are never deferred. Warm jobs and blocking
scopes are never budgeted. The budget is per `render_into` call (one view). The engine's deferred blended,
transmissive, mask-decal and thin-film variants already skip their draw and build on the warm workers when a
`PipelineWarmQueue` runs. When the material-graph workers cannot take a set (they cannot start, or were shut
down), `Defer` builds it on the frame thread only as the frame's first pipeline work (one set past the cap, like the
mesh that crosses the upload cap) and the other draws wait for the next frames (ADR-8122).

**What `Defer` does not bound.** A frame-level forward set (`lab forward`, `lab depth`, `lab shadow`, `lab sky` and the
GPU-driven colour variants of one `PipelineKey`: target format, sample count, sun trace, many-light, double-sided) is
still built on the frame that first needs it, whatever the budget: without its set the frame cannot draw any of its
scene, so there is nothing to draw "a few frames later". That is the stall class of DO27 tester A2's first gameplay
frame (79 pipelines). Cover it with the warm instead: `SceneReadiness::prepare` warms the map's frame (its key at the
window's size) before the scene is shown, and `pipeline_warm::register_warm_target` covers every other target the
host renders into (see Targets and late pipelines below). A settings change that makes a new key mid-play (MSAA, the
sun trace) builds the set on the frame that applies it.

`FrameRenderer::load_budget_report()` has the last frame's counters: `deferred_meshes`, `deferred_bytes`,
`deferred_draws`, `async_pipelines_pending` (graph sets on their workers plus the deferred variants, process-wide),
`admitted_meshes`, `admitted_bytes`, `async_graph_sets` and `bursts`. A `debug` line on target `regia_render` marks the
start and the end of each deferral burst, never every frame.

| Method | Params | Result |
|---|---|---|
| `render.load_budget` | none | `{ budget: { upload_bytes_per_frame, new_pipelines_per_frame, late, active }, last_frame: { deferred_meshes, deferred_bytes, deferred_draws, async_pipelines_pending, admitted_meshes, admitted_bytes, async_graph_sets, bursts } }` |
| `render.set_load_budget` | `upload_bytes_per_frame?`, `new_pipelines_per_frame?` (positive integers, or null for no cap), `late?` (`block`, `defer`), `recommended?`, `reset?` | `{ before, after }` (each a `budget` object) |

Absent fields keep their value; `recommended: true` starts from the recommended budget, `reset: true` from the
default. Applies from the next frame; not persisted.

Errors: `load_budget_renderer_missing` (no `RuntimeRenderer`), `load_budget_bad_param` (a cap that is not a positive
integer or null, `new_pipelines_per_frame` past 32 bits, `late` not `block` or `defer`, a flag that is not a boolean,
or `recommended` and `reset` together; the budget is unchanged).

Check: `cargo test -p regia_render --test load_budget` (lavapipe): a 300 MiB burst under 64 MiB uploads at most 64 MiB
plus one mesh per frame and draws what the `Block` run draws once resident; 40 new graph keys under 8 pipelines create
at most 8 pipelines and no blocking poll per frame on the frame thread; the default and `Block` with caps match.
Measure: `boot_gpu --late-map 340 --mode inline --game --load-budget 64,8` (report `late_map.load_budget`).

## Targets and late pipelines

A renderer that draws into a colour format or MSAA sample count the warm never rendered (a render-to-texture camera, a
second window, an offscreen capture) builds its forward set (`lab forward`, `lab depth`, `lab shadow`, `lab sky`), the
post stack's output pipelines and the effects' sets for that format on its first frame (ADR-7885). Tell the warm about
every such target where you create it, before the game-frame warm:

```rust
use regia_render::pipeline_warm;

// Pass the minimap camera's target key: Rgba8UnormSrgb with 4x MSAA.
fn register_minimap(target: pipeline_warm::WarmTargetKey) {
    pipeline_warm::register_warm_target(target.format, target.samples);
    // Or, with the queue at hand: queue.warm_target(target.format, target.samples).
}
```

Every frame warm that starts afterwards (`GameFrameWarm`, `FrameRenderer::warm_scene`, `warm_extracted`) renders its
warm frame into each registered target as well, twice with the frame's post stack and once without, at the target's
sample count. `VfxGpu::warm_formats` and `terrain_warm_formats` add the registered formats. A target created after the
game-frame warm ran: register it, then call `GameFrameWarm::request()` (or warm the renderer that draws into it with
`PipelineWarmQueue::warm_renderer`). Switching the window to MSAA mid-play is the same case: register
`(window format, 4)` before the warm.

The game-frame warm also builds every deferred blended and transmission variant (`lab blend`, `lab premul`,
`lab additive`, `lab transmit *`) of every forward set it made, and the GPU-driven cull pipelines, whatever the warm
frame held, so the first glass or blended draw of play compiles nothing (before, they built on a warm worker after
warm-up while the draw was skipped).

**What a late pipeline says.** Every pipeline creation carries `CreateDetail`: the target format, depth format, sample
count, the thread (`frame`, or `warm_worker` for a warm job or background build), the call site and `new_axes`, the
forward-set key axes the renderer had no set for (`first_set`, `format`, `samples`, `encode`, `double_sided`,
`sun_rt`, `studio_off`, `restir_di`). A late set with `new_axes: ["format"]` is an unregistered target; `["samples"]`
an MSAA switch; `thread: warm_worker` was compiled off the frame thread (no stall, but the warm did not cover it).

- Telemetry: the `pipeline_after_warmup` event lists `creates` with these fields; the frame summary's
  `created_after_warmup` has `sites` (per kind, label, format, samples and thread, with a count), `unnamed`
  (creations the 32-entry ring had overwritten), `frame_thread` and `warm_worker`. The counts are kept on every frame,
  so the rate-limited event loses nothing.
- Agents: `render.pipeline_warm` answers `targets: [{ format, samples }]` and `recent_pipelines: [{ label, format,
  samples, depth_format, thread, site, new_axes }]` (the newest 32 creations of the process, pipelines only).

```json
{"jsonrpc":"2.0","id":5,"method":"render.pipeline_warm","params":{}}
```

- Bench: `do27_chase --assert-no-late-pipelines` fails the run when a pipeline is created after the warm-up frames and
  prints each one with these fields.
- Scene checks (lavapipe): `cargo test -p arena --test late_pipelines` and `cargo test -p showcase --test
  late_pipelines` warm the arena and the showcase like the game-frame warm, run 300 fixed 60 Hz steps and fail with
  these fields on any pipeline created after the warm; the arena's second test adds a registered `Rgba8UnormSrgb` 4x
  MSAA offscreen renderer created after warm-up (ADR-8122).

**TSR bind groups.** TSR's resolve builds its bind groups for both ping-pong parities of the frame's colour input and
of the views the frame hands it instead on other frames: the stochastic SSR pass returns its own input once 30
readbacks saw no glossy pixel, so a still shot of a matte area switched the resolve to a view it never bound during
warm-up (two `tsr resolve` groups about 31 frames into a still shot, the 2026-11-15 Switchbacks soak). The pass
falling asleep or waking up after warm-up now creates none (ADR-8122; `cargo test -p regia_render --test
pipeline_warm_targets tsr_creates_no_resolve_group`).

No new error codes: `render.pipeline_warm` answers `F0011` without a warm queue, as before.

## Terrain preparation

Hosts can queue `regia_terrain::prepare_terrain` before installing terrain. The existing
`render.pipeline_warm` result counts the `prepare terrain` job in the map tier, including
constructor and optional draw-pipeline compiles. Its Rust `TerrainPreparation` handle reports
success separately from terminal failure and transfers the prepared draw once. No additional
agent method is needed. See [prepare terrain pipelines before install](../user-guide/terrain-data.md#prepare-terrain-pipelines-before-install).

## Prepare maps

A driver pipeline cache only helps on the GPU and driver that wrote it, and a game cannot ship one
for every machine, so a cold cache is compiled before play instead (ADR-8453): at first launch,
after a driver update, or after a game update that changed a map.

- `regia prepare --cache-dir <game cache> --project <dir> --scene levels/a.level.ron --scene
  levels/b.level.d` warms each level the way the game-frame warm warms it (the window formats,
  the `--profile` preset's settings when given, and every `--target <format>[xN]`, for example
  `--target rgba16fx4` for a render-to-texture camera), on the warm workers, then saves the
  cache and writes the stamp. Each level is spawned headless; each distinct `MeshSource`
  material (`.material.ron`, `.matgraph.ron`, `.matinst.ron`, with 1×1 stand-in textures) draws
  on a stand-in quad 5 m in front of the level's first camera (a default camera when it has
  none), so none is culled: pipelines depend on the vertex layout, the material's kind and graph
  and the frame's settings, not on geometry or pixels.
- Progress: one `load:<map>` line per level, then the usual JSON lines; while a map is warmed the
  `phase` is `map:<name>` (in `prepare-status.json` too).
- `--check` exits 2 when the map list changed: the stamp's unit list holds `map:<name>:<format>`
  per map and `maps:<hash>` of the names and fingerprints (a hash of each level's files and the
  material files it names). A request without `--scene` keeps the unit list older stamps hold.
- In the game: build one `regia_render::prepare::PrepareMap { name, fingerprint, frames }` per
  map from your own loader (spawn the map, run `extract_frame`, take the `ExtractedFrame`), put
  them in `PrepareRequest::maps` (and your extra targets in `PrepareRequest::targets`), and read
  `prepare::host_status(&request, &device.adapter.get_info())` before the menu: when `needed` is
  true, show a "Preparing shaders" page and run `prepare::run_on(&device, &request, |p| ...)` on
  a background thread. `HostPrepareStatus { needed, state, done, total, eta }` drives the page;
  `fraction()` is its bar.
- Packaging: `regia package --prepare-scene <level>` (repeatable) records `prepare_maps:
  [{ name, path }]` in `build_info.json`, the list the game (or its launcher) prepares.
- Agents: `render.pipeline_warm` answers `prepare: null | { state, phase, done, total, fraction,
  pipelines, skipped, error, elapsed_ms, eta_ms }` for a prepare this process ran.

```json
{"jsonrpc":"2.0","id":6,"method":"render.pipeline_warm","params":{}}
```

No new error codes. What the CLI loader does not see: content the game spawns from code, skinned
or morphed meshes, and terrain or effects a level does not carry as components; prepare those
from the game with `run_on`.
