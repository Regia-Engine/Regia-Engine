// SPDX-License-Identifier: MPL-2.0
//! v0.5 import, cook and package acceptance (evidence rows N10, N11 and N13; ADR-4898).
//!
//! Every test runs the whole route a shipped game takes: authored sources in `assets/`, the project
//! cook, the shipping pak writer, then the runtime readers (`PakReader`, the cooked decoders,
//! `regia_physics::hull_set`, `regia_audio`) on what the pak or the package staging holds. All the
//! sources are written here (engine-owned, no third-party file); no GPU, no goldens, no clock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_asset::{
    AssetManifest, CookedMesh, CookedTexture, MetaSidecar, PakReader, cooked_cache_dir, cooked_file_name,
    decode_cooked_file, meta_path_for,
};
use regia_asset_pipeline::audio_cook::stage_loudness_table;
use regia_asset_pipeline::audio_transcode::shipped_audio_bytes;
use regia_asset_pipeline::{
    CookError, CookOptions, CookReport, cook_project, host_platform, pack_cooked_directory_with,
};
use regia_audio::{
    Audio, AudioSettings, LOUDNESS_TABLE_FILE, LoudnessTable, PlaybackParams, REFERENCE_LUFS, SoundAssets,
    measure_loudness,
};
use regia_physics::hull_set::{HULL_SET_TYPE_PATH, HullSet};

/// The rendered-loudness tolerance of the run-time loudness tests (`regia_audio` `loudness_import`).
const RENDER_TOLERANCE_LU: f32 = 0.5;

fn options(root: &Path, platform: &str) -> CookOptions {
    CookOptions {
        project_root: root.to_path_buf(),
        platform: platform.to_string(),
        incremental: true,
    }
}

fn cook(root: &Path) -> CookReport {
    let report = cook_project(options(root, host_platform())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    report
}

fn set_meta(source: &Path, settings: &str) {
    let path = meta_path_for(source);
    let mut meta = MetaSidecar::from_ron(&std::fs::read(&path).expect("meta")).expect("parse meta");
    meta.settings = Some(settings.into());
    std::fs::write(&path, meta.to_ron_string().expect("ron")).expect("write meta");
}

/// Packs the host cook into `<root>/build/paks/<host>/base.fpak` with the shipping writer.
fn pack(root: &Path) -> PathBuf {
    let cache = cooked_cache_dir(root, host_platform());
    let out = root.join("build/paks").join(host_platform());
    std::fs::create_dir_all(&out).expect("pak dir");
    let paks = pack_cooked_directory_with(&cache, &out, &BTreeMap::new(), true, false).expect("pack");
    assert_eq!(paks.len(), 1, "{paks:?}");
    paks.into_iter().next().expect("base pak")
}

/// Every manifest row of `pak`: path → (type path, cooked payload), read back the way the runtime
/// reads it (mapped reader, lz4, cooked envelope).
fn pak_payloads(pak: &Path) -> BTreeMap<String, (String, Vec<u8>)> {
    let reader = PakReader::open(pak).expect("open pak");
    let mut out = BTreeMap::new();
    for entry in reader.manifest() {
        let bytes = reader
            .read_decompressed(entry.id)
            .unwrap_or_else(|e| panic!("{}: {e}", entry.path));
        let (_, payload) = decode_cooked_file(&bytes).unwrap_or_else(|e| panic!("{}: {e}", entry.path));
        out.insert(entry.path.clone(), (entry.type_path.clone(), payload.to_vec()));
    }
    out
}

/// The loose cooked payload of manifest row `path` (the editor's view of the same asset).
fn loose_payload(root: &Path, path: &str) -> Vec<u8> {
    let cache = cooked_cache_dir(root, host_platform());
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("a manifest");
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("no manifest row {path}"));
    let bytes = std::fs::read(cache.join(cooked_file_name(entry.id))).expect("cooked file");
    decode_cooked_file(&bytes).expect("decode").1.to_vec()
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// A 1 m cube with one UV square and one normal per face: 8 positions, 24 distinct corners (every
/// cube edge is a UV and normal seam).
const SEAM_CUBE: &str = "\
o crate
v -0.5 -0.5 -0.5
v 0.5 -0.5 -0.5
v 0.5 0.5 -0.5
v -0.5 0.5 -0.5
v -0.5 -0.5 0.5
v 0.5 -0.5 0.5
v 0.5 0.5 0.5
v -0.5 0.5 0.5
vt 0 0
vt 1 0
vt 1 1
vt 0 1
vn 0 0 -1
vn 0 0 1
vn -1 0 0
vn 1 0 0
vn 0 -1 0
vn 0 1 0
f 1/1/1 4/2/1 3/3/1 2/4/1
f 5/1/2 6/2/2 7/3/2 8/4/2
f 1/1/3 5/2/3 8/3/3 4/4/3
f 2/1/4 3/2/4 7/3/4 6/4/4
f 1/1/5 2/2/5 6/3/5 5/4/5
f 4/1/6 8/2/6 7/3/6 3/4/6
";

#[test]
fn an_obj_with_corner_seams_and_convex_collision_ships_through_the_pak() {
    let root = regia_test_utils::unique_temp_dir("accept-obj");
    let source = root.join("assets/props/crate.obj");
    std::fs::create_dir_all(source.parent().expect("dir")).expect("dirs");
    std::fs::write(&source, SEAM_CUBE).expect("obj");
    cook(&root);
    set_meta(
        &source,
        "(collision: ConvexDecomposition, collision_hulls: (max_hulls: 4, resolution: 32))",
    );
    cook(&root);
    let pak = pack(&root);
    let shipped = pak_payloads(&pak);

    let meshes: Vec<(&String, &Vec<u8>)> = shipped
        .iter()
        .filter(|(path, (ty, _))| {
            path.starts_with("assets/props/crate.obj#mesh/") && ty == "regia_asset::CookedMesh"
        })
        .map(|(path, (_, payload))| (path, payload))
        .collect();
    assert_eq!(meshes.len(), 1, "{:?}", shipped.keys());
    let (mesh_path, mesh_payload) = meshes[0];
    assert_eq!(
        mesh_payload,
        &loose_payload(&root, mesh_path),
        "the pak ships the cooked mesh byte for byte"
    );
    let mesh = CookedMesh::from_postcard(mesh_payload)
        .expect("mesh decodes")
        .to_cpu_mesh()
        .expect("mesh unpacks");
    assert_eq!(mesh.positions.len(), 24, "every seam corner stays its own vertex");
    assert_eq!(mesh.indices.len(), 36);
    // Each face keeps its own UV square and normal at the shared cube corners.
    let mut corners: Vec<([i32; 3], [i32; 2], [i32; 3])> = (0..mesh.positions.len())
        .map(|i| {
            let p = mesh.positions[i].map(|v| (v * 2.0).round() as i32);
            let uv = mesh.uvs[i].map(|v| v.round() as i32);
            let n = mesh.normals[i].map(|v| v.round() as i32);
            (p, uv, n)
        })
        .collect();
    corners.sort_unstable();
    corners.dedup();
    assert_eq!(corners.len(), 24, "no two corners collapsed");
    let positions: std::collections::BTreeSet<[i32; 3]> = corners.iter().map(|c| c.0).collect();
    assert_eq!(positions.len(), 8);

    let hulls: Vec<&Vec<u8>> = shipped
        .iter()
        .filter(|(path, (ty, _))| {
            path.starts_with("assets/props/crate.obj#collision/") && ty == HULL_SET_TYPE_PATH
        })
        .map(|(_, (_, payload))| payload)
        .collect();
    assert_eq!(hulls.len(), 1, "{:?}", shipped.keys());
    let set = HullSet::from_bytes(hulls[0]).expect("hull set decodes");
    assert!(!set.hulls.is_empty());
    for hull in &set.hulls {
        for v in hull {
            assert!(
                v.abs().max_element() <= 0.5 + 1.0 / 16.0,
                "a hull vertex outside the cube: {v:?}"
            );
        }
    }
    assert!(
        set.to_collider_shape().is_some(),
        "physics builds a collider from the shipped hulls"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_malformed_obj_names_its_line_and_leaves_the_last_good_cook_intact() {
    let root = regia_test_utils::unique_temp_dir("accept-obj-bad");
    let source = root.join("assets/props/crate.obj");
    std::fs::create_dir_all(source.parent().expect("dir")).expect("dirs");
    std::fs::write(&source, SEAM_CUBE).expect("obj");
    cook(&root);
    let cache = cooked_cache_dir(&root, host_platform());
    let before: BTreeMap<PathBuf, Vec<u8>> = files_under(&cache)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "fckd"))
        .map(|p| {
            let bytes = std::fs::read(&p).expect("read");
            (p, bytes)
        })
        .collect();
    assert!(!before.is_empty());

    // Line 20: a face corner that names a missing position.
    let broken = SEAM_CUBE.replace("f 1/1/1 4/2/1 3/3/1 2/4/1", "f 1/1/1 4/2/1 3/3/1 99/4/1");
    std::fs::write(&source, broken).expect("broken obj");
    let report = cook_project(options(&root, host_platform())).expect("the cook itself runs");
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    let (path, message) = &report.errors[0];
    assert!(path.ends_with("props/crate.obj"), "{path}");
    assert!(message.contains("OBJ line 20"), "{message}");
    // The previous cooked files are untouched and nothing half-written is left behind.
    for (file, bytes) in &before {
        assert_eq!(
            &std::fs::read(file).expect("still there"),
            bytes,
            "{}",
            file.display()
        );
    }
    for file in files_under(&cache) {
        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        assert!(
            !name.contains(".tmp"),
            "a temporary file was left: {}",
            file.display()
        );
    }
    // The last good cook still packs and reads.
    let shipped = pak_payloads(&pack(&root));
    assert!(
        shipped.keys().any(|p| p.starts_with("assets/props/crate.obj")),
        "{:?}",
        shipped.keys()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unsupported_platform_tag_is_refused_before_anything_is_written() {
    let root = regia_test_utils::unique_temp_dir("accept-platform");
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    std::fs::write(root.join("assets/crate.obj"), SEAM_CUBE).expect("obj");
    for tag in ["", "  ", "../escape", "a/b", "c:\\x", "linux\0", "ps5"] {
        match cook_project(options(&root, tag)) {
            Err(CookError::UnsupportedPlatform(message)) => {
                assert!(
                    message.starts_with("cook_platform_unsupported"),
                    "{tag:?}: {message}"
                );
            }
            other => panic!("{tag:?}: expected UnsupportedPlatform, got {other:?}"),
        }
    }
    assert!(!root.join(".regia").exists(), "a refused cook wrote nothing");
    assert!(!root.parent().expect("parent").join("escape").exists());
    assert!(!root.join("assets/crate.obj.meta").exists());
    // Every shipped tag (any case, padded) and the host's own tag still cook.
    for tag in ["Linux", " windows ", "android", host_platform()] {
        let report = cook_project(options(&root, tag)).unwrap_or_else(|e| panic!("{tag:?}: {e}"));
        assert!(report.errors.is_empty(), "{tag:?}: {:?}", report.errors);
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A non-square power-of-two RGBA pattern that is not flat (so mips differ).
fn pattern_png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |x, y| {
        image::Rgba([
            (x * 7 % 256) as u8,
            (y * 13 % 256) as u8,
            ((x ^ y) % 256) as u8,
            255,
        ])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).expect("png");
    out.into_inner()
}

#[test]
fn texture_mips_stream_metadata_and_atlas_rects_survive_the_pak() {
    let root = regia_test_utils::unique_temp_dir("accept-tex");
    let tex = root.join("assets/tex");
    let ui = root.join("assets/ui");
    std::fs::create_dir_all(&tex).expect("tex");
    std::fs::create_dir_all(&ui).expect("ui");
    std::fs::write(tex.join("wall.png"), pattern_png(512, 256)).expect("wall");
    std::fs::write(ui.join("a.png"), pattern_png(16, 16)).expect("a");
    std::fs::write(ui.join("b.png"), pattern_png(32, 8)).expect("b");
    std::fs::write(
        ui.join("hud.atlas.ron"),
        r#"(version: 1, padding: 2, sprites: [(name: "a", image: "a.png"), (name: "b", image: "b.png", nine_slice: Some((2, 2, 2, 2)))])"#,
    )
    .expect("atlas");
    cook(&root);
    let shipped = pak_payloads(&pack(&root));

    let (ty, payload) = &shipped["assets/tex/wall.png"];
    assert_eq!(ty, "regia_asset::CookedTexture");
    assert_eq!(payload, &loose_payload(&root, "assets/tex/wall.png"));
    let texture = CookedTexture::from_postcard(payload).expect("texture decodes");
    let loose = CookedTexture::from_postcard(&loose_payload(&root, "assets/tex/wall.png")).expect("loose");
    // The renderer's reader finds the cook's mip-stream stamp in the shipped bytes.
    let stamped = regia_asset_pipeline::mip_stream::cooked_mip_stream(&loose);
    assert!(stamped.is_some(), "the cook stamps mip-stream metadata");
    let runtime = regia_render::CompressedTexture::from_cooked(&texture).expect("the renderer reads it");
    assert_eq!(
        runtime.mip_stream, stamped,
        "mip stream metadata survives the pak"
    );
    let reader = ktx2::Reader::new(&texture.ktx2[..]).expect("ktx2");
    let header = reader.header();
    assert_eq!((header.pixel_width, header.pixel_height), (512, 256));
    // The full chain down to 1 × 1 (log2(512) + 1 levels), each level present.
    assert_eq!(reader.levels().count(), 10);
    assert!(reader.levels().all(|level| !level.data.is_empty()));

    let (ty, payload) = &shipped["assets/ui/hud.atlas.ron"];
    assert_eq!(ty, regia_2d::atlas::ATLAS_TYPE_PATH);
    let atlas = regia_2d::SpriteAtlas::from_postcard(payload).expect("atlas decodes");
    let a = atlas.sprite("a").expect("sprite a");
    let b = atlas.sprite("b").expect("sprite b");
    assert_eq!(a.source_size, [16, 16]);
    assert_eq!(b.source_size, [32, 8]);
    assert_eq!([a.rect[2], a.rect[3]], [16, 16], "{a:?}");
    assert_eq!([b.rect[2], b.rect[3]], [32, 8], "{b:?}");
    assert!(b.nine_slice.is_some() && a.nine_slice.is_none());
    for s in [a, b] {
        let page = &atlas.pages[s.page as usize];
        assert!(
            s.rect[0] + s.rect[2] <= page.width && s.rect[1] + s.rect[3] <= page.height,
            "{s:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// PCM16 WAV of a 1 kHz sine at `rate`: `channels` interleaved, the right channel of a stereo file
/// at `right` × the left amplitude (1.0 = dual mono).
fn tone_wav(rate: u32, channels: u16, seconds: f64, amp: f32, right: f32) -> Vec<u8> {
    let frames = (seconds * f64::from(rate)) as usize;
    let mut data = Vec::with_capacity(frames * 2 * usize::from(channels));
    for i in 0..frames {
        let s = ((i as f64 / f64::from(rate)) * 1000.0 * std::f64::consts::TAU).sin() as f32 * amp;
        for c in 0..channels {
            let v = if c == 1 { s * right } else { s };
            data.extend_from_slice(&((v * 32_767.0) as i16).to_le_bytes());
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
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * block).to_le_bytes());
    out.extend_from_slice(&(block as u16).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

/// The sounds of the loudness tests: (file, rate, channels, amplitude, right-channel ratio).
const SOUNDS: &[(&str, u32, u16, f32, f32)] = &[
    ("low_rate_mono.wav", 22_050, 1, 0.02, 1.0),
    ("cd_stereo.wav", 44_100, 2, 0.5, 0.5),
    ("studio_stereo.wav", 48_000, 2, 0.1, 1.0),
    ("hires_mono.wav", 96_000, 1, 0.3, 1.0),
];

fn sound_project(policy: Option<&str>) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir("accept-loudness");
    let sfx = root.join("assets/sfx");
    std::fs::create_dir_all(&sfx).expect("sfx");
    for (name, rate, channels, amp, right) in SOUNDS {
        std::fs::write(sfx.join(name), tone_wav(*rate, *channels, 1.5, *amp, *right)).expect("wav");
    }
    if let Some(toml) = policy {
        std::fs::write(root.join("audio_cook.toml"), toml).expect("policy");
    }
    root
}

/// Stages what `regia package` ships for sounds: each sound's shipped bytes (source or Ogg Vorbis)
/// under `<package>/assets/...`, and the loudness table.
fn stage_sounds(root: &Path) -> PathBuf {
    let package = root.join("build/package");
    let assets = package.join("assets");
    for (name, ..) in SOUNDS {
        let source = root.join("assets/sfx").join(name);
        let rel = format!("assets/sfx/{name}");
        let out = assets.join("sfx").join(name);
        std::fs::create_dir_all(out.parent().expect("dir")).expect("dirs");
        match shipped_audio_bytes(root, host_platform(), &source, &rel).expect("ship") {
            Some(bytes) => std::fs::write(&out, bytes).expect("write"),
            None => {
                std::fs::copy(&source, &out).expect("copy");
            }
        }
    }
    let staged = stage_loudness_table(root, host_platform(), &assets)
        .expect("stage the table")
        .expect("a table ships");
    assert_eq!(staged, assets.join(LOUDNESS_TABLE_FILE));
    package
}

/// Renders `sound` once (1.5 s, 48 kHz stereo) and returns its integrated loudness.
fn rendered_lufs(sounds: &SoundAssets, name: &str) -> f32 {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    let asset = sounds
        .by_name(name)
        .expect("registered")
        .decode_to_memory()
        .expect("decode");
    audio
        .start_asset(&asset, PlaybackParams::default())
        .expect("play");
    let mut out = Vec::new();
    for _ in 0..90 {
        audio.tick(1.0 / 60.0);
        out.extend_from_slice(&audio.render_offline(800).expect("render"));
    }
    measure_loudness(&out, 2, 48_000).expect("valid").integrated_lufs
}

/// The packaged route: register the shipped files, load the shipped table, play.
fn check_packaged_playback(package: &Path, label: &str) {
    let table = LoudnessTable::load(package.join("assets").join(LOUDNESS_TABLE_FILE)).expect("shipped table");
    assert_eq!(
        table.entries.len(),
        SOUNDS.len(),
        "{label}: {:?}",
        table.entries.keys()
    );
    let mut sounds = SoundAssets::default();
    for (name, ..) in SOUNDS {
        sounds
            .add_file(*name, package.join("assets/sfx").join(name))
            .unwrap_or_else(|e| panic!("{label}: {name}: {e}"));
    }
    let raw: Vec<f32> = SOUNDS
        .iter()
        .map(|(name, ..)| rendered_lufs(&sounds, name))
        .collect();
    assert_eq!(
        sounds.set_loudness_normalization(Some(table.clone())),
        SOUNDS.len()
    );
    // Turning it on again, reloading a file, or decoding to memory never stacks a second gain.
    assert_eq!(sounds.set_loudness_normalization(Some(table.clone())), 0);
    let first = package.join("assets/sfx").join(SOUNDS[0].0);
    assert_eq!(sounds.reload_file(&first).expect("reload"), 1);
    for ((name, rate, ..), raw) in SOUNDS.iter().zip(&raw) {
        let key = format!("assets/sfx/{name}");
        let record = &table.entries[&key];
        assert_eq!(record.sample_rate, *rate, "{label}: {name}: {record:?}");
        assert!(
            record.normalize && !record.short_clip,
            "{label}: {name}: {record:?}"
        );
        let asset = sounds.by_name(name).expect("registered");
        assert_eq!(asset.loudness_gain_db(), record.gain_db, "{label}: {name}");
        let decoded = asset.decode_to_memory().expect("decode");
        assert_eq!(decoded.loudness_gain_db(), record.gain_db, "{label}: {name}");
        let normalized = rendered_lufs(&sounds, name);
        assert!(
            (normalized - REFERENCE_LUFS).abs() < RENDER_TOLERANCE_LU,
            "{label}: {name} at {rate} Hz renders {normalized} LUFS (raw {raw}, gain {})",
            record.gain_db
        );
        // Exactly one gain: the normalized render is the raw render moved by the table gain.
        assert!(
            (normalized - raw - record.gain_db).abs() < 0.1,
            "{label}: {name}: {raw} + {} != {normalized}",
            record.gain_db
        );
        // The record measures the source: the cook's figure matches the analytic meter on the
        // source frames as played (dual mono for a mono file).
        let measured = record.integrated_lufs.expect("loud enough");
        assert!(
            (measured + record.gain_db - record.target_lufs).abs() < 0.01 || record.peak_limited,
            "{label}: {name}: {record:?}"
        );
    }
}

#[test]
fn multi_rate_sounds_ship_with_their_loudness_table_and_play_normalized_once() {
    let root = sound_project(None);
    cook(&root);
    let package = stage_sounds(&root);
    // `keep`: the source bytes ship.
    for (name, ..) in SOUNDS {
        assert_eq!(
            std::fs::read(package.join("assets/sfx").join(name)).expect("shipped"),
            std::fs::read(root.join("assets/sfx").join(name)).expect("source"),
        );
    }
    check_packaged_playback(&package, "keep");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn vorbis_shipped_sounds_keep_the_source_gain_and_land_on_the_target() {
    let root = sound_project(Some("mode = \"vorbis\"\n"));
    cook(&root);
    let package = stage_sounds(&root);
    for (name, ..) in SOUNDS {
        let bytes = std::fs::read(package.join("assets/sfx").join(name)).expect("shipped");
        assert_eq!(&bytes[..4], b"OggS", "{name} ships as Ogg Vorbis");
    }
    check_packaged_playback(&package, "vorbis");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_loudness_table_stages_atomically_and_refuses_a_raw_file_in_its_place() {
    let root = sound_project(None);
    let assets = root.join("build/package/assets");
    // Before any cook there is no table: nothing ships, nothing is created.
    assert_eq!(
        stage_loudness_table(&root, host_platform(), &assets).expect("no table"),
        None
    );
    assert!(!assets.exists());
    cook(&root);
    // A raw `assets/loudness.ron` already staged would be silently replaced: refused, by code.
    std::fs::create_dir_all(&assets).expect("assets");
    std::fs::write(assets.join(LOUDNESS_TABLE_FILE), "(not: a table)").expect("raw");
    let err = stage_loudness_table(&root, host_platform(), &assets).expect_err("conflict");
    assert!(err.starts_with("loudness_table_conflict"), "{err}");
    assert_eq!(
        std::fs::read_to_string(assets.join(LOUDNESS_TABLE_FILE)).expect("kept"),
        "(not: a table)"
    );
    std::fs::remove_file(assets.join(LOUDNESS_TABLE_FILE)).expect("remove");
    // A damaged cooked table is refused by code and stages nothing.
    let cooked = cooked_cache_dir(&root, host_platform()).join(LOUDNESS_TABLE_FILE);
    let good = std::fs::read(&cooked).expect("cooked table");
    std::fs::write(&cooked, &good[..good.len() / 2]).expect("truncate");
    let err = stage_loudness_table(&root, host_platform(), &assets).expect_err("damaged");
    assert!(err.starts_with("loudness_table_invalid"), "{err}");
    assert!(files_under(&assets).is_empty(), "{:?}", files_under(&assets));
    // Restored, it stages byte for byte, with no temporary file left.
    std::fs::write(&cooked, &good).expect("restore");
    let staged = stage_loudness_table(&root, host_platform(), &assets)
        .expect("stage")
        .expect("staged");
    assert_eq!(std::fs::read(&staged).expect("staged"), good);
    assert_eq!(files_under(&assets), vec![staged]);
    let _ = std::fs::remove_dir_all(&root);
}
