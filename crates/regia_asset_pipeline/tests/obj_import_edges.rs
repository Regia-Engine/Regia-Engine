// SPDX-License-Identifier: MPL-2.0
//! OBJ importer edge cases: triangulation, index forms, hostile numbers, limits, path safety,
//! material mapping, and repeated-run determinism. No GPU, no goldens, no clock.

use std::path::Path;

use regia_asset_pipeline::obj_import::{
    ObjError, ObjImportSettings, ObjLimits, convert_obj_to_glb, read_obj_file,
};
use regia_asset_pipeline::{Forward, import_gltf};
use regia_test_utils::unique_temp_dir;

fn convert(text: &str) -> Result<regia_asset_pipeline::obj_import::ObjConversion, ObjError> {
    convert_obj_to_glb(text.as_bytes(), None, &ObjImportSettings::default())
}

fn code(text: &str) -> &'static str {
    convert(text).expect_err("expected an error").code()
}

fn with_limits(limits: ObjLimits) -> ObjImportSettings {
    ObjImportSettings {
        limits,
        ..ObjImportSettings::default()
    }
}

const TRI: &str = "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n";

#[test]
fn a_quad_and_a_pentagon_fan_into_two_and_three_triangles() {
    let quad = convert("v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3 4\n").expect("quad");
    assert_eq!((quad.report.vertices, quad.report.triangles), (4, 2));
    let pent = convert("v 0 0 0\nv 1 0 0\nv 2 1 0\nv 1 2 0\nv 0 1 0\nf 1 2 3 4 5\n").expect("pentagon");
    assert_eq!((pent.report.vertices, pent.report.triangles), (5, 3));
}

#[test]
fn negative_indices_count_back_from_the_vertices_defined_so_far() {
    let rel = convert("v 0 0 0\nv 1 0 0\nv 0 1 0\nf -3 -2 -1\n").expect("relative");
    let abs = convert(TRI).expect("absolute");
    assert_eq!(rel.glb, abs.glb);
    // A relative index only sees the vertices before the face.
    assert_eq!(code("v 0 0 0\nv 1 0 0\nf -3 -2 -1\nv 0 1 0\n"), "obj_parse");
}

#[test]
fn a_parse_error_names_its_line() {
    let err = convert("v 0 0 0\nv 1 0 0\nf 1 2\n").expect_err("two corners");
    assert!(matches!(err, ObjError::Parse { line: 3, .. }), "{err:?}");
    let err = convert("# comment\nv 0 0 zero\n").expect_err("bad number");
    assert!(matches!(err, ObjError::Parse { line: 2, .. }), "{err:?}");
    assert!(err.to_string().contains("line 2"), "{err}");
    // Too few numbers.
    assert_eq!(code("v 1 2\n"), "obj_parse");
    // A face that reaches past what is defined.
    assert_eq!(code("v 0 0 0\nf 1 2 3\n"), "obj_parse");
    assert_eq!(code("v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1/1 2/1 3/1\n"), "obj_parse");
    assert_eq!(code("v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1//1 2//1 3//1\n"), "obj_parse");
}

#[test]
fn files_without_faces_are_no_mesh() {
    assert_eq!(code(""), "obj_no_mesh");
    assert_eq!(code("\n# only a comment\n"), "obj_no_mesh");
    assert_eq!(code("v 0 0 0\nv 1 0 0\nv 0 1 0\n"), "obj_no_mesh");
    assert_eq!(code("o empty\n"), "obj_no_mesh");
}

#[test]
fn non_finite_coordinates_become_zero_and_the_glb_still_imports() {
    let conv = convert("v nan inf -inf\nv 1 0 0\nv 0 1 0\nf 1 2 3\n").expect("converts");
    let mesh = import_gltf(&conv.glb, Forward::Keep).expect("imports");
    for p in &mesh.positions {
        assert!(p.iter().all(|c| c.is_finite()), "{p:?}");
    }
    // The sanitized vertex sits at the origin: the extent is the unit triangle.
    let max_x = mesh.positions.iter().map(|p| p[0]).fold(f32::MIN, f32::max);
    let min_x = mesh.positions.iter().map(|p| p[0]).fold(f32::MAX, f32::min);
    assert_eq!((min_x, max_x), (0.0, 1.0));
}

#[test]
fn scale_applies_and_a_bad_scale_falls_back_to_one() {
    let extent = |scale: f32| {
        let settings = ObjImportSettings {
            scale,
            ..ObjImportSettings::default()
        };
        let conv = convert_obj_to_glb(TRI.as_bytes(), None, &settings).expect("converts");
        let mesh = import_gltf(&conv.glb, Forward::Keep).expect("imports");
        mesh.positions.iter().map(|p| p[0]).fold(f32::MIN, f32::max)
    };
    assert_eq!(extent(1.0), 1.0);
    assert_eq!(extent(0.01), 0.01);
    for bad in [0.0, -2.0, f32::NAN, f32::INFINITY] {
        assert_eq!(extent(bad), 1.0, "scale {bad}");
    }
}

#[test]
fn the_v_texture_coordinate_is_flipped() {
    let conv =
        convert("v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 0 1\nf 1/1 2/2 3/3\n").expect("converts");
    let doc = gltf::Gltf::from_slice(&conv.glb).expect("glb");
    let blob = doc.blob.as_deref();
    let prim = doc
        .meshes()
        .next()
        .expect("mesh")
        .primitives()
        .next()
        .expect("prim");
    let reader = prim.reader(|b| match b.source() {
        gltf::buffer::Source::Bin => blob,
        gltf::buffer::Source::Uri(_) => None,
    });
    let uvs: Vec<[f32; 2]> = reader.read_tex_coords(0).expect("uv0").into_f32().collect();
    assert_eq!(uvs, vec![[0.0, 1.0], [1.0, 1.0], [0.0, 0.0]]);
}

#[test]
fn an_attribute_some_corners_lack_is_dropped_for_the_whole_primitive() {
    // One corner carries a texture coordinate, the others do not.
    let conv = convert("v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nf 1/1 2 3\n").expect("converts");
    let doc = gltf::Gltf::from_slice(&conv.glb).expect("glb");
    let prim = doc
        .meshes()
        .next()
        .expect("mesh")
        .primitives()
        .next()
        .expect("prim");
    assert!(prim.get(&gltf::Semantic::TexCoords(0)).is_none());
}

#[test]
fn conversion_is_deterministic() {
    let text =
        "o a\nv 0 0 0\nv 1 0 0\nv 0 1 0\nv 1 1 0\nusemtl m1\nf 1 2 3\nusemtl m2\nf 2 4 3\no b\nf 1 2 4\n";
    let a = convert(text).expect("first");
    let b = convert(text).expect("second");
    assert_eq!(a.glb, b.glb);
    assert_eq!(a.report, b.report);
    assert_eq!((a.report.objects, a.report.triangles), (2, 3));
}

#[test]
fn faces_group_by_object_and_material_in_first_use_order() {
    let text =
        "o a\nv 0 0 0\nv 1 0 0\nv 0 1 0\nusemtl red\nf 1 2 3\nusemtl blue\nf 1 2 3\nusemtl red\nf 1 3 2\n";
    let conv = convert(text).expect("converts");
    let doc = gltf::Gltf::from_slice(&conv.glb).expect("glb");
    let mesh = doc.meshes().next().expect("mesh");
    assert_eq!(mesh.primitives().count(), 2, "red and blue");
    // Both materials are unknown here: one warning each.
    assert_eq!(
        conv.report
            .warnings
            .iter()
            .filter(|w| w.contains("no such material"))
            .count(),
        2
    );
}

#[test]
fn limits_are_enforced_with_the_limit_code() {
    let quad = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3 4\n";
    let tight = |f: &dyn Fn(&mut ObjLimits)| {
        let mut limits = ObjLimits::default();
        f(&mut limits);
        convert_obj_to_glb(quad.as_bytes(), None, &with_limits(limits))
    };
    assert_eq!(
        tight(&|l| l.max_vertices = 3).expect_err("vertices").code(),
        "obj_limit"
    );
    assert!(tight(&|l| l.max_vertices = 4).is_ok(), "exactly the limit passes");
    assert_eq!(
        tight(&|l| l.max_indices = 5).expect_err("indices").code(),
        "obj_limit"
    );
    assert!(tight(&|l| l.max_indices = 6).is_ok());
    assert_eq!(
        tight(&|l| l.max_polygon_corners = 3).expect_err("corners").code(),
        "obj_limit"
    );
    assert!(tight(&|l| l.max_polygon_corners = 4).is_ok());
    let objects = "o a\no b\no c\n";
    let limits = ObjLimits {
        max_objects: 2,
        ..ObjLimits::default()
    };
    assert_eq!(
        convert_obj_to_glb(objects.as_bytes(), None, &with_limits(limits))
            .expect_err("objects")
            .code(),
        "obj_limit"
    );
    let limits = ObjLimits {
        max_file_bytes: 10,
        ..ObjLimits::default()
    };
    let err = convert_obj_to_glb(TRI.as_bytes(), None, &with_limits(limits)).expect_err("size");
    assert_eq!(err.code(), "obj_too_large");
}

#[test]
fn reading_a_file_checks_the_size_cap_and_reports_io() {
    let dir = unique_temp_dir("obj-read");
    let path = dir.join("a.obj");
    std::fs::write(&path, TRI).expect("write");
    let exact = ObjLimits {
        max_file_bytes: TRI.len() as u64,
        ..ObjLimits::default()
    };
    assert_eq!(read_obj_file(&path, &exact).expect("exact"), TRI.as_bytes());
    let under = ObjLimits {
        max_file_bytes: TRI.len() as u64 - 1,
        ..ObjLimits::default()
    };
    assert!(matches!(
        read_obj_file(&path, &under),
        Err(ObjError::TooLarge { .. })
    ));
    let err = read_obj_file(&dir.join("missing.obj"), &ObjLimits::default()).expect_err("missing");
    assert_eq!(err.code(), "obj_io");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn settings_strings_parse_default_or_fail_with_a_code() {
    let blank = ObjImportSettings::try_from_meta_string(Some("  \n")).expect("blank");
    assert_eq!(blank, ObjImportSettings::default());
    assert_eq!(
        ObjImportSettings::try_from_meta_string(None).expect("none"),
        ObjImportSettings::default()
    );
    let set =
        ObjImportSettings::try_from_meta_string(Some("(scale: 0.5, limits: (max_vertices: 8))")).expect("ok");
    assert_eq!(set.scale, 0.5);
    assert_eq!(set.limits.max_vertices, 8);
    assert_eq!(
        set.limits.max_objects,
        ObjLimits::default().max_objects,
        "others keep defaults"
    );
    // glTF importer keys sit in the same string and are ignored.
    assert!(ObjImportSettings::try_from_meta_string(Some("(meshlets: true, scale: 2.0)")).is_ok());
    // A typo inside `limits` is an error, not a silently ignored cap.
    let err = ObjImportSettings::try_from_meta_string(Some("(limits: (max_vertexes: 8))")).expect_err("typo");
    assert_eq!(err.code(), "obj_bad_settings");
    assert_eq!(
        ObjImportSettings::try_from_meta_string(Some("(scale: "))
            .expect_err("syntax")
            .code(),
        "obj_bad_settings"
    );
}

fn write(dir: &Path, name: &str, bytes: &[u8]) {
    if let Some(parent) = dir.join(name).parent() {
        std::fs::create_dir_all(parent).expect("dirs");
    }
    std::fs::write(dir.join(name), bytes).expect("write");
}

#[test]
fn mtllib_stays_inside_the_source_directory() {
    let root = unique_temp_dir("obj-mtl");
    let dir = root.join("model");
    std::fs::create_dir_all(&dir).expect("dir");
    write(&root, "secret.mtl", b"newmtl leaked\nKd 1 0 0\n");
    let obj = format!(
        "mtllib ../secret.mtl\nmtllib ..\\secret.mtl\nmtllib {}\n{TRI}",
        root.join("secret.mtl").display()
    );
    let conv =
        convert_obj_to_glb(obj.as_bytes(), Some(&dir), &ObjImportSettings::default()).expect("converts");
    assert_eq!(conv.report.materials, 0, "nothing outside the directory is read");
    assert_eq!(
        conv.report
            .warnings
            .iter()
            .filter(|w| w.contains("not found beside the OBJ"))
            .count(),
        3,
        "{:?}",
        conv.report.warnings
    );
    // The same file inside the directory is read, with either separator.
    write(&dir, "sub/ok.mtl", b"newmtl ok\nKd 0 1 0\nd 0.5\n");
    let obj = format!("mtllib sub\\ok.mtl\nusemtl ok\n{TRI}");
    let conv =
        convert_obj_to_glb(obj.as_bytes(), Some(&dir), &ObjImportSettings::default()).expect("converts");
    assert_eq!(conv.report.materials, 1);
    let doc = gltf::Gltf::from_slice(&conv.glb).expect("glb");
    let material = doc.materials().next().expect("material");
    assert_eq!(
        material.pbr_metallic_roughness().base_color_factor(),
        [0.0, 1.0, 0.0, 0.5]
    );
    assert_eq!(material.alpha_mode(), gltf::material::AlphaMode::Blend);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn without_a_directory_an_mtllib_warns_and_the_mesh_still_converts() {
    let conv = convert(&format!("mtllib a.mtl\n{TRI}")).expect("converts");
    assert!(
        conv.report
            .warnings
            .iter()
            .any(|w| w.contains("no source directory"))
    );
}

#[test]
fn missing_and_undecodable_textures_drop_the_slot_with_a_warning() {
    let dir = unique_temp_dir("obj-tex");
    write(
        &dir,
        "m.mtl",
        b"newmtl m\nmap_Kd missing.png\nmap_Bump bad.tga\nmap_Ke ../escape.png\n",
    );
    write(&dir, "bad.tga", b"not an image");
    let obj = format!("mtllib m.mtl\nusemtl m\n{TRI}");
    let conv =
        convert_obj_to_glb(obj.as_bytes(), Some(&dir), &ObjImportSettings::default()).expect("converts");
    assert!(conv.report.textures.is_empty());
    assert!(conv.report.warnings.iter().any(|w| w.contains("missing.png")));
    assert!(
        conv.report
            .warnings
            .iter()
            .any(|w| w.contains("could not be decoded"))
    );
    assert!(conv.report.warnings.iter().any(|w| w.contains("escape.png")));
    let doc = gltf::Gltf::from_slice(&conv.glb).expect("glb");
    let material = doc.materials().next().expect("material");
    assert!(material.pbr_metallic_roughness().base_color_texture().is_none());
    assert!(material.normal_texture().is_none());
    assert!(material.emissive_texture().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bmp_texture_is_re_encoded_and_a_shared_texture_is_one_image() {
    let dir = unique_temp_dir("obj-bmp");
    let mut bmp = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]))
        .write_to(&mut bmp, image::ImageFormat::Bmp)
        .expect("encode");
    write(&dir, "t.bmp", bmp.get_ref());
    write(
        &dir,
        "m.mtl",
        b"newmtl a\nmap_Kd t.bmp\nnewmtl b\nmap_Kd -s 1 1 1 t.bmp\n",
    );
    let obj = format!("mtllib m.mtl\nusemtl a\n{TRI}usemtl b\nf 1 3 2\n");
    let conv =
        convert_obj_to_glb(obj.as_bytes(), Some(&dir), &ObjImportSettings::default()).expect("converts");
    assert_eq!(conv.report.textures, vec!["t.bmp".to_string()]);
    let doc = gltf::Gltf::from_slice(&conv.glb).expect("glb");
    assert_eq!(doc.images().count(), 1);
    assert_eq!(doc.materials().count(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn shininess_maps_to_roughness_and_explicit_roughness_wins() {
    let dir = unique_temp_dir("obj-rough");
    write(
        &dir,
        "m.mtl",
        b"newmtl hard\nNs 1000\nnewmtl soft\nNs 0\nnewmtl neg\nNs -50\nnewmtl both\nNs 1000\nPr 0.25\nnewmtl none\n",
    );
    let obj = format!("mtllib m.mtl\n{TRI}");
    let conv =
        convert_obj_to_glb(obj.as_bytes(), Some(&dir), &ObjImportSettings::default()).expect("converts");
    let doc = gltf::Gltf::from_slice(&conv.glb).expect("glb");
    let rough: Vec<f32> = doc
        .materials()
        .map(|m| m.pbr_metallic_roughness().roughness_factor())
        .collect();
    assert!(rough[0] < 0.06, "{rough:?}");
    assert_eq!(rough[1], 1.0);
    assert_eq!(rough[2], 1.0, "a negative exponent is clamped, not NaN");
    assert_eq!(rough[3], 0.25);
    assert_eq!(rough[4], 1.0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn out_of_range_material_values_still_yield_a_loadable_glb() {
    let dir = unique_temp_dir("obj-range");
    write(
        &dir,
        "m.mtl",
        b"newmtl odd\nKd 3 -1 0.5\nd 2\nTr 9\nPm 7\nPr -3\n",
    );
    let obj = format!("mtllib m.mtl\nusemtl odd\n{TRI}");
    let conv =
        convert_obj_to_glb(obj.as_bytes(), Some(&dir), &ObjImportSettings::default()).expect("converts");
    let doc = gltf::Gltf::from_slice(&conv.glb).expect("glb");
    let pbr = doc.materials().next().expect("material").pbr_metallic_roughness();
    for v in pbr.base_color_factor() {
        assert!((0.0..=1.0).contains(&v), "base color {v} outside 0..=1");
    }
    assert!(
        (0.0..=1.0).contains(&pbr.metallic_factor()),
        "{}",
        pbr.metallic_factor()
    );
    assert!(
        (0.0..=1.0).contains(&pbr.roughness_factor()),
        "{}",
        pbr.roughness_factor()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
