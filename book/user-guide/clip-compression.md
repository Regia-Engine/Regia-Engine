# Animation clip compression

`regia_animation::clip_compress` cooks an `AnimationClip`'s skeletal tracks into a smaller
quantised form under an object-space error bound, checked at every key time, the midpoints between
keys and a uniform grid (the report shows the measured maximum). It is a cook-time tool: the
runtime only pays for interpolation.

## API

```rust,wrap
use regia_animation::clip_compress::{compress, CompressOptions};
# use regia_animation::{AnimationClip, Skeleton};
# let clip = AnimationClip::empty("wave", 1.0);
# let skeleton = Skeleton::default();

let options = CompressOptions::new(0.01); // 1 cm maximum object-space error per bone
let cooked = compress(&clip, &skeleton, &options)?;

let mut pose = skeleton.bind_pose_pose();
cooked.sample(0.25, &mut pose);

let report = cooked.report();
assert!(report.max_error <= options.tolerance);
println!("{} -> {} bytes", report.original_bytes, report.compressed_bytes);
```

`compress` returns a `CompressError` for invalid input (NaN or infinite key times or values,
a degenerate quaternion, duplicate or out-of-range tracks, a non-positive tolerance) instead of
panicking. `CompressedClip::sample` is a pure function of the stored data and the sample time,
so decompression is bit-deterministic. Start from the bind pose: channels that reproduce the
bind pose within tolerance are dropped, and sampling leaves those joints untouched.

## How it works

Error is measured where it is seen. Each bone carries probe points — its origin plus six points
at its _shell distance_ (`shell_radius`, default 1 unit; per bone via `per_bone_shell`), standing
in for its farthest skinned vertices. A candidate approximation is charged for the displacement
of those probes in model space after forward kinematics, for the bone itself **and every
descendant**, so a small root rotation error that swings a distant hand counts as the large error
it is.

Bones are reduced root-first. Each channel gets an equal share of every descendant's tolerance
(`tolerance`, overridable per bone via `per_bone_tolerance`), so the whole-clip error stays
within tolerance by the triangle inequality; a final verification pass measures the real error
and restores full key sets on any chain that overruns. Per channel the compressor tries, in
order: dropping the track (bind value), collapsing it to a constant, then iterative
least-error key removal on the quantised keys — repeatedly removing the interior key whose
removal causes the least object-space error while the budget holds. End keys are never removed.

## Storage formats

| Channel | Format |
|---|---|
| Rotations | Smallest-three packing: 48 bits (2-bit dropped index + 3×15-bit) or 32 bits (2 + 3×10-bit), chosen per track by its packed error (`RotationFormatPreference`) |
| Translations, scales | Per-track range-normalised 16-bit keys (per-axis min/max stored once) |
| Constant tracks | One stored value |
| Bind-pose tracks | Dropped (no keys) |

Decompression lerps translations and scales and uses normalised lerp for rotations.

## The report

`CompressionReport` records `original_bytes` and `compressed_bytes` under a stated size model
(raw `f32` storage versus the quantised layout, both excluding serde framing), key counts
before and after, and `max_error_per_bone` — the measured maximum probe displacement per bone.
If an extremely small tolerance is below what quantisation alone can represent, the report says
so (`max_error` above tolerance) rather than failing.

Only skeletal TRS tracks are carried. Morph tracks, notifies, named curves and property tracks
are not part of this format; cook those through `regia_animation::compress`.
