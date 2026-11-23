# Card costs of opt-in renderer features

The renderer features that are off in every preset (the radiance cache, blue noise, order-independent
transparency, screen-space subsurface scattering, shadow occlusion culling and the lens effects) each ask
for a card number before a preset turns them on, as do the weather (rain, snow, wet surfaces) and the
material features (thin film, rough transmission). `cargo xtask gpu-bench --suite features` measures them:
each feature against the plain frame, per tier, on one card, interleaved (ADR-2309).

It is repository tooling, like the [public benchmark scenes](bench.md): there is no running app to call,
so it has no agent-protocol method. An agent runs the command and reads `features.json`. The GPU memory
it records is the same set of fields as `seeing.memory_report`.

## Run it

A dry run plans the launches and writes the tables with no card and no binary:

```sh
cargo xtask gpu-bench --suite features --out /tmp/fc --dry-run \
    --features oit,radiance-cache --rounds 2 --bench-scenes synthetic_atrium,toy_car
cat /tmp/fc/features.md
```

On a card, build the binaries once, then hold the card's lock for the whole run so nothing else uses it
between two launches:

```sh
cargo build --release -p lookdev --features alloc-count --bin do27_chase -p regia_cli --bin regia -p xtask
regia bench --fetch toy_car a_beautiful_game flight_helmet --assets .regia/bench-scenes
flock /tmp/regia-gpu-0.lock flock /tmp/ferrite-gpu-0.lock env REGIA_GPU_LOCK_HELD=0 REGIA_GPU_INDEX=0 \
    REGIA_GPU_UUID=<uuid from nvidia-smi -L> REGIA_GPU_STRICT=1 REGIA_DO27_CONTENT=<do27-content> \
    target/release/xtask gpu-bench --suite features --rounds 3 \
    --bench-scenes synthetic_atrium,toy_car,a_beautiful_game,flight_helmet \
    --assets .regia/bench-scenes --out out/features
```

`gpu-bench` takes `/tmp/regia-gpu-<n>.lock` and then `/tmp/ferrite-gpu-<n>.lock` (ADR-8670) per launch itself unless `REGIA_GPU_LOCK_HELD` names the
card whose lock the caller already holds.

| Flag | Default | Meaning |
|---|---|---|
| `--tiers LIST` | `desktop-high,desktop-ultra` | `LOOK_R2_TIER` names for the chase, `regia bench --preset` names for the scenes (`desktop-low` also accepted). |
| `--features LIST` | every name of each list | The chase's `radiance-cache`, `blue-noise`, `oit`, `sss`, `shadow-occlusion`, `lens-effects`, `rain`, `snow`, `vfx-layer`, `wet`; the bench scenes' first six plus `thin-film`, `transmission`, `rough-transmission`. A name runs on the launches that know it; bench-scene names alone need `--bench-scenes`. |
| `--rounds N` | 3 | Interleaved rounds: each runs the plain frame and every feature once per tier. |
| `--bench-scenes LIST` | none | Also toggle every feature on these `regia bench` scenes (2560x1440). |
| `--regia FILE` | `target/release/regia` | The `regia` binary for `--bench-scenes`. |
| `--assets DIR` | `regia bench`'s | Where the glTF scenes were fetched. |
| `--skip-chase` | off | Scenes only: no DO27 content needed. |
| `--frames N`, `--warmup N`, `--map`, `--bin`, `--content` | as the other suites | The chase run. |

The chase takes the same switches directly: `do27_chase --on oit,sss` and `--memory-json FILE`. `regia
bench` takes `--on LIST` and `--preset NAME` ([Public benchmark scenes](bench.md)).

## What it measures

A feature's cost is the median over rounds of (feature run − plain run of the same round): the GPU frame
p50 and p99 (the timestamps around the whole frame), and the CPU-inclusive frame p50. A run also records
the renderer's GPU memory after its last frame and, for the chase built with `--features alloc-count`,
the heap allocations per measured frame (process-wide, every thread).

A feature costs something only where it has work: OIT needs blended meshes on screen, SSS a material that
scatters, the radiance cache a frame that runs both the probes and screen-space GI, the lens effects a tier
that allows the pass. The DO27 maps have no blended or scattering materials, so those rows read about 0
there; the scene rows say what a frame that uses them pays.

The weather and material probes give a feature work where the content has none:

- `rain` and `snow` (chase only) draw the runtime precipitation layer (`regia_vfx`) at the tier's budget:
  spec 99's `storm` (48k streaks and 2k splashes at High) or `blizzard`, with the preset's wind, the
  occlusion view and its redraws. The chase prints the occlusion view's report after the last frame.
- `vfx-layer` (chase only) is the same VFX layer under clear weather: nothing falls. Rain's or snow's own
  cost is its row minus this one; a game that already draws VFX pays only that part.
- `wet` (chase only) shades the full surface response: water film, puddles and ripples at 1.
- `thin-film` (bench scenes only) films every non-transmissive material, so the whole screen pays: the
  upper bound of the film's cost.
- `transmission` reads the glTF transmission, IOR and volume extensions as the importer does (the default
  bench load ignores them), as clear glass; `rough-transmission` also spreads what the glass shows by the
  material's roughness. Rough transmission's own cost is the `rough-transmission` row minus the
  `transmission` row. A scene without transmissive materials (`probed_materials.transmission` 0) reads 0.

Spec 99 § 12 states the weather's budgets (5080, 1440p, Desktop High): rain 0.6 ms, snow 0.5 ms, the
surface response 0.15 ms. A row over its budget is `OVER`.

Compare numbers only from one card in one session: two cards of one model differ by about 3 %.

## The report

`features.json`: `schema` (1), `suite`, `date`, `commit`, `adapter`, `driver`, `gpu_index`, and `costs`, one
object per tier (or `<tier>@<scene>`) and feature with `rounds`, `gpu_p50_ms`, `cost_p50_ms`,
`cost_p99_ms`, `frame_p50_delta_ms`, `allocs_per_frame`, `allocs_delta`, `vram_mib`, `vram_delta_mib`,
`budget_ms` (the budget a spec or ADR states, `null` when none is stated) and `verdict` (`OVER`, `ok` or
`-` when there is no budget). `features.md` is the same table. `gpu-bench.json` keeps every run, with a
`memory` object per row.

`do27_chase --memory-json` writes `schema` (1), `available`, `backend`, `allocated_bytes`,
`reserved_bytes`, `rss_bytes`, `categories`, `labels` (top 20), `features`, `measured_frames`,
`allocs_total` and `allocs_per_frame` (`null` without the `alloc-count` build).

## Errors

The command exits non-zero when a launch fails (its row says why: `timed out`, `exit N`, or for a scene
launch the `regia bench` error code). Unknown tiers, features, `--rounds 0` and an empty scene list are
refused before anything runs. `regia bench --on` with an unknown name answers `bench_unknown_feature`
(exit 2).

## Counter checks on lavapipe (ADR-2482)

The card numbers say what a feature costs on one card; three test binaries say, without a card or a clock,
what a steady frame of a feature does, and fail when it grows:

- `cargo test -p regia_render --test lens_effects_alloc -- --nocapture`: heap allocations per frame of a
  lens frame against the same frame without the pass, the allocations inside the pass's own `record`, and
  the texture bytes it created, which must equal `lens_effects::lens_effects_memory` for the view.
  `LensEffectsBudget::of(effects, width, height)` gives the render passes and HDR copies of a view: 8 and 2
  for the cinematic look (5 bright mips, the light cells, the ghosts, the composite; the frame's per-frame
  copy and the aberration's). At 2560 × 1440 the cinematic look holds 60.4 MiB of textures, 28.1 MiB of them
  the aberration's copy.
- `cargo test -p regia_render --test shadow_occlusion_cost -- --nocapture`: the cull's compute passes and
  dispatches per culled frame (`regia_render::shadow_occlusion_work`): one pass for every culled kind's
  phase 1 and one per kind for its pyramid and phase 2, and the allocations a culled frame adds.
- `cargo test -p regia_render --test thin_film_cost -- --nocapture`: 500 filmed materials against the same
  500 without the film: creates, uploads and allocations per frame, and the colour pass's draws and
  bind-group switches. Filmed draws take the CPU colour loop, so the filmed frame binds an object group and a
  film group per draw; the ignored `filmed_draws_ride_the_gpu_driven_batches` is the acceptance check of the
  frame change that moves them onto the GPU-driven batches.

`tests/feature_cost_pins.rs` pins the three features' frames by hash on lavapipe, so a cost fix that moves a
pixel fails. The exact allocation budgets hold on lavapipe only (wgpu's backends allocate differently);
another adapter checks a per-pass bound.

The shadow casters' cull is a profile column now: `RenderProfile::shadow_occlusion` (off in every preset;
`RenderSettings::shadow_occlusion` forces it), so a preset can turn it on once the card numbers allow.

## The match bench (`do27_chase --match`)

The census chase has no units, local lights or glass, so it cannot show what a live match costs. `do27_chase --match`
(`lookdev::match_scene`, ADR-7887) places DO27's units, parked vehicles, buildings and props from the content mirror
along the Switchbacks route, seeded, at a live match's counts, with 24 local lights (every other one shadowed), four
VFX emitters and a 6 x 6 decal grid. The table gains the counts telemetry reports in `frame_summary` (`drawn`,
`mesh instances`, `shadow casters`, `local lights`), so a run can be checked against a player's log, and splits the
forward pass into terrain, glass and the rest (`meshes+sky (forward-terrain-glass)`; on DO27's planets the sky is
drawn by the atmosphere pass, not in forward). The run prints a first-frame hash of the instance list: two runs of
one seed draw the same scene.

`cargo run --release -p lookdev --example match_levers -- --content <dir> --scale 0.87` runs the same match under
the forward-pass levers (`base`, `vis` = visibility buffer, `noprepass`, `nolights`, `noshadowlights`, `flat`,
`novfx` = no VFX emitters and no decal grid) in interleaved rounds on one device and prints each run's passes and the PSNR of its plate against the first base
run (base rounds 2+ are the run-to-run floor).

## Shadow caches and glass on the match bench

`do27_chase` prints what the shadow caches redrew, from `FrameRenderer::shadow_cache_report` (ADR-7986): per sun
cascade the pages redrawn per frame and the hit rate (pages kept of 64), the frames it was redrawn whole and its caster
draws (a caster over several redrawn pages is drawn once per page); the casters that moved, appeared and disappeared
in the kept cascades; and for the local-light atlas the dirty lights, tiles redrawn, faces kept and caster draws
(`ShadowCacheReport::{atlas_caster_draws, atlas_faces_kept, sun_caster_draws}`). The base line printed with it,
`dirty lights x casters`, is what v0.5.0 drew: every redrawn tile drew every caster.

A point light is six atlas tiles. A dirty light redraws only the faces a moved caster reaches, and each face draws
only the casters that reach the light's sphere and its frustum; a caster outside a face is clipped there anyway, so
the shadows do not change. Two things make every sun cascade redraw whole on every frame, whatever the scene: a sun
direction that changes every frame (a cascade keeps its placement only for the same light direction), and a camera
that leaves a cascade's kept placement (the near cascade's, mostly). A game that animates its time of day should step
the sun rarely rather than move it every frame.

The forward pass's `glass (in forward)` row is the blended run only. `refraction copy (in forward)` and
`glass front layer (in forward)` (`GpuTimings::{refraction_copy_ms, glass_front_layer_ms}`) time the opaque-frame copy
that refracting glass reads and the front-layer pass, which run before it; both are 0 on a frame without a visible
refracting draw (the copy is taken once per frame, and only then).
