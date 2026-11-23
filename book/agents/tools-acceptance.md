# Authoring tools acceptance

`regia_editor`'s `headless_editor::tools_acceptance` tests drive the real editor over
`regia_agent::client::Client`. They create isolated projects, check authored values with
`play.wait_until` and `play.expect`, stop the editor with `agent.shutdown`, then open the
same files in a fresh editor process. No golden images or elapsed-time assertions are used.

Run the deterministic flow on a build host:

```sh
cargo test -p regia_editor --test headless_editor tools_ -- --nocapture --test-threads=1
```

The combined flow covers:

1. `terrain.set_level_terrain`, `terrain.stroke`, `terrain.export_heights` and
   `editor.save_level`: compare every height sample before, after, undo, redo and reopen.
   The source heightfield is explicitly exported before clean platform cooks.
2. `editor.create_prefab_variant`, `editor.set_variant_override` and
   `editor.spawn_prefab`: author a child transform override and check the instantiated
   transform after reopening the level. The separate variant regression checks exact
   file bytes across undo/redo.
3. `editor.mode2d_set`, `editor.tile_stroke` and `editor.tilemap_save`: paint an autotile
   strip containing an animated tile, compare the whole map across undo/redo, then read
   the cooked map through `PakReader` and sample its animation at exact millisecond steps.
4. `editor.ui_designer_open`, `editor.ui_designer_edit`, `editor.ui_designer_preview`
   and `editor.ui_designer_save`: bind a score, compare designer history, check English,
   French and pseudo-localization, and reopen the saved document.
5. `editor.seq_curve_open`, `editor.seq_curve_edit`, `editor.seq_section_edit` and
   `editor.seq_curve_save`: edit a camera curve and audio gain, compare keys and sections
   across undo/redo, reopen, then drive `sequence.player` on its external clock. A
   reflected camera transform must reach the expected position.
6. `profiler.set_capture` and `profiler.capture`: require a v1 capture with nonempty CPU
   call-tree nodes and folded stacks. GPU timing is not required for this CPU assertion.

Set `REGIA_TOOLS_EVIDENCE_DIR` to an empty directory to retain the authored project,
its runtime-only Cargo consumer and profiler capture. Set `REGIA_TOOLS_GPU=1` on an
approved rendering host to exercise `editor.capture_viewport` and retain its JSON
response, including `png_base64`. A no-GPU run provides no screenshot evidence.
These variables do not weaken any persistence or state assertion.

The retained project can be packaged with the ordinary CLI:

```sh
regia package --platform linux --format tar --project /path/to/evidence/tools-project
```

Run the executable in the resulting package. Its runtime loaders check prefab expansion,
terrain, UI/localization files, animated tilemap frames, sequence audio samples and the
camera transform. The raw scene, prefab, UI and sequence formats ship under `assets`; localization
ships as hash-checked `assets/loc/index.ron` bundles. The terrain and tilemap also
have cooked pak payloads. The package verifier requires the bundle index and never
falls back to authoring FTL files. An in-memory cook is not a substitute for this package run.
The fixture does not claim audible device playback or a windowed editor usability pass.

Android validation targets these runtime consumers, never `regia_editor`. The terrain
regression cooks Linux, Windows and Android data and loads each with the runtime loader;
this is host-side format evidence, separate from a device run. Record native Windows,
Android compilation and device results independently. Existing method errors and versions
are documented in the linked [editor](editor.md), [terrain](terrain-editing.md),
[designer](ui-designer.md), [sequence](sequence-player.md) and [profiler](profiler.md) pages.

The Android named-level terrain case is deliberately separate and unignored:
`tools_android_authored_level_terrain_runtime_data`. It exposed the mismatch between the
Android cook's tiled-terrain omission and the named-level runtime loader's required
heightfield payload; since ADR-4914 the Android cook writes the tiled heightfield (since
ADR-5294 with ASTC 4×4 texture tiles beside it), and the case passes. A successful desktop subset does
not close that acceptance requirement: run and record this case independently.

On a device, the same retained project packages as an ordinary APK. Give the exported Cargo
project an Android `cdylib` (`[lib] name = "main"`) whose `android_main` reads the packaged files
through `regia_platform::ApkAssetMount`, lays them out like the desktop package
(`assets/<path>`, `content/base.fpak`) and calls the unchanged consumer's `verify`; set
`[platforms.android] cargo_package` in `Regia.toml`, then:

```sh
regia package --platform android --format apk --abi arm64-v8a --project /path/to/evidence/tools-project
```

Install the APK, start its activity and read the consumer's result from the app's files
directory with `run-as`; uninstall it afterwards. The same consumer built as an `aarch64`
executable can also run over the APK's extracted `assets/` folder in `/data/local/tmp`
(`content` linked to `assets/content`). Both read the Android cook: the tiled terrain
heightfield, the cooked tilemap and the normalized locale bundles.

The combined test also drives `editor.play`, `editor.pause`, `editor.step` and
`editor.stop`. It polls `editor.play_state` across protocol frame boundaries, requires
exactly two paused simulation steps, and verifies that stopping leaves authored edit
transforms intact. Saved format headers are read directly, without a migrator hiding
an older writer. The variant companion case includes a dependent variant and a no-op
edit that must not add a history entry.

For cross-built test executables staged on a native host, set
`REGIA_EDITOR_TEST_BINARY` to that candidate's `regia-editor` executable. The normal
Cargo test run defaults to Cargo's executable path. This override avoids embedding a
Linux build-host path in a test launched on Windows; it does not select another editor
automatically. Use matching commit hashes for the test runner and editor.

## UI designer acceptance

`tools_designer_hierarchy_properties_bindings_multisize_locale` builds a menu from an empty
document (`editor.ui_designer_open { new: true }`) over the real editor socket. Every edit is one
undo step: the test keeps the hierarchy after each one, then walks the whole history back to the
empty document and forward again, comparing each hierarchy, and ends on
`ui_designer_nothing_to_undo` / `ui_designer_nothing_to_redo`. It covers:

- the hierarchy: `add` of panel, text, progress and button presets, `move` (a bar moved up the
  column), `duplicate` (the lint reports `duplicate_name`) and `remove`;
- properties, checked through the layout they produce: `style.direction`, `style.size` in points
  and percent, localized `text` spans and a `command`;
- bindings: a `{:03}` format and a `number` converter with grouping, read through `samples`; a
  `value` target on a progress bar is a `binding_target_mismatch` lint error;
- refused edits (`ui_designer_bad_move`, `ui_designer_bad_node`, `ui_designer_bad_size`) change
  nothing and add no history;
- the preview at five sizes (1920x1080, 1280x720, the 2400x1080 and 1080x2400 phone presets with
  their insets, and a custom 800x600) in English, French and pseudo-localized English: full-width
  rows, point sizes times `scale`, column order, a row of auto-width buttons that widen with the
  French and pseudo labels, and the safe-area flag only where an inset is crossed.

The saved file is reopened by a fresh editor process (same hierarchy, same previews, clean lint).
Then the test lays the saved file out with the runtime `spawn_ui` in its own headless app, without
any designer code, at every size: each of the eight named nodes has the rect the designer
previewed. No GPU is needed; `editor.ui_designer_render` stays covered by the `REGIA_TOOLS_GPU` run
of the combined flow.

## Render-out of an authored sequence

A headless editor started with `--no-gpu` opens no GPU device: `render.out_start` and
`render.out_job` answer `editor_headless` there, start no job and write nothing (ADR-5180). The
test renders the authored sequence on a runtime host, as `regia render --job` does on a farm.
`tools_render_out_of_an_editor_authored_sequence` does both halves; the editor half needs no GPU, so
`tools_no_gpu_editor_refuses_render_out_and_saves_its_edits` also runs it alone on every host:

1. The headless editor (`--no-gpu`) refuses both render-out calls, then moves the camera curve's second key with
   `editor.seq_curve_edit`, sets the audio section to -6 dB with `editor.seq_section_edit` and
   saves with `editor.seq_curve_save`.
2. A headless runtime app (`RenderPlugin { headless: true }`, the headless mixer, the agent)
   runs `render.out_job` on the saved `.seq.ron` with a v2 binding table
   (`"camera": Name("CameraTrack")`), two takes (`Whole` and `Seconds(1.0, 2.0)`) and a PCM16
   mixdown. It renders the unedited file once and the edited file twice.

It checks the dry-run plan against the run (20 frames at 10 fps, one job-owned sequence player, no
unbound targets), that the tail take's frame hashes are the full take's last ten, that frame 0
matches the unedited sequence while the last frame does not, that the mixdown is silent outside the
section and 6 dB louder inside it than the unedited one (within 2 %), and that the edited job repeats
its frames and WAV bytes exactly. The runtime half needs a Vulkan device (any Vulkan adapter, lavapipe in
CI): the test returns early when `/usr/share/vulkan/icd.d` has no lavapipe manifest, and otherwise
renders on the adapter the renderer picks, so select lavapipe to match CI:

```sh
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json VK_LOADER_DRIVERS_SELECT='*lvp*' \
  cargo test -p regia_editor --test headless_editor -- --nocapture --test-threads=1 \
  tools_render_out_of_an_editor_authored_sequence tools_no_gpu_editor_refuses_render_out_and_saves_its_edits \
  tools_designer_hierarchy
```

A run that did the work prints `render_out_acceptance: frames=20 ...`.
