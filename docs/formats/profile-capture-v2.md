# `.rprof.json`, profile capture, format version 2

A frame profiler capture (`regia_telemetry::profile_capture::ProfileCapture`, registry name
`profile-capture`): the CPU call tree averaged over a window of frames, the GPU pass times, the folded
stacks, and (new in v2) the per-thread timeline of the captured frames. A diagnostic artifact the
engine never loads; written by `profiler.capture` and the editor's Profiler panel. Design:
[`docs/specs/83-prefab-profiler-volumes.md`](../specs/83-prefab-profiler-volumes.md) § 4.2 (ADR-2323) and § 9
(ADR-7325). Version 1: [`profile-capture-v1.md`](profile-capture-v1.md).

```json
{
  "format_version": 2,
  "frames": 60,
  "frame_ms": 8.1,
  "cpu": [{ "name": "frame", "inclusive_ms": 7.9, "self_ms": 0.2, "calls": 1.0, "children": [] }],
  "truncated": 0,
  "gpu": [{ "pass": "shadow", "ms": 0.5 }],
  "folded": "frame 0.2\n",
  "timeline": [
    {
      "number": 41,
      "frame_ns": 8100000,
      "dropped": 0,
      "threads": [
        {
          "index": 0,
          "name": "main",
          "spans": [
            { "name": "frame", "start_ns": 0, "end_ns": 7900000, "depth": 0 },
            { "name": "update", "start_ns": 120000, "end_ns": 3100000, "depth": 1 }
          ]
        },
        {
          "index": 3,
          "name": "regia-compute (0)",
          "spans": [{ "name": "par_chunk", "start_ns": 1500000, "end_ns": 1720000, "depth": 0 }]
        }
      ]
    }
  ],
  "timeline_truncated": 0
}
```

| Field | Type | Notes |
|---|---|---|
| `format_version` | integer | 2. Left out (0) or 1: read as 2 with no timeline. Newer: refused. |
| `frames`, `frame_ms`, `cpu`, `truncated`, `gpu`, `folded` | | As in v1. |
| `timeline` | array of frames | Default empty (the profiler's timeline was off, or `profiler.capture { timeline: false }`). Oldest first; frames with no spans are left out. |
| `timeline[].number` | integer | The frame's number in the profiled process (frames ended before it). |
| `timeline[].frame_ns` | integer | Wall time of the frame, ns. |
| `timeline[].dropped` | integer | Spans past the profiler's per-frame cap (8,192), not recorded. Default 0. |
| `timeline[].threads` | array | By the profiler's thread index: `index`, `name` (`thread <index>` for an unnamed thread), `spans`. |
| `spans[]` | object | `name`, `start_ns` and `end_ns` (ns from the frame's start; a span entered in an earlier frame starts at 0), `depth` (recorded spans of the same thread open around it). Sorted by start, then depth, then end. |
| `timeline_truncated` | integer | Spans left out by the capture's limit (200,000 over all frames; a frame is cut at a thread boundary). Default 0. |

Times are integers (ns), so a capture serializes byte-identically when read and written again.

**Versioning**: the serde field lists (`ProfileCapture`, `CaptureNode`, `GpuPassTime`, `CaptureTimelineFrame`,
`CaptureThread`, `CaptureSpan`) are hashed by `cargo xtask check-formats` (entry `profile-capture`);
`profile_capture_migration_sources()` is `[0, 1]`. Both older shapes are the current one with defaults, so the reader
step is `ProfileCapture::from_json`. Captures are written under the project (`captures/`), so `regia upgrade-project`
lists them: it re-writes a version-0 or version-1 file at version 2 with an empty timeline. Fuzz target:
`profile-capture` (its fixture carries a one-frame timeline).
