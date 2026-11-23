# BC7 block encoder

`regia_asset_pipeline::bc7_encode` is a self-contained BC7 (BPTC unorm) encoder in
plain safe Rust — no FFI, no dependencies beyond the pipeline crate's own. The texture
cook still compresses through `intel_tex_2`; this module is the portable reference
encoder: deterministic bytes for a given input, the same output on every platform, and
quality knobs a caller can reason about. Its output round-trips through the runtime
decoder in `regia_asset::bcn_decode` (and, of course, through GPU BC7 sampling).

## API

```rust,ignore
use regia_asset_pipeline::bc7_encode::{
    encode_block, encode_image, try_encode_image, EncodeOptions,
};

// One 4x4 block of row-major RGBA texels -> 16 bytes.
let block: [u8; 16] = encode_block(&texels, &EncodeOptions::normal());

// A whole tightly packed RGBA8 image -> ceil(w/4) * ceil(h/4) blocks.
// Edge blocks are padded by replicating the last row and column.
let blocks: Vec<u8> = encode_image(&rgba, width, height, &EncodeOptions::high());

// Fallible form: zero dimensions or a short buffer return Bc7EncodeError
// instead of encode_image's empty-vec fallback.
let blocks = try_encode_image(&rgba, width, height, &EncodeOptions::fast())?;
```

`EncodeOptions` carries two fields:

- `quality: Bc7Quality` — `Fast` searches BC7 mode 6 only. `Normal` (the default) also
  searches the two-subset modes 1, 3 and 7 over the few partitions with the lowest
  within-subset variance, plus the rotated dual-index modes 5 and 4. `High` fits all 64
  two-subset partitions and runs one more refinement pass. Because every quality runs
  the same first pass and later passes only keep improvements, the full search is never
  worse than mode 6 alone on any block (a unit test pins this).
- `channel_weights: [f32; 4]` — per-channel weights for the squared-error metric used
  to fit and rank candidates. Uniform by default. A zero weight makes the encoder
  indifferent to that channel; negative or non-finite weights are sanitized to 1.0.

Two helpers round out the surface: `block_mode(&block) -> Option<u8>` reads back the
BC7 mode number a decoder would see, and `decode_block(&block) -> [[u8; 4]; 16]`
decodes through the `regia_asset` reference decoder for round-trip checks.

## How it encodes

Per block, each candidate mode (and partition, rotation and index-selection where the
mode has them) is fitted independently and the winner is the lowest weighted squared
error measured on texels decoded with the exact integer arithmetic a GPU uses.

Endpoint fitting starts from the subset's principal axis: the dominant eigenvector of
the channel-weighted covariance, found by power iteration, with endpoints at the
extreme projections. Then weights and endpoints alternate — project each texel onto
the segment to pick its interpolation weight, re-solve both endpoints by least squares
with the weights held fixed — first in float, then again after the endpoints are
quantized to the mode's stored precision. P-bits are chosen by endpoint error (per
endpoint, or the shared bit in mode 1). Endpoint order is flipped per subset when that
is what lets the subset's anchor texel — whose stored index is one bit short — reach
its end of the segment. Modes 4 and 5 fit colour and alpha as separate lines with
independent index sets, in each of the four channel rotations.

Modes 0 and 2 (three subsets) are not emitted. Everything is a pure function of the
input: fixed iteration counts and tie-breaks, no clocks, no randomness, no hash-map
ordering. The algorithm sources are recorded in `docs/provenance.md` (Khronos Data
Format Specification 1.3 for the format; Pearson 1901 and Castaño 2007 for the
principal-axis fit; Lloyd 1982 for the refinement iteration).

## Quality expectations

The module's tests assert PSNR floors against the reference decoder on synthetic
content at `High`: diagonal gradients above 45 dB, alpha ramps above 45 dB, smooth
(band-limited) noise above 30 dB, and solid colours exact within one level per channel
(an endpoint's single P-bit forces one parity on all four channels, so one level is
the representable worst case). I.i.d. white noise is information-theoretically capped
in the mid-teens dB at BC7's 8 bits per texel; the test floor there is a sanity
bound, not a quality claim.

Measured against the cook's existing ISPC encoder (`intel_tex_2`) on four 320x180 lookdev
renders (RGB PSNR, same reference decoder): `Fast` matches its ultra-fast setting
(42.8-49.3 dB), `Normal` matches its fast setting (51.5-52.9 dB) and `High` sits between its
basic and slow settings (52.1-53.8 dB). The encoder is scalar and single-threaded: about
2.9 / 0.06 / 0.008 Mtexel/s for Fast / Normal / High with the crate optimized, 10-30x slower
than the ISPC encoder at equal quality. `crates/regia_asset_pipeline/tests/bc7_encode_quality_report.rs`
(ignored by default) prints the full table.
