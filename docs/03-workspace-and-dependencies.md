# 03 — Workspace and dependencies

All versions are pinned in the root `Cargo.toml` `[workspace.dependencies]`; member crates use
`{ workspace = true }`. The skeleton was verified with `cargo check --workspace` on 2026-10-03
(rustc 1.99.0, Linux x86_64 host only — the Windows and Android graphs are first built in M0/M1).

## 1. Dependency table

| Crate | Version | Used by | Why / notes |
|---|---|---|---|
| bevy_ecs | =0.20.0 | ecs | Archetype ECS, parallel scheduler, change detection, relationships (hierarchy). Facade: `regia_ecs`. Feature `trace` (v0.1.4, ADR on frame telemetry): one span per system run, timed per system by `FrameProfiler` for telemetry's `top_systems`. Stable 0.20.0 since ADR-258. |
| bevy_reflect | =0.20.0 | ecs | Reflection, type registry, reflect-driven serde. Features `glam`, `uuid`, `smallvec`, `reflect_documentation` (doc comments for `describe_type`, `specs/23` § 1; enabled in M2). |
| bevy_tasks | =0.20.0 | core | Thread pools used by bevy_ecs; reused for engine jobs. |
| bevy_state | =0.20.0 | ecs, app | Game states: `States`/`SubStates`/`ComputedStates`, `OnEnter`/`OnExit`/`OnTransition`, `in_state`, `DespawnOnExit` (MIT/Apache, adopted 2026-10-08, port catalogue G1). `default-features = false`, features `std`, `bevy_reflect` (no `bevy_app`: `regia_app` runs the `StateTransition` schedule itself). Facade: `regia_ecs::state`. **Version-locked with `bevy_ecs`; bump together.** Pulls `bevy_state_macros` and `variadics_please`. |
| glam | 0.33.12 | all | Math. **Must** match bevy_reflect's, glamx's (rapier) and kira's glam. |
| bytemuck | 1.25 | core, render | GPU buffer packing (`#[repr(C)]` + `Pod`). No encase — layouts are hand-written with static size asserts. |
| uuid, blake3 | 1.27, 1.8 | core (uuid), asset, cli, render | Asset ids; content hashing for cache keys and pak-freshness checks; the persistent pipeline cache checksum and shader hash (render). |
| smallvec, parking_lot, crossbeam-channel | | core (smallvec, parking_lot), world | Utilities. |
| thiserror / anyhow | 2 / 1 | libs / bins | Typed errors in libraries, `anyhow` only in binaries and tools. |
| tracing (+ subscriber, tracy) | 0.1 | all | Logging + profiling spans. `tracy` cargo feature enables `tracing-tracy`. |
| profiling | 1.0.18 | lookdev (optional) | Already in the graph through wgpu-core. lookdev's opt-in `wgpu-spans` feature enables `profile-with-tracing` so `do27_chase --census` sees wgpu-core's own create calls (hitch hunt, `specs/27` § 2.4). Never a runtime dependency. |
| android_logger, log | | core (android) | Route logs to logcat. |
| serde, ron, postcard, toml | | core, ecs, asset, render, cli, agent, xtask | RON for human-edited data (scenes, configs, `.meta`; `regia_core` peeks the format `version` field); postcard for cooked binary; TOML for `Regia.toml` and `.regia/agent.toml` (M11). `regia_render` parses its GPU tier table (`quality/gpu_tiers.ron`, ADR-157) with serde + ron. `cargo xtask smoke` writes one tiny uncompressed pak so the lobby row can stream it. |
| lz4_flex, memmap2 | | asset, telemetry (lz4_flex only) | Pak compression (fast decode, pure Rust) and mmapped reads on desktop. |
| winit | 0.30.13 | platform | Windowing + event loop. Feature `android-game-activity`. Coupled to egui-winit. |
| libc | 0.2 | platform (android), render (linux, android, windows), core (linux, android), script_lua (unix), asset (unix), detsim (linux, android, macos), net (unix, windows) | `_exit` when a game ends on Android (ADR-013). `setpriority` drops the renderer's sky worker to nice 19 so background bakes never take the frame thread's core (`specs/27` § 2.4). `atexit` for the pipeline-warm exit hook on Windows too (`regia_render::platform`). `process_vm_readv`/`mincore` for the code-residency keeper (`regia_core::residency`, ADR-125). The `O_NOFOLLOW`/`O_NONBLOCK`/`ELOOP` constants for opening Lua mod files (`regia_script_lua::files`, ADR-2385). `O_NOFOLLOW`/`O_NONBLOCK` opens for `mods://` (`regia_asset::mods_vfs`, ADR-2418). Already in the lockfile through std-adjacent crates. `regia_detsim` reads `CLOCK_THREAD_CPUTIME_ID` for the tick profiler (Linux and Android; macOS thread clock value 16; ADR-8498). MIT OR Apache-2.0, the same licence as the existing row. | `regia_net` sets `SO_REUSEADDR` before `bind` on `LanSocket` (ADR-7989).
| android-activity, ndk, jni | 0.6.1, 0.9, 0.22 | platform (android) | GameActivity glue, AAssetManager, Java calls (soft keyboard, vibration, etc.). |
| `com.google.android.play:asset-delivery` (Gradle, Java) | 2.2.2 (Play Core SDK Terms of Service, not an open source licence) | generated Android project only | Store asset packs: added by `regia package --platform android` only for a project with `fast_follow` or `on_demand` chunks, never compiled into a Rust crate (spec 40 § 2.6, ADR-1876). The pin is `ASSET_DELIVERY_VERSION` in `regia_cli::android`. |
| gilrs | 0.11 | input (desktop) | Gamepads on desktop. **Not available on Android**. |
| *(no crate)* | — | input (desktop) | SpaceMouse HID (spec 29, ADR-219): no HID library. Linux reads `hidraw` with std (sysfs + `/dev/hidrawN`); Windows uses `windows-sys` (above). `hidapi` (MIT, bundles C hidapi under BSD) was rejected as larger than needed and C-compiling. |
| windows-sys | 0.61 | platform (Windows), detsim (Windows) | Win32 calls for `regia_platform` services: locale (`GetUserDefaultLocaleName`), memory (`GlobalMemoryStatusEx`), `ShellExecuteW` for `open_url`, registry version keys, `FlashWindowEx`, `SHGetKnownFolderPath` + `CoTaskMemFree` (Saved Games folder, `Win32_System_Com`), `SetThreadPriority` (background mode for the pak integrity thread, ADR-172), raw HID for the SpaceMouse (`regia_platform::hid`: SetupAPI enumeration, `hid.dll` attributes/caps, `CreateFileW`/`ReadFile`; features `Win32_Devices_DeviceAndDriverInstallation`, `Win32_Devices_HumanInterfaceDevice`, `Win32_Storage_FileSystem`, `Win32_System_IO`, ADR-219), `SetConsoleCtrlHandler` (`regia_platform::console_ctrl`, feature `Win32_System_Console`, ADR-1886). Already in the lockfile through winit/wgpu. M1 (ADR-013). `regia_net` also uses it directly on Windows (DACL checks for private net files: `private_file` for identity keys and known hosts (`server.key`, `known_hosts.ron`), `windows_acl` for admin tokens, native-crash terminate; features `Win32_Security`, `Win32_Security_Authorization`, `Win32_System_SystemServices`, `Win32_Storage_FileSystem`, `Win32_System_Threading`, ADR-4828). The crash uploader obtains Windows file identity through `regia_platform::open_file_identity` (`GetFileInformationByHandle`, `Win32_Storage_FileSystem`, ADR-4821/ADR-4852); it adds no direct Win32 dependency. Editor hot reload also uses loaded-module enumeration (`Win32_System_ProcessStatus`, `Win32_System_Threading`; MIT OR Apache-2.0). Job objects for play-in-editor child processes (`regia_platform::child_bind`, feature `Win32_System_JobObjects`, ADR-2682). Job children start suspended and are resumed once assigned (`ChildGroup::spawn`: `CreateToolhelp32Snapshot`/`Thread32First`/`Thread32Next`, feature `Win32_System_Diagnostics_ToolHelp`, ADR-7548). `regia_agent` uses it on Windows only for the agent local transport's named pipe (`Win32_System_Pipes`: `CreateNamedPipeW`/`ConnectNamedPipeW`/`DisconnectNamedPipe`/`SetNamedPipeHandleState`; the owner-only DACL is `regia_net::private_file`, spec 23 § 2, ADR-5708). `regia_detsim` calls `GetThreadTimes` for the tick profiler (`Win32_System_Threading` and `Win32_Foundation`, already enabled, ADR-8498). |
| dirs, arboard | 7, 3.6 | platform | User data paths; clipboard (desktop, text only: `default-features = false`, because the default `image-data` feature pulls `image` into runtime crates). |
| wgpu | 30 | render, editor, xtask, agent (the render-out AOV pass, ADR-1521; tests, ADR-515), viewer (feature `webgl` on wasm32 only, ADR-2539), looks (optional, features `gpu` and `viewer`, ADR-2538) | GPU abstraction: Vulkan, DX12, Metal. Coupled to egui-wgpu. Opt-in `static-dxc` feature (`regia_render`/`regia`, Windows x86_64): pulls `mach-dxcompiler-rs` (statically linked DXC, MIT/Apache-2.0 via wgpu-hal) for the DX12 fallback backend. `cargo xtask smoke` opens one headless device for the alpha GPU session. |
| wasm-bindgen, wasm-bindgen-futures, js-sys, web-sys | =0.2.129, 0.4.79, 0.3.106, 0.3.106 (already in the lockfile through wgpu and winit) | web player (`regia_web`, on wasm32 only, ADR-2541) | The browser boundary of the web player: the JS class, promises, DOM events, `fetch`, `requestAnimationFrame`. MIT OR Apache-2.0. `wasm-bindgen` is pinned exactly because the `wasm-bindgen-cli` tool must match it (`cargo xtask web-player` checks). |
| naga_oil | 0.23 | render | WGSL module composition (`#import`, `#ifdef`) on top of naga 30. |
| naga | 30 | render, looks (dev: `wgsl-in` + `glsl-out`, the GLSL ES 3.00 translation tests of the look shaders, ADR-2538) | Direct dependency so composed modules can be passed to `wgpu` (`naga-ir`), so `check-shaders` can emit SPIR-V (`spv-out`), and so the same gate can parse WGSL outside `regia_render` (`wgsl-in`). `hlsl-out` (ADR-295, no new crate, same MIT/Apache-2.0 licence) lets a Linux test translate every engine shader to HLSL Shader Model 6.0, the DX12 path's first half; wgpu already enables it on Windows. `msl-out` (ADR-2644, no new crate, same licence) lets the shader permutation matrix (`check-shaders --matrix`) translate every composed module to Metal Shading Language with wgpu's Metal options on any host; wgpu already enables it on macOS. |
| ktx2 | 0.5 | asset, render, terrain | Container format for cooked textures. `regia_terrain` parses BCn blobs for hardware-sampled upload and builds a minimal KTX2 in the streaming round-trip tests. |
| rapier3d | 0.36 | physics, xtask | Physics. Uses `glamx` (glam 0.33) types. Feature `serde-serialize`. `cargo xtask smoke` replays two worlds for the determinism row. |
| rapier2d | 0.36 (Apache-2.0) | physics (feature `physics2d`) | 2D rigid bodies for the 2D toolkit (`regia_physics::physics2d`, specs/22 § T2.10). Same `glamx` 0.3 / glam 0.33.12 / nalgebra / parry 0.31 as rapier3d: `cargo tree -d -p regia_physics -F physics2d` lists no glam, glamx, nalgebra, simba or parry duplicate. Its `math::Vector` *is* glam `Vec2`. Off by default (`regia/physics2d`). |
| kira | 0.12.5 | audio | Audio engine (mixer tracks, effects, spatial, streaming) on cpal 0.18 + symphonia. **Vendored and patched** at `vendor/kira` (ADR-8830, MIT OR Apache-2.0): its cpal stream restart no longer panics when a device cannot be reopened; see `vendor/kira/REGIA-PATCHES.md`. A kira bump re-applies the patch on the new source. |
| symphonia | 0.6.1 (MPL-2.0) | audio | Already in the tree through kira (same version; its codec features come from kira). A direct dependency of `regia_audio` since ADR-4922 for the bounded whole-file decoder (`SoundDecodeLimits`); `default-features = false`. |
| cpal | 0.18 | audio | Audio input streams for voice capture (M8); the output side arrives through kira. |
| rustfft | 6.4 | audio | FFTs for binaural (HRTF) overlap-save convolution (`regia_audio::hrtf`, adapted from the MIT `hrtf` crate). Pure Rust, MIT OR Apache-2.0. |
| taffy | 0.14 | ui | Flexbox/grid layout. |
| cosmic-text | 0.19 | ui, asset_pipeline | Shaping, bidi, font fallback, rasterisation (swash). The cook's font subsetting shapes each distinct grapheme cluster of locale text with it to see which glyphs the text reaches (ADR-2697). |
| skrifa | 0.40 | asset_pipeline (and cli tests) | Reads fonts for the cook's font subsetting: `cmap`, `GSUB` lookups, outlines in tests (ADR-2697). The version cosmic-text 0.19 already pulls in, so no new crate in the graph. MIT OR Apache-2.0. |
| unicode-script | 0.5 | ui | `Script` values for implementing cosmic-text's `Fallback` trait with the bundled font families (same version cosmic-text uses; no new crate in the graph). |
| fluent-bundle, unic-langid | 0.16, 0.9 | ui | Localization (Project Fluent). |
| fluent-syntax | 0.12 (Apache-2.0 OR MIT) | ui | The Fluent parser fluent-bundle already builds; the localization validator, gather and pseudo tools read its AST (ADR-1466). |
| intl-memoizer | 0.5 (Apache-2.0 OR MIT) | ui | The memoizer fluent-bundle already builds (no new crate in the build); `loc_format`'s `NUMBER()`/`DATETIME()` result type implements `FluentType`, whose methods name it (ADR-2698). |
| lyon | 1.0.19 | quick | Fill and stroke tessellation (joins, caps, tolerance) for the quick draw layer's paths and outlines (`regia_quick`). MIT OR Apache-2.0; pure Rust (`lyon_tessellation`, `lyon_path`, `lyon_geom`, `euclid`, `float_next_after`, `arrayvec`), builds for Android. |
| num-traits | 0.2.19 | core | Generic scalar bounds of `regia_core::math::geom` (ported from nannou_core). Already in the lockfile through several crates. |
| gltf | 1.4 | pipeline, editor | glTF 2.0 import; the editor reads source glTF for the viewport's `MeshSource` resolver and the mesh viewer. `third_person` keeps it as a dev-dependency only, to check the cooked Fox skin against the source glTF (shipped games may not link `gltf`). Feature `extras` (ADR-1875): the importer accepts application vertex attributes (`_WIND`, `_WIND_PIVOT` of `docs/formats/plant-wind.md`); without it the parser rejects every `_NAME` semantic. |
| image | 0.25 | pipeline, editor, xtask | PNG/JPEG/HDR/EXR/TGA/BMP/WebP decode at import time (feature `webp`, for `EXT_texture_webp`, ADR-3016; it adds `image-webp`, MIT OR Apache-2.0, pure Rust); the editor decodes source textures for viewport materials and the texture viewer. `xtask golden-sheets` decodes blessed PNGs into review contact sheets. |
| intel_tex_2 | 0.5 | pipeline | BC1–7, BC6H encoders (ISPC kernels). **Verify Windows MSVC support in M4** (macOS arm64 when that port happens); fallback plan in `specs/12`. |
| ctt-astcenc | 0.6 | pipeline | Vendored ARM astcenc for ASTC KTX2 on Android cook (`intel_tex_2` ASTC path is unimplemented upstream). |
| meshopt | 0.6 | pipeline, editor | Vertex cache/overdraw/fetch optimisation, simplification (LODs), meshlets (M15d), the vertex and triangle decoders of `EXT_meshopt_compression` (ADR-3016); the editor's mesh viewer previews the cook's LOD chain with the same simplifier. |
| bevy_mikktspace | 1 | pipeline, render | MikkTSpace tangent generation (import and runtime fallback). |
| notify, walkdir | 8.2, 2.5 | asset (desktop), pipeline | File watching for hot reload; directory scans. |
| tiled | 0.16 (MIT) | pipeline | Tiled `.tmx`/`.tsx` import (specs/22 § T2.10), `default-features = false` (no zstd: zstd-compressed layers are rejected with an error). Pulls `quick-xml 0.31` beside the workspace's 0.41 (a duplicate recorded in `deny.toml`). `.tmj`/`.tsj` JSON maps are parsed by Regia Engine with serde. Forbidden in runtime crates (check-layers). |
| texture_packer | 0.30 (MIT) | pipeline | Skyline packing of sprite atlases at cook time (`.atlas.ron`, specs/22 § T2.10), `default-features = false`; uses the workspace `image 0.25`. Regia Engine composes the pages itself (trim, extrude). Forbidden in runtime crates (check-layers). |
| base64, flate2 | 0.22, 1 | pipeline | Base64 and zlib/gzip tile data in Tiled JSON maps. Both already in the lockfile through `tiled`. |
| resvg, usvg | 0.45.1 (Apache-2.0 OR MIT) | pipeline, xtask | SVG import at cook time (ADR-3350): `usvg` builds the render tree from a document `roxmltree` has already parsed and checked, `resvg` draws it with `tiny-skia`. `default-features = false`: no `text`, no `system-fonts`, no `raster-images`, so the renderer reads no font, file or network resource. Forbidden in runtime crates (check-layers). `cargo xtask editor-icons` draws the editor's app icon set from its one SVG with the same renderer (ADR-20000). Pulls `svgtypes`, `simplecss`, `kurbo`, `data-url`, `imagesize` and similar small pure-Rust crates, all MIT or Apache-2.0. |
| tiny-skia, roxmltree | 0.11.4, 0.20 (BSD-3-Clause, MIT OR Apache-2.0) | pipeline | The pixmap and transform types the SVG import draws into, and the XML parse with a node limit and no DTD that runs before `usvg`. Both were already in the lockfile (winit's Wayland decorations, `fontconfig-parser`); one version of each. |
| tungstenite | 0.30 | agent | Agent-protocol WebSocket server on its own thread, loopback only, no TLS (ADR-014). |
| snow | 0.10 (Apache-2.0 OR MIT) | net | Noise Protocol Framework for transport encryption (ADR-1901): `Noise_NX_25519_ChaChaPoly_BLAKE2s`. `default-features = false` with `use-chacha20poly1305`, `use-blake2`, `use-curve25519`, `use-getrandom` only; the `std` feature stays off because it names `ring/std` as a hard edge. Pulls `chacha20poly1305` 0.10, `chacha20` 0.9, `poly1305` 0.8, `blake2` 0.10, `curve25519-dalek` 4.1 (with `fiat-crypto` on some targets), `getrandom` 0.3, `subtle`, `zeroize`. Pure Rust; no `ring`, no AES. **Audit status:** `chacha20poly1305` documents an NCC Group audit; `snow` states it has had no formal audit, and `curve25519-dalek` and `blake2` document none. Open item for the pre-release security review. |
| semver | 1.0 (MIT OR Apache-2.0) | framework | Extension manifests: dependency version ranges and engine version bounds (`regia_framework::extension`, specs/57, ADR-1669). Already in the lockfile; now a workspace dependency. `toml` and `blake3` (both already workspace dependencies) join it in `regia_framework`. |
| png | 0.18 | agent, editor, render, framework, regia_procgen, regia_vfx (dev), xtask, viewer (feature `tools`, dev), looks (dev: evidence stills) | PNG encoding of agent screenshots in dev builds and of `editor.capture_viewport`; the screenshot in a feedback bundle (`regia_framework::feedback`, shipped builds too); decoding a level's colour-LUT strip (`regia_render::ColorLut`); 16-bit heightfield PNG encode/decode in `regia_terrain` tests; decal evidence PNGs in `regia_vfx` tests (`image` stays pipeline/test-only). `cargo xtask smoke` writes the GPU session's frame. |
| exr | 1.74 (BSD-3-Clause) | agent | OpenEXR writer of render-out: the scene-linear frames of `format: "exr"` and the EXR AOV files (depth, ids, motion; ADR-1521). Pure Rust, builds for Android; `default-features = false` (no rayon). Already in the lockfile through `image`'s `exr` importer feature, so no new crate enters the graph. |
| rmcp | 3.9 (Apache-2.0) | cli | Official Rust MCP SDK for `regia mcp` (stdio). CLI only. |
| rmcp | 3.9 (Apache-2.0) | cli | Official Rust MCP SDK for `regia mcp` (stdio). CLI only; security update in ADR-4855. |
| tokio | 1.53 | cli | Async runtime required by rmcp. CLI only; forbidden in runtime crates (ADR-014). |
| steamworks | 0.13 (resolves 0.13.1) | steam | Rust bindings over Valve's Steamworks SDK, behind `regia_steam`'s off-by-default `steamworks` feature (desktop only; target-gated, Android builds it as a no-op). **The SDK and its redistributable are under Valve's Steamworks SDK Access Agreement, never committed**; builds with the feature need `STEAM_SDK_LOCATION` (`crates/regia_steam/README.md`). steamworks-sys vendors the SDK headers (excluded from its MIT/Apache licence) and pre-generated bindings; `cargo deny check` passes (declared `MIT / Apache-2.0`); the SDK itself ships only inside Steam depots via `regia package --steam`. RUSTSEC-2026-0121 is patched in 0.13.1 (the locked version). **Hosts that use `Steam::client()` must depend on the same `steamworks` 0.13.x** (one shared client; never `Client::init` in the host). |
| ureq (rustls) | 2.12 (MIT OR Apache-2.0) | crash, asset; `samples/product_viewer` (package download through the guarded connector, with webpki-roots 0.26; rcgen and rustls as dev-dependencies for its TLS test, ADR-2533) | Sync HTTP client for the opt-in `regia_crash` uploader's off-by-default `tls` feature (`default-features = false, features = ["tls"]` → rustls with the `ring` provider, no native-tls/openssl). Cross-compiles to Android. `http://` uploads use `regia_crash`'s dependency-free `std::net` transport instead; only `https://` pulls ureq. § 7, specs/21 § 5. Also `regia_asset`'s off-by-default `content-http` feature (forwarded as `regia/content-http`): `https://` content-chunk and manifest fetches, same configuration (specs/40 § 2.6, ADR-1585 (d), ADR-1876); its loopback TLS test uses the same `rustls` + `rcgen` dev-dependencies as `regia_crash`'s. |
| crash-handler | 0.8 (MIT OR Apache-2.0, Embark Studios) | platform (Linux, Windows, Android) | Installs the native crash handlers (Linux signals SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGABRT/SIGTRAP; Windows unhandled-exception filter) behind `regia_platform::native_crash`, the one place with the `unsafe` call. Target-gated to Linux, Windows and (since v0.1.28, ADR-328) Android, where its `linux` module installs the signal handlers that write the in-process crash record; licence unchanged. Pulls `crash-context` (MIT). ADR-158. |
| minidumper | 0.11 (MIT OR Apache-2.0, Embark Studios) | framework (Linux, Windows) | The out-of-process crash monitor for native crash bundles (`feedback::native`): IPC client in the game, server in the monitor, which writes the dump with `minidump-writer` 0.13 (MIT). Pulls `uds`, `goblin`, `procfs-core`, `scroll` (all MIT or MIT/Apache). Target-gated to Linux and Windows (on Windows over an `AF_UNIX` socket file, dump by `MiniDumpWriteDump`). ADR-158. |
| minidump, minidump-processor, minidump-unwind | 0.27 (MIT, rust-minidump) | xtask | `cargo xtask minidump`: parse a native crash bundle's minidump and walk the crashing thread's stack (`minidump-processor` with `default-features = false`, no HTTP symbol server). **Dev tool only, never a runtime dependency.** Frames are resolved with binutils `addr2line` against `regia package`'s `symbols/<binary>.debug`. ADR-158. |
| goblin | 0.9.3 (MIT) | xtask | `cargo xtask binary-size` (ADR-3188): reads ELF and PE section tables and ELF symbol sizes to attribute a shipped binary to crates. Features `std`, `elf32`, `elf64`, `pe32`, `pe64`, `endian_fd` only; dev tool, never a runtime dependency. Already in the lock through `minidump-writer`. |
| egui, egui-wgpu, egui-winit | 0.36.2 | editor; `samples/product_viewer` (egui, egui-wgpu: the Android option panel, without `default_fonts`, ADR-2533) | Editor UI. |
| egui_dock | 0.21 | editor | Dockable tabs. |
| egui-snarl | 0.12 | editor | Node graph widget (animation graphs, material graphs). |
| transform-gizmo-egui | 0.11 | editor | Translate/rotate/scale gizmos. Pulls **glam 0.32** internally — convert at the boundary via `[f32; N]` arrays; never let its glam types leak. |
| rfd | 0.17 | editor (desktop targets only) | Native file dialogs. A desktop-only target dependency of `regia_editor` (`cfg(not(target_os = "android"))`): rfd has no Android implementation of its dialog traits, so the Android workspace check compiles the editor with the no-op `FileDialog` of `regia_editor::file_dialog` (ADR-10356). |
| libloading | 0.9 | editor | Loading the game dylib. No further hot-reload crate: `subsecond` and `dioxus-devtools` are not added (ADR-015). |
| clap | 4.6 | cli | CLI parsing. |
| syn, quote, proc-macro2 | 2, 1, 1 | macros, xtask | Derive macros. `cargo xtask api-snapshot` reads the stable crates' public items from source (ADR-1942). |
| image-compare, criterion | | tests | Golden images (SSIM + per-pixel), benchmarks. Property tests use hand-rolled deterministic generators and the `regia_test_utils::fuzz` / `regia_core::fuzz` mutators; the workspace `proptest` entry was removed in ADR-3388 (nothing referenced it). |
| unicode-segmentation | 1.13 | ui, asset_pipeline | Grapheme-cluster and word boundaries (UAX #29) for `TextInput` caret movement, deletion and word selection (ADR-336). Same version cosmic-text 0.19 already pulls in; no new crate in the graph. MIT OR Apache-2.0. |
| icu_segmenter | 2.3 (Unicode-3.0) | ui | UAX #14 class SA word segmentation (Thai, Lao, Khmer, Myanmar) for line breaking: `LineSegmenter::new_auto` with compiled dictionaries (Thai, Burmese, Lao+Khmer) and LSTM models, behind a 32-entry LRU in `regia_ui::line_break_sa` (ADR-3323). Adds ~420 KiB to x86_64, aarch64-android and wasm32 release binaries (measured on the test machine, ADR-3323). Default features (compiled_data + auto); no serde. |
| signal-hook | 0.3 (MIT OR Apache-2.0) | net, cli (Unix only) | SIGTERM / SIGINT / SIGHUP flags for the dedicated-server host (`regia_net::server_host::signals`, spec 76 § 5, ADR-1663) and SIGTERM forwarding in `regia server` (ADR-1886): the safe `flag` API only, `default-features = false` (no channel or iterator threads), target-gated `cfg(unix)`. Pulls `signal-hook-registry` (already in the lockfile) and `libc`. The same lane makes `regia_net` use the workspace `toml` for `server.toml`. |
| ash | 0.38 (the version wgpu-hal 30 resolves; no new crate in the graph) | dlss (Linux and Windows only) | Raw Vulkan handle and format types for the NGX calls in `regia_dlss` (ADR-2002). MIT OR Apache-2.0. |
| bindgen, cc | 0.73, 1 (build-dependencies, optional) | dlss (`regia_dlss` feature `sdk`, which `regia_render`'s `dlss` turns on) | Bindings to NVIDIA's NGX headers and a C file for their `static inline` helpers, generated only when `DLSS_SDK` names an SDK checkout at build time (else the crate builds as a stub). bindgen BSD-3-Clause (its `clang-sys` Apache-2.0, needs libclang only when it runs); cc MIT OR Apache-2.0. **The NVIDIA DLSS SDK (v310.9.1, NVIDIA RTX SDKs licence, owner-approved 10-12) is never committed**: its static `nvsdk_ngx` library is linked from `DLSS_SDK`, and the runtime `libnvidia-ngx-dlss.so.<version>` / `nvngx_dlss.dll` ships next to the executable (ADR-2002). |
| socket2 | 0.6 (MIT OR Apache-2.0) | cli | Not a direct dependency: tokio's `net` feature, which `regia_cli` turns on with `io-util` for `regia mcp --http` (ADR-2346), links it. CLI only; tokio stays out of runtime crates (ADR-014). |
| regex | 1.13 (MIT OR Apache-2.0; already locked through naga_oil, default features, no new crate in the graph) | configurator (`regia_configurator` feature `validate`, default on; a web player builds without it, ADR-2535) | The `pattern` and `patternProperties` keywords of the product viewer contract schemas (`regia_configurator::schema`, ADR-2480). ECMA-262 patterns are evaluated in the Rust `regex` dialect (no look-around or back-references); a schema whose pattern the crate does not accept is refused at compile time. |
| gltf, image, flate2, blake3, glam (existing workspace dependencies, no new crate in the graph) | 1.4, 0.25, 1, 1.8, 0.33 | viewer_pack (`gltf`, `image`, `flate2` only behind its desktop `cook` feature, enabled by cli), viewer_view (`glam`) | The product viewer (ADR-2540): `regia viewer pack` reads part-kit GLBs, re-encodes textures for the web (JPEG/PNG) and measures gzip sizes; packages carry BLAKE3 hashes. The runtime reader (web, Android) pulls only blake3, glam, serde_json. |
| ring | 0.17.14 (Apache-2.0 AND ISC; the version rustls already resolves, no new crate in the lock) | asset (`content-signing`, which `content-http` turns on), cli | Ed25519 (RFC 8032) for signed content manifests (ADR-2710): `UnparsedPublicKey::verify` in the game, `Ed25519KeyPair` key generation and signing in `regia assets keygen` / `regia assets sign`. `default-features = false, features = ["alloc", "std"]`. Builds C and assembly through `cc` (already true of every `content-http` and crash `tls` build, Android included). Chosen over `ed25519-dalek` (ADR-2138's proposal) because it is already in the graph. |
| ureq (rustls), rustls, rcgen | 2.12, 0.23 (dev), 0.14 (dev) | asset_pipeline | `https://` shared cook caches (ADR-2710): a new edge of the approved ureq row, same configuration (`ring` provider, web PKI roots), redirects off; `REGIA_SHARED_COOK_CACHE_AUTH` is sent as the `Authorization` header and never logged. `rustls` and `rcgen` are dev-dependencies for the loopback TLS test, the same pins as `regia_asset`'s and `regia_crash`'s. |
| roxmltree | 0.20 (MIT OR Apache-2.0) | asset_pipeline | MaterialX (`.mtlx`) import (ADR-3341): a read-only DOM XML parser with no dependencies, DTDs refused and a node limit set (`ParsingOptions`). Already in the lockfile through fontconfig-parser, so no new crate is downloaded. Forbidden in runtime crates (check-layers). |
| sha2 | 0.11.1 (MIT OR Apache-2.0) | cli (transitive) | SHA-256 dependency of rmcp 3.9; ADR-4855 security update. |
| ureq, ufbx, openusd, resvg + usvg + tiny-skia, vorbis_rs + kira (existing rows, no new crate in the graph) | as above | asset_pipeline (optional, all in `default`) | ADR-7972: each is a `regia_asset_pipeline` feature, on by default so the editor and CLI build as before: `http_cache` = ureq (and through it rustls + ring), `fbx_import` = ufbx, `usd_import` = openusd, `svg_import` = resvg, usvg, tiny-skia, `audio_transcode` = vorbis_rs, kira. `rustls` and `rcgen` are dev-dependencies only. A player that cooks glTF at run time writes `default-features = false`; `cargo xtask check-layers` keeps that tree (resolved for the crate alone) free of ureq, rustls, rcgen, ring, ufbx, openusd, resvg, usvg and vorbis_rs. |

### Added in later milestones (not in the skeleton yet)

| Crate | Milestone | Notes |
|---|---|---|
| cargo-ndk (tool, `cargo install`) | M1 | Android builds. |
| cargo-llvm-cov 0.9 (tool, `cargo install --locked`; Apache-2.0 OR MIT) | ADR-2705 | `cargo xtask coverage`: measured line and function coverage of named crates. A tool, never a crate dependency; installed lane-private (`CARGO_INSTALL_ROOT=<tools>`) with the `llvm-tools-preview` rustup component in a private `RUSTUP_HOME`. |
| cargo-fuzz (tool, `cargo install --locked`, MIT OR Apache-2.0) and a nightly toolchain with `rust-src`, `llvm-tools-preview` | spec 97 § 10 (ADR-2723) | Tools, not dependencies: `cargo xtask fuzz` and `crates/regia_media/fuzz` only. The `fuzz/` crate's own `libfuzzer-sys` (MIT OR Apache-2.0 OR NCSA) sits outside the workspace, so it never reaches a build of the engine or `cargo deny`. |
| winresource | M16 (built: ADR-2645) | Windows exe icon/version resources. **0.1.34, MIT**, a `[build-dependencies]` entry of the generated game crates (the templates' `launcher/build.rs`, the quick crate's `build.rs`, the net game's `client/build.rs`) and, since ADR-20000, of `regia_editor` alone among engine crates (its `build.rs` embeds the editor's icon and version info into `regia-editor.exe` on Windows targets only, `default-features = false`, so without `toml`; no runtime crate links it): pulls `toml` 1.x (MIT OR Apache-2.0) and `version_check` (MIT OR Apache-2.0). The same build script uses `png` 0.18 and `toml` 1 (`std`, `parse`) to read `Regia.toml` and write the ICO; `regia_templates` has them as dev-dependencies to test that script. Needs `rc.exe` (Windows SDK) or `llvm-rc` (cross builds; `RC_PATH` overrides); without one the build warns and ships no resources. |
| ufbx | T2 (FBX import) | C-backed; pipeline only. |
| quinn 0.11.12 (MIT OR Apache-2.0) | T2 networking, feature `net-quic` on `regia_net` (off by default) | RFC 9221 datagrams on one loopback connection. Only new direct dependency; `default-features = false`, `rustls-ring` and `runtime-smol` only. The re-exported rustls provider creates the in-process Ed25519 certificate. No certificate generator, public CA store or platform verifier. Quinn drives its protocol tasks on Smol; Regia keeps its reliability and session crypto. The existing transitive async-executor lock entry is patched from yanked 1.14.0 to 1.14.1 (MIT OR Apache-2.0). See ADR-3970. |
| rerecast / landmass | T2 navigation | Currently on glam 0.32 / 0.30 — **not used**: `regia_nav` implements the Recast-style pipeline in-house (no new external dependency), see `docs/provenance.md`. |
| bvh | T2 lightmap baker | |
| cargo-mutants 27.1 (tool, `cargo install --locked`; MIT) | ADR-3389 | `cargo xtask mutants`: mutation testing of named crates (`docs/06` § 20). A tool, never a crate dependency; installed lane-private (`CARGO_INSTALL_ROOT=<repo>/tools`). |

## 2. Version coupling groups

Upgrade these **together**, in one PR, never individually:

1. **GPU/UI group:** `wgpu`, `naga_oil`, `egui`, `egui-wgpu`, `egui-winit`, `egui_dock`,
   `egui-snarl`, `transform-gizmo-egui`, `winit`. Rule: pick the newest egui-wgpu; it dictates wgpu
   and winit.
2. **Math/ECS group:** `glam`, `bevy_ecs`, `bevy_reflect`, `bevy_tasks`, `bevy_state`, `rapier3d`, `rapier2d`, `kira`. Rule: glam
   must resolve to exactly one 0.x version in `cargo tree -i glam` for runtime crates.

## 3. Upgrade policy

- Dependencies are upgraded deliberately, at most once per milestone, as a dedicated task with its own
  commit, after all tests pass on the old versions.
- `cargo update` (semver-compatible) is allowed any time; CI runs with the committed `Cargo.lock`.
- Never re-export a dependency's types in Regia Engine's public API unless the facade crate is the designated
  owner (`regia_ecs` for bevy types, `regia_core::math` for glam, `regia_render` for the subset of
  wgpu types advanced users need). This keeps future replacement possible.

## 4. Licence policy (`deny.toml`, created in M0)

Allowed: MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, Zlib, ISC,
Unicode-3.0, CC0-1.0, BSL-1.0, MPL-2.0 (file-level copyleft; acceptable as an unmodified dependency),
OFL-1.1 (fonts only), Ubuntu-font-1.0 (only for `epaint_default_fonts`, which egui pulls into the editor —
it is already in the skeleton's lockfile, so `deny.toml` needs this exception from the start),
CDLA-Permissive-2.0 (only for `webpki-roots`, the CA root list behind `regia_crash/tls`; ADR-1378; exception confirmed by the owner 2026-11-08: "mozilla is fine").
Denied: GPL, LGPL, AGPL, SSPL, any "non-commercial" licence. Also enforce:
no duplicate major versions of the coupling groups above in runtime crates; advisories database check.
`deny.toml` checks the graph with **every feature on** (`[graph] all-features = true`, ADR-1378): an
off-by-default feature ships as soon as a game enables it, so its dependencies get the same advisory and licence
checks as the default build. Dev-dependencies stay out of the gate (they never ship); the audit below covers them.

Ported source code is not a dependency and is not in `deny.toml`'s graph: each ported file carries its notice and
is listed in `docs/ported-sources.toml` (rendered into `THIRD_PARTY_NOTICES.md`). `regia_nav::tile_cache` and
`regia_nav::crowd` are ported from Recast & Detour (zlib, commit 9f4ce64458; ADR-1484); no crate was added.

### 4b. Advisory audit (2026-10-22, ADR-1378)

Run with cargo-deny 0.20.2 against the RustSec database of 2026-10-22, twice: the gate's `deny.toml`, and a strict
copy (all features, dev-dependencies included, the macOS targets added, no ignores). Every finding:

| Advisory | Crate (locked) | Reached through | Status |
|---|---|---|---|
| RUSTSEC-2026-0346, -0347, -0348, -0349 (vulnerabilities) | rustls 0.23.45 | `regia_crash/tls` (ureq), and `regia_crash`'s TLS test (dev) | **Fixed**: `cargo update` to 0.23.46. The gate missed them: `tls` is off by default and `deny.toml` checked default features only |
| RUSTSEC-2026-0382 (vulnerability) | rustls-webpki 0.103.15 | same | **Fixed**: 0.103.16 |
| RUSTSEC-2026-0381 (vulnerability) | webpki-roots 1.0.9 (via the 0.26.11 shim) | `regia_crash/tls` | **Fixed**: 1.0.10 drops the distrusted emSign root |
| RUSTSEC-2026-0194, -0195 (vulnerabilities) | quick-xml 0.31.0 | tiled 0.16 (Tiled importer, pipeline only) | Ignored with its ADR; tiled 0.16.0 is still the latest release. Input limits in `regia_asset_pipeline::tiled_import` |
| RUSTSEC-2024-0436 (unmaintained) | paste 1.0.15 | proc-macro via egui_dock (editor) and exr (importers; render-out EXR in `regia_agent`, ADR-1521) | Ignored; no maintained release; a proc-macro, so no runtime code |
| RUSTSEC-2026-0192 (unmaintained) | ttf-parser 0.25.1 | cosmic-text/fontdb (runtime UI), ab_glyph (Wayland decorations) | Ignored; 0.25.1 is the latest release; parses only fonts the project ships. The old "review in M7" note is replaced by a dated review |
| RUSTSEC-2026-0370, -0371, -0372 (unmaintained, informational) | zvariant 5.15.1, zbus_names 4.3.5, zvariant_derive 5.15.1 | zbus 5 → atspi 0.29 → `accesskit_unix` 0.21 (`accesskit` feature, off by default, ADR-1998) | Ignored: merged into zbus 6.0 as modules, not vulnerabilities; no `accesskit_unix` for accesskit 0.24 (the core egui 0.36 locks) is on zbus 6. Drop with the GPU/UI group bump that moves AccessKit |

Licences with every feature on: one new licence, CDLA-Permissive-2.0 (`webpki-roots` 0.26.11 and 1.0.10, Mozilla's
CA list as data). § 7 recorded it as MPL-2.0, which was true of older releases. It is a permissive data licence with
no copyleft; `deny.toml` allows it for that crate only. **Owner confirmed** this exception ("mozilla is fine", 2026-11-08; ADR-1378).
Bans and sources: clean with every feature on. Yanked crates: none.

How to repeat: `cargo deny --locked check` (the gate's view), then the strict view with a copy of `deny.toml` that
sets `exclude-dev = false`, clears `[advisories] ignore` and adds `aarch64-apple-darwin`:
`cargo deny --all-features --config <copy> check advisories`. Every finding of the strict run must be fixed or named
in the table above.

### 4d. Dependency audit since v0.2.4 (2026-11-01, ADR-2052)

Method: `Cargo.lock` of tag `v0.2.4` against the lock at `4306ead36` (v0.2.10 line); `cargo metadata --all-features` for
licences and the crates that pull each addition; `cargo deny check` (cargo-deny 0.20.2, RustSec database of 2026-11-01)
with the gate's `deny.toml`; and the strict view of § 4b (all features, dev-dependencies in, macOS target added, no
ignores). 810 → 913 `[[package]]` entries; nothing was removed. Results:

- **Licences, bans, sources:** `cargo deny check` is clean (`advisories ok, bans ok, licenses ok, sources ok`). Every
  addition is MIT, Apache-2.0, BSD-2/3-Clause or a dual of those (`fiat-crypto`: MIT OR Apache-2.0 OR BSD-1-Clause).
  The one non-registry source is `rav1d` (git, allowed in `deny.toml`).
- **Advisories:** the strict run reports exactly the seven ignored advisories of § 4b and no new one. The additions
  carry no advisory.
- **Yanked:** none (`yanked = "deny"` is on and passes against the sparse index).
- **Dependency bumps proposed:** none. The only ignores that could move are waiting on upstream (tiled for quick-xml,
  accesskit for zbus 6); ADR-2052 records that.

New crates, by the feature that reaches them (every row is already described in § 1 or § 7; this table is the audit view):

| Feature (default?) | Crates added (version) | Licence | Unsafe / native code | Notes |
|---|---|---|---|---|
| `regia_net` (default network): `snow` | snow 0.10.0, chacha20poly1305 0.10.1, chacha20 0.9.1, poly1305 0.8.0, blake2 0.10.6, curve25519-dalek 4.1.3 (+ `-derive`), fiat-crypto 0.2.9, aead, cipher, inout, universal-hash, digest 0.10.7, block-buffer 0.10.4, crypto-common 0.1.7, generic-array 0.14.7, cpufeatures 0.2.17, opaque-debug | MIT/Apache-2.0; curve25519-dalek BSD-3-Clause | RustCrypto and dalek use limited `unsafe` (SIMD backends, `cpufeatures` CPUID, `generic-array` transmutes); no C | Unaudited as a set apart from chacha20poly1305 (§ 1 `snow` row). Owner item for the pre-release security review. Pure Rust: no `ring`. |
| `regia_cli`, `regia_net`: `signal-hook` | signal-hook 0.3.18 | Apache-2.0/MIT | Signal handlers (`unsafe` by nature), small | Unix only. |
| `regia_procgen`: `i_overlay` | i_overlay 9.1.0, i_float 5.0.0, i_shape 5.0.0, i_tree 0.20.0, i_key_sort 0.12.0 | MIT / MIT OR Apache-2.0 | Safe Rust apart from small index tricks; single-author | Integer-grid API only; multithreading off. |
| `regia_render`: `libm` | libm 0.2.16 (already in the tree via i_float) | MIT OR Apache-2.0 | Pure-Rust port of musl's math library, safe, no `build.rs`, no system library | Software `f32` transcendentals (`soft_math`) for outputs that must be bit-identical on every OS: the CPU sun, atmosphere LUTs, `display_radiance` and the glass crack pages (audit G render5 determinism, ADR-4781). `f32::sqrt` stays the builtin (IEEE correctly rounded). |
| `lua` (off): `regia_script_lua` | mlua 0.12.2, mlua-sys 0.13.0, lua-src 551.0.2, luajit-src 210.7.4, bstr 1.13.1, which 8.0.6 (lock-only for luajit) | MIT | **FFI plus bundled C** (Lua 5.1.5 built by `cc`); `mlua` wraps it safely, `mlua-sys` is raw | Highest-risk addition: a script VM. Sandbox policy lives in `regia_script_lua`. `luajit-src` and `which` are locked, never compiled. |
| `media-codecs` (off): `rav1d`, `unsafe-libopus`, `lewton` | rav1d 1.1.1 (git `2e7216c2`), av-data, atomig, strum 0.27, assert_matches, to_method, nasm-rs (build only, not invoked), unsafe-libopus 0.2.0, lewton 0.10.2 | BSD-2-Clause, BSD-3-Clause, MIT, CC0-1.0 | **Unsafe-heavy by design:** `rav1d` is a port of dav1d with `unsafe` and aarch64 assembly; `unsafe-libopus` is c2rust output (the whole codec is `unsafe`). `lewton` is safe. | Decoders for untrusted media: ADR-1593 allows this class. Fuzz targets and the panic guard are in `regia_media`. `rav1d` is a git pin; move to a crates.io release when 1.1.1 is published. |
| `accesskit` (off): `regia_platform`, `regia_ui` | accesskit_unix 0.21.1, accesskit_windows 0.32.1, accesskit_macos 0.26.3, accesskit_atspi_common 0.18.1, accesskit_consumer 0.35/0.36/0.38, atspi 0.29, atspi-common, atspi-proxies, zbus 5.19.0 (+ names, lockstep, xml, macros), zvariant 5.15.1 (+ derive, utils), zcheapstr, endi, async-io/lock/process/signal/broadcast/recursion, blocking, piper, ordered-stream, uds_windows, enumflags2, serde_repr, phf 0.13, siphasher, memoffset, tempfile 3.27, glob | MIT / Apache-2.0 | `zbus` is `#![forbid(unsafe_code)]`-style safe except its socket layer; `async-io` and `async-signal` use `unsafe` for polling and signals | Three RUSTSEC "unmaintained" notices (zvariant, zbus_names, zvariant_derive), ignored with ADR-1998. Drop when AccessKit moves to zbus 6. |
| `regia_dlss` (off, build time): `bindgen` | bindgen 0.73.2, clang-sys 1.9.1, cexpr, glob, prettyplease 0.3.0 | BSD-3-Clause / Apache-2.0 / MIT | Build-time only; links libclang | Needs libclang on the build host. Not in shipped binaries. |
| `media-encode` (off; on in `regia_cli`): `rav1e` | rav1e 0.8.1, v_frame 0.3.9, av1-grain 0.2.5, av-scenechange 0.14.1, y4m 0.8.0, bitstream-io 4.10.0, aligned-vec 0.6.4, aligned 0.4.3, as-slice 0.2.1, equator 0.4.2 (+ `-macro`), maybe-rayon 0.1.1, simd_helpers 0.1.0, arg_enum_proc_macro 0.3.4, noop_proc_macro 0.3.0, new_debug_unreachable 1.0.6, pastey 0.1.1, no_std_io2 0.9.4, nom 8.0.0, built 0.8.1 (build); lock-only under cfg(fuzzing): arbitrary 1.5.0, libfuzzer-sys 0.4.13, interpolate_name 0.2.4, rand 0.9.5, rand_chacha 0.9.0, rand_core 0.9.5, ppv-lite86 0.2.21 | BSD-2-Clause (rav1e, v_frame, av1-grain), MIT, MIT OR Apache-2.0 | Rust with `unsafe` in its SIMD and frame code; no C compiled (`asm` off, `libfuzzer-sys` only under `cfg(fuzzing)`) | Offline AV1 encoder for render-out and tools (ADR-3369). The Opus encoder reuses `unsafe-libopus`. |
| `cross-platform-determinism` (off): `regia_core` → `glam/scalar-math`, `glam/libm` | none (libm 0.2.16 is already in the tree and already glam's locked optional dependency) | MIT OR Apache-2.0 | None (glam's scalar path; pure-Rust libm) | glam without SSE2/NEON and with libm's `sinf`/`cosf`/`atan2f`… instead of the platform's, so crowd and pose math give the same bits on x86_64 and aarch64 and on every OS (ADR-5176). |

"Unsafe / native code" is the auditor's reading of each crate's documentation and known design, not a `cargo geiger`
count (geiger is not installed on the lane host). The lead can run `cargo geiger --all-features -p regia_media` before
the pre-release security review.

New duplicate versions since v0.2.4 (all from the same new subtrees; `multiple-versions` stays `allow`):

| Crate | Versions | Why |
|---|---|---|
| digest, block-buffer, crypto-common, cpufeatures, chacha20 | 0.10 / 0.12 (digest 0.10.7 / 0.11.3, block-buffer 0.10.4 / 0.12.1, crypto-common 0.1.7 / 0.2.2, cpufeatures 0.2.17 / 0.3.1, chacha20 0.9.1 / 0.10.2) | `snow` 0.10 uses the RustCrypto 0.10 generation; the older tree uses the 0.11/0.12 generation. Resolves when `snow` moves to it. |
| zerocopy (and `-derive`) | 0.7.35 / 0.8.59 | `rav1d`; already a reviewed duplicate (§ 1 `rav1d` row). |
| accesskit_consumer | 0.35.0 / 0.36.0 / 0.38.0 | One per AccessKit platform adapter (upstream release set). |
| nom, rand, rand_core | 7.1.3 / 8.0.0; 0.9.5 / 0.10.x | `rav1e` 0.8 (`media-encode`, ADR-3369): nom 8 through av1-grain; rand 0.9 only under `cfg(fuzzing)`, never compiled. |

The full duplicate list (`cargo tree -d --workspace --all-features -e normal`, host target, at the
2026-11-10 lock) is 57 crate names; the older ones (glam
0.32 in the editor gizmo, rustix, getrandom, thiserror 1/2, syn 2/3, base64, and so on) predate v0.2.4 or follow
the coupling groups of § 2, which `cargo xtask check-versions` checks per runtime crate.

To repeat: `cargo deny check` and the strict recipe in § 4b; diff `git show v0.2.4:Cargo.lock` against the current lock.

**Delta since the 2026-11-01 audit (2026-11-06, deps-audit-v038).** `Cargo.lock` at `b7defd11a` (the security-deps-audit
commit) against this line; `cargo metadata` and `cargo deny check` (cargo-deny 0.20.2) on the build server. 913 → 923
`[[package]]` entries; the delta is two third-party changes plus first-party version moves:

| Change | Crate (locked) | Reached through | Audit |
|---|---|---|---|
| Added | socket2 0.6.5 (MIT OR Apache-2.0) | tokio `net` in `regia_cli` (`regia mcp --http`, § 1 row exists) | No build script, no proc macro, no vendored C; `unsafe` limited to socket options; no RustSec advisory; maintained |
| Bumped | async-executor 1.14.0 → 1.14.1 (MIT OR Apache-2.0) | bevy_tasks | 1.14.0 is yanked; the sbom lane's lock-only fix (§ 4e); no build script; advisory-free |
| Version move | workspace crates 0.2.10 → 0.2.16; 8 new sample members | — | First-party; no new external crate enters the graph |

`cargo deny check` (all features, committed `deny.toml`, cargo-deny 0.20.2 on the build server, RustSec database of
2026-11-06): **advisories ok, bans ok, licenses ok, sources ok**; `deny.toml` is unchanged and no yanked crate
reported. The delta's additions carry no advisory (socket2, async-executor 1.14.1), and the strict view of § 4b
was clean on 2026-11-05 (§ 4e) with no advisory touching the 2026-11-06 delta. New duplicate versions created by
the delta: none. The duplicate ratchet (`docs/ci/duplicate-deps.txt`, ADR-3301) records the 53 standing
duplicates with
their removal paths (2026-11-10, fixrev-03-k: `--all-features` across the four shipping targets, so
platform-only and optional-feature duplicates are ratcheted too; the host-only all-features list is 57 names);
lock-only unification was attempted for each in this lane and every attempt is refused by a
parent crate's semver cap, so none moved.

### 4e. Audit tooling: SBOM, asset licences, unused dependencies, duplicates (2026-11-05, ADR-2760)

What runs, when, and who reads it:

| Check | Command | When | Who reads it |
|---|---|---|---|
| Licences, advisories, bans, sources | `cargo deny check` (`deny.toml`, every feature on) | Merge gate; before each release tag; the advisory database is fetched fresh | Lead reads the gate log; any new advisory needs a fix or an ADR before it is ignored |
| Third-party notices and ported sources | `cargo xtask notices --check` (every feature, ADR-5556; offline while the resolve seal is fresh, ADR-5903) | `cargo xtask preflight`, merge gate | Lane that changed a dependency regenerates the file (`cargo xtask notices`, which also refreshes the seal) |
| Binary-asset licence record | `cargo xtask notices --check` (`docs/asset-licences.toml`); `--strict` also fails on `unknown` | Same; `--strict` before a release tag | Owner settles every `unknown` row (the report lists them) |
| Bill of materials | `cargo xtask sbom --profile engine\|game` (`game`: add the game's `--features crate/feature`, ADR-5903) | Per release; attach the `game` document to a game build | Owner and counsel (licence review), customers who ask for an SBOM |
| Unused dependencies | `cargo-machete --with-metadata` (installed outside the repository, not a project dependency), then confirm each hit | Before each release tag and after a large merge | Lead; a hit is a candidate, never an instruction (see below) |
| Duplicate versions | `cargo tree -d`, `cargo xtask check-versions` | Check-versions at the gate; the table below at each release | Lead; a duplicate inside a coupling group (§ 2) is a defect |
| New duplicate versions | `cargo xtask duplicate-deps` (`docs/ci/duplicate-deps.txt`) | `cargo xtask preflight`, merge gate | Lane that changed a dependency; a new duplicate fails until unified or recorded with a reason (ADR-3301) |

**Confirming an unused-dependency hit.** `cargo-machete` reads source text, so it misses macro-expanded paths, build
scripts behind a feature, and dependencies kept only to pin a feature. Read the crate, remove the line, then compile:
`cargo check --workspace --all-targets`, the Android check (`cargo ndk -t arm64-v8a check -p regia`), and the feature
matrix (`cargo xtask check-features`). What stays: `regia_editor` in the hot-reload and extension fixtures (a feature
anchor that keeps `regia_dylib` from relinking, specs/20 § 1), `android-activity` in `regia_platform` (the workspace
pins it with `game-activity`, which `winit` would otherwise resolve differently), `bindgen` and `cc` in `regia_dlss`
(build script, `sdk` feature), `profiling` in `lookdev` (optional, feature `profile-with-tracing`).

**Result of 2026-11-05.** `Cargo.lock` has 921 packages before and after: every removal was a dependency edge between
workspace crates or a crate that another crate still needs. Removed from crate `Cargo.toml` files: `regia_macros`
(regia), `regia_ecs` (regia_2d, regia_input, regia_script_lua, regia_xr, and the samples mm_locomotion and sim_bridge),
`blake3` and `crossbeam-channel` (regia_core), `regia_core` (regia_crash and ui_gallery; the `regia_fluid` edge was
also removed on this lane's base but is restored at the merge — the coupling plugin reads `regia_core::math`,
`crates/regia_fluid/src/coupling.rs`), `uuid` (regia_ecs),
`anyhow` (regia_editor), `glam` (regia_ui), `thiserror` (regia_xr), `postcard` (anim_lab), `regia_scene` (boot_probe),
`regia_animation` (render_lab) and `png` (xtask); `proptest` left `[workspace.dependencies]` (no member used it).
`THIRD_PARTY_NOTICES.md` is regenerated by the lane's tooling; its only content delta is the `async-executor` row
moving 1.14.0 → 1.14.1 (the yanked-version lock-only fix below).

**Duplicates.** `cargo xtask check-versions` is green: bevy_ecs/reflect/tasks 0.20.0, glam 0.33.12, kira 0.12.5, naga
30.0.1, naga_oil 0.23.0, rapier3d 0.36.0, wgpu 30.0.1 and winit 0.30.13 resolve to one version in every runtime crate,
so there is no coupling-group defect. The other duplicate names in `Cargo.lock` (67 names; the `windows*`, `objc2*` and
`redox_syscall` families are platform-only and not listed). None is removable from this repository without a version
move of a dependency; ADR-2052 (3) and `cargo deny` propose no patch-level bump, so none was applied.

| Crate | Versions | Pulled by | Removable? |
|---|---|---|---|
| glam | 0.30, 0.31, 0.32 (also 0.33.12) | `nalgebra` conversion features via `glamx` (rapier3d, parry3d); 0.32 also `transform-gizmo` (editor, § 1) | Not a coupling defect: runtime trees resolve 0.33.12 only. Drops when nalgebra/glamx or the gizmo crate move |
| thiserror | 1.0.69 / 2.0.21 | 1.x: calloop, ndk, smithay-client-toolkit (winit stack) | With the winit/calloop move |
| syn | 2.0.119 / 3.0.6 | 2.x: bevy and older derive crates | With upstream releases |
| rustix, linux-raw-sys | 0.38.44 / 1.1.5 | 0.38: winit's calloop and smithay stack | With the winit move |
| calloop, calloop-wayland-source, smithay-client-toolkit, smol_str 0.2 | two each | winit 0.30.13 (old) against cosmic-text / smithay-clipboard (new) | Coupled to winit (AGENTS.md § 5); not touched |
| rustc-hash | 1.1.0 / 2.1.3 | 1.x: naga, naga_oil, wgpu-core | With the wgpu bump |
| codespan-reporting | 0.12.0 / 0.13.1 | naga_oil / naga | With naga_oil |
| base64 | 0.13.1 / 0.22.1 / 0.23.1 | gltf; regia_asset_pipeline, regia_cli, tiled, ureq; pem, rmcp | Pipeline-only (0.13); rest with upstream |
| minidump-common, scroll, scroll_derive, num-derive | two each | minidump-writer (0.26) against minidump 0.27 (cli-polish-crash moves the minidump crates) | Re-check after that lane |
| quick-xml | 0.31.0 / 0.41.0 | tiled 0.16 (pipeline only, advisory ignores); wayland-scanner | Waits for a newer tiled (ADR-2052) |
| digest, block-buffer, crypto-common, cpufeatures, chacha20 | two each | `snow` 0.10 (RustCrypto 0.10 generation) against blake3, sha1, rand | When `snow` moves (ADR-2052 (3)) |
| zerocopy | 0.7.35 / 0.8.59 | rav1d (media-codecs, off by default) | With rav1d 1.1.1 on crates.io |
| accesskit_consumer | 0.35 / 0.36 / 0.38 | one per platform adapter | Upstream release set |
| webpki-roots | 0.26.11 / 1.0.10 | ureq (shim over 1.x) in `regia_crash/tls` | With ureq |
| itertools, hashbrown, heapless, getrandom, bitflags, libloading, skrifa, read-fonts, harfrust, font-types, miniz_oxide, downcast-rs, jni-sys, darling, spin, hash32, embedded-io | two or three | text stack (cosmic-text against epaint/egui), bevy, postcard, criterion (dev) | Each follows its parent crate |

**`cargo deny check`, 2026-11-05** (cargo-deny 0.20.2, run on the workstation through `the workstation-build.sh`):

- `cargo deny check` (all features, as `deny.toml` is committed): **advisories ok, bans ok, licenses ok, sources ok**, after one fix. The first run failed on one finding that is not an advisory text: `async-executor` 1.14.0 is **yanked** (`yanked = "deny"`; pulled in through `bevy_tasks`). `cargo update -p async-executor` moved it to 1.14.1 (patch level, `Cargo.lock` only, two lines).
- Default features (a copy of `deny.toml` with `all-features = false`): advisories ok, bans ok, licenses ok, sources ok. It prints "no crate matched" warnings for the ignores whose crates only the off-by-default features reach (`zvariant` and friends via `accesskit`), which is why the committed file keeps `all-features = true`.
- Strict view (§ 4b: dev-dependencies in, no ignores): exactly the seven already-ignored advisories (paste, ttf-parser, quick-xml 0.31 twice, zvariant, zbus_names, zvariant_derive) and nothing new. No new licence. `deny.toml` is unchanged.

How to repeat: `cargo deny check` (all features, from `deny.toml`); the default-feature view is the same file with
`all-features = false` in a copy; the strict view is § 4b. `cargo xtask sbom --profile game --out sbom-game.cdx.json`
writes the bill; `python3` with `jsonschema` validates it against CycloneDX's `bom-1.5.schema.json` (done once, 0 errors
on both profiles on 2026-11-05).

### 4a. Bundled default fonts (`crates/regia_ui/assets/fonts`)

The runtime UI embeds a default font set so unpinned text covers Latin, Arabic, CJK (incl. kana,
Hangul) and emoji on every platform — Android loads no system fonts at all (neither fontdb nor
cosmic-text has an Android loader), and desktop fallback only reaches installed super-families.
All OFL-1.1; licence texts ship next to the files; `subset.py` regenerates them (see its docstring
for the upstream sources and the derivation of each charset):

| Font | Family (fallback name) | Subset basis | Size |
|---|---|---|---|
| Inter Regular/Bold/Italic | Inter (sans-serif default) | full | 412/420/417 KiB |
| Noto Naskh Arabic | Noto Naskh Arabic | full Arabic block, joining GSUB kept | 142 KiB |
| Noto Sans SC | Noto Sans SC | Jun Da modern-Chinese top-2000 han (97.1 % of running text), full kana, CJK punctuation, fullwidth | 827 KiB |
| Noto Sans KR | Noto Sans KR | all 2350 KS X 1001:1998 Hangul syllables, compatibility jamo | 429 KiB |
| Noto Sans Hebrew | Noto Sans Hebrew | Hebrew block and presentation forms, layout tables kept (`subset_indic.py`, ADR-1842) | 16 KiB |
| Noto Sans Devanagari | Noto Sans Devanagari | Devanagari, extended and Vedic marks, GSUB/GPOS kept (`subset_indic.py`) | 162 KiB |
| Noto Sans Thai | Noto Sans Thai | Thai block, GSUB/GPOS kept (`subset_indic.py`) | 20 KiB |
| Noto Color Emoji | Noto Color Emoji | ~138 common UI emoji + regional indicators for common flags (CBDT bitmaps, GSUB ligatures) | 655 KiB |

Recorded in `THIRD_PARTY_NOTICES.md` (generated by `cargo xtask notices`). Not covered by
default (projects bundle their own): other Indic scripts, rare han,
uncommon emoji.

### 4c. Ported source without a new dependency (ADR-086)

Source ported into a crate (not linked as a crate) is listed in `docs/ported-sources.toml`, carries the upstream notice
in each file's header and is rendered into `THIRD_PARTY_NOTICES.md` by `cargo xtask notices`. Lightmap UV atlas:

| Upstream | Licence | Ported into | ADR |
|---|---|---|---|
| xatlas (jpcy/xatlas `f700c77`; includes thekla_atlas / NVIDIA Texture Tools and Fast-BVH notices) | MIT | `regia_asset_pipeline::atlas` (pipeline only) | ADR-1483 |
| Direct Order 27, the owner's own game (`[[do27]]` rows: crate, path, lines, commit, mark, class; no notice rendered) | the owner's own code (copy exception, owner 2026-11-16) | planned: `regia_detsim`, `regia_world_geom`, `regia_nav::det`, `regia_net` lockstep, `regia_framework::rules` | ADR-8012 |

## 5. Features

Umbrella `regia` crate features (all default-on except where noted):
`physics`, `audio`, `animation`, `ui`, `framework`, `terrain` (from M13), `vfx` (from M14),
`hot-reload` (off; editor/dev only), `tracy` (off), `dev-assets` (on in debug; reads the cooked cache +
watches files), `agent` (on in dev builds and the editor; off under `shipping` unless the game opts in —
`specs/23` § 2), `steam` (off; `specs/24`), `crash` (off; opt-in crash + match-log upload, `specs/21` § 5,
re-exporting `regia_crash` as `regia::crash`; enable `regia_crash/tls` for `https://`),
`shipping` (off; strips debug UI, dev console, file watching,
asserts that are not invariants), `media` (off; video playback as `regia::media`, ADR-1670) and
`media-codecs` (off; adds the AV1, Opus and Vorbis decoders, `regia_media/video`, ADR-1869) and
`media-codecs-asm` (off; adds the x86 AV1 assembly, needs `nasm` on PATH, ADR-1869 § 2),
`voice-opus` (off; Opus for voice chat through `regia::media`, ADR-1911),
`voice-net` (off; voice chat over the net runtime, `regia_net/voice`, ADR-1911) and
`accesskit` (off; the screen-reader OS bridge through AccessKit, needs `ui`, ADR-1998).
`media-encode` (off; the offline AV1/Opus WebM encoder for render-out and tools, `regia_media/media-encode` and
`regia_agent/media-encode`, ADR-3369; never in a shipped game; `regia_cli` turns it on by default).

## 6. Build speed (iteration is a feature)

Rust compile times are the most common complaint about Rust game development. Budgets, measured on
the desktop reference machine and reported by CI (`cargo build --timings` artifacts):

| Build | Budget |
|---|---|
| Incremental rebuild of a game crate after a one-line change (dev profile) | ≤ 5 s |
| Hot reload visible in the running editor after save | ≤ 10 s |
| Clean build of the editor (dev) | ≤ 6 min |
| Clean shipping build of a sample | ≤ 15 min |

Settings (M0):
- `[profile.dev] debug = "line-tables-only"` for workspace crates; dependencies at `opt-level = 3` (set).
- Linkers: `rust-lld` (the default on x86_64 Linux since Rust 1.90 in rustup toolchains) or `mold`/`wild` via
  `.cargo/config.toml` when installed; `lld-link` on Windows; Apple's default linker on macOS.
- `split-debuginfo = "unpacked"` on Linux/macOS dev builds.
- Optional: Cranelift codegen backend for game crates in dev where the toolchain provides it; `sccache`
  for CI and clean builds.
- Keep the game crate small and the engine prebuilt: the editor and engine are compiled once; only the
  game crate recompiles during iteration (`specs/20`).
- Any change that regresses a budget by more than 20% needs an ADR.

## 7. Dependencies added by the revised plan (not in the skeleton until their milestone)

| Crate | Milestone | Notes |
|---|---|---|
| rmcp 3.9 (Apache-2.0) | M2 | Official Rust MCP SDK, for `regia mcp` |
| rmcp 3.9 (Apache-2.0) | M2 | Official Rust MCP SDK, for `regia mcp`; security update in ADR-4855 |
| serde_json | M2 | Agent protocol (JSON-RPC 2.0); `regia_render` sizes the GPU breadcrumb report against its 64 KiB cap (spec 54). Feature `float_roundtrip` (ADR-1917): numbers parse correctly rounded, so a float an agent sends arrives exactly and re-encodes to the same text |
| png 0.18 | M2 | PNG encoding for agent `screenshot` in runtime dev builds (`image` stays pipeline- and test-only, `06` § 7) |
| tokio-tungstenite or tungstenite | M2 | Agent protocol WebSocket transport (pick in M2; ADR) |
| subsecond | M2 spike | Evaluated (bevy_ecs feature `hotpatching`). Not adopted and not a workspace dependency (ADR-015). |
| steamworks 0.13 (MIT/Apache) + Steamworks SDK (Valve agreement) | M19 | `deny.toml` exception, ADR |
| cpal input streams (already via kira's cpal) | M8 | Voice capture |
| ureq 2.12 (MIT OR Apache-2.0) + rustls 0.23 (`ring`, Apache-2.0/ISC/MIT) + webpki-roots 0.26 (CDLA-Permissive-2.0 since 0.26.11; MPL-2.0 before) | crash-upload (DO27 alpha blocker, specs/21 § 5) | **Approved (lead decision, ADR-033).** HTTPS POST for the opt-in `regia_crash` uploader, behind its off-by-default `tls` feature. `ureq` `default-features = false, features = ["tls"]` uses rustls with the `ring` provider — no native-tls/openssl, no cmake/C build, cross-compiles to Android. `http://` still uses the dependency-free `std::net` transport (for local dev servers and the offline tests). `cargo deny` checks it in the gate since ADR-1378 (`all-features = true`); before that the feature was outside the checked graph, and the 2026-10-22 audit found six rustls/webpki advisories and the CDLA-Permissive-2.0 licence there (§ 4b) |
| oidn 2.5.1 (MIT) + Intel Open Image Denoise 2.5.1 native library (Apache-2.0, not vendored) | oidn-preview (oss-survey3 item 10) | **Tools only.** Optional dependency of `regia_pt` behind its off-by-default `denoise` feature (re-exposed as `regia_agent/denoise`, which implies `reference-trace`): OIDN previews of low-spp reference traces (`seeing.reference_trace(denoise: true)`, `pt_denoise`). Never a runtime crate's dependency, never on Android. Default `pkg-config` feature only — the crate's `bundled` feature (downloads a release in `build.rs`) is not used. Build: `OIDN_DIR` (or pkg-config) points at an OIDN 2.5+ install; run: `LD_LIBRARY_PATH=$OIDN_DIR/lib`. Farm: `<build-dir>` (the official `oidn-2.5.1.x86_64.linux.tar.gz`, sha256 `743c3e2a…37cd6`, matching oidn-rs's pinned hash). `cargo deny` checks it in the gate (`all-features = true`, ADR-1378; MIT, on the § 4 allow-list) |
| ufbx 0.11.5 (MIT OR Unlicense; bundles the ufbx 0.23.1 C parser, Copyright (c) 2020 Samuli Raivio, same licence) | fbx-import (v0.1.28, ADR-1602) | **Pipeline only.** Dependency of `regia_asset_pipeline` (cook, CLI, editor) for FBX import; never a runtime crate's dependency (`xtask/src/layers.rs` `RUNTIME_FORBIDDEN_EXACT`), never in a shipped game, never on Android. Pinned `=0.11.5`; its `build.rs` compiles `ufbx.c` with `cc` (no system library, no download). Used under the MIT alternative (on the § 4 allow-list; Unlicense is the other). The crate package does not include the LICENSE file; the text was confirmed from github.com/ufbx/ufbx and github.com/ufbx/ufbx-rust on 2026-10-28. Untrusted-input caps and the Rust pre-scan run before the C parser (ADR-1602, `book/agents/fbx-import.md`) |
| i_overlay 9.1 (MIT OR Apache-2.0; with its i_float, i_shape, i_tree and i_key_sort helper crates by the same author, MIT; i_float pulls the already-locked pure-Rust `libm`, used by its float adapter, not by the integer API Regia calls) | spec 64 polygons (sprint b18, ADR-1664) | Runtime dependency of `regia_procgen` only: polygon union, intersection, difference and xor on the 1 mm integer grid (`regia_procgen::polygon`, i64 engine). Pure Rust, no `build.rs`, no system library; no cargo feature gate because `Anchored` runtime generation uses it. `allow_multithreading` stays off (rayon not pulled; results single-threaded and deterministic). Fallback named by the spec if it ever has to go: a Martinez-Rueda sweep on the same grid behind the same module API |
| rav1d 1.1.1 (BSD-2-Clause; git rev `2e7216c2`, until 1.1.1 is on crates.io) | media-codecs (ADR-1593, ADR-1869) | **Runtime, optional.** AV1 decoding in `regia_media` behind its `av1` feature (part of `video`; `regia/media-codecs`, off by default). The Rust port of dav1d by the Prossimo project (memorysafety/rav1d), used through its safe Rust API (`Decoder`, `Picture`) by `regia_media::av1`, the only module that calls it; 1.1.0 from crates.io panicked across its C ABI on a fuzzed stream (ADR-1869 § 1). `deny.toml` allows this one git source. `default-features = false` with `bitdepth_8` and `bitdepth_16`; on aarch64 also `asm`, `asm_arm64_dotprod`, `asm_arm64_i8mm`, `asm_arm64_sve2` (GNU `.S` files assembled by `cc` with the target clang). x86 assembly is off: it needs NASM, which ADR-1593 § 5 keeps out of the build. Pulls `av-data` (MIT, the color enums of the API), `atomig`, `static_assertions`, `to_method` (CC0-1.0), `assert_matches`, `zerocopy 0.7` (a reviewed duplicate in `deny.toml`) and the build-only `nasm-rs` (never invoked without x86 `asm`). Builds with cargo alone for Linux, Windows MSVC (`cargo xwin`), Android aarch64 and macOS |
| unsafe-libopus =0.2.0 (BSD-3-Clause) | media-codecs, voice-real (ADR-1593, ADR-1869, ADR-1911) | **Runtime, optional.** libopus 1.3.1 (Xiph.Org, BSD-3-Clause) translated to unsafe Rust with c2rust, no C toolchain and no cmake (the published libopus `-sys` crates all build with cmake, which spec 59 § 3 rejects). In `regia_media`: Opus decoding of media tracks behind `opus` (wrapped by `regia_media::opus`) and voice chat, encoder and decoder, behind `voice-opus` (`regia/voice-opus`, off by default; wrapped by `regia_media::voice_opus`, which reaches `regia_audio::voice_chat` through `VoiceCodecProvider`, so `regia_audio` links no codec). Pulls `arrayref` (BSD-2-Clause) and `num-complex` (MIT OR Apache-2.0) |
| lewton 0.10.2 (MIT OR Apache-2.0) | media-codecs (specs/50 § 9, ADR-1869) | **Runtime, optional.** Vendored and patched at `vendor/lewton` (ADR-2164). Vorbis decoding of media tracks in `regia_media` behind its `vorbis` feature; safe Rust, `default-features = false` (no Ogg layer: WebM blocks carry raw Vorbis packets). Pulls `tinyvec` and `byteorder` (both already in the tree). `symphonia` stays out of `regia_media` |
| vorbis_rs 0.5.6 (BSD-3-Clause) + aotuv_lancer_vorbis_sys 0.1.6 + ogg_next_sys 0.1.5 (BSD-3-Clause; bundle the Xiph libvorbis/libogg C sources with the aoTuV and Lancer patches, Copyright (c) Xiph.Org Foundation and contributors, same licence) | audio-cook-transcode (v0.1.28, ADR-3230, ADR-1593) | **Pipeline only.** Dependency of `regia_asset_pipeline` (cook, package) for the Ogg Vorbis encoder; never a runtime crate's dependency (`xtask/src/layers.rs` `RUNTIME_FORBIDDEN_EXACT`), never in a shipped game, never on Android (the cook runs on the desktop host). `default-features = false`: no random stream serial, so the output is byte-reproducible. The `build.rs` of the `-sys` crates compiles the bundled C with `cc` (no system library, no download). The crates' LICENSE texts were confirmed from github.com/ComunidadAylas/vorbis-rs on 2026-11-06 |
| mlua 0.12.2 (MIT) + mlua-sys 0.13 (MIT) + lua-src 551.x (MIT; bundles the PUC-Rio Lua 5.1.5 C sources, Copyright (C) 1994-2012 Lua.org, PUC-Rio, MIT) + bstr 1 (MIT OR Apache-2.0, new in the tree) | lua-mods (v0.1.28 sprint e5, ADR-1751) | **Optional runtime crate.** Dependency of `regia_script_lua` only, reached through the facade's off-by-default `lua` feature; the agent server and every other runtime crate stay free of it. Pinned `=0.12.2`, features `lua51`, `vendored` (lua-src compiles Lua 5.1.5 with `cc`: no system Lua, no pkg-config, no download), `send` (a state is a Send + Sync resource). Builds with the platform C compiler on Linux, Windows (MSVC, `cargo xwin` clang-cl) and Android aarch64 (NDK clang). Its other dependencies (either, num-traits, parking_lot, rustc-hash 2, libc, cc, cfg-if, pkg-config) are already locked. `Cargo.lock` also lists mlua-sys's optional `luajit-src` 210.x (MIT) and `which` 8 (MIT), never compiled with these features (cargo locks optional dependencies of every feature). The Lua licence text ships as `regia_script_lua::LUA_LICENSE_NOTICE` because the notices generator lists crates, not the C sources inside lua-src. |
| accesskit 0.24.1 + accesskit_unix 0.21.1 + accesskit_windows 0.32.1 + accesskit_macos 0.26.3 (all MIT OR Apache-2.0); accesskit_consumer 0.36 (MIT OR Apache-2.0, `regia_ui` dev-dependency) | screen-reader OS bridge (gap audit D12, ADR-1998; the owner moved AccessKit off the Parked list on 2026-10-31) | **Runtime, optional.** `regia_platform` and `regia_ui` feature `accesskit`, `regia` feature `accesskit` (forwarded by `regia_dylib`), off by default. `accesskit` is the version egui 0.36 already locks; the adapters are target-gated: `accesskit_unix` on Linux (`default-features = false`, `async-io`; AT-SPI over `zbus` 5 with `atspi` 0.29, `async-io`, `async-executor` and their helpers, all MIT or MIT OR Apache-2.0, new in the tree), `accesskit_windows` on Windows (`windows` 0.62, already locked), `accesskit_macos` on macOS (`objc2` 0.5, already locked). No Android adapter. `accesskit_consumer` comes in three versions (0.35 via the Windows adapter, 0.36 via the Unix one and the tests, 0.38 via the macOS one; the upstream release set); `multiple-versions` is `allow`. Coupled to egui: bump with the GPU/UI group (§ 2). |
| regex 1 (MIT OR Apache-2.0) | sprint perf-budget-harness (ADR-2704) | **Dev tool only**, dependency of `xtask`: `cargo xtask perf-budgets` matches the metric regex of each `docs/perf/budgets.ron` entry. Already in the lock file through criterion and tracing-subscriber, so no new crate enters the tree. |
| rav1e =0.8.1 (BSD-2-Clause) | video-encode-av1 (ADR-1593, ADR-3369) | **Tools, optional.** AV1 encoding in `regia_media::encode::av1` behind `media-encode` (off by default; `regia/media-encode`, `regia_agent/media-encode`, on by default in `regia_cli` for `regia media encode`). Pure Rust from Xiph/Mozilla, used through its safe API. `default-features = false` plus `threading` (rayon through `maybe-rayon`); `asm` stays off because rav1e's x86 kernels need NASM (ADR-1593 § 5). Chosen over libaom and SVT-AV1 (better and faster respectively, but cmake + NASM C builds) with the measurements in ADR-3369. Encoding is for render-out and tools only, never a shipped game's runtime path (ADR-1593 § 1) |
| objc2 0.6.4 (MIT), feature `disable-encoding-assertions` | mac-pass-v0216 (ADR-3373) | **Tests only, macOS only.** A `[target.'cfg(target_os = "macos")'.dev-dependencies]` entry of `regia_render` that turns off objc2's debug-build check of every Objective-C message encoding: the check allocated on each Metal call (about 1500 of a steady frame's 1873 heap allocations on the M5), so the allocation-count tests measured objc2 there, not the renderer. objc2 0.6.4 is already locked through wgpu-hal; no new crate. Release builds never run the check, and builds without tests do not activate dev-dependencies. |
| accesskit_android 0.7.5 (MIT OR Apache-2.0) | TalkBack bridge (ADR-3454, amends ADR-1998 § 6) | **Runtime, optional, Android only.** `regia_platform` feature `accesskit` (already off by default; `regia` and `regia_dylib` already forward it). `default-features = false`, feature `embedded-dex` (the Java class `dev.accesskit.android.Delegate` loads from a dex file inside the crate, so the activity shim is unchanged). Depends on `accesskit` ^0.24.1 (the locked 0.24.1) and `accesskit_consumer` ^0.38.0 (already in the lock). Also pulls `jni` 0.21.1 beside the workspace `jni` 0.22; call sites use `accesskit_android::jni` and do not mix the two. 0.8.0 and 0.9.0 need `accesskit` 0.25 and were not taken. `multiple-versions` stays `allow`; the new duplicate is listed in `deny.toml`. |
| openusd =0.8.0 (MIT; one generated file, `src/usd/core_schemas.rs`, under the Tomorrow Open Source Technology License 1.0, an Apache-2.0 derivative, notice in the crate's `third-party/`) with the new crates `logos`, `logos-codegen`, `logos-derive` 0.16.1 (MIT OR Apache-2.0), `zip` 8.6.0 (MIT; `deflate` feature), `zopfli` 0.8.4 (Apache-2.0, zip's deflate encoder), `typed-path` 0.12.4 (MIT OR Apache-2.0), `strum` and `strum_macros` 0.28.0 (MIT, a second version beside 0.27, `deny.toml` skip entry); `derive_more` 2, `half` 2, `regex-lite`, `lz4_flex`, `bytemuck`, `bitflags` 2 and `windows-sys` 0.61 were already locked | import-usd (T2.14, ADR-3235) | **Pipeline only.** Dependency of `regia_asset_pipeline` (cook, CLI, editor) for USD import: pure-Rust reader of `.usda`, `.usdc` and `.usdz` with stage composition. Never a runtime crate's dependency, never on Android. `default-features = false` (no `mmap`, no `serde`). The importer reads every file itself (caps, project confinement, cycle and depth checks) and composes through a resolver that serves only those bytes; `.usdz` entries must be stored uncompressed (the reader refuses compressed entries), so the `zip` deflate code is linked but not used on package entries. Fuzzed by `import.usd` and `import.usdz`. |
| objc2-metal 0.3.2 (Zlib OR Apache-2.0 OR MIT), `default-features = false`, features `std`, `MTLDevice` | mac-pass-v048 (ADR-5860) | **Runtime, macOS only.** A `[target.'cfg(target_os = "macos")'.dependencies]` entry of `regia_render`: `platform/macos.rs` reads `MTLDevice.currentAllocatedSize` through `wgpu::Device::as_hal::<Metal>` for `MemoryReport::device_allocated_bytes`, so a GPU memory cap is measured on Metal too (Metal has no allocator report). Already locked through wgpu-hal 30's Metal backend, same version, and both features are a subset of the ones wgpu-hal enables, so no new crate or build. Coupled to wgpu: bump with the GPU/UI group (§ 2). |

## 8. Licence of Regia Engine itself
Regia Engine is under the Mozilla Public License 2.0 (`LICENSE`, `docs/07-licensing.md`, ADR-10225). The
workspace declares `license = "MPL-2.0"` and every engine crate inherits it (`license.workspace = true`); sample
and template crates declare `license = "MIT-0"`; all keep `publish = false`. `cargo deny check licenses` checks
the workspace's own crates too (`[licenses.private] ignore = false`).

Dependency licence policy: a dependency must be under a licence on the allow-list in § 4 (permissive licences,
MPL-2.0 and the listed font/data licences). Those are all compatible with shipping Regia under the MPL and with a
game shipping closed: **no GPL, LGPL or AGPL** code is linked (an LGPL-only crate, for example, would force a game
to allow relinking), and no licence that adds conditions on the game's own files. A new licence needs an ADR and a
`deny.toml` change. Third-party notices ship with every build; `regia package` also writes the engine's own MPL
notice with the source location (`REGIA-ENGINE-NOTICE.txt`). Every first-party `.rs`/`.wgsl` file starts with its
SPDX line (`cargo xtask spdx --check`); ported files carry their upstream licence's id.

Integration kbatch2 (2026-11-09): rmcp and rmcp-macros select 3.9.0 to resolve
RUSTSEC-2026-0457, RUSTSEC-2026-0461 and RUSTSEC-2026-0462 reported by `cargo deny check`.
The SDK adds transitive `sha2 0.11.1` (MIT OR Apache-2.0), used only by the CLI SDK;
the pre-existing sha2 version used elsewhere is unchanged. No runtime dependency is added.

### Server-bot dependency direction (ADR-3346)

`regia_ai/net` adds only the existing workspace `regia_net` crate and enables `regia_framework/net`; it adds
no external package or licence. The optional dependency keeps behaviour-tree bot scheduling in `regia_ai`,
which already depends on the framework, and avoids a framework → AI dependency cycle. `regia/server`
forwards the feature.

**v0.2.23 integration (ADR-4949).** Restoring spawn-order extraction in `regia_2d`
restores its direct `regia_ecs` workspace dependency, removed by the 2026-11-05
unused-dependency audit. No external package or license is added.


### XeSS Super Resolution (ADR-4656, optional Windows runtime)

`regia_xess` sits at platform layer 0b. `regia_render/xess`, `regia/xess` and
`regia_dylib/xess` enable its `sdk` build feature. It reuses workspace `wgpu` 30,
`ash` 0.38 (MIT OR Apache-2.0), `libloading` 0.9 (ISC), `bindgen` 0.73
(BSD-3-Clause; build time only), `thiserror` and `tracing` (MIT OR Apache-2.0 / MIT).
The direct runtime `libloading` exception is limited to this vendor DLL boundary.

Intel XeSS SDK bundle **3.0.2**, [official release](https://github.com/intel/xess/releases/tag/v3.0.2),
is external, owner-approved 2026-10-12 and download-approved 2026-11-08.
`XESS_SDK` names its absolute root; `VULKAN_SDK` names a Vulkan headers installation.
Bindgen writes declarations only to Cargo OUT_DIR. No SDK files or generated bindings are
committed. Without the SDK, Linux, Windows and Android build the `not_linked` stub.
The runtime uses Intel Simplified Software License (Version October 2022); its
SR DLL version is distinct from the bundle version (bundle 3.0.2 carries 2.0.2.68).
A XeSS-enabled distribution must include the unmodified `bin/libxess.dll`, the SDK's
`LICENSE.txt` and `third-party-programs.txt`, preserving all required notices.
Frame generation and low-latency DLLs are not used by this SR adapter. Place the runtime
beside the executable or in an absolute directory named by `REGIA_XESS_PATH`.

Pristine SDK archive SHA-256: `88b8a373f30e33f3558a77a93e634f11b8132fc3047ea1a8edeead32b8471990`.

### Notices scope: every optional feature (ADR-5556)

`cargo xtask notices` resolves each shipped crate with `--all-features`, the same graph `deny.toml` checks
(`all-features = true`). `regia package` copies `THIRD_PARTY_NOTICES.md` into every game's package (ADR-1379), so a
crate that only an off-by-default feature links (`regia/accesskit`, `regia/net-quic`, `regia/physics2d`,
`regia/static-dxc`, `regia_steam/steamworks`, `regia_pt/denoise`, ...) is listed as well. A lane that adds an optional
dependency therefore regenerates the notices too. The demo's own list (`cargo xtask demo-notices`, ADR-4847) stays
exactly the demo binary's normal tree. Vendor runtimes that a crate bundles rather than builds (Valve's `steam_api`
inside `steamworks-sys`) are an open owner/counsel item in ADR-5556.

### `regia_gen`: procedural mesh helpers (ADR-7240)

`crates/regia_gen` is a workspace crate at layer 2 (systems; `xtask/src/layers.rs`): extrude with bevel,
lathe, boolean-lite plane clipping and convex intersection, planar and box UV projection, and recipe
building over `regia_asset::CpuMesh` (spec 23 § 6). It depends on `regia_core` (the deterministic
`math::det::sin_cos`), `regia_asset`, and the existing workspace crates `serde`, `thiserror`, `blake3`
and `tracing`; `serde_json` is a dev-dependency. No new external dependency, so nothing changes in
`deny.toml` or the notices. `regia_agent` depends on it for `gen.mesh`. It is not in the API-snapshot
`STABLE_CRATES` list yet (new, unstable surface). ADR-7280 adds the texture and heightfield half (noise,
masks, mesh edge/cavity and wear masks, `Heightfield` tools, `gen.texture` and `gen.heightfield`) with no
new dependency; it also uses `regia_core::math::det::{cos, det_powf}`.

### `regia_detsim` and `regia_world_geom`: crates DO27's sim links (ADR-8012, planned)

Two new workspace crates, created by their build lanes (F-01 and W-01 of the DO27-absorb plan), recorded here ahead
of them (ADR-8012 A-5):

| Crate | Layer | Depends on | New external dependency | Notes |
|---|---|---|---|---|
| `regia_detsim` | 0a (Foundation, beside `regia_core`; no ECS, GPU or platform crate) | no Regia crate; `libm` only (`serde`/`tracing` when a module needs them) | `libm = "=0.2.16"` direct, exact pin (MIT OR Apache-2.0; already in the tree via glam and i_float, so the lockfile does not change) | `math` (transcendentals with DO27's exact bits), tick constants, the desync hash, deterministic ids and broadphase, the hover step; the `cargo xtask det-scan` lockstep-closure list starts with it. Semver API, DO27 signs off on changes |
| `regia_world_geom` | 0a (Foundation; no ECS, GPU or platform crate) | `regia_detsim`, `serde`, `tracing` | none | GPU-free, ECS-free world-geometry query and cooked storage in i32 mm (shape: lane D-03's spec). Semver API, DO27 signs off on changes |
| `regia_rules` | 0a (Foundation; no ECS, GPU or platform crate) | `regia_detsim`, `serde`, `thiserror`, `tracing`; dev-only `serde_json` for the existing round-trip test | none; existing serde/thiserror/serde_json are MIT OR Apache-2.0 and tracing is MIT; internal crate uses the workspace license | S-14 match modes, objectives, CTF, KOTH, match flow, playlists and sessions, moved without computation changes (ADR-8239); re-exported by `regia_framework::rules` |
| `regia_nav_det` | 0a (Foundation; no ECS, GPU or platform crate) | `regia_detsim`, `regia_world_geom`, `serde`, `thiserror`, `tracing`; dev-only `ron` and `regia_world_geom` `fixtures` | none; all existing workspace entries (serde, thiserror, ron MIT OR Apache-2.0, tracing MIT); internal crate uses the workspace license | The deterministic navigation (grid, search, flow fields, navmesh, layered tunnels, agents, crowds), moved out of `regia_nav::det` without computation changes so a headless lockstep sim links it without bevy_ecs (ADR-8877); re-exported by `regia_nav` as `det`; a `[[crate]]` row of the lockstep closure |

The `libm` pin is exact (`=`), not the usual caret: a patch release of `libm` could change the bits of a
transcendental and desync a lockstep match across builds. Bumping it is a sim-version change (ADR-8012 A-4, Stage B
rules). Neither crate may gain a dependency on `bevy_ecs`, `wgpu` or `regia_platform`; the layer check
(`xtask/src/layers.rs`) gets their rows when they are created.

`regia_detsim` exists since lane F-01 (ADR-8013): `math`, `tick` and the FTZ/DAZ guard `fpu`; its only dependency is `libm`
(the workspace entry is now `=0.2.16`). `serde` and `tracing` join when a module needs them. The lockstep-closure list is
`docs/ci/lockstep-closure.toml`. Lane S-07 (ADR-8079) adds the hover step, `ballistic`, `site_physics` and `lowg`, and a
`serde` feature (off by default, no dependency of its own: `serde = []`) that switches on serde derives for the hover
parameters and state; `serde` itself is a required dependency of the crate (the desync hash, ADR-8047).
ADR-8085 adds `float` (`det_min`, `det_max`, `det_clamp`, `canonical`: portable zero and NaN sign), no new dependency.
Since lane F-04 (ADR-8047) it also has `hash`, the per-system desync hash, and depends on `serde` (`serde_json` for tests
only).

Lane S-09 (ADR-8152) adds `regia_detsim::ai`, the deterministic AI core (DO27's `AiRng`, schedule and utility scorer), and
with it the `serde` and `tracing` dependencies (workspace entries, no new crate in the tree). `regia_ai` depends on
`regia_detsim` to re-export the core as `regia_ai::det`.
Gap AI-7 (ADR-8609) adds `ron` (workspace entry, MIT OR Apache-2.0, already in the tree through every RON format
crate) to `regia_detsim` for `UtilityCatalogue::from_ron_str`, the `.detutility.ron` parser a headless sim calls
without an asset system.

`regia_world_geom` exists since lane W-01 (ADR-8050): Stage A `compat` (DO27's tunnel layout, meshers, floor query,
heightfield sampler and loaders, copied as-is). Its dependencies are `serde`, `serde_json` (the baked `do27-tunnels/1`
file) and `tracing`, all workspace entries already; `regia_detsim` joined with ADR-8166 (libm trig and `det_*`
min/max/clamp in the copied code). Its `fixtures` feature (off by
default) compiles DO27's map layouts as test data. It is on the lockstep closure and in the layer table (0a).
Lane W-06 (ADR-8112) adds the meshlayers formats and the cook's canonical half: `regia_detsim` (`sqrt_f64` in the
cook's strip measure), `ron` (the source documents), `postcard` (the cooked file), `blake3` (`content_hash`) and
`thiserror`, all workspace entries already; no new external crate.
Since ADR-8167 it has one dev-dependency, the workspace's pinned `libm` (=0.2.16, MIT OR Apache-2.0, already a
`regia_detsim` dependency), for the test that compares `regia_detsim::math` with the libm functions DO27's math calls.
Lane W-14 (ADR-8291) reads that cooked geometry from `regia_terrain`: `regia_world_geom` is a runtime dependency
there (the `fixtures` feature stays on the dev-dependency). No new external crate, and no second triangle store.
Lane W-25 (ADR-8416): `regia_audio` depends on `regia_world_geom` with `stage_b` (layer 0a under layer 2) for
sound over cooked portals (`GeomSoundField`); `regia_agent` gains it as a dev-dependency with `fixtures`. No new
external crate.
Lane F-03 (ADR-8081) adds the workspace `glam` to `regia_detsim` as a **dev-dependency only**: the
planted SIMD scenario of the det-golden runners lives in the crate's tests; `src/` still has no glam (det-scan).

S-18 (ADR-8151): `regia_detsim` now uses the already-approved workspace `tracing`
(MIT) for harness steps, snapshots, arena steps and soak/profile spans. No new
external package or version is introduced; the A-5 dependency boundary is unchanged.

Lane S-16 (ADR-8154) adds `regia_detsim::fog` (team visibility) and `regia_detsim::los` (the line-of-sight hook), with no new
dependency. `regia_framework` depends on `regia_detsim` for the per-viewer sim-bridge filter `regia_framework::fog_view`.

ADR-8555 (lane buildout-det-orca-lp-obstacles) makes `serde` (workspace entry, MIT OR Apache-2.0, already in the
tree) a normal dependency of `regia_detsim` for the avoidance settings data (`OrcaSettings`, `OrcaSolver`,
`OrcaSegment`), and `serde_json` (workspace entry, MIT OR Apache-2.0) a dev-dependency for their round-trip test. No
new crate enters the lockfile.

C-09 / ADR-8062: `regia_cli` reuses the workspace's existing `regex` dependency
(MIT OR Apache-2.0) for configurable package hygiene expressions. No new resolved
third-party package or version is introduced; expressions use bounded compilation.

### Terrain source readers (ADR-8063, C-20)

`regia_terrain` now uses the existing workspace `serde_json` (MIT OR Apache-2.0)
and `png` (MIT OR Apache-2.0) packages at runtime for count-based authoring inputs
and CPU overview encoding. `png` was already its dev dependency. No package,
version or new license is introduced; cooked asset formats are unchanged.
The workspace's existing `serde_json/float_roundtrip` feature is required here:
`schema2_decimal_tokens_round_directly_to_f32` pins a decimal just above a binary32
midpoint, including optional origin fields, to reject accidental f64-first parsing.
ADR-8146: `regia_net` depends on `regia_detsim` (the float-environment guard `regia_net::lockstep` calls before it hands
out a frame) and the dev-only `regia_net_soak` does too (its lockstep sample simulation steps through `math` and `float`).
Both are workspace-internal edges: no new third-party crate, and the lockfile gains only those two entries.

`regia_cli` names the existing workspace `flate2` directly (ADR-8035): `regia logs triage` inflates
deflate ZIP members of tester uploads in memory. The crate was already in the CLI's graph through
`regia_viewer_pack`'s `cook` feature, so the lockfile, `deny.toml` and the notices do not change.

C-14 / ADR-8429: `regia logs perf-gate` uses the existing workspace `toml` and `serde_json`
packages already depended on by `regia_agent`. No new crate, version or license.
ADR-8483 adds the workspace `serde` (MIT OR Apache-2.0, already in the tree) as a dependency for `rng::DetRng` and
`fixed::{Mm, Mm2}`, and `serde_json` (MIT OR Apache-2.0) as a dev-dependency for their round-trip tests; the det-golden
bundle (`xtask/src/det_golden.rs` `bundle_manifest`) lists both.

CT-16 / ADR-8348: `regia_cli` gains a direct edge to the workspace's existing `flate2` 1
(MIT OR Apache-2.0, default `miniz_oxide` backend) to decode ZIP deflate entries and gzip
streams during final-artifact audits. It was already in the `regia_cli` build graph through
`regia_asset_pipeline`; no new resolved package or version is introduced.

`regia_framework` depends on the workspace crate `regia_detsim` (ADR-8550) for `det_hover`, the hover hulls in an ECS app;
no external crate enters the graph (`libm` and `serde` are already in it). `regia_detsim` itself stays ECS-free.

ADR-8551 adds `ground_vehicle` to `regia_detsim` (kinematic tracked and wheeled models), no new external dependency, and
gives `regia_physics` a path dependency on `regia_detsim` (a Foundation crate, allowed by the layer check):
`regia_physics::vehicle_track` steps through `regia_detsim::ground_vehicle::tracked`.
