// SPDX-License-Identifier: MPL-2.0
//! Public error paths and small behaviors of the asset pipeline that no other test pins:
//! the KTX2 writer's size checks, the asset database file, pak writing without cooked files,
//! image import errors, the host platform tag and the glass-crack cooker's failures.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;

use regia_asset::{AssetId, AssetManifest, AssetTypeTag, asset_db_path, meta_path_for};
use regia_asset_pipeline::lod_classes::glob_match;
use regia_asset_pipeline::{
    AssetDatabase, AstcBlock, BcnFormat, DbEntry, DbError, Forward, GlassCrackCookError, ImportError,
    PakWriteError, PakWriteOptions, cook_glasscrack, gpu_meshes_for, host_platform, import_gltf,
    import_png_height, import_png_rgba8, pack_cooked_directory, parse_lod_name, write_ktx2_astc,
    write_ktx2_bcn, write_pak,
};
use regia_test_utils::unique_temp_dir;

fn png_bytes(img: image::DynamicImage) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .expect("encode png");
    out.into_inner()
}

// ---- KTX2 writer ----------------------------------------------------------------------------

#[test]
fn ktx2_rejects_zero_dimensions() {
    let err = write_ktx2_bcn(0, 4, BcnFormat::Bc7Srgb, &[vec![0u8; 16]]).expect_err("expected an error");
    assert!(err.to_string().contains("0x4"), "{err}");
    let err =
        write_ktx2_astc(4, 0, AstcBlock::Block4x4, true, &[vec![0u8; 16]]).expect_err("expected an error");
    assert!(err.to_string().contains("4x0"), "{err}");
}

#[test]
fn ktx2_rejects_a_mip_of_the_wrong_size() {
    // BC7 4x4 is one 16-byte block.
    let err = write_ktx2_bcn(4, 4, BcnFormat::Bc7Srgb, &[vec![0u8; 15]]).expect_err("expected an error");
    let text = err.to_string();
    assert!(
        text.contains("mip 0") && text.contains("15") && text.contains("16"),
        "{text}"
    );
    // The second level of an 8x8 BC1 chain is one 8-byte block, not two.
    let err = write_ktx2_bcn(8, 8, BcnFormat::Bc1Srgb, &[vec![0u8; 32], vec![0u8; 16]])
        .expect_err("expected an error");
    assert!(err.to_string().contains("mip 1"), "{err}");
}

#[test]
fn ktx2_writes_every_bcn_format_and_the_reader_agrees() {
    let cases = [
        (BcnFormat::Bc1Srgb, 8usize),
        (BcnFormat::Bc1Linear, 8),
        (BcnFormat::Bc3Srgb, 16),
        (BcnFormat::Bc3Linear, 16),
        (BcnFormat::Bc7Linear, 16),
        (BcnFormat::Bc7Srgb, 16),
        (BcnFormat::Bc5, 16),
        (BcnFormat::Bc4, 8),
    ];
    for (format, block_bytes) in cases {
        // 8x8 base (four blocks), 4x4 second level (one block).
        let mips = [vec![0u8; block_bytes * 4], vec![0u8; block_bytes]];
        let blob = write_ktx2_bcn(8, 8, format, &mips).unwrap_or_else(|e| panic!("{format:?}: {e}"));
        let reader = ktx2::Reader::new(&blob).expect("reader");
        let header = reader.header();
        assert_eq!((header.pixel_width, header.pixel_height), (8, 8), "{format:?}");
        assert_eq!(header.level_count, 2, "{format:?}");
        let levels: Vec<usize> = reader.levels().map(|l| l.data.len()).collect();
        assert_eq!(levels, [block_bytes * 4, block_bytes], "{format:?}");
    }
}

#[test]
fn ktx2_srgb_flag_selects_the_astc_format_and_partial_blocks_round_up() {
    for block in [
        AstcBlock::Block4x4,
        AstcBlock::Block5x5,
        AstcBlock::Block6x6,
        AstcBlock::Block8x8,
    ] {
        // 10x10 needs ceil(10/b)^2 blocks of 16 bytes for every block size.
        let b = match block {
            AstcBlock::Block4x4 => 4u32,
            AstcBlock::Block5x5 => 5,
            AstcBlock::Block6x6 => 6,
            AstcBlock::Block8x8 => 8,
        };
        let blocks = (10u32.div_ceil(b) * 10u32.div_ceil(b)) as usize;
        let mips = [vec![0u8; blocks * 16]];
        let srgb = write_ktx2_astc(10, 10, block, true, &mips).expect("srgb");
        let linear = write_ktx2_astc(10, 10, block, false, &mips).expect("linear");
        let f_srgb = ktx2::Reader::new(&srgb).expect("reader").header().format;
        let f_linear = ktx2::Reader::new(&linear).expect("reader").header().format;
        assert!(f_srgb.is_some() && f_linear.is_some(), "{block:?}");
        assert_ne!(f_srgb, f_linear, "{block:?}: sRGB and linear must differ");
    }
}

// ---- Asset database -------------------------------------------------------------------------

fn entry(path: &str) -> DbEntry {
    DbEntry {
        source_path: path.to_string(),
        asset_id: AssetId::new_random(),
        source_hash: [1; 32],
        settings_hash: [2; 32],
        importer: "png".to_string(),
        importer_version: 3,
        cache_key: [4; 32],
        cooked_rel: "x.fckd".to_string(),
    }
}

#[test]
fn database_open_creates_the_directory_and_returns_empty() {
    let root = unique_temp_dir("db-open");
    let db = AssetDatabase::open(&root).expect("open");
    assert!(db.entries.is_empty());
    assert!(asset_db_path(&root).parent().expect("parent").is_dir());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn database_round_trips_and_save_leaves_no_temp_file() {
    let root = unique_temp_dir("db-roundtrip");
    let mut db = AssetDatabase::default();
    for p in ["assets/b.png", "assets/a.png"] {
        db.entries.insert(p.to_string(), entry(p));
    }
    db.save(&root).expect("save");
    let path = asset_db_path(&root);
    assert!(path.is_file());
    assert!(
        !path.with_extension("bin.tmp").exists(),
        "rename must consume the temp file"
    );
    let loaded = AssetDatabase::open(&root).expect("reopen");
    assert_eq!(loaded, db);
    let keys: Vec<_> = loaded.entries.keys().cloned().collect();
    assert_eq!(
        keys,
        vec!["assets/a.png", "assets/b.png"],
        "rows are ordered by source path"
    );

    // A second save replaces the first.
    let mut db2 = loaded;
    db2.entries.remove("assets/a.png");
    db2.save(&root).expect("save again");
    assert_eq!(AssetDatabase::open(&root).expect("reopen").entries.len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn database_open_treats_a_corrupt_index_as_empty_instead_of_panicking() {
    let root = unique_temp_dir("db-corrupt");
    let path = asset_db_path(&root);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(&path, [0xFFu8; 7]).expect("write garbage");
    // The database is a cache (ADR-875): a damaged index is logged and opens empty, so the next
    // cook rebuilds it instead of every cook failing.
    match AssetDatabase::open(&root) {
        Ok(db) => assert!(db.entries.is_empty(), "{db:?}"),
        other => panic!("expected an empty database, got {other:?}"),
    }
    // A save over the damaged file gives a database that reopens.
    AssetDatabase::default()
        .save(&root)
        .expect("save over the damaged index");
    assert!(AssetDatabase::open(&root).expect("reopen").entries.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn discover_sources_is_sorted_recursive_and_skips_meta_files() {
    let root = unique_temp_dir("db-discover");
    let assets = root.join("assets");
    std::fs::create_dir_all(assets.join("sub/deeper")).expect("dirs");
    for rel in [
        "z.png",
        "a.png",
        "sub/m.glb",
        "sub/deeper/t.png",
        "a.png.meta",
        "sub/m.glb.meta",
    ] {
        std::fs::write(assets.join(rel), b"x").expect("write");
    }
    let found = AssetDatabase::discover_sources(&assets);
    let rel: Vec<String> = found
        .iter()
        .map(|p| {
            p.strip_prefix(&assets)
                .expect("under assets")
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    let mut sorted = rel.clone();
    sorted.sort();
    assert_eq!(rel, sorted, "paths are sorted");
    assert_eq!(rel.len(), 4, "{rel:?}");
    assert!(rel.iter().all(|p| !p.ends_with(".meta")), "{rel:?}");
    assert!(AssetDatabase::discover_sources(&root.join("missing")).is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn load_or_create_meta_is_stable_and_rejects_a_broken_sidecar() {
    let root = unique_temp_dir("db-meta");
    let source = root.join("assets/tex/albedo.png");
    let first = AssetDatabase::load_or_create_meta(&source, "png").expect("create");
    assert!(
        meta_path_for(&source).is_file(),
        "the sidecar is written next to the source"
    );
    let second = AssetDatabase::load_or_create_meta(&source, "png").expect("load");
    assert_eq!(first.id, second.id, "the id survives a reload");
    // An existing sidecar wins over the importer argument.
    let third = AssetDatabase::load_or_create_meta(&source, "other").expect("load");
    assert_eq!(third.importer, "png");

    let broken = root.join("assets/tex/broken.png");
    std::fs::write(meta_path_for(&broken), "this is not ron (((").expect("write");
    match AssetDatabase::load_or_create_meta(&broken, "png") {
        Err(DbError::Meta(_)) => {}
        other => panic!("expected a meta error, got {other:?}"),
    }
    // The broken sidecar is left alone for the author to fix.
    assert_eq!(
        std::fs::read_to_string(meta_path_for(&broken)).expect("read"),
        "this is not ron ((("
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Pak writing ----------------------------------------------------------------------------

#[test]
fn write_pak_reports_a_missing_cooked_file_with_its_id_and_path() {
    let dir = unique_temp_dir("pak-missing");
    let id = AssetId::new_random();
    let out = dir.join("out.fpak");
    let err = write_pak(
        &out,
        &dir,
        &[(id, AssetTypeTag::mesh().0)],
        &AssetManifest::default(),
        &PakWriteOptions::default(),
    )
    .expect_err("expected an error");
    match &err {
        PakWriteError::MissingCooked { id: got, path, .. } => {
            assert_eq!(*got, id);
            assert!(path.starts_with(&dir), "{}", path.display());
        }
        other => panic!("expected MissingCooked, got {other:?}"),
    }
    assert!(!out.exists(), "no pak is left behind by a failed write");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pak_options_default_to_compressed_base_chunk() {
    let o = PakWriteOptions::default();
    assert!(o.compress);
    assert_eq!(o.chunk_name, "base");
    assert!(o.dependencies.is_empty());
}

#[test]
fn gpu_meshes_stay_off_for_android_and_by_default() {
    // The process environment is not touched: an unset variable (the test default) means off.
    if regia_core::brand::env_var(regia_asset_pipeline::PAK_GPU_MESHES_ENV).is_err() {
        assert!(!gpu_meshes_for(Path::new("/tmp/.regia/cooked/linux")));
    }
    assert!(!gpu_meshes_for(Path::new("/tmp/.regia/cooked/android")));
}

#[test]
fn pack_cooked_directory_of_an_empty_cache_writes_an_empty_base_pak() {
    let cooked = unique_temp_dir("pak-empty-cooked");
    let out = unique_temp_dir("pak-empty-out");
    let paks = pack_cooked_directory(&cooked, &out, &BTreeMap::new(), true).expect("pack");
    assert_eq!(paks.len(), 1);
    assert_eq!(paks[0].file_name().and_then(|n| n.to_str()), Some("base.fpak"));
    let reader = regia_asset::PakReader::open(&paks[0]).expect("open");
    assert!(reader.toc.entries.is_empty());
    let _ = std::fs::remove_dir_all(&cooked);
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn pack_cooked_directory_rejects_a_corrupt_manifest() {
    let cooked = unique_temp_dir("pak-bad-manifest");
    let out = unique_temp_dir("pak-bad-manifest-out");
    std::fs::write(cooked.join("asset_manifest.bin"), [0xFFu8; 5]).expect("write");
    match pack_cooked_directory(&cooked, &out, &BTreeMap::new(), false) {
        Err(PakWriteError::Manifest(_)) => {}
        other => panic!("expected a manifest error, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&cooked);
    let _ = std::fs::remove_dir_all(&out);
}

// ---- Image and glTF import errors -----------------------------------------------------------

#[test]
fn png_import_rejects_garbage_and_keeps_pixels() {
    assert!(matches!(
        import_png_rgba8(b"not a png"),
        Err(ImportError::Image(_))
    ));
    assert!(matches!(import_png_rgba8(&[]), Err(ImportError::Image(_))));

    let img = image::RgbaImage::from_fn(3, 2, |x, y| image::Rgba([x as u8 * 10, y as u8 * 20, 7, 200]));
    let cooked = import_png_rgba8(&png_bytes(image::DynamicImage::ImageRgba8(img.clone()))).expect("import");
    assert_eq!((cooked.width, cooked.height), (3, 2));
    assert_eq!(cooked.rgba8, img.into_raw());
}

#[test]
fn png_import_expands_rgb_to_opaque_rgba() {
    let img = image::RgbImage::from_pixel(2, 2, image::Rgb([9, 8, 7]));
    let cooked = import_png_rgba8(&png_bytes(image::DynamicImage::ImageRgb8(img))).expect("import");
    assert_eq!(cooked.rgba8.len(), 16);
    for px in cooked.rgba8.as_chunks::<4>().0 {
        assert_eq!(*px, [9, 8, 7, 255]);
    }
}

#[test]
fn heightfield_import_requires_16_bit_gray() {
    let eight = image::GrayImage::from_pixel(4, 4, image::Luma([10]));
    match import_png_height(&png_bytes(image::DynamicImage::ImageLuma8(eight))) {
        Err(ImportError::HeightfieldNot16Bit(bits, _)) => assert_eq!(bits, 8),
        other => panic!("expected HeightfieldNot16Bit, got {other:?}"),
    }
    let rgb = image::RgbImage::from_pixel(4, 4, image::Rgb([1, 2, 3]));
    assert!(matches!(
        import_png_height(&png_bytes(image::DynamicImage::ImageRgb8(rgb))),
        Err(ImportError::HeightfieldNot16Bit(..))
    ));
    assert!(matches!(import_png_height(b"junk"), Err(ImportError::Image(_))));

    let samples = [0u16, 1, 256, 65535];
    let gray16 =
        image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(2, 2, samples.to_vec()).expect("buffer");
    let hf = import_png_height(&png_bytes(image::DynamicImage::ImageLuma16(gray16))).expect("import");
    assert_eq!((hf.width, hf.height), (2, 2));
    assert_eq!(hf.samples, samples, "row-major, top to bottom, full 16-bit range");
}

#[test]
fn gltf_import_rejects_non_gltf_bytes_without_panicking() {
    for bytes in [&b""[..], &b"glTF"[..], &b"{ \"asset\": "[..], &[0u8; 64][..]] {
        assert!(import_gltf(bytes, Forward::Convert).is_err(), "{bytes:?}");
    }
}

#[test]
fn gltf_import_without_a_mesh_is_an_error() {
    let json =
        br#"{"asset":{"version":"2.0"},"scenes":[{"nodes":[0]}],"scene":0,"nodes":[{"name":"empty"}]}"#;
    assert!(matches!(
        import_gltf(json, Forward::Keep),
        Err(ImportError::NoMesh)
    ));
}

// ---- Platform tag, LOD names, globs ---------------------------------------------------------

#[test]
fn host_platform_matches_the_runtime_cache_directory_name_on_known_targets() {
    let tag = host_platform();
    assert!(["linux", "windows", "android", "macos"].contains(&tag), "{tag}");
    assert_eq!(tag, regia_asset::current_platform());
}

#[test]
fn lod_names_need_a_stem_and_all_digit_suffix() {
    assert_eq!(parse_lod_name("Hull_LOD0"), Some(("Hull", 0)));
    assert_eq!(parse_lod_name("Hull_LOD12"), Some(("Hull", 12)));
    assert_eq!(parse_lod_name("a_LOD1_LOD2"), Some(("a_LOD1", 2)));
    assert_eq!(parse_lod_name("_LOD1"), None, "empty stem");
    assert_eq!(parse_lod_name("Hull_LOD"), None, "empty suffix");
    assert_eq!(parse_lod_name("Hull_LODx"), None);
    assert_eq!(parse_lod_name("Hull_LOD-1"), None);
    assert_eq!(parse_lod_name("Hull_LOD1b"), None);
    assert_eq!(parse_lod_name("Hull_lod1"), None, "case sensitive");
    assert_eq!(parse_lod_name("Hull"), None);
    assert_eq!(
        parse_lod_name("Hull_LOD99999999999999999999"),
        None,
        "overflow is not a level"
    );
}

#[test]
fn glob_match_edge_cases() {
    assert!(glob_match("**", ""));
    assert!(glob_match("**", "a/b/c.glb"));
    assert!(glob_match("units/**", "units/a/b.glb"));
    assert!(
        glob_match("units/**/hull.glb", "units/hull.glb"),
        "`**/` matches no directory"
    );
    assert!(glob_match("units/**/hull.glb", "units/a/b/hull.glb"));
    assert!(glob_match("units/*.glb", "units/a.glb"));
    assert!(
        !glob_match("units/*.glb", "units/a/b.glb"),
        "`*` stops at a separator"
    );
    assert!(glob_match("units/?.glb", "units/a.glb"));
    assert!(!glob_match("units/?.glb", "units/ab.glb"));
    assert!(
        !glob_match("units/?.glb", "units//.glb"),
        "`?` never matches a separator"
    );
    assert!(!glob_match("units/**", "other/a.glb"));
    // `**/` matches whole folders only: the swallowed-slash flaw the shared matcher fixed
    // (ADR-5586) must not live on in this copy either.
    assert!(
        !glob_match("**/props/*_n.png", "assets/myprops/rock_n.png"),
        "`**/` must not match inside a segment"
    );
    assert!(!glob_match("**/ui/**", "assets/gui/a.png"));
    assert!(glob_match("**/props/*_n.png", "assets/props/rock_n.png"));
    assert!(glob_match("exact.glb", "exact.glb"));
    assert!(!glob_match("exact.glb", "exact.glb2"));
    // Over-long inputs never match instead of blowing up.
    let long = "a/".repeat(100_000);
    assert!(!glob_match("**", &long));
    assert!(!glob_match(&long, "a"));
}

// ---- Glass-crack cooker ---------------------------------------------------------------------

#[test]
fn glasscrack_cook_reports_io_and_format_errors() {
    let dir = unique_temp_dir("glasscrack-errors");
    match cook_glasscrack(&dir.join("missing.glasscrack.ron")) {
        Err(GlassCrackCookError::Io { path, .. }) => assert!(path.ends_with("missing.glasscrack.ron")),
        other => panic!("expected Io, got {other:?}"),
    }
    let bad = dir.join("bad.glasscrack.ron");
    std::fs::write(&bad, "definitely not a crack set").expect("write");
    assert!(matches!(
        cook_glasscrack(&bad),
        Err(GlassCrackCookError::Format(_))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
