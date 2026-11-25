# Known issues and limits

<!-- index: Known issues -->

Limits that are true of this version. The [catalog](catalog.md) repeats the status. This page is
the sentence an agent should trust before describing a feature as finished. It is not a second
copy of `docs/versions.md`.

The play lane read spec 15 (physics) against the physics guide and recorded it built. The create
lane read spec 40 (content chunks) and recorded it built, with the limits below. The UI default
design stays partial: themes and the focus ring are in, and the open list from that spec's § 10
is below. Specs 21 (tooling), 41 (functional tests), 57 (the plugin API), and 67 (the procedural
graph editor) keep the inventory status. The editor lane and the ship lane had not reported
when this page was written. Do not treat this paragraph as a reason to mark those four built.

## Platform matrix

<!-- index: Platform -->

One table for the platform pages, so they do not drift. "Yes" means the introduction's platform
table says the runtime or the editor is supported. The limit column is the part that is not a
yes.

| Platform | Runtime | Editor | Graphics | Limit |
|---|---|---|---|---|
| Linux x86_64, including Steam Deck | yes | yes | Vulkan | The Deck uses the `Deck` quality tier. |
| Windows 10/11 x86_64 | yes | yes | DX12 or Vulkan | |
| Android arm64-v8a | yes | no | Vulkan | Surface drop on suspend is specified. `docs/versions.md` (2026-11-08) still listed failed lifecycle acceptance. Not re-run for this manual. |
| macOS | builds and tests | not verified | Metal | Testing only (ADR-009, ADR-264). Not a dedicated-server package target. |
| Product viewer (`regia_web`) | browser | no | WebGPU, WebGL2 fallback | Not a general web game. |
| Parked targets | no | no | | Consoles, a general web game, and an iOS release. See the ladder below. |

Pages: [Linux packages in the install chapter](../start/install.md), [Windows](../user-guide/windows.md),
[Android](../user-guide/android.md), [macOS](../user-guide/macos.md),
[Web player](../agents/web-player.md).

## OpenXR

<!-- index: OpenXR -->
<!-- index: xr_no_loader -->

Engine-side XR is in `regia_xr`: config, input, comfort, the simulator, and stereo render.
The OpenXR loader, instance and swapchains are not in the crate. `XrBackendKind::OpenXr` (and
`Auto`, when it would open a runtime) starts in the `Off` state with `last_error.code`
`xr_no_loader`. The desktop game runs. [XR (agents)](../agents/xr.md).

## Vendor upscalers

<!-- index: DLSS -->
<!-- index: XeSS -->

DLSS is an optional `sdk` feature plus an external `DLSS_SDK`. The SDK is not in this
repository. Android and macOS never link it. When it is not linked, the engine falls back.

XeSS is Windows Vulkan plus an external `XESS_SDK`. It is not bundled. Otherwise it is not
linked.

The in-tree upscaler path does not require either SDK. [Rendering](../user-guide/rendering.md).
DX12 reports `dlss_needs_vulkan` for DLSS. A build without the SDK uses TAAU, TSR, or FSR 3.

Frame generation needs the `frame-gen` cargo feature. Without it the blocker is `not_compiled`.
It stays off until `FrameGenSettings.enabled` is set. Below 55 rendered frames per second the
pacer presents real frames only.

`RenderProfile::subsurface_scattering` is off in every preset. A version 6 `subsurface` material
draws without the blur until a game turns the flag on. `ShadingModel::Skin` is a different path
and runs on every tier.

The generated render-profile table lists the six real-time presets. VeryLow is a quality tier
(Desktop Low at one third of the output in each axis, then SGSR 1), not a preset column.
Cinematic is an offline profile, not in that six. Android, and a detected tier below High, do
not run Cinematic unless the documented gate says so. Turning vsync off asks for `Immediate`.
On Android the request is refused and frames stay on the display's vsync grid
(`android_display_paced`).

Derived LOW and ULTRA_LOW lightmaps still miss their original seam and dark-point checks. The
Mobile Low reference image is waiting on an owner re-bless.

## macOS

<!-- index: macOS -->

Testing only, on Metal (ADR-009, ADR-264). Not a package target for the dedicated server. The
editor is not verified on macOS. Cursor grab follows the platform limit below.
[macOS](../user-guide/macos.md).

## Input and windows

Locked cursor grab falls back to confined on Windows and X11. Confined falls back to locked on
macOS. [Input](../user-guide/input.md).

Spec 11: only the primary window becomes an OS window today. Additional views are not extra
operating-system windows. [The editor](../user-guide/editor.md).

## Networking that is only partly proven

<!-- index: Steam sockets -->
<!-- index: QUIC -->

Steam sockets and Steam lobbies: the code is in the tree. The 2026-11-05 versions note recorded
mock tests. A two-account Steam session is run by the owner. This manual does not say a real
match has been proven. [Steam](../user-guide/steam.md),
[Networking](../user-guide/networking.md).

`regia steam` uploads depots with `steamcmd`. Credentials come from the environment
(`STEAM_USERNAME`, `STEAM_PASSWORD`, `STEAM_TOTP`). The owner creates the Steam app.

The self-hosted lobby hub (`regia-lobby-hub`, ADR-8559) is a different piece and the inventory
records it as built. The in-process lobby directory is built too. Steam lobbies stay partial.
[Self-hosting a lobby hub](../user-guide/networking.md#self-hosting-a-lobby-hub).

`regia steam upload` has no `--retries` field. `regia release upload --retries` is a different
command. The inventory row `regia steam upload --retries` is a note for a later uploader, not a
flag of the current command.

QUIC: `regia_net::quic` exists. A shipped session uses raw UDP. QUIC is not a supported session
transport in this version.

Voice: strong noise suppression runs as light. Chat HRTF and a voice editor panel are not in
this version. Opus needs the `voice-opus` feature.
[Voice chat](../agents/voice-chat.md).

Android has no lobby platform backend (`none`). Dedicated-server packages are Linux and Windows.
Android and macOS exit 2. There is no standalone relay binary in the server package. `--relay`
runs inside the server, or the game runs `RelayServer` itself. `regia package --platform linux`
does not pack the three-crate `net-game` template. `regia package --server` packs a dedicated
server. The networked-game tutorial states the client-layout gap.

`session.connect` with a join code returns `join_code_unsupported` (`F0002`). The net-game
client `--join` resolves the code. An agent connects by address after that. A password on a
non-loopback bind with encryption off returns `password_needs_encryption`. Steam lobby calls
return `lobby_unavailable` when the feature is off, the client is not running, or the platform
has no lobby backend.

## Other partial features

Voxel terrain is partial. Dual contouring and volume-mesh layers work when the host supplies a
field. `regia_voxel_core`, the operation log, and `.voxfield` are not in this version (spec 122).
[Volume meshes](../user-guide/volume-mesh.md).

Outline-shaped lakes are not built. Swell does not refract over the bed and does not run up the
beach. Floaters do not leave wakes. Water GPU time is a second draw into a private target.
Water extras (underwater view, ring culling, the sea-state table, water GPU time) stay off until
the project turns them on.
[Author a terrain](../how-to/world/terrain.md).

Drawn rain, surface weather, and weather-driven scene wind are off unless
`assets/config/weather.ron` enables them. `net_game` and `quick` do not ship that file.
Lightning, lens drops, wall streaks, hail, snow deformation, and regional weather volumes are
outside spec 99. The path tracer and baked GI stay dry.
[Sky](../user-guide/sky-lab.md).

`docs/formats/terrain.md` opens by saying the layers format is at v4. The example in that file,
and the photoreal chapter, are version 5.

Video playback (spec 50): codecs are Cargo features and off by default.
[Video playback](../agents/video-playback.md).

DCC scene feed (spec 60): Cargo feature `dcc-feed`, off by default, for development builds.
[DCC scene feed](../user-guide/editor-guide/dcc-feed.md).

Destruction level 2 (pre-fractured chunks and bonds that break from hits) is built.
[Fracture](../user-guide/editor-guide/fracture.md). Level 3 (runtime fracture, carving, and
deformation, with collider, navmesh, and network sync) is parked on the v1.2 rung. A wall does
not fracture from a shot until a cooked `.destructible.ron` is present.

Cursor grab: Locked falls back to Confined on Windows and X11. Confined falls back to Locked on
macOS. Wayland implements both. A rejected grab is retried next frame.
[Input](../user-guide/input.md).

UI default design, still open (spec § 10): no JetBrains Mono TTF in `regia_ui/assets/fonts`; no
shipped icon glyphs; no Checkbox widget; Radio, SpinBox, Foldable, Split, Tabs, Toast,
Progress, and Scroll have no theme type of their own; no toast role bars or hover pause; no
compact tree indent; no split 5 percent keyboard step; no `focus_mode`; no default edge rules
(a `FocusGroup` works when set); no gamepad-cursor snapping; elevation tokens are not drawn as
shadows; no JSON Schema for `.theme.ron`; no `Regia.toml` `[ui] theme`. There is no `[ui] theme`
key. Use `ActiveTheme` or `ui.theme_select`. Built: the four themes (`regia-dark`,
`regia-light`, `regia-high-contrast`, `regia-light-high-contrast`), token resolution, the focus
ring in navigation mode, an icon atlas once a sheet is loaded, theme swap, and reduced motion.
ADR-274 remains proposed. [UI](../user-guide/ui.md).

Motion matching has no learned database compression. The ground is the XZ plane. 2D lights are
not built. Sprites other than the map's tiles are not drawn in the editor viewport (Play shows
them). Spec 53 does not replicate or predict ability grants in this version. Lockstep games do
not read rapier poses, contacts, events, vehicles, or character sweeps (ADR-8841). Rapier CCD
can let two fast bodies pass through each other. `SweptCcd` is the opt-in that sweeps both
sides. Aircraft, as documented on the vehicles page: no downwash, ground effect, propeller
wash, compressibility, or stall hysteresis.

`regia export usda` writes triangles, points, normals, and the first UV set from a `.glb`. It
does not write materials, textures, skins, animation, cameras, lights, or morph targets.
`usda_empty` leaves the output path untouched. The file is not upgraded by
`regia upgrade-project`.

`https://` chunk fetch needs Cargo feature `content-http`, off by default. A `.level.d/`
directory is authoring-only. A package does not ship it. A level may be a file or a directory,
not both (`ambiguous_level_form`). There is no single call that spawns a glTF as entities.
`load_cooked_model` loads LOD 0 draws. The game spawns the entities.
[Content chunks](../user-guide/content-chunks.md).

## Parked ladder

<!-- index: Version ladder -->

These are not part of this version. The catalog lists them as `deferred`.

- Consoles.
- A general web game target. The product viewer (WebGPU and WebGL2) is not that target.
- Digital humans.
- Visual-graph code generation.
- Google Play services.
- A WASM mod SDK. Sandboxed Lua mods are a separate, opt-in feature; they are not this row.
- An iOS release.
- Destruction and fracture level 3.

## Sample game and starter content

<!-- index: Sample game -->

ADR-8588 dropped the sample game. Templates (`blank`, `first_person`, `third_person`,
`top_down`, `quick`, `net-game`) stay. Starter content was still thin at the 2026-11-01 truth
pass: templates and samples, not a packaged starter set. Spec 61 stays partial.
