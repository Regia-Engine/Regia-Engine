// SPDX-License-Identifier: MPL-2.0
//! The audio cook policy and Ogg Vorbis transcode, end to end through `cook_project`
//! (`docs/specs/16-audio.md` "Audio cook", ADR-3230).

use std::path::Path;

use regia_asset::{MetaSidecar, cooked_cache_dir, meta_path_for};
use regia_asset_pipeline::audio_transcode::{
    AudioCookMode, AudioCookPolicy, AudioShipPlan, DEFAULT_ANDROID_QUALITY, DEFAULT_QUALITY,
    load_audio_cook_policy, resolve_plan, shipped_audio_bytes, transcode_to_vorbis,
};
use regia_asset_pipeline::{CookOptions, CookReport, cook_project, host_platform};
use regia_audio::{LOUDNESS_TABLE_FILE, LoudnessTable};

const RATE: u32 = 44_100;

/// A tone with two harmonics and a slow beat, so the codec has real work to do.
fn signal(seconds: f64) -> Vec<f32> {
    let len = (seconds * f64::from(RATE)) as usize;
    (0..len)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            0.3 * (t * 440.0 * std::f32::consts::TAU).sin()
                + 0.15 * (t * 1_320.0 * std::f32::consts::TAU).sin()
                + 0.05 * (t * 3_100.0 * std::f32::consts::TAU).sin() * (t * 3.0).sin()
        })
        .collect()
}

/// PCM16 WAV bytes of `samples` (mono).
fn wav(samples: &[f32]) -> Vec<u8> {
    let data: Vec<u8> = samples
        .iter()
        .flat_map(|s| ((s * 32_767.0) as i16).to_le_bytes())
        .collect();
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

fn decode(bytes: &[u8]) -> (Vec<kira::Frame>, u32) {
    let data = kira::sound::static_sound::StaticSoundData::from_cursor(std::io::Cursor::new(bytes.to_vec()))
        .expect("decodes through the runtime decoder");
    (data.frames.to_vec(), data.sample_rate)
}

fn project(extra_toml: Option<&str>) -> std::path::PathBuf {
    let root = regia_test_utils::unique_temp_dir("audio-transcode");
    std::fs::create_dir_all(root.join("assets/sfx")).expect("dirs");
    std::fs::write(root.join("assets/sfx/a.wav"), wav(&signal(1.5))).expect("a");
    let louder: Vec<f32> = signal(1.0).iter().map(|s| s * 0.5).collect();
    std::fs::write(root.join("assets/sfx/b.wav"), wav(&louder)).expect("b");
    if let Some(toml) = extra_toml {
        std::fs::write(root.join("audio_cook.toml"), toml).expect("policy");
    }
    root
}

fn cook_for(root: &Path, platform: &str) -> CookReport {
    let report = cook_project(CookOptions {
        project_root: root.to_path_buf(),
        platform: platform.to_string(),
        incremental: true,
    })
    .expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    report
}

fn set_meta(root: &Path, rel: &str, settings: &str) {
    let path = meta_path_for(&root.join(rel));
    let mut meta = MetaSidecar::from_ron(&std::fs::read(&path).expect("meta")).expect("parse");
    meta.settings = Some(settings.into());
    std::fs::write(&path, meta.to_ron_string().expect("ron")).expect("write meta");
}

fn ship(root: &Path, platform: &str, name: &str) -> Option<Vec<u8>> {
    shipped_audio_bytes(
        root,
        platform,
        &root.join("assets/sfx").join(name),
        &format!("assets/sfx/{name}"),
    )
    .expect("ship")
}

#[test]
fn a_wav_becomes_a_smaller_ogg_that_decodes_with_the_same_length_and_a_high_snr() {
    let root = project(Some("mode = \"vorbis\"\n"));
    cook_for(&root, host_platform());
    let source = std::fs::read(root.join("assets/sfx/a.wav")).expect("source");
    let ogg = ship(&root, host_platform(), "a.wav").expect("transcoded");
    assert_eq!(&ogg[..4], b"OggS");
    assert!(ogg.len() * 4 < source.len(), "{} vs {}", ogg.len(), source.len());
    let (src, src_rate) = decode(&source);
    let (out, out_rate) = decode(&ogg);
    assert_eq!(src_rate, out_rate);
    assert!(
        src.len().abs_diff(out.len()) <= 2_048,
        "{} vs {}",
        src.len(),
        out.len()
    );
    // SNR against the source, after removing the codec's constant delay (none for Vorbis in the
    // decoder, so compare sample for sample); floor 20 dB at the default quality.
    let n = src.len().min(out.len());
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for i in 0..n {
        let s = f64::from(src[i].left);
        let d = s - f64::from(out[i].left);
        sig += s * s;
        err += d * d;
    }
    let snr = 10.0 * (sig / err.max(1e-30)).log10();
    assert!(snr > 20.0, "snr {snr} dB");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn android_uses_its_own_quality() {
    let root = project(Some("mode = \"vorbis\"\nquality = 0.8\nandroid_quality = 0.0\n"));
    let desktop = ship(&root, "linux", "a.wav").expect("desktop");
    let android = ship(&root, "android", "a.wav").expect("android");
    assert!(
        android.len() < desktop.len(),
        "{} vs {}",
        android.len(),
        desktop.len()
    );
    let (policy, problem) = load_audio_cook_policy(&root);
    assert!(problem.is_none());
    let sound = Default::default();
    assert_eq!(
        resolve_plan(&policy, &sound, "wav", "android"),
        AudioShipPlan::Vorbis { quality: 0.0 }
    );
    assert_eq!(
        resolve_plan(&policy, &sound, "wav", "linux"),
        AudioShipPlan::Vorbis { quality: 0.8 }
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_defaults_keep_the_source_and_compressed_sources_pass_through_unless_forced() {
    let policy = AudioCookPolicy::default();
    assert_eq!(policy.mode, AudioCookMode::Keep);
    let sound = Default::default();
    assert_eq!(resolve_plan(&policy, &sound, "wav", "linux"), AudioShipPlan::Keep);
    let vorbis = AudioCookPolicy {
        mode: AudioCookMode::Vorbis,
        ..policy
    };
    for ext in ["ogg", "mp3"] {
        assert_eq!(
            resolve_plan(&vorbis, &sound, ext, "linux"),
            AudioShipPlan::Keep,
            "{ext}"
        );
    }
    for ext in ["wav", "flac"] {
        assert_eq!(
            resolve_plan(&vorbis, &sound, ext, "linux"),
            AudioShipPlan::Vorbis {
                quality: DEFAULT_QUALITY
            }
        );
        assert_eq!(
            resolve_plan(&vorbis, &sound, ext, "android"),
            AudioShipPlan::Vorbis {
                quality: DEFAULT_ANDROID_QUALITY
            }
        );
    }
    let forced = AudioCookPolicy {
        force: true,
        ..vorbis
    };
    assert!(matches!(
        resolve_plan(&forced, &sound, "mp3", "linux"),
        AudioShipPlan::Vorbis { .. }
    ));

    // End to end: an .ogg source ships unchanged, and ships re-encoded with force.
    let root = project(Some("mode = \"vorbis\"\n"));
    let ogg = transcode_to_vorbis(
        &std::fs::read(root.join("assets/sfx/a.wav")).expect("a"),
        "a",
        0.4,
    )
    .expect("enc");
    std::fs::write(root.join("assets/sfx/c.ogg"), &ogg).expect("c");
    assert!(ship(&root, "linux", "c.ogg").is_none(), "an ogg ships as it is");
    std::fs::write(root.join("audio_cook.toml"), "mode = \"vorbis\"\nforce = true\n").expect("policy");
    assert!(ship(&root, "linux", "c.ogg").is_some(), "force re-encodes it");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn changing_the_policy_recooks_only_the_sounds_it_affects() {
    let root = project(Some("mode = \"vorbis\"\nquality = 0.4\n"));
    cook_for(&root, host_platform());
    set_meta(&root, "assets/sfx/b.wav", "(mode: Some(keep))");
    // b's own settings changed: only b is recooked.
    let report = cook_for(&root, host_platform());
    assert_eq!(report.cooked, 1, "{report:?}");
    // Project quality changes: a (vorbis) is recooked; b (keep) is not.
    std::fs::write(root.join("audio_cook.toml"), "mode = \"vorbis\"\nquality = 0.6\n").expect("policy");
    let report = cook_for(&root, host_platform());
    assert_eq!(report.cooked, 1, "{report:?}");
    // Nothing changed: nothing is recooked.
    let report = cook_for(&root, host_platform());
    assert_eq!(report.cooked, 0, "{report:?}");
    // Only a has a cache file, and only one generation of it.
    let cache = cooked_cache_dir(&root, host_platform()).join("audio");
    assert_eq!(std::fs::read_dir(cache).expect("cache").count(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_loudness_gain_is_the_same_under_keep_and_vorbis() {
    let table = |root: &Path| {
        LoudnessTable::load(cooked_cache_dir(root, host_platform()).join(LOUDNESS_TABLE_FILE)).expect("table")
    };
    let keep = project(None);
    cook_for(&keep, host_platform());
    let vorbis = project(Some("mode = \"vorbis\"\n"));
    cook_for(&vorbis, host_platform());
    let (k, v) = (table(&keep), table(&vorbis));
    assert_eq!(k.entries.len(), 2);
    for (path, record) in &k.entries {
        assert_eq!(record, &v.entries[path], "{path}");
    }
    let _ = std::fs::remove_dir_all(&keep);
    let _ = std::fs::remove_dir_all(&vorbis);
}

#[test]
fn two_transcodes_are_byte_identical() {
    let source = wav(&signal(1.0));
    let a = transcode_to_vorbis(&source, "a", 0.4).expect("first");
    let b = transcode_to_vorbis(&source, "a", 0.4).expect("second");
    assert_eq!(a, b);
    // Cooks into two different trees agree too.
    let one = project(Some("mode = \"vorbis\"\n"));
    let two = project(Some("mode = \"vorbis\"\n"));
    assert_eq!(ship(&one, "linux", "a.wav"), ship(&two, "linux", "a.wav"));
    let _ = std::fs::remove_dir_all(&one);
    let _ = std::fs::remove_dir_all(&two);
}

#[test]
fn a_corrupt_source_fails_with_the_audio_import_error() {
    let root = project(Some("mode = \"vorbis\"\n"));
    std::fs::write(root.join("assets/sfx/bad.wav"), b"RIFF not really a wav").expect("bad");
    let report = cook_project(CookOptions {
        project_root: root.clone(),
        platform: host_platform().to_string(),
        incremental: true,
    })
    .expect("cook");
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(
        report.errors[0].1.starts_with("audio import error"),
        "{:?}",
        report.errors
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_bad_policy_file_falls_back_to_the_defaults_with_a_problem() {
    let root = project(Some("quality = 7.0\n"));
    let (policy, problem) = load_audio_cook_policy(&root);
    assert_eq!(policy, AudioCookPolicy::default());
    assert!(problem.expect("problem").contains("outside"));
    std::fs::write(root.join("audio_cook.toml"), "mod = \"vorbis\"\n").expect("policy");
    assert!(
        load_audio_cook_policy(&root).1.is_some(),
        "unknown keys are refused"
    );
    let _ = std::fs::remove_dir_all(&root);
}
