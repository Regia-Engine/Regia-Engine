# Product viewer Android app (agents)

`samples/product_viewer` is the product viewer as an app: one configurable piece, tap to switch options, drag to
turn the view. It hosts the web player's core (`regia_web::Player`, [Web player](web-player.md)), so picks, refusals
and the view behave the same on both. This page covers what the host adds: the surface, the option panel, packages,
and the `viewer_app.*` calls. ADR-2533, spec 42 § 9.

## Build and run

```sh
regia package --platform android --abi arm64-v8a --project samples/product_viewer   # debug-signed APK
REGIA_ANDROID_KEYSTORE=<file> REGIA_ANDROID_KEYSTORE_PASSWORD=<pw> REGIA_ANDROID_KEY_ALIAS=<alias> REGIA_ANDROID_KEY_PASSWORD=<pw> \
  regia package --platform android --profile shipping --format apk --project samples/product_viewer   # shipping APK, 20 MB
adb install -r samples/product_viewer/dist/android/*.apk
regia viewer standin -o /tmp/standin --pack                                          # stand-in packages
adb shell am start -n com.regiaengine.samples.product_viewer/.RegiaActivity          # creates the folders
adb push /tmp/standin/packages/<name> /sdcard/Android/data/com.regiaengine.samples.product_viewer/files/packages/
```

On desktop: `cargo run -p product_viewer -- --packages DIR [--open SLUG] [--fetch URL]`.

## Packages

| How | What happens |
|---|---|
| `adb push <folder> .../files/packages/` | Listed on the next look at the product list. Every file is checked when it opens |
| `adb shell am start -n <app>/.RegiaActivity --es regia_package_url https://host/products/a1/` | Downloaded once and kept in internal storage, then opened. Offline from then on |
| `--es regia_package <folder name>` | Opens a package that is already here |
| `--es regia_backend gl` | Forces OpenGL ES (`vulkan` forces Vulkan, `auto` is the default: Vulkan, GLES if there is none). For testing the fallback on a phone that has Vulkan. Desktop: `--backend gl`. `viewer_app.stats` names the adapter in `adapter` |
| `--es regia_samples 1` | Forces MSAA off (`4`: on). For measuring what it costs on a slow GPU. Desktop: `--samples 1`. `viewer_app.stats` says what is in use in `gpu_start.samples` |

A download lands in a temporary folder, is verified whole and only then replaces the old package. Plain `http://`,
credentials, queries and redirects are refused. A LAN server with a self-signed certificate is accepted only with a
pin, the SHA-256 of the certificate's DER in the fragment:
`https://<lan-host>/products/a1/#sha256=<hex>` (`openssl x509 -noout -fingerprint -sha256` prints it). The pin
replaces the public roots for that download; the manifest's hashes still decide which files are accepted.

## A package that is refused

A package can be refused when it opens: the store refuses a file whose hash does not match (`pack_*`), the player a
product the snapshot does not sell (`product_not_orderable`) or one it cannot read (`package_invalid`). The screen then
shows the reason on the panel's status line, in the refusal color, with the product list open, so another package can
be picked. A product already open stays open and on screen. With nothing open the panel is drawn over the plain
backdrop, once: the app stays idle. The same words are in `viewer_app.options` (`status`, `status_is_error`) and in
logcat under `product_viewer`. Packages cooked before the stricter resolve are refused this way: cook them again with
`regia viewer standin --pack`.

## Panel and locked test phones

A row of options wider than the screen scrolls by dragging it, and the panel draws no scroll bars: a bar just under
a row took taps meant for its chips. When driving the panel with `adb shell input tap`, read the chip positions from a
screenshot of that product first: the panel's height follows the number of rows.

A debug APK draws over the keyguard (`showWhenLocked` in the debug manifest), so a locked test phone can be measured
without its PIN if its storage is unlocked (`adb shell am get-started-user-state 0` prints `RUNNING_UNLOCKED`). A phone
that has not been unlocked since it booted (`RUNNING_LOCKED`) cannot start the app.

## Idle

With nothing moving the app draws nothing and wakes once a second; the runner starts no vsync thread for it.
`viewer_stats`, `viewer_switch_ms`, `viewer_first_frame_ms` and `viewer_package_open_ms` go to logcat under the target
`product_viewer`, so a sideloaded APK with no agent can be measured.

## Agent methods

They exist in a dev build with the feature `agent` (`cargo ndk ... -p product_viewer_android --features agent`), which
links the whole engine. A sideloaded APK has none of them. Reach a device through `adb forward` as usual.

| Method | Params | Answer |
|---|---|---|
| `viewer_app.options` | | `{product, open, groups, status, status_is_error, showing_products}` |
| `viewer_app.set_pick` | `group`, `value` (a label, or a list for a multi group) | `{ok, build}`; a refusal changes nothing |
| `viewer_app.build` | | the current `jewelry.build/1`, with no client data |
| `viewer_app.view` | | `{view, moving}` |
| `viewer_app.set_view` | `tilt`, `spin`, `pan`, `zoom`, `animate` | `{view, clamped}`; values are clamped to the recipe's limits, never refused |
| `viewer_app.reset_view` | | `{ok}` |
| `viewer_app.touch` | `events: [{id, phase, x, y, at_ms}]`, phases `down`, `move`, `up`, `cancel`, physical pixels | `{ok}`; the same routing as a real finger |
| `viewer_app.set_deterministic` | `on` | `{ok, deterministic}`; while on, the panel and the picture move only in `viewer_app.step` |
| `viewer_app.step` | `dt_ms`, `force`, `capture` (`hash` or `png`) | `{drew, frame, clock_ms, width?, height?, hash?, png_base64?}`; [capture hooks](capture-hooks.md) |
| `viewer_app.stats` | | `{wakes, wakes_per_second, idle, frame_interval_ms, switch_ms, first_frame_ms, package_open_ms, suspends, resumes, player, gpu_start, adapter, surface_format, has_surface, surfaces_created, surfaces_dropped, presents}` |
| `viewer_app.stats_clear` | | `{ok}`; forgets the timing samples |
| `viewer_app.packages` | | `{packages, roots, busy}` |
| `viewer_app.open_package` | `slug` | `{ok, started}`; reads and checks in the background |
| `viewer_app.fetch_package` | `url` | `{ok, started}`; downloads in the background |

## Error codes

Refusals carry `reason` in the error's `data`. The player's codes are on [Web player](web-player.md)
(`pick_not_sold`, `no_product`, `bad_argument`, ...). The host adds:

| Code | Meaning |
|---|---|
| `package_invalid_url` | The URL is not an `https://` folder, or its pin is malformed |
| `package_fetch_failed` | A file could not be read or downloaded, or the certificate did not match |
| `package_store_failed` | The local folder could not be written |
| `package_busy` | Another package task is running |
| `pack_*` | The package failed verification ([Product viewer packages](viewer-pack.md)) |

## Compiled pipelines are kept between runs

On Vulkan the driver's compiled pipelines go to a file in the app's private folder after the first start
(`files/pipeline-cache/`, one file per adapter and driver), and later starts read it. `viewer_app.stats` reports the start
in `gpu_start`: `pipelines_ms`, `samples`, `pipeline_cache_loaded_bytes` (0: compiled from nothing) and
`pipeline_cache_saved_bytes` (0: nothing new to keep). The file is only a speed-up: a missing, damaged or old one means a
compile, and a file that cannot be written means the next start compiles again. To time a first start, clear the app's
data or delete the folder. Logcat has the same numbers in the `viewer_pipelines_ms` line.
