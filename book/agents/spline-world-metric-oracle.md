# Spline world-metric regression recipe

The spline agent surface is unchanged. Existing `spline.get` and `spline.sample` coverage is listed in
[COVERAGE](../COVERAGE.md); this recipe records the CPU regression check for its world-space
behavior.

## Run the check

From the repository root, run:

```sh
cargo test -p regia_scene --test spline_world_metric
```

The suite compares a world-transformed cubic against an independent f64 Hermite evaluator and fixed-step Simpson
quadrature. It checks exact 10 m straight segments along differently scaled axes, total and per-segment lengths,
distance-fraction samples, tangent direction, orthonormal frames, reversed direction, closed seams, and a large
representable translation. Its test tolerance is 2e-4 m or 2 ppm for length (whichever is larger), and sample position
tolerance is 2 mm except for the large translated cubic, where f32 world-position precision permits 4 cm.

The integration uses 8,192 and 16,384 Simpson intervals to check convergence once; the 16,384-step result is the
reference. A transform-omission mutation should make the transformed cubic assertions fail. No timing threshold or
GPU is involved.
