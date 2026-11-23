# HDR block edge tiles and masks: what the deeper validation holds

The lightmap's block-compressed texel encodings — BC6H on desktop, ASTC 4×4 HDR on phones
(`regia_asset::hdr_blocks`, ADR-2314, `.flmap` texel encodings 2 and 3) — must keep the correct
irradiance where a lightmap atlas is awkward: partial 4×4 tiles at a chart's right and bottom
edges, and texels the coverage marks unused (empty atlas space a bilinear tap can reach). This page
lists the properties the unit tests in `crates/regia_asset/src/hdr_blocks.rs` hold for both codecs
and how to run them through the agent protocol. Test-only lane: no runtime surface, format or
threshold changed.

## The held properties

All inputs are bounded and deterministic (fixed sizes, fixed values, no clocks, no randomness beyond
the encoders' own fixed iterations).

- **Edge tiles repeat the last row and column.** For 1×1, 1×5, 3×7, 4×4, 5×4 and 5×5 images, every
  tile's 16 encoded bytes equal the bytes of an explicitly constructed 4×4 tile whose out-of-range
  texels (and mask entries) come from `min(x, w−1)` / `min(y, h−1)` — the documented
  repeated-last-row/column padding. The check is exact (byte for byte), so it pins the padding
  policy, the row-major block order and the block count (`ceil(w/4) · ceil(h/4) · 16` bytes) at
  once, with and without a mask that has holes. Tests:
  `hdr_blocks::tests::edge_tiles_match_an_explicitly_padded_tile`.
- **Decoding keeps dimensions and row-major orientation.** The same sizes decode back to exactly
  `w·h` texels, on images whose border texels are analytically distinct: the red channel carries
  the position along one axis (a 15 % step, more than twice the documented error bound, so no flip,
  transpose or misindexed linearization survives) and each decoded channel stays within
  `BC6H_MAX_REL_ERROR` / `ASTC_HDR_MAX_REL_ERROR` of its input relative to the texel's brightest
  channel. The image is run with the position axis on x and on y.
  Test: `hdr_blocks::tests::decode_keeps_dimensions_and_orientation_on_distinct_borders`.
- **Masked-out texels are inert.** With a single valid texel, and with four valid corners, changing
  only the invalid texels over black, mid and the brightest finite value (65504) leaves the decoded
  valid texels bit for bit identical and within the documented error of their input. The endpoints,
  the index choice of a valid texel and the decoded value at a valid texel never depend on an
  invalid texel's value. Test: `hdr_blocks::tests::masked_out_texels_do_not_change_valid_texels`.
- **An empty mask is deterministic and encodes every texel.** A mask with no valid texel gives the
  same bytes on repeated runs and exactly the bytes of an absent mask: both codecs fall back to
  fitting all 16 texels. Test: `hdr_blocks::tests::an_empty_mask_is_deterministic_and_encodes_every_texel`.
- **NaN/Inf clamp policy is unchanged.** NaN, negatives and −infinity clamp to 0;
  +infinity and values at or above the largest finite half (65504) clamp to 0x7BFF
  (`f32_to_f16_bits` maps `v.is_nan() || v <= 0.0` to 0 and `v >= 65504` — which includes
  +infinity — to 0x7BFF); a block carrying them decodes to finite texels in 0..=65504.
  Test: `hdr_blocks::tests::non_finite_and_over_range_texels_stay_finite`.
- **Truncation, extra blocks and foreign blocks are errors, not panics**, over bounded fixed-size
  arrays: one block short or one block extra is `HdrBlockError::Size` naming the full byte count,
  and a 0xFF block (no mode either encoder writes) is `HdrBlockError::Block` at its own row-major
  index. Test: `hdr_blocks::tests::truncated_extra_and_foreign_blocks_report_size_and_index`.

The documented error constants `BC6H_MAX_REL_ERROR` (0.05) and `ASTC_HDR_MAX_REL_ERROR` (0.06) are
unchanged; no quality threshold was added or relaxed.

## Verifying through the agent protocol

No new agent method exists for this: the existing test-run surface
([jobs](jobs.md#running-a-test-target-agentrun_tests)) covers it. From a project with the engine
workspace (granted the `build` permission):

```sh
regia agent call agent.run_tests '{"target": "lib", "package": "regia_asset", "filter": "hdr_blocks"}'
```

answers a `tests:<n>` job; poll `agent.job_status {"job": "tests:<n>"}`. A green run answers
`result.ok: true` with `exit_code: 0`; a failing property answers `ok: false`, `exit_code: 101`,
the failing test's name and its `failures[].output`. A compile error answers `build_failed: true`
with the compiler lines in `errors`. The stable error codes are the existing job ones: `F0002`
(bad request, e.g. `run_tests_bad_name` for a `filter` outside the allowed alphabet,
`run_tests_busy` when a run is still going, `run_tests_no_manifest` without a `Cargo.toml`) and `F0003` (`job_not_found`). A spawn failure
is different: `agent.run_tests` still answers a `tests:<n>` job, and the job ends `failed` with the
reason `run_tests_spawn_failed` (poll `agent.job_status` to see it), like any failed command
([jobs](jobs.md)). This lane adds no
new code and no new error code.

Equivalent plain cargo command (what a developer or CI runs):

```sh
cargo test -p regia_asset --lib hdr_blocks
```
