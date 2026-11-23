# KTX2 level drop (`regia cook drop-levels`)

Status: proposed (ADR-271). No code yet. Related: `specs/12-assets.md` §§ 4, 7.1 (texture cooking),
`formats/cooked-pack-guide.md`, `formats/format-hashes.ron`, `specs/23-agent-native.md`.

Conformance audit 2026-11-10 (ADR-5663, on `lane/sprint-integ-v047`): still no code. No `drop-levels` subcommand, no
`KDROP_*` code, no `RegiaLevelDrop` key, no `assets.drop_levels` method, no fuzz target, no guide page. The plan lists it
as blocked on the owner's answers to § 11 Q1 to Q4 (`docs/master-plan.md`; `docs/open-questions.md`). The building
blocks the spec names exist: the KTX2 writer `regia_asset_pipeline::ktx2_write` (`write_ktx2_bcn`, `write_ktx2_astc`,
`write_ktx2_etc2`; supercompression none), `transcode_bcn_ktx2_to_astc` (`texture_compress.rs`), the `astc_compress`
module, `regia cook` (`crates/regia_cli/src/commands/cook.rs`), the texture budget (ADR-130), `cargo xtask check-unwraps`
and the agent method `assets.texture_memory`.

## 1. Purpose

A cooked KTX2 texture carries a full mip chain, level 0 first. A target that never samples level 0
(a lower texture tier, a phone, a memory-capped CI box) still pays for it: level 0 is about 75 % of
the bytes of a full chain. DO27's cooked textures (about 20 GB, in `.bct` and `.ktx.postcard`
files) are the motivating case. The authored sources are not always at hand, and a recook
from them is slow (BC7 encode) and needs the recipe format Tincal has not frozen yet.

`drop-levels` removes the top N levels of an already-cooked KTX2 blob **without decoding or
re-encoding any texel data**. The surviving levels are copied byte for byte, so the result is exactly
what a cook with a smaller `max_size` would have kept for those levels. It is a lossless-in-the-rest, size-only transformation.

Non-goals: re-encoding to another format (that is `transcode_bcn_ktx2_to_astc`, `specs/12` § 7.1),
adding levels, regenerating mips, changing colour space, supercompression changes, and
recipe-driven regeneration (§ 11).

## 2. Inputs and outputs

Inputs, one of:

- a `.ktx2` file;
- a cooked-asset container whose payload holds a KTX2 blob (`specs/12` § 4: `CookedHeader` + postcard
  payload);
- a directory or pak, walked recursively, selecting the above by extension and header magic;
- DO27's `.bct` and `.ktx.postcard` wrappers, **only once their layout is documented** (§ 11, Q1).
  Until then the tool refuses them with `KDROP_UNKNOWN_WRAPPER` and never guesses.

Outputs:

- a rewritten file at the same relative path under `--out`, or in place with `--in-place`;
- a JSON report (`--report <path>`, and the agent result, § 8): per file, the old and new level
  counts, base extent, byte sizes, source and result blake3, and the outcome code.

Never overwrites without `--in-place`. `--in-place` writes a temp file in the same directory,
fsyncs, then renames atomically (the asset DB's rewrite rule, `specs/12` § 6).

## 3. CLI

A subcommand of the existing `regia cook` (`crates/regia_cli/src/commands/cook.rs`), since cook
already owns platform, quality and report handling:

```
regia cook drop-levels <INPUT>... [--out DIR | --in-place]
    (--drop N | --max-size PX | --budget-mib MIB)
    [--min-extent PX]          # floor, default 4 (see § 4)
    [--only <glob>]            # path filter
    [--dry-run]                # plan and report only, write nothing
    [--report PATH]            # JSON report
    [--jobs N]                 # default: physical cores, capped at 8
    [--check-format-hash]      # refuse inputs whose hash tag is not current (§ 6)
```

Exactly one selector:

- `--drop N`: drop exactly N levels from every input (clamped by the floor, § 4).
- `--max-size PX`: drop until the larger base side is at most PX (a power of two). The usual form: it
  expresses a tier ("nothing above 2048").
- `--budget-mib MIB`: choose the smallest uniform drop count that brings the total of all inputs
  under the budget. Reported, deterministic (§ 6), never per-file heuristics.

Exit codes: 0 all files ok or skipped by rule; 1 any file failed; 2 usage error. `--dry-run` exits
0 even when a file would fail, and lists the failures.

## 4. Level drop rules

Let the KTX2 header give `levelCount` L, `pixelWidth` W, `pixelHeight` H, `vkFormat` F, block extent
(bw, bh) from F. Level i has extent `max(1, W >> i) × max(1, H >> i)`.

1. **Whole levels only, power-of-two steps.** One dropped level halves both sides. There is no
   fractional drop and no resize.
2. **Floor.** Never produce a base level smaller than `max(--min-extent, bw, bh)` on either side
   (default 4×4 for BCn, which is also its block size). If dropping N would break the floor, the
   count is reduced to the largest allowed value; a file that cannot drop at all is reported
   `skipped_floor` (not an error).
3. **Mip-chain integrity.** At least one level remains. L is reduced by the drop count; a chain that
   was already truncated (`L < floor(log2(max(W,H))) + 1`) stays truncated by the same rule: the tool
   does not rebuild missing tail levels.
4. **Non-power-of-two bases.** Allowed. The extent rule is the KTX2/Vulkan one (`max(1, x >> i)`);
   a level whose size is not a multiple of the block extent is padded as the file already stores
   it. The tool copies stored bytes and recomputes only offsets.
5. **Preserve everything else.** `vkFormat`, DFD (colour model, primaries, transfer function,
   channel layout), `supercompressionScheme` (none today, `ktx2_write.rs`), `layerCount`,
   `faceCount`, `pixelDepth`, and all key/value data are copied unchanged. A sRGB texture stays sRGB,
   a BC5 normal map stays BC5. `KTXorientation`, `KTXwriter` and any DO27 key stay. A single added
   key, `RegiaLevelDrop` = `{dropped: N, source_blake3: <hex>}`, records provenance (§ 6 on why
   it is in the hash).
6. **Arrays and cubes.** Layers and faces drop together; every layer of a given level is cut at
   once. Per-layer different counts are never produced.
7. **Supercompressed or format-less inputs** (`vkFormat == 0`, zstd/zlib/BasisLZ) are refused with
   `KDROP_SUPERCOMPRESSED` in v1: dropping a level of a supercompressed stream means a decode,
   which this tool does not do. Revisit if DO27's blobs turn out to use it (Q2).
8. **ASTC.** Same rules; the block extent from F (4×4, 6×6, 8×8). The floor is the block extent.
9. **Idempotence.** Dropping N then M equals dropping N+M, byte for byte except the provenance key
   (which keeps the chain's first `source_blake3`, adds the sum).
10. **Metadata that names the size.** A cooked container may carry a recorded extent or VRAM estimate
    (`LoadKind::Texture`'s estimate in `cooked-pack-guide.md`, the `assets.texture_memory` figures).
    The tool updates the ones it understands and fails the file with `KDROP_STALE_METADATA` for any
    it cannot. Which fields exist in DO27's wrappers is Q1.

## 5. Streaming and virtual-texture interplay

The runtime texture budget (`ADR-130`) can already keep a texture at a lower level. A dropped level is a level the streamer can never fetch: the cap is permanent for that
file. Terrain layer arrays and virtual-texture sources must drop the **same** count across a set
(array layers need equal extents); `--only` on a subset of a layer set is refused with
`KDROP_SET_MISMATCH` when the tool can see the set (terrain manifests), and warned otherwise.

## 6. Determinism and hashing

- **Pure function** of (input bytes, selector, floor). No timestamps, no host paths, no thread-order
  dependence: files are processed in parallel but results are collected and the report sorted by
  relative path. `--budget-mib` sums the inputs in sorted order, so the chosen N is stable.
- The output header/DFD/KV bytes are re-serialized canonically (key/value pairs sorted by key, as
  the KTX2 spec requires; level index recomputed; alignment per spec). Level payload bytes are
  copied, never re-touched. Running the tool twice on the same input gives identical bytes.
- **Hashes.** The report carries blake3 of the input and of the output. Asset-DB cache keys
  (`specs/12` § 6: blake3 of source ‖ settings ‖ importer version ‖ platform ‖ dependency hashes) treat
  the drop as a derived output: key = blake3(input blake3 ‖ selector ‖ floor ‖ tool format version).
- **`format-hashes.ron`.** The tool defines two things `cargo xtask check-formats` should pin: the
  report JSON schema (`ktx2-drop-report`, version 1) and the provenance key layout
  (`RegiaLevelDrop`). The cooked KTX2 layout itself is unchanged, so no existing hash moves and
  no migrator is needed (`specs/12` § 4a). Adding the two entries is `cargo xtask check-formats
  --update` and needs the owner's review like any hash change.
- `--check-format-hash` (opt-in) refuses an input whose container `format_version` the tool does
  not know, instead of passing it through. Default behaviour on unknown container versions is the
  refusal anyway; the flag exists for KTX2 files that carry no container.

## 7. Failure modes

Every failure has a stable code, the offending path and a fix hint (`specs/23` § 4). Per-file failures
never abort the batch; the tool completes the rest and exits 1.

| Code | Cause | Fix hint |
|---|---|---|
| `KDROP_BAD_KTX2` | header or level index does not parse, offsets out of range | re-cook or restore the file |
| `KDROP_SUPERCOMPRESSED` | `vkFormat == 0` or scheme not none | recook from source; see Q2 |
| `KDROP_UNKNOWN_WRAPPER` | `.bct` / `.ktx.postcard` without a documented layout | wait for Q1 or point at the raw KTX2 |
| `KDROP_STALE_METADATA` | container records an extent/estimate the tool cannot update | recook, or extend the wrapper support |
| `KDROP_SET_MISMATCH` | partial drop of a layer array or terrain set | include the whole set |
| `KDROP_TOO_FEW_LEVELS` | requested drop leaves no level (reported as `skipped_floor` for `--max-size`/`--budget-mib`, an error only for explicit `--drop N`) | lower N |
| `KDROP_IO` | read/write failure, out of disk | check space; `--in-place` leaves the original untouched |
| `KDROP_EXISTS` | output path exists, no `--in-place` | choose another `--out` |

Robustness: the parser is bounds-checked everywhere (no `unwrap` on file data; `check-unwraps`
applies), caps total claimed size before allocating, and is added to the `cargo fuzz` targets
(`specs/12`, fuzz list). A truncated file never produces a valid-looking output.

## 8. Agent protocol

Per `AGENTS.md` rule 8 and `specs/23` § 4 (long operations return a job id):

- Tool `assets.drop_levels` (capability `edit_assets` + `cook`): arguments mirror the CLI
  (`inputs`, `selector`, `min_extent`, `only`, `dry_run`); returns a job id; `job_status` streams
  progress; the result is the report JSON.
- `dry_run: true` is the default for agent calls; a write needs `dry_run: false`.
- Read side: the texture-memory report (`assets.texture_memory`, the agent-facing name, confirmed 2026-11-10; it reads
  `regia_render::TextureMemoryReport`) gets the per-texture level count, so an agent can
  plan a drop, run it, and `expect` the totals to fall. Today each `textures[]` row carries `width`, `height`,
  `resident_width`, `resident_height`, `format`, `gpu_bytes` and `cpu_bytes` but no level count: adding one (a new
  `TextureMemoryEntry` field, a host-visible struct change) is part of the implementing lane.
- Agent-guide page `book/agents/texture-level-drop.md` ("shrink cooked textures without the sources"),
  listed in `book/COVERAGE.md`.
- Also available headless with no editor, like `cook`.

## 9. Test plan

All tests are written first (`AGENTS.md` rule 7); none weakened to pass.

Fixtures (generated by the tests with the real cooker, never checked-in third-party textures; small,
deterministic procedural images):

1. 256×256 BC7 sRGB colour, full chain (9 levels).
2. 256×256 BC7 linear (ORM), full chain.
3. 128×128 BC5 normal map.
4. 64×64 BC4 single channel.
5. 130×66 non-power-of-two BC7 (padding path).
6. 16×16 chain (floor path), and a 4×4 one-level file (cannot drop).
7. ASTC 4×4 and 6×6 (via `astc_compress`), Android path.
8. A 2-layer array and a cube (layers/faces).
9. A truncated chain (3 levels of a 256 base).
10. Negative fixtures: supercompressed header, corrupted level index, truncated file.

Tests:

- **Equivalence:** drop N from the full chain equals the levels N.. of the same input byte for byte,
  and equals a cook of the same pixels with the smaller `max_size` for the level payloads that
  the cooker generates deterministically (compare the payloads we can; mip filters must match, so
  this one is limited to textures cooked with the same filter).
- **Format preservation:** `vkFormat`, DFD, KV (except the provenance key), layer/face counts unchanged.
- **Floor:** table-driven (extent, N, block size) against the expected clamp.
- **Idempotence and composition:** drop 1 then 1 equals drop 2.
- **Determinism:** twice, and with `--jobs 1` vs `--jobs 8`, byte-identical output and report; a
  recorded golden blake3 per fixture (`tests/golden/ktx2_drop/*.blake3`, updated only when the
  tool's output format intentionally changes, with the reason in the commit message).
- **Runtime load:** the dropped files load through the real texture path headless and render in the
  GPU test; the sample screenshot of a textured quad equals the reference within the existing
  texture-tier tolerance when the quad's screen size selects the new level 0 anyway.
- **Failure modes:** each code in § 7 has a negative test; the batch continues after a failure; no
  partial output on error.
- **Fuzz:** `ktx2_drop_parse` target; CI runs it for a short fixed budget.
- **Agent:** an `regia_agent` scripted check: query `assets.texture_memory`, `drop_levels` dry run,
  real run, `expect` that the total fell by the reported amount.
- **Platforms:** pure Rust, no platform code; tests run on Linux and Windows; compile-check on
  Android (`cargo ndk -t arm64-v8a check`). The tool itself is desktop CLI.
- **Corpus smoke (owner-run, not CI):** run `--dry-run` over DO27's cooked textures on a bulk disk
  host and record the file counts, outcome-code histogram and the byte saving per selector in
  `docs/progress.md`. This answers Q3.

## 10. Performance

I/O bound. Reads each file once, writes the surviving range once, no decode. Budget: more than
1 GB/s on NVMe per run at `--jobs 8`; a `tracing` span per file. Memory: stream the level payloads,
never load a whole 20 GB set.

## 11. Open questions (need DO27 / Tincal)

Q1. **Wrapper layouts.** What exactly are `.bct` and `.ktx.postcard`? The repo defines neither. We
    need the byte layout (header, where the KTX2 blob sits, any recorded extents/estimates, any
    checksum). Until then only raw KTX2 and Regia Engine `FCKD` containers are supported.
    *Owner: DO27.*
Q2. **Supercompression.** Are DO27's blobs plain (scheme none, as `ktx2_write.rs` writes) or zstd/Basis?
    This decides whether v1 covers them.  *Owner: DO27.*
Q3. **Regenerable share.** How much of the 20 GB has sources (and a recipe) at hand, and how much
    only exists cooked? The corpus smoke (§ 9) gives sizes; the split is DO27's knowledge. It decides
    how much of the work is the drop tool versus recipe recook.  *Owner: DO27.*
Q4. **Which tiers.** The `--max-size` values DO27 wants (for example 2048 / 1024 for tiers), and
    whether terrain layer sets, UI images and HDR env maps are in scope or excluded.  *Owner: DO27.*
Q5. **Recipe expansion.** A later `--recook` mode (regenerate from sources when a recipe exists,
    drop otherwise) waits for Tincal to freeze the recipe format. Questions for Tincal: recipe file
    location, whether the recipe names the source, usage and `max_size`, and whether the recipe
    hash will be recorded in the cooked file so the tool can match them.  *Owner: Tincal.*
Q6. **Hash policy.** Whether DO27's pipeline pins cooked-file hashes in its own manifests; if so a
    drop changes them and the manifest needs a rewrite step or a "derived" marker.  *Owner: DO27.*
Q7. **Where it runs.** In place on the shipping content tree, or always to a separate output
    (the safer default here)?  *Owner: DO27.*

## 12. Rollout

1. Owner accepts or amends ADR-271 and answers Q1 to Q4.
2. Test-first lane: fixtures and tests (§ 9), then the parser/rewriter in `regia_asset_pipeline`
   (next to `ktx2_write.rs`), then the CLI and agent tool, then the guide page and `check-formats`
   entries.
3. Wrapper support for `.bct` / `.ktx.postcard` as a follow-up after Q1.
4. `--recook` after Q5.
