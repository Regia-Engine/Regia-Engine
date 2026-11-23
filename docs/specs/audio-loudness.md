# Spec — Audio loudness normalization (EBU R128 / ITU-R BS.1770-4)

This is the `specs/22` § T2.14 item "audio loudness normalisation (EBU R128)", split into its own
spec per `specs/22`. Implementation: `crates/regia_audio/src/loudness.rs` (ADR-1563 g).

## 1. Purpose

Peak and RMS track a waveform; loudness tracks the ear. Two assets with the same peak can be heard
as very different levels, so a game that ships raw recordings has inconsistent SFX/VO. This spec
adds standard loudness **measurement** (EBU R128 / ITU-R BS.1770-4) and a **normalization gain**
so an asset can be baked to a target perceived level.

## 2. Model

- Loudness is measured on decoded PCM: mono, stereo or 5.1 interleaved `f32` frames.
- The **K-weighting** (BS.1770 § 3): a +4 dB high shelf (the "head" pre-filter) followed by the
  "RLB" high-pass, exact at every sample rate (§ 4).
- Loudness in **LUFS**: `L = −0.691 + 10·log₁₀(Σᵢ Gᵢ·zᵢ)`, where `zᵢ` is the mean square of the
  K-weighted channel and `Gᵢ` the channel weight (mono `1.0`; stereo `1.0, 1.0`; 5.1 in the order
  L, R, C, LFE, Ls, Rs: `1.0, 1.0, 1.0, 0, 1.41, 1.41`, BS.1770-4 Table 3, the LFE excluded from the
  loudness but not from the true peak; ADR-7227).
- **Momentary** loudness over a 400 ms window; **short-term** over 3 s.
- **Integrated** loudness over the whole buffer, gated (BS.1770 § 4.1): 400 ms gating blocks with
  75 % overlap (100 ms hop); the **absolute gate** excludes blocks below −70 LUFS; the **relative
  gate** then excludes blocks more than 10 LU below the gated mean; the integrated loudness is the
  mean (in the power domain) of the surviving blocks.
- **Loudness range** (LRA, EBU Tech 3342): the 10th–95th percentile of the short-term loudness
  after the absolute gate and the −20 LU relative gate (below the power mean of the
  absolute-gated readings), from a 0.1 LU histogram.
- **True peak** (dBTP), BS.1770-4 Annex 2: the largest magnitude of the signal oversampled 4× below
  96 kHz (the Annex's 48-tap polyphase filter, 12 taps per phase) and 2× at 96 kHz and above (a 12-tap
  Hann-windowed sinc for the half-sample point: the Annex prints no 2× table), and never below the sample peak.

## 3. API

In `regia_audio`, re-exported at the crate root:

- `LoudnessMeter::new(channels, sample_rate) -> Result<Self, LoudnessError>` (channels 1, 2 or 6;
  `channel_weights(channels)` and `FIVE_ONE_WEIGHTS` give the weights),
  `push(&[f32])` (interleaved frames; a trailing partial frame is ignored), and the live getters
  `momentary_lufs`, `short_term_lufs`, `true_peak_db`, plus `finish() -> Loudness`.
- `Loudness { momentary_lufs, short_term_lufs, integrated_lufs, loudness_range_lu, true_peak_db }`.
  A loudness value with no data behind it is `−∞`; the loudness range is `0.0` with fewer than two
  readings; the true peak of silence, or of a meter that has seen no sample, is `−100` dBTP, the
  floor of `gain_to_db`, so it is always finite (ADR-5662).
- `measure_loudness(interleaved, channels, sample_rate) -> Result<Loudness, LoudnessError>`: the
  one-shot form (measure an asset at cook time).
- `normalize_gain(measured_lufs, target_lufs) -> f32`: `10^((target − measured)/20)`, clamped to
  ±60 dB, `1.0` for non-finite input.
- Constants: `REFERENCE_LUFS = −23.0`, `MOMENTARY_SECS`, `SHORT_TERM_SECS`, `GATING_BLOCK_SECS`,
  `GATING_HOP_SECS`, `ABSOLUTE_GATE_LUFS`, `RELATIVE_GATE_LU`, `LOUDNESS_OFFSET_DB`,
  `MAX_NORMALIZE_GAIN_DB`.

Errors (`LoudnessError`): `Empty`, `ZeroSampleRate`, `UnsupportedChannels(u8)`. Non-finite input
samples are dropped (the K-weighting clears its state so later finite samples recover), matching
`regia_audio`'s "bad input never reaches the mixer" rule.

## 4. Sample rate

BS.1770-4 publishes the K-weighting at 48 kHz only. Each stage of that table is inverted once into its
analog prototype `H(s) = (vh·s² + (vb/q)·s + vl) / (s² + s/q + 1)`, with `s` in units of `2π·f0` (the
shelf comes out at `f0 ≈ 1681.97 Hz`, `q ≈ 0.7072`, `vh ≈ +4 dB`; the high-pass at `f0 ≈ 38.135 Hz`,
`q ≈ 0.5003`). The bilinear transform with `f0` pre-warped, `K = tan(π·f0/rate)` as in R. Bristow-Johnson's
"Audio EQ Cookbook", maps them to the buffer's rate. At 48 kHz that returns the published table within
1e-9, so 48 kHz readings are unchanged. A rate at or under twice a corner keeps the corner just under
Nyquist, which is stable if not meaningful. Window and block sizes follow the same rate. Calibration: a
997 Hz sine at −20 dBFS on two in-phase channels reads −20.0 LUFS at 44.1, 48 and 96 kHz.

## 5. Editor / cook integration (follow-up, not this lane)

This lane ships the measurement and gain; it does **not** wire the cook. `specs/95-loudness-importers.md`
(ADR-1855) closes the follow-up: the cook measures each sound into `loudness.ron`, and the host turns
the gains on at run time. The PCM is not baked.

## 6. Agent protocol

`audio.render_check` (`book/agents/audio-mix.md`) reports the rendered mix's integrated loudness
(`lufs`) and true peak (`true_peak_db`) with this module (ADR-1563 g). The cook/import
surface is `audio.loudness` and `audio.set_loudness_normalization` (`specs/95`).

As built (ADR-7227): the live program-loudness meter. `regia_audio::program_loudness` runs a tap inside
the master limiter effect (after limiting), off by default, allocation-free on the audio thread. It
publishes momentary, short-term, gated integrated loudness, loudness range and true peak every 100 ms
of output; the integrated value comes from a fixed 0.01 LU histogram (count and summed power per bin),
so only the relative-gate bin is approximate. `Audio::{program_loudness, set_program_loudness_enabled,
reset_program_loudness}`; agent `audio.program_loudness { enable?, reset? }`
(`book/agents/audio-program-loudness.md`); the editor mixer dock's Program row and
`editor.audio_mixer_get` `program`. `tests/program_loudness.rs` holds it to the offline meter within
0.01 LU on a 1 kHz sine, pink noise and a level step.

## 7. Acceptance

- Unit: the K-weighting biquads have the specified shelf/high-pass response at DC and Nyquist; the
  meter's measured gain matches the analytical `|H(e^jω)|` at several frequencies; doubling
  amplitude raises loudness 6.02 LU; a second identical channel raises it 3.01 LU; the absolute
  gate excludes silence; a constant signal has LRA ≈ 0; `normalize_gain` maps a measurement to its
  target; the true peak catches a 12 kHz ±0.707-sampled sine peaking at 1.0 between samples; bad
  inputs return errors.
- Integration (`tests/loudness.rs`): normalizing a quiet (0.05) and a loud (0.4) sine to
  `REFERENCE_LUFS` and re-measuring lands both within 0.05 LU of −23 LUFS; stereo is supported;
  `normalize_gain` is never non-finite.
- Conformance (`tests/loudness_conformance.rs`, ADR-5662): EBU Tech 3341 cases 1–5 (steady tones
  read their level as momentary, short-term and integrated loudness; the absolute and relative
  gates) within ±0.1 LU, EBU Tech 3342 cases 1–4 (loudness range of level steps) within ±1 LU, a
  non-finite sample reads as silence and the meter recovers, and an empty meter reads `−∞` and
  `−100` dBTP. Sample-rate calibration and the true peak are in `tests/loudness_accuracy.rs`.

## 8. Provenance

ITU-R BS.1770-4 (K-weighting coefficients, offset, gates, windows); EBU R128 (the −23 LUFS
reference); EBU Tech 3342 (LRA); ITU-R BS.1770-4 Annex 2 (true peak); R. Bristow-Johnson, "Audio EQ
Cookbook" (the bilinear transform); windowed-sinc interpolation (standard multirate DSP, the 2× path). See
`docs/provenance.md`.
