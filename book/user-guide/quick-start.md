# Quick start: a game in one file

*Prerequisites: a built engine checkout ([getting started](../getting-started.md)). Reading time:
8 minutes.*

`regia::quick` is the shortest path from nothing to something on screen: one crate, one
`main.rs`, three functions. It runs on the full engine (the same renderer, input, agent protocol
and hot reload), so a quick app can grow into a full project without a rewrite
([Sketch → App → systems → project](sketch-to-project.md)).

## Create it

From the folder that holds your projects:

```sh,walkthrough=new,template=quick
regia new "Paddle Ball" --template quick
cd paddle_ball
```

Then build and run it:

```console
$ cargo run
```

The template is a small paddle game. Its `src/main.rs` is the whole game. In a script or without a
display (under `xvfb-run -a`), let it run five frames and exit:

```sh,walkthrough=smoke,template=quick
cargo run -- --exit-after-frames 5
```

A quick app is one Cargo crate with no `Regia.toml`, no `launcher/` and no assets to cook:
`regia run` and `regia package` need a project folder and stop with an error, and `regia cook` and
`regia upgrade-project` run but have nothing to do. A build you can hand to someone is the release
binary:

```sh,walkthrough=package,template=quick
cargo build --release
```

It is `target/release/paddle_ball` (or under `$CARGO_TARGET_DIR/release` when you set it), and it
runs from any folder. When you want packaging, cooked assets or Android, move to a full project
([Sketch → App → systems → project](sketch-to-project.md)).

## The three functions

```rust
use regia::quick::prelude::*;

#[derive(Reflect, Default)]
struct Model {
    ball: Vec2,
    velocity: Vec2,
}

fn main() {
    regia::quick::app(model).update(update).view(view).title("Bounce").run();
}

/// Runs once, when the app is built. Returns the state.
fn model(_q: &Quick) -> Model {
    Model { velocity: Vec2::new(200.0, 150.0), ..Model::default() }
}

/// Runs every frame: input and time in, state out.
fn update(q: &Quick, m: &mut Model) {
    let half = q.window_size() / 2.0;
    m.ball += m.velocity * q.delta();
    if m.ball.x.abs() > half.x {
        m.velocity.x = -m.velocity.x;
    }
    if m.ball.y.abs() > half.y {
        m.velocity.y = -m.velocity.y;
    }
    if q.key_pressed("Space") {
        m.velocity *= 1.2;
    }
}

/// Runs every frame after `update`: draws the state.
fn view(q: &Quick, m: &Model) {
    let draw = q.draw();
    draw.background().color(Color::BLACK);
    draw.ellipse().xy(m.ball).radius(12.0).color(Color::WHITE);
    draw.text(&format!("speed {:.0}", m.velocity.length())).y(q.window_size().y / 2.0 - 30.0);
}
```

- **`model`** builds the state. The model is a plain struct with `#[derive(Reflect)]`: the engine
  stores it as the resource `Model<YourModel>`, hot reload carries it across code edits, and agents
  can read it (`QuickModel`, see the [agent guide](../agents/quick-app.md)). Fields that cannot be
  reflected (a random generator, a cache) take `#[reflect(ignore)]` and a `Default`.
- **`update`** changes the state. It gets the model mutably and the `Quick` handle read-only.
- **`view`** draws the state. It only reads the model.
- Any of the three may be a closure: `.update(|q: &Quick, m: &mut Model| m.ball.x += 1.0)`.

No model at all? Use a sketch: `regia::quick::sketch(|q: &Quick| { /* draw */ }).run()`.

## The `Quick` handle

| Call | Gives |
|---|---|
| `q.time()`, `q.delta()`, `q.frame()` | Seconds since start, seconds since the last frame, frame number |
| `q.key_down("ArrowLeft")`, `q.key_pressed("Space")`, `q.key_released(..)` | Keys by physical name (`KeyW`, `Space`, `ArrowUp`, `Enter`, `Digit1`, `ShiftLeft`, …) |
| `q.mouse()`, `q.mouse_down(MouseButton::Left)`, `q.mouse_delta()`, `q.mouse_wheel()` | The cursor in drawing coordinates, buttons, raw motion, wheel lines |
| `q.touches()` | Fingers in drawing coordinates with their phase (Android, touch screens) |
| `q.window_rect()`, `q.window_size()` | The window in drawing coordinates (center origin) |
| `q.draw()` | 2D drawing over the frame: `rect`, `ellipse`, `line`, `polyline`, `polygon`, `tri`, `quad`, `arrow`, `mesh`, `text` ([quick draw](../agents/quick-draw.md)) |
| `q.draw_3d()`, `q.camera_3d(eye, target)` | Lit 3D primitives (`cube`, `sphere`, `cylinder`, `cone`, `plane`, `grid`, `line`, `cube_wires`) and the camera that looks at them |
| `q.grab_cursor(true)` | Lock and hide the cursor for mouse look |
| `q.save_frame("shots/frame.png")` | Save the next presented frame as a PNG |
| `q.load_bytes("levels/one.txt")` | Read a file through the VFS on the IO pool; poll the request in later frames (works for Android APK assets too) |
| `q.set_run_mode(..)`, `q.quit()` | Change the loop, exit |
| `q.commands(f)` | Anything else: `f` gets `Commands` to spawn entities or insert resources |

**Coordinates.** Drawing uses logical units with the origin at the window center, +X right and +Y
up. `q.mouse()` and `q.touches()` are already converted; `q.screen_to_world(..)` and
`q.world_to_screen(..)` convert raw window pixels.

## Run modes

| Mode | Builder | `update`/`view` run |
|---|---|---|
| `RunMode::Loop` (default) | — | Every frame |
| `RunMode::Wait` | `.wait()` | On the first frame and on frames with input or a resize; the last drawing stays on screen in between and the loop idles at 30 fps |
| `RunMode::LoopNTimes(n)` | `.loop_once()`, `.loop_ntimes(n)` | `n` times, then the drawing is frozen on screen and the loop idles at 10 fps; the window stays closable |

`LoopNTimes` suits generated images: draw once, `q.save_frame(..)`, look at it.

## Examples

`examples/quick/` has nine single-file games and demos (ported from macroquad's examples):

| Example | Shows |
|---|---|
| `basic_shapes` | A sketch: line, rectangle, circle, text |
| `input_touch` | Touches by phase (mouse on desktop) |
| `snake` | Grid game, key steering, restart |
| `arkanoid` | A playfield in its own units scaled to the window |
| `asteroids` | Frame-rate-independent motion, wrap-around, outlines |
| `life` | Conway's Game of Life on a cell grid |
| `platformer` | Tile collision with pixel-stepped movement and a moving platform |
| `first_person` | `draw_3d`, `camera_3d`, mouse look, a 2D HUD over 3D |
| `destruction` | A brick wall and a plank bridge broken by a hit script (run with `--features quick`) |

Run one with `cargo run -p regia --example snake`.
