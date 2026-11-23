# Spec 28: Log contract between Regia Engine and its host games

Status: **agreed, 2026-10-14** (Regia Engine draft, DO27 review with amendments (a)–(f) folded in below). Implementation: Regia Engine in a lane after v0.1.8 (target v0.1.9, before the 10-17 alpha),
DO27 in its own lane. Purpose: tester logs that both sides' tools (Regia Engine `xtask alpha-triage`, DO27's triage)
read without guessing, and that answer "what went wrong on this PC" in minutes.

Ground truth used for this draft: the first Windows tester logs (v0.0.4 on v0.1.6), the owner's 10-hour session,
and this week's diagnoses (tunnel-hole cost, cache thrash, shader waits, audio drop-outs, stalls, quit freezes).
Today's logs are compact (~0.4 MB per 20-minute session) and already carried every one of those diagnoses; this
contract fixes the friction, it does not redesign the logs.

## 1. Files (the host owns names and folders)
Unchanged split, one addition:
| File | Writer | Content |
|---|---|---|
| `<app>-<stamp>-<pid>.log` | host | human-readable lines |
| `<app>-<stamp>-<pid>.jsonl` | host's `tracing` sink (`regia_telemetry::json_layer`) | one JSON object per line: engine + game events |
| `console-*.log` | host | stdout/stderr |
| `match/<map>-N.jsonl` | host | per-match game records |
| `stall-*/`, crash bundles | host / engine | report, events, last lines, dump |
| **`summary.json` (new)** | engine writes the record, host chooses the path (default next to the `.jsonl`, same stem) | one record per session, § 4 |

## 2. Line format (shared)
- Every JSON line keeps `t_ms`, `wall` (UTC ISO-8601), `level`, `target`, `msg`, `fields`.
- **`fields.data` is a nested JSON object** when the field's text is JSON. `regia::frame_summary` and
  `regia::hitch` carry that object only. Every other target also writes the same text as the string
  `fields.data_json` (the v0.1.9 compatibility copy). A parser that reads only `fields.data_json` gets no
  record for those two targets; they still have nested `fields.data`. `data_json` is removed in a later
  breaking release. Parsers should accept both shapes.
- Engine targets stay stable: `regia::session`, `regia::boot`, `regia::frame_summary`, `regia::hitch`,
  `regia::gpu_error`, `regia::device_lost`, `regia::surface`, `regia::problems`, `regia::stream`, plus the
  new `regia::phase` (§ 3) and `regia::exit` (§ 5).
- Game targets stay under the game's prefix (`do27::…`).

## 3. Phase markers (new, shared target `regia::phase`)
One event per transition, `data`: `{phase, name?, ms?, …}`. Engine-emitted (it knows them) and host-emitted
(through `regia_telemetry::phase(name, data)`), same target so one tool reads both:
| Phase | Emitted by | Data |
|---|---|---|
| `boot_done` | engine | first presented frame: `ms` since process start |
| `shaders_ready` | engine | warm done: `pipelines`, `ms`, `ms_each`, `cache` (`hit`/`miss`), `threads` |
| `menu_ready` | **host** | |
| `scene_load_start` / `scene_load_end` | engine (scene warm, ADR-190) + host `name` (map) | end: `ms`, `mb_uploaded`, `pipelines`, `meshes` |
| `match_start` / `match_end` | **host** | `name` (map), end: `ms`, result fields at the host's choice |
| `quit_start` / `quit_done` | engine (clean exit, ADR-188) | done: per-step `ms` (§ 5) |

## 4. `summary.json` (new, written at clean exit; also on a caught panic when possible)
One object; every section optional when unknown. Engine fills all but `host`.
- **Hard locks never reach clean exit** (and those sessions matter most). The engine writes `summary.partial.json`
  every 60 s (the first one 5 s after the header; write to a temp file, then atomic rename) and the final `summary.json` at clean exit. On start, if the
  previous session's folder has a partial and no final, or a partial newer than the final, or a final whose `exit.clean` is false, the new session's summary gets
  `"previous_exit": {"clean": false, "last_partial": <that partial's contents>}` and the engine emits a WARN
  `regia::phase` event `previous_session_unclean`.
- `display.monitors[]` lists every monitor (native mode, refresh, scale, HDR) and marks the one in use.
- `frames_by_map`: the same percentiles per map (map names from the host's `scene_load_*` / `match_*` markers).
- `settings`: also `upscaler` (the one actually used), `auto_plan` (the first-launch choice, ADR-157). The host adds
  `data_tier` under `host`.
- `host` is a free-form object the host fills through `TelemetryHost`; recommended keys: `app`, `version`, `git`,
  `build`, `data_tier`, and `network` (`role`, `peers`, `rtt_p50_ms`, `rtt_p95_ms`, `loss_pct`, `desyncs`).
- `shaders`: also `backend` and `driver_cache` (`on`/`off`/`unknown`, when detectable).

Example:
```json
{
  "schema": 1,
  "regia": {"version": "0.1.9", "git": "…", "profile": "release"},
  "host": {"app": "do27", "version": "0.0.5", "git": "…", "build": "high"},
  "system": {"os": "Windows 11 25H2", "cpu": "AMD Ryzen 7 7800X3D", "cpu_threads": 16, "ram_gb": 32,
             "gpu": "NVIDIA GeForce RTX 4080", "vendor": "0x10de", "driver": "617.42", "backend": "vulkan", "vram_gb": 16},
  "display": {"width": 3440, "height": 1440, "refresh_hz": 144, "mode": "fullscreen", "present": "immediate", "hdr": true},
  "settings": {"preset": "high", "render_scale": 0.75, "aa": "taau", "gi": "ambient"},
  "session": {"seconds": 1240, "frames": 51234, "maps": ["bore_twelve", "switchbacks"]},
  "frames": {"p50_ms": 18.2, "p95_ms": 31.0, "p99_ms": 54.0, "over_33ms_pct": 6.1, "over_50ms_pct": 1.2, "bound": "cpu"},
  "hitches": {"count": 284, "suppressed": 12, "top_causes": [["terrain_upload", 41], ["emissive_lights", 22]]},
  "stalls": [{"t_s": 349.7, "ms": 3120, "phase": "scene_load"}],
  "memory": {"vram_peak_gb": 7.6, "rss_peak_gb": 17.0, "texture_budget_exceeded": true},
  "shaders": {"pipelines": 146, "ms_each": 85, "cache": "miss"},
  "audio": {"suspends": 0, "throttle_suspends": 0},
  "errors": {"gpu_errors": 0, "device_lost": 0, "surface_errors": 0, "panics": 0},
  "exit": {"clean": true, "ms": 255, "watchdog": false}
}
```
`xtask alpha-triage` reads `summary.json` first and the `.jsonl` for detail.

## 5. Engine changes (Regia Engine lane, v0.1.9)
1. `fields.data` nested (`data_json` on every target except `regia::frame_summary` and `regia::hitch`).
2. Session header gains `cpu` (model), `ram_gb`, `vram_gb` (device-local), `os_version`.
3. `regia::phase` events (§ 3) and `regia_telemetry::phase()` for hosts.
4. `summary.json` writer (§ 4), called from the clean exit (ADR-188) after the last frame summary; path from
   `TelemetryOptions` (default: same folder and stem as the JSONL sink).
5. The frame profiler (per-system and host spans) on by default in `DefaultPlugins`; a host that installs its own
   subscriber gets a one-line warning if the layer is missing. Default host-span prefix: the executable's file stem (lower-cased, non-alphanumerics become `_`) followed by `::`, plus `do27::`; a stem that starts with `regia` or a well-known dependency's name (`wgpu`, `tracing`, …) gives none (ADR-194, ADR-5630); a launcher whose binary name differs from the game crate calls `regia_core::profiling::set_host_span_prefixes` with the crate path.
6. `regia::exit` event with the clean-exit step timings (ADR-188).
7. Already in v0.1.8: stall and hitch durations are real (no longer clamped at 250 ms); shader summary line.
8. Privacy: user home paths in engine messages are written as `~` (e.g. `C:\Users\<name>\…` → `~\…`).
9. `alpha-triage`: reads `summary.json`, nested `data`, and `regia::phase`.

## 6. DO27 changes (accepted by DO27; lane LOG_LAYOUT_1014)
1. Stall-watchdog stack samples go **only** into the stall bundle (`stall-*/`), not the main `.log`/`.jsonl`
   (the owner's 10-hour session had thousands of "stack … repeated Nx" lines).
2. Emit `menu_ready`, `match_start`/`match_end` (with map `name`) and the map name for `scene_load_start` through
   `regia_telemetry::phase`.
3. Fill `summary.json`'s `host` section through `TelemetryHost` (app, version, git, build tier).
4. Include `summary.json` in the log-upload zip and name the zip with its stem.
5. Repeated warnings (e.g. the texture-budget warning) once per change, not every few seconds.
6. Keep `do27::header` and `regia::session` consistent; drop fields that duplicate the engine's.

DO27 also keeps one JSONL (its own targets `do27::net`, `audio`, `sim_audit`, `match` next to the engine's), makes the
human `.log` a concise events view, scrubs home paths in its own messages, and writes the `regia::phase` shape itself
behind one function until v0.1.9's `regia_telemetry::phase` exists.

## 7. Resolved questions
- `summary.json` location and name: accepted.
- Missing fields: added in § 4 (partials, monitors, per-map frames, upscaler/auto_plan/data_tier, network, shader backend).
- One release of `data_json`: enough, except `regia::frame_summary` and `regia::hitch` never write it.

## 8. Offline triage (C-13, ADR-8035)

`regia logs triage PATH... [--out NEW_DIR] [--json]` analyzes extracted upload folders,
individual JSONL files, ZIPs and complete `.partNNofMM.zip` sets. `logs.triage` supplies the same
analyzer with text bundles without filesystem access. See [the agent guide](../../book/agents/logs-triage.md)
for input shapes, settings, diagnostic interpretation and stable errors. No existing event or
summary schema changes. Triage reads frame timings from `data.frame_ms.p50` and `data.frame_ms.p99`,
then legacy `p50_ms` / `p99_ms`, then tracing fields recorded beside `data`. Folder-per-session
`summary.json` and `summary.partial.json` are summaries. Source byte ranges remain distinct from scrubbed output offsets.

## 9. Performance gate (C-14, ADR-8429)

`regia logs perf-gate --thresholds FILE LOG...` and `logs.perf_gate` judge play-phase session
summaries (`regia::summary`, `summary`, and telemetry `regia::frame_summary`) against a thresholds
TOML. Hidden and loading frames are excluded. A baseline summary can fail the gate on frame-time
regression. `--run` judges trace captures with a separate nearest-rank percentile. No existing
event or summary schema changes. `perf-gate.toml` is operator configuration, not a frozen content
format.
