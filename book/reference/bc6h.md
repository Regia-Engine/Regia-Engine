# BC6H HDR texture blocks

BC6H compresses HDR textures: 4×4 blocks of linear RGB half floats become 16 bytes (8 bits a
texel, the same footprint as BC7 but for `rgba16float`-range content). The encoder and
reference decoder live in `regia_asset_pipeline::bc6h_encode`, written from the public BC6H
format description (Khronos Data Format Specification, BPTC section). See `docs/provenance.md`
and ADR-2331.

The module is a library. The texture cook does not write BC6H yet, so it has no agent method
or CLI command.

## When to use it

- **Desktop HDR color**: emissive maps, lightmaps, environment probes, bloom inputs: content
  whose values exceed 1.0.
- **Not for** sRGB albedo (use the BC7 cook) or data maps with a fixed 0..1 range.
- `Bc6hFormat::Unsigned` (`BC6H_UF16`, wgpu `Bc6hRgbUfloat`) for non-negative color: it
  clamps negatives to zero and spends no bit on a sign. `Bc6hFormat::Signed` (`BC6H_SF16`,
  wgpu `Bc6hRgbFloat`) for content with negative values.

## API

```rust,ignore
use regia_asset_pipeline::bc6h_encode::{
    encode_block, encode_image, decode_image, Bc6hFormat, Bc6hOptions, Bc6hMetric, Bc6hModes,
};

// One 4×4 block (row-major, top-left first) -> 16 bytes. Never fails: NaN folds to 0,
// ±inf clamps to ±65504, unsigned clamps negatives to 0.
let block: [u8; 16] = encode_block(&texels, Bc6hFormat::Unsigned, &Bc6hOptions::default());

// A whole level: `rgb` holds width*height*3 floats; the last row/column of blocks
// repeat the image's last row/column of texels. Errors only on size mismatch.
let blocks: Vec<u8> = encode_image(w, h, &rgb, Bc6hFormat::Unsigned, &options)?;

// Back to floats (drops the padding texels).
let rgb: Vec<f32> = decode_image(w, h, &blocks, Bc6hFormat::Unsigned)?;
```

Blocks are rows of `ceil(width/4) × ceil(height/4)`, left to right, top to bottom: the layout
`ktx2_write::write_ktx2_bcn` expects for 16-byte BC formats, and what a GPU samples.

## Modes and options

The encoder searches the modes enabled in `Bc6hOptions::modes` (all 14 by default) and keeps
the block's best fit. The decoder reads all 14.

| Mode | Subsets | Endpoints | Indices |
|---|---|---|---|
| 1 | 2 | 10-bit base + 5-bit deltas | 3-bit |
| 2 | 2 | 7-bit base + 6-bit deltas | 3-bit |
| 3, 4, 5 | 2 | 11-bit base + 5/4/4, 4/5/4, 4/4/5-bit (R/G/B) deltas | 3-bit |
| 6 | 2 | 9-bit base + 5-bit deltas | 3-bit |
| 7, 8, 9 | 2 | 8-bit base + 6/5/5, 5/6/5, 5/5/6-bit deltas | 3-bit |
| 10 | 2 | 6-bit direct | 3-bit |
| 11 | 1 | 10-bit direct | 4-bit |
| 12, 13, 14 | 1 | 11, 12, 16-bit base + 9, 8, 4-bit delta | 4-bit |

Two-subset modes try the four partitions (of 32) that a quantization-free fit ranks best.

```rust,ignore
// One-subset modes only (fastest):
let fast = Bc6hOptions {
    modes: Bc6hModes::MODE_11
        .union(Bc6hModes::MODE_12)
        .union(Bc6hModes::MODE_13)
        .union(Bc6hModes::MODE_14),
    ..Bc6hOptions::default()
};
```

An empty mode set falls back to mode 11 rather than failing.

## Error metrics

`Bc6hMetric` decides both index choice and mode competition:

- `Linear` (default): plain squared error. Bright pixels dominate; fine for emissive maps
  whose brights matter most.
- `ToneMapped`: `x/(1+|x|)` per channel before squaring. Use when dark detail must survive
  next to a bright spike.
- `LogDomain`: `log2(1+|x|)` per channel (a deterministic bit-manipulation log): even
  *relative* error across the range. The best choice for HDR color.

Under `ToneMapped` and `LogDomain` the encoder also refines each candidate's endpoints once by
least squares for the chosen indices, keeping the result only when the error drops. `Linear`
skips this step.

All three are pure arithmetic, so the same input and options always produce the same bytes:
no libm calls, no clocks, no hash maps.

## Guarantees and limits

- **GPU-compatible**: `tests/bc6h_encode_verify.rs` decodes the module's blocks on a Vulkan
  driver and with an independent decoder, and both must match the module's decoder exactly.
- **Deterministic** byte for byte, per platform and per run.
- **No panics on bad input**: NaN folds to 0, infinities clamp, negatives clamp (unsigned) or
  encode directly (signed); wrong buffer sizes return `Bc6hError`, not a crash.
- **Endpoint deltas never wrap**: a delta that does not fit its field is clamped toward the
  base, and a mode whose deltas cannot span the block loses the mode competition.
- **Quality**: endpoints start as each subset's per-channel bounding box. On the verification
  images, `LogDomain` comes close to the ISPC BC6H encoder the crate already links, at slow
  settings (log-RMSE 0.0043 vs 0.0045 on a sky gradient, 0.019 vs 0.021 with bright spikes, 0.028 vs
  0.027 on a hue sweep, 0.151 vs 0.136 on per-channel noise). `Linear` is weaker on noisy and
  spiky content.
- **Speed**: scalar and single-threaded, about 0.02 to 0.1 Mtexel/s depending on the metric,
  several times slower than ISPC.
