# Editor crash reports

ADR-3931. Code: `regia_editor::crash_reports`. The user's view is
[When the editor crashes](../user-guide/editor.md#when-the-editor-crashes).

The editor installs the same panic hook games use (`regia_platform::crash::install_panic_hook`)
before the window opens, aimed at the editor log folder (`<user data>/RegiaEngine/<exe>/logs`).
The written file is a game crash report: panic message, location, backtrace, build, OS, GPU
adapter and the last 200 log lines. An editor step then replaces home directories with `~`
and caps the file (256 KiB, cut on a UTF-8 boundary, ending in `[truncated]`).

A panic inside game code during play-in-editor is the same file with the headline `pie:`
instead of `panic:`. Play stops. The editor keeps running. The flag is process-wide because
the schedule may run that system on a worker thread.

A native crash (a segfault or a driver fault) uses the game feedback monitor: on for Linux,
opt-in on Windows (`native_crashes_windows` stays false; `REGIA_NATIVE_CRASH=1` forces it on,
`REGIA_NO_NATIVE_CRASH=1` turns it off). That artifact is a `feedback-*.zip` whose `crash.txt`
starts with `native crash:`, not a `crash-*.txt`.

`regia crash list` and `regia crash show` read the text reports with the same layout as a game
panic report. A `panic:` headline is a panic. A `pie:` headline is accepted and labeled `pie`
(the reader's "other" kind, the same slot a `device_lost` headline already uses). A native zip
is a native crash.

Pass the editor log folder explicitly to the CLI: `regia crash list "<editor-log-folder>"`.
Read one file with `regia crash show "<report-path>"`. The CLI default folder belongs to its
own executable, so it may differ from the editor folder.

The next launch's recovery dialog says the editor closed unexpectedly. When a report is
pending it offers **View report**, **Copy to clipboard** and **Open folder**, beside Restore
and Discard when an autosave is pending too.

## `editor.crash_reports`

Read-only. No params.

```json
{ "method": "editor.crash_reports", "params": {} }
```

Returns `{ reports: [{ time, kind, first_line, path }] }`. `time` is the report's `unix_ms:`
line, or the file's modification time when that line is absent. `kind` is `panic`, `native`
or `pie`. A headline that is neither `pie` nor `native crash` (for example `device_lost`) is
listed as `panic`. `first_line` is the headline line, not the title banner. `path` is the
`crash-*.txt` or `feedback-*.zip`. Oldest first.

## `editor.crash_report_read`

Read-only. `path` (string, required) is a full path or a file name from `editor.crash_reports`.
A native bundle returns the text of its `crash.txt`.

```json
{ "method": "editor.crash_report_read", "params": { "path": "crash-2026-11-07_12-00-00.txt" } }
```

Returns `{ text }`. A read longer than 1 MiB is cut and ends with `[truncated]`.

| `code` | Meaning | What to do |
|---|---|---|
| `report_not_found` | The name is a report name and the file is missing or unreadable. | Call `editor.crash_reports` and pass one of those paths. |
| `invalid_report_path` | Empty, a `..` segment, a relative path with a separator, a name that is not `crash-*.txt` or `feedback-*.zip`, or a path outside the report directory. | Pass a path from `editor.crash_reports`. |
| `crash_reports_unavailable` | The editor has not configured a report directory. | Start the editor so it can install the hook. |

**Dismiss report** hides the offer for the current session and keeps the report on disk.
