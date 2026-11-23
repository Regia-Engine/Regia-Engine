// SPDX-License-Identifier: MPL-2.0
//! USD import acceptance tests (ADR-3235). Every fixture is written here, small and by hand.

use std::path::Path;

use super::*;

/// A 2x2 PNG.
fn png() -> Vec<u8> {
    let image = image::RgbaImage::from_fn(2, 2, |x, y| {
        image::Rgba([255 * (x as u8), 128, 255 * (y as u8), 255])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("png");
    out.into_inner()
}

/// The standard unit cube, scaled by `half` (faces of four corners; per-face normals; one UV
/// quad per face) with a textured preview surface reading `texture`.
fn cube(header: &str, half: f32, texture: &str) -> String {
    let h = half;
    format!(
        r#"#usda 1.0
(
    defaultPrim = "World"
    {header}
)

def Xform "World"
{{
    def Mesh "Cube" (
        prepend apiSchemas = ["MaterialBindingAPI"]
    )
    {{
        int[] faceVertexCounts = [4, 4, 4, 4, 4, 4]
        int[] faceVertexIndices = [0, 1, 3, 2, 2, 3, 5, 4, 4, 5, 7, 6, 6, 7, 1, 0, 1, 7, 5, 3, 6, 0, 2, 4]
        point3f[] points = [(-{h}, -{h}, {h}), ({h}, -{h}, {h}), (-{h}, {h}, {h}), ({h}, {h}, {h}), (-{h}, {h}, -{h}), ({h}, {h}, -{h}), (-{h}, -{h}, -{h}), ({h}, -{h}, -{h})]
        normal3f[] normals = [(0, 0, 1), (0, 1, 0), (0, 0, -1), (0, -1, 0), (1, 0, 0), (-1, 0, 0)] (
            interpolation = "uniform"
        )
        texCoord2f[] primvars:st = [(0, 0), (1, 0), (1, 1), (0, 1)] (
            interpolation = "faceVarying"
        )
        int[] primvars:st:indices = [0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3]
        rel material:binding = </World/Looks/Paint>
    }}

    def Scope "Looks"
    {{
        def Material "Paint"
        {{
            token outputs:surface.connect = </World/Looks/Paint/Surface.outputs:surface>

            def Shader "Surface"
            {{
                uniform token info:id = "UsdPreviewSurface"
                color3f inputs:diffuseColor.connect = </World/Looks/Paint/Albedo.outputs:rgb>
                float inputs:roughness = 0.4
                float inputs:metallic = 0
                token outputs:surface
            }}

            def Shader "Albedo"
            {{
                uniform token info:id = "UsdUVTexture"
                asset inputs:file = @{texture}@
                float2 inputs:st.connect = </World/Looks/Paint/Reader.outputs:result>
                float3 outputs:rgb
            }}

            def Shader "Reader"
            {{
                uniform token info:id = "UsdPrimvarReader_float2"
                token inputs:varname = "st"
                float2 outputs:result
            }}
        }}
    }}
}}
"#
    )
}

/// One triangle mesh prim named `name` (body only, no header).
fn triangle(name: &str) -> String {
    format!(
        r#"def Mesh "{name}"
{{
    int[] faceVertexCounts = [3]
    int[] faceVertexIndices = [0, 1, 2]
    point3f[] points = [(0, 0, 0), (1, 0, 0), (0, 0, 1)]
}}
"#
    )
}

fn write(dir: &Path, rel: &str, bytes: &[u8]) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("dirs");
    }
    std::fs::write(path, bytes).expect("write");
}

fn convert_file(path: &Path) -> Result<UsdConversion, UsdError> {
    convert_usd_to_glb(UsdSource::File(path), &UsdImportSettings::default())
}

/// The converted GLB, parsed.
struct Glb {
    doc: gltf::Document,
    blob: Vec<u8>,
}

impl Glb {
    fn new(bytes: &[u8]) -> Self {
        let gltf = gltf::Gltf::from_slice(bytes).expect("the converted GLB parses");
        let blob = gltf.blob.clone().unwrap_or_default();
        Self {
            doc: gltf.document,
            blob,
        }
    }

    fn node(&self, name: &str) -> Option<gltf::Node<'_>> {
        self.doc.nodes().find(|n| n.name() == Some(name))
    }

    fn positions(&self, node: &str) -> Vec<[f32; 3]> {
        let mesh = self.node(node).and_then(|n| n.mesh()).expect("mesh node");
        mesh.primitives()
            .flat_map(|p| {
                p.reader(|_| Some(&self.blob[..]))
                    .read_positions()
                    .map(Iterator::collect::<Vec<_>>)
                    .unwrap_or_default()
            })
            .collect()
    }
}

#[test]
fn a_centimeter_cube_with_uvs_and_a_texture_imports_in_meters() {
    let dir = regia_test_utils::unique_temp_dir("usd-cube-cm");
    write(&dir, "albedo.png", &png());
    write(
        &dir,
        "cube.usda",
        cube("metersPerUnit = 0.01\n    upAxis = \"Y\"", 50.0, "albedo.png").as_bytes(),
    );
    let conversion = convert_file(&dir.join("cube.usda")).unwrap_or_else(|e| panic!("{e} ({})", e.code()));
    let report = &conversion.report;
    assert_eq!(report.format, "usda");
    assert_eq!(report.meshes, 1);
    assert_eq!(
        report.vertices, 24,
        "four corners per face, unwelded across faces"
    );
    assert_eq!(report.triangles, 12);
    assert_eq!(report.materials, 1);
    assert_eq!(report.textures, vec!["albedo.png".to_string()]);
    assert!((report.meters_per_unit - 0.01).abs() < 1e-12);
    assert!((report.scale - 0.01).abs() < 1e-12);
    let glb = Glb::new(&conversion.glb);
    let positions = glb.positions("Cube");
    assert_eq!(positions.len(), 24);
    for p in &positions {
        for c in p {
            assert!((c.abs() - 0.5).abs() < 1e-5, "50 cm is 0.5 m: {p:?}");
        }
    }
    let mesh = glb.node("Cube").and_then(|n| n.mesh()).expect("mesh");
    let primitive = mesh.primitives().next().expect("primitive");
    let uvs: Vec<[f32; 2]> = primitive
        .reader(|_| Some(&glb.blob[..]))
        .read_tex_coords(0)
        .expect("TEXCOORD_0")
        .into_f32()
        .collect();
    assert_eq!(uvs.len(), 24);
    assert!(
        uvs.contains(&[0.0, 1.0]) && uvs.contains(&[1.0, 0.0]),
        "V flipped to glTF: {uvs:?}"
    );
    assert!(primitive.reader(|_| Some(&glb.blob[..])).read_normals().is_some());
    let material = primitive.material();
    assert_eq!(material.name(), Some("Paint"));
    let pbr = material.pbr_metallic_roughness();
    assert!(pbr.base_color_texture().is_some(), "the base color texture");
    assert!((pbr.roughness_factor() - 0.4).abs() < 1e-6);
    assert_eq!(glb.doc.images().count(), 1);

    // The whole import through the glTF importer.
    let imported = import_usd_path(
        &dir.join("cube.usda"),
        AssetId::new_random(),
        Forward::Convert,
        "linux",
        None,
        TextureCookQuality::Low,
    )
    .unwrap_or_else(|e| panic!("import: {e}"));
    assert!(
        imported
            .gltf
            .sub_assets
            .iter()
            .any(|s| s.type_path.ends_with("CookedStandardMaterial") && s.path.contains("Paint")),
        "{:?}",
        imported
            .gltf
            .sub_assets
            .iter()
            .map(|s| &s.path)
            .collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_z_up_file_turns_to_y_up_and_a_settings_scale_applies() {
    let dir = regia_test_utils::unique_temp_dir("usd-z-up");
    let text = format!(
        "#usda 1.0\n(\n    metersPerUnit = 1\n    upAxis = \"Z\"\n)\n\ndef Xform \"Lift\"\n{{\n    \
         double3 xformOp:translate = (0, 0, 5)\n    uniform token[] xformOpOrder = [\"xformOp:translate\"]\n\n{}}}\n",
        triangle("Tri")
    );
    write(&dir, "z.usda", text.as_bytes());
    let conversion = convert_file(&dir.join("z.usda")).expect("convert");
    assert_eq!(conversion.report.up_axis, "Z");
    let glb = Glb::new(&conversion.glb);
    let lift = glb.node("Lift").expect("Lift");
    let (t, _, s) = lift.transform().decomposed();
    assert!(
        (t[0]).abs() < 1e-5 && (t[1] - 5.0).abs() < 1e-5 && t[2].abs() < 1e-5,
        "{t:?}"
    );
    assert!(s.iter().all(|c| (c - 1.0).abs() < 1e-5), "{s:?}");
    let positions = glb.positions("Tri");
    assert!(
        positions
            .iter()
            .any(|p| (p[1] - 1.0).abs() < 1e-5 && p[2].abs() < 1e-5),
        "+Z became +Y: {positions:?}"
    );
    assert!(
        positions.iter().all(|p| p[2].abs() < 1e-5),
        "+Y became -Z, and no point had Y: {positions:?}"
    );

    // Settings: a scale on top of the unit, and an up-axis override.
    let settings = UsdImportSettings::try_from_meta_string(Some("(scale: 2.0, up_axis: Some(\"Y\"))"))
        .expect("settings");
    let scaled = convert_usd_to_glb(UsdSource::File(&dir.join("z.usda")), &settings).expect("scaled");
    assert_eq!(scaled.report.up_axis, "Y");
    let glb = Glb::new(&scaled.glb);
    assert!(glb.positions("Tri").iter().any(|p| (p[2] - 2.0).abs() < 1e-5));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_two_level_reference_and_an_instanced_prim_produce_the_expected_nodes() {
    let dir = regia_test_utils::unique_temp_dir("usd-refs");
    write(
        &dir,
        "props/parts/seat.usda",
        format!(
            "#usda 1.0\n(\n    defaultPrim = \"Seat\"\n)\n\n{}",
            triangle("Seat")
        )
        .as_bytes(),
    );
    write(
        &dir,
        "props/chair.usda",
        b"#usda 1.0\n(\n    defaultPrim = \"Chair\"\n)\n\ndef Xform \"Chair\"\n{\n    def \"Seat\" (\n        prepend references = @./parts/seat.usda@\n    )\n    {\n    }\n}\n",
    );
    let root = format!(
        r#"#usda 1.0
(
    metersPerUnit = 1
)

class Xform "_Leaf"
{{
{leaf}}}

def Xform "Root"
{{
    def "Chair" (
        prepend references = @./props/chair.usda@
    )
    {{
        double3 xformOp:translate = (2, 0, 0)
        uniform token[] xformOpOrder = ["xformOp:translate"]
    }}

    def Xform "A" (
        instanceable = true
        prepend references = </_Leaf>
    )
    {{
    }}

    def Xform "B" (
        instanceable = true
        prepend references = </_Leaf>
    )
    {{
        double3 xformOp:translate = (0, 3, 0)
        uniform token[] xformOpOrder = ["xformOp:translate"]
    }}
}}
"#,
        leaf = triangle("Leaf")
    );
    write(&dir, "root.usda", root.as_bytes());
    let conversion = convert_file(&dir.join("root.usda")).unwrap_or_else(|e| panic!("{e} ({})", e.code()));
    let report = &conversion.report;
    assert_eq!(
        report.layers,
        vec![
            "root.usda".to_string(),
            "props/chair.usda".into(),
            "props/parts/seat.usda".into()
        ]
    );
    let glb = Glb::new(&conversion.glb);
    let chair = glb.node("Chair").expect("Chair from the first reference");
    assert!((chair.transform().decomposed().0[0] - 2.0).abs() < 1e-5);
    let seat = glb.node("Seat").expect("Seat from the second reference");
    assert!(seat.mesh().is_some());
    assert!(chair.children().any(|c| c.name() == Some("Seat")));
    assert!(glb.node("_Leaf").is_none(), "the class prim is abstract");
    let leaves: Vec<gltf::Node<'_>> = glb.doc.nodes().filter(|n| n.name() == Some("Leaf")).collect();
    assert_eq!(leaves.len(), 2, "one Leaf node per instance");
    assert!(leaves.iter().all(|n| n.mesh().is_some()));
    assert_eq!(report.meshes + report.instanced_meshes, 3, "{report:?}");
    assert_eq!(
        report.instanced_meshes, 1,
        "the second instance reuses the first's mesh: {report:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_variant_set_picks_its_selection() {
    let text = format!(
        r#"#usda 1.0
(
    metersPerUnit = 1
)

def Xform "Lamp" (
    variants = {{
        string size = "big"
    }}
    prepend variantSets = "size"
)
{{
    variantSet "size" = {{
        "big" {{
{big}
        }}
        "small" {{
{small}
        }}
    }}
}}
"#,
        big = triangle("BigShade"),
        small = triangle("SmallShade")
    );
    let conversion = convert_usd_to_glb(
        UsdSource::Bytes {
            name: "lamp.usda",
            bytes: text.as_bytes(),
        },
        &UsdImportSettings::default(),
    )
    .unwrap_or_else(|e| panic!("{e} ({})", e.code()));
    let glb = Glb::new(&conversion.glb);
    assert!(glb.node("BigShade").is_some());
    assert!(glb.node("SmallShade").is_none());
    assert!(
        conversion
            .report
            .variant_selections
            .contains(&"/Lamp: size = big".to_string()),
        "{:?}",
        conversion.report.variant_selections
    );
}

#[test]
fn a_reference_cycle_and_an_over_deep_chain_are_refused_with_codes() {
    let dir = regia_test_utils::unique_temp_dir("usd-cycle");
    let referencing = |target: &str| {
        format!(
            "#usda 1.0\n(\n    defaultPrim = \"P\"\n)\n\ndef Xform \"P\" (\n    prepend references = @./{target}@\n)\n{{\n}}\n\n{}",
            triangle("T")
        )
    };
    write(&dir, "a.usda", referencing("b.usda").as_bytes());
    write(&dir, "b.usda", referencing("a.usda").as_bytes());
    let error = convert_file(&dir.join("a.usda")).expect_err("cycle");
    assert_eq!(error.code(), "usd_reference_cycle", "{error}");
    assert!(
        error.to_string().contains("a.usda -> b.usda -> a.usda"),
        "{error}"
    );

    // A chain of six files: fine under the default depth, refused under a depth of 3.
    for i in 0..5 {
        write(
            &dir,
            &format!("l{i}.usda"),
            referencing(&format!("l{}.usda", i + 1)).as_bytes(),
        );
    }
    write(
        &dir,
        "l5.usda",
        format!("#usda 1.0\n(\n    defaultPrim = \"P\"\n)\n\n{}", triangle("P")).as_bytes(),
    );
    let ok = convert_file(&dir.join("l0.usda")).unwrap_or_else(|e| panic!("{e} ({})", e.code()));
    assert_eq!(ok.report.layers.len(), 6);
    let mut settings = UsdImportSettings::default();
    settings.limits.max_reference_depth = 3;
    let error = convert_usd_to_glb(UsdSource::File(&dir.join("l0.usda")), &settings).expect_err("too deep");
    assert_eq!(error.code(), "usd_reference_too_deep", "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_usdz_package_with_a_texture_imports() {
    let dir = regia_test_utils::unique_temp_dir("usd-usdz");
    let mut writer = openusd::usdz::ArchiveWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .add_layer(
            "scene.usda",
            cube("metersPerUnit = 1", 0.5, "tex/albedo.png").as_bytes(),
        )
        .expect("layer");
    writer.add_layer("tex/albedo.png", &png()).expect("texture");
    let package = writer.finish().expect("finish").into_inner();
    write(&dir, "crate.usdz", &package);
    let conversion = convert_file(&dir.join("crate.usdz")).unwrap_or_else(|e| panic!("{e} ({})", e.code()));
    assert_eq!(conversion.report.format, "usdz");
    assert_eq!(conversion.report.triangles, 12);
    assert_eq!(
        conversion.report.textures,
        vec!["crate.usdz[tex/albedo.png]".to_string()]
    );
    let glb = Glb::new(&conversion.glb);
    assert_eq!(glb.doc.images().count(), 1);
    let image = glb.doc.images().next().expect("image");
    assert!(
        matches!(image.source(), gltf::image::Source::View { .. }),
        "embedded"
    );
    // In-memory bytes of the package read the same (nothing outside them is needed).
    let inline = convert_usd_to_glb(
        UsdSource::Bytes {
            name: "crate.usdz",
            bytes: &package,
        },
        &UsdImportSettings::default(),
    )
    .expect("inline package");
    assert_eq!(inline.glb, conversion.glb);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_binary_crate_file_imports_like_its_text() {
    let dir = regia_test_utils::unique_temp_dir("usd-usdc");
    write(&dir, "albedo.png", &png());
    write(
        &dir,
        "cube.usda",
        cube("metersPerUnit = 0.01", 50.0, "albedo.png").as_bytes(),
    );
    let layer = openusd::sdf::Layer::open(dir.join("cube.usda").to_string_lossy()).expect("open text");
    layer
        .export(dir.join("cube.usdc").to_string_lossy())
        .expect("export crate");
    let text = convert_file(&dir.join("cube.usda")).expect("text");
    let binary = convert_file(&dir.join("cube.usdc")).unwrap_or_else(|e| panic!("{e} ({})", e.code()));
    assert_eq!(binary.report.format, "usdc");
    assert_eq!(binary.report.vertices, text.report.vertices);
    assert_eq!(binary.report.triangles, text.report.triangles);
    assert_eq!(binary.report.textures, text.report.textures);
    assert_eq!(binary.glb, text.glb, "the same scene converts to the same bytes");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn converting_twice_is_byte_identical() {
    let dir = regia_test_utils::unique_temp_dir("usd-twice");
    write(&dir, "albedo.png", &png());
    write(
        &dir,
        "cube.usda",
        cube("metersPerUnit = 0.01", 50.0, "albedo.png").as_bytes(),
    );
    let a = convert_file(&dir.join("cube.usda")).expect("first");
    let b = convert_file(&dir.join("cube.usda")).expect("second");
    assert_eq!(a.glb, b.glb);
    assert_eq!(a.inputs, b.inputs);
    // A texture edit changes the input digest.
    write(&dir, "albedo.png", &[png(), vec![0]].concat());
    let c = convert_file(&dir.join("cube.usda")).expect("third");
    assert_ne!(a.inputs, c.inputs);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_and_escaping_paths_warn_and_link_escapes_stay_out() {
    let dir = regia_test_utils::unique_temp_dir("usd-missing");
    let project = dir.join("game");
    std::fs::create_dir_all(project.join("assets")).expect("dirs");
    std::fs::write(project.join(regia_core::brand::MANIFEST_FILE), "").expect("manifest");
    write(
        &dir,
        "outside.usda",
        format!("#usda 1.0\n{}", triangle("Outside")).as_bytes(),
    );
    let text = format!(
        "#usda 1.0\n(\n    metersPerUnit = 1\n)\n\ndef Xform \"Root\"\n{{\n    def \"Gone\" (\n        prepend references = @./gone.usda@\n    )\n    {{\n    }}\n    \
         def \"Out\" (\n        prepend references = @../../outside.usda@\n    )\n    {{\n    }}\n    def \"Abs\" (\n        prepend references = @/etc/hosts.usda@\n    )\n    {{\n    }}\n{}}}\n",
        triangle("Kept")
    );
    write(&project, "assets/root.usda", text.as_bytes());
    let conversion = convert_file(&project.join("assets/root.usda")).unwrap_or_else(|e| panic!("{e}"));
    let warnings = conversion.report.warnings.join("\n");
    assert!(warnings.contains("gone.usda"), "{warnings}");
    assert!(warnings.contains("outside.usda"), "{warnings}");
    assert!(warnings.contains("/etc/hosts.usda"), "{warnings}");
    let glb = Glb::new(&conversion.glb);
    assert!(glb.node("Kept").is_some());
    assert!(glb.node("Outside").is_none());
    #[cfg(unix)]
    {
        // A symlink inside the project that leads out of it is not followed.
        std::os::unix::fs::symlink(dir.join("outside.usda"), project.join("assets/link.usda")).expect("link");
        let linked = "#usda 1.0\ndef \"L\" (\n    prepend references = @./link.usda@\n)\n{\n}\n";
        let text = format!("{linked}{}", triangle("Kept"));
        write(&project, "assets/linked.usda", text.as_bytes());
        let conversion = convert_file(&project.join("assets/linked.usda")).expect("linked");
        assert!(Glb::new(&conversion.glb).node("Outside").is_none());
        assert!(
            conversion
                .report
                .warnings
                .join("\n")
                .contains("outside the import root")
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn display_colors_cameras_lights_and_subsets_convert() {
    let text = r#"#usda 1.0
(
    metersPerUnit = 1
)

def Xform "World"
{
    def Mesh "Two" (
        prepend apiSchemas = ["MaterialBindingAPI"]
    )
    {
        int[] faceVertexCounts = [3, 3]
        int[] faceVertexIndices = [0, 1, 2, 0, 2, 3]
        point3f[] points = [(0, 0, 0), (1, 0, 0), (1, 1, 0), (0, 1, 0)]
        color3f[] primvars:displayColor = [(1, 0, 0)] (
            interpolation = "constant"
        )

        def GeomSubset "Second" (
            prepend apiSchemas = ["MaterialBindingAPI"]
        )
        {
            uniform token elementType = "face"
            uniform token familyName = "materialBind"
            int[] indices = [1]
            rel material:binding = </World/Gold>
        }
    }

    def Material "Gold"
    {
        token outputs:surface.connect = </World/Gold/S.outputs:surface>
        def Shader "S"
        {
            uniform token info:id = "UsdPreviewSurface"
            color3f inputs:diffuseColor = (1, 0.8, 0.2)
            float inputs:metallic = 1
            float inputs:roughness = 0.2
            float inputs:opacity = 0.5
            token outputs:surface
        }
    }

    def Camera "Cam"
    {
        float focalLength = 50
        float verticalAperture = 24
        float2 clippingRange = (0.1, 100)
    }

    def DistantLight "Sun"
    {
        float inputs:intensity = 2
        float inputs:exposure = 1
    }

    def SphereLight "Bulb"
    {
        float inputs:intensity = 3
    }

    def Cube "Implicit"
    {
    }
}
"#;
    let conversion = convert_usd_to_glb(
        UsdSource::Bytes {
            name: "misc.usda",
            bytes: text.as_bytes(),
        },
        &UsdImportSettings::default(),
    )
    .unwrap_or_else(|e| panic!("{e} ({})", e.code()));
    let report = &conversion.report;
    assert_eq!(report.cameras, 1);
    assert_eq!(report.lights, 2);
    assert_eq!(report.triangles, 2);
    assert!(
        report.warnings.iter().any(|w| w.contains("Cube")),
        "{:?}",
        report.warnings
    );
    let glb = Glb::new(&conversion.glb);
    let mesh = glb.node("Two").and_then(|n| n.mesh()).expect("mesh");
    assert_eq!(mesh.primitives().count(), 2, "one primitive per subset");
    let names: Vec<Option<&str>> = mesh.primitives().map(|p| p.material().name()).collect();
    assert!(names.contains(&Some("Gold")), "{names:?}");
    let gold = mesh
        .primitives()
        .map(|p| p.material())
        .find(|m| m.name() == Some("Gold"))
        .expect("gold");
    assert_eq!(gold.alpha_mode(), gltf::material::AlphaMode::Blend);
    assert!((gold.pbr_metallic_roughness().metallic_factor() - 1.0).abs() < 1e-6);
    let red = mesh
        .primitives()
        .map(|p| p.material())
        .find(|m| m.name() != Some("Gold"))
        .expect("display color");
    assert_eq!(
        red.pbr_metallic_roughness().base_color_factor(),
        [1.0, 0.0, 0.0, 1.0]
    );
    let camera = glb.node("Cam").and_then(|n| n.camera()).expect("camera");
    match camera.projection() {
        gltf::camera::Projection::Perspective(p) => {
            let expected = 2.0 * (24.0f32 / 100.0).atan();
            assert!((p.yfov() - expected).abs() < 1e-4, "{}", p.yfov());
            assert!((p.znear() - 0.1).abs() < 1e-5);
        }
        gltf::camera::Projection::Orthographic(_) => panic!("perspective expected"),
    }
}

#[test]
fn hostile_inputs_are_refused_without_panics() {
    let settings = UsdImportSettings::default();
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (b"just words".to_vec(), "usd_not_usd"),
        (b"#usda 1.0\ndef Xform \"A\" {".to_vec(), "usd_parse"),
        (format!("#usda 1.0\n{}{}", "(".repeat(5000), ")".repeat(5000)).into_bytes(), "usd_limit"),
        (b"#usda 1.0\ndef Xform \"A\" {}\n".to_vec(), "usd_no_mesh"),
        (
            b"#usda 1.0\ndef Mesh \"M\" {\n int[] faceVertexCounts = [3]\n int[] faceVertexIndices = [0, 1, 7]\n point3f[] points = [(0,0,0),(1,0,0),(0,1,0)]\n}\n".to_vec(),
            "usd_parse",
        ),
        (
            b"#usda 1.0\ndef Mesh \"M\" {\n int[] faceVertexCounts = [3, 3]\n int[] faceVertexIndices = [0, 1, 2]\n point3f[] points = [(0,0,0),(1,0,0),(0,1,0)]\n}\n".to_vec(),
            "usd_parse",
        ),
        (
            b"#usda 1.0\ndef Mesh \"M\" {\n int[] faceVertexCounts = [-3]\n int[] faceVertexIndices = []\n point3f[] points = [(0,0,0)]\n}\n".to_vec(),
            "usd_parse",
        ),
        (
            b"#usda 1.0\ndef Mesh \"M\" {\n int[] faceVertexCounts = [5000]\n int[] faceVertexIndices = [0]\n point3f[] points = [(0,0,0)]\n}\n".to_vec(),
            "usd_limit",
        ),
        (b"PXR-USDC\x00\x08\x00\x00\x00\x00\x00\x00\xff\xff\xff\xff\xff\xff\xff\x7f".to_vec(), "usd_parse"),
        (b"PK\x03\x04truncated".to_vec(), "usd_parse"),
    ];
    for (bytes, code) in cases {
        let error = convert_usd_to_glb(
            UsdSource::Bytes {
                name: "hostile.usda",
                bytes: &bytes,
            },
            &settings,
        )
        .expect_err("hostile input must be refused");
        assert_eq!(error.code(), code, "{error}");
    }
    let error = convert_usd_to_glb(
        UsdSource::File(Path::new("/nonexistent/regia/absent.usda")),
        &settings,
    )
    .expect_err("missing");
    assert_eq!(error.code(), "usd_io");
    let mut small = UsdImportSettings::default();
    small.limits.max_file_bytes = 4;
    let error = convert_usd_to_glb(
        UsdSource::Bytes {
            name: "x.usda",
            bytes: b"#usda 1.0\n",
        },
        &small,
    )
    .expect_err("too large");
    assert_eq!(error.code(), "usd_too_large");
}

#[test]
fn fuzzed_text_and_package_inputs_never_panic_the_reader() {
    use crate::fuzz_entry::{usd_fixture, usd_load, usdz_fixture, usdz_load};
    usd_load(&usd_fixture()).expect("the text fixture imports");
    usdz_load(&usdz_fixture()).expect("the package fixture imports");
    let reader_failures = |error: &str| error.contains("usd_reader_failed");
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    for (fixture, load) in [
        (
            usd_fixture().into_bytes(),
            (|b: &[u8]| usd_load(&String::from_utf8_lossy(b)).map(|_| ())) as fn(&[u8]) -> Result<(), String>,
        ),
        (
            usdz_fixture(),
            (|b: &[u8]| usdz_load(b).map(|_| ())) as fn(&[u8]) -> Result<(), String>,
        ),
    ] {
        for _ in 0..300 {
            let mut bytes = fixture.clone();
            for _ in 0..1 + next() % 8 {
                if bytes.is_empty() {
                    break;
                }
                let at = (next() % bytes.len() as u64) as usize;
                match next() % 4 {
                    0 => bytes[at] = next() as u8,
                    1 => {
                        bytes.remove(at);
                    }
                    2 => bytes.insert(at, next() as u8),
                    _ => bytes.truncate(at),
                }
            }
            if let Err(error) = load(&bytes) {
                assert!(!reader_failures(&error), "the reader panicked: {error}");
            }
        }
    }
}

#[test]
fn settings_and_error_codes_are_protocol_shaped() {
    assert!(UsdImportSettings::try_from_meta_string(None).is_ok());
    assert!(UsdImportSettings::try_from_meta_string(Some("  ")).is_ok());
    for bad in [
        "(scale: 0.0)",
        "(meters_per_unit: Some(-1.0))",
        "(up_axis: Some(\"X\"))",
        "(limits: (nope: 1))",
    ] {
        assert_eq!(
            UsdImportSettings::try_from_meta_string(Some(bad)).map_err(|e| e.code()),
            Err("usd_bad_settings"),
            "{bad}"
        );
    }
    let errors = [
        UsdError::Io(String::new()),
        UsdError::TooLarge { size: 0, limit: 0 },
        UsdError::NotUsd(String::new()),
        UsdError::Parse(String::new()),
        UsdError::ReferenceCycle(String::new()),
        UsdError::ReferenceTooDeep {
            limit: 0,
            chain: String::new(),
        },
        UsdError::Limit(String::new()),
        UsdError::Settings(String::new()),
        UsdError::NoMesh,
        UsdError::Reader(String::new()),
    ];
    for error in errors {
        let code = error.code();
        assert!(code.starts_with("usd_"), "{code}");
        assert!(
            code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{code}"
        );
    }
    assert!(is_usd_extension("USDZ") && is_usd_extension("usd") && !is_usd_extension("obj"));
}

#[test]
fn a_sublayer_and_a_payload_compose() {
    let dir = regia_test_utils::unique_temp_dir("usd-sublayer-payload");
    write(
        &dir,
        "base.usda",
        format!(
            "#usda 1.0\n\ndef Xform \"World\"\n{{\n{}}}\n",
            triangle("FromBase")
        )
        .as_bytes(),
    );
    write(
        &dir,
        "heavy.usda",
        format!(
            "#usda 1.0\n(\n    defaultPrim = \"Heavy\"\n)\n\n{}",
            triangle("Heavy")
        )
        .as_bytes(),
    );
    let root = "#usda 1.0\n(\n    metersPerUnit = 1\n    subLayers = [@./base.usda@]\n)\n\nover \"World\"\n{\n    \
                double3 xformOp:translate = (0, 0, 7)\n    uniform token[] xformOpOrder = [\"xformOp:translate\"]\n\n    \
                def \"Loaded\" (\n        prepend payload = @./heavy.usda@\n    )\n    {\n    }\n}\n";
    write(&dir, "root.usda", root.as_bytes());
    let conversion = convert_file(&dir.join("root.usda")).unwrap_or_else(|e| panic!("{e} ({})", e.code()));
    assert_eq!(
        conversion.report.layers,
        vec!["root.usda".to_string(), "base.usda".into(), "heavy.usda".into()]
    );
    let glb = Glb::new(&conversion.glb);
    let world = glb.node("World").expect("World defined by the sublayer");
    assert!(
        (world.transform().decomposed().0[2] - 7.0).abs() < 1e-5,
        "the root's opinion wins"
    );
    assert!(glb.node("FromBase").and_then(|n| n.mesh()).is_some());
    assert!(glb.node("Loaded").is_some(), "the payload prim");
    assert!(
        glb.node("Heavy").is_none(),
        "a payload's default prim is the payload prim itself"
    );
    let loaded = glb.node("Loaded").expect("Loaded");
    assert!(loaded.mesh().is_some(), "the payload's mesh is loaded");
    let _ = std::fs::remove_dir_all(&dir);
}
