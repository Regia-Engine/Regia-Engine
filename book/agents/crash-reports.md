# Crash reports (`diagnostics.crash_reports`)

When a running app panics, the platform panic hook writes a self-contained crash report to
`logs://crash-<timestamp>.txt` (panic message + backtrace, build identity, OS/GPU facts, the active
level and frame count, and the last 200 log lines). Pending reports survive restarts; an opt-in
uploader (`regia_crash::CrashReporter`, umbrella `regia::crash`) can send them to a
game-configured endpoint. `https://` endpoints need `regia_crash`'s `tls` feature (ureq + rustls);
without it `https://` is refused. Plain `http://` is sent only when the host string is `localhost`
(ASCII case-insensitive) or a literal loopback IP such as `127.0.0.1` or `::1`. The name is not
resolved, so a LAN address of this machine is treated as remote, and `localhost` is treated as local
even when a resolver would return something else. Any other `http://` host leaves uploads off unless
the game sets `CrashReporter::allow_cleartext`. The uploader then sends scrubbed UTF-8, not the raw
file bytes: invalid UTF-8 becomes U+FFFD, the home directory becomes `~`, secret values become
`[redacted]`, and email addresses become `[email]`.

The agent method `diagnostics.crash_reports` (MCP tool `crash_reports`) lists and reads those
reports so you can diagnose a failure without touching the device's filesystem.

## List

Call `diagnostics.crash_reports` with no parameters:

```json
{ "jsonrpc": "2.0", "id": 1, "method": "diagnostics.crash_reports", "params": {} }
```

```json
{ "reports": [{ "name": "crash-2026-10-06_12-34-56.txt", "bytes": 1234, "modified_unix_ms": 1768210496000 }], "count": 1 }
```

Reports are ordered oldest first. An empty `reports` array means no bare `crash-*.txt` is pending.
The uploader removes a report after the server answers 2xx, when the path is still the file that
was sent. A final rejection (any status other than 2xx, 408, 429, or 5xx, including a 301 or a
404) renames the file to `*.rejected`. That suffix is not listed here and is not sent again. A
transport failure, 408, 429, or 5xx leaves the `crash-*.txt` for the next start.

## Read

Pass the file `name` to read one report's full text:

```json
{ "jsonrpc": "2.0", "id": 2, "method": "diagnostics.crash_reports", "params": { "name": "crash-2026-10-06_12-34-56.txt" } }
```

```json
{ "name": "crash-2026-10-06_12-34-56.txt", "content": "Regia Engine crash report\n…\npanic: …\nbacktrace:\n…" }
```

An unknown name answers `F0003`. The `name` is always a bare file name (path traversal is
rejected), so you can only read what `list` showed you.

## Reading a report

The report body has fixed sections. The first line of the body is `panic: <message>` for a panic,
or `<kind>: <reason>: <message>` for a device or surface loss (`device_lost` or
`surface_unavailable`). Then `location:`, `build:` (version, git hash, profile, engine), `system:`
(OS, device, RAM, CPUs, GPU), `runtime:` (frame count, active level), `backtrace:` (symbolicated
where debug info exists), and `recent logs (last 200):`. A stripped shipping binary's backtrace
shows raw addresses — resolve them with the symbols kept in `dist/<p>/symbols/` (see the packaging
guide).

## Privacy

Crash reports can contain file paths, GPU/driver strings and log text. Reading them needs only the
`read` permission, but treat their contents as user data: don't post them anywhere the project
hasn't approved. Sending is the game's choice (the uploader is opt-in and consent-gated); the agent
protocol only exposes the local copies.

## Exits that skip the exit handlers

Not every abnormal end is a crash report. Two exits end the process on purpose without running its exit handlers
(`_exit` / `TerminateProcess`), and write no `crash-*.txt`:

- **Device lost or no window surface** (ADR-276): exit code 3 or 4, with a feedback bundle that explains it.
- **A GPU worker still running at the end of a quit** (ADR-287): after the app has exited, the engine waits for its
  threads that may be inside the GPU driver (`regia-telemetry-probe`, `regia-pipeline-warm-N`,
  `regia-matgraph-N`, `regia-encode-N`, `regia-renderer-drop`, `regia-exit-renderer`,
  `regia-exit-device`). If one is still running when the budget runs out (30 s from the start of the quit,
  `REGIA_EXIT_WORKER_WAIT_MS`), the process ends with the app's own exit code, and the last log line is:

  ```text
  regia exit: 1 GPU worker(s) still running after 30000 ms: regia-matgraph-2 (in a driver call); ending the process with code 0 without process-exit handlers (ADR-287)
  ```

  `(in a driver call)` means the thread was inside a compile, an allocator report or a segment finish. This is a
  driver that did not return, not an engine crash: report it with the log and the GPU driver version. A quit that
  took long but finished logs `regia exit: waited <ms> ms for GPU worker(s) <names>` instead.

The agent connection is gone by then, so read these from the log file of the run (`logs://`) on the next launch or
from the machine.

## Android: native crash records

A native crash on Android (a SIGSEGV in the Mali or Adreno driver, a bad pointer in native code) cannot write a
minidump: Android has no crash monitor process (ADR-328). Instead the crashing process writes a small record itself,
from the signal handler, into the app's internal storage (`files/crash/pending.bin`, prepared at startup), and the
**next launch** reads it back. Every Android Regia Engine app has this; it needs no setup.

On the next launch you get, in order:

1. One warning line in logcat (tag `regia_platform`) and in `logs://`:

   ```text
   native crash record from the last run: SIGSEGV (code 1) at 0x10 on thread regia-crasher (tid 9131), record /data/user/0/<app>/files/crash/2026-10-21_17-02-11.crash.bin, report /data/user/0/<app>/files/logs/crash-2026-10-21_17-02-11-native.txt
   ```

2. A text report `logs://crash-<time>-native.txt`, which `diagnostics.crash_reports` lists and reads like any other
   report. It has the signal and code, the fault address, the thread name and id, the build identity of the run that
   crashed (version, git hash, profile, engine), and the stack.
3. With `FeedbackPlugin`, a crash bundle in the feedback folder (`feedback-<time>-crash.zip`), announced like any
   crash bundle. Its `feedback.json` `crash` object has the same keys as a desktop native crash, with
   `"minidump": null`, `"minidump_error"` saying why, `"platform": "android"` and a `"stack"` array; `log_tail.txt`
   holds the end of the crashed run's log file.

The record file is renamed to `files/crash/<time>.crash.bin`; the newest 5 are kept. A record that is cut short or
corrupt is removed and logged (`native crash record from the last run is unreadable and was removed: ...`). A
`SIGABRT` within 5 s after a panic report is the `panic = "abort"` abort: it gets no second report or bundle.

### Reading the stack

The stack is the program counter, the link register, then up to 32 return addresses from the frame-pointer chain
(the walk stops at the bound, at an address that is not a valid frame, or at the end of the chain; the report says
which). Each line names the library and the file offset in it, and, when the library is still on the device, its
virtual address:

```text
pc 0x0000007a1c2d1234 /vendor/lib64/egl/libGLES_mali.so+0x41234 (vaddr 0x52234)
#00 0x0000007b88001a40 /data/app/~~x/<app>/lib/arm64/libmain.so+0x3a1a40 (vaddr 0x3b1a40)
```

Resolve the game's frames with the unstripped library from the release's symbols:
`llvm-addr2line -fC -e libmain.so 0x3b1a40` (use the `vaddr`). A frame in a vendor driver (`libGLES_mali.so`,
`vulkan.*.so`, `libllvm-qcom.so`) cannot be symbolized by you, but the library name and the offset are what a driver bug
report needs. Code built without frame pointers ends the walk early; the PC and LR are always there.

### What is not recorded

No minidump, no other threads' stacks, no registers beyond PC/LR/SP/FP, no frame-time ring and no host fields (the
record is written inside the crashing process, with no allocation). The OS tombstone still has the full register dump:
`adb logcat -b crash` or `adb bugreport`.

### Checking it on a device

`cargo xtask android-pass --package <app id> --project <game> --crash-test` installs the app, sets the
`debug.regia.crash_test` property (`segv:8`), launches it, waits for it to die, clears the property, relaunches it,
and passes when the record line names `SIGSEGV` and the thread `regia-crasher`. With `--uninstall` it removes the
app at the end, also when the crash test fails, and `summary.txt` says `uninstall: done` only when
`pm list packages` no longer lists it. By hand:

```sh
adb shell setprop debug.regia.crash_test segv:8     # the app reads address 0x10 8 s after startup
adb shell monkey -p <app id> -c android.intent.category.LAUNCHER 1
adb shell setprop debug.regia.crash_test "''"        # after it died
adb shell monkey -p <app id> -c android.intent.category.LAUNCHER 1
adb logcat -d -s regia_platform | grep "native crash record"
```

Only `adb shell` or root can set a `debug.` property, so the trigger cannot be pulled by another app.

## Hang snapshots (P-08, ADR-8156)

A Linux GNU host can call `regia_crash::stack_dump::install()` once at startup,
then `write_hang_report(reason, &Settings::default())` on its watchdog thread.
The returned path is a normal pending crash report. List it and read it with
`diagnostics.crash_reports` using the calls above. Assert on the returned `content`
for `hang:`, `thread stacks:`, a worker name or an explicit outcome.
There is no new agent method or error code. Existing list/read errors are unchanged.

`capture` returns structured `StackDump` rows for every thread enumerated through
`/proc/self/task`, including its caller. Thread creation after enumeration and
thread-ID reuse are inherent snapshot races. Each row has the OS ID/name,
`is_caller`, `outcome`, and symbolized stack text. Identical stacks retain all
identities. `Captured` contains raw instruction pointers; other outcomes are
`Exited`, `NoResponse`, `WalkTimedOut`, `Busy`, `LimitReached` or `SignalError(errno)`.
A blocked signal does not produce fabricated frames. A timed-out active walker
keeps exclusive buffer ownership until it finishes, even across later captures.

Settings control maximum frames (1–256, default 96), maximum sampled threads
(default 1024), signal wait (250 ms), additional walk wait (2 s), and maximum bytes
per ELF symbol/string table (64 MiB; zero skips names). Symbol names are capped at
1024 bytes by `max_symbol_name_bytes`; parsing also caps total name bytes scanned.
Limits never drop identities.
Times are diagnostic budgets, not simulation timing. Collection is sequential,
not an atomic view of all threads. Total work can approach the thread count times
the configured waits; call it on a watchdog, not per frame. ELF names may remain
mangled. Addresses are translated through executable ELF load segments for
offline `addr2line -C -f -i`; keep the exact executable/modules. If the ELF
metadata is unavailable or exceeds the budget, `file+` labels explicitly identify
file offsets and retain the raw instruction pointer instead of guessing a symbol.

Installation reserves `SIGRTMIN + 6`; `install_with_signal` selects another
real-time signal at startup. Existing handlers are refused, not replaced. The
host coordinates process-wide signal users, must retain the sampler's code and
handler for its lifetime, and must not unload or replace them.

Requested dumps are opt-in: `install_request_handler()` reserves SIGUSR1, or
`install_request_signal(signal)` reserves SIGUSR1, SIGUSR2 or another real-time
signal. Its handler only sets a flag; a watchdog polls `take_request()` and calls
`write_hang_report` on its own thread. `request()` sets the same coalescing flag
without a signal. Occupied signals are refused. No periodic task or automatic
fatal-signal/panic capture is installed. Unwinding is
best effort and is not promised async-signal-safe by libc; a self-sample on the
calling thread cannot time out its own unwinder. This is a live-hang diagnostic,
not a reliable fatal-crash handler. Linux musl, Windows, macOS and Android return
`Unsupported`; their existing native crash mechanisms are unchanged.

Saved reports use the existing privacy scrubber, retention and opt-in upload
policy. Raw `StackDump` data is local diagnostic data and is not scrubbed until
saved. Missing logs configuration or IO failure returns an error to the host.

## Windows bare Alt (P-08)

The platform runner installs bare-Alt suppression on the primary window. A custom
menu-free winit host calls `regia_platform::alt_menu::install(&window)` on the
thread that owns the window. Other operating systems treat installation as a no-op.
`swallows_message` exposes the exact decision for portable host-policy tests.

Only `WM_SYSCOMMAND` with command `SC_KEYMENU` and no mnemonic (`lparam == 0`)
is swallowed. Alt key press/release, Alt+Space, Alt+F4, Alt+Tab and other messages
continue through the normal input/window procedure. The subclass removes itself
when the window is destroyed. Installation failure is returned to custom hosts
and logged by the runner. Agents can inspect settings with `window.get` and
exercise Alt input with `input.key`; native Windows tests must separately verify
focus and continued redraw. Synthetic input alone is not proof of Win32 delivery.

The session logs themselves are uploaded incrementally by the report outbox when the game sets
`CrashReporter::outbox`; a crash report also resends its session's log once. See
[Report outbox](report-outbox.md) and `report.outbox_status`.
