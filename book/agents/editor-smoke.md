# Editor UI smoke (`cargo xtask editor-smoke`)

ADR-7865, amended by ADR-7955. Code: `xtask/src/editor_smoke/`.

One command launches the real editor on a throwaway copy of a sample project and drives every panel, menu item,
palette command and registered UI action it has through the agent protocol, then reports what broke. It is a test
tool: it changes no editor behavior, and it writes nothing outside its run folder.

```text
cargo build -p regia_editor --bin regia-editor
cargo xtask editor-smoke                       # lavapipe on a private Xvfb, stop at the first FAIL
cargo xtask editor-smoke --keep-going          # run every step
cargo xtask editor-smoke --only 'panel:*'      # one kind; globs match `kind:name`
flock /tmp/regia-gpu-1.lock flock /tmp/ferrite-gpu-1.lock env REGIA_GPU_LOCK_HELD=1 cargo xtask editor-smoke --card 1 --keep-going
cargo xtask editor-smoke --windowed --keep-going   # a real window on the desktop (the Mac, the Windows rig)
```

| Flag | Meaning |
|---|---|
| `--only <glob>` | Run only steps whose `kind:name` matches (`*`, `?`; repeatable): `panel:viewport`, `menu:File/*`, `command:view.*`. |
| `--skip <glob>` | Leave matching steps out (repeatable). |
| `--card <idx>` | Use that GPU (the NVIDIA index `nvidia-smi` prints, selected by its UUID with `REGIA_GPU_STRICT=1`; elsewhere `REGIA_GPU_INDEX`). The run must hold the card's locks (new, then pre-rename; ADR-8670): `flock /tmp/regia-gpu-<idx>.lock flock /tmp/ferrite-gpu-<idx>.lock env REGIA_GPU_LOCK_HELD=<idx> ...`. Without it: lavapipe (`VK_LOADER_DRIVERS_SELECT=*lvp*`). |
| `--windowed` | Use the desktop's display instead of a private Xvfb. |
| `--keep-going` | Run every step after a failure (default: stop at the first FAIL, still write the report). |
| `--editor <exe>` | The editor executable (default `regia-editor` in the cargo target folder's `debug`). |
| `--sample <dir>` | The project to copy (default `samples/third_person`, the sample with the most asset kinds: anim graph, camera, conversations, localization, abilities, montage, behavior trees, vehicle). |
| `--out <dir>` | Output root (default `target/editor-smoke`). |
| `--headless` | Drive `regia-editor --headless` (no window): menus, commands, actions and routes run, screenshots are the offscreen viewport, and the panel, keyboard, scale and theme steps are skipped. For a card that cannot present to Xvfb (an NVIDIA card on a server without a desktop). |
| `--keep-work` | Keep the project copy and the redirected per-user folders (`work/`) to read what the steps saved. |
| `--inventory-only` | Read and print the inventory, drive nothing. |

## What it enumerates

Nothing is hard-coded: a panel or an action another lane adds is driven by the next run. At start the harness reads,
from the running editor:

| Kind | Source |
|---|---|
| Panels | `editor.layout_get` → `layout.known_panels` (every built-in panel), plus the shown panels it does not list (extension panels) |
| Menu items | `editor.menu_list` (path, action, class, method, params, enabled) |
| Palette commands | `editor.command { limit: 200 }` |
| Registered actions and control sites | `editor.actions` (the parity registry and the allowlist) |
| Agent methods | `agent.hello` → `methods` |
| Required params | `docs/api-snapshots/editor-agent-methods.txt` (the editor-method snapshot) |
| Safe default params | the first `{"method": "editor.…", "params": …}` line of each method in `book/agents/*.md` |

It prints the counts per kind first and writes them to `inventory.json`. Two findings come from reading alone: an
allowlist site of class `gap` (a control with a GUI action and no agent route, `no_agent_route`), and a registry
action or menu item whose method the running editor does not answer (`route_missing`).

## What it drives

| Kind (`kind:` in globs) | Step |
|---|---|
| `panel` | `editor.layout_apply { open }`, wait two frames (`play.wait_until` on `Time.frame`), check the panel is in the layout, wait two more, `play.expect` that two frames drew, `seeing.screenshot` cropped to the panel body from `editor.layout_get`, `editor.layout_reset`. |
| `menu` | The item's own call (`method` with `params`; an `os_dialog` path becomes a file under `<project copy>/smoke-out/`). |
| `command` | `editor.command { run }` for every command no menu item already ran. |
| `action` | Every registered action no menu item or command already covered, through its method with the guide's example params (a `confirm` action gets `confirm: true`). |
| `route` | Every distinct agent method an allowlist control site names (the buttons inside panels) that no step above called, once, with the guide's example params. |
| `keyboard` | Real key presses through the X test extension on the private Xvfb (the harness focuses the editor window): Tab three times and Down three times must change `editor.layout_get.focused_widget` to a different non-null ID; Ctrl+Shift+P must open the palette (`editor.command` → `open`), and typing `togglegrid` plus Return must run `view.toggle_grid` (`recent`; the grid is put back). Skipped with `--windowed` (keys would go to the desktop). |
| `scale200`, `light` | `editor.prefs_set` `appearance.ui_scale` 2.0, then `appearance.theme` `regia-light`; the main layout and every panel again; `editor.prefs_reset` after. A play session an earlier step left running is stopped first (`editor.stop`). A panel crop that is pixel-identical to the same panel's crop in the first pass fails: the preference changed nothing. |

Every call that may change the level records `editor.history` → `undo_depth` and a fingerprint of the level
(`scene.serialize`, hashed) before it. After it, the undo entries it added are undone with `editor.undo` and the
fingerprint must be back. A change of the level without an increase in undo depth fails the step, including a missing depth. Two fingerprint reads a few
frames apart must agree first for undo-restoration comparisons (said in the notes); an observed untracked change
still fails even when those initial reads disagree.

**Skipped on purpose** (the reason is in the step): quitting, packaging and publishing, network sessions and remotes,
device deploys and launches, external programs (browser, code editor, file manager), clearing cache folders, building
game code, and long bake, cook or render jobs. Everything else runs inside the project copy; `HOME`, the XDG folders,
`REGIA_EDITOR_CONFIG_DIR`, `REGIA_AGENT_DIR` and `TMPDIR` point into the run folder, so preferences, layouts and
keymaps the steps save never reach the user's files. A call the editor refuses with a typed error (`invalid_params` or
a `data.code`) is SKIP with the editor's message: the default params did not fit, which is not a defect.

**Play sessions** (ADR-8847): a step that is not a play control starts in edit mode. When `editor.play_state` says a
session an earlier step left running is still playing or paused, the harness calls `editor.stop`, waits three frames
and checks the mode is `stopped` (the step's reason says so; a session that does not stop fails the step). Play
controls keep the session so `⏸ Pause` after `▶ Play` still finds it: Play menu items, `play.*` actions and commands,
and the routes `editor.play`, `editor.pause`, `editor.resume`, `editor.step`, `editor.stop`, `editor.simulate`,
`editor.play_*` and `editor.pie_*`. Before this, the split-screen session of `menu:Play/Players/4 players` ran
through every later step, and their headless screenshots were the possessed game view of the empty sample level, one
flat color. A play control's own flat screenshot still fails; its reason names the play view (`playing, 2 players`).

## Checks per step

- The editor still runs and its log printed no panic. A crashed editor is restarted on the same copy (at most six
  times) and the run goes on with `--keep-going`. A restart passes `--normal-mode`: two crashes in a row would
  otherwise start the editor in safe mode, which never opens the agent listener (ADR-8119).
- No error-level log record (`logs.read`) and no egui id warning ("changed id between passes", "Double use of").
  egui paints a clashing id on screen too: look at the step's screenshot.
- No panel draw over the slow-frame bar (`editor.panel_timings`, the slowest panel's draw during the step): 250 ms on
  lavapipe, 50 ms on a card or a desktop window.
- The screenshot is not one flat color (all pixels within 2 levels of the first in every RGBA8 channel). Panel
  steps save and inspect only the visible panel body, using `panel_regions` and `pixels_per_point` from
  `editor.layout_get`. Missing, invalid, empty or out-of-image rectangles fail; there is no whole-window fallback.
  The crop rounds inward so tab bars and neighboring panels cannot supply the variation.
- The step's own expectation: the panel is in the layout after the open and two frames drew; the call answered; an undo
  restored the fingerprint. `play.expect` must return successfully with boolean `pass == true`; missing methods,
  transport errors, missing fields and other values fail. Focus reads must succeed and report valid IDs or null;
  an absent/malformed field fails. Losing focus without reaching another widget also fails.

## Output

`target/editor-smoke/<YYYYMMDD-HHMMSS>/`:

| File | Content |
|---|---|
| `result.json` | `format_version` 1, `started`, `mode` (`lavapipe`, `card <n>`, `windowed`), `adapter`, `frame_budget_ms`, `command`, `inventory` (counts), `findings`, `steps` (`area`, `kind`, `name`, `outcome` `PASS`/`FAIL`/`SKIP`, `reason`, `time_ms`, `frame_ms`, `call`, `log_excerpt`, `screenshot`), `totals`. |
| `summary.md` | Counts overall and per kind, every FAIL with its log line, screenshot and reproduction command (`cargo xtask editor-smoke --only '<kind:name>'`), the inventory findings, notes, skips with reasons, and the contact-sheet cell lists. |
| `panels.png`, `menus.png`, `dialogs.png`, `scale200.png`, `light.png` | Contact sheets, six thumbnails a row in run order, a red frame round a failed step. |
| `shots/` | Every screenshot. |
| `levels/` | The level text before and after a step that changed it without an undo entry, or whose undo did not restore it. |
| `inventory.json` | The full inventory. |
| `editor.log` | The editor's stdout and stderr. |

The project copy and the redirected folders are deleted at the end (`--keep-work` keeps them). The exit code is 1 when any step failed.

## Hosts

Linux needs `Xvfb` unless `--windowed` or `--headless`. On the Mac and on Windows use `--windowed` (a real desktop
session); keep the machine awake (`caffeinate -d` on the Mac). An NVIDIA card on a server without a desktop cannot
present to Xvfb (the editor exits with `regia-editor: GPU selection failed: … cannot present to this window`, which the
harness reports at once): run the card with `--headless`.

Measured on the build server (2026-11-15, debug build, shared farm at load 230-700): lavapipe, 659 steps in about 20 minutes;
RTX 5090 with `--headless`, 476 steps in about 6 minutes. The panel draw times of a loaded host are inflated; read the
slow-frame reason ("the first draw; median of its other draws …") before acting on one.

A harness killed from outside (SIGKILL, a lost ssh session) cannot stop its editor and Xvfb: stop them by their process
ids (the editor's working folder is the run's `work/project`).

## Regression checks for the harness

```text
cargo test -p xtask --lib editor_smoke
cargo test -p regia_editor --lib observation
```

The step fixtures require a blank panel to fail even with a varied window, strict expectation replies,
undo-depth growth for changed scenes, and actual focus-ID movement even while window pixels change.
The editor observation tests also render a stub dock panel on a headless device: visible tab chrome with
an empty body must yield a uniform body crop; adding a label must make it non-uniform, at both 1× and 2×.
These tests require a working Vulkan adapter (lavapipe is sufficient) and create no golden images.
