# Running and iterating

How to run a Regia Engine game during development, read its logs, configure the window, iterate
quickly, try it on Android and turn it into a build you can hand to someone.

## The development loop

```console
$ regia run            # cook, build the launcher and run it
$ regia run --dev      # the same, re-cooking saved assets and reloading them while it runs
$ cargo run -p my_game_launcher   # just build and run (cook with `regia cook` first)
```

`regia run` and `regia run --dev` need the `launcher/` crate (`<name>_launcher`) that the `blank`,
`first_person`, `third_person` and `top_down` templates have; in a [quick app](../user-guide/quick-start.md)
use plain `cargo build` and `cargo run`, and in a `net_game` project plain `cargo build`, then
`cargo run -p <name>_server` and `cargo run -p <name>_client` as its `README.md` says. Run in either kind of
project, `regia run` says so and prints what to run instead. To cook without running (after you
change assets, before a `cargo run`), from the project folder:

```sh,walkthrough=cook
regia cook
```

`regia run` takes `--level PATH` to start in another level, `--release` to build with the release
profile and `--project DIR` (default `.`), and passes everything after `--` to the game ([CLI reference](../reference/cli.md)). A development run reads assets from the project's dev cook cache (`.regia/cooked/<platform>/`)
and raw files from `assets/`. The asset server finds the project by looking for `Regia.toml`
above the working directory (or `$REGIA_PROJECT`), so run from the project root or set that
variable. `cargo build --release` gives you representative frame rates. The `quick` and `net-game`
templates build their dependencies at `opt-level = 2` in the `dev` profile so debug runs stay playable;
the other templates use Cargo's default `dev` profile, so add a `[profile.dev.package."*"]` section to
the project's `Cargo.toml` if debug runs are too slow.

Three ways to iterate faster than restart-and-rebuild:

- **Asset changes**: re-cook the changed source (`regia cook`, or Reimport in the editor's
  Content Browser). Processes built with `regia_asset`'s `dev-assets` feature, the editor
  among them, watch the cook cache and reload the changed mesh, texture or material in place
  ([Assets and the cook](../fundamentals/assets.md)).
- **Code changes**: open the project in the editor with `--game <crate>` and rebuild the game
  crate; the editor swaps it in and keeps the world state
  ([Hot reload](../user-guide/hot-reload.md)).
- **Agent checks**: script `regia agent call` to set up a situation, step frames and take a
  screenshot, instead of playing to it by hand ([Agent protocol and MCP](../user-guide/agent-protocol.md)).

## Checking that it starts

A script or a CI job cannot close the window by hand. Every game `regia new` writes accepts
`--exit-after-frames N`: it renders N frames, then exits with the game's exit code. From the project
folder:

```sh,walkthrough=smoke
regia run -- --exit-after-frames 5
```

Without a display (a server, a CI runner), start it under `xvfb-run -a`. The packaged binary
accepts the same flag.

## Logs

`regia::run` installs `tracing` logging to the console, to a session log file, and on Android to
logcat. Filter with the `REGIA_LOG` environment variable (the same syntax as `RUST_LOG`):

```console
$ REGIA_LOG=info,regia_render=debug,my_game=trace cargo run -p my_game_launcher
```

The default is `info` for the engine with noisy GPU crates at `warn`. Log from your own code with
the `tracing` macros the prelude re-exports (`info!`, `warn!`, `error!`, `debug!`, `trace!`).

Session logs go to `logs://`: `<data dir>/RegiaEngine/<exe name>/logs` on desktop (for example
`~/.local/share/RegiaEngine/my_game_launcher/logs` on Linux, `%APPDATA%\RegiaEngine\…` on Windows) and the
app's internal storage on Android. The agent protocol's `logs.read` returns the same lines.

When a game panics, the platform writes a crash report next to the logs
(`logs://crash-<timestamp>.txt`): the panic message and backtrace, build identity, OS and GPU, the
active level, the frame count and the last 200 log lines ([Packaging](../user-guide/packaging.md)
covers crash upload and symbols).

## Environment variables

Every engine environment variable in this book is named `REGIA_*`. The engine still reads each
one under its name from before the rename (`FERRITE_*`) when the `REGIA_*` one is unset, and logs <!-- rename-keep -->
a one-time warning that names the replacement; set the `REGIA_*` names in new scripts
([Renaming to Regia Engine](../user-guide/rename-regia.md)).

## Configuring the app before it runs

`regia::run(game)` is `regia::run_with(game, |_| {})`. Use `run_with` to change resources
before the first frame, for example the window:

```rust
use regia::prelude::*;

struct Game;

impl Plugin for Game {
    fn build(&self, _app: &mut App) {}
}

fn main() {
    let _exit = regia::run_with(Game, |app| {
        app.insert_resource(WindowSettings {
            window: Window {
                title: "My Game".into(),
                logical_size: (1600.0, 900.0),
                ..Default::default()
            },
        });
    });
}
```

`Window` also carries the fullscreen mode (`WindowMode`), the present mode (`PresentMode`:
`Vsync`, `VsyncRelaxed` or `Immediate`), resizability, decorations, cursor visibility and grab, and IME. The
player-facing settings (graphics preset, resolution, audio volumes, language) live in
`GameSettings`, which the framework loads from `user://settings.ron` and applies live
([Gameplay framework](../user-guide/gameplay.md)).

`run_with` (and `run`) returns the `AppExit` value. A system ends the app by writing
`AppExit::Success` (or `AppExit::Error(code)`) through a `MessageWriter<AppExit>`. The launcher
`regia new` generates ignores the value, so its process exits with 0 either way; when a CI job or
a script must see a failure, make the launcher return it:

<!-- ignore-reason: `my_game::Game` is the reader's own crate -->
```rust,ignore
use regia::app::AppExit;

fn main() -> std::process::ExitCode {
    match regia::run(my_game::Game) {
        AppExit::Success => std::process::ExitCode::SUCCESS,
        AppExit::Error(code) => std::process::ExitCode::from(code.get()),
    }
}
```

## Command-line flags of the samples

The engine's samples share a few flags that are convenient for automated runs; your launcher can
copy the pattern (they are ordinary argument parsing in each sample's `main.rs`):

| Flag | Samples | Effect |
|---|---|---|
| `--exit-after-frames N` | most samples, and every project `regia new` writes | Quit after N frames (smoke tests, soak runs) |
| `--screenshot PATH` | `third_person` | Write a PNG a few frames before exiting |
| `--seed N`, `--bots N`, `--no-local-player`, `--bench` | `arena` | Deterministic matches and the bot benchmark |

## Running on Android

```console
$ regia install-android --project my_game
```

`install-android` packages a debug APK, installs it with `adb`, launches it and streams the app's
logcat (`--device SERIAL` picks one of several devices, `--no-build` reinstalls the last package,
`--no-logcat` returns after launch). The game's `android/` crate is the `cdylib` entry point; it
calls `regia::android_main!`, which runs the same `Game` plugin. Android drops the window
surface when the app is paused and recreates it on resume; the engine handles this, and your
game sees `LifecycleEvent::Suspended` / `Resumed` messages if it wants to pause audio or
gameplay ([App, plugins and schedules](../fundamentals/app-and-plugins.md)).

To check that the Android crate compiles without a device or the Gradle tools (the
[Android setup](install.md#5-android-optional) without step 5 is enough), from the `my_game`
project folder:

```sh,walkthrough=android,template=blank
cargo ndk -t arm64-v8a check -p my_game_android
```

## Making a build

```console
$ regia package --platform linux
$ regia package --platform windows
$ regia package --platform android --format apk
```

Packaging cooks the assets into `.fpak` archives, builds the launcher, and writes a
self-contained archive under `dist/<platform>/`. The default profile is `dev`; `--profile shipping`
builds the optimized, stripped release with separate debug symbols. The agent-protocol server is
part of the `regia` crate's default `agent` feature, so it is compiled into shipping builds too
unless every crate of the build depends on `regia` with `default-features = false` (keep `"ui"`
if you use the runtime UI). It listens on loopback only and requires the per-run token. Details, signing
and Steam: [Packaging](../user-guide/packaging.md) and [Steam](../user-guide/steam.md).

## After an engine update

Projects record the engine release line they target (`Regia.toml [project] engine`). After you
build the tools of a newer engine release, run this in each project folder:

```sh,walkthrough=upgrade
regia upgrade-project
```

It lists what would change: content-format migrations, the engine dependency paths and the release
line. Nothing is written until you add `--write` (it keeps a backup of every file it rewrites); on a
project that is already current it changes nothing. `--engine path:<engine folder>` moves the
project to another engine folder ([Distribution and upgrades](../agents/distribution-upgrade.md)).
