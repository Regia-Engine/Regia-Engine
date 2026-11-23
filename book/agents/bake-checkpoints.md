# Resumable bakes (checkpoints)

A lightmap or probe bake can run for hours. With `--checkpoint`, `regia bake-lightmaps` and `regia bake-gi` write what
they have finished to a checkpoint file beside the output every few batches, and the same command run again after a
crash, a driver reset or a killed job continues from it instead of from zero. The result is the same file, byte for
byte, as a bake that never stopped. Design: ADR-3095. Format: `docs/formats/bake-checkpoint.md` (`.bakeck`, v1).

## Running a checkpointed bake

```text
regia bake-lightmaps --scene assets/levels/yard.level.ron --checkpoint
regia bake-gi --from-scene assets/levels/yard.level.ron --bake --checkpoint
```

- The checkpoint is `.<output file name>.bakeck` beside the output (`assets/maps/.yard.flmap.bakeck`), a dot file so
  content scans pass it by. It is written to a temporary name and renamed, so a crash mid-write keeps the last one.
- When that file is there, the bake prints `found a checkpoint at … (N/M texels done): continuing it` and continues.
- `--resume <file>` names a checkpoint elsewhere (and keeps checkpointing to it).
- The bake removes its checkpoint once its output is written (a failed final write keeps it, and the next run
  resumes); the summary's `checkpoint` object says what happened:
  `{path, resumed, resumed_units, resumed_batches, batches, written, largest_bytes, work_seconds, removed}`.
  `largest_bytes` is the disk the checkpoint holds (twice that for a moment during the rename).
- Without `--checkpoint` nothing changes: no file, the same bake as before.

| Option | Default | Meaning |
|---|---|---|
| `--checkpoint` | off | Checkpoint beside the output and continue a checkpoint found there. |
| `--resume <file>` | | Continue from this checkpoint. |
| `--checkpoint-every N` | 1 | Write after every N batches ... |
| `--checkpoint-seconds S` | 60 | ... and at most every S seconds of baking (0: every N batches). |
| `--checkpoint-batch UNITS` | 0 | Texels (lightmaps) or probes (the probe bake's main trace) per batch; 0 = 65536 texels or 4096 probes. |

What a batch is: a lightmap bake estimates its texels in batches (on the GPU, a batch is a memory chunk); a probe bake
writes one checkpoint when its relocation rounds are done (batch 1) and then batches the main trace's probes. A crash
during the relocation reruns the relocation (it is a few cheap traces).

## Which bake a checkpoint belongs to

A checkpoint holds a hash of the bake's inputs: the scene, the layout, the receivers or probes, every setting that
changes the light and the seed, and the baker (`lightmap_cpu`, `lightmap_gpu`, `gi_cpu`, `gi_gpu`). A bake with other
inputs refuses it with `bake_checkpoint_mismatch` and leaves it alone: delete it (or use the editor's **Discard**) to
bake from the start. Settings that never change the result are not in the hash, so a resumed bake may use other
`--threads`, `--gpu-tile-rays`, `--gpu-memory-mib` or checkpoint options. A GPU bake that falls back to the CPU
(`--gpu-fallback`) continues only a CPU checkpoint and otherwise starts over.

## Why the result is identical

Every work unit keys its random streams by its own index (a texel's entry and place, a probe's index among the traced
probes), never by its batch, chunk or thread, and the checkpoint stores the finished units' sums bit for bit. A
resumed bake therefore traces exactly the units the uninterrupted bake would still have traced, with the same streams.
Tests stop a bake with a hook at batch K (not a kill, not a sleep) and compare bytes: the CPU lightmap baker, both probe
tracers (the CPU one and the GPU path tracer on lavapipe), and the GPU lightmap baker on lavapipe. On a GPU the claim is
for the same device and driver, as for any two GPU bakes.

## Progress and the estimate

A checkpointed bake's progress lines say what is left and when it ends:

```text
  lightmap: 48231/131072 texels (82841 left, eta 412 s)
  bake: 120/480 probes (360 left, eta 35 s)
```

The rate is the units done over the seconds of baking of every run, both stored in the checkpoint, so after a resume
the estimate continues from where the earlier runs left it (it does not count the resumed units as done in no time,
and it does not count the time the bake was stopped). The editor's bake jobs read these lines: `editor.bake_status` and
`editor.lightmap_status` report `progress.remaining` and `progress.eta_seconds`.

## Seeing a checkpoint from the agent and the editor

- `level.export_bake` returns `checkpoint` for the level's volume: `{exists, resumable, path, kind, unit, stage,
  units_total, units_done, remaining, fraction, batches_done, work_seconds, eta_seconds}`, or `{exists: true,
  resumable: false, code, error}` for a file that does not read; `null` when the level names no volume.
- `editor.bake_status` and `editor.lightmap_status` return the same `checkpoint` object for the last job's output.
- The Lightmaps panel shows one line under the bake state when the bake it would start has a checkpoint
  (`resumable checkpoint: 40 % (60 texels left); the next bake continues it`) with a **Discard** button; a bake started
  from the panel passes `--checkpoint`.

## Error codes

| Code | When |
|---|---|
| `bake_checkpoint_mismatch` | The checkpoint was written by another bake (inputs, settings, seed or baker differ). |
| `bake_checkpoint_damaged` | Truncated, edited or torn: a hash, length or count does not match. Never a panic. |
| `bake_checkpoint_version` | Another `.bakeck` format version; delete it and bake again. |
| `bake_checkpoint_io` | It cannot be read (a missing `--resume` file) or written (a full disk). |
| `bake_checkpoint_settings` | `--checkpoint-every 0` or `--checkpoint-seconds` outside 0..=86400 (`path` names the option). |
| `bake_interrupted` | The test hook stopped the bake after batch K (`--checkpoint-stop-after K`, hidden; tests only). |

`regia bake-lightmaps` returns them as its usual `{ok: false, code, path, error}`; `regia bake-gi` prints the same object
before it fails.

## Library

`regia_pt::lightmap::bake_lightmap_checkpointed`, `regia_pt::lightmap::bake_lightmap_gpu_checkpointed` (feature
`lightmap-gpu`) and `regia_pt::probe::bake_gi_volume_checkpointed` take a `regia_pt::checkpoint::BakeCheckpointPlan`
(`path`, `every_batches`, `min_interval`, `batch_units`, `resume`, `stop_after_batches`) and return
`BakeCheckpointStats` beside the bake. `regia_asset::bake_checkpoint` reads and writes the container and its header
(`read_header`, `checkpoint_status`, `BakeCheckpointStatus::line`).
