// SPDX-License-Identifier: MPL-2.0
//! The audio cook policy and the Ogg Vorbis transcode (`docs/specs/16-audio.md` § "Audio cook",
//! ADR-3230).
//!
//! A sound ships as its source file (`keep`, the default) or as Ogg Vorbis (`vorbis`). The policy
//! is the project table `<project>/audio_cook.toml` ([`AudioCookPolicy`]) with the per-sound `.meta`
//! override ([`crate::audio_cook::AudioImporterSettings`]). Already-compressed sources (`.ogg`,
//! `.mp3`) are kept unless `force` is set. The encoder is libvorbis (aoTuV/Lancer) through the
//! `vorbis_rs` binding, pipeline-only (ADR-1593).
//!
//! The transcoded bytes keep the sound's path and asset id: the shipped file has the source's name
//! (a `door.wav` ships Ogg Vorbis bytes under that name) so game code and the loudness table need
//! no change; the runtime decoder probes the container by content. The loudness record is measured
//! on the **source** (the cook decodes the source it was given), so the normalization gain is
//! identical under `keep` and `vorbis`. The transcode is cached in
//! `<cooked cache>/audio/<path hash>-<key>.ogg`; the key covers the source bytes, the quality, the
//! channel layout and [`ENCODER_VERSION`].
//!
//! The encoder is the `audio_transcode` feature (on by default, ADR-7972). Without it the policy,
//! the plan and the ship report stay, a `keep` sound cooks as before, and a sound whose plan is
//! `vorbis` fails its cook with [`AUDIO_TRANSCODE_DISABLED_CODE`] (never shipped untranscoded).

#[cfg(feature = "audio_transcode")]
use std::num::{NonZeroU8, NonZeroU32};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
#[cfg(feature = "audio_transcode")]
use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoderBuilder};

use crate::audio_cook::AudioImporterSettings;

/// The project's audio cook policy file, at the project root.
pub const AUDIO_COOK_CONFIG_FILE: &str = "audio_cook.toml";

/// Identifies the encoder build and the transcode recipe; part of every cache key, so changing
/// either re-encodes.
pub const ENCODER_VERSION: &str = "vorbis_rs-0.5.6/aotuv-lancer-b6.03;regia-transcode-1";

/// Default Vorbis quality on desktop (libvorbis scale: about 128 kbit/s stereo).
pub const DEFAULT_QUALITY: f32 = 0.4;

/// Default Vorbis quality on Android (about 96 kbit/s stereo).
pub const DEFAULT_ANDROID_QUALITY: f32 = 0.3;

/// Allowed quality range (libvorbis accepts -0.2..=1.0; the cook stops at -0.1).
pub const QUALITY_RANGE: (f32, f32) = (-0.1, 1.0);

/// Subdirectory of the cooked cache holding the transcoded files.
pub const TRANSCODE_CACHE_DIR: &str = "audio";

/// The error code a `vorbis` sound's cook fails with in a build without the `audio_transcode`
/// feature (ADR-7972).
pub const AUDIO_TRANSCODE_DISABLED_CODE: &str = "audio_transcode_disabled";

/// Fixed Ogg stream serial ("RGVB"): the output is a pure function of the key.
#[cfg(feature = "audio_transcode")]
const STREAM_SERIAL: i32 = 0x5247_5642;

/// Frames handed to the encoder per block (libvorbis recommends a few thousand).
#[cfg(feature = "audio_transcode")]
const BLOCK_FRAMES: usize = 4096;

/// How a sound is shipped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioCookMode {
    /// Ship the source file as it is (the default).
    #[default]
    Keep,
    /// Ship Ogg Vorbis.
    Vorbis,
}

/// `<project>/audio_cook.toml`: the project-wide defaults.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AudioCookPolicy {
    /// The mode of every sound that does not say otherwise. Default `keep`.
    pub mode: AudioCookMode,
    /// Vorbis quality on desktop, [`QUALITY_RANGE`]. Default [`DEFAULT_QUALITY`].
    pub quality: f32,
    /// Vorbis quality on Android. Default [`DEFAULT_ANDROID_QUALITY`].
    pub android_quality: f32,
    /// Transcode `.ogg` and `.mp3` sources too (a lossy-to-lossy generation). Default off.
    pub force: bool,
}

impl Default for AudioCookPolicy {
    fn default() -> Self {
        Self {
            mode: AudioCookMode::Keep,
            quality: DEFAULT_QUALITY,
            android_quality: DEFAULT_ANDROID_QUALITY,
            force: false,
        }
    }
}

/// True when `quality` is a usable Vorbis quality.
#[must_use]
pub fn quality_ok(quality: f32) -> bool {
    quality.is_finite() && (QUALITY_RANGE.0..=QUALITY_RANGE.1).contains(&quality)
}

/// The project's policy from `<project>/audio_cook.toml`, and its problem (malformed, unknown key,
/// quality out of range; the defaults then apply). No file is the defaults and no problem.
#[must_use]
pub fn load_audio_cook_policy(project_root: &Path) -> (AudioCookPolicy, Option<String>) {
    let Ok(text) = std::fs::read_to_string(project_root.join(AUDIO_COOK_CONFIG_FILE)) else {
        return (AudioCookPolicy::default(), None);
    };
    let policy: AudioCookPolicy = match toml::from_str(&text) {
        Ok(policy) => policy,
        Err(e) => {
            return (
                AudioCookPolicy::default(),
                Some(format!("{AUDIO_COOK_CONFIG_FILE}: {e}; using the defaults")),
            );
        }
    };
    for (name, q) in [
        ("quality", policy.quality),
        ("android_quality", policy.android_quality),
    ] {
        if !quality_ok(q) {
            return (
                AudioCookPolicy::default(),
                Some(format!(
                    "{AUDIO_COOK_CONFIG_FILE}: {name} {q} is outside {}..={}; using the defaults",
                    QUALITY_RANGE.0, QUALITY_RANGE.1
                )),
            );
        }
    }
    (policy, None)
}

/// What the cook does with one sound on one platform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioShipPlan {
    /// Ship the source bytes.
    Keep,
    /// Ship Ogg Vorbis at this quality.
    Vorbis {
        /// Vorbis quality, [`QUALITY_RANGE`].
        quality: f32,
    },
}

impl AudioShipPlan {
    /// The cache-key text of the plan: empty for `keep`, so a `keep` sound's cook key does not move
    /// when the transcode settings change.
    #[must_use]
    pub fn key_text(&self) -> String {
        match self {
            Self::Keep => String::new(),
            Self::Vorbis { quality } => format!(";ship=vorbis;q={quality:.3};enc={ENCODER_VERSION}"),
        }
    }
}

/// Resolves the plan for a sound of extension `ext` (lowercase) on `platform`.
#[must_use]
pub fn resolve_plan(
    policy: &AudioCookPolicy,
    sound: &AudioImporterSettings,
    ext: &str,
    platform: &str,
) -> AudioShipPlan {
    let mode = sound.mode.unwrap_or(policy.mode);
    let force = sound.force.unwrap_or(policy.force);
    let compressed = matches!(ext, "ogg" | "mp3");
    if mode == AudioCookMode::Keep || (compressed && !force) {
        return AudioShipPlan::Keep;
    }
    let quality = if platform == "android" {
        sound.android_quality.unwrap_or(policy.android_quality)
    } else {
        sound.quality.unwrap_or(policy.quality)
    };
    AudioShipPlan::Vorbis { quality }
}

/// Decodes `bytes` with the runtime decoder and encodes Ogg Vorbis at `quality`. A sound whose
/// every frame has equal left and right samples (a mono source, which the runtime decoder expands)
/// is encoded as one channel; anything else as two.
///
/// # Errors
///
/// The bytes do not decode, hold no frame, or the encoder failed; in a build without the
/// `audio_transcode` feature, always (the message starts with [`AUDIO_TRANSCODE_DISABLED_CODE`]).
pub fn transcode_to_vorbis(bytes: &[u8], label: &str, quality: f32) -> Result<Vec<u8>, String> {
    let _span = tracing::info_span!(target: "regia_asset_pipeline", "transcode_vorbis", label).entered();
    #[cfg(not(feature = "audio_transcode"))]
    {
        let _ = (bytes, quality);
        Err(format!(
            "{AUDIO_TRANSCODE_DISABLED_CODE}: {label}: the Ogg Vorbis transcode is not built in (regia_asset_pipeline feature `audio_transcode` is off)"
        ))
    }
    #[cfg(feature = "audio_transcode")]
    {
        encode_vorbis(bytes, label, quality)
    }
}

/// The decode and the libvorbis encode of [`transcode_to_vorbis`].
#[cfg(feature = "audio_transcode")]
fn encode_vorbis(bytes: &[u8], label: &str, quality: f32) -> Result<Vec<u8>, String> {
    let data = kira::sound::static_sound::StaticSoundData::from_cursor(std::io::Cursor::new(bytes.to_vec()))
        .map_err(|e| format!("{label}: {e}"))?;
    let frames = &data.frames;
    if frames.is_empty() {
        return Err(format!("{label}: no audio frames"));
    }
    let rate = NonZeroU32::new(data.sample_rate).ok_or_else(|| format!("{label}: sample rate 0"))?;
    let mono = frames.iter().all(|f| f.left == f.right);
    let channels = NonZeroU8::new(if mono { 1 } else { 2 }).ok_or("channels")?;
    let mut builder = VorbisEncoderBuilder::new_with_serial(rate, channels, Vec::<u8>::new(), STREAM_SERIAL);
    builder.bitrate_management_strategy(VorbisBitrateManagementStrategy::QualityVbr {
        target_quality: quality,
    });
    let mut encoder = builder.build().map_err(|e| format!("{label}: encoder: {e}"))?;
    for block in frames.chunks(BLOCK_FRAMES) {
        let left: Vec<f32> = block.iter().map(|f| f.left).collect();
        if mono {
            encoder
                .encode_audio_block([left])
                .map_err(|e| format!("{label}: encode: {e}"))?;
        } else {
            let right: Vec<f32> = block.iter().map(|f| f.right).collect();
            encoder
                .encode_audio_block([left, right])
                .map_err(|e| format!("{label}: encode: {e}"))?;
        }
    }
    encoder.finish().map_err(|e| format!("{label}: finish: {e}"))
}

/// The cache file of a sound's transcode: `<cache>/audio/<hash of rel>-<key>.ogg`.
fn cache_file(cache_dir: &Path, rel_path: &str, source: &[u8], plan: AudioShipPlan) -> (PathBuf, String) {
    let rel_hash = blake3::hash(rel_path.as_bytes()).to_hex();
    let mut key = blake3::Hasher::new();
    key.update(source);
    key.update(plan.key_text().as_bytes());
    let key = key.finalize().to_hex();
    let prefix = rel_hash.as_str()[..16].to_string();
    let name = format!("{prefix}-{}.ogg", &key.as_str()[..32]);
    (cache_dir.join(TRANSCODE_CACHE_DIR).join(name), prefix)
}

/// Removes the cached transcodes of the sound with path-hash `prefix` from `dir`.
fn remove_cached(dir: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("{prefix}-"))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The transcoded bytes of `source` for `plan`, from the cache or encoded now (and cached, the
/// sound's older cache files removed). `Keep` returns `None`.
///
/// # Errors
///
/// The source does not decode, or the cache cannot be written.
pub fn transcode_cached(
    cache_dir: &Path,
    rel_path: &str,
    source: &[u8],
    plan: AudioShipPlan,
) -> Result<Option<Vec<u8>>, String> {
    let (file, prefix) = cache_file(cache_dir, rel_path, source, plan);
    let AudioShipPlan::Vorbis { quality } = plan else {
        // A sound switched back to `keep` leaves no stale transcode behind.
        remove_cached(&cache_dir.join(TRANSCODE_CACHE_DIR), &prefix);
        return Ok(None);
    };
    if let Ok(bytes) = std::fs::read(&file)
        && !bytes.is_empty()
    {
        return Ok(Some(bytes));
    }
    let bytes = transcode_to_vorbis(source, rel_path, quality)?;
    let dir = cache_dir.join(TRANSCODE_CACHE_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{prefix}-"))
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    crate::cook_ledger::write_atomic(&file, &bytes).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(Some(bytes))
}

/// The bytes a package ships for the sound at `source` (`rel_path` is its `assets/…` path):
/// `Ok(None)` when the source ships as it is, the Ogg Vorbis bytes when the policy transcodes it.
/// Reads the project policy and the sound's `.meta`; never writes a `.meta`.
///
/// # Errors
///
/// The source cannot be read or does not decode.
pub fn shipped_audio_bytes(
    project_root: &Path,
    platform: &str,
    source: &Path,
    rel_path: &str,
) -> Result<Option<Vec<u8>>, String> {
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !crate::audio_cook::is_audio_extension(&ext) {
        return Ok(None);
    }
    let (policy, _) = load_audio_cook_policy(project_root);
    let settings = std::fs::read(regia_asset::meta_path_for(source))
        .ok()
        .and_then(|b| regia_asset::MetaSidecar::from_ron(&b).ok())
        .map(|m| AudioImporterSettings::from_meta_string(m.settings.as_deref()).0)
        .unwrap_or_default();
    let plan = resolve_plan(&policy, &settings, &ext, platform);
    if plan == AudioShipPlan::Keep {
        return Ok(None);
    }
    let bytes = std::fs::read(source).map_err(|e| format!("{}: {e}", source.display()))?;
    let cache_dir = regia_asset::cooked_cache_dir(project_root, platform);
    transcode_cached(&cache_dir, rel_path, &bytes, plan)
}

/// File name of the per-cook audio ship report in the cooked cache.
pub const AUDIO_SHIP_REPORT_FILE: &str = "audio_ship.json";

/// One sound's line of the audio ship report: the policy it got and what it costs to ship.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioShipEntry {
    /// The sound's `assets/…` path.
    pub path: String,
    /// `keep` or `vorbis`.
    pub mode: String,
    /// Vorbis quality for `vorbis` entries.
    pub quality: Option<f32>,
    /// Size of the source file.
    pub source_bytes: u64,
    /// Size of what a package ships: the cached Ogg Vorbis for `vorbis` (the source size when the
    /// transcode is not cached yet), the source for `keep`.
    pub shipped_bytes: u64,
}

/// Builds the audio ship report for the sounds in `db` on `platform`, and writes it as
/// `<cache_dir>/audio_ship.json` when the project has any sound. Derived data: the next cook writes
/// it again.
///
/// # Errors
///
/// The report could not be written.
pub fn write_audio_ship_report(
    project_root: &Path,
    cache_dir: &Path,
    platform: &str,
    db: &crate::db::AssetDatabase,
) -> Result<Vec<AudioShipEntry>, String> {
    let (policy, _) = load_audio_cook_policy(project_root);
    let mut out = Vec::new();
    for (rel_path, entry) in &db.entries {
        let source = project_root.join(rel_path);
        if entry.importer != crate::audio_cook::AUDIO_IMPORTER || !source.is_file() {
            continue;
        }
        let ext = source
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let settings = std::fs::read(regia_asset::meta_path_for(&source))
            .ok()
            .and_then(|b| regia_asset::MetaSidecar::from_ron(&b).ok())
            .map(|m| AudioImporterSettings::from_meta_string(m.settings.as_deref()).0)
            .unwrap_or_default();
        let plan = resolve_plan(&policy, &settings, &ext, platform);
        let source_bytes = std::fs::metadata(&source).map_or(0, |m| m.len());
        let (mode, quality, shipped_bytes) = match plan {
            AudioShipPlan::Keep => ("keep", None, source_bytes),
            AudioShipPlan::Vorbis { quality } => {
                let cached = std::fs::read(&source)
                    .ok()
                    .map(|b| cache_file(cache_dir, rel_path, &b, plan).0)
                    .and_then(|f| std::fs::metadata(f).ok())
                    .map_or(source_bytes, |m| m.len());
                ("vorbis", Some(quality), cached)
            }
        };
        out.push(AudioShipEntry {
            path: rel_path.clone(),
            mode: mode.to_string(),
            quality,
            source_bytes,
            shipped_bytes,
        });
    }
    if !out.is_empty() {
        let text = serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?;
        crate::cook_ledger::write_atomic(&cache_dir.join(AUDIO_SHIP_REPORT_FILE), text.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    Ok(out)
}
