# Headless deterministic capture

Run any Regia Engine app **offscreen** (no window, no compositor) with a fixed timestep, step exact
frames through the agent protocol, then screenshot. Two lavapipe runs of the same script produce
**byte-identical** PNGs — the path DO27 and CI use for goldens.

## Setup

```rust
use regia::prelude::*;
use regia_app::enable_fixed_step;
use regia_render::RenderPlugin;

fn main() {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ));
    enable_fixed_step(&mut app, 60.0); // ManualDuration + one FixedUpdate per App::update
}
```

- `RenderPlugin { headless: true }` skips surfaces but still runs `extract_frame`, so
  `seeing.screenshot` with `target: "offscreen"` draws the extracted runtime frame
  (`MeshRenderer` + `Camera` via `RuntimeRenderer::render_offscreen`). There is no window and
  no compositor.
- `enable_fixed_step(app, hz)` pins `TimeUpdateStrategy::ManualDuration` and `FixedTime` so wall
  clock never enters the sim. Start the app paused (`play.pause`) so the connection delay does
  not advance the clock before the first `play.step`.

## Agent script

For offscreen capture, `seeing.screenshot` accepts `target`, `width`, `height`, `window_layers`,
`hud`, `layout`, and `calibration`. Width and height default to 256×144 and must each be in
1..=4096; layout is `auto`, `landscape`, or `portrait`; calibration is `applied` or `neutral`.
Invalid values return `F0002`; unsupported view modes or overlays return `F0013`; failure to
create the headless renderer returns `F0011`.

```json
play.pause {}
play.step { "frames": 8 }
seeing.screenshot {
  "target": "offscreen",
  "width": 64,
  "height": 64,
  "timeout_ms": 30000
}
→ { "width": 64, "height": 64, "mime": "image/png", "png_base64": "..." }
```

On lavapipe, a zero-sized request fails parameter validation before rendering:

```json
{"jsonrpc":"2.0","id":10,"method":"seeing.screenshot","params":{"target":"offscreen","width":0,"height":64}}
→ {"jsonrpc":"2.0","id":10,"error":{"code":-32602,"message":"`width`/`height` must be in 1..=4096 for target \"offscreen\"","data":{"code":"F0002","hint":"check the method's parameters in the agent guide"}}}
```

Offscreen capture needs a `Camera` and at least one `MeshRenderer`. It answers immediately
(no present wait). A mesh that rotates in `FixedUpdate` makes frame N differ from frame N+10;
two runs of the same step counts are byte-identical on lavapipe. Pin `Camera.exposure_ev` and
turn auto-exposure off so the exposure does not drift between runs.

## Determinism checklist

Workflow: start paused with a fixed timestep, add a camera and mesh, step the chosen frame count,
capture offscreen, and repeat from a fresh app with the same inputs. Compare PNG bytes; then
change the step count and confirm the moving object changes the image.

1. Use lavapipe for goldens (`VK_LOADER_DRIVERS_SELECT=*lvp*` — already set in this repo’s cargo
   env). Never bless a golden on the real GPU.
2. Call `enable_fixed_step` before the first `App::update`.
3. Pin exposure (`Camera.exposure_ev` and `PostSettings::golden_deterministic()`). Auto
   exposure adapts and breaks byte identity.
4. Disable film grain, TAA, and bloom for goldens.
5. Re-run the same script twice; PNG bytes must match. The agent test
   `two_headless_runs_match_and_the_clock_moves_the_mesh` renders a mesh that spins with the fixed
   clock: the frame is not blank, frame N differs from frame N+10, and both runs match.

See also [seeing](seeing.md) and [testing](testing.md).
