# Lightmap bake settings: reject before loading or writing

`regia bake-lightmaps` validates bake, GPU, UV layout, seam budget and minimum-area settings before resolving a scene. A bad value cannot trigger scene loading, a GPU bake or output replacement. Invalid settings preserve the existing output and its directory entries.

The existing error codes remain stable:

| Code | `path` identifies |
|---|---|
| `lightmap_bake_settings` | Bake controls such as `samples`, `bounces`, `noise_target`, `denoise`, `seam_budget`, and GPU controls `gpu_tile_rays`, `gpu_memory_mib`, `gpu_yield_ms` |
| `lightmap_uv_settings` | `density`, `padding`, `max_atlas`, `chart_angle`, or `min_area` |

GPU endpoint rules are `gpu_tile_rays = 0` (automatic) or 4096 through 67108864, `gpu_yield_ms` from 0 through 1000, and finite nonnegative `gpu_memory_mib` (0 means automatic/device limits). The CLI always prints one JSON object; a rejected request has `ok: false`, the stable `code`, `path`, and `message`, and a nonzero process exit. There is no separate `--json` switch for this command.

Run the real executable contract with:

```sh
cargo test -p regia_cli --test book_cli_coverage bake_lightmaps_cli_rejects_before_scene_load_and_preserves_output
```

The broader regression set is:

```sh
cargo test -p regia_cli --bin regia bake_lightmaps
```

The existing `agent.run_tests` surface can invoke a test target from a project root; for example, its `target: "bins"`, `package: "regia_cli"`, and `filter: "bake_lightmaps"` runs the binary unit tests. A dry run of that agent method returns the exact Cargo command without executing it.
