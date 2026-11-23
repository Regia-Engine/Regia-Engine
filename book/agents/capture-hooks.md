# Capture hooks

Two ways to make repeatable frames of a configured product, both at raster quality, with no path tracing and no
stream. ADR-2757.

- **Scripted capture of the player:** the web page or the Android app on a fixed clock, stepped frame by frame. It
  shows the real UI on the Android app. Use it for videos about the viewer.
- **A render job with a product:** a `.render_job.ron` that gives a build, a camera path and timed picks. It writes clean
  frames with no UI, through the render-out's takes, writers and manifest. Use it for videos of the piece.

Both draw through the same player and the same shaders, so they show what a customer sees. Frame hashes repeat on one
adapter and driver, not across them: compare a run with a run on the same machine.

## Deterministic mode in a page

```js
viewer.setDeterministic(true);                        // no browser frames; the script's clock starts at 0
viewer.setView({ tilt: [0, 0], zoom: 1, animate: false });
viewer.step(1000 / 60, true);                         // one frame of 16.67 ms; true: draw even if nothing changed
const first = viewer.frameHash();                     // read in the same task
viewer.setPick("Material", "Titanium");
viewer.setView({ tilt: [12, -7], zoom: 1.3 });        // animated: the damping runs on the steps you give it
for (let i = 0; i < 30; i++) {
  viewer.step(1000 / 60, true);
  const png = canvas.toDataURL("image/png");
}
```

| Call | What it does |
|---|---|
| `setDeterministic(on)` | While on, frames come only from `step`. Turning it on starts the script's clock at 0; repeating a state changes nothing. |
| `step(dtMs, force?)` | One frame of `dtMs`. Returns whether it drew. Without `force` it draws only if something moved or changed, as the lazy loop would. Refused with `bad_argument` outside the mode, or for a negative or non-finite time. |
| `frameHash()` | FNV-1a 64 of the canvas's RGBA bytes, 16 hex digits: the render-out manifest's `frame_hashes`. |
| `stats()` | `scriptFrame` and `scriptClockMs` are the script's clock. `lastSwitchMs` reads it in this mode. |

Read the canvas in the task that called `step`: a browser keeps a canvas's picture only until that task ends. Pointer events keep working and take the script's clock as their time, so a drag dispatched by a script flings the same way every run. A step longer than 100 ms is clamped by the view's damping, as in the live loop.

To keep runs equal, give the canvas a fixed CSS size and `maxPixelRatio`, and start each run from a known view with `animate: false`.

## Deterministic mode on the Android app

The `viewer_app.*` calls exist in a dev build with the feature `agent` ([Product viewer Android app](product-viewer.md)).
While the mode is on, the panel and the picture move only in `viewer_app.step`, on the script's clock. How many app
frames run between two steps changes nothing. The panel is part of the frame, as it is on the screen.

These are sample-host extension requests, outside the runtime schema checked by `agent-examples`.
`product_viewer`'s `player_host` tests exercise the capture handlers.

```json,ignore
{ "method": "viewer_app.set_deterministic", "params": { "on": true } }
{ "method": "viewer_app.set_view", "params": { "tilt": [0, 0], "zoom": 1, "animate": false } }
{ "method": "viewer_app.step", "params": { "dt_ms": 16.667, "force": true, "capture": "png" } }
```

`viewer_app.step` answers `{drew, frame, clock_ms}`. With `capture` and a frame drawn it adds `width`, `height` and
`hash` (the same hash), and for `"png"` also `png_base64`. With the mode off, `viewer_app.step` is refused with `bad_argument`. With the
surface gone (the app in the background) it still draws the capture when one is asked for.

## A render job with a product

```ron,ignore
(
    version: 4,
    name: "swap",
    out: "renders/swap",
    output: (width: Some(1080), height: Some(1920), fps: Some(30), supersample: Some(2)),
    takes: [(name: "all", range: Seconds(start: 0.0, end: 6.0))],
    product: Some((
        package: "products/piece-a",
        build: Some("capture/piece-a.build.json"),
        options: [
            (at: Seconds(2.0), group: "Material", value: One("14K Rose Gold")),
            (at: Marker("swap"), group: "Finish", value: Many(["Satin"]), adjust: true),
        ],
        view: [
            (at: Seconds(1.0), tilt: Some((10.0, -6.0)), zoom: Some(1.2)),
            (at: Seconds(5.0), tilt: Some((-8.0, 4.0)), ease: Some("linear")),
        ],
    )),
)
```

The full field list is in `docs/formats/render-job-ron.md`.

| Run it | How |
|---|---|
| With no app | `regia render --job swap.render_job.ron --project DIR`. Prints the manifest path. Exit 0 done, 2 refused, 3 failed (or no GPU adapter), 4 cancelled. `--dry-run` prints the plan. |
| In a running app | `render.out_job { file \| job, date?, dry_run? }`, then `render.out_progress` and `render.out_cancel`. The app needs no game renderer. |

- **`package`** is a `regia.viewer_package/1` folder ([Product viewer packages](viewer-pack.md)), checked when it opens.
- **`build`** is a `jewelry.build/1` file: frame 0's picks and view. Without it the product starts on its first sold
  picks and its default view. It must be this product's and still sold.
- **`options`** is the option-change track. A pick applies before its frame draws, as `setPick` would. `adjust: true` is
  `setPick`'s `adjust`.
- **`view`** is a camera path in `setView`'s terms: what a key leaves out stays at the key before it. The view eases from
  the start view into the first key and holds after the last. `ease` is `smooth` (the default), `linear` or `hold`.
- **`shots`** in the job is the other way to give a camera path: a `.shots.ron` ([Camera paths](camera-paths.md)) whose
  numbers read as mm in the piece's frame. Its fades and depth of field are not drawn, and the plan warns
  `product_shot_effects_not_applied`. Its shot boundaries are the take's cut list.
- **`at`** is `Frame(n)`, `Seconds(t)` (the first frame at or after it) or `Marker(name)` of the job's `sequence`.
- **`Whole`** with no `shots` and no `sequence` runs up to the last timed event.
- **`supersample`** renders larger and averages down. The limits stay those of the delivered size.

### A job is refused before its first frame

The whole timeline is walked with no GPU, including with `dry_run`, so nothing is rendered for a job a player would refuse
part way through. `view_range` is binding: unlike `setView`, a job does not clamp, since a silently moved shot is worse
than a refusal.

| Code | When |
|---|---|
| `render_job_bad_package` | The package folder cannot be read, fails verification or does not open. |
| `render_job_build_refused` | The build is not valid JSON, is another product's, or is no longer sold. |
| `render_job_pick_refused` | A timed pick would be refused on its frame; the message names the frame, group, value and the player's code. |
| `render_job_outside_view_range` | A frame's camera is outside the recipe's limits: tilt, spin, pan, or closer than `max_px_per_mm` at the output size. |
| `render_job_invalid` | The job sets what a raster viewer frame cannot do: `subframes` above 1, `bit_depth` 16, `format: "exr"`, `aovs`, `audio`, `time_map`, `warmup_frames`, `settle_frames`, `bindings`; or both `shots` and `product.view`. |

The rest of the codes are on [Render jobs](render-jobs.md).

Warnings in the plan: `product_option_past_end` (a pick at or past the end is dropped), `product_build_recipe_changed` and
`product_build_snapshot_changed` (the build was saved against another recipe or snapshot; it still resolves),
`product_look_problems` (a zone has no usable library entry and draws a neutral look; `product.problems` lists them).

### The manifest

`schema` is `jewelry.render_out/1`, and `extends` names `regia.render_out/2`, whose fields it keeps (`frame_hashes`,
`files`, `frame_times`, `job` with the takes and their cut lists). It adds `product`:

| Field | Meaning |
|---|---|
| `renderer_used`, `quality` | `regia_viewer`, `raster` |
| `adapter`, `backend` | Where the frames were drawn; hashes repeat on the same one |
| `package_hash` | The package's hash |
| `identity` | `house`, `tenant_id`, `product_key`, `design_key`, `geometry_key`, `snapshot_id`, `recipe`, `start_build_hash`, `start_picks`, `start_view` |
| `meshes` | Every mesh the package holds |
| `problems` | What fell back to a neutral look |
| `option_changes` | Per timed pick: `frame`, `time_s`, `group`, `value`, `adjusted`, `build_hash`, `orderable` |
| `camera` | Per frame: `frame`, `eye_mm`, `look_at_mm`, `up`, `fov_short_deg` |

`engine` and `frame_hashes` are at the top level. There are no id AOVs: a package holds one house and its shared parts, and
`identity.house` says which. No cost and no client data are written.

## Test it

```sh
cargo test -p regia_web --test capture
cargo test -p regia_agent --test render_out_product
cargo test -p regia_cli --test render_product_job
cargo test -p product_viewer --test player_host
```

They run on a software driver with no GPU card, and skip when the machine offers no adapter
(`REGIA_VIEWER_REQUIRE_ADAPTER` fails instead). The browser check is in `tests/smoke.html`
(`capture_same_hashes_twice`); run it as on [Web player](web-player.md).
