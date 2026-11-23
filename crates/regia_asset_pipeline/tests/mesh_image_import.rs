// SPDX-License-Identifier: MPL-2.0
//! PLY, STL and PSD importers end to end (ADR-3244): the fixtures are written by the tests (the
//! cube and PSD writers in the library, plus hand-written hostile files), converted, imported
//! through the glTF importer or cooked through `cook_project`. No GPU, no goldens, no clock.

use std::path::Path;

use regia_asset::{AssetManifest, CookedTexture, cooked_cache_dir, cooked_file_name, decode_cooked_file};
use regia_asset_pipeline::ply_import::{PlyEncoding, PlyImportSettings, convert_ply_to_glb, cube_fixture};
use regia_asset_pipeline::psd_import::fixture::{self, Compression};
use regia_asset_pipeline::psd_import::{PsdImportSettings, decode_psd};
use regia_asset_pipeline::stl_import::{
    StlImportSettings, convert_stl_to_glb, cube_fixture_ascii, cube_fixture_binary,
};
use regia_asset_pipeline::{CookOptions, CookReport, Forward, cook_project, host_platform, import_gltf};

fn cook(root: &Path) -> CookReport {
    cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook")
}

/// The cooked payload (header stripped) and type path of `rel`.
fn cooked(root: &Path, rel: &str) -> (String, Vec<u8>) {
    let cache = cooked_cache_dir(root, host_platform());
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("a manifest");
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.path == rel)
        .unwrap_or_else(|| panic!("no manifest entry for {rel}"));
    let bytes = std::fs::read(cache.join(cooked_file_name(entry.id))).expect("cooked file");
    let (_, payload) = decode_cooked_file(&bytes).expect("decode");
    (entry.type_path.clone(), payload.to_vec())
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(path, bytes).expect("write");
}

#[test]
fn a_ply_cube_imports_in_each_encoding_with_colors_and_uvs() {
    for encoding in [
        PlyEncoding::Ascii,
        PlyEncoding::BinaryLittleEndian,
        PlyEncoding::BinaryBigEndian,
    ] {
        let conversion =
            convert_ply_to_glb(&cube_fixture(encoding), &PlyImportSettings::default()).expect("convert");
        assert_eq!(conversion.report.vertices, 8, "{encoding:?}");
        assert_eq!(conversion.report.triangles, 12, "{encoding:?}");
        let mesh = import_gltf(&conversion.glb, Forward::Keep).expect("glTF import");
        assert_eq!(mesh.indices.len(), 36, "{encoding:?}");
        assert_eq!(mesh.colors.len(), mesh.positions.len(), "vertex colors survive");
        assert_eq!(mesh.uvs.len(), mesh.positions.len(), "UVs survive");
        // Vertex 3 of the fixture: (-1, 1, -1), color (90, 165, 128), s = 1, t = 1.
        let at = mesh
            .positions
            .iter()
            .position(|p| *p == [-1.0, 1.0, -1.0])
            .expect("the vertex is there");
        let c = mesh.colors[at];
        for (got, want) in c.iter().zip([90.0 / 255.0, 165.0 / 255.0, 128.0 / 255.0, 1.0]) {
            assert!((got - want).abs() < 1e-3, "{encoding:?} color {c:?}");
        }
        // V is flipped to glTF's top-left origin: t = 1 reads as v = 0.
        assert_eq!(mesh.uvs[at], [1.0, 0.0], "{encoding:?}");
    }
}

#[test]
fn ply_properties_have_their_documented_aliases() {
    let text = "ply\nformat ascii 1.0\nelement vertex 3\n\
        property double x\nproperty double y\nproperty double z\n\
        property ushort diffuse_red\nproperty ushort diffuse_green\nproperty ushort diffuse_blue\nproperty ushort diffuse_alpha\n\
        property float texture_u\nproperty float texture_v\n\
        element face 1\nproperty list uchar uint vertex_index\nend_header\n\
        0 0 0 65535 0 0 65535 0 0\n1 0 0 0 65535 0 65535 1 0\n0 1 0 0 0 65535 0 0 1\n3 0 1 2\n";
    let conversion = convert_ply_to_glb(text.as_bytes(), &PlyImportSettings::default()).expect("convert");
    assert!(conversion.report.has_colors && conversion.report.has_uvs && !conversion.report.has_normals);
    let mesh = import_gltf(&conversion.glb, Forward::Keep).expect("import");
    let red = mesh
        .positions
        .iter()
        .position(|p| *p == [0.0, 0.0, 0.0])
        .expect("vertex 0");
    assert_eq!(mesh.colors[red], [1.0, 0.0, 0.0, 1.0]);
}

#[test]
fn a_ply_point_cloud_is_refused_with_its_code() {
    let text = "ply\nformat binary_little_endian 1.0\nelement vertex 1\nproperty float x\nproperty float y\nproperty float z\nend_header\n";
    let mut bytes = text.as_bytes().to_vec();
    bytes.extend_from_slice(&[0; 12]);
    let e = convert_ply_to_glb(&bytes, &PlyImportSettings::default()).expect_err("point cloud");
    assert_eq!(e.code(), "ply_no_faces");
}

#[test]
fn stl_welding_and_the_millimeter_scale() {
    let settings = StlImportSettings {
        crease_angle_degrees: 180.0,
        ..StlImportSettings::default()
    };
    for bytes in [cube_fixture_binary(10.0), cube_fixture_ascii(10.0).into_bytes()] {
        let conversion = convert_stl_to_glb(&bytes, &settings).expect("convert");
        assert_eq!(conversion.report.vertices_in, 36);
        assert_eq!(conversion.report.vertices, 8, "36 corners weld to the cube's 8");
        let mesh = import_gltf(&conversion.glb, Forward::Keep).expect("glTF import");
        // 10 mm edge: 0.01 m, with the Z-up file turned to Y up.
        let (mut min, mut max) = ([f32::MAX; 3], [f32::MIN; 3]);
        for p in &mesh.positions {
            for k in 0..3 {
                min[k] = min[k].min(p[k]);
                max[k] = max[k].max(p[k]);
            }
        }
        for k in 0..3 {
            assert!((max[k] - min[k] - 0.01).abs() < 1e-6, "axis {k}: {min:?} {max:?}");
        }
    }
    // Inches and a Y-up file.
    let inches = StlImportSettings {
        unit: regia_asset_pipeline::stl_import::StlUnit::Inches,
        up_axis: regia_asset_pipeline::mesh_convert::UpAxis::Y,
        ..settings
    };
    let conversion = convert_stl_to_glb(&cube_fixture_binary(1.0), &inches).expect("convert");
    let mesh = import_gltf(&conversion.glb, Forward::Keep).expect("import");
    let top = mesh.positions.iter().map(|p| p[1]).fold(f32::MIN, f32::max);
    assert!((top - 0.0254).abs() < 1e-6, "{top}");
}

#[test]
fn a_psd_composite_decodes_to_the_source_pixels() {
    let w = 7;
    let h = 5;
    let rgba8: Vec<u8> = (0..w * h)
        .flat_map(|i| [(i * 5) as u8, (i * 11 + 3) as u8, 255 - i as u8, 90 + i as u8])
        .collect();
    for (depth, compression) in [
        (8, Compression::Raw),
        (8, Compression::Rle),
        (16, Compression::Rle),
        (16, Compression::ZipPredict),
    ] {
        let bytes = fixture::write(&fixture::Spec {
            size: (w, h),
            depth,
            gray: false,
            alpha: true,
            compression,
            rgba8: rgba8.clone(),
            layers: vec![fixture::Layer {
                name: "Background".into(),
                origin: (0, 0),
                size: (1, 1),
                rgba8: vec![0; 4],
                effects: false,
                divider: 0,
            }],
            psb: false,
        });
        let image = decode_psd(&bytes, &PsdImportSettings::default()).expect("decode");
        assert_eq!(image.rgba8, rgba8, "depth {depth} {compression:?}");
    }
}

#[test]
fn a_psd_cooks_like_the_same_png() {
    let root = regia_test_utils::unique_temp_dir("psd-cook");
    let (w, h) = (16u32, 16u32);
    let rgba8: Vec<u8> = (0..w * h)
        .flat_map(|i| [(i % w * 16) as u8, (i / w * 16) as u8, 99, 255])
        .collect();
    write(
        &root,
        "assets/tex/a.psd",
        &fixture::write(&fixture::Spec {
            size: (w, h),
            depth: 8,
            gray: false,
            alpha: false,
            compression: Compression::Rle,
            rgba8: rgba8.clone(),
            layers: Vec::new(),
            psb: false,
        }),
    );
    let mut png = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_raw(w, h, rgba8)
        .expect("image")
        .write_to(&mut png, image::ImageFormat::Png)
        .expect("png");
    write(&root, "assets/tex/a.png", png.get_ref());
    let report = cook(&root);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let (psd_type, psd) = cooked(&root, "assets/tex/a.psd");
    let (png_type, png_payload) = cooked(&root, "assets/tex/a.png");
    assert_eq!(psd_type, png_type);
    assert_eq!(psd_type, "regia_asset::CookedTexture");
    assert_eq!(psd, png_payload, "the PSD cooks to the same texture as the PNG");
    let texture = CookedTexture::from_postcard(&psd).expect("texture");
    assert_eq!((texture.width, texture.height), (w, h));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_named_psd_layer_cooks_through_the_meta_setting() {
    let root = regia_test_utils::unique_temp_dir("psd-layer-cook");
    let sprite: Vec<u8> = (0..8 * 8).flat_map(|i| [i as u8, 200, 10, 255]).collect();
    write(
        &root,
        "assets/tex/ui.psd",
        &fixture::write(&fixture::Spec {
            size: (32, 32),
            depth: 8,
            gray: false,
            alpha: false,
            compression: Compression::Rle,
            rgba8: vec![255; 32 * 32 * 4],
            layers: vec![fixture::Layer {
                name: "Sprite".into(),
                origin: (4, 4),
                size: (8, 8),
                rgba8: sprite,
                effects: false,
                divider: 0,
            }],
            psb: false,
        }),
    );
    let first = cook(&root);
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    let (_, whole) = cooked(&root, "assets/tex/ui.psd");
    let whole = CookedTexture::from_postcard(&whole).expect("texture");
    assert_eq!((whole.width, whole.height), (32, 32));
    // Point the .meta at the layer: the recook picks it up.
    let meta_path = regia_asset::meta_path_for(&root.join("assets/tex/ui.psd"));
    let mut meta =
        regia_asset::MetaSidecar::from_ron(&std::fs::read(&meta_path).expect("meta")).expect("parse");
    meta.settings = Some("(layer: Some(\"Sprite\"))".into());
    std::fs::write(&meta_path, meta.to_ron_string().expect("ron")).expect("write meta");
    let second = cook(&root);
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    let (_, layer) = cooked(&root, "assets/tex/ui.psd");
    let layer = CookedTexture::from_postcard(&layer).expect("texture");
    assert_eq!((layer.width, layer.height), (8, 8));
    // A missing layer is a cook error with the documented code.
    meta.settings = Some("(layer: Some(\"Nope\"))".into());
    std::fs::write(&meta_path, meta.to_ron_string().expect("ron")).expect("write meta");
    let third = cook(&root);
    assert_eq!(third.errors.len(), 1, "{:?}", third.errors);
    assert!(
        format!("{:?}", third.errors).contains("Nope"),
        "{:?}",
        third.errors
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn ply_and_stl_cook_to_meshes_and_cook_the_same_bytes_twice() {
    let mut first_payloads = Vec::new();
    for pass in 0..2 {
        let root = regia_test_utils::unique_temp_dir(&format!("ply-stl-cook-{pass}"));
        write(
            &root,
            "assets/m/cube.ply",
            &cube_fixture(PlyEncoding::BinaryLittleEndian),
        );
        write(&root, "assets/m/part.stl", &cube_fixture_binary(25.0));
        let report = cook(&root);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let mut payloads = Vec::new();
        for rel in ["assets/m/cube.ply", "assets/m/part.stl"] {
            let (type_path, payload) = cooked(&root, rel);
            assert!(!type_path.is_empty());
            payloads.push(payload);
        }
        if pass == 0 {
            first_payloads = payloads;
        } else {
            assert_eq!(
                first_payloads, payloads,
                "the same sources cook to the same bytes"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn hostile_ply_stl_and_psd_cook_as_errors_not_panics() {
    let root = regia_test_utils::unique_temp_dir("hostile-cook");
    let cube = cube_fixture(PlyEncoding::BinaryLittleEndian);
    write(&root, "assets/h/cut.ply", &cube[..cube.len() - 7]);
    write(&root, "assets/h/empty.stl", b"solid x\nendsolid x\n");
    write(&root, "assets/h/junk.psd", b"8BPS\0\x01\0\0\0\0\0\0");
    let report = cook(&root);
    assert_eq!(report.errors.len(), 3, "{:?}", report.errors);
    let text = format!("{:?}", report.errors);
    for needle in ["PLY", "STL", "PSD"] {
        assert!(text.contains(needle), "{needle} in {text}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Binary STL has no magic for the registry harness to break, so its bytes are mutated here: every
/// mutation converts or is refused, none panics.
#[test]
fn mutated_binary_stl_never_panics() {
    use regia_test_utils::fuzz::{FuzzRng, mutate_bytes};
    let fixture = cube_fixture_binary(10.0);
    let mut refused = 0usize;
    for seed in 1..=1500u64 {
        let mut rng = FuzzRng::new(seed);
        let bytes = mutate_bytes(&fixture, &mut rng);
        match convert_stl_to_glb(&bytes, &StlImportSettings::default()) {
            Ok(c) => assert!(c.report.triangles <= c.report.triangles_in),
            Err(e) => {
                assert!(e.code().starts_with("stl_"), "{e}");
                refused += 1;
            }
        }
    }
    assert!(refused > 0, "the mutations must corrupt something");
}
