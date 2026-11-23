# 02 — Architecture

## 1. Layering

Crates form a strict DAG. A crate may depend only on crates in lower layers (or the same layer
where listed; e.g. `regia_agent` → `regia_framework`, ADR-018). Enforced in CI by a script that parses `cargo metadata` (see `06-testing-and-ci.md`).

```
Layer 6  tools        regia_cli  regia_editor          (desktop only)
Layer 5  pipeline     regia_asset_pipeline, regia_templates, regia_release_chunks   (desktop only; the templates crate is the project-template data, ADR-1485; release_chunks is the CLI's release packager)
Layer 4  umbrella     regia  regia_dylib
                      (ADR-015: the umbrella moves to regia_internal and regia becomes a facade
                      that force-links regia_dylib under the hot-reload feature. That split lands
                      with the M3 hot-reload host. The hot-reload feature may name regia_editor so
                      game and editor builds resolve the same features; the editor crate is not linked
                      into the game library, and shipping builds leave the feature off — 06 § 7.)
Layer 3  gameplay     regia_framework (incl. sim_bridge; feature "net" → regia_net, destruction_net, ADR-1967)  regia_agent (protocol server, feature "agent")  regia_steam (feature "steam")  regia_media (feature "media", FFI to media decoders, ADR-1593; → regia_audio; regia_agent → regia_media; first milestone ADR-1670)
                      regia_quick (immediate Draw / Draw3d over render, ui, platform; regia_agent → regia_quick)
                      regia_telemetry (structured regia::* events, JSONL layer; → regia_framework; regia_agent → regia_telemetry; ADR-159)
                      regia_procgen (procedural content graphs and generators, specs/62-63; regia_agent → regia_procgen; ADR-1640)
                      regia_configurator (product viewer contracts: jewelry.* JSON schemas and checks; regia_agent → regia_configurator; ADR-2480, ADR-2535)
                      (regia_agent → regia_viewer: the product viewer's renderer, checked headlessly; ADR-2539)
                      (regia_agent → regia_looks: looks.* data and the look board still; ADR-2538)
Layer 2  systems      regia_render  regia_physics  regia_audio  regia_animation  regia_ui(→render,input)
                      regia_viewer (product viewer: lean forward pass for native, WebGPU, WebGL2; no engine crate below it; ADR-2539)
                      regia_looks(→viewer) (product looks: metal, anodize, gem, cabochon, opal, frosted hooks and stand-in data; ADR-2538)
                      regia_terrain(→render)  regia_vfx(→render)          (created in M13 / M14)
                      regia_matgraph(→render)                               (material graphs, specs/22 T2.4)
                      regia_2d(→render)                                     (2D toolkit, specs/22 T2.10)
                      regia_net(→ecs; →audio with feature "voice", ADR-1911) (client-server networking, ADR-1600)
                      regia_xr(→render,input,scene)                         (OpenXR, specs/58; feature "xr"; regia_agent → regia_xr)
                      regia_world(→asset,scene)                             (world streaming runtime, specs/78; regia_agent → regia_world; regia_asset_pipeline → regia_world)
                      regia_fluid(→render,scene with feature "draw")         (particle liquid, PIC/FLIP; feature "fluid"; regia_agent → regia_fluid; ADR-2441)
                      regia_viewer_view                                     (product viewer drag view: damped camera clamped to a recipe's view_range; regia_agent → regia_viewer_view; ADR-2540)
                      regia_viewer_pack(→viewer_view,looks)                       (product viewer packages: quantized meshes, manifest, cook behind feature "cook"; regia_agent, regia_cli → regia_viewer_pack; ADR-2540)
                      regia_web(→viewer, viewer_view, viewer_pack, configurator, looks)   (product viewer's web player: browser loop, input, fetch, JS API; ADR-2541)
                      regia_gen(→asset)                                     (procedural mesh helpers: extrude, bevel, lathe, boolean-lite, UV projection; spec 23 § 6; regia_agent → regia_gen; ADR-7240)
Layer 1  world        regia_scene(→asset)   regia_asset   regia_input(→platform)
Layer 0b platform     regia_platform   regia_dlss (ADR-2002)   regia_xess (external SDK Vulkan FFI, ADR-4656)
Layer 0a foundation   regia_core  regia_ecs(→core)  regia_app(→ecs)  regia_macros
                      regia_detsim (lockstep kit: libm-pinned math, tick constants, FTZ/DAZ guard; no Regia crate, no ECS/GPU/platform; ADR-8012 A-5, ADR-8013)
                      regia_world_geom(→detsim) (lockstep world geometry: tunnel meshes, floor and wall queries; spec 121, ADR-8050, ADR-8166)
                      regia_rules(→regia_detsim) (ECS-free match modes, objectives, flow and playlists; ADR-8239)
Test only             regia_test_utils, regia_net_soak   (dev-dependency only; outside the runtime rules below)
```

Key rules:

- **Runtime crates (layers 0–4) never depend on layers 5–6**, except the `hot-reload` feature edge
  in the layer diagram (ADR-015). A shipped game contains no editor or importer code.
- Systems in layer 2 do not depend on each other, except on `regia_render`: `regia_ui`,
  `regia_terrain`, `regia_vfx` and `regia_2d` draw through the render graph. Cross-system interaction (e.g.
  animation driving a skinned mesh) happens through shared components defined in a lower layer
  (`regia_scene` — which is why `Skeleton` and `SkinnedMesh` live there, `specs/17` § 2), or in
  `regia_framework`. Floating bodies are such a case: `regia_framework::buoyancy` applies the water surface of
  `regia_terrain` to `regia_physics` bodies (ADR-1828).
- Editor-only metadata on runtime types (display names, ranges, tooltips) is expressed with
  reflection attributes, which cost nothing at runtime.
- **Samples**: `render_lab` depends directly on `regia_render` (Layer 2) and `regia` (Layer 4), and
  is classified as Layer::Pipeline for layer-check purposes (it is a desktop-only developer tool,
  not a shipped game sample; it may use `regia_test_utils`). The other sample crates (`lookdev`, `showcase`,
  `vfx_lab`, `arena`, …) are Layer::Sample: they use runtime crates and other samples only, and
  `regia_test_utils` only as a dev-dependency. The table is `xtask/src/layers.rs`.
- **The `server` build** (spec 76 § 2.1, ADR-1886): with `--no-default-features --features server` the graph holds no
  window, render or audio crate. Default features own those edges (`regia/client`, `regia_framework/client`,
  `regia_platform/window`, `regia_input/devices`, `regia_ai/debug_draw`), so a crate that a server links takes
  `regia_platform` and `regia_input` with `default-features = false`. `regia_net` depends on `regia_platform` on Windows
  only (`console_ctrl`). `xtask check-layers` reads this graph and `check-server-build` compiles it.

## 2. The data model

- **ECS (bevy_ecs via `regia_ecs`).** All runtime state is entities + components + resources.
- **Reflection (bevy_reflect via `regia_ecs`).** Every component, resource and asset type that is
  saved, edited or hot-reloaded derives `Reflect` and is registered in the `TypeRegistry`. Reflection
  drives: the inspector, scene/prefab serialization, undo/redo, hot-reload state transfer, network
  replication (Tier 2), and property animation.
- **Stable identity.**
  - Entities that live in saved data carry `EntityGuid(u128)`; references between them serialize as
    GUIDs and are remapped to `Entity` on load.
  - Assets have `AssetId(Guid)` from their `.meta` sidecar; serialized references are `AssetRef<T>`.
  - Types serialize by their **type path** (`my_game::Door`), never by `TypeId`.

## 3. Coordinate system and units

Right-handed, **+Y up, −Z forward, +X right**. 1 unit = 1 metre. Handedness, up axis and units match
glTF. Facing does not: glTF defines the *front* of an asset as +Z, the opposite of Regia Engine's forward. The
glTF importer therefore turns every imported asset 180° about +Y by default (ADR-009, `specs/12` § 7), so
a standard glTF model's nose ends up on −Z and "forward" means the same thing for cameras, lights,
characters and vehicles. This is also DO27's own mesh contract (metres, +Y up, forward −Z). Angles in
radians in code, degrees in the editor UI. Reversed-Z depth with an infinite far plane. Front faces wind
counter-clockwise. Mass in kg, time in seconds, audio levels in dB at the API.

## 4. Application model

```rust
// Game crate (lib):
pub struct MyGame;
impl regia::GamePlugin for MyGame {
    fn register(&self, app: &mut App) {
        app.register_script::<Door>()
           .add_systems(Update, spin_pickups)
           .set_game_rules::<MyRules>();
    }
}
regia::export_game!(MyGame);   // regia_game_entry + REGIA_ABI_HASH (specs/20 § 2)
```

`App` holds the main `World`, the schedules, and the plugin list. `DefaultPlugins` add the platform,
input, asset, scene, render, physics, audio, animation, UI and framework plugins, each of which can be
disabled or replaced. Headless builds (dedicated servers, tests) use `MinimalPlugins` + chosen subsets.

## 5. Frame loop

The platform layer owns the OS event loop (winit `ApplicationHandler`). One **frame** runs these
schedules on the main `World`, in order:

| Schedule | Purpose |
|---|---|
| `First` | time update, frame counters, message buffer swap (`specs/10` § 2.1) |
| `PreUpdate` | platform events → input state, asset load completions, hot-reload swaps |
| `FixedUpdate` (0..N times) | fixed-step simulation at `FixedTime::hz` (default 60): physics step, character movement, `Script::fixed_update` |
| `Update` | gameplay: scripts, game systems, animation graph evaluation, UI logic |
| `PostUpdate` | in this order: late scripts, physics interpolation, animation pose → joint transforms, transform propagation, visibility, audio listener/emitter sync, UI layout |
| `Extract` | copy render-relevant data into the **render world** (see § 6) |
| `Render` | render graph execution, submission, present |
| `Last` | diagnostics, deferred despawns, frame-end cleanup |

Outside the frame, the platform runner runs the `Lifecycle` schedule synchronously on every lifecycle transition (`Started`, `Suspended`, `Resumed`, `LowMemory`, `WillTerminate`), so that work which must finish before the OS callback returns — dropping render surfaces on Android suspend — happens in time (ADR-012).

Fixed step uses an accumulator with a max of 5 steps per frame by default, hard cap 64 (spiral-of-death guard) and exposes
`FixedTime::overstep_fraction()` for interpolation of rendered transforms.

## 6. Render world and extraction

The renderer owns a separate `RenderWorld` (its own ECS `World`). In `Extract`, render plugins read
the main world (read-only) and write compact render-side data (meshes to draw, lights, cameras,
skin matrices, UI draw lists). Render systems never read the main world.

Tier 1 runs `Extract` and `Render` on the main thread (render work itself is parallel via tasks for
culling, sorting and command encoding). Because the render world is already isolated, Tier 2's
**pipelined rendering** (render frame N on a render thread while simulating frame N+1) is a scheduling
change, not an API change. Do not let any render code assume it runs on the main thread.

## 7. Threading

- Thread pools (from `bevy_tasks`, wrapped by `regia_core::tasks`):
  `ComputePool` (ECS systems, culling, animation), `AsyncComputePool` (cooking, navmesh builds,
  long jobs), `IoPool` (file/pak IO, decompression).
- ECS systems in a schedule run in parallel where their declared access permits.
- Gameplay scripts (`Script` trait) run on the main thread in a single exclusive system per schedule,
  giving them unrestricted world access (simple mental model, like single-threaded game logic in
  other engines). Bulk/hot-path gameplay uses ordinary parallel systems.
- Audio runs on kira's audio thread; physics uses rapier's own (optional) parallelism.
- Android: the event loop thread is the main thread created by `android-activity`.

## 8. Asset flow

```
source files (assets/**/*.gltf, .png, ...) + .meta sidecars
        │  regia_asset_pipeline (editor/CLI, desktop only)
        ▼
asset database  ──►  cooked cache .regia/cooked/<platform>/<asset-id>.bin   (dev builds read this)
        │  regia cook --platform X
        ▼
paks dist/<platform>/content/*.fpak   (shipping builds read this)
        │  regia_asset (runtime) — AssetServer, Handle<T>, async loads on IoPool
        ▼
render/audio/physics resources (GPU upload in Extract/Render)
```

The runtime never parses source formats (glTF, PNG...). Dev builds and the editor load cooked data
from the cache, which the asset database keeps current, so dev and shipping exercise the same loaders.

## 9. Hot reload paths

| What | Mechanism | Where |
|---|---|---|
| Assets | file watcher → reimport → in-place swap behind the existing `Handle` → `AssetEvent::Modified` | editor + dev builds |
| Shaders | WGSL file watcher → recompile pipelines → swap on next frame; errors keep the old pipeline | editor + dev builds |
| Game code | `on_unload` → reflect-serialize → load a salted game dylib → restore → `on_load` (`specs/20`) | editor host only; from M3 to M9 that host is a bare `regia_editor` window without panels |
| Config (input, physics layers, audio buses, project settings) | same as assets | editor + dev builds |

## 10. Project structure (user projects created by `regia new`)

```
MyGame/
  Regia.toml            project manifest (name, version, engine version, platforms, startup level, android/ios ids)
  Cargo.toml              workspace: game, editor, launcher, android  (+ a sim dylib only when the game has an external sim)
  game/                   gameplay crate — an rlib; the editor also builds it as a dylib (specs/20 § 1)
  editor/                 bin: regia_editor, engine dylib, depends on `game` (feature anchor)
  launcher/               bin: shipping/desktop executable; links game statically
  android/                cdylib "main": android_main → regia::run_android(game)
  assets/                 source assets + .meta files (committed to VCS)
  config/                 input.ron, physics.ron, audio.ron, render.ron, project-level settings
  generators/             generator manifests (specs/23 § 6)
  .regia/               caches (cooked assets, asset DB, hot-reload copies) — gitignored
  build/android/          generated Gradle project — regenerable, gitignored
  dist/                   packaged builds — gitignored
```

The editor binary is built *inside the user project* (like the game) so that it is compiled by the same
compiler, with the same engine build, as the game dylib — a requirement for Rust dylib compatibility.

## 11. Error handling philosophy

- Content errors (missing asset, bad material, shader compile failure, corrupt save) never crash a
  running game: log, substitute a visible placeholder (magenta checker texture, error mesh, silent
  sound), continue. Errors surface in the editor's output log with the asset path.
- Programming errors (violated invariants) panic with a clear message. Shipping builds use
  `panic = "abort"` with a panic hook that writes a crash log (and minidump on desktop, Tier 2).
- GPU device loss: recreate the device and re-upload resident assets (desktop); on Android, treat as
  a suspend/resume cycle.

## Bike lab sample registration (ADR-4017)

`bike_lab` is `Layer::Sample`: its normal dependency is the runtime `regia_physics` crate. The
headless lab has no editor or importer dependency and follows the existing sample exclusions.
`xtask/src/layers.rs` registers it explicitly so the complete workspace layer check covers it.
