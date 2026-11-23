# Environment sun extraction: longitude seam contract

`regia_pt::env_map::EnvMap::extract_sun` uses connected texels on an equirectangular map. Its
longitude neighbors wrap from the final column to column zero, so a small bright disc crossing
that boundary remains one extracted light.

## Regression recipe

The internal-only test `crates/regia_pt/tests/env_sun_longitude_seam.rs` builds 128 × 64 dark
maps with a compact analytic bright disc. It compares a disc centered across the seam with the
same texels shifted by exactly 32 columns. It checks component texel count and solid angle,
illuminance, angular radius, and direction after the corresponding longitude rotation. It repeats
the comparison for a disc in the first latitude row near the pole.

The test independently sums radiance with exact f64 row-band solid angles before extraction and
after extraction, adding the extracted sun's illuminance. The tolerance covers the disc's
cosine-weighted illuminance approximation and f32 map values; the exact column rotation itself
does not use a discretization tolerance. A broad bright belt must still produce the existing
“not a sun disc” refusal.

Run it with:

```text
cargo test -p regia_pt --test env_sun_longitude_seam
```

This contract is an internal library primitive. There is no dedicated agent method or lowercase
agent error code for component extraction; `seeing.reference_trace` does not expose the component
finder. The test therefore calls `EnvMap::extract_sun` directly and leaves the existing agent
surface unchanged.

## Sensitivity check

The test passed on the existing implementation. In a temporary remote source copy, replacing
wrapped longitude neighbors with clamped neighbors made the seam test fail: the extracted
component count differed (3 versus 9). The original neighbor search was restored, and the test
passed again.
