// SPDX-License-Identifier: MPL-2.0
//! Loudness at import and the DDS, TGA and BMP importers, end to end through `cook_project`
//! (`docs/specs/95-loudness-importers.md` § 6, ADR-1855).

use std::path::Path;
use std::time::{Duration, SystemTime};

use regia_asset::{
    AssetManifest, CookedImage, CookedTexture, MetaSidecar, cooked_cache_dir, cooked_file_name,
    decode_cooked_file, meta_path_for,
};
use regia_asset_pipeline::{CookOptions, CookReport, cook_project, host_platform};
use regia_audio::{LOUDNESS_TABLE_FILE, LoudnessTable, REFERENCE_LUFS, db_to_gain, measure_loudness};

const RATE: u32 = 48_000;

fn sine(seconds: f64, amp: f32) -> Vec<f32> {
    let len = (seconds * f64::from(RATE)) as usize;
    (0..len)
        .map(|i| ((i as f32 / RATE as f32) * 1000.0 * std::f32::consts::TAU).sin() * amp)
        .collect()
}

/// WAV (PCM16, `channels` interleaved copies) bytes of `samples`.
fn wav(samples: &[f32], channels: u16) -> Vec<u8> {
    let mut data = Vec::with_capacity(samples.len() * 2 * usize::from(channels));
    for s in samples {
        for _ in 0..channels {
            data.extend_from_slice(&((s * 32_767.0) as i16).to_le_bytes());
        }
    }
    let block = 2 * u32::from(channels);
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * block).to_le_bytes());
    out.extend_from_slice(&(block as u16).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

/// A legacy-FourCC DDS header plus `levels`.
fn dds(width: u32, height: u32, fourcc: &[u8; 4], levels: &[Vec<u8>]) -> Vec<u8> {
    let mut h = vec![0u8; 124];
    let put = |h: &mut Vec<u8>, at: usize, v: u32| h[at..at + 4].copy_from_slice(&v.to_le_bytes());
    put(&mut h, 0, 124);
    put(&mut h, 4, 0x1007 | if levels.len() > 1 { 0x2_0000 } else { 0 });
    put(&mut h, 8, height);
    put(&mut h, 12, width);
    put(&mut h, 24, levels.len() as u32);
    put(&mut h, 72, 32);
    put(&mut h, 76, 0x4);
    h[80..84].copy_from_slice(fourcc);
    put(&mut h, 104, 0x1000);
    let mut out = b"DDS ".to_vec();
    out.extend_from_slice(&h);
    for level in levels {
        out.extend_from_slice(level);
    }
    out
}

/// BC1 blocks for a `w × h` level whose two endpoint colors vary per block (so the levels
/// differ and the pass-through check means something).
fn bc1_level(w: u32, h: u32, seed: u16) -> Vec<u8> {
    let blocks = w.div_ceil(4) * h.div_ceil(4);
    let mut out = Vec::new();
    for b in 0..blocks {
        let c0 = 0xF800u16 ^ (b as u16).wrapping_mul(37) ^ seed;
        out.extend_from_slice(&c0.to_le_bytes());
        out.extend_from_slice(&0x001Fu16.to_le_bytes());
        out.extend_from_slice(&(0x1B1B_1B1Bu32 ^ b).to_le_bytes());
    }
    out
}

fn rgba_pattern(w: u32, h: u32) -> image::RgbaImage {
    image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 8) as u8, (y * 8) as u8, 128, 255]))
}

fn encoded(img: &image::RgbaImage, format: image::ImageFormat) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, format).expect("encode");
    out.into_inner()
}

fn write_project(root: &Path) {
    let sfx = root.join("assets/sfx");
    let tex = root.join("assets/tex");
    std::fs::create_dir_all(&sfx).expect("sfx");
    std::fs::create_dir_all(&tex).expect("tex");
    std::fs::write(sfx.join("quiet.wav"), wav(&sine(1.5, 0.02), 1)).expect("quiet");
    std::fs::write(sfx.join("loud.wav"), wav(&sine(1.5, 0.5), 2)).expect("loud");
    std::fs::write(sfx.join("blip.wav"), wav(&sine(0.1, 0.3), 1)).expect("blip");
    let chain: Vec<Vec<u8>> = (0..5)
        .map(|i| bc1_level((16 >> i).max(1), (16 >> i).max(1), i as u16))
        .collect();
    std::fs::write(tex.join("brick.dds"), dds(16, 16, b"DXT1", &chain)).expect("dds");
    let single = vec![vec![0x55u8; 16 * 16]]; // BC3: 16 blocks of 16 bytes
    std::fs::write(tex.join("flat.dds"), dds(16, 16, b"DXT5", &single)).expect("dds");
    std::fs::write(
        tex.join("sign.tga"),
        encoded(&rgba_pattern(16, 16), image::ImageFormat::Tga),
    )
    .expect("tga");
    std::fs::write(
        tex.join("panel.bmp"),
        encoded(&rgba_pattern(16, 16), image::ImageFormat::Bmp),
    )
    .expect("bmp");
}

fn age_assets(root: &Path) {
    let then = SystemTime::now() - Duration::from_secs(3600);
    for entry in walk(&root.join("assets")) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&entry)
            .expect("open");
        file.set_modified(then).expect("set mtime");
    }
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).expect("dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

fn cook(root: &Path) -> CookReport {
    let report = cook_project(CookOptions {
        project_root: root.to_path_buf(),
        platform: host_platform().to_string(),
        incremental: true,
    })
    .expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    report
}

fn table(root: &Path) -> LoudnessTable {
    let cache = cooked_cache_dir(root, host_platform());
    LoudnessTable::load(cache.join(LOUDNESS_TABLE_FILE)).expect("loudness table")
}

/// The cooked payload of `rel` (`assets/…`).
fn payload(root: &Path, rel: &str) -> (String, Vec<u8>) {
    let cache = cooked_cache_dir(root, host_platform());
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("a manifest");
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.path == rel)
        .expect("manifest entry");
    let bytes = std::fs::read(cache.join(cooked_file_name(entry.id))).expect("cooked file");
    let (_, payload) = decode_cooked_file(&bytes).expect("decode");
    (entry.type_path.clone(), payload.to_vec())
}

#[test]
fn sounds_cook_to_a_loudness_table_that_brings_them_to_the_target() {
    let root = regia_test_utils::unique_temp_dir("loudness-cook");
    write_project(&root);
    let report = cook(&root);
    assert_eq!(report.cooked, 7, "{report:?}");
    let t = table(&root);
    assert_eq!(t.format_version, 1);
    assert_eq!(t.policy.target_lufs, REFERENCE_LUFS);
    assert_eq!(t.entries.len(), 3, "{:?}", t.entries.keys());
    for (name, amp, channels) in [("quiet", 0.02f32, 1u8), ("loud", 0.5, 2)] {
        let entry = &t.entries[&format!("assets/sfx/{name}.wav")];
        assert!(
            entry.normalize && !entry.short_clip && !entry.peak_limited,
            "{entry:?}"
        );
        // The kira decoder delivers stereo frames for both files: identical channels.
        assert!(entry.dual_mono, "{name}: {entry:?}");
        // Apply the gain to the source signal as played (dual mono) and re-measure.
        let played: Vec<f32> = sine(1.5, amp).iter().flat_map(|&s| [s, s]).collect();
        let gain = db_to_gain(entry.gain_db);
        let scaled: Vec<f32> = played.iter().map(|s| s * gain).collect();
        let after = measure_loudness(&scaled, 2, RATE).expect("valid").integrated_lufs;
        assert!(
            (after - REFERENCE_LUFS).abs() < 0.1,
            "{name} ({channels} ch): {after} {entry:?}"
        );
    }
    let blip = &t.entries["assets/sfx/blip.wav"];
    assert!(blip.short_clip && blip.integrated_lufs.is_some(), "{blip:?}");
    let (type_path, _) = payload(&root, "assets/sfx/loud.wav");
    assert_eq!(
        type_path, "regia_audio::CookedSound",
        "ADR-2788: the sound and its record"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_dds_chain_passes_through_and_a_single_level_is_recooked() {
    let root = regia_test_utils::unique_temp_dir("dds-cook");
    write_project(&root);
    cook(&root);
    let (type_path, bytes) = payload(&root, "assets/tex/brick.dds");
    assert_eq!(type_path, "regia_asset::CookedTexture");
    let tex = CookedTexture::from_postcard(&bytes).expect("texture");
    assert_eq!((tex.width, tex.height), (16, 16));
    let reader = ktx2::Reader::new(&tex.ktx2[..]).expect("ktx2");
    assert_eq!(reader.header().format, Some(ktx2::Format::BC1_RGB_SRGB_BLOCK));
    let levels: Vec<Vec<u8>> = reader.levels().map(|l| l.data.to_vec()).collect();
    let expected: Vec<Vec<u8>> = (0..5)
        .map(|i| bc1_level((16 >> i).max(1), (16 >> i).max(1), i as u16))
        .collect();
    assert_eq!(levels, expected, "the source blocks, bit for bit");

    // One BC3 level: decoded and cooked with a full chain (desktop BC7 for a color map).
    let (type_path, bytes) = payload(&root, "assets/tex/flat.dds");
    assert_eq!(type_path, "regia_asset::CookedTexture");
    let tex = CookedTexture::from_postcard(&bytes).expect("texture");
    let reader = ktx2::Reader::new(&tex.ktx2[..]).expect("ktx2");
    assert_eq!(reader.levels().count(), 5);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn tga_and_bmp_cook_like_png() {
    let root = regia_test_utils::unique_temp_dir("tga-bmp-cook");
    write_project(&root);
    cook(&root);
    for rel in ["assets/tex/sign.tga", "assets/tex/panel.bmp"] {
        let (type_path, bytes) = payload(&root, rel);
        assert_eq!(type_path, "regia_asset::CookedTexture", "{rel}");
        let tex = CookedTexture::from_postcard(&bytes).expect("texture");
        assert_eq!((tex.width, tex.height), (16, 16), "{rel}");
    }
    // Uncompressed (`compress: false`) keeps the decoded pixels exactly.
    let meta_path = meta_path_for(&root.join("assets/tex/sign.tga"));
    let mut meta = MetaSidecar::from_ron(&std::fs::read(&meta_path).expect("meta")).expect("parse");
    meta.settings = Some("(compress: Some(false))".into());
    std::fs::write(&meta_path, meta.to_ron_string().expect("ron")).expect("write meta");
    cook(&root);
    let (type_path, bytes) = payload(&root, "assets/tex/sign.tga");
    assert_eq!(type_path, "regia_asset::CookedImage");
    let img = CookedImage::from_postcard(&bytes).expect("image");
    assert_eq!(img.rgba8, rgba_pattern(16, 16).into_raw());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_recook_skips_the_new_sources_and_the_policy_file_recooks_sounds() {
    let root = regia_test_utils::unique_temp_dir("loudness-skip");
    write_project(&root);
    cook(&root);
    age_assets(&root);
    let second = cook(&root);
    assert_eq!((second.cooked, second.imports_skipped), (0, 7), "{second:?}");
    let before = table(&root);

    std::fs::write(root.join("loudness.toml"), "target_lufs = -16.0\n").expect("policy");
    let third = cook(&root);
    assert_eq!(third.cooked, 3, "only the sounds: {third:?}");
    let after = table(&root);
    assert_eq!(after.policy.target_lufs, -16.0);
    for (key, entry) in &after.entries {
        let old = &before.entries[key];
        assert_eq!(
            entry.integrated_lufs, old.integrated_lufs,
            "{key}: the measurement is unchanged"
        );
        if !entry.peak_limited {
            assert!(
                (entry.gain_db - old.gain_db - 7.0).abs() < 1e-3,
                "{key}: {entry:?} {old:?}"
            );
        }
    }

    // A bad policy file is a warning and the defaults apply.
    std::fs::write(root.join("loudness.toml"), "target_lufs = 9.0\n").expect("policy");
    let bad = cook(&root);
    assert!(
        bad.warnings.iter().any(|w| w.contains("loudness.toml")),
        "{:?}",
        bad.warnings
    );
    assert_eq!(table(&root).policy.target_lufs, REFERENCE_LUFS);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_meta_opt_out_gives_no_gain() {
    let root = regia_test_utils::unique_temp_dir("loudness-optout");
    write_project(&root);
    cook(&root);
    let meta_path = meta_path_for(&root.join("assets/sfx/quiet.wav"));
    let mut meta = MetaSidecar::from_ron(&std::fs::read(&meta_path).expect("meta")).expect("parse");
    assert_eq!(meta.importer, "audio");
    meta.settings = Some("(normalize: Some(false))".into());
    std::fs::write(&meta_path, meta.to_ron_string().expect("ron")).expect("write meta");
    cook(&root);
    let entry = &table(&root).entries["assets/sfx/quiet.wav"];
    assert!(!entry.normalize);
    assert_eq!(entry.gain_db, 0.0);
    assert!(entry.integrated_lufs.is_some(), "still measured");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_deleted_sound_leaves_the_table_and_a_broken_one_is_an_error() {
    let root = regia_test_utils::unique_temp_dir("loudness-delete");
    write_project(&root);
    cook(&root);
    std::fs::remove_file(root.join("assets/sfx/blip.wav")).expect("remove");
    cook(&root);
    assert!(!table(&root).entries.contains_key("assets/sfx/blip.wav"));
    std::fs::write(root.join("assets/sfx/broken.ogg"), b"OggS not really").expect("broken");
    std::fs::write(root.join("assets/tex/broken.dds"), b"DDS short").expect("broken dds");
    let report = cook_project(CookOptions {
        project_root: root.clone(),
        platform: host_platform().to_string(),
        incremental: true,
    })
    .expect("cook");
    let errors: Vec<&str> = report.errors.iter().map(|(_, e)| e.as_str()).collect();
    assert_eq!(report.errors.len(), 2, "{errors:?}");
    assert!(
        errors.iter().any(|e| e.starts_with("audio import error")),
        "{errors:?}"
    );
    assert!(errors.iter().any(|e| e.contains("dds_bad_header")), "{errors:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_android_cook_transcodes_a_dds_chain_to_astc() {
    let root = regia_test_utils::unique_temp_dir("dds-android");
    write_project(&root);
    let report = cook_project(CookOptions {
        project_root: root.clone(),
        platform: "android".into(),
        incremental: true,
    })
    .expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let cache = cooked_cache_dir(&root, "android");
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("a manifest");
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.path == "assets/tex/brick.dds")
        .expect("entry");
    let bytes = std::fs::read(cache.join(cooked_file_name(entry.id))).expect("cooked");
    let (_, payload) = decode_cooked_file(&bytes).expect("decode");
    let tex = CookedTexture::from_postcard(payload).expect("texture");
    let reader = ktx2::Reader::new(&tex.ktx2[..]).expect("ktx2");
    let format = format!("{:?}", reader.header().format);
    assert!(format.contains("ASTC"), "{format}");
    assert_eq!(reader.levels().count(), 5);
    let _ = std::fs::remove_dir_all(&root);
}

/// A packaged game has the sounds and their gains in the pak and no loose file (ADR-2788): the packed sound
/// decodes to the loose sound's length and rate, plays at the same loudness gain, a table without it falls
/// back to 0 dB, and a cut-short pak or entry errors with a code instead of panicking.
#[test]
fn a_packed_sound_plays_with_the_loose_files_gone_at_the_loose_gain() {
    use std::collections::BTreeMap;

    use regia_asset::{AssetServer, AssetServerSettings};
    use regia_audio::SoundAssets;

    let root = regia_test_utils::unique_temp_dir("audio-pak");
    write_project(&root);
    cook(&root);
    let cache = cooked_cache_dir(&root, host_platform());
    let loose_table = table(&root);

    // The loose reference: file sounds under the cook's own table.
    let mut loose = SoundAssets::default();
    for name in ["quiet", "loud"] {
        loose
            .add_file(name, root.join(format!("assets/sfx/{name}.wav")))
            .expect("loose sound");
    }
    loose.set_loudness_normalization(Some(loose_table.clone()));

    let content = root.join("content");
    regia_asset_pipeline::pack_cooked_directory(&cache, &content, &BTreeMap::new(), true).expect("pack");
    // The shipped game: no `assets/`, no cooked cache, only the pak.
    let shipped = regia_test_utils::unique_temp_dir("audio-pak-game");
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: shipped,
        platform: host_platform().to_string(),
        content_dir: Some(content.clone()),
    })
    .expect("open");
    let mut packed = SoundAssets::default();
    for name in ["quiet", "loud"] {
        packed
            .add_packed_from_assets(name, &mut server, &format!("assets/sfx/{name}.wav"))
            .expect("packed sound");
    }
    // Off by default: 0 dB, as a loose sound before normalization is turned on.
    assert_eq!(packed.by_name("loud").expect("loud").loudness_gain_db(), 0.0);
    let changed = packed.set_loudness_normalization(Some(packed.packed_loudness_table()));
    assert_eq!(changed, 2);
    for name in ["quiet", "loud"] {
        let (a, b) = (
            loose.by_name(name).expect("loose"),
            packed.by_name(name).expect("packed"),
        );
        assert_eq!(
            a.loudness_gain_db(),
            b.loudness_gain_db(),
            "{name}: same gain through the pak"
        );
        assert_ne!(b.loudness_gain_db(), 0.0, "{name} is not at the target already");
        assert_eq!(a.sample_rate(), b.sample_rate());
        assert_eq!(a.duration_secs(), b.duration_secs());
    }
    let key = "assets/sfx/quiet.wav";
    assert_eq!(
        packed.packed_loudness_table().entries[key],
        loose_table.entries[key],
        "the record in the pak is the record in loudness.ron"
    );

    // No table entry for a sound (a missing loudness.ron): 0 dB, the spec's fallback.
    assert_eq!(
        packed.set_loudness_normalization(Some(LoudnessTable::default())),
        2
    );
    assert_eq!(packed.by_name("quiet").expect("quiet").loudness_gain_db(), 0.0);

    // A path the manifest does not know and a cut-short entry are errors with a code.
    let err = packed
        .add_packed_from_assets("nope", &mut server, "assets/sfx/nope.wav")
        .expect_err("not packed");
    assert!(err.to_string().contains("cooked_sound_missing"), "{err}");
    let id = server.id_for_path(key).expect("id");
    let (bytes, _) = server.read_cooked(id).expect("entry");
    let (_, payload) = decode_cooked_file(&bytes).expect("wrapper");
    for len in [0, 5, 11, 20] {
        let err = packed
            .add_packed("cut", key, &payload[..len])
            .expect_err("truncated");
        assert!(err.to_string().contains("cooked_sound_"), "{len}: {err}");
    }

    // A pak cut in half does not open. The cut copy is a new file: `server` still maps
    // `base.fpak`, and a mapped pak is never modified in place (Windows refuses the write with
    // a sharing violation; on Unix it would cut the live mapping short).
    let pak = content.join("base.fpak");
    let whole = std::fs::read(&pak).expect("pak bytes");
    let cut = root.join("cut.fpak");
    std::fs::write(&cut, &whole[..whole.len() / 2]).expect("truncate");
    assert!(
        regia_asset::PakReader::open(&cut).is_err(),
        "a truncated pak is an error"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Rewrites the `.meta` `settings` of `rel` (created by an earlier cook).
fn set_meta_settings(root: &Path, rel: &str, settings: &str) {
    let meta_path = meta_path_for(&root.join(rel));
    let mut meta = MetaSidecar::from_ron(&std::fs::read(&meta_path).expect("meta")).expect("parse");
    meta.settings = Some(settings.into());
    std::fs::write(&meta_path, meta.to_ron_string().expect("ron")).expect("write meta");
}

/// Spec 95 § 3: `max_size` drops leading levels while the edge is above the cap and always keeps
/// the last, so a 64×64 file with levels 64 and 32 and `max_size: 8` cooks 32×32 (level 1's blocks,
/// unchanged); `compress: Some(false)` decodes level 0 to RGBA8 instead of passing blocks through.
#[test]
fn dds_max_size_keeps_the_last_level_and_compress_off_decodes_level_zero() {
    let root = regia_test_utils::unique_temp_dir("dds-max-size");
    let tex = root.join("assets/tex");
    std::fs::create_dir_all(&tex).expect("tex");
    let levels = vec![bc1_level(64, 64, 3), bc1_level(32, 32, 9)];
    std::fs::write(tex.join("tall.dds"), dds(64, 64, b"DXT1", &levels)).expect("dds");
    cook(&root);
    let (_, bytes) = payload(&root, "assets/tex/tall.dds");
    let tex_full = CookedTexture::from_postcard(&bytes).expect("texture");
    assert_eq!((tex_full.width, tex_full.height), (64, 64), "no cap: both levels");

    set_meta_settings(&root, "assets/tex/tall.dds", "(max_size: Some(8))");
    cook(&root);
    let (type_path, bytes) = payload(&root, "assets/tex/tall.dds");
    assert_eq!(type_path, "regia_asset::CookedTexture");
    let capped = CookedTexture::from_postcard(&bytes).expect("texture");
    assert_eq!(
        (capped.width, capped.height),
        (32, 32),
        "the last level stays although it is above the cap"
    );
    let reader = ktx2::Reader::new(&capped.ktx2[..]).expect("ktx2");
    let kept: Vec<Vec<u8>> = reader.levels().map(|l| l.data.to_vec()).collect();
    assert_eq!(kept, vec![levels[1].clone()], "level 1's blocks, bit for bit");

    set_meta_settings(&root, "assets/tex/tall.dds", "(compress: Some(false))");
    cook(&root);
    let (type_path, bytes) = payload(&root, "assets/tex/tall.dds");
    assert_eq!(
        type_path, "regia_asset::CookedImage",
        "decoded, not passed through"
    );
    let img = CookedImage::from_postcard(&bytes).expect("image");
    assert_eq!((img.width, img.height), (64, 64));
    let expected =
        regia_asset::bcn_decode::decode_bcn(regia_asset::bcn_decode::BcnKind::Bc1, 64, 64, &levels[0])
            .expect("decode level 0");
    assert_eq!(img.rgba8, expected, "level 0 decoded to RGBA8");
    let _ = std::fs::remove_dir_all(&root);
}

/// Spec 95 § 2.2: a `.meta` `target_lufs` override replaces the project target for that sound only.
#[test]
fn a_meta_target_override_moves_only_that_sound() {
    let root = regia_test_utils::unique_temp_dir("loudness-override");
    write_project(&root);
    cook(&root);
    let before = table(&root);
    set_meta_settings(&root, "assets/sfx/quiet.wav", "(target_lufs: Some(-18.0))");
    cook(&root);
    let after = table(&root);
    let quiet = &after.entries["assets/sfx/quiet.wav"];
    assert_eq!(quiet.target_lufs, -18.0);
    assert!(quiet.normalize && !quiet.peak_limited, "{quiet:?}");
    let measured = quiet.integrated_lufs.expect("measured");
    assert!((quiet.gain_db - (-18.0 - measured)).abs() < 1e-4, "{quiet:?}");
    assert_eq!(
        after.policy.target_lufs, REFERENCE_LUFS,
        "the project policy is unchanged"
    );
    assert_eq!(
        after.entries["assets/sfx/loud.wav"], before.entries["assets/sfx/loud.wav"],
        "other sounds keep their record"
    );

    // A malformed settings string is a cook warning and the defaults apply.
    set_meta_settings(&root, "assets/sfx/quiet.wav", "(target_lufs: loud)");
    let report = cook(&root);
    assert!(
        report.warnings.iter().any(|w| w.contains("quiet.wav")),
        "{:?}",
        report.warnings
    );
    let reset = &table(&root).entries["assets/sfx/quiet.wav"];
    assert_eq!(reset.target_lufs, REFERENCE_LUFS, "{reset:?}");
    let _ = std::fs::remove_dir_all(&root);
}
