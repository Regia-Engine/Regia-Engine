# Feedback bundles (`feedback.capture`, `feedback.list`)

A feedback bundle is one `.zip` in the game's feedback folder (`user://feedback`, for example
`~/.local/share/RegiaEngine/<game>/feedback/` on Linux). On Windows it is the non-roaming
`%LOCALAPPDATA%\RegiaEngine\<game>\feedback\`, so crash minidumps stay on the machine (ADR-525). The player writes one from the in-game prompt. The panic hook writes one
automatically, without a screenshot. An agent writes one with `feedback.capture`. All three
have the same parts, so a bug a player reports can be read the same way as one an agent
captures. The engine uploads nothing (see [Feedback and bug reports](../user-guide/feedback.md)).

## Capture a bundle

```json
{ "jsonrpc": "2.0", "id": 1, "method": "feedback.capture",
  "params": { "category": "visual", "note": "hull flickers at the gate", "screenshot": true } }
```

`category` is `bug`, `performance`, `visual` or `other` (default `other`). With `screenshot`
true (the default) the bundle holds the next presented frame. `timeout_ms` (default 30000) is the
answer deadline; the connection waits that long plus a 5 s grace. The reply arrives once the file is
written, which is about one frame plus the PNG encode on a worker thread:

```json
{ "name": "feedback-2026-10-11_14-02-31-visual.zip",
  "path": "/home/p/.local/share/RegiaEngine/do27/feedback/feedback-2026-10-11_14-02-31-visual.zip",
  "bytes": 2481152, "category": "visual",
  "entries": ["feedback.json", "note.txt", "screenshot.png", "frame_times.csv", "hitch_report.txt",
              "system.txt", "settings.ron", "render_problems.txt", "log_tail.txt", "build_info.json",
              "host.json"] }
```

A crash bundle (`-crash.zip`) has no `screenshot.png`. It adds `crash.txt`, which holds the
panic message, location, thread and backtrace.

| Entry | What |
|---|---|
| `feedback.json` | category, note, UTC time, `map` / `position` (from the host), which parts are present, why the screenshot is missing, hitch verdict, build summary |
| `screenshot.png` | the frame when the prompt opened |
| `frame_times.csv` | the last 30 s: `t_s, frame_ms, cpu_ms, gpu_ms` and per-frame pipelines, bind groups, buffers, textures, blocking polls, inline bakes |
| `hitch_report.txt` | the `specs/27` § 2.4 report over those frames (the same code as `play.hitch_report`) |
| `system.txt` | OS and version, device, CPU model, logical CPUs, RAM, GPU + backend + driver |
| `settings.ron` | the active `GameSettings`, graphics preset included |
| `render_problems.txt` | `render.problems` at capture time |
| `log_tail.txt` | the newest 400 log records, with the home directory as `~` and secrets as `[redacted]` |
| `build_info.json` | version, git hash, profile, engine, plus the packaged `build_info.json` (glibc floor) under `package` |
| `host.json` | the game's fields (`FeedbackHost`): match id, map, player count, position |

## List and open bundles

```json
{ "jsonrpc": "2.0", "id": 2, "method": "feedback.list", "params": {} }
```

The reply is `{ folder, bundles: [{ name, path, bytes, modified_unix_ms, category }], count }`,
oldest first. Crash bundles are included. Pass `name` to open one. That returns each entry's
size and the parsed `feedback.json`:

```json
{ "jsonrpc": "2.0", "id": 3, "method": "feedback.list",
  "params": { "name": "feedback-2026-10-11_14-02-31-visual.zip" } }
```

To read the screenshot or the CSV, unzip the file at `path`. Any zip tool works, because the
entries are stored without compression.

## Checking a fix

1. Reproduce the issue, then call `feedback.capture` with `screenshot: true`.
2. Open the bundle. Read `render_problems.txt`, look at `hitch_report.txt` and the spikes in
   `frame_times.csv`, and check `settings.ron` for the preset.
3. After the fix, capture again and compare the two bundles.

Errors: without `FeedbackPlugin` both methods return `F0003` (not found). An unknown `category`
or a `name` that is not a bundle file name returns invalid params.


## Saved pipeline cache

Normal captures may include a validated `pipelines/*.fpc` from the last renderer
checkpoint. `feedback.json.pipeline_cache` names it; `pipeline_cache_missing`
explains a failed attachment. Hosts control the byte budget and can opt out.
See [Pipeline caches in feedback and packages](pipeline-cache-harvest.md) for
capture checks, `--pipeline-cache-reference`, limits and driver compatibility.
