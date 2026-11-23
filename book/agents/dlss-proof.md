# DLSS real-SDK proof and packaging

DLSS Super Resolution is optional (`regia_render/dlss`, `regia/dlss`). A feature
build can still be the SDK-free stub. Check the live status and evaluated frame
count; a compiled test or a located runtime alone does not prove DLSS ran.

## Establish the live path

Use the existing [upscaler methods](upscalers.md) on an NVIDIA Vulkan device.
`render.set_upscaler` refuses unavailable backends; `render.set_vendor_upscaler`
can deliberately select one to check fallback behavior. With the real SDK,
check `binding_built` and `sdk_version` in the DLSS row's `runtime`, then check
the top-level `vendor_frame`:

```json
{"method":"render.get_upscalers","params":{}}
{"method":"render.set_upscaler","params":{"backend":"dlss","quality":"quality"}}
{"method":"render.get_upscalers","params":{}}
```

If the runtime starts at `found` / `runtime_not_initialized`, first select
`render.set_vendor_upscaler {"vendor":"dlss"}` and render a TAA frame. This
initializes NGX; then the ordinary availability-gated setter can select it.

After rendering, require `vendor_frame.active: true`, increasing evaluated
`vendor_frame.frames`, and zero `vendor_frame.fallbacks`. Repeat with `native` (DLAA), `balanced`, `performance` and
`ultra_performance`. Keep screenshots and motion sequences alongside native and
TSR from the same camera path. A runtime error or built-in fallback fails the
real-SDK acceptance, even if the fallback image looks correct.

## Focused hardware checks

Set `DLSS_SDK` at build time and `REGIA_DLSS_PATH` at run time to the matching
release runtime directory. `VULKAN_SDK` is needed when system Vulkan headers are
absent. The SDK and generated bindings stay outside the repository.

```sh
cargo test -p regia_dlss --features sdk --test gpu_smoke --no-run
# Run through the assigned GPU/card wrapper, with REGIA_DLSS_SMOKE=1:
cargo test -p regia_dlss --features sdk --test gpu_smoke -- --nocapture --test-threads=1
cargo test -p regia_render --features dlss --test vendor_upscaler_frame \
  dlss_all_modes_native_flip_motion_and_timing -- --ignored --exact --nocapture --test-threads=1
```

The agent dispatcher check is also explicit:

```sh
cargo test -p regia_agent --features regia_render/dlss --test dlss_real_sdk \
  -- --ignored --nocapture --test-threads=1
```

It checks the live `render.get_upscalers` responses after actual evaluations in
every mode, requiring increasing frame counts and zero fallbacks. With the
evidence directory set it writes `agent-status.json`.

The frame test is explicitly ignored in ordinary runs. Set
`REGIA_DLSS_PROOF_DIR` to an absolute evidence directory outside the source tree
and `REGIA_GIT_HASH` to the source revision. It saves PNGs and `results.json`:
all five modes, native FLIP, paired TSR measurements, zero-fallback evaluation
counts, and a 48-frame moving-slab comparison. It reports GPU timestamps separately from synchronous frame latency, which
includes CPU work and readback. Neither is an FPS promise.
The trail mask uses pixels exposed since eight native frames earlier; the
comparison permits one quantization level at its minimum contrast. This small
scene supplements the [temporal chart](temporal-chart.md) and motion review.
Do not bless these captures as goldens or call a skipped smoke a pass.

## Package the release runtime

The CLI stages vendor files only with `REGIA_PACKAGE_DLSS=1`. This is an explicit
packaging opt-in; it neither enables the game's Cargo feature nor certifies
linkage. Set `DLSS_SDK` to the approved absolute SDK root and `REGIA_DLSS_NOTICES`
to an absolute text file containing the complete runtime third-party notices
from that SDK's programming guide. The normal `package` command and existing
build tooling remain the surface:

```sh
REGIA_PACKAGE_DLSS=1 DLSS_SDK=/approved/dlss \
REGIA_DLSS_NOTICES=/approved/dlss-runtime-notices.txt \
regia package --platform linux --profile shipping --project /projects/game
```

For Windows use `--platform windows`. The packager takes only the SR runtime
from `lib/<platform>_x86_64/rel`, puts it beside the executable, and adds
`NVIDIA-DLSS-LICENSE.txt` and `NVIDIA-DLSS-THIRD-PARTY-NOTICES.txt`. It preserves
runtime bytes and NVIDIA's signature. AppImage keeps these files beside the
binary in `usr/bin`. Server and Android packages refuse this opt-in. With it
unset or `0`, ordinary packages stage no NVIDIA files.

`dlss_package_invalid` means an unsupported target/server, a flag other than
`0`/`1`, missing/relative SDK or notice path, missing/empty release file, a path
escaping the release directory, or failed staging IO. Correct the supplied
paths and complete notices; never substitute `dev` for a missing `rel` runtime.
The packager does not interpret the legal sufficiency of a notice file.

Use the licensing requirements (`docs/07-licensing.md` § 9, DLSS-enabled distributions, ADR-2002, ADR-4758)
for product attribution, downstream terms and release notification. Launch the
staged application with `REGIA_DLSS_PATH` unset and repeat the live-path check
so it proves executable-directory discovery. Record runtime and notice hashes.
