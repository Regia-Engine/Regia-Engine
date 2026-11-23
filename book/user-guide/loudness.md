# Loudness (EBU R128)

`regia_audio` measures perceived loudness per EBU R128 / ITU-R BS.1770-4, so you can normalize
assets to a consistent level instead of guessing by peak or RMS. See
`docs/specs/audio-loudness.md` and ADR-1563 g.

## Why loudness

A quiet voice line and a loud explosion can share a peak level yet be heard very differently.
Loudness tracks the ear. The unit is **LUFS** (Loudness Units relative to Full Scale), and EBU
R128's reference level is **−23 LUFS**.

## Measuring

```rust,wrap
# let decoded_pcm: Vec<f32> = Vec::new();
use regia_audio::{measure_loudness, normalize_gain, REFERENCE_LUFS};

let samples: Vec<f32> = decoded_pcm; // decoded mono (or stereo interleaved) PCM
let loud = measure_loudness(&samples, 1, 48_000).expect("valid input");

// Integrated loudness, plus momentary (400 ms), short-term (3 s), loudness range and true peak.
println!("integrated {} LUFS, true peak {} dBTP", loud.integrated_lufs, loud.true_peak_db);
```

`measure_loudness` measures a whole buffer in one call — the cook-time form. For a live meter,
feed a `LoudnessMeter` blocks of samples with `push` and read `momentary_lufs`, `short_term_lufs`
and `true_peak_db` as it runs, then `finish()` for the gated integrated loudness and loudness
range.

## Normalizing

```rust,wrap
# use regia_audio::{normalize_gain, REFERENCE_LUFS};
# let loud: regia_audio::Loudness = todo!();
let gain = normalize_gain(loud.integrated_lufs, REFERENCE_LUFS);
// Apply `gain` to the samples, or store it and apply at play time.
```

`normalize_gain` returns the amplitude multiplier that brings a measurement to a target
(`10^((target − measured)/20)`), clamped to ±60 dB. Applying it and re-measuring lands within a
fraction of an LU of the target (see `crates/regia_audio/tests/loudness.rs`).

## Details and limits

- **K-weighting** uses the BS.1770 48 kHz coefficient table. Other sample rates use the same
  table (an approximation that only diverges near Nyquist, where a loudness signal carries almost
  no energy).
- **Mono and stereo** interleaving are supported; other layouts return `UnsupportedChannels`.
- **Gating**: the integrated value ignores silence (the −70 LUFS absolute gate) and blocks more
  than 10 LU below the gated mean (the relative gate).
- **Loudness range** leaves out short-term readings below −70 LUFS and readings more than 20 LU
  below their power mean (EBU Tech 3342), so a quiet intro does not widen it.
- **True peak** follows BS.1770-4 Annex 2: 4× oversampled below 96 kHz, 2× at 96 kHz and above, so
  inter-sample peaks a sample peak misses are caught.
- **Agents** read the loudness of the current mix with `audio.render_check` (`lufs`,
  `true_peak_db`; headless only).

## Normalizing at import

The cook measures every `.wav`, `.ogg`, `.flac` and `.mp3` in the project and writes
`loudness.ron` into the cooked cache: one record and one gain per sound. Nothing changes until a
host turns it on:

```rust,wrap
# let sounds: &mut SoundAssets = todo!();
use regia_audio::{LoudnessTable, SoundAssets};

let table = LoudnessTable::load(".regia/cooked/linux/loudness.ron")?;
sounds.set_loudness_normalization(Some(table)); // `None` returns every sound to 0 dB
```

Every sound registered with `SoundAssets::add_file` then plays at the project's target, by default
−23 LUFS with true peaks at or under −1 dBTP. Sounds built in code keep their own level. Set another
target in `loudness.toml` at the project root; exclude one sound with `(normalize: Some(false))` in
its `.meta` `settings`. Agents use `audio.loudness` and `audio.set_loudness_normalization`
([Loudness at import](../agents/loudness-import.md)); the file format is in
`docs/formats/loudness-ron.md`.
