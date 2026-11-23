// SPDX-License-Identifier: MPL-2.0
//! The audio importer: loudness at import (`docs/specs/95-loudness-importers.md` § 2, ADR-1855).
//!
//! A `.wav`, `.ogg`, `.flac` or `.mp3` source is decoded with the runtime decoder and measured
//! (ITU-R BS.1770-4 / EBU R128, `regia_audio::SoundLoudness`). The cooked output is the record
//! together with the source file's bytes (type `regia_audio::CookedSound`,
//! `regia_audio::cooked_sound`, ADR-2788), so a pak carries each sound and its gain in one entry and a
//! packaged game plays it with no loose file; the source file on disk is not changed, and a loose
//! file still loads as before. After every cook the records are gathered into
//! `<cooked cache>/loudness.ron` ([`write_loudness_table`]), which a host hands to
//! `SoundAssets::set_loudness_normalization`.
//!
//! Policy: `<project>/loudness.toml` (`target_lufs`, `true_peak_ceiling_db`; optional), and per
//! sound the `.meta` `settings` string ([`AudioImporterSettings`]).
//!
//! Two more rows (ADR-2721): a sound whose settings say `impulse_response: Some(true)` is also
//! checked as a convolution impulse response (it must decode whole, fit the desktop tier's 4 s,
//! and is never loudness-normalized, which would change the room's level), and an
//! `.ambience.ron` is validated ([`cook_ambience`]) and cooked as its own text.

use std::path::Path;

use regia_asset::{AssetId, decode_cooked_file};
use regia_audio::{LOUDNESS_TABLE_FILE, LoudnessPolicy, LoudnessTable, SoundLoudness};
use serde::{Deserialize, Serialize};

use crate::cook::CookError;
use crate::db::AssetDatabase;

/// The project's loudness policy file, at the project root.
pub const LOUDNESS_CONFIG_FILE: &str = "loudness.toml";

/// The importer name the cook records for sounds.
pub const AUDIO_IMPORTER: &str = "audio";

/// The cooked type path of a sound's loudness record alone (written before ADR-2788; read, never written).
pub const SOUND_LOUDNESS_TYPE: &str = "regia_audio::SoundLoudness";

/// The cooked type path of a sound: its source bytes and loudness record (ADR-2788).
pub const COOKED_SOUND_TYPE: &str = regia_audio::cooked_sound::COOKED_SOUND_TYPE;

/// True for the source extensions the audio importer reads (lowercase, no dot).
#[must_use]
pub fn is_audio_extension(ext: &str) -> bool {
    matches!(ext, "wav" | "ogg" | "flac" | "mp3")
}

/// Per-sound audio importer settings: the `.meta` `settings` string, RON.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioImporterSettings {
    /// `Some(false)` excludes this sound from normalization (a pre-mastered stem). Default on.
    pub normalize: Option<bool>,
    /// Replaces the project's target loudness for this sound, LUFS.
    pub target_lufs: Option<f32>,
    /// Overrides the project's ship mode for this sound (`audio_cook.toml`; RON `Some(vorbis)`).
    pub mode: Option<crate::audio_transcode::AudioCookMode>,
    /// Vorbis quality on desktop for this sound, replacing the project's.
    pub quality: Option<f32>,
    /// Vorbis quality on Android for this sound, replacing the project's.
    pub android_quality: Option<f32>,
    /// Overrides the project's `force` (transcode an `.ogg` or `.mp3` source too).
    pub force: Option<bool>,
    /// `Some(true)`: the sound is a convolution impulse response (checked as one, never
    /// normalized; ADR-2721).
    pub impulse_response: Option<bool>,
}

impl AudioImporterSettings {
    /// Parses the `.meta` settings string; an empty string is the defaults. A malformed string,
    /// or a target outside `LoudnessPolicy::TARGET_RANGE`, gives the defaults and the problem.
    #[must_use]
    pub fn from_meta_string(settings: Option<&str>) -> (Self, Option<String>) {
        let Some(text) = settings.filter(|s| !s.trim().is_empty()) else {
            return (Self::default(), None);
        };
        match ron::from_str::<Self>(text) {
            Ok(parsed) => {
                let (lo, hi) = LoudnessPolicy::TARGET_RANGE;
                if let Some(q) = [parsed.quality, parsed.android_quality]
                    .into_iter()
                    .flatten()
                    .find(|q| !crate::audio_transcode::quality_ok(*q))
                {
                    return (
                        Self::default(),
                        Some(format!("audio settings: quality {q} is outside the Vorbis range")),
                    );
                }
                match parsed.target_lufs {
                    Some(t) if !(t.is_finite() && (lo..=hi).contains(&t)) => (
                        Self::default(),
                        Some(format!("audio settings: target_lufs {t} is outside {lo}..={hi}")),
                    ),
                    _ => (parsed, None),
                }
            }
            Err(e) => (
                Self::default(),
                Some(format!("audio settings are not valid RON: {e}")),
            ),
        }
    }

    /// Whether normalization applies (default on; never for an impulse response).
    #[must_use]
    pub fn normalize(&self) -> bool {
        self.normalize.unwrap_or(true) && !self.is_impulse_response()
    }

    /// Whether the sound is a convolution impulse response.
    #[must_use]
    pub fn is_impulse_response(&self) -> bool {
        self.impulse_response.unwrap_or(false)
    }
}

/// `loudness.toml`'s shape.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoudnessConfigFile {
    target_lufs: Option<f32>,
    true_peak_ceiling_db: Option<f32>,
}

/// The project's policy from `<project>/loudness.toml`, and its problem (malformed, unknown key,
/// out of range; the defaults then apply). No file is the defaults and no problem.
#[must_use]
pub fn load_loudness_policy(project_root: &Path) -> (LoudnessPolicy, Option<String>) {
    let path = project_root.join(LOUDNESS_CONFIG_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (LoudnessPolicy::default(), None);
    };
    let file: LoudnessConfigFile = match toml::from_str(&text) {
        Ok(file) => file,
        Err(e) => {
            return (
                LoudnessPolicy::default(),
                Some(format!("{LOUDNESS_CONFIG_FILE}: {e}; using the defaults")),
            );
        }
    };
    let defaults = LoudnessPolicy::default();
    let policy = LoudnessPolicy {
        target_lufs: file.target_lufs.unwrap_or(defaults.target_lufs),
        true_peak_ceiling_db: file.true_peak_ceiling_db.unwrap_or(defaults.true_peak_ceiling_db),
    };
    match policy.problem() {
        None => (policy, None),
        Some(problem) => (
            defaults,
            Some(format!("{LOUDNESS_CONFIG_FILE}: {problem}; using the defaults")),
        ),
    }
}

/// The cook's output for one sound: importer, settings hash, id, type path, payload.
pub(crate) struct AudioCook {
    pub(crate) settings_hash: [u8; 32],
    pub(crate) id: AssetId,
    pub(crate) payload: Vec<u8>,
}

/// Decodes and measures one sound source, applying the project policy and its `.meta` settings.
/// The loudness is measured on the **source** whatever the ship plan; a transcode plan also
/// encodes the sound into the cooked cache (`audio_transcode`, ADR-3230), so a corrupt source
/// fails here with the same error as before.
pub(crate) fn cook_audio(
    source: &Path,
    bytes: &[u8],
    ext: &str,
    project_root: &Path,
    rel_path: &str,
    cache_dir: &Path,
    platform: &str,
) -> Result<AudioCook, CookError> {
    let _span =
        tracing::info_span!(target: "regia_asset_pipeline", "cook_audio", path = %source.display()).entered();
    let meta = AssetDatabase::load_or_create_meta(source, AUDIO_IMPORTER).map_err(CookError::Db)?;
    let resolved = crate::import_rules::resolve_for_cook(
        project_root,
        rel_path,
        AUDIO_IMPORTER,
        meta.settings.as_deref(),
    );
    let (settings, problem) =
        AudioImporterSettings::from_meta_string((!resolved.is_empty()).then_some(resolved.as_ref()));
    if let Some(problem) = problem {
        tracing::warn!(target: "regia_asset_pipeline", path = %source.display(), "{problem}");
        // Spec 95 § 2.2: a malformed settings string is a cook warning (the cook report drains
        // this queue after each source), not only a log line; the defaults apply.
        crate::subasset_remap::push_warnings([format!("{rel_path}: {problem}; using the defaults")]);
    }
    let (policy, policy_problem) = load_loudness_policy(project_root);
    if let Some(problem) = policy_problem {
        tracing::warn!(target: "regia_asset_pipeline", "{problem}");
    }
    let (ship_policy, ship_problem) = crate::audio_transcode::load_audio_cook_policy(project_root);
    if let Some(problem) = ship_problem {
        tracing::warn!(target: "regia_asset_pipeline", "{problem}");
    }
    let plan = crate::audio_transcode::resolve_plan(&ship_policy, &settings, ext, platform);
    if settings.is_impulse_response() {
        check_impulse_response(bytes)?;
    }
    let label = source.display().to_string();
    let mut record = regia_audio::measure_sound_bytes(bytes.to_vec(), &label)
        .map_err(|e| CookError::Audio(format!("{ext}: {e}")))?;
    record.apply_policy(&policy, settings.normalize(), settings.target_lufs);
    crate::audio_transcode::transcode_cached(cache_dir, rel_path, bytes, plan)
        .map_err(|e| CookError::Audio(format!("{ext}: {e}")))?;
    let settings_hash = *blake3::hash(
        format!(
            "audio2;normalize={:?};target={:?};policy={:?},{:?}{}{}",
            settings.normalize,
            settings.target_lufs,
            policy.target_lufs,
            policy.true_peak_ceiling_db,
            plan.key_text(),
            // Only an IR adds to the key, so every other sound keeps its cache entry.
            if settings.is_impulse_response() { ";ir" } else { "" }
        )
        .as_bytes(),
    )
    .as_bytes();
    let payload = regia_audio::cooked_sound::CookedSound {
        loudness: record,
        data: bytes.to_vec(),
    }
    .to_bytes()
    .map_err(|e| CookError::Audio(format!("{ext}: {e}")))?;
    Ok(AudioCook {
        settings_hash,
        id: meta.id,
        payload,
    })
}

/// Checks a sound marked as an impulse response: it decodes whole (a cut WAV is refused), is not
/// silent or non-finite, and fits the desktop tier at 48 kHz. The header's frame count is checked
/// against the tier before the decode, so a file that declares too many frames is refused
/// without decoding it. The error starts with the `regia_audio::ConvolutionError::code`.
///
/// # Errors
/// [`CookError::Audio`] naming the refusal.
pub fn check_impulse_response(bytes: &[u8]) -> Result<(), CookError> {
    // A header the probe cannot read falls through to the decode, which names the problem.
    if let Ok(header) = regia_audio::probe_sound_header(bytes, "impulse response")
        && let Some(frames) = header.frames
    {
        let tier = regia_audio::ConvolutionTier::Desktop;
        let cap = tier.max_ir_frames(48_000);
        // The length `ConvolutionKernel::prepare` resamples to: ceil(frames * 48000 / rate).
        let at_check_rate = (frames as f64 * 48_000.0 / f64::from(header.sample_rate)).ceil();
        if at_check_rate > cap as f64 {
            let e = regia_audio::ConvolutionError::TooLong {
                frames: at_check_rate as usize,
                cap,
                tier: tier.name(),
            };
            return Err(CookError::Audio(format!("{}: {e}", e.code())));
        }
    }
    let ir = regia_audio::ImpulseResponse::from_encoded_bytes(bytes)
        .map_err(|e| CookError::Audio(format!("{}: {e}", e.code())))?;
    regia_audio::ConvolutionKernel::prepare(&ir, 48_000, regia_audio::ConvolutionTier::Desktop)
        .map_err(|e| CookError::Audio(format!("{}: {e}", e.code())))?;
    Ok(())
}

/// The importer name the cook records for `.ambience.ron` files.
pub const AMBIENCE_IMPORTER: &str = "ambience";

/// The cooked type path of an ambience file (its validated RON text).
pub const AMBIENCE_TYPE: &str = "regia_audio::AmbienceDef";

/// True for an `.ambience.ron` source.
#[must_use]
pub fn is_ambience(source: &Path) -> bool {
    source
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| crate::cook::name_ends_with(n, ".ambience.ron"))
}

/// Validates an `.ambience.ron` (`regia_audio::AmbienceDef::from_ron`) and cooks it as its text:
/// `(id, settings hash, payload)`. A file that does not load fails the cook with the
/// `ambience_*` code, so a broken ambience never ships.
///
/// # Errors
/// [`CookError::Audio`] with the `ambience_*` code; a `.meta` error.
pub fn cook_ambience(source: &Path, bytes: &[u8]) -> Result<(AssetId, [u8; 32], Vec<u8>), CookError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| CookError::Audio(format!("ambience_parse: {}: not UTF-8 text", source.display())))?;
    regia_audio::AmbienceDef::from_ron(text)
        .map_err(|e| CookError::Audio(format!("{}: {}: {e}", e.code(), source.display())))?;
    let meta = AssetDatabase::load_or_create_meta(source, AMBIENCE_IMPORTER).map_err(CookError::Db)?;
    Ok((meta.id, *blake3::hash(bytes).as_bytes(), bytes.to_vec()))
}

/// Gathers every cooked sound's record into `<cache_dir>/loudness.ron` (written atomically), with
/// the project's policy. Sources no longer on disk are left out. Nothing is written when the
/// project has no sounds and no table yet. Returns the number of entries and the problems met
/// (an unreadable record is left out of the table and named).
///
/// # Errors
///
/// The table could not be serialized or written.
pub fn write_loudness_table(
    project_root: &Path,
    cache_dir: &Path,
    db: &AssetDatabase,
) -> Result<(usize, Vec<String>), String> {
    let _span = tracing::debug_span!(target: "regia_asset_pipeline", "write_loudness_table").entered();
    let (policy, problem) = load_loudness_policy(project_root);
    let mut problems: Vec<String> = problem.into_iter().collect();
    let mut table = LoudnessTable {
        policy,
        ..LoudnessTable::default()
    };
    for (rel_path, entry) in &db.entries {
        if entry.importer != AUDIO_IMPORTER || !project_root.join(rel_path).is_file() {
            continue;
        }
        match read_record(&cache_dir.join(&entry.cooked_rel)) {
            Ok(record) => {
                table.entries.insert(rel_path.clone(), record);
            }
            Err(e) => problems.push(format!("loudness table: {rel_path}: {e}")),
        }
    }
    let path = cache_dir.join(LOUDNESS_TABLE_FILE);
    if table.entries.is_empty() && !path.is_file() {
        return Ok((0, problems));
    }
    let text = table.to_ron_string().map_err(|e| e.to_string())?;
    crate::cook_ledger::write_atomic(&path, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok((table.entries.len(), problems))
}

/// Stages the cook's loudness table into a package (ADR-4898): `<cooked cache>/loudness.ron` of
/// `platform` is checked and written to `<assets_dir>/loudness.ron`, next to the sounds it names, so
/// a packaged game loads it with `LoudnessTable::load` (or `audio.set_loudness_normalization` with
/// `table: "assets/loudness.ron"`); the table's `assets/...` keys match the shipped files by path
/// suffix. Returns the staged path, or `None` when the cook wrote no table (no sounds). The write
/// is atomic (temporary file, then rename); nothing is created on an error.
///
/// # Errors
///
/// The message starts with its code: `loudness_table_invalid` (the cooked table does not read),
/// `loudness_table_conflict` (a file is already staged at that path, such as a raw
/// `assets/loudness.ron` of the project) or `loudness_table_write` (the staged file could not be
/// written).
pub fn stage_loudness_table(
    project_root: &Path,
    platform: &str,
    assets_dir: &Path,
) -> Result<Option<std::path::PathBuf>, String> {
    let _span = tracing::debug_span!(target: "regia_asset_pipeline", "stage_loudness_table").entered();
    let cooked = regia_asset::cooked_cache_dir(project_root, platform).join(LOUDNESS_TABLE_FILE);
    let bytes = match std::fs::read(&cooked) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("loudness_table_invalid: {}: {e}", cooked.display())),
    };
    let table = LoudnessTable::from_ron(&bytes)
        .map_err(|e| format!("loudness_table_invalid: {}: {e}", cooked.display()))?;
    if table.entries.is_empty() {
        return Ok(None);
    }
    let out = assets_dir.join(LOUDNESS_TABLE_FILE);
    let conflict = || {
        format!(
            "loudness_table_conflict: {} is already staged (a raw asset may not use the loudness table's name)",
            out.display()
        )
    };
    if out.exists() {
        return Err(conflict());
    }
    std::fs::create_dir_all(assets_dir)
        .map_err(|e| format!("loudness_table_write: {}: {e}", assets_dir.display()))?;
    match crate::cook_ledger::write_new_atomic(&out, &bytes) {
        Ok(true) => Ok(Some(out)),
        Ok(false) => Err(conflict()),
        Err(e) => Err(format!("loudness_table_write: {}: {e}", out.display())),
    }
}

fn read_record(path: &Path) -> Result<SoundLoudness, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let (header, payload) = decode_cooked_file(&bytes).map_err(|e| e.to_string())?;
    if header.type_tag == regia_core::id::TypePathHash::of(SOUND_LOUDNESS_TYPE) {
        return postcard::from_bytes(payload).map_err(|e| e.to_string());
    }
    regia_audio::cooked_sound::CookedSound::from_bytes(payload)
        .map(|sound| sound.loudness)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16-bit mono WAV of `samples` at 48 kHz.
    fn wav(samples: &[f32]) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&48_000u32.to_le_bytes());
        out.extend_from_slice(&96_000u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&((s * 30_000.0) as i16).to_le_bytes());
        }
        out
    }

    #[test]
    fn impulse_responses_are_checked_and_never_normalized() {
        let (s, p) = AudioImporterSettings::from_meta_string(Some("(impulse_response: Some(true))"));
        assert!(p.is_none() && s.is_impulse_response() && !s.normalize());
        let ir: Vec<f32> = (0..2_400)
            .map(|i| 0.8 * (-(i as f32) / 300.0).exp() * if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let good = wav(&ir);
        check_impulse_response(&good).expect("a decaying IR passes");
        let cut = check_impulse_response(&good[..good.len() - 100]).expect_err("cut");
        assert!(cut.to_string().contains("ir_truncated"), "{cut}");
        let silent = check_impulse_response(&wav(&[0.0; 64])).expect_err("silent");
        assert!(silent.to_string().contains("ir_silent"), "{silent}");
        let long = check_impulse_response(&wav(&vec![0.1; 48_000 * 5])).expect_err("5 s");
        assert!(long.to_string().contains("ir_too_long"), "{long}");
    }

    /// A 16-bit mono WAV at `rate` whose header declares `declared_frames` frames, followed by
    /// only the `present` samples.
    fn wav_declaring(rate: u32, declared_frames: u32, present: &[f32]) -> Vec<u8> {
        let data_len = declared_frames * 2;
        let mut out = wav(&[]);
        out[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
        out[24..28].copy_from_slice(&rate.to_le_bytes());
        out[28..32].copy_from_slice(&(rate * 2).to_le_bytes());
        out[40..44].copy_from_slice(&data_len.to_le_bytes());
        for s in present {
            out.extend_from_slice(&((s * 30_000.0) as i16).to_le_bytes());
        }
        out
    }

    /// The v05-rc1-editor note (finding 12): an IR was decoded whole before the tier check. The
    /// header's frame count is now checked against the desktop tier first, so a file that
    /// declares 10 minutes is refused as `ir_too_long` from its header (its 1 s of data is never
    /// decoded; the decode would have refused it as `ir_truncated`).
    #[test]
    fn an_impulse_response_is_checked_against_the_tier_from_its_header() {
        let second: Vec<f32> = (0..48_000).map(|i| 0.5 * (-(i as f32) / 4_000.0).exp()).collect();
        let ten_minutes = check_impulse_response(&wav_declaring(48_000, 48_000 * 600, &second))
            .expect_err("10 minutes is past every tier");
        let text = ten_minutes.to_string();
        assert!(
            text.contains("ir_too_long"),
            "the header check refuses first: {text}"
        );
        assert!(
            text.contains("28800000"),
            "the header's frame count is named: {text}"
        );
        // At 96 kHz the header's frames are counted at the 48 kHz check rate: 5 s (480 000
        // frames, 240 000 at 48 kHz) is past the 4 s desktop cap.
        let fast = check_impulse_response(&wav_declaring(96_000, 96_000 * 5, &second[..9_600]))
            .expect_err("5 s at 96 kHz");
        assert!(fast.to_string().contains("ir_too_long"), "{fast}");
        assert!(fast.to_string().contains("240000"), "{fast}");
        // A header within the cap still decodes (and the cut is still caught by the decode).
        let cut = check_impulse_response(&wav_declaring(48_000, 48_000 * 2, &second))
            .expect_err("2 s declared, 1 s present");
        assert!(cut.to_string().contains("ir_truncated"), "{cut}");
    }

    #[test]
    fn ambience_files_are_validated_at_cook() {
        let dir = regia_test_utils::unique_temp_dir("ambience-cook");
        let good = dir.join("glade.ambience.ron");
        let text = include_str!("../../../docs/formats/glade.ambience.ron");
        std::fs::write(&good, text).expect("write");
        assert!(is_ambience(&good));
        assert!(!is_ambience(&dir.join("glade.music.ron")));
        let (_, _, payload) = cook_ambience(&good, text.as_bytes()).expect("cooks");
        assert_eq!(payload, text.as_bytes());
        let bad = dir.join("broken.ambience.ron");
        let broken = "( version: 1, name: \"x\", one_shots: [( name: \"a\", sounds: [], interval_secs: (1.0, 2.0), distance_m: (1.0, 2.0) )] )";
        std::fs::write(&bad, broken).expect("write");
        let error = cook_ambience(&bad, broken.as_bytes()).expect_err("no sounds");
        assert!(error.to_string().contains("ambience_invalid"), "{error}");
    }

    #[test]
    fn settings_parse_and_fall_back() {
        assert_eq!(
            AudioImporterSettings::from_meta_string(None),
            (AudioImporterSettings::default(), None)
        );
        let (s, p) = AudioImporterSettings::from_meta_string(Some("(normalize: Some(false))"));
        assert!(!s.normalize() && p.is_none());
        let (s, p) = AudioImporterSettings::from_meta_string(Some("(target_lufs: Some(-18.0))"));
        assert_eq!(s.target_lufs, Some(-18.0));
        assert!(s.normalize() && p.is_none());
        let (s, p) = AudioImporterSettings::from_meta_string(Some("(target_lufs: Some(5.0))"));
        assert_eq!(s, AudioImporterSettings::default());
        assert!(p.expect("problem").contains("outside"));
        let (s, p) = AudioImporterSettings::from_meta_string(Some("(normalize: maybe)"));
        assert_eq!(s, AudioImporterSettings::default());
        assert!(p.is_some());
    }

    #[test]
    fn the_policy_file_is_optional_and_checked() {
        let root = regia_test_utils::unique_temp_dir("loudness-policy");
        assert_eq!(load_loudness_policy(&root), (LoudnessPolicy::default(), None));
        std::fs::write(root.join(LOUDNESS_CONFIG_FILE), "target_lufs = -16.0\n").expect("write");
        let (policy, problem) = load_loudness_policy(&root);
        assert_eq!(policy.target_lufs, -16.0);
        assert_eq!(
            policy.true_peak_ceiling_db,
            regia_audio::DEFAULT_TRUE_PEAK_CEILING_DB
        );
        assert!(problem.is_none());
        std::fs::write(root.join(LOUDNESS_CONFIG_FILE), "target_lufs = 4.0\n").expect("write");
        let (policy, problem) = load_loudness_policy(&root);
        assert_eq!(policy, LoudnessPolicy::default());
        assert!(problem.expect("problem").contains("outside"));
        std::fs::write(root.join(LOUDNESS_CONFIG_FILE), "target = -16.0\n").expect("write");
        assert!(
            load_loudness_policy(&root).1.is_some(),
            "unknown keys are refused"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn audio_extensions() {
        for ext in ["wav", "ogg", "flac", "mp3"] {
            assert!(is_audio_extension(ext));
        }
        assert!(!is_audio_extension("png"));
    }
}
