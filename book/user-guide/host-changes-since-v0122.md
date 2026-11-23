# What changed for hosts since v0.1.22

For Red-Rouge and Direct Order 27. A host is the program that links the engine: `shell-ferrite`, a game's `main`, a
build or capture script. This page lists, release by release, what a host has to edit, what it can ignore, and how to
check it. The full text of each release is in `docs/releases/`; the exact code edits for DO27 are in the
[DO27 host checklist](do27-host-checklist.md).

Take v0.1.27 in one step and build against it before merging: it changes several public types (below). A host that
only runs the engine through `regia::run` and `DefaultPlugins` has the shortest list.

## Edits that break the build or the content

| Since | Edit | Who |
|---|---|---|
| v0.1.27 | `CookedTexture.ktx2` is `PakPayload`, `CompressedTexture.levels` is `Vec<PakPayload>`: build with `bytes.into()`, clear with `PakPayload::default()`. `TileSource::load` and `TileCache::residency` return `PakPayload`. | code that edits texture levels or tile bytes |
| v0.1.27 | `AssetLoadError` has a variant `GpuMesh`; `UiEventKind` has six new variants. Add a wildcard arm. | exhaustive `match` |
| v0.1.27 | `UiEvent { target, kind }` becomes `UiEvent::new(target, kind)`. `UiPointer`, `UiInputState`, `UiThemes`, `Themed` gained fields: use `..Default::default()`. | `regia_ui` users |
| v0.1.27 | `MorphTrack`, `CompressedMorphTrack`, `GltfAnimationImport`, `FinishedEval` gained fields: use `MorphTrack::new` / `for_node`. | morph clip code |
| v0.1.27 | `LevelLook` (`baked_gi: None`), `DetailBrick` (`flags: 0`), `GiBakeDescription`, `LayoutDesc`, `GiBakeJob`, `BakedGiStatus`, `BakeGiArgs` gained fields. `CookError::BakedGi(String)` is new. | level and bake code |
| v0.1.27 | `SCENE_FORMAT_VERSION` is 10, cooked clips are v2, `.bake.ron` is v1, `.fgiv` is v2. Run `regia upgrade-project --write`. | content |
| v0.1.25 | `.theme.ron` v1; old `UiTheme` data migrates with `regia upgrade-project --write`. | content |
| v0.1.24 | `.cue.ron` is v2; v1 files still play, `regia upgrade-project --write` rewrites them. | audio content |

Code that decodes pak mesh entries with `CookedMesh::from_postcard` must use `regia_asset::load_mesh_bytes` or
`AssetServer::load_mesh` before it opens a pak built with `REGIA_PAK_GPU_MESHES=1`. Never overwrite or truncate a
mounted pak in place; replace it by rename.

## Behavior that changes without a compile error

- **Look of the runtime UI (v0.1.27).** Classed widgets follow the active theme; the palette is the design palette and a
  disabled widget draws at 0.45 opacity. UI goldens that show classed widgets change. Set
  `UiThemes::disabled_opacity = 1.0` or select your own theme to keep the old look. An open `Modal` now blocks pointer
  input behind it.
- **Agent output folders (v0.1.27).** `render.out_start` writes only under the project root, the temp dir, the working
  dir or an `output_roots` entry in `.regia/agent.toml`. A broken `agent.toml` grants `read` only. Errors show no
  absolute paths.
- **Quit (v0.1.27).** `regia::run_with` waits for registered GPU worker threads (up to 30 s, only if a driver call
  hangs) and then ends the process with the app's exit code. A host with its own `main` calls
  `regia_platform::finish_exit(code, flush)` last (v0.1.24 introduced `finish_deliberate_exit`, which still works).
  Threads that hold a GPU device start through `GpuWorkerBuilder`.
- **Shutdown waits (v0.1.23, v0.1.24).** Dropping a renderer while material graphs compile, and the warm-pipeline wait at
  exit, can take up to 30 s on a loaded machine (they were 1 s to 5 s). `REGIA_MATGRAPH_SHUTDOWN_WAIT_MS` and
  `REGIA_WARM_SHUTDOWN_WAIT_MS` set another limit. After a device-lost exit the process ends with code 3 (4 for no
  window surface) without process-exit handlers (v0.1.24).
- **First launch after an update (v0.1.27).** A pak not yet verified is read as copies until the background check passes.
  Peak memory during a map load of texture-heavy maps is about half of before.
- **Locks (v0.1.26).** Game code must not hold a lock across a `regia_core::tasks` fan-out that another pool task may
  take; copy what you need and release first.

## New things a host can use

- Zero-copy pak loads, parallel mesh decode, optional GPU-ready pak meshes (`REGIA_PAK_GPU_MESHES=1`, desktop only) and
  `pak_migrate <old.fpak> <new.fpak>`. Check: `assets.status` shows `paks: { mounted, zero_copy }`.
- Seeded cue playback: `Audio::play_cue_seeded`, `play_cue_event`, `cue_event_seed(event_id, tick)` replay the same
  variation for the same event (v0.1.24).
- UI themes (`.theme.ron`, `ActiveTheme`, `ThemeScope`, `ThemeOverride`), double-click, context click, scroll, long press and
  repeat events, input layers (`push_layer`, `pop_layer`), and the `ui.*` agent methods. Check: `ui.query`, `ui.click`,
  `ui.layers`.
- Per-node morph weights (`NodeMorphPlayer`, `MorphTrack::for_node`). Check: `anim.morph_weights`.
- Procedural loops (`.loopgen.ron`). Check: `audio.loopgen_render`.
- `cargo xtask windows-pass` for a Windows machine (not verified on hardware since 2026-10-14).

## Not ready yet

At v0.1.27 baked light was groundwork only. The v0.5.0 state: `regia bake-gi` runs end to end, from a level's statics
(`--from-scene <level>`) through the bake to a `.fgiv` that `LevelLook::baked_gi` names and `BakedGiLevelPlugin`
installs when the level opens (test `the_fixture_bakes_installs_and_lights_an_open_and_a_sealed_cell` in
`crates/regia_cli/tests/bake_e2e.rs`; [Baking GI from meshes](../agents/baked-gi-bake.md)). A layout over one 128 MiB
binding is written as a split volume of up to four parts ([Split baked GI volumes](../agents/baked-gi-split.md)); a
layout no four-part split fits fails with `bake_volume_too_large`. Check a map's cost first with
[Sizing a GI bake](../agents/baked-gi-sizing.md). No DO27 map has been baked yet: the Bore Twelve numbers in
`docs/research/bake-sizing-bore-twelve.md` come from a synthetic stand-in, not from DO27 content, so measure a real map
before planning one around baked GI.

## Checks after taking v0.1.27

1. `cargo build` the shell; fix the compile errors from the table.
2. `regia upgrade-project` (without `--write`) lists what needs rewriting; then run it with `--write`.
3. Quit 20 times from a loaded map: the exit code is as expected and the log has no `GPU worker(s) still running` line.
4. `assets.status` after one launch: `zero_copy` equals `mounted`.
5. If the shell uses `regia_ui`: `ui.query` lists its widgets, and a modal blocks clicks behind it.
