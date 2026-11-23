# The ETC2 / EAC block encoder

The `regia_asset_pipeline::etc2_encode` module is the full-mode ETC2 / EAC block encoder
for Android GPUs without ASTC (ADR-1379's follow-up). It is a companion to
`regia_asset_pipeline::etc2` (the ETC1-modes-only encoder the cook currently wraps), not a
replacement: nothing in the cook selects it yet.

## What it encodes

| Function | Output | Bytes a 4×4 block |
|---|---|---|
| `encode_block_rgb(&texels, &opts)` | ETC2 RGB8, all five modes searched | 8 |
| `encode_block_rgba(&texels, &opts)` | ETC2 RGBA8 (EAC alpha word, then the RGB block) | 16 |
| `encode_block_r11(&texels)` | EAC R11 unsigned (R channel) | 8 |
| `encode_image(w, h, rgba, format, &opts)` | any of the above, whole image, edge texels padded | — |

`texels` are the block's 16 RGBA pixels in row order (`y * 4 + x`); the RGB encoders ignore
alpha. `encode_image` takes any non-zero size (a partial trailing block repeats the last row and
column) and returns `Etc2EncodeError::BadBuffer` on a length mismatch — it never panics on
degenerate input.

The reference decoder lives in the same module — `decode_block_rgb`, `decode_block_rgba`,
`decode_block_r11`, `decode_image` — together with `classify_block`, which reports which of the
five RGB modes a block decodes through. The encoder decodes every block it returns and checks it
against the mode the search picked, falling back to the individual mode otherwise.

## The search

- **Individual / differential** (the ETC1 subset): both sub-block orientations, all eight
  intensity modifier tables, 4-bit (individual) and 5-bit (differential) base colours from each
  sub-block's mean ±1 in every channel. The differential mode's second base is clamped into the
  3-bit signed delta range of the first, so `base + delta` never leaves 0..=31.
- **T / H**: candidate colour pairs from the block mean, the four sub-block means, the
  per-channel extremes and the two centres of a 2-means split; every ordered pair and all
  eight distances (H takes only the distances its colour order allows, since the distance
  index's lowest bit is whether `C1 >= C2`). The modes' unused bits are set so the block
  decodes through the intended mode, so any 4-bit colour pair can be written.
- **Planar**: a per-channel least-squares plane, anchors quantized to the mode's 6/7-bit fields,
  then the ±1 neighbourhood of each channel's three anchors searched together.
- **EAC** (alpha, R11): all sixteen modifier tables, the multipliers around the one the block's
  value range implies (plus the fine-step multiplier 0 for R11), bases ±2 around the centring
  estimate.

The winner is the block with the least squared error; `EncodeOptions::perceptual` weights the
channels 2 : 4 : 1 instead of equally. Ties break toward the ETC1 modes. Output is deterministic
byte-for-byte: no hash maps, no clocks, no randomness.

## Verification

The ETC1-mode and EAC layouts, tables and decode rules are byte-identical to the ones in
`etc2_compress` (ADR-1379). The T, H and planar layouts follow the Khronos Data Format
Specification. The module was first written by an outside model without a compiler; its T, H and
planar modes did not match the specification, and were fixed and checked (ADR-2333) against an
independent decoder (`texture2ddecoder` 0.1.2, run from a scratch crate outside the repository,
not a dependency): bit-exact on 200 000 random block words of every mode and on every block the
encoder writes for four test images. Six of those words and their decoded texels are pinned in
the module's tests. No phone GPU has decoded the new modes yet; `ModeSet::ETC1` restricts
encoding to the two ETC1 modes when wanted.

## Quality and speed

Measured on the build server (release build, one thread), 64×64 test images, PSNR over RGB:

| Image | All modes | ETC1 modes only |
|---|---|---|
| Smooth gradient with a disc | 43.75 dB | 35.92 dB |
| Gradient ± 24 noise | 26.18 dB | 19.22 dB |
| Two to four colours a block | 21.68 dB | 15.32 dB |
| Uniform noise | 14.32 dB | 13.08 dB |

The search costs about 110–280 µs a block with every mode (100–190 µs ETC1 only), so a
1024 × 1024 texture takes roughly 7–18 s on one core: an offline cook encoder, not a runtime one.
