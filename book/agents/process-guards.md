# Fast quit and single instance

Two process guards for shipped games (ADR-8833): a quit that takes the window away at once and ends
the process without tearing the world down, and one running instance per user.

## Fast quit

The default quit is the clean exit: stop background GPU work, leave fullscreen, wait for the GPU,
drop the surfaces, the world and the device, save the pipeline cache (see [frame loop and
quitting](loop.md)). With a large world partly in swap, or on a slow laptop, that can take seconds
with the window still on screen. `ExitMode::Fast` takes another path:

1. **Hide**, in the frame the quit arrives: every OS window is taken off screen (X11 unmap, Win32
   `SW_HIDE`, macOS `orderOut`, Wayland a null buffer commit), the renderer stops presenting, and the
   host's hide hooks run (`ExitHooks::on_fast_hide`). A hard deadline is armed (3 s, exit code 5).
2. **Flush**, at the end of that frame: every flush job (`ExitHooks::add_flush`: a save, the pipeline
   cache, a network leave) runs on its own thread, all in parallel, inside the flush budget (1.5 s).
   Files registered with `regia_platform::keep_file_on_fast_exit` are synced to disk. A job still
   running at the budget is left behind.
3. **End**: one summary line goes to the log and stdout, stdio is flushed, and the process ends with
   `_exit` (Linux, macOS, Android) or `TerminateProcess` (Windows). No world, renderer or device is
   dropped; the OS frees the memory and the GPU driver frees its objects in bulk.

A game opts in for every quit (window close, `AppExit`, `regia::request_clean_exit`) by inserting the
resource:

```rust,wrap
# let mut app = regia_app::App::new();
app.insert_resource(regia_platform::ExitMode::Fast)
    .insert_resource(regia_platform::FastExitSettings::default()); // budgets, optional
```

or quits fast once from a `&mut World` handler with
`regia_platform::request_fast_exit(world, code, "reason", 0)`. A host loop without the platform
runner calls `regia_platform::poll_fast_exit(&mut app)` after each update; if it never does, the
`AppExit` the request also writes ends it the clean way.

What persists must be written by a flush job or synced by `keep_file_on_fast_exit`: nothing is
dropped, so `Drop` impls do not run.

### `quit.request { mode: "fast" }`

The agent's `quit.request` takes an optional `mode`: `clean` (the default, like closing the window,
`ClosePolicy` decides) or `fast` (the host's decision, `ClosePolicy` does not apply):

```json
{ "mode": "fast" }
→ { "window": null, "will_exit": true, "handled_by": "engine", "mode": "fast",
    "requested_frame": 412, "flush_frame": 413, "windows_hidden": 1 }
```

The windows are hidden at once; the flush and the process end come one frame later, so this reply
leaves the socket first. The connection then closes. Check it in the app's log:
`regia fast exit: requested by agent quit.request ...` then
`regia fast exit: code=0 reason="agent quit.request" frame=412 windows_hidden=1 flush_ms=<ms> steps=<name>:ok:<ms>,...`
(`timeout` instead of `ok` for a job cut at the budget). The process exit code is 0, or 5 if the hard
deadline ended it.

| Error | When |
|---|---|
| `INVALID_PARAMS` (`F0002`) | `mode` is not a string, or not `clean` or `fast`. |

## Single instance

`regia_platform::SingleInstanceLock::acquire(app_id, wait)` takes a per-user, per-app lock at start-up.
A second start of the same app by the same user waits up to `wait` for the first to leave (a
relaunch), then is refused with `SingleInstanceError::AlreadyRunning`, which names the running
instance's pid; the host prints it and exits with `EXIT_ALREADY_RUNNING` (75). On Windows the running
instance's window is raised first.

```rust,wrap
# use std::time::Duration;
let _instance = match regia_platform::SingleInstanceLock::acquire("my-game", Duration::from_secs(4)) {
    Ok(lock) => lock, // keep it alive for the whole run
    Err(err) => {
        eprintln!("{err}");
        std::process::exit(regia_platform::EXIT_ALREADY_RUNNING);
    }
};
```

| Platform | Lock | Where |
|---|---|---|
| Linux | `flock` | `$XDG_RUNTIME_DIR/<app>.instance.lock` (else the temp dir with the uid in the name) |
| macOS | `flock` | the per-user `$TMPDIR` |
| Windows | `LockFileEx` | `%LOCALAPPDATA%\RegiaEngine\instance\<app>.instance.lock` |
| Android | none | one task per app is the OS's rule (the activity's launch mode); `acquire` returns an OS-managed guard |

The OS releases the lock when the process ends, crashes included, so a crashed game never blocks the
next start. The holder's pid sits in `<lock>.pid`; a clean release removes it, so a pid found there
while the lock is free belongs to an instance that died holding it. The new instance takes it over and
reports it (`took_over_from()`, a warning in the log). On a file system without locks the pid record
decides alone: a live pid refuses the start, a dead one is taken over.

`REGIA_ALLOW_MULTI_INSTANCE=1` skips the lock (test runs that start several copies).

| Code | Meaning |
|---|---|
| `already_running` | Another live instance holds the lock (`SingleInstanceError::AlreadyRunning`). |
| `instance_lock_io` | The lock file could not be created or opened (`SingleInstanceError::Io`). |
