# The pure-Rust ASTC encoder

`regia_asset_pipeline::astc_encode` is a dependency-free, safe-Rust ASTC block encoder for
LDR textures, written from the Khronos ASTC LDR specification (see `docs/provenance.md`). It
exists next to the Android cook's vendored `astcenc` path (`crate::astc`): use `astcenc` for
shipping cooks today; use this module where no FFI codec is available (on-device tooling,
tests, platforms without a prebuilt encoder) and to validate ASTC bytes without a GPU.

## What it encodes

| Aspect | Support |
|---|---|
| Block sizes | 4×4, 5×5, 6×6, 8×8 (2D), 16 bytes per block |
| Partitions | 1 per block |
| Weight grid | the block footprint (one weight per texel), single plane |
| Endpoint modes | luminance, luminance+alpha, RGB (alpha = 1), RGBA — all direct |
| Constant blocks | void-extent blocks (bit-exact) |
| Input | RGBA8, any size (edge blocks replicate the last row/column) |

Endpoint quantization is implicit in ASTC: the decoder derives the largest quantization
range whose Bounded Integer Sequence Encoding (BISE) fits the bits left after the weights.
The encoder therefore searches over *weight* ranges and endpoint modes; the endpoint range
follows. Legal weight configurations are narrower than the bit budget alone suggests:
their sequence encoding must spend between 24 and 96 bits and leave at least the format's
minimum endpoint budget, and the block-mode row that encodes square 6×6/8×8 grids has no
precision bit, so those sizes only offer the six low-precision weight ranges. At 8×8 the
only legal footprint-grid choice is 2 weight levels, which is the main quality limit at
that size (the encoder refits endpoints to the stored levels' centroids to soften it).
Multiple partitions, smaller weight grids, dual plane and the base+offset/base+scale
endpoint modes are the natural follow-ups; none are emitted yet.

## API

```rust,ignore
use regia_asset_pipeline::astc_encode::{
    encode_block, try_encode_block, encode_image, decode_block, decode_image,
    inspect_block, BlockSize, Quality,
};

// Whole image: RGBA8 in, row-major 16-byte blocks out.
let blocks = encode_image(w, h, &rgba, BlockSize::Block6x6, Quality::Balanced)?;
let back: Vec<u8> = decode_image(w, h, &blocks, BlockSize::Block6x6)?;

// One block (never fails: short input is padded by edge replication,
// long input truncated; use try_encode_block to reject a wrong texel count).
let block: [u8; 16] = encode_block(&texels, BlockSize::Block4x4, Quality::Fast);
let info = inspect_block(&block, BlockSize::Block4x4); // kind, mode, derived ranges
let texels: Vec<[u8; 4]> = decode_block(&block, BlockSize::Block4x4);
```

- `Quality::Fast` fits a heuristic endpoint mode at ≤ 2 weight ranges.
  `Balanced` (default) tries several modes and up to 5 weight ranges, each with an
  endpoint-level tweak pass. `Thorough` tries every mode and weight range and tweaks
  endpoint levels by one achievable value, keeping a tweak only when the true decoded
  error falls.
- Endpoint fitting starts from the texels' principal axis, alternates least-squares
  refits of endpoints and weights, and refits endpoints once more against the quantized
  weights; every candidate is scored by actually packing and decoding it, so best-of
  selection sees all quantization effects. Endpoints are stored in non-decreasing
  RGB-sum order, so decoders take the direct branch — blue contraction never applies
  to this encoder's blocks (the bundled decoder still implements it for foreign blocks).
- Encoding is deterministic and single-threaded: same input, same bytes.
- `decode_block`/`decode_image` are a reference decoder for the subset above. Blocks
  outside it (partitions, dual plane, other endpoint modes) decode as opaque magenta;
  `inspect_block` reports them as `BlockKind::Invalid`.
- `bise_encode`/`bise_decode`/`bise_bits` expose the underlying Bounded Integer Sequence
  Encoding over the 21 ranges in `QUANT_RANGES` (weights use `WEIGHT_RANGES`).

Fed into a cooked texture, the blocks are plain ASTC payloads: package them with
`write_ktx2_astc` exactly like an `astcenc` level. The test suite cross-checks the bytes
against the vendored `astcenc` reference decoder on every run: gradients, void-extent
blocks, and a sweep of hand-built blocks over every block size, endpoint mode, and legal
weight range, whose texels must agree between the two decoders to within one level.

## Measured quality and speed

Verification (`tests/astc_encode_verify.rs`): every block the encoder emitted over 1 512 varied
source blocks (all sizes and qualities) decoded bit-exact in `astcenc`, and 7 734 random
128-bit blocks inside the bundled decoder's subset decoded the same in both decoders.

`tests/astc_encode_quality_report.rs` (ignored; run with `--ignored --nocapture`) compares it
with `astcenc` on synthetic images and the four lookdev renders, both decoded by `astcenc`.
Lookdev renders (320x180, RGB PSNR) with the crate at opt-level 3, one thread:

| Block | this encoder Fast / Balanced / Thorough | astcenc fastest / medium / thorough |
|---|---|---|
| 4x4 | 42.7–47.6 / 43.2–50.4 / 43.3–50.7 dB | 49.5–51.5 / 51.7–53.4 / 52.9–54.6 dB |
| 6x6 | 38.0–41.7 / 38.5–42.3 / 38.5–42.3 dB | 40.9–43.8 / 42.8–45.3 / 43.6–46.6 dB |
| 8x8 | 32.0–36.0 / 32.0–36.0 / 32.0–36.1 dB | 36.0–39.3 / 37.9–40.7 / 38.5–41.7 dB |

Speed at 4x4: 0.10 / 0.017 / 0.005 Mtexel/s (Fast / Balanced / Thorough) against 3.1 / 1.3 /
0.44 Mtexel/s for `astcenc` fastest / medium / thorough. The gap comes from the scope (no
partitions, no dual plane, no reduced weight grids, so 8x8 has only two weight levels) and
from scoring every candidate by a full pack and decode. Use `astcenc` for cooking; this module
is a portable reference and a format checker.
