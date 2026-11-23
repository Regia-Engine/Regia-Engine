// SPDX-License-Identifier: MPL-2.0
//! Loudness at import, run-time half (`docs/specs/95-loudness-importers.md` § 2.5, ADR-1855):
//! measure two sound files the way the cook does, build a `loudness.ron` table, register the files,
//! flip the normalization switch and check the gains and the rendered loudness.

use regia_audio::{
    Audio, AudioSettings, LoudnessPolicy, LoudnessTable, PlaybackParams, REFERENCE_LUFS, SoundAssets,
    SoundLoudness, measure_loudness, measure_sound, measure_sound_bytes,
};

const RATE: u32 = 48_000;

/// WAV (PCM16 mono) bytes of a 1 kHz sine.
fn wav_bytes(seconds: f64, amp: f32) -> Vec<u8> {
    let len = (seconds * f64::from(RATE)) as usize;
    let mut data = Vec::with_capacity(len * 2);
    for i in 0..len {
        let s = ((i as f32 / RATE as f32) * 1000.0 * std::f32::consts::TAU).sin() * amp * 32_767.0;
        data.extend_from_slice(&(s as i16).to_le_bytes());
    }
    let mut wav = Vec::with_capacity(44 + data.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    wav
}

struct Project {
    root: std::path::PathBuf,
    quiet: std::path::PathBuf,
    loud: std::path::PathBuf,
    table: LoudnessTable,
}

fn project() -> Project {
    let root = regia_test_utils::unique_temp_dir("loudness-import");
    let sfx = root.join("assets/sfx");
    std::fs::create_dir_all(&sfx).expect("dir");
    let mut table = LoudnessTable::default();
    let mut paths = Vec::new();
    for (name, amp) in [("quiet.wav", 0.02f32), ("loud.wav", 0.5)] {
        let bytes = wav_bytes(1.5, amp);
        let path = sfx.join(name);
        std::fs::write(&path, &bytes).expect("write wav");
        let mut record = measure_sound_bytes(bytes, name).expect("decodes");
        record.apply_policy(&table.policy, true, None);
        table.entries.insert(format!("assets/sfx/{name}"), record);
        paths.push(path);
    }
    let loud = paths.pop().expect("loud");
    let quiet = paths.pop().expect("quiet");
    Project {
        root,
        quiet,
        loud,
        table,
    }
}

#[test]
fn a_mono_file_is_measured_as_the_dual_mono_frames_it_plays() {
    let bytes = wav_bytes(1.0, 0.1);
    let record = measure_sound_bytes(bytes.clone(), "tone").expect("decodes");
    assert!(record.dual_mono, "{record:?}");
    assert_eq!(record.sample_rate, RATE);
    assert_eq!(record.frames, u64::from(RATE));
    // The same tone measured as one channel reads 3.01 LU lower.
    let mono: Vec<f32> = (0..RATE as usize)
        .map(|i| ((i as f32 / RATE as f32) * 1000.0 * std::f32::consts::TAU).sin() * 0.1)
        .collect();
    let single = measure_loudness(&mono, 1, RATE).expect("valid").integrated_lufs;
    let played = record.integrated_lufs.expect("finite");
    assert!(
        (played - regia_audio::DUAL_MONO_OFFSET_LU - single).abs() < 0.05,
        "{played} vs {single}"
    );
}

#[test]
fn the_switch_gives_file_sounds_their_table_gain_and_takes_it_back() {
    let p = project();
    let mut sounds = SoundAssets::default();
    let quiet = sounds.add_file("quiet", &p.quiet).expect("quiet");
    let loud = sounds.add_file("loud", &p.loud).expect("loud");
    assert_eq!(
        sounds.get(&quiet).expect("sound").loudness_gain_db(),
        0.0,
        "off by default"
    );

    let changed = sounds.set_loudness_normalization(Some(p.table.clone()));
    assert_eq!(changed, 2);
    let quiet_gain = sounds.get(&quiet).expect("handle stays valid").loudness_gain_db();
    let loud_gain = sounds.get(&loud).expect("handle stays valid").loudness_gain_db();
    assert!(quiet_gain > 5.0 && loud_gain < -10.0, "{quiet_gain} {loud_gain}");
    assert_eq!(Some(quiet_gain), p.table.gain_db_for("assets/sfx/quiet.wav"));

    // Re-measuring the asset itself still reads the source (the gain is applied at play time).
    let raw = measure_sound(sounds.get(&quiet).expect("sound")).expect("measure");
    assert_eq!(
        raw.integrated_lufs,
        p.table.entries["assets/sfx/quiet.wav"].integrated_lufs
    );

    // A file registered after the switch gets its gain at once; a reload keeps it.
    let again = sounds.add_file("quiet2", &p.quiet).expect("quiet2");
    assert_eq!(sounds.get(&again).expect("sound").loudness_gain_db(), quiet_gain);
    assert_eq!(sounds.reload_file(&p.quiet).expect("reload"), 2);
    assert_eq!(sounds.get(&again).expect("sound").loudness_gain_db(), quiet_gain);

    // Turning it on again with the same table changes nothing; off returns every gain to 0.
    assert_eq!(sounds.set_loudness_normalization(Some(p.table.clone())), 0);
    assert_eq!(sounds.set_loudness_normalization(None), 3);
    assert_eq!(sounds.get(&loud).expect("sound").loudness_gain_db(), 0.0);
    assert!(sounds.loudness_normalization().is_none());
    let _ = std::fs::remove_dir_all(&p.root);
}

#[test]
fn normalized_renders_of_a_quiet_and_a_loud_file_land_on_the_target() {
    let p = project();
    let mut sounds = SoundAssets::default();
    sounds.add_file("quiet", &p.quiet).expect("quiet");
    sounds.add_file("loud", &p.loud).expect("loud");
    sounds.set_loudness_normalization(Some(p.table.clone()));
    let mut levels = Vec::new();
    for name in ["quiet", "loud"] {
        let mut audio = Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        });
        let asset = sounds.by_name(name).expect("registered").clone();
        audio
            .start_asset(&asset, PlaybackParams::default())
            .expect("play");
        // Static sounds: tick + render per 1/60 s frame, no pacing needed (no decoder thread).
        let mut out = Vec::new();
        for _ in 0..60 {
            audio.tick(1.0 / 60.0);
            out.extend_from_slice(&audio.render_offline(800).expect("render"));
        }
        let measured = measure_loudness(&out, 2, RATE).expect("valid").integrated_lufs;
        levels.push((name, measured));
    }
    for (name, lufs) in &levels {
        assert!((lufs - REFERENCE_LUFS).abs() < 0.5, "{name}: {lufs} ({levels:?})");
    }
    let _ = std::fs::remove_dir_all(&p.root);
}

#[test]
fn the_table_file_loads_and_a_missing_file_is_an_error() {
    let p = project();
    let path = p.root.join(regia_audio::LOUDNESS_TABLE_FILE);
    std::fs::write(&path, p.table.to_ron_string().expect("ron")).expect("write");
    assert_eq!(LoudnessTable::load(&path).expect("load"), p.table);
    assert!(LoudnessTable::load(p.root.join("nope.ron")).is_err());
    let policy = LoudnessPolicy::default();
    let record: &SoundLoudness = &p.table.entries["assets/sfx/loud.wav"];
    assert_eq!(record.target_lufs, policy.target_lufs);
    let _ = std::fs::remove_dir_all(&p.root);
}

/// `tests/fixtures/tone_1k.{flac,ogg,mp3}`: one second of a 1 kHz sine at amplitude 1/8, mono,
/// 48 kHz, written by `ffmpeg -f lavfi -i sine=frequency=1000:sample_rate=48000:duration=1`.
#[test]
fn every_lossy_and_lossless_source_format_measures_like_the_wav() {
    let wav = measure_sound_bytes(wav_bytes(1.0, 0.125), "wav").expect("wav decodes");
    let expected = wav.integrated_lufs.expect("loud enough");
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for name in ["tone_1k.flac", "tone_1k.ogg", "tone_1k.mp3"] {
        let bytes = std::fs::read(fixtures.join(name)).expect("fixture");
        let record = measure_sound_bytes(bytes, name).unwrap_or_else(|e| panic!("{name}: {e}"));
        let measured = record.integrated_lufs.unwrap_or_else(|| panic!("{name}: silent"));
        assert!(
            (measured - expected).abs() < 0.5,
            "{name}: {measured} vs {expected}"
        );
        assert_eq!(record.sample_rate, RATE, "{name}");
        assert!(record.dual_mono, "{name}: a mono source plays as dual mono");
        assert!(!record.short_clip, "{name}");
    }
}

/// The cook-time cost probe of spec 95 § 8: measures 60 s of 48 kHz stereo and prints the meter's
/// cost per second of audio. Prints only (no time limit in a test); run by hand with
/// `cargo test --release -p regia_audio --test loudness_import -- --ignored --nocapture cost`.
#[test]
#[ignore = "cost probe: prints a timing, asserts none"]
fn cost_probe_measuring_a_minute_of_stereo() {
    let seconds = 60usize;
    let samples: Vec<f32> = (0..seconds * RATE as usize * 2)
        .map(|i| ((i / 2) as f32 / RATE as f32 * 1000.0 * std::f32::consts::TAU).sin() * 0.25)
        .collect();
    let start = std::time::Instant::now();
    let measured = measure_loudness(&samples, 2, RATE).expect("valid");
    let spent = start.elapsed().as_secs_f64();
    assert!(measured.integrated_lufs.is_finite());
    println!(
        "measure_loudness: {seconds} s of 48 kHz stereo in {:.1} ms ({:.3} % of real time)",
        spent * 1e3,
        spent / seconds as f64 * 100.0
    );
}

/// Spec 95 § 2.5: the per-asset gain is 0 by default, a non-finite value is refused (counted) and
/// leaves 0 dB, and a finite one is clamped to ±`MAX_NORMALIZE_GAIN_DB` like the cook's gain.
#[test]
fn a_sound_refuses_a_non_finite_loudness_gain_and_clamps_the_rest() {
    let clip = regia_audio::SoundClip::new(vec![0.1f32; 4_800], RATE).expect("clip");
    let sound = regia_audio::SoundAsset::from_clip(&clip);
    assert_eq!(sound.loudness_gain_db(), 0.0, "no gain by default");
    assert_eq!(sound.clone().with_loudness_gain_db(-7.5).loudness_gain_db(), -7.5);
    let rejected_before = regia_audio::rejected_value_count();
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let refused = sound
            .clone()
            .with_loudness_gain_db(12.0)
            .with_loudness_gain_db(bad);
        assert_eq!(refused.loudness_gain_db(), 0.0, "{bad}: refused, 0 dB");
    }
    assert!(
        regia_audio::rejected_value_count() >= rejected_before + 3,
        "each refusal is counted"
    );
    let max = regia_audio::MAX_NORMALIZE_GAIN_DB;
    assert_eq!(sound.clone().with_loudness_gain_db(1e6).loudness_gain_db(), max);
    assert_eq!(sound.with_loudness_gain_db(-1e6).loudness_gain_db(), -max);
}
