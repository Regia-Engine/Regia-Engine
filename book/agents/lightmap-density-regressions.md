# Lightmap density under transforms

`crates/regia_asset_pipeline/tests/lightmap_atlas_isometries.rs` checks the two existing UV2
builders against transformed versions of the same small meshes. It uses an asymmetric box with
face-split source vertices and a welded two-triangle plane. Integer-valued coordinates make the
tested quarter-turns, reflections, translations and uniform scales exact in `f32`.

The test measures world triangle area from the input positions and allocated chart-interior area
from the unwrapped triangles in texel coordinates. It checks the documented density rule
(`density × density_scale × fit_scale`), source-index mapping, finite UVs, chart gutters and
non-collapsed UV triangles. Pixel rounding is allowed by a one-pixel-per-triangle perimeter
bound. Chart order, UV orientation and atlas dimensions are intentionally not compared.

The anisotropic case stretches one axis and computes its expected area from every transformed
triangle. The uniform doubling case pairs doubled world size with half `density_scale`, so the
effective density and requested footprint return to the baseline within rounding tolerance.

Run the regressions with:

```sh
cargo test -p regia_asset_pipeline --test lightmap_atlas_isometries
cargo test -p regia_asset_pipeline --lib lightmap_uv
```

These tests add no runtime or agent method. Existing `agent.run_tests` can run the integration
target with `{"target":"lightmap_atlas_isometries","package":"regia_asset_pipeline"}` or the
library tests with `{"target":"lib","package":"regia_asset_pipeline","filter":"lightmap_uv"}`.
Both answer a `tests:<n>` job; inspect it with
`agent.job_status {"job":"tests:<n>","since":0}`. Stable refusals include `run_tests_bad_name`,
`run_tests_busy`, and `job_not_found`; see [Jobs and test runs](jobs.md).
