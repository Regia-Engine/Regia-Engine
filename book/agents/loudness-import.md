# Loudness at import

Every sound the cook sees (`.wav`, `.ogg`, `.flac`, `.mp3`) is decoded once and measured against
EBU R128 / ITU-R BS.1770-4. The cook writes `loudness.ron` into the cooked cache, with one record
and one normalization gain per sound. Normalization is off until a host or an agent turns it on, so
a project sounds the same as before until then (ADR-1855, `docs/specs/95-loudness-importers.md`).

Both methods need `regia_audio::AudioPlugin` (`regia::DefaultPlugins` installs it); without it they
return `UNSUPPORTED` (`F0013`).

## Measure one sound

`audio.loudness` (read-only, heavy: it decodes the whole sound) measures a registered sound or a
project file. Give exactly one of `sound` and `path`. `path` is relative to the project root, with
no `..`.

```json
{ "method": "audio.loudness", "params": { "sound": "door", "target_lufs": -18 } }
→ { "integrated_lufs": -17.42, "loudness_range_lu": 0.0, "true_peak_db": -3.1,
    "short_clip": true, "dual_mono": true, "sample_rate": 48000, "frames": 9600,
    "duration_secs": 0.2, "target_lufs": -18.0, "gain_db": -0.58, "peak_limited": false,
    "applied_gain_db": 0.0 }
```

- `integrated_lufs` is the gated integrated loudness, `null` for silence. A sound shorter than one
  400 ms gating block has no gated value: `short_clip` is true and the figure is the ungated
  loudness of the whole clip.
- `dual_mono` means every frame has equal left and right samples. The decoder always delivers
  stereo, so a mono file reads 3.01 LU above its single-channel value, which is how it plays
  unpanned.
- `gain_db` is what the policy would give the sound: the active table's policy, else −23 LUFS and a
  −1 dBTP ceiling. `target_lufs` (−70..=0) replaces the target for this reply only.
  `peak_limited` means the true-peak ceiling lowered the gain.
- `applied_gain_db` (registered sounds only) is the gain in force now, 0 while normalization is off.

## Turn normalization on or off

`audio.set_loudness_normalization` (`play` permission) is the run-time switch.

```json
{ "method": "audio.set_loudness_normalization",
  "params": { "enabled": true, "table": ".regia/cooked/linux/loudness.ron" } }
→ { "enabled": true, "entries": 42, "changed": 37, "target_lufs": -23.0 }
```

`table` is a project-relative `loudness.ron`; without it the active table stays. `enabled: false`
returns every file sound to 0 dB. `changed` counts the sounds whose gain changed. Only sounds
registered from a file (`SoundAssets::add_file`) are affected, and they take the new gain on their
next play: voices already playing keep theirs.

## Errors

| `data.code` | Meaning |
|---|---|
| `F0002` | Bad parameters: neither or both of `sound` and `path`, `target_lufs` out of range, a path outside the project, `enabled` missing. |
| `F0003` | No sound with that name, or no file at that path. |
| `F0013` | The app has no audio plugin. |
| `loudness_decode_failed` | The sound does not decode or holds no frame. |
| `loudness_no_table` | `enabled: true` with no `table` and no active table. |
| `loudness_table_unreadable` | The table path could not be read (a directory, a permission error). |
| `loudness_table_invalid` | The file is not a loudness table. |
| `loudness_unsupported_version` | The table's `format_version` is newer than this build reads. |

A refused table leaves the previous state in place.

## Policy

Optional `loudness.toml` at the project root sets the project target and ceiling:

```toml
target_lufs = -23.0          # -70.0 ..= 0.0
true_peak_ceiling_db = -1.0  # -30.0 ..= 0.0
```

Per sound, the `.meta` `settings` string holds `(normalize: Some(false))` to exclude a pre-mastered
stem, or `(target_lufs: Some(-18.0))` to change the target. Editing either recooks the affected
sounds; no audio is re-encoded. A bad file is a cook warning and the defaults apply.

## Importers

`editor.import` and the cook accept these sources, next to glTF, FBX, OBJ, PNG and `.material.ron`:

| Source | What the cook does |
|---|---|
| `.wav`, `.ogg`, `.flac`, `.mp3` | Measures it and writes the record together with the file's bytes as authored (one cooked sound, so a pak carries the audio and its gain; see below). The file on disk is not converted. |
| `.dds` BC1, BC3, BC4, BC5, BC7 with two or more mips | Keeps the blocks when `compress` is not `Some(false)` (desktop KTX2, or ASTC on Android). `compress: Some(false)` decodes level 0 to RGBA8. `max_size` drops leading levels and always keeps the last, so a short chain can stay larger than the cap. |
| `.dds` single level, any BC2, BC1 with punch-through alpha, uncompressed 32-bit | Decodes to RGBA8, then cooks like a PNG. |
| `.tga`, `.bmp` | Decodes, then cooks like a PNG. |

DDS cube maps, arrays, BC6H and signed BC4 and BC5 are read too ([DDS import](dds-import.md)). The cook
refuses DDS volumes, other formats and truncated mip chains, with a per-file error in the cook report.
The error text starts with its code:

| Code | Meaning |
|---|---|
| `dds_bad_header` | Not a DDS file (no `DDS ` magic), a wrong header or pixel-format size field, a size outside 1..=32768, more mip levels than the size allows, a `DX10` FourCC with no DX10 header, a DX10 array size outside 1..=2048, or cube faces that are not square. |
| `dds_unsupported` | A valid DDS of a kind the importer does not read: a volume (legacy caps or a DX10 3D resource), a legacy cube map without all six faces, a DX10 resource that is not 2D or 3D, another DXGI format or FourCC, or a legacy layout that is not 32-bit 8:8:8(:8). |
| `dds_truncated` | The file ends before its last mip level (the message names the level and the bytes missing). |

A malformed `.meta` audio `settings` string (or a `target_lufs` outside -70..=0) is a cook warning naming the
sound, and that sound cooks with the defaults.

## Sounds in a pak (a packaged game)

A cooked sound (`regia_audio::CookedSound`, ADR-2788) holds the source file's bytes and its loudness record,
and `regia package` puts it into the pak with the rest of the cooked assets. A packaged game registers it
with no loose file:

```rust
# use regia_asset::AssetServer;
# use regia_audio::{AudioError, SoundAssets};
# fn register_packed(sounds: &mut SoundAssets, mut asset_server: &mut AssetServer) -> Result<(), AudioError> {
sounds.add_packed_from_assets("door", &mut asset_server, "assets/sfx/door.wav")?;
sounds.set_loudness_normalization(Some(sounds.packed_loudness_table()));
# Ok(())
# }
```

The decode reads the pak entry (the dev cache in a development run), and `packed_loudness_table()` is
the records of the sounds registered so far: the gains equal the loose `loudness.ron`'s. A sound with no
table entry plays at 0 dB. `assets.cooked_pack` lists the entries as `kinds.audio` (count and bytes).
Errors (text in the `AudioError::Invalid` message): `cooked_sound_missing` (the path is not in the
manifest, the entry does not read, or it is another type), `cooked_sound_bad_magic`, `cooked_sound_version`
(a layout this build does not read: cook again) and `cooked_sound_malformed` (a cut-short entry).
A payload whose audio would decode past the default `SoundDecodeLimits` (30 minutes or 1 GiB of
frames, ADR-4922) returns `AudioError::DecodeLimit` and is not registered, the same stop as a loose file.

## Verify a mix

1. Cook the project, then `audio.set_loudness_normalization { "enabled": true, "table": … }`.
2. Play the sound headless and read `audio.render_check { "seconds": 1.0 }`: `lufs` is the rendered
   loudness of the mix.
3. `audio.loudness { "sound": … }`: `applied_gain_db` is the gain on the registered sound. After `audio.set_loudness_normalization`, that is the table entry's gain for the sound's file (0 dB when the table has no entry). `gain_db` is measured again from the active table's policy, or the defaults when no table is set, unless this call passes `target_lufs`. The two match when the cook used that same target. A `.meta` `(target_lufs: Some(-18.0))` changes the cooked gain and leaves normalization on, so they differ. `(normalize: Some(false))` stores 0 dB while this call still reports the policy gain.

## Packaged games (ADR-4898)

`regia package` stages the cook's table as `assets/loudness.ron`, next to the sounds it names
(also inside the Android APK assets). A packaged game turns normalization on with
`audio.set_loudness_normalization { "enabled": true, "table": "assets/loudness.ron" }` (or
`LoudnessTable::load` and `SoundAssets::set_loudness_normalization` from host code). The table's
`assets/...` keys match the shipped files by path suffix, and a sound shipped as Ogg Vorbis keeps the
gain measured on its source. The gain is applied once per play: turning normalization on again,
reloading a file or decoding a stream to memory does not add it a second time.

Packaging stops with an error, staging nothing, when:

| Code | Meaning |
|---|---|
| `loudness_table_invalid` | The cooked `loudness.ron` does not read (damaged or newer); recook. |
| `loudness_table_conflict` | A raw `assets/loudness.ron` of the project would take the table's place; rename it. |
| `loudness_table_write` | The staged table could not be written. |

To verify a package, start it with the agent server, then run steps 1 to 3 of "Verify a mix" with
`"table": "assets/loudness.ron"` and `expect` that `lufs` is within 0.5 LU of `target_lufs`.

The cook itself refuses a platform tag that is not `linux`, `windows`, `android`, `macos` or the
host's own, before writing anything; the message starts with `cook_platform_unsupported`.
