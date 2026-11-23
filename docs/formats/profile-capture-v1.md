# `.rprof.json`, profile capture, format version 1

A frame profiler capture (`regia_telemetry::profile_capture::ProfileCapture`, registry name
`profile-capture`): the CPU call tree averaged over a window of frames, the GPU pass times and the
folded stacks. A diagnostic artifact the engine never loads; written by `profiler.capture` and the
editor's Profiler panel. Design: [`docs/specs/83-prefab-profiler-volumes.md`](../specs/83-prefab-profiler-volumes.md)
§ 4.2 (ADR-2323).

```json
{
  "format_version": 1,
  "frames": 60,
  "frame_ms": 8.1,
  "cpu": [{ "name": "frame", "inclusive_ms": 7.9, "self_ms": 0.2, "calls": 1.0, "children": [] }],
  "truncated": 0,
  "gpu": [{ "pass": "shadow", "ms": 0.5 }],
  "folded": "frame 0.2\n"
}
```

| Field | Type | Notes |
|---|---|---|
| `format_version` | integer | Left out: 0, read as 1. Newer: refused. |
| `frames` | integer | Frames the CPU tree averages over. |
| `frame_ms` | number | Mean frame wall time over the same frames. |
| `cpu` | array of nodes | `name`, `inclusive_ms`, `self_ms`, `calls` (per-frame means), `children` (most inclusive first, ties by name). |
| `truncated` | integer | Nodes left out by the depth or node limit (default 0). |
| `gpu` | array | `pass`, `ms` of the last frame; empty without GPU timestamps. |
| `folded` | string | `a;b;c <self ms>` per line, for flame-graph tools. |

**Versioning**: the serde field lists are hashed by `cargo xtask check-formats` (entry `profile-capture`);
`profile_capture_migration_sources()` is `[0]`. `regia upgrade-project` re-writes a version-0 file at
version 1. Fuzz target: `profile-capture`.
