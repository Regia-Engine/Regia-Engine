# Sketch → App → systems → project

*Prerequisites: [quick start](quick-start.md). Reading time: 7 minutes.*

A Regia Engine game can start as ten lines and grow into a full project. There are four rungs, and each
one keeps the code of the rung before it. Move up when the current rung starts to hurt, not before.

| Rung | You write | Move up when |
|---|---|---|
| **Sketch** | One `view` function | You need state that lasts between frames |
| **App** | `model`, `update`, `view` | `update` is long and does unrelated things, or you want engine features (physics, audio, scenes) |
| **Systems** | Ordinary ECS systems, still using `Quick` | You want levels made in the editor, several crates, Android packaging, or a team |
| **Project** | A `GamePlugin` in a project from `regia new` | — |

## 1. Sketch

A drawing that reacts to time and input, with no state of its own:

```rust
use regia::quick::prelude::*;

fn main() {
    regia::quick::sketch(|q: &Quick| {
        let draw = q.draw();
        draw.background().color(Color::BLACK);
        draw.ellipse().xy(q.mouse()).radius(20.0 + 10.0 * q.time().sin()).color(Color::WHITE);
    })
    .run();
}
```

## 2. App

State goes into a reflected model; `update` changes it and `view` draws it:

```rust
use regia::quick::prelude::*;

#[derive(Reflect, Default)]
struct Trail {
    points: Vec<Vec2>,
}

fn main() {
    regia::quick::app(|_q: &Quick| Trail::default())
        .update(|q: &Quick, t: &mut Trail| {
            t.points.push(q.mouse());
            if t.points.len() > 120 {
                t.points.remove(0);
            }
        })
        .view(|q: &Quick, t: &Trail| {
            q.draw().background().color(Color::BLACK);
            q.draw().polyline().weight(3.0).color(Color::WHITE).points(t.points.iter().copied());
        })
        .run();
}
```

The model is the resource `regia::quick::Model<Trail>`, so it is visible to the inspector and
agents and survives hot reload.

## 3. Systems

`Quick` is an ordinary system parameter. The functions you wrote for the app can be called from
normal systems, and normal systems can sit next to the quick app's `update` and `view`. Split
`update` into systems when it does several unrelated jobs:

```rust
use regia::quick::prelude::*;

#[derive(Reflect, Default)]
struct Game {
    score: u32,
    paused: bool,
}

/// The same signature as a quick `update`: usable from the app and from a system.
fn tick_score(q: &Quick, game: &mut Game) {
    if !game.paused && q.key_pressed("Space") {
        game.score += 1;
    }
}

/// A plain system that reads the quick model and runs after the quick app's chain.
fn pause_on_p(q: Quick, mut game: ResMut<Model<Game>>) {
    if q.key_pressed("KeyP") {
        game.model.paused = !game.model.paused;
    }
}

fn main() {
    regia::quick::app(|_q: &Quick| Game::default())
        .update(tick_score)
        .view(|q: &Quick, g: &Game| {
            q.draw().text(&format!("score {}", g.score));
        })
        .run();
}

/// To add the extra system, build the app yourself instead of calling `.run()`.
fn main_with_systems() {
    regia::run_with(regia::quick::QuickEngine, |app| {
        app.add_plugins(regia::quick::app(|_q: &Quick| Game::default()).update(tick_score))
            .add_systems(Update, pause_on_p.after(QuickSet));
    });
}
```

- The quick app's own systems are in `QuickSet` (in `Update`): plan the frame → `update` → `view`.
  Order your systems against it.
- `QuickEngine` adds what `.run()` adds around a quick app: the input layer and the quick draw layer.
- A test can drive the same function through a quick app and through a plain system and compare
  the results (`crates/regia_quick/tests/quick_in_system.rs`).

## 4. Project

When you want the editor, levels, hot reload while playing, several crates or an Android build,
create a project and move the code into its game crate:

```console
$ regia new "My Game" --template blank
```

Register the quick app from the game plugin. Everything else (editor play, hot reload, the Android
shell) loads the plugin the same way:

```rust
use regia::quick::prelude::*;
use regia_framework::game::GamePlugin;

#[derive(Reflect, Default)]
struct Game {
    score: u32,
}

pub struct MyGame;

impl GamePlugin for MyGame {
    fn register(&self, app: &mut App) {
        app.add_plugins(
            regia::quick::app(|_q: &Quick| Game::default())
                .update(|q: &Quick, g: &mut Game| g.score += u32::from(q.key_pressed("Space")))
                .view(|q: &Quick, g: &Game| {
                    q.draw().text(&format!("score {}", g.score));
                }),
        );
    }
}
```

Export it with `regia_framework::export_game!(MyGame);` in the game crate (see
[hot reload](hot-reload.md)). On a reload the model is snapshotted, the game library is swapped, the
builder runs `model` again, and the snapshot is applied over it, so the state carries over and the
new `update`/`view` code runs. A field you add to the model starts at its type's `Default`.

From here, entities, scenes, physics and the rest of the engine are available next to the quick
app. The quick app can shrink over time as systems take over its jobs.
