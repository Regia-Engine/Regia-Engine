# RGBE scanline and token boundaries

The existing `regia_pt::hdr` reader supplies HDR sky pixels to offline baking and reference work.
It reads flat, old-RLE and adaptive-RLE RGBE, returns linear RGB with the top row first, and accepts
`-Y h +X w` and `+Y h +X w`. `EXPOSURE` metadata remains unapplied; use the bake's explicit sky scale.
This page adds a deterministic test recipe for that existing reader, not a new agent method or format.
HDR texture import/cook is a separate surface.

## Status

The sprint-rgbe-run-boundaries lane adds structural coverage of the existing reader. No decoder
fix is claimed. The high-order old-run digit cases exercise the last in-range byte shift and the
following checked shift limit; an 8-bit count shifted by 24/56 fits on a 32/64-bit host.
The focused `cargo test -p regia_pt --lib hdr` run passed all 18 matching tests, including the
16 reader tests. Changing only the adaptive split from `> 128` to `>= 128` made
`adaptive_literal_and_repeat_counts_straddle_128_without_crossing_rows` fail; the deliberate defect
was restored before further verification. Clippy, strict rustdoc, Android and workspace formatting
also passed. All 299 library tests passed in the full crate run, but that run timed out during later
integration tests after the shared-slot wait. The lane remains **PARTIAL**: the full crate run,
actual agent dispatch and recorded-base preflight still need completion. The lane report has exact
commands and evidence paths.

## Deterministic reader checks

From the repository root on the permitted build host, run:

```sh
cargo test -p regia_pt --lib hdr
cargo test -p regia_pt
```

Use the build-host resource wrapper required by your lane. All fixtures live in memory; the suite
needs no files, image goldens, random seeds, device, timing budget or synchronization sleeps.
The fixture generator deliberately does not use `encode_flat`; the expected pixels are independent
fractions with known exponents. Fixed loops cover:

- Equivalent flat, old-RLE, adaptive-literal and adaptive-repeat encodings, in both supported row
  orientations, at widths 1, 2, 7, 8, 9, 127, 128, 129, 255, 256, 257, 32767 and 32768. Adaptive
  encoding applies only to widths 8 through 32767.
- Literal counts 1, 2, 3, 126, 127, 128 and repeat counts 1, 2, 3, 126, 127; exact row endings,
  byte cursors and subsequent rows; one-pixel overruns, zero adaptive tokens and missing repeat
  values in all four components.
- Adaptive width mismatches and flat dispatch when the marker or its high bit does not match.
- Old repeat counts with zero low digits and chained high digits, a repeat without a first pixel,
  row overruns, resets after a literal and between scanlines, and machine-word shift limits.
- Every truncated prefix of fixtures bounded to widths 8, 129 and 257, in all four encodings and
  both orientations; unsupported headers; CRLF; non-unit cumulative EXPOSURE metadata; bounded
  hostile resolutions without reserving the declared frame.

`HdrError::Format` carries a nonempty reason; tests check relevant reason substrings rather than
requiring every human-readable message to stay identical. `HdrError::Io` remains the file-read error.
These Rust errors are not agent protocol error codes.

The allocation guard is pre-existing: commit `8ba769d42f0fc3bf8e8de3b56b1892033667c128`
changed the reader to decode rows before growing the frame. This lane claims regression coverage,
not that earlier fix.

## Existing agent test surface

With a running trusted project host rooted at this workspace and the existing `build` permission
granted, the supported invocation is:

```sh
regia agent call agent.run_tests '{"package":"regia_pt","target":"lib","filter":"hdr","dry_run":true}' --project .
regia agent call agent.run_tests '{"package":"regia_pt","target":"lib","filter":"hdr"}' --project .
regia agent call agent.job_status '{"job":"tests:1","since":0}' --project .
```

Replace `tests:1` with the returned job id, and use each answer's `next` for the next `since`.
The dry run reports the existing cargo command without starting it. On completion assert
`finished == true`, `state == "done"`, `result.ok == true`, `result.failed == 0`, and
`result.build_failed == false`; inspect the named test events and failures. Do not grant permissions
or bypass shared-host resource limits just to run this recipe. A host can supply the existing
`RunTestsProgram` resource with its permitted cargo wrapper.

The existing `play.expect` validates reflected scene conditions, not arbitrary job JSON or HDR byte
streams. Reader assertions execute through the Rust tests above; no HDR-specific condition or method
is introduced. These CLI calls were inspected in the existing implementation and guide; actual
protocol verification is pending in the lane report. See [Jobs and test runs](jobs.md) for the
protocol contract.

Stable lowercase reasons from that existing surface are `run_tests_bad_name`, `run_tests_busy`,
`run_tests_no_manifest`, `run_tests_spawn_failed`, `job_id_invalid`, `job_not_found` and
`job_not_cancellable`. Invalid parameters use `F0002`; an unknown job uses `F0003`. A failed Rust
assertion is reported in the job result and does not introduce a new protocol reason.

## Public format reference

Fixtures are independently assembled from Greg Ward Larson's
[Radiance File Formats](https://floyd.lbl.gov/radiance/refer/filefmts.pdf), picture headers and
scanline records (printed pages 29–33 and 39), the format notes already cited by `hdr.rs` alongside
Ward's “Real Pixels,” *Graphics Gems II* (1991). No reference images or decoder implementation
are copied.
