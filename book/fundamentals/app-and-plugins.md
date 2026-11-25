# App, plugins and schedules

<!-- index: Plugin -->
<!-- index: Schedule -->
<!-- index: Fixed timestep | see: Schedule -->
<!-- index: DefaultPlugins | see: Plugin -->

An `App` owns one ECS world, the frame schedules and the plugins that configured them.
Spec: `docs/specs/10-core.md`. A runner
drives it: the platform's window runner for games, `run_headless` for tests, servers and tools.
Everything is configured through **plugins**, including the engine itself.

## Plugins

A plugin is a type that implements `Plugin`. `build` adds systems, resources, messages, types and
other plugins to the app:

```rust
use regia::prelude::*;

/// Everything about the player.
pub struct PlayerPlugin;

#[derive(Resource, Default)]
struct PlayerStats {
    jumps: u32,
}

impl Plugin for PlayerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PlayerStats>()
            .add_systems(Startup, spawn_player)
            .add_systems(Update, (read_input, move_player).chain());
    }
}

fn spawn_player(mut commands: Commands) {}
fn read_input() {}
fn move_player() {}

/// The game: a plugin made of plugins.
pub struct Game;

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        app.add_plugins((regia_scene::ScenePlugin, PlayerPlugin));
    }
}
```

Split a game into one plugin per feature, the way the engine is split. `add_plugins` takes one
plugin or a tuple. A plugin can also implement `finish` (runs once every plugin has been built)
and `cleanup`. `App::has_plugin` asks whether one was added, by name.

## `regia::run` and `DefaultPlugins`

`regia::run(game)` does four things: starts logging, creates an `App`, adds `DefaultPlugins` and
then your game plugin, and hands the app to the platform runner. `DefaultPlugins` contains:

| Plugin | Gives you |
|---|---|
| `DiagnosticsPlugin` | Frame timing and the process frame profiler (`Profiler` resource) |
| `PlatformPlugin` | The window, the lifecycle, raw input messages, the VFS, platform services (clipboard, haptics, safe area, locale) |
| `RenderPlugin` | The renderer: cameras, meshes, lights, post, shadows, atmosphere |
| `AssetPlugin` | The `AssetServer`: dev cook cache in development, `.fpak` archives in a package |
| `AudioPlugin` | Audio output: sounds, music, buses, the default output device (`REGIA_AUDIO=off` runs it headless) |
| `TelemetryPlugin`, `ResidencyPlugin` | Structured telemetry (`REGIA_TELEMETRY`) and keeping frame-critical data paged in |
| `UiPlugin` (feature `ui`, on by default) | The runtime UI |
| `AgentPlugin` (feature `agent`, on by default) | The agent-protocol server |

It also adds framework helpers that stay idle until a level uses them (start level, baked GI and
lightmap installs, world streaming, exposure zones, content chunks, the performance overlay,
captions and conversations).

Everything else is opt-in, because games differ in what they need. A typical game adds some of:

| Plugin | Crate | Chapter |
|---|---|---|
| `ScenePlugin` | `regia_scene` | [Scenes](../user-guide/scenes.md) (transform propagation, levels, prefabs) |
| `InputPlugin::new(config)` | `regia_input` | [Input](../user-guide/input.md) |
| `PhysicsPlugin` | `regia_physics` | [Physics](../user-guide/physics.md) |
| `AnimationPlugin` | `regia_animation` | [Animation](../user-guide/animation.md) |
| `VfxPlugin` | `regia_vfx` | [VFX and decals](../systems/vfx.md) |
| `NavPlugin`, `AiPlugin` | `regia_nav`, `regia_ai` | [Navigation and AI](../user-guide/navigation-ai.md) |
| `CharacterPlugin`, `CameraPlugin`, `PlayerPlugin`, `RulesPlugin`, `ScriptPlugin`, `SavePlugin`, `SettingsPlugin`, `TimerPlugin`, `TweenPlugin`, `SimBridgePlugin` | `regia_framework` | [Gameplay framework](../user-guide/gameplay.md) |

## The frame

Each frame runs the schedules in this order:

| Schedule | Runs | Use it for |
|---|---|---|
| `StateTransition` | once, before `Startup` on the first frame | applies the initial states, so every `OnEnter` for them runs before `Startup` |
| `Startup` | once, after that first `StateTransition` | spawning the initial world |
| `First` | every frame | clocks, frame counters, message buffers (engine) |
| `PreUpdate` | every frame | platform events → input state, asset completions, hot-reload swaps (engine) |
| `StateTransition` | every frame, between `PreUpdate` and `FixedUpdate` | applies `NextState` values written earlier in the frame, runs `OnExit`/`OnEnter` |
| `FixedUpdate` | 0…N times per frame at `FixedTime::hz` | simulation that must not depend on frame rate: physics, AI, match logic |
| `Update` | every frame | most gameplay |
| `PostUpdate` | every frame | late scripts, physics interpolation, animation, transform propagation, visibility, audio and UI layout, in that order (`PostUpdateSet`) |
| `Extract` | every frame | copying render data out of the world (engine) |
| `Render` | every frame | render graph and present (engine) |
| `Last` | every frame | diagnostics, frame-end cleanup |

Two more run outside the frame: `Lifecycle`, synchronously on every application lifecycle
transition (for work that must finish before the OS callback returns), and `OnLevelLoaded` /
`OnLevelUnloading` when the scene system loads or unloads a level.

Systems that touch an OS window must stay on the main thread. The window setters winit exposes
(`set_title`, `request_inner_size`, `set_cursor_grab`, …) post to the event-loop thread and block
when called from a task-pool thread; on Windows that deadlocks the frame loop, because the main
thread is inside `App::update` waiting for the very system that is waiting on it. Add a
`NonSendMarker` system parameter to pin such a system to the main thread:

```rust
use regia::prelude::*;

/// `PostUpdate`: applies changed `Window` components to the OS window.
fn apply_window_changes(
    _main: NonSendMarker, // forces this system onto the main thread
    windows: Query<&Window>,
) {
    // …call os.set_title(…), os.set_cursor_grab(…), etc.
}
```

(`NonSendMarker` is a `!Send` marker. `CursorGrab::Locked` falls back to `Confined` on Windows
and on X11, where winit does not support `Locked`. `CursorGrab::Confined` falls back to `Locked`
on macOS. A grab the OS rejects is retried on a later frame.)

Anything that writes `Transform` should run before `PostUpdateSet::TransformPropagate` (anything in
`Update` does) so the renderer sees the new position in the same frame.

## Time and the fixed timestep

`Res<Time>` is the game clock: `delta_secs()`, `elapsed_secs()`, `frame_count()`, plus `paused`
and `time_scale` fields that the game, the editor and the agent protocol's `play.pause` /
`play.set_time_scale` use. `RealTime` is the unscaled wall clock.

`FixedUpdate` runs at `FixedTime::hz` (default 60), as many times per frame as the accumulated
time requires, at most `max_substeps` (default 5, hard cap `MAX_SUBSTEPS_LIMIT` = 64) so a slow frame cannot
spiral. Inside `FixedUpdate`, `Time` is swapped for the step: `Time::delta` and `Time::sim_delta`
are one step, and `Time::elapsed` and `Time::sim_elapsed` are the fixed clock; the frame's values
are restored after the last step. `FixedTime::step()` gives the same length. `FixedTime::overstep_fraction()` is how far the clock is
into the next step, for interpolating what you draw.

```rust
use regia::prelude::*;

fn configure(app: &mut App) {
    // A 120 Hz simulation.
    let mut fixed = FixedTime::default();
    fixed.hz = 120.0;
    app.insert_resource(fixed);
}

/// A fixed-step system: integrate with the step length, not the frame delta.
fn simulate(fixed: Res<FixedTime>) {
    let dt = fixed.step().as_secs_f32();
    let _ = dt;
}
```

For tests and deterministic captures, `enable_fixed_step(app, hz)` pins
`TimeUpdateStrategy::ManualDuration` to one step at `hz` (`hz` sanitized to `1..=1000000000`;
`0` becomes 1 Hz and a non-finite value becomes 60) and sets `FixedTime.max_substeps` to 1, so
the frame does not follow the wall clock. One `App::update` runs one `FixedUpdate` when the
simulation delta is at least one step: `time_scale` is 1, no shorter `SimStep` is installed, and
the app is not paused or suspended. `Time::advance` multiplies that manual delta by `time_scale`.
A scale below 1 banks time and a step runs only once the bank reaches `1/hz`. Scale `0.5` at 1 Hz
runs a step on every second update, because a half step is an exact number of nanoseconds. Scale
`0` runs none. At 60 Hz the stored step is 16,666,667 ns and half of that frame is 8,333,333 ns,
so the first step lands on the third update. A scale above 1 still runs at most one
step and drops the rest. Pause and suspend are a zero gameplay delta, so no `FixedUpdate` runs.
That pinning is how two runs on the same machine produce byte-identical screenshots. The manual
duration is not clamped to `MAX_DELTA`; that clamp is only for the wall clock (`RealTime::tick`).
`Time.paused` is a zero gameplay delta, not a schedule gate: `Update` still runs every frame, so
gate gameplay systems on `Time::delta()` or a run condition, and UI systems keep running. `paused`
changes only when the game sets it; suspend does not write `paused`. The agent protocol's
`play.step` does not use `enable_fixed_step`: it unpauses `Time` for the requested number of
frames and pauses again.

Frame-rate caps live in `FramePacing { target_fps }`; render profiles can install one (the Steam
Deck profile caps at 40 fps).

## Headless apps

Tools, servers and tests run the same app without a window:

```rust
use regia::prelude::*;

#[derive(Resource, Default)]
struct Ticks(u32);

fn tick(mut ticks: ResMut<Ticks>, mut exit: MessageWriter<AppExit>) {
    ticks.0 += 1;
    if ticks.0 == 10 {
        exit.write(AppExit::Success);
    }
}

fn main() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .init_resource::<Ticks>()
        .add_systems(Update, tick);
    let _exit = regia::app::run_headless(app);
}
```

In a test you can also call `app.update()` yourself, one frame at a time, and inspect
`app.world()` between frames. The renderer has an offscreen mode for the same purpose
(`RenderPlugin { headless: true }`, see [Rendering](../user-guide/rendering.md)).

## Platform lifecycle

On Android the OS takes the window away when the app goes to the background and gives it back on
resume. The engine drops the GPU surface on `Suspended` and recreates it on `Resumed`; the device
and every GPU resource survive. Games see the transitions as `LifecycleEvent` messages
(`Started`, `Suspended`, `Resumed`, `LowMemory`, …) and as the `AppLifecycle` resource:

```rust
use regia::prelude::*;

fn pause_on_suspend(mut events: MessageReader<LifecycleEvent>, mut time: ResMut<Time>) {
    for event in events.read() {
        match event {
            LifecycleEvent::Suspended => time.paused = true,
            LifecycleEvent::Resumed => time.paused = false,
            _ => {}
        }
    }
}
```

On desktop the window stays alive while minimized: the runner throttles frames and the renderer
skips presenting until the window is visible again, so a desktop game sees `Started` and
`Resumed` once and nothing more. Code written against the messages works on every platform.
