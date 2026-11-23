# Video capture presets (agents)

Hosting sites re-encode what they are given, so a soft or blocky clip comes back worse. A capture preset
bundles the choices that keep it sharp (ADR-2449):

- **Render big, deliver exact.** The frames render at the delivery size times `supersample` (2, so
  3840x2160 renders 7680x4320) and average down. Edges and foliage are clean before any codec sees them.
- **Master first.** The render-out writes a ProRes 4444 movie (through `ffmpeg`, as `render.out_start`
  already does).
- **Deliver at a high bitrate.** The plan carries the `ffmpeg` arguments that turn the master into a
  delivery file: 85 Mbps H.264 for 4K 60 (55 for 4K 30, 40 for 1440p 60), HEVC at 65 percent and AV1 at 50
  percent of that, a rate cap at 1.25 times the target and a buffer at 2 times (not for AV1: SVT-AV1 allows a
  cap only in constant-quality mode, and takes at most 100 Mbps), BT.709 tags, limited range, fast-start.
  The H.264, HEVC and AV1 commands were run against ffmpeg 7.x on a test clip. The engine never runs them.
- **Deterministic camera.** The camera moves on a shot sequence (`render.sequence`, a `.shots.ron` file,
  see [camera paths](camera-paths.md)). Every pose is a pure function of the clock and the render-out
  steps the clock by exactly one frame, so the same file and request give the same frames.

| Method | Permission | What it does |
|---|---|---|
| `render.capture_plan` | read | No params: list the presets. With `preset`: the steps and sizes. Renders and writes nothing. |

Presets: `showcase_4k60`, `showcase_4k30`, `showcase_1440p60`. Params: `preset`, `name` (file stem,
default `showcase`), `codec` (`h264`, `hevc`, `av1`), `bitrate_mbps` (1..500), `duration_s` or `frames`
(default 10 s), `sequence` (the `.shots.ron` path), and any `render.out_start` key, which wins over the
preset's.

`render.out_start` also takes `preset`: the preset fills in `width`, `height`, `fps`, `supersample` and
`format` for every key the request left out. `codec` and `sequence` are capture-plan parameters only.

```json
{ "method": "render.capture_plan", "params": { "preset": "showcase_4k60", "name": "forest",
  "codec": "hevc", "duration_s": 20, "sequence": "shots/forest.shots.ron" } }
→ { "preset": "showcase_4k60", "width": 3840, "height": 2160, "fps": 60, "frames": 1200,
    "supersample": 2, "render_pixels": 33177600, "render_pixel_limit": 100000000,
    "master": "prores", "master_file": "forest.mov",
    "delivery": { "codec": "hevc", "bitrate_mbps": 55.25, "file": "forest_delivery.mp4",
                  "ffmpeg_args": ["-hide_banner", "-y", "-i", "forest.mov", "…"] },
    "steps": [{ "method": "render.sequence", "…": "…" },
               { "method": "render.out_start", "…": "…" }, { "run": "ffmpeg", "args": ["…"] }] }
```

The render pixel limit (100 million) applies as for any render-out, so the plan never promises a refused
job. `F0002` with `data.reason`: `capture_unknown_preset`, `capture_unknown_codec`, `capture_bad_name`.
