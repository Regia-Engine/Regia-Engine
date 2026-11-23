# Post-effect and LUT references (CPU)

Four CPU reference modules exist so that effect arithmetic can be tested before, and kept
honest after, a GPU pass is written (ADR-1482). None of them runs per frame, none has an
agent-protocol method yet, and none is a runtime path of the renderer. A later lane that ports an
effect to the GPU gets its agent surface then (`agents/` page update and a `COVERAGE.md` method
row); until then a host calls these as plain Rust functions.

## `.cube` color lookup tables (`regia_asset::lut_cube`)

`lut_cube::parse(text)` reads the Adobe Cube LUT Specification 1.0 text format (`TITLE`,
`DOMAIN_MIN`, `DOMAIN_MAX`, `LUT_3D_SIZE` 2..=256, `LUT_1D_SIZE` 2..=65536, rows of three floats,
3D rows red-fastest, `#` comments). Malformed input returns a `CubeLutError` carrying the line
number: unparsable or non-finite values, a wrong value count, a size outside the range (refused
before any table memory is allocated), a second size keyword, too many or too few rows. Nothing is
clamped on load, so HDR tables survive. `CubeLut::sample` maps straight RGB through the domain,
clamps to the grid, and blends trilinearly or tetrahedrally (3D) or per channel (1D).

## Post effects (`regia_render::post_ref`)

- `radial_blur`: zoom and spin blur about a center. Length is linear in the distance from the
  center and zero at it; edge modes, an optional matte, mix, and `smooth_zoom(strength)`, the
  engine's own one-control preset.
- `glow_longtail`: the delta over the existing bloom. At falloff above zero, a halo from five
  exponential kernels with doubling decay lengths (`kernel_weights`, `halo`), plus a
  channel-offset chromatic fringe on the finished halo (`apply_fringe`). `apply` runs the whole
  chain.
- `lens_flare`: lens prescription schema and validation, sequential and two-bounce ghost ray
  tracing, Fresnel reflectance, Cauchy dispersion, ghost ranking. The module holds a schema only;
  lens data is transcribed from public patents by the host or a later lane.

Every function is deterministic: the same inputs give the same bytes.
