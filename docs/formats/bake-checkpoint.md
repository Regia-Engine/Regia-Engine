# `.bakeck` (format_version 1)

A bake checkpoint (`regia_asset::bake_checkpoint`, ADR-3095): what a long lightmap or probe bake
has finished, so a bake that dies (a crash, a driver reset, a killed job) continues from there.
`regia bake-lightmaps --checkpoint` and `regia bake-gi --checkpoint` write it beside their output as
`.<output file name>.bakeck` (a dot file), `--resume <file>` names one, and the bake removes it
when it finishes. The writer is `regia_pt::checkpoint`; the container is in `regia_asset` so the
editor and the agent can read its header without the path tracer.

The file is written to `<path>.tmp` and renamed over `<path>`, so a crash mid-write leaves the
previous checkpoint whole. It is transient: `regia upgrade-project` knows the suffix
(`bake-checkpoint`, v1) but never migrates it; a checkpoint of another version is refused
(`bake_checkpoint_version`) and the bake starts over.

All numbers are little-endian.

## Header (112 bytes)

| Offset | Type | Field |
|---|---|---|
| 0 | `[u8; 8]` | magic `RGBKCKPT` |
| 8 | `u32` | `format_version` (1) |
| 12 | `u32` | header length (112) |
| 16 | `u32` | baker: 1 `lightmap_cpu`, 2 `lightmap_gpu`, 3 `gi_cpu`, 4 `gi_gpu` |
| 20 | `u32` | stage (a probe bake: 1 once the relocation is done; a lightmap bake: 0) |
| 24 | `[u8; 32]` | input hash: SHA-256 of the bake's inputs, settings, seed and baker |
| 56 | `u64` | units of the stage (texels or probes of the main trace) |
| 64 | `u64` | units done (at most the units of the stage) |
| 72 | `u64` | batches done, over every run of the bake |
| 80 | `f64` | seconds of baking over every run (the rate of the estimate) |
| 88 | `u64` | payload length (at most 16 GiB, and exactly the rest of the file) |
| 96 | `u64` | FNV-1a 64 of the payload |
| 104 | `u64` | FNV-1a 64 of bytes 0..104 |

## Lightmap payload (`lightmap_cpu`, `lightmap_gpu`)

`rays: u64`, `count: u64` (the units done), then `count` texel estimates in the baker's task order
(always a prefix of the tasks), 36 bytes each: `e: [f32; 3]` (mean irradiance), `dir: [f32; 3]`
(luminance-weighted light direction), `rel_err: f32`, `back: u32` (first rays that hit a back face),
`taken: u32` (samples). Floats are stored as their bits, NaN included.

## Probe payload (`gi_cpu`, `gi_gpu`)

`probes: u64` (the bake's probes), then per probe `offset: [f32; 3]`, `flags: u32` (a `u16` of
`ProbeFlags`), `last_trace: { tag: u8, back_fraction: f32, rays: u32 }`; then
`relocation_traces: u64`, `relocation_seconds: f64`, `traced: u64` (the probes of the main
trace), `count: u64` (the units done) and `count` traces: `sh: [f32; 48]` (4 components × 3 colors
× 4 coefficients), `rays, back_faces, hits, rejected: u32`, `closest_back` and `closest_front` as
`{ tag: u8, dir: [f32; 3], dist: f32 }`, `moments: u32` (the layout's moment texels) and as many
`[f32; 2]`, `clamped: u32`, `clamp_loss: f32`, `suppressed_groups: u32`, `firefly_loss: f32`,
`noise: { tag: u8, value: f32 }`.

## Refusals

Every count is checked against the bake resuming it and against the bytes left before anything is
allocated. A file that does not read is `bake_checkpoint_damaged` (magic, header length, a header
or payload hash, a truncated payload, a count past the bake or the file, more units done than
total, trailing bytes); another version is `bake_checkpoint_version`; another bake's input hash or
baker is `bake_checkpoint_mismatch`. None of them panics (`fuzz_targets` target `bake-checkpoint`,
the payload decoders' mutation tests in `regia_pt`).

## Migration note

v1 is the first version. A later layout bumps `format_version`; old checkpoints are not migrated
(the bake starts over), so a bump needs no migrator, only `cargo xtask check-formats --update`.
