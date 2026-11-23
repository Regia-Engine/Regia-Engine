# `loudness.ron` — the loudness table (format v1)

Loader: `regia_audio::LoudnessTable::from_ron` (`load` for a file). Writer: the cook, atomically,
after every cook, at `<cooked cache>/loudness.ron` (`LOUDNESS_TABLE_FILE`). Registered with `cargo
xtask check-formats` as `loudness-table`. See `docs/specs/95-loudness-importers.md` § 2.4, ADR-1855.

```ron
(
    format_version: 1,
    policy: (target_lufs: -23.0, true_peak_ceiling_db: -1.0),
    entries: {
        "assets/sfx/door.wav": (
            integrated_lufs: Some(-17.42),  // None for silence
            loudness_range_lu: 0.0,
            true_peak_db: Some(-3.1),       // None for an all-zero clip
            short_clip: true,               // under 400 ms: integrated_lufs is ungated
            dual_mono: true,                // every frame has left == right
            sample_rate: 48000,
            frames: 9600,
            normalize: true,                // false when the .meta opted out
            target_lufs: -23.0,
            gain_db: -5.58,
            peak_limited: false,
        ),
    },
)
```

`entries` is sorted by path, so a re-cook of the same inputs writes the same bytes.

A `format_version` newer than the build reads is refused (`LoudnessTableError::UnsupportedVersion`).
Later fields get a serde default and a version bump plus a migrator (`docs/specs/12-assets.md` § 4a).

**Migration note.** v1 is the first version. Projects cooked before it have no table, and
normalization finds no entry for any sound, so every gain stays 0 dB. The table is derived data:
deleting it and cooking again rebuilds it, so `regia upgrade-project` has no step for it.

## `loudness.toml`

Optional, at the project root, hand-written. Not versioned: it has two optional keys.

```toml
target_lufs = -23.0          # -70.0 ..= 0.0
true_peak_ceiling_db = -1.0  # -30.0 ..= 0.0
```

A malformed or out-of-range file is a cook warning and the defaults apply.
