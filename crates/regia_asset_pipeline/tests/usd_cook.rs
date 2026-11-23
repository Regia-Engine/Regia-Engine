// SPDX-License-Identifier: MPL-2.0
//! USD cook end to end (ADR-3235): a `.usda` referencing another layer, with a textured material, cooks through the
//! project cook to the glTF outputs; cooking it again from scratch writes byte-identical cooked files; an edit to the
//! referenced layer recooks the referencing file; a reference cycle is a cook error with its code.

use std::collections::BTreeMap;
use std::path::Path;

use regia_asset::{AssetManifest, cooked_cache_dir, cooked_file_name};
use regia_asset_pipeline::{CookOptions, cook_project, host_platform};

const CHAIR: &str = r#"#usda 1.0
(
    defaultPrim = "Chair"
    metersPerUnit = 0.01
    upAxis = "Z"
)

def Xform "Chair"
{
    double3 xformOp:translate = (0, 0, 40)
    uniform token[] xformOpOrder = ["xformOp:translate"]

    def "Seat" (
        prepend references = @./parts/seat.usda@
    )
    {
    }
}
"#;

fn seat(z: f32) -> String {
    format!(
        r#"#usda 1.0
(
    defaultPrim = "Seat"
    metersPerUnit = 0.01
    upAxis = "Z"
)

def Mesh "Seat" (
    prepend apiSchemas = ["MaterialBindingAPI"]
)
{{
    int[] faceVertexCounts = [4]
    int[] faceVertexIndices = [0, 1, 2, 3]
    point3f[] points = [(-20, -20, {z}), (20, -20, {z}), (20, 20, {z}), (-20, 20, {z})]
    texCoord2f[] primvars:st = [(0, 0), (1, 0), (1, 1), (0, 1)] (
        interpolation = "vertex"
    )
    rel material:binding = </Seat/Wood>

    def Material "Wood"
    {{
        token outputs:surface.connect = </Seat/Wood/S.outputs:surface>

        def Shader "S"
        {{
            uniform token info:id = "UsdPreviewSurface"
            color3f inputs:diffuseColor.connect = </Seat/Wood/T.outputs:rgb>
            float inputs:roughness = 0.7
            token outputs:surface
        }}

        def Shader "T"
        {{
            uniform token info:id = "UsdUVTexture"
            asset inputs:file = @../textures/wood.png@
            float3 outputs:rgb
        }}
    }}
}}
"#
    )
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(path, bytes).expect("write");
}

fn png() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(4, 4, image::Rgba([150, 100, 60, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("png");
    out.into_inner()
}

/// The cooked bytes of every manifest row of `assets/chair.usda`, by row path.
fn cooked_chair(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let cache = cooked_cache_dir(root, host_platform());
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("present");
    manifest
        .entries
        .iter()
        .filter(|e| e.path.starts_with("assets/chair.usda"))
        .map(|e| {
            let bytes = std::fs::read(cache.join(cooked_file_name(e.id))).expect("cooked file");
            (format!("{} {}", e.path, e.type_path), bytes)
        })
        .collect()
}

#[test]
fn a_usd_project_cooks_twice_byte_identically_and_recooks_on_a_reference_edit() {
    let root = regia_test_utils::unique_temp_dir("usd-cook");
    write(&root, "assets/chair.usda", CHAIR.as_bytes());
    write(&root, "assets/parts/seat.usda", seat(0.0).as_bytes());
    write(&root, "assets/textures/wood.png", &png());
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let first = cooked_chair(&root);
    let types: Vec<&String> = first.keys().collect();
    assert!(types.iter().any(|t| t.ends_with("CookedMesh")), "{types:?}");
    assert!(types.iter().any(|t| t.ends_with("CookedModel")), "{types:?}");
    assert!(
        types
            .iter()
            .any(|t| t.contains("Wood") && t.ends_with("CookedStandardMaterial")),
        "{types:?}"
    );
    let meta = std::fs::read_to_string(root.join("assets/chair.usda.meta")).expect("meta");
    assert!(meta.contains("usd"), "{meta}");

    // From scratch again: the same bytes.
    std::fs::remove_dir_all(cooked_cache_dir(&root, host_platform())).expect("clear cache");
    let again = cook_project(CookOptions::for_host(root.clone())).expect("cook again");
    assert!(again.errors.is_empty(), "{:?}", again.errors);
    assert_eq!(
        cooked_chair(&root),
        first,
        "a second cook from scratch is byte-identical"
    );

    // Incremental: unchanged files skip; an edit to the referenced layer recooks the chair.
    let unchanged = cook_project(CookOptions::for_host(root.clone())).expect("cook unchanged");
    assert!(unchanged.errors.is_empty(), "{:?}", unchanged.errors);
    assert!(unchanged.skipped >= 2, "{unchanged:?}");
    write(&root, "assets/parts/seat.usda", seat(5.0).as_bytes());
    let edited = cook_project(CookOptions::for_host(root.clone())).expect("cook edited");
    assert!(edited.errors.is_empty(), "{:?}", edited.errors);
    assert_ne!(
        cooked_chair(&root),
        first,
        "the referenced layer's edit reached the chair"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_reference_cycle_is_a_cook_error_with_its_code() {
    let root = regia_test_utils::unique_temp_dir("usd-cook-cycle");
    let referencing = |target: &str| {
        format!(
            "#usda 1.0\n(\n    defaultPrim = \"P\"\n)\n\ndef Xform \"P\" (\n    prepend references = @./{target}@\n)\n{{\n}}\n"
        )
    };
    write(&root, "assets/a.usda", referencing("b.usda").as_bytes());
    write(&root, "assets/b.usda", referencing("a.usda").as_bytes());
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
    let text = format!("{:?}", report.errors);
    assert!(text.contains("USD reference cycle"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}
