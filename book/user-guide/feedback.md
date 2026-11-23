# Feedback and bug reports

`regia_framework::feedback` lets a player send feedback from inside the game. The game keeps
running while they do: nothing pauses and networking is not touched. The player picks
*Send feedback* in the pause or settings menu, chooses a category (bug, performance, visual or
other), types a note and presses Send (or Enter). The engine then writes **one file** to the
feedback folder and shows where it went. When the game panics, the same file is written
automatically. On the next launch a notice tells the player where that file is.

**Privacy:** the engine writes local files only. Nothing leaves the machine unless the game
uploads the bundle itself, and the game should ask the player before it does. The prompt says
so on screen.

## Add it

```rust,wrap
# use regia::prelude::*;
# let app: &mut App = todo!();
use regia::prelude::*;
use regia_framework::feedback::{FeedbackHost, FeedbackPlugin};

app.add_plugins(FeedbackPlugin::default());
```

It needs `regia_ui::UiPlugin` for the prompt and the notices. Without the UI plugin,
`Feedback::capture` still writes bundles. It needs `RenderPlugin` for the screenshot, the CPU/GPU
frame times and `render.problems`. Without the renderer those parts say why they are missing.

## Open the prompt

**No key is bound by default.** DO27 uses the F-keys. Use one of these instead:

- **A menu entry.** Put `FEEDBACK_MENU_ITEM` ("Send feedback") in the pause or settings menu and
  call `regia_framework::feedback::open_prompt(world)` when the player picks it. Or spawn the
  ready-made button with `spawn_feedback_button(&mut commands, menu_parent)`, which opens the
  prompt when clicked.
- **An input action.** The plugin watches the `"Feedback"` action (`FEEDBACK_ACTION`). It has no
  binding. A game that wants a key binds the action in its `InputConfig`, and players can rebind
  it like any other action (`InputRebinder`).
- **A raw key.** Set `FeedbackConfig::hotkey` to a physical key name such as `"KeyB"`. Leave it
  `None` for DO27.

The screenshot is taken before the panel appears, so the panel is never in it.
`Feedback::is_prompt_open()` is true while the prompt shows. Skip gameplay input during that
time, because the text field and the game read the same keys. Escape cancels.

## What a bundle holds

Bundles are `feedback-<UTC time>-<category>.zip`, written to `user://feedback` (on Windows
`%LOCALAPPDATA%\RegiaEngine\<game>\feedback`, not the roaming `%APPDATA%`, so crash minidumps never sync
with a roaming profile; ADR-525):

| Entry | What |
|---|---|
| `feedback.json` | category, note, time, map and position, the list of parts |
| `note.txt` | the note |
| `screenshot.png` | the frame when the prompt opened (crash bundles have none) |
| `frame_times.csv` | the last 30 s of frame, CPU and GPU times, plus device creates per frame |
| `hitch_report.txt` | the hitch report over those frames |
| `system.txt` | OS, device, CPU, RAM, GPU and driver |
| `settings.ron` | the active settings and graphics preset |
| `render_problems.txt` | the renderer's problems (`render.problems`) |
| `log_tail.txt` | the recent log |
| `build_info.json` | version, git hash, profile, and the packaged `build_info.json` |
| `host.json` | the game's own fields |
| `crash.txt` | crash bundles only: the panic message, location and backtrace |

The folder keeps the newest 50 bundles (`FeedbackConfig::keep`). The PNG encode and the file
write run on a worker thread, so sending never causes a hitch.

## Add game state (host hook)

```rust,wrap
# use regia_ecs::prelude::World;
# use regia_framework::feedback::FeedbackHost;
# let mut world = World::new();
# world.insert_resource(regia_framework::feedback::FeedbackHost::default());
# let match_id = "match-42";
# let players = 2_u32;
let mut host = world.resource_mut::<FeedbackHost>();
host.set("match_id", match_id);
host.set("map", "mars_outpost");
host.set("player_count", players.to_string());
// Values that change every frame are computed only when a bundle is captured:
host.set_provider(|_| vec![("position".into(), "0, 0, 0".into())]);
```

`set` fields go into crash bundles too. Provider fields go only into prompt and agent captures,
because a panic hook cannot reach the world.

## Upload later (the game's choice)

The engine never uploads. A game that wants to collect bundles does it like this:

- read `FeedbackWritten { info, from_prompt }` messages, or call `pending_bundles(world)` at
  start-up to find bundles from earlier sessions (crash bundles included);
- ask the player, then send `info.path` to its own endpoint. `regia_crash`'s transport (HTTPS
  with backoff) is one option.

## Crashes

A panic writes `feedback-<time>-crash.zip`. The engine's `crash-*.txt` report in `logs://` is
still written as before. Two panics within 5 s write only one bundle, so a panic caught in a loop
cannot fill the disk. On the next launch a notice shows the path for 15 s, and the log has a warning
with the same path. Each crash is announced once (`announced.txt` in the folder records which).

### Native crashes (segfaults, driver crashes)

On Linux and Windows a native crash writes the same bundle. On Linux the trigger is SIGSEGV,
SIGBUS, SIGILL, SIGFPE, SIGABRT or SIGTRAP; on Windows it is an unhandled exception (access
violation, illegal instruction, stack overflow, heap corruption, a C runtime abort or invalid
parameter). Both include a crash inside the GPU driver. It needs one line first in the game's `main`:

```rust
fn main() {
    // In the crash monitor process this runs the monitor and never returns.
    regia_framework::feedback::run_crash_monitor_if_requested();
    // ... the game as before ...
}
```

How it works:

- At startup, `FeedbackPlugin` re-launches the game's own executable as a small **monitor**
  process (Embark's `minidumper`). Writing a dump inside a crashed process is unsafe, so the
  monitor does it.
- On a crash, the signal handler (on Windows an unhandled-exception filter; both Embark's
  `crash-handler`) sends the monitor the signal, the
  frame ring and the log tail. It copies them from memory allocated at startup: no allocation in
  the signal path, and only `try_lock`, so a crash while the frame or log lock is held just drops
  that part.
- The monitor writes `minidump.dmp`, then the game dies exactly as it would have (same exit
  status, Rust's stack-overflow message, a core dump if enabled; on Windows the exception code as
  exit code and Windows Error Reporting as usual).
- The monitor then writes `feedback-<time>-crash.zip`. It holds `crash.txt` (signal, meaning,
  fault address, thread name and id, the last frame), `minidump.dmp`, the frame times and hitch
  report, the log tail, settings, render problems, host fields, system facts (GPU and driver
  included) and `build_info.json`.
- The next launch announces it exactly like a panic bundle.
- The crash path is bounded: the moment the signal arrives, a watchdog is forked that kills the
  game (`SIGKILL`) 5 s later (on Windows a thread started at arming time calls `TerminateProcess`
  after 5 s). A stuck monitor never leaves a crashed fullscreen game frozen on
  screen; at worst the bundle has no minidump.

It never makes a crash worse. Nothing is installed, and crashes behave as before, in any of these
cases:

- `main` skipped the early call.
- The monitor could not start or answer within 10 s.
- The host set `FeedbackConfig::native_crashes = false` (or `crash_bundles = false`).
- The player started the game with `REGIA_NO_NATIVE_CRASH=1`.
- On Windows: capture is opt-in (not yet tested on real Windows). Set `FeedbackConfig::native_crashes_windows = true`
  or `REGIA_NATIVE_CRASH=1`. `REGIA_CRASH_WATCHDOG_S` (default 5) bounds the crash path;
  `REGIA_CRASH_DUMP_WAIT_S` (default 30) is how long Windows waits for a minidump still being written.

Under `panic = "abort"` (the shipping profile), a panic writes its panic bundle. The abort that
follows writes no second bundle. `feedback::native_crash_status()` says which case applies
(`Armed`, `NoEarlyCall`, `OptedOut`, `Failed`, `Unsupported`). Android builds it as a no-op
(`Unsupported`); it keeps panic bundles only.

On Windows `crash.txt` names the exception (`EXCEPTION_ACCESS_VIOLATION (write to an inaccessible
address)`), the monitor talks to the game over an `AF_UNIX` socket file in the temp directory (it
needs Windows 10 1803 or newer) and removes it when it ends. A `panic = "abort"` abort on Windows
is a fast fail (`STATUS_STACK_BUFFER_OVERRUN`) that skips every exception filter, so the panic
bundle is the record there and no native bundle is written for it. Real Windows crash tests are
still pending: the code is cross-checked with `cargo xwin clippy` and the bundle layout is unit
tested on Linux.

Cost: the monitor is an idle process blocked in `poll`, and the game installs signal handlers.
It sends the monitor the crash context only when that changes (settings, host fields, render
problems), which is when the panic hook's copy changes too. There is no per-frame work.

**Reading a minidump.** `cargo xtask minidump <bundle.zip or .dmp> --symbols dist/<platform>/symbols`
prints the crash reason, the crashing thread and its stack. It uses `rust-minidump` to walk the
stack and `addr2line` (binutils) to resolve each frame to function, file and line.

The symbols must match the build that crashed:

- `regia package` writes `symbols/<binary>.debug` for every shipped binary.
- **Keep that `symbols/` directory for every release you ship**, next to the release's
  `build_info.json`. It is not in the player's download.
- The report prints the main module's build id. Use it to pick the right release's symbols.
- A local unstripped build needs no `--symbols`: the binary at the path recorded in the dump is
  used.
- Frames inside system libraries and the GPU driver show as `module+offset`.

**Reading a Windows minidump on Linux.** `regia package --profile shipping` on Windows writes
`symbols/<name>.pdb` next to the package (not in the player's download; keep it with the release,
like the Linux `.debug` files). `cargo xtask minidump` names the exception and the crashing
thread's modules and offsets, but it resolves function names only through `addr2line`, which does
not read PDBs. For names, convert the PDB to Breakpad symbols and walk the dump with
rust-minidump's CLI:

```sh
cargo install minidump-stackwalk dump_syms           # once
dump_syms symbols/game.pdb --store symbols/breakpad  # per release, from the matching PDB
unzip feedback-<time>-crash.zip minidump.dmp
minidump-stackwalk --symbols-path symbols/breakpad minidump.dmp
```

`--store` lays the files out as `game.pdb/<debug id>/game.sym`, which is what the stack walker
looks up by the module's debug id. A PDB from another build has another id and is ignored, so
keep one `symbols/` per release. `minidump-stackwalk --human` prints a readable report.
(Not yet exercised on a real Windows dump.)

## Cost

When nobody uses it, each frame pushes one sample into a ring that was allocated once (8192
samples, about 320 KiB) and runs a few change checks. Nothing is allocated per frame, and
`tests/feedback_cost.rs` checks this. The prompt system stays idle behind a run condition until
something opens it.

## Agents

`feedback.capture` and `feedback.list`: see [Feedback bundles](../agents/feedback.md).
