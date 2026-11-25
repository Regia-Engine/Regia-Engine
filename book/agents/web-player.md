# Web player

<!-- index: Product viewer -->
<!-- index: WebGPU -->
<!-- index: WebGL2 -->
<!-- index: Web game | see: Product viewer -->

The product viewer in a browser: one configurable piece on a canvas, tap to switch options, drag to turn the view.
No server and no stream: a page and a product package are static files. Spec: `docs/specs/42-product-viewer.md` § 3.
Decision: ADR-2541.

## What this program is

WebGPU and WebGL2 are the graphics backends of this viewer (`backend` `webgpu`, `webgl2`, or `auto`).
They are not a backend for a general game, and this page does not load a level, a pak, or the desktop
renderer. A general web game target is parked (`docs/versions.md`, the Parked row). Building this
player does not make that target exist.

The same kind of viewer runs in a native window on Linux, Windows, and Android
(`samples/product_viewer`). That window is still a product viewer. `auto` asks for WebGPU first. If
WebGPU takes the canvas and then fails, the page creates the player again on a new canvas with
`backend: "webgl2"` (`backend_unavailable` below). There is no further graphics API in this crate.

## Build it

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked   # must match the crate; the command checks
cargo xtask web-player --out dist/                          # build, bindgen, wasm-opt if present, size report
```

`dist/` then holds `regia_web.js` and `regia_web_bg.wasm`. Serve them and a package folder from any static host.
`regia viewer standin -o site --pack` writes generic packages and a demo page to try it.

## Use it from a page

```js
import init, { RegiaViewer } from "./regia_web.js";
await init();
const viewer = await RegiaViewer.create(canvas, { backend: "auto", maxPixelRatio: 2, msaa: 4 });
viewer.on("changed", (e) => console.log(e.build));
await viewer.open("products/a1/");
const groups = viewer.options();
viewer.setPick("Material", "Titanium");
viewer.setView({ tilt: [10, -5], zoom: 1.5, animate: true });
```

## Calls

| Call | What it does | Fails with |
|---|---|---|
| `RegiaViewer.create(canvas, opts)` | Picks the API, creates the device, wires input. `opts`: `studio` (folder with `env.renv`; the built-in studio when absent), `backend` (`auto`, `webgpu`, `webgl2`), `maxPixelRatio`, `msaa` (1 or 4). Unknown options are refused | `bad_argument`, `backend_unavailable`, `studio_fetch_failed`, `studio_invalid` |
| `open(url)` | Fetches and checks the package folder, opens the configurator, uploads every mesh, draws. Resolves after the first frame. A later `open` replaces the earlier one | `package_fetch_failed`, `package_hash_mismatch`, `package_version_unsupported`, `package_invalid`, `product_not_orderable`, `open_superseded` |
| `options()` | `[{group, kind, current, visual, required, applies, values: [{value, label, available, reason?, conflicts?, material_id?}]}]`. `available` says whether `setPick` would pass right now | `no_product` |
| `setPick(group, value, opts?)` | Applies the pick only if the result is sold; a refusal changes nothing. `{adjust: true}` keeps the pick and changes the fewest other groups. Returns `{ok, build, adjusted}` | `pick_not_sold`, `pick_unknown_group`, `pick_unknown_value`, `bad_argument`, `no_product` |
| `build()` | The current `jewelry.build/1` | `no_product` |
| `setView({tilt, spin, pan, zoom, animate})` | Sets the view target; absent fields stay. Limits clamp and never fail. Returns `{view, clamped}` | `bad_argument`, `no_product` |
| `view()` | `{tilt, spin, pan, zoom}` as shown now | `no_product` |
| `resetView()` | Animates to the default view (also a double tap) | `no_product` |
| `on(name, fn)`, `off(name, fn)` | Listeners for `ready`, `changed`, `error` | `bad_argument` |
| `stats()` | `{backend, requests, frames, draws, skipped, picks, refused, pending, dirty, visible, deterministic, scriptFrame, scriptClockMs, pipelines, looks, lastSwitchMs, drawing, problems}`. `problems` lists look fallbacks and `warm_set_miss: …` when the renderer created a pipeline after the product's first frame (spec 42 § 7) | none |
| `setDeterministic(on)`, `step(dtMs, force?)`, `frameHash()` | Scripted capture: see below | `bad_argument` |
| `destroy()` | Stops the loop, removes every listener, drops GPU objects. Later calls fail with `destroyed` | none |

Async calls reject and sync calls throw a JS `Error` with a `code` property. Every failure also fires `error`.

## Events

| Event | Body | When |
|---|---|---|
| `ready` | `{type, incomplete, backend}` | The first frame of an opened product is on the canvas. `incomplete: true`: the listing is partial; never present it as the full range |
| `changed` | `{type, cause, build}` | `cause` is `pick` (once per applied pick) or `view` (once when a moved view comes to rest, not per frame) |
| `error` | `{type, code, message}` | Any failure |

The same events are dispatched on the canvas as `CustomEvent`s `regia-ready`, `regia-changed`, `regia-error`, with the
body in `detail`. A listener may call straight back into the player.

## Error codes

| Code | Meaning |
|---|---|
| `backend_unavailable` | The browser offers neither WebGPU nor WebGL2, or not the one requested. With `auto`, a WebGPU failure after WebGPU took the canvas is reported as is (a canvas keeps its first context, so WebGL2 cannot use it): create the player on a new canvas with `backend: "webgl2"` |
| `bad_argument` | A call got an argument of the wrong shape, including one with no JSON form (a function, a symbol) |
| `destroyed` | The player was destroyed |
| `device_lost` | The GPU device or WebGL context was lost; the player restores it when it can |
| `no_product` | The call needs an open product |
| `open_superseded` | A later `open` replaced this one before it finished |
| `package_fetch_failed` | A package file could not be fetched |
| `package_hash_mismatch` | A package file's size or hash does not match the manifest, or a listed file is missing |
| `package_invalid` | The package is damaged, or its recipe, options or materials are refused, or its option record is for a house or product the manifest does not name |
| `package_version_unsupported` | The package is newer than this player reads |
| `pick_not_sold` | The snapshot does not sell the combination this pick would make |
| `pick_unknown_group` | The product has no such option group |
| `pick_unknown_value` | The option group has no such value |
| `product_not_orderable` | The snapshot does not sell the product at all |
| `render_failed` | The renderer refused a frame |
| `studio_fetch_failed` | The studio package could not be fetched |
| `studio_invalid` | The studio package is damaged |

## The loop

A browser frame is requested only while something changed: input, damping still moving, a pick, a resize, a restored
context. When nothing changed no callback is pending, so an idle page costs nothing. `stats().pending`,
`stats().requests` and `stats().draws` show this directly: hold them across a few browser frames and none moves. Moving
the mouse over the canvas wakes nothing. A hidden page draws nothing and draws one frame when shown again.

A frame the canvas surface skips (a timeout, an outdated or occluded surface; `stats().skipped` counts them) is drawn
again on the next browser frame, so `ready` and a pick never wait for an input. After 120 skipped frames in a row the
player waits for the next change, so a surface that never comes back does not keep the page busy.

A new product frees the previous one's anodize color tables: a page can open product after product, and two
products that share a material id still show their own colors.

`create` sets `touch-action: none` on the canvas. Pointer events (mouse, touch, pen) and the wheel go to the drag view;
default browser gestures are suppressed on the canvas only.

## Scripted capture

`setDeterministic(true)` stops asking the browser for frames and starts the player on the script's clock at 0.
`step(dtMs)` runs one frame with that time and returns whether it drew. `step(dtMs, true)` draws even when nothing
changed, as a video needs a frame at every step. The same calls make the same frames on one browser and machine. Read
the canvas, or `frameHash()`, in the task that called `step`:

```js
viewer.setDeterministic(true);
viewer.setPick("Material", "Titanium");
viewer.setView({ tilt: [12, -7], zoom: 1.3 });
for (let i = 0; i < 10; i++) viewer.step(1000 / 60, true);
const hash = viewer.frameHash();   // 16 hex digits, FNV-1a 64 of the canvas's RGBA
const png = canvas.toDataURL("image/png");
```

More in [capture hooks](capture-hooks.md).

## Test it

`cargo test -p regia_web` runs the player's logic with no browser and no GPU. The browser test needs a build:

```sh
cargo xtask web-player --out /tmp/web
REGIA_WEB_DIST=/tmp/web REGIA_JW_REQUIRE_BROWSER=1 cargo test -p regia_web --test browser_smoke -- --nocapture
```

It looks for a headless Chromium in `REGIA_HEADLESS_CHROME`, then under `~/.cache/ms-playwright`, and runs it on
software rendering (no GPU card): WebGL2, WebGPU, `auto` both ways, and no graphics at all. Set `REGIA_WEB_EVIDENCE` to
a folder to keep the screenshots.

The same page runs in Firefox when `REGIA_FIREFOX` names its binary and `Xvfb` is installed (`REGIA_JW_REQUIRE_FIREFOX=1`
fails instead of skipping):

```sh
REGIA_WEB_DIST=/tmp/web REGIA_FIREFOX=<firefox>/firefox REGIA_JW_REQUIRE_FIREFOX=1 \
  cargo test -p regia_web --test browser_smoke the_player_runs_in_firefox -- --nocapture
```

Headless Firefox finds no GL driver for WebGL, so the test starts a private `Xvfb` display and gives Firefox Mesa's
software GL (llvmpipe) and the software Vulkan driver (lavapipe) for WebGPU. No GPU card is used. It runs WebGL2,
WebGPU, `auto` both ways, no graphics, a 2x screen and the demo page. Its screenshots start with `firefox-`.

On a phone, with its screen unlocked and awake, the same page reports real frame, switch and idle numbers in its `perf` row:

```sh
REGIA_WEB_DIST=/tmp/web REGIA_WEB_ADB_SERIAL=<serial> REGIA_WEB_BACKEND=webgl2 REGIA_WEB_ANDROID_BROWSER=com.android.chrome \
  cargo test -p regia_web --test browser_smoke on_a_phone -- --ignored --nocapture
```

## Looks

The player draws the `regia_looks` shaders through `regia_web::scene::player_config`, the configuration it shares with
the Android viewer ([Product looks](product-looks.md), ADR-2615). `stats().looks` is `true` when every shader family is
the looks' version, so a page can tell it from a build on the renderer's starting-point shaders. A stone's size comes
from its mesh, and a size pick rewrites the material along with the mesh.
