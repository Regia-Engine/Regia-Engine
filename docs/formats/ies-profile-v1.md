# FIES: IES photometric profile v1

Library: `regia_asset::ies` (`IesFile::{parse, to_text, candela}`, `CookedIesProfile::{from_file, to_bytes,
from_bytes, relative}`). Cook: `regia_asset_pipeline::ies_cook` (the `.ies` arm of `cook_project`). Decision:
ADR-2592. Source format: IESNA LM-63 (the public standard; the 1986, 1991, 1995, 2002 and 2019 header lines are
read, `TILT=NONE` and `TILT=INCLUDE`; `TILT=<file>` is an error).

## Source `.ies` (LM-63 text)

`IesFile::parse` reads the optional standard line (`IESNA:LM-63-2002`, `IESNA91`, ...), the `[KEYWORD] value`
lines, `TILT=`, then whitespace- or comma-separated numbers across any line breaks: lamp count, lumens per lamp
(−1 for absolute photometry), candela multiplier, vertical and horizontal angle counts (each at most 4096, at most
2²⁰ values together), photometric type (1 C, 2 B, 3 A), units (1 feet, 2 metres), the luminous opening (width,
length, height), ballast factor, ballast-lamp photometric factor, input watts, the vertical angles, the horizontal
angles, and one row of vertical values per horizontal angle. Angles must be strictly ascending; type C vertical
angles lie in 0..=180 and horizontal in 0..=360; types A and B in −90..=90. A negative or non-finite value,
trailing numbers and a truncated table are errors. Every value is scaled by multiplier × ballast factor ×
ballast-lamp factor (`IesFile::candela_scale`).

Type C horizontal symmetry (LM-63 § 5.15): one horizontal angle is rotational symmetry; a last angle of 90 is
quadrant symmetry; 180 is bilateral symmetry about the 0-180 plane; a range 90..270 is bilateral symmetry about
the 90-270 plane; 0..360 is none. `IesFile::candela_at_angles` unfolds them and interpolates linearly in both
angles; it returns 0 outside the file's vertical range.

The photometric frame: `-Z` is the nadir (vertical 0°), `+X` the horizontal 0°, `+Y` 90°. A spot light's
direction is the nadir; a point light's nadir is world `-Y` unless the entity's rotation turns it.

## Cooked payload (little endian)

| Bytes | Content |
|---|---|
| 4 | magic `FIES` |
| 4 | `format_version` (u32, 1) |
| 4 | vertical sample count (u32, 256): `θ_i = i · 180° / 255` |
| 4 | horizontal sample count (u32, 32): `φ_j = j · 360° / 32`, wrapping |
| 4 | peak candela (f32, > 0): what a relative 1.0 stands for |
| 4 | flux (f32, lm): ∫ I dΩ of the resampled profile at file intensity |
| 12 | luminous opening, metres (f32 × 3, absolute values) |
| 8 | vertical range of the file, degrees (f32 × 2) |
| 256 · 32 · 4 | relative intensities in `[0, 1]`, 32 rows (φ) of 256 values (θ) |

The cook wraps the payload in the normal cooked header under the type path `regia_asset::ies::CookedIesProfile`.
`from_bytes` rejects a bad magic, another version or grid size, a truncated payload, trailing bytes, a non-finite
header value, a negative peak, flux or opening, and a value outside `[0, 1]`. The lookup the renderer and the
path tracer use (`CookedIesProfile::relative_at_angles`) is bilinear on this grid.

Migration: none at v1 (`ies_profile_migration_sources()` is empty). The payload is derived data: a layout change
bumps `IES_PROFILE_FORMAT_VERSION`, and the importer re-cooks from the `.ies` source. It has no serde shape, so
`cargo xtask check-formats` fingerprints a description of the layout (`ies_profile_schema_fingerprint()`,
registered as `ies-profile`). `regia upgrade-project` reports the version of every cooked profile in the dev
cache (`.fckd` files whose type path is the profile's, read by `cooked_ies_profile_file_version`). The fuzz
targets `ies` (the text) and `ies-profile` (the payload) run under `agent.fuzz`.
