# Spec 10 — Core: `regia_core`, `regia_ecs`, `regia_app`

## 1. `regia_core`

### 1.1 Math
- `pub use glam;` and `pub mod math` re-exporting `Vec2/3/4, Quat, Mat3/4, Affine3A, IVec*, UVec*`.
- Engine types: `Transform` lives in `regia_scene`; here only pure math:
  `Aabb { min, max }`, `Sphere`, `Plane`, `Frustum` (6 planes, from view-proj, reversed-Z aware),
  `Ray`, `Rect`, `Color` (linear RGBA f32 with `from_srgb_u8`, `to_srgb_u8`, `from_hex`), `Curve<T>`
  (keyframed, linear/step/cubic Hermite, used by animation, audio, particles, UI tweening), easing
  functions, `lerp`/`inverse_lerp`/`remap`, `Isometry` helpers, random (`fastrand`-style `Rng` with
  seedable PCG32 — implement in-house, ~60 lines) with `Rng::range`, `unit_vector`, `in_sphere`.
- Tests: frustum/AABB culling cases, color conversions round-trip, curve sampling vs analytic values.

### 1.2 Identifiers
- `Guid(u128)` with random generation, hex `Display`/`FromStr`, serde as string in RON and as u128 in
  postcard. `EntityGuid` and `AssetId` are newtypes over it (defined in their crates).
- `Name` (interned-ish `Arc<str>` newtype with cheap clone) for human-readable names.
- `TypePathHash` utility (stable 64-bit hash of a type path string, FNV-1a; a leading engine crate name `regia`/`regia_*` is hashed in its pre-rename spelling, see `id.rs` and spec 12 § 4a) for compact binary references.

### 1.3 Logging and profiling
- `regia_core::log::init(LogConfig)`: tracing-subscriber with env filter (`REGIA_LOG`), console
  output, optional file sink (`user://logs/<YYYY-MM-DD_HH-MM-SS>.log`, one per session, ring of last 10;
  `LogConfig::file_dir` defaults to none and the `regia` entry points pass
  `regia_platform::paths::default_log_dir()`, ADR-5608), logcat on Android.
- `LogConfig` also installs a panic hook that logs the panic + backtrace and flushes sinks.
- `profiling` module: `scope!("name")` macro (wraps `tracing::info_span!`), and a lightweight
  in-process frame profiler (`FrameProfiler`; `regia_core` has no ECS, so the world holds it as the
  `regia::Profiler` and `regia_telemetry::SpanProfiler` resources, ADR-5608) collecting per-span CPU time
  for the last 300 frames, consumed by the editor profiler panel and the in-game stats overlay. Span exits add to a
  per-thread table that `end_frame` folds once. With feature `tracy`,
  spans also stream to Tracy. Flame capture (`FrameProfiler::set_flame_enabled`) additionally folds
  each span exit's call path into a per-thread tree, up to 65536 exits per frame; further exits in
  that frame are dropped without building a path. `FrameProfiler::flame_graph(n)` merges those trees
  into a `FlameGraph` call
  tree (inclusive/self time, `folded_stacks`, `to_svg`) for the editor's flame view and capture to file.

### 1.4 Tasks
- Wrap bevy_tasks pools: `ComputePool`, `AsyncComputePool`, `IoPool`, initialised once with thread counts
  derived from `available_parallelism()`; on Android cap compute threads at `big cores` (default 4) to
  limit thermal throttling. Expose `spawn`, `scope`, `par_chunks`-style helpers.

## 2. `regia_ecs`

### 2.1 Facade
- `pub use bevy_ecs::{prelude::*, ...}` and `pub use bevy_reflect::{prelude::*, ...}` under curated
  module names (`regia_ecs::prelude`, `regia_ecs::reflect`). Game code never names `bevy_*`.
- Hierarchy: use bevy_ecs relationships (`ChildOf` / `Children`) re-exported as `Parent`-style API in
  `regia_scene`.
- Terminology: bevy_ecs 0.20 separates buffered **messages** (`Message`, `MessageWriter`/`MessageReader`,
  double-buffered per frame) from observer-triggered **events** (`Event`, `commands.trigger`). Where
  these specs say "ECS event" for per-frame data (`AssetEvent`, `WindowResized`, `CollisionStarted`,
  `UiEvent`, `SimEvent`, ...) they mean a message; the type names keep their `...Event` spelling.

### 2.2 Editor attributes
`regia_ecs::attrs`: `Range<f64>`, `Step(f64)`, `Tooltip(&'static str)`, `DisplayName(&'static str)`,
`Category(&'static str)`, `Hidden`, `ReadOnly`, `Multiline`, `ColorHdr`, `AssetFilter(&'static str)`,
`Units(&'static str)` ("m", "deg", "s", "dB"). Used with bevy_reflect custom attributes syntax.
The inspector reads them; runtime ignores them.

### 2.3 Reflected serialization
- `ReflectCodec` (with the free functions `to_ron`/`from_ron`/`to_postcard`/`from_postcard`, which wrap
  bevy's `ReflectSerializer`/`ReflectDeserializer`; ADR-5608) producing **RON** (text, diff-friendly, for
  scenes/prefabs/configs) and **postcard** (binary, for cooked scenes, saves, hot-reload snapshots, net).
- Rules: values serialize by type path; missing fields of a named struct or a named enum variant →
  the container's `Default`, else the field type's `Default`; unknown fields → warning +
  ignored (forward compatibility); unknown types → preserved as opaque RON value in editor contexts
  (so a scene with a type from an unloaded game dylib can be re-saved without data loss) and skipped
  with a warning at runtime. Postcard is positional (ADR-016): it cannot skip an unknown type (the load
  fails with a construct error) or an unknown field, so binary data must match the registry. A type whose own serde
  skips a field (`skip_serializing_if`) writes postcard it cannot read back; the scene cook keeps the RON text for such
  a value (ADR-5608). `LevelSettings` and `LevelLook` skip their `None` fields only for a human-readable serializer, so
  they round-trip through reflected postcard (ADR-7232). A tuple struct is not filled: a missing element is an error. A type with
  `#[reflect(Deserialize)]` or `#[reflect(DeserializeWithRegistry)]` is decoded by its own serde, so
  it does not get Default-fill and an unknown field produces no warning.
- Entity references inside reflected data (`Entity` fields) are mapped to/from a stable 128-bit id via the
  `EntityIdMapper` trait (`regia_ecs::serialize`) during (de)serialization. Mapping is opt-in
  (`ReflectCodec::entity_mapper`). Without a mapper, an `Entity` is the raw `u64` from `Entity::to_bits()`,
  valid inside one process. `0` is the `EntityIdMap` miss token, written only when a mapper is set and
  the entity is not in the table. This is not bevy's
  `EntityMapper`, which only remaps entities inside a live world. A type that handles its own serde
  (`#[reflect(Serialize)]` or `#[reflect(Deserialize)]`) is not walked, so `Entity` fields inside it stay
  raw bits even when a mapper is set.
- Tests: round-trip every built-in component type through RON and postcard (generated test over the
  registry); unknown-field and missing-field cases; entity remapping.

### 2.4 Type registration
- `App::register_type::<T>()` plus `ReflectComponent`, `ReflectResource`, `ReflectDefault`,
  `ReflectSerialize/Deserialize`, and Regia Engine type data (ADR-5608): the custom inspector UI hook is
  editor-only type data, `regia_editor::inspector::InspectorEguiImpl` (a function pointer the editor
  registers; field options as `ReflectInspectorOptions`, extension widgets through
  `register_inspector`); scripts use the `regia_framework::script::ReflectScript` trait. Custom data assets
  carry `regia_asset::data_asset::ReflectAsset` (type path, default value, from-reflect), added by
  `App::register_data_asset::<T>()` or `#[reflect(Asset)]` on a `#[derive(Asset)]` type; their files are `.data.ron`
  (`docs/formats/data-ron.md`, ADR-7232).
- Every type registration records its **owner** (`TypeOwner::Engine` or `TypeOwner::Game(id)`). Systems, messages and
  observers record an owner only when it is a game plugin; engine registrations of those three are not
  stored. Hot reload uses the records to remove game-owned types, systems and observers (see `specs/20`).

## 3. `regia_app`

### 3.1 API
```rust
pub struct App { world: World, schedules: ..., plugins: ..., runner: Box<dyn FnOnce(App)> }
pub trait Plugin: Send + Sync + 'static {
    fn build(&self, app: &mut App);
    fn finish(&self, _app: &mut App) {}      // after all plugins built (e.g. create GPU resources)
    fn cleanup(&self, _app: &mut App) {}
    fn name(&self) -> &str { std::any::type_name::<Self>() }
}
impl App {
    pub fn new() -> Self;
    pub fn add_plugins(&mut self, p: impl Plugins) -> &mut Self;
    pub fn add_systems<M>(&mut self, schedule: impl ScheduleLabel, systems: impl IntoScheduleConfigs<..., M>) -> &mut Self;
    pub fn insert_resource<R: Resource>(&mut self, r: R) -> &mut Self;
    pub fn add_message<M: Message>(&mut self) -> &mut Self;   // buffered "ECS events", § 2.1
    pub fn register_type<T: GetTypeRegistration>(&mut self) -> &mut Self;
    pub fn set_runner(&mut self, f: impl FnOnce(App) -> AppExit + 'static) -> &mut Self;
    pub fn update(&mut self);               // runs one frame (all schedules in order)
    pub fn run(self) -> AppExit;           // the runner's exit status
}
```
Schedules exactly as in `02-architecture.md` § 5, plus `Startup` (once, before first frame),
`OnLevelLoaded` / `OnLevelUnloading` trigger points used by the scene system, and `bevy_state`'s
`StateTransition` (once before `Startup`, then every frame right after `PreUpdate`; ADR-107). System sets inside
`PostUpdate`, in this order: late scripts, `PhysicsSync`, `AnimationApply`, `TransformPropagate`,
`Visibility`, `AudioSync`, `UiLayout` — the systems in the sets before `TransformPropagate` run before
propagation. A system added to `PostUpdate` outside the sets has no edge to it: order it with
`.before(PostUpdateSet::TransformPropagate)` if it writes `Transform`.

### 3.2 Time
- `Time` resource: `delta`, `elapsed`, `frame_count`, `time_scale` (gameplay slow-mo; UI uses
  `RealTime`), `paused` (gameplay pause: a zero gameplay delta, so `Time::delta()` and the fixed loop's input are
  zero; `Update` still runs every frame, and UI and audio read `RealTime`).
- `FixedTime`: `hz` (default 60, project setting), `overstep_fraction()`, max substeps 5 by default (hard cap 64, `MAX_SUBSTEPS_LIMIT`).
  While `FixedUpdate` runs, `Time::delta()` is one step and `Time::elapsed()` is the fixed clock (it starts
  where the last step ended and never passes the frame's `elapsed`); the frame values are restored after the
  last step (ADR-1340). A `Time::set_clocks` between frames (a render-out job restarting or handing the clocks
  back, ADR-1881) re-anchors the fixed clock: the next steps start at the frame's simulation start minus the
  banked sub-step remainder, so the fixed clock neither lags a forward jump nor starts past a rewound frame
  clock (ADR-5762).
- `RealTime`: unscaled wall-clock delta, for UI and editor.
- Frame pacing helper: optional frame-rate cap (`target_fps`) using sleep + spin for the last ~1 ms.

### 3.2a What the gameplay pause freezes (ADR-10105)
`Time::paused` freezes the game world, not only what reads `Time::delta()`. Systems on a clock of their own follow
it: audio voices on game buses hold in place (`Ui` and `Music` play on; spec 16 § 2), video players hold their
clock in every mode (`Audio` and `Lockstep` included), a threaded sim host is held (`hold_sim_host_on_pause`), coroutine frame waits
count unpaused frames only (`Co::game_frame`), the VFX entry streak holds its frame, and the renderer's scene hold
follows the pause when the game sets `SceneHold::set_follow_pause(true)`, so the world pixels stay identical while
only the UI redraws (TAA/TSR history, jitter, noise and GI accumulation do not move). On purpose on `RealTime`: UI,
the spectator camera, `ShakeClock::Unscaled`, the baked-GI stream fade, net and voice-chat transport, overlays, the
audio output gate and stall watchdog, governors. `Time::frame_count` keeps counting paused frames. Physics,
animation, particles, decals, weather, materials, AI, navigation and rules already follow `Time` (the fixed loop
takes no step; the interpolation blend is the banked overstep). Agent: `play.freeze_state`.

### 3.3 Diagnostics
`Diagnostics` resource: FPS, frame time (avg/1% low), entity count, draw calls (filled by renderer),
memory (allocated bytes via a counting global allocator in dev builds — optional feature).
The counting allocator is `regia_core::memory::TaggingAllocator` (opt-in `#[global_allocator]`): it also
attributes each allocation to the engine subsystem the thread is working for (`memory::enter`), reported by the agent's
`memory.report` and telemetry's `memory.subsystems` (ADR-539).

## 4. Acceptance (M2)
- Unit tests above pass. A headless `App` with `MinimalPlugins` runs 10,000 frames of a test system
  deterministically.
- Bench: schedule overhead with 200 empty systems < 0.15 ms/frame on desktop reference machine.

## 5. Peer logging extensions (v0.6 NL-07, ADR-8149)

`log::set_tick` stamps standard console/file lines and tracing ring messages; it is diagnostic
state only. Opt-in `log::session::PeerLog` keeps independent peer ticks, exclusive per-session
peer directories and exact decimal-tick/16-digit-hex `.hash` checkpoint lines. Successful
writes enter the existing `logs.read` ring. `SessionOptions` bounds named trace files and
recovery tail bytes/lines. File IO errors are returned. `finish` syncs before publishing a
clean marker; Drop never implies clean completion. The next creation checks only the newest
inactive older session of that peer and atomically publishes a bounded unclean report. OS
file leases exclude live writers. Hosts own retention and diagnostic-thread scheduling.
`log::devenv` reads developer environment variables only with the explicit `devtools` feature
(default off in every profile). No simulation hash/math algorithm or content format changes.
See [the agent guide](../../book/agents/log-extras.md) for host integration and protocol checks.

`SessionOptions::with_async_writer` (ADR-8493) moves accepted records to one owned worker per
session. The caller captures the already-computed tick and a per-peer sequence at enqueue.
The queue is bounded by records and by bytes. Human lines that do not fit are counted and
dropped. A checkpoint that cannot be queued or written marks the session incomplete, and
`finish` does not publish `clean`. `flush` and `finish` drain accepted records; drop joins
the worker. Counters are visible on the existing `logs.read` target
`regia_core::log::peer_async`. The worker does not compute simulation hashes or advance time.

Unclean recovery (CT-31, ADR-8494) scrubs report fields and tail lines with `privacy::scrub_report`
before the staged directory is renamed. `session.log` and `.hash` files stay as written.
`privacy.txt` names policy `regia.recovery.privacy` at version 1 and records `sanitized`,
`complete`, invalid UTF-8, a partial tail line, and the post-scrub byte counts. The tail cap is
applied after replacement and is at most 8 MiB. Links and names other than `report.txt`,
`lastlines.log` and `privacy.txt` are refused. The report is not uploaded. The unclean ring
line carries `sanitized`, `complete` and `policy_version` for the existing `logs.read`,
`play.wait_until` and `play.expect` checks.
