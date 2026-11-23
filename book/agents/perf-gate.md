# Performance gate

Use `regia logs perf-gate` on captured session logs. Use read-only `logs.perf_gate` (MCP tool
`perf_gate_logs`) when the caller already has the text. Neither surface opens a path from the
agent call, and neither changes a running game.

```sh
regia logs perf-gate --thresholds perf-gate.toml session.jsonl
regia logs perf-gate --thresholds perf-gate.toml --baseline previous.jsonl --json session.jsonl
regia logs perf-gate --run captures/ --baseline baseline.json --regression-pct 10
```

Exit 0 is a pass. Exit 1 is a failed gate. Exit 2 is bad input (missing thresholds, a thresholds
file that is not TOML, a corrupt baseline, or a missing run directory). With `--json`, stdout is
one JSON document and nothing else. Stderr on exit 2 starts with `perf_gate: error:`.

## What is judged

Summary mode reads JSONL, or one JSON document. A record counts when `target`, `record`, `type`,
`kind` or `msg` is `regia::summary` or `summary`, and `phase` is `play` or absent. Telemetry
`regia::frame_summary` documents are accepted when they carry `frames_by_map` or a `frames`
object with `p50_ms` or `p99_ms`. Sampled frames win over a telemetry summary for the same map.

Hidden frames (`hidden`, `visible: false`, or `outside_update_ms` at least 80 with no positive
render time) and `loading` frames are excluded. The table header counts them. A map with no
visible frames is dropped. If nothing usable remains, the gate fails.

Thresholds are TOML, one row per map under a resolution class and tier, with a `default` row.
A missing map row uses that default and warns. A missing class or tier warns and assumes `1080p`
and `HIGH`. Overrides: `--res`, `--tier`. The shipped file is `crates/regia_cli/perf-gate.toml`.
It has class and tier defaults only. Map rows belong in the operator's copy.

Checked when the key is present and the series has samples: `fps_min`, `frame_ms_p50_max`,
`frame_ms_p95_max`, `frame_ms_p99_max`, `update_ms_p50_max`, `render_ms_p50_max`,
`outside_update_ms_p50_max`, `gpu_ms_p50_max`, `hitches_per_minute_max`, `rss_peak_mb_max`,
`vram_peak_mb_max`. Summary percentiles are linear interpolation. A hitch is a visible frame at
or above `hitch_ms` (default 25). Hitch rate is count times 60000 divided by the sum of frame
times. Memory peaks in `_gb` fields are converted with a factor of 1000.

`--baseline` is an older summary of the same shape. Per map, frame p50 may rise by
`regression_pct` (default 10). Frame p99 may rise by twice that. A map missing from the baseline
warns and is not a regression. A non-positive baseline value is skipped.

## Trace captures

`--run DIR` reads `frames-<tier>-<W>x<H>.csv`, `feel-<tier>-<W>x<H>.csv` or `.jsonl`, and
`mem-<tier>-<W>x<H>.csv`. The tier is lowercase letters. Percentiles here are nearest-rank.
Budgets are fixed per tier and width class (1080, 1440, 4k), not taken from the thresholds file.
A row with no budget reports `budget` as `NO DATA`, which does not fail the gate. A missing feel
file reports `feel` as `NO DATA` and does not fail. `--update-baseline` rewrites `--baseline`
(version 1, `entries` keyed `tier@WxH`) and exits 0. The agent form returns `baseline_json` and
writes nothing.

## Agent request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "logs.perf_gate",
  "params": {
    "logs": ["{\"target\":\"regia::summary\",\"phase\":\"play\",\"map\":\"map_a\",\"res_class\":\"1080p\",\"tier\":\"HIGH\",\"frames\":[{\"frame_ms\":16.6}]}"],
    "thresholds": "[classes.\"1080p\".HIGH.default]\nfps_min = 60.0\nframe_ms_p99_max = 33.0\n",
    "regression_pct": 10,
    "hitch_ms": 25
  }
}
```

`logs` is an array of strings. `thresholds` is the TOML text. `baseline` is an optional summary
or trace baseline. `res` and `tier` override the class and tier. `run` is an object of capture
file name to text; when it is set, the call uses trace mode instead of `logs`.
`update_baseline` asks for `baseline_json` in the result. `regression_pct` and `hitch_ms` are
finite and non-negative.

A failing gate is still a successful result: `overall` is `PASS` or `FAIL`, and `failed` is the
boolean. The result also carries `mode`, `stats`, `results`, `regressions`, `warnings`, `info`,
`res_class`, `tier`, `rows`, `baseline_json`, `hidden_frames`, `loading_frames` and
`malformed_lines`. Permission: Read. Rate class: heavy. Limits: `logs` at most 64 items,
`thresholds` at most 1048576 bytes, `baseline` at most 8388608 bytes.

| Code | When |
|---|---|
| `logs_perf_gate_input` | Missing logs and run, a wrong type, or a non-finite `regression_pct` or `hitch_ms` |
| `logs_perf_gate_thresholds` | Summary mode without thresholds text, or thresholds that are not TOML |
| `logs_perf_gate_baseline` | Baseline text that is not valid JSON when a baseline was supplied |
