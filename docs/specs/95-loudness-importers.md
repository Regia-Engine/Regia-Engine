# Spec 95 — Loudness at import, and the remaining texture importers (v0.5.0)

Status: proposed (ADR-1855). Master-plan rows: "loudness normalization" and "importers" of the
v0.5.0 deliverables (`docs/master-plan.md` § v0.5.0), `specs/22` T2.14, the gap row A6 ("DDS import
(BC1–BC3, mips); TGA and BMP"). Builds on `specs/audio-loudness.md` (the meter, ADR-1563 g), whose
§ 5 follow-up this spec closes.

## 1. Purpose

1. **Loudness at import.** Every sound source the cook sees (`.wav`, `.ogg`, `.flac`, `.mp3`) is
   decoded once and measured (ITU-R BS.1770-4 / EBU R128: integrated loudness, loudness range, true
   peak). The cook records the measurement and a normalization gain per sound. At run time a host
   turns normalization on with one call, and every sound registered from a file plays at the
   project's target loudness. Off by default: an existing project sounds exactly as before.
2. **Importers.** The cook accepts `.dds` (BC1, BC2, BC3, BC4, BC5, BC7, and uncompressed 32-bit
   RGBA/BGRA; full or partial mip chains), `.tga` and `.bmp`. Before this spec the cook rejected
   all three (and every audio file) with "unsupported extension".

Already present and not changed here: OBJ (`obj_import.rs`), FBX (`fbx_import.rs`, ADR-1602), convex
decomposition (`regia_physics::convex_decomposition`), UI texture atlasing (`atlas_cook.rs`).

## 2. Audio import (`regia_asset_pipeline::audio_cook`, `regia_audio::loudness_table`)

### 2.1 What is measured

- The source is decoded with the runtime's own decoder (kira over symphonia,
  `StaticSoundData::from_cursor`), so the cook measures exactly the frames the mixer will play.
  kira always delivers stereo frames; a mono source arrives as identical left and right samples.
- The measurement is **as played**: the stereo frames, BS.1770 channel weights 1.0 and 1.0. A mono
  file therefore reads 3.01 LU above its single-channel BS.1770 value; the record says so with
  `dual_mono: true` (every frame has `left == right`), so a tool can show the single-channel figure
  (`integrated − 3.0103`). As-played makes a normalized mono SFX and a normalized stereo SFX sound
  equally loud when played unpanned, which is what a game mix needs.
- **Short clips.** A clip shorter than one 400 ms gating block has no gated integrated loudness. The
  record then holds the ungated K-weighted loudness of the whole clip (the meter's momentary value
  after the last sample, which averages the filled part of its window) and `short_clip: true`.
- Silence (no block above the −70 LUFS absolute gate, or an all-zero clip) records
  `integrated_lufs: None` and a gain of 0 dB.

### 2.2 The gain policy

`gain_db = target_lufs − integrated_lufs`, then limited so the normalized true peak stays at or
under the ceiling: `gain_db ≤ true_peak_ceiling_db − true_peak_db` (`peak_limited: true` when this
bound wins), then clamped to ±`MAX_NORMALIZE_GAIN_DB` (60 dB). A sound whose `.meta` opts out, or a
silent one, gets 0 dB. Defaults: target −23 LUFS (EBU R128, `REFERENCE_LUFS`), ceiling −1 dBTP (EBU
R128's maximum true-peak level).

Project policy, optional, `<project>/loudness.toml`:

```toml
target_lufs = -23.0          # -70.0 ..= 0.0
true_peak_ceiling_db = -1.0  # -30.0 ..= 0.0
```

A malformed or out-of-range file is a cook warning and the defaults apply. Per sound, the `.meta`
`settings` string (RON) may hold `(normalize: Some(false))` to exclude a pre-mastered stem, and
`(target_lufs: Some(-18.0))` to override the project target for that sound. A malformed settings
string is a cook warning and the defaults apply.

### 2.3 Cooked outputs

- Per sound: a cooked file (the usual `CookedHeader` envelope) of type `regia_audio::SoundLoudness`,
  postcard payload, importer `audio` 1.0.0. The cache key covers the source bytes, the `.meta`
  settings and the project policy, so changing `loudness.toml` recooks every sound (cheap: no audio
  is re-encoded) and an unchanged sound is skipped without decoding (`cook_inputs`, the policy file
  is one of the audio side inputs).
- Per cook: `<cooked cache>/loudness.ron`, the **loudness table**: every sound's record keyed by its
  `assets/…` path, plus the policy it was computed with. Written atomically after every cook.

The source audio file itself is not converted: the runtime keeps loading it as before
(`SoundAssets::add_file`). Since ADR-2788 the per-sound cooked file is type `regia_audio::CookedSound`
(`regia_audio::cooked_sound`, magic `FSND` v1: the loudness record as RON text, then the source bytes as
authored), so a pak holds each sound and its gain in one entry; a packaged game registers it with
`SoundAssets::add_packed_from_assets` (no loose file) and hands `packed_loudness_table()` to
`set_loudness_normalization`. A sound with no table entry plays at 0 dB. The cooked sound is a cache: a
cook of another layout version is cooked again. Since ADR-4898 `regia package` also stages the cook's table as
`assets/loudness.ron` next to the loose sounds (codes `loudness_table_invalid`, `loudness_table_conflict`,
`loudness_table_write`), so a host that loads files turns normalization on with `LoudnessTable::load`.

### 2.4 Format: `loudness.ron` v1

```ron
(
    format_version: 1,
    policy: (target_lufs: -23.0, true_peak_ceiling_db: -1.0),
    entries: {
        "assets/sfx/door.wav": (
            integrated_lufs: Some(-17.42),
            loudness_range_lu: 0.0,
            true_peak_db: Some(-3.1),
            short_clip: true,
            dual_mono: true,
            sample_rate: 48000,
            frames: 9600,
            normalize: true,
            target_lufs: -23.0,
            gain_db: -5.58,
            peak_limited: false,
        ),
    },
)
```

`format_version` is checked on load: a newer version is refused
(`LoudnessTableError::UnsupportedVersion`), a missing or older one has no migrator yet because v1 is
the first version. Every later field gets a serde
default and a version bump plus a migrator (`specs/12` § 4a). The format is registered with
`cargo xtask check-formats` as `loudness-table` (fingerprint `regia_audio::loudness_table_schema_fingerprint`).
**Migration note:** v1 is new; projects cooked before it simply have no table, and normalization
finds no entry for any sound (every gain 0 dB). The table is derived data: deleting it and
re-cooking rebuilds it, so `regia upgrade-project` needs no step for it.

The `.meta` audio settings string is new optional content inside the existing `.meta` v-current
`settings` field (no `.meta` shape change).

### 2.5 Run-time API (`regia_audio`)

- `LoudnessTable { format_version, policy, entries }`, `LoudnessTable::from_ron(&[u8])`,
  `to_ron_string()`, `load(path)`, `gain_db_for(path)` (exact key, or a file path that ends with
  `/<key>`), `LOUDNESS_TABLE_FILE = "loudness.ron"`, `LOUDNESS_TABLE_VERSION = 1`.
- `SoundLoudness` (the record above), `SoundLoudness::measure_interleaved_stereo(samples, rate)`,
  `SoundLoudness::apply_policy(policy, normalize, target_override)`, `LoudnessPolicy`.
- `measure_sound_bytes(bytes, label) -> Result<SoundLoudness, AudioError>` (`label` names the sound in
  errors) and `measure_sound_bytes_with_limits(bytes, label, &SoundDecodeLimits)` (ADR-4922: a decode past
  the limits is `AudioError::DecodeLimit`); `measure_sound(&SoundAsset)` (a streaming sound is decoded to
  memory first).
- `SoundAsset::with_loudness_gain_db(db)` / `loudness_gain_db()`: a per-asset gain added to every
  play's `volume_db` (0 by default; non-finite values are refused, counted in `rejected_value_count`, and
  leave 0; finite values are clamped to ±`MAX_NORMALIZE_GAIN_DB`, ADR-5821).
- `SoundAssets::set_loudness_normalization(Option<LoudnessTable>) -> usize`: the runtime switch.
  `Some(table)` gives every sound registered from a file its table gain (and every later
  `add_file`/`reload_file`); `None` returns them all to 0 dB. Returns how many sounds changed gain.
  `SoundAssets::loudness_normalization()` reads the current table.

## 3. Texture importers (`regia_asset_pipeline::dds_import`, `cook.rs`)

- **DDS** (Microsoft's documented layout: `DDS_HEADER`, `DDS_PIXELFORMAT`, optional
  `DDS_HEADER_DXT10`). Accepted: FourCC `DXT1`, `DXT2`/`DXT3` (BC2), `DXT4`/`DXT5`, `ATI1`/`BC4U`,
  `ATI2`/`BC5U`, the DX10 header with DXGI BC1/BC2/BC3/BC4/BC5/BC7 (`UNORM`/`UNORM_SRGB`/`TYPELESS`)
  and `R8G8B8A8_UNORM(_SRGB)`/`B8G8R8A8_UNORM(_SRGB)`, and legacy uncompressed 32-bit RGB masks.
  Refused with a clear error whose text starts with its code (`DdsError::code`, ADR-5821): cube maps
  (legacy caps or the DX10 misc flag), volume textures, texture arrays, a DX10 resource that is not 2D,
  signed BC4/BC5, BC6H and every other format (`dds_unsupported`), a truncated mip chain
  (`dds_truncated`), and a file that is not a DDS or whose header is inconsistent (no magic, wrong size
  fields, more mip levels than the size allows, a missing DX10 header: `dds_bad_header`).
- **Pass-through.** A BC1/BC3/BC4/BC5/BC7 DDS with two or more mip levels keeps its blocks when
  `compress` is not `Some(false)`: desktop writes them unchanged into the cooked KTX2 (no re-encode,
  no quality loss); Android transcodes them to ASTC through `transcode_bcn_ktx2_to_astc`.
  `compress: Some(false)` decodes level 0 to RGBA8 and cooks that like a PNG with compression off.
  `max_size` drops leading levels while the edge is above the cap and always keeps the last level,
  so a chain that stops early can cook an edge larger than `max_size` (a 64×64 file with levels
  64 and 32 and `max_size: 8` stays 32×32). Color space: the DX10 `_SRGB` formats are sRGB; a legacy FourCC
  carries none, so the usage (file name or `.meta`, as for PNG) decides (`ColorSrgb` → sRGB).
  Source lint reads that same header and reports the surviving edge. A 64×64 file with
  levels 64 and 32 and `max_size: 8` is evidence of 32×32, not 8 and not 0. The check does
  not resize the source file.
- **Decode.** A single-level BC DDS, any BC2 DDS (the cooked formats have no BC2), a BC1 DDS that
  uses punch-through alpha (a block in its three-color mode; the cooked BC1 is opaque, so passing it
  through would lose the alpha) and every uncompressed DDS are decoded to RGBA8 (BC2: explicit 4-bit alpha plus a BC1 color block,
  decoded here; the other BC kinds through `regia_asset::bcn_decode`) and cooked exactly like a
  PNG: full mips, the usage's BCn (or ASTC on Android), every `.meta` texture setting honoured.
- **TGA and BMP** decode through the `image` crate's existing `tga` and `bmp` features (no new
  dependency) and cook exactly like a PNG (the `png` raster importer; importer version 1.0.0).
- All three are incremental like PNG: an unchanged source with an unchanged `.meta` is skipped
  without import.
- **Packaging.** `regia package` no longer copies `.dds`, `.tga` and `.bmp` raw next to their cooked
  forms. Sounds keep shipping as raw files: the runtime reads them, the cook only measures them.

## 4. Agent protocol

- `audio.loudness { sound?, path?, target_lufs? }` — measures one sound: a registered name
  (`sound`, decoded from the `SoundAssets` entry) or a project file (`path`, project-relative, no
  `..`, no symlink out of the project, the rule of every agent file read). Heavy (it decodes the
  whole sound), read-only. Reports `integrated_lufs` (null for silence), `loudness_range_lu`,
  `true_peak_db`, `short_clip`, `dual_mono`, `sample_rate`, `frames`, `duration_secs`, the
  `target_lufs`, `gain_db` and `peak_limited` that the policy (the active table's, else the
  defaults; `target_lufs` overrides the target) would give it, and, for a registered sound, its
  `applied_gain_db`. Errors: `F0002` (bad parameters, neither or both of `sound`/`path`, a target out
  of range, a path outside the project), `F0003` (unknown sound, missing file),
  `loudness_decode_failed`, `F0013` when the world has no audio.
- `audio.set_loudness_normalization { enabled, table? }` — the runtime switch (`play` permission).
  `enabled: true` loads the table from `table` (a project-relative `loudness.ron`) or keeps the
  active one; `false` turns it off. Reports `enabled`, `entries`, `changed` (sounds whose gain
  changed), `target_lufs` (null when off). Errors: `F0002`, `F0003` (missing file), `F0013`,
  `loudness_no_table` (enabling without a table and none active), `loudness_table_unreadable`,
  `loudness_table_invalid`, `loudness_unsupported_version`.
- Import: `editor.import` already copies and cooks any extension the editor lists; this spec adds
  `dds`, `tga`, `bmp`, `wav`, `ogg`, `flac`, `mp3` to `content::importable_extensions`.

Guide page: `book/agents/loudness-import.md` (both calls, the policy, the table, the importers).
Format page: `docs/formats/loudness-ron.md`.

## 5. Determinism

The measurement is a pure function of the decoded frames: sequential `f64` accumulation in a fixed
order, no threads, no clock. The cooked record and the table (a `BTreeMap`, sorted keys) are
byte-identical across cooks of the same inputs on one platform. Decoders are the runtime's own, so
the cook and a host that measures at run time agree. The run-time gain is a constant per sound added
to the play volume; it does not change mixer state between frames.

## 6. Tests and acceptance

`regia_audio`:
- record: a 0.1-amplitude 1 kHz stereo sine measures within 0.1 LU of the meter; the policy gain
  brings it to −23 (re-measured after scaling, within 0.05 LU); the ceiling limits a hot transient
  (`peak_limited`); silence has no loudness and 0 dB gain; a 100 ms clip is `short_clip` with a
  finite loudness that matches the same tone measured long (within 0.5 LU); `dual_mono` detection.
- table: RON round trip; a newer `format_version` is refused; `gain_db_for` matches the exact key
  and a path suffix at a component boundary, and not a partial file name.
- formats: FLAC, Ogg Vorbis and MP3 fixtures (ffmpeg-generated 1 kHz tones) measure within 0.5 LU of
  the same tone as WAV.
- `SoundAssets::set_loudness_normalization`: a file-registered sound gets its table gain, `None`
  restores 0, the count is reported; an offline render of a quiet and a loud file with
  normalization on measures both within 0.5 LU of the target (integration test, offline backend).

`regia_asset_pipeline`:
- DDS parser: BC1/BC3/BC7 (FourCC and DX10), full and partial chains, odd sizes (5×3), BC2 decode,
  BGRA swizzle; cube map / volume / truncated / bad magic refused.
- cook: a project with a `.wav` (quiet and loud), a `.dds` (BC1 with mips, BC3 single level), a
  `.tga` and a `.bmp` cooks with no errors; the table holds both sounds with gains that bring them
  to the target; the DDS pass-through KTX2 holds the source blocks bit for bit; a re-cook skips
  every new source; editing `loudness.toml` recooks the sounds and changes the table policy; a
  `.meta` `normalize: Some(false)` gives 0 dB.

`regia_agent`: both calls answer through the dispatcher on an app with audio; every error code
(`audio_loudness.rs` unit tests, plus the coverage, contract and fuzz tests that walk every method).

`regia_cli`: `copy_raw_assets` skips `.dds`, `.tga`, `.bmp` and keeps `.wav`.

## 7. Platforms

Linux, Windows, Android aarch64: the measurement and the table are plain Rust in `regia_audio`
(runtime crate, every target). The cook is desktop-only (`regia_asset_pipeline`); Android cooks run
on the desktop with `platform = "android"`, where DDS transcodes to ASTC. No platform code, no
`cfg(target_os)`. macOS builds the same code.

## 8. Performance

Cook: decoding plus measuring is linear in the sound's length; the meter costs about one biquad pair
and the true-peak interpolator (the BS.1770-4 Annex 2 phases since ADR-2517) per sample per channel (well under 1 % of a second of CPU for a second of
48 kHz stereo on a desktop core). Measured 2026-10-30 on the build server under a load average near 100, workspace
dev profile: 60 s of 48 kHz stereo in 347 ms, 0.58 % of real time (the ignored probe
`cost_probe_measuring_a_minute_of_stereo` in `crates/regia_audio/tests/loudness_import.rs`; decode
time not included). It is a cook-time cost only; incremental cooks skip unchanged
sounds without decoding. Run time: `set_loudness_normalization` is O(registered file sounds) once;
per play, one extra `f32` add. Nothing runs per frame, so no frame cost and no `tracing` span is
needed on a per-frame path; the cook function and the switch carry spans.

## 8a. Measurement accuracy (ADR-2517)

The meter is exact at every sample rate (`specs/audio-loudness.md` § 4) and the true peak follows
BS.1770-4 Annex 2. Cooked records made before this keep their figures until the sound is recooked:
the table is derived data.

## 9. Out of scope

Dropping the loose audio copy from `regia package` (audio sources are in the pak since ADR-2788; the raw
copy stays for hosts that load files); baking the gain into the PCM; DDS volumes (cube maps, arrays, BC6H and signed
BC4/BC5 were added by ADR-3266); texture mip streaming metadata and Android asset packs (the other T2.14 items). The
per-sample-rate K-weighting and the BS.1770 Annex 2 true-peak filter, out of scope when this spec was
written, were done by ADR-2517 (§ 8a).

## 10. PLY, STL and PSD importers (ADR-3244)

Three source formats with no importer until now. All three readers are pure Rust (no new dependency; `flate2`, already in
the pipeline, inflates PSD ZIP data), bounds-checked for untrusted input, and covered by fuzz targets
(`import.ply`, `import.stl`, `import.psd`). The user-facing description, settings and error codes are in
`book/agents/mesh-image-import.md`; this section records the design.

**Meshes (PLY, STL).** Like OBJ (ADR-1602) a mesh file is converted to an in-memory glTF 2.0 binary (one node, one
primitive, no material) and cooked by `import_gltf_converted`, so the cooked formats, LODs, meshlets and `.meta`
handling are the glTF ones and no format version changes. The `.meta` settings string is parsed by the format's own
settings type (`PlyImportSettings`, `StlImportSettings`); unknown keys (the glTF keys) are ignored. The cook key
hashes the settings string and the converter version (`importer_version` 1.1.0 shared with OBJ and FBX, which follow
the glTF importer). PLY has no unit or axis metadata (meters, Y up by default, `scale` and `up_axis` settings). STL
defaults to millimeters and Z up, the CAD and slicer convention. A point cloud PLY is `ply_no_faces`; point clouds are a
follow-up.

**Images (PSD).** `.psd` cooks through `cook_rgba8_payload`, the path `.png` takes, so usage, `max_size`, compression,
mips and streaming apply unchanged and the cooked type is `regia_asset::CookedTexture`. The importer is `psd` 1.0.0;
the cook key adds the `layer` and `alpha` settings. 16-bit channels round to 8 bits. The composite is the default
because it is what the artist sees; `layer` exists for sprite sheets and UI art kept as layers.

**Hostile input.** Every count is checked against `limits` before anything is allocated (PLY vertex, face and
triangle counts; STL triangle count against the file length; PSD pixels from the header, RLE rows against their declared
width, ZIP output against its expected size), every read is bounds-checked, and every failure is a typed error with a
lowercase code. The unit tests truncate each fixture at every length and feed counts of four billion.

**Out of scope here.** PLY point clouds, PSB, CMYK/indexed/Lab/32-bit PSD, layer effects, masks and blend modes, PSD
smart-object linking, STL colors (the VisCAM/Materialise attribute extensions), the `regia import` CLI command for
these formats and the texture size report for `.psd`.

## 11. USD import (T2.14 importers, ADR-3235)

`.usda`, `.usdc`, `.usd` and `.usdz` sources cook like FBX and OBJ (ADR-1602): the pure-Rust `openusd` reader (MIT,
pipeline only) composes the stage, `regia_asset_pipeline::usd_import` converts it to an in-memory glTF 2.0 binary,
and the glTF importer cooks that (importer `usd`, version 1.0.0). No cooked format changes.

1. **Inputs.** Before composition the importer walks the layer graph itself (`usd_import::vfs`): root, sublayers,
   references, payloads (in every variant) and the textures materials name, each file read once within the caps from
   inside the project (or the source's folder outside a project), symlinks checked. Cycles (`usd_reference_cycle`)
   and chains deeper than `max_reference_depth` (`usd_reference_too_deep`) are refused; absolute, URL and escaping
   paths are warnings. The stage is composed through a resolver that serves only the walked bytes.
2. **Mapping.** Per `book/agents/fbx-import.md` § USD: Xform ops to TRS; meshes with fan triangulation, every primvar
   interpolation and indexed primvars, UV sets (`st` first), display color, `materialBind` subsets as primitives;
   UsdPreviewSurface with `UsdUVTexture` (embedded images, metallic/roughness packed when needed); cameras; distant,
   sphere, disk and rect lights; `metersPerUnit` (fallback 0.01) and `upAxis` baked to meters, +Y up; references,
   payloads, sublayers and variants flattened by composition; instances share one converted mesh per prototype.
   UsdSkel is not imported (rigid meshes, warning).
3. **Settings and limits.** `UsdImportSettings` in the `.meta` (`scale`, `meters_per_unit`, `up_axis`,
   `import_cameras`, `import_lights`, `limits`); `UsdLimits` caps file and total bytes, layers, reference depth,
   prims, hierarchy depth, vertices, indices, polygon corners and text nesting.
4. **Surfaces.** `regia import <file.usd*>`, `editor.usd_inspect` (`data.usd_error` codes), fuzz targets `import.usd`
   and `import.usdz`.
5. **Determinism.** The same files convert to the same GLB bytes (maps are ordered, images deduplicated by resolved
   path); the cook's input digest covers every layer and texture read, so an edit to a referenced file recooks.
6. **Follow-ups.** UsdSkel skinning, blend shapes and animation; `UsdTransform2d` as `KHR_texture_transform`;
   point instancers; implicit shapes (`Cube`, `Sphere`, ...); per-instance mesh sharing in the cooked model.
