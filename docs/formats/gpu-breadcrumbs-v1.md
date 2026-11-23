# `gpu_breadcrumbs.json`, format version 1

The GPU breadcrumb report inside a crash bundle (`regia_render::breadcrumbs::BreadcrumbReport`), next to
`crash.txt`; also what `render.breadcrumbs` returns. Design: `docs/specs/54-gpu-breadcrumbs-and-trace-markers.md`
(ADR-1671). A diagnostic output, not project content: `regia upgrade-project` never rewrites it, crash bundles are
never rewritten, and the feedback bundle stays `BUNDLE_FORMAT = 1` (an added zip entry is additive).

At most 64 KiB; on overflow the oldest frames are dropped and `truncated` is set.

```json
{
  "format_version": 1,
  "level": "fenced",
  "adapter": { "name": "llvmpipe", "backend": "Vulkan", "device_type": "Cpu", "driver": "llvmpipe",
               "driver_info": "Mesa", "vendor": 65541, "device": 0 },
  "lost": { "reason": "unknown", "message": "driver text, verbatim", "frame": 812, "unix_ms": 1790000000000 },
  "counters": { "submitted": 813, "completed": 811, "acknowledged_frame": 811 },
  "suspects": [ { "name": "gbuffer", "frame": 812, "submission": 812 } ],
  "frames": [ { "frame": 812, "passes": ["shadow_cascades", "gbuffer"], "submissions": [[812, 0, 2]], "truncated": false } ],
  "truncated": false
}
```

- `level`: `timestamped`, `fenced`, `recorded` or `none` (best evidence first). A report never implies more.
- `suspects`: passes the GPU was most likely inside, in GPU order. `frames` is newest first;
  `submissions` rows are `[submission id, first pass index, end pass index]`.
- Readers ignore unknown keys. A layout change bumps `format_version` and the reader keeps a decoder for every
  older version.
