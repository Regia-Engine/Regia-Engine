// SPDX-License-Identifier: MPL-2.0
//! glTF extension import (feature audit G9, ADR-252): clearcoat, sheen, specular, anisotropy,
//! texture transform and emissive strength, the unknown-extension side table with its single
//! warning, the handler hook, and hostile input. Khronos sample assets are read when
//! `REGIA_KHRONOS_ASSETS` points at them; the synthetic documents below cover every extension
//! without them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use regia_asset::{AssetId, CookedStandardMaterial};
use regia_asset_pipeline::gltf_extensions::{
    ExtensionScope, ExtensionTarget, GltfExtensionHandler, GltfExtensionRegistry, GltfExtensionTable,
    GltfLightKind, GltfNodeLight, TextureTransform,
};
use regia_asset_pipeline::{
    Forward, TextureCookQuality, import_gltf_glb_bytes, import_gltf_glb_bytes_with_handlers,
};
use regia_test_utils::fuzz::fuzz_loader;
use serde_json::Value;

/// A GLB with one triangle drawn with material 0, whose JSON tail (`materials`, `textures`,
/// ... and any root `extensions`) is `extra`.
fn glb_with(extra: &str) -> Vec<u8> {
    glb_with_node_extensions("{}", extra)
}

/// [`glb_with`] with `node_extensions` (a JSON object) on the node.
fn glb_with_node_extensions(node_extensions: &str, extra: &str) -> Vec<u8> {
    let mut bin = Vec::new();
    for p in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
        for c in p {
            bin.extend_from_slice(&c.to_le_bytes());
        }
    }
    for i in [0u32, 1, 2] {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],
        "nodes":[{{"mesh":0,"name":"tri","extensions":{node_extensions}}}],
        "meshes":[{{"name":"m","primitives":[{{"attributes":{{"POSITION":0}},"indices":1,"material":0}}]}}],
        "buffers":[{{"byteLength":{blen}}}],
        "bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}},{{"buffer":0,"byteOffset":36,"byteLength":12}}],
        "accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},
          {{"bufferView":1,"componentType":5125,"count":3,"type":"SCALAR"}}],
        {extra}}}"#,
        blen = bin.len(),
        node_extensions = node_extensions
    );
    let mut json_bytes = json.into_bytes();
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json_bytes);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out
}

const ALL_EXTENSIONS: &str = r#""materials":[{"name":"M_All",
  "pbrMetallicRoughness":{"baseColorTexture":{"index":0,"extensions":{"KHR_texture_transform":
      {"offset":[0.25,0.5],"rotation":1.5707964,"scale":[2,3],"texCoord":1}}}},
  "extensions":{
    "KHR_materials_clearcoat":{"clearcoatFactor":0.8,"clearcoatRoughnessFactor":0.15,
      "clearcoatNormalTexture":{"index":1,"scale":0.5}},
    "KHR_materials_sheen":{"sheenColorFactor":[0.9,0.2,0.3],"sheenRoughnessFactor":0.4},
    "KHR_materials_specular":{"specularFactor":0.5,"specularColorFactor":[1.0,0.9,0.8]},
    "KHR_materials_anisotropy":{"anisotropyStrength":0.7,"anisotropyRotation":1.0},
    "KHR_materials_emissive_strength":{"emissiveStrength":4.0}}}],
  "textures":[{"source":0},{"source":1}],
  "images":[{"uri":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP43+DwHwAHAAK/K9fH4gAAAABJRU5ErkJggg=="},
            {"uri":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP43+DwHwAHAAK/K9fH4gAAAABJRU5ErkJggg=="}]"#;

fn table(json: &str) -> GltfExtensionTable {
    GltfExtensionTable::from_bytes(json.as_bytes(), &mut GltfExtensionRegistry::new())
}

#[test]
fn every_material_extension_reads_into_the_side_table() {
    let t = GltfExtensionTable::from_bytes(&glb_with(ALL_EXTENSIONS), &mut GltfExtensionRegistry::new());
    let m = t.material(0).expect("material 0");
    let cc = m.clearcoat.as_ref().expect("clearcoat");
    assert!((cc.factor - 0.8).abs() < 1e-6 && (cc.roughness - 0.15).abs() < 1e-6);
    assert!((cc.normal_scale - 0.5).abs() < 1e-6);
    assert_eq!(m.textures["clearcoat_normal"].image, 1);
    let sheen = m.sheen.as_ref().expect("sheen");
    assert_eq!(sheen.color, [0.9, 0.2, 0.3]);
    assert!(sheen.fabric_sheen().is_some());
    let spec = m.specular.as_ref().expect("specular");
    assert!((spec.factor - 0.5).abs() < 1e-6);
    assert_eq!(spec.color, [1.0, 0.9, 0.8]);
    let an = m.anisotropy.as_ref().expect("anisotropy");
    assert!((an.strength - 0.7).abs() < 1e-6 && (an.rotation - 1.0).abs() < 1e-6);
    assert_eq!(m.emissive_strength, Some(4.0));
    let base = m.textures["base_color"];
    assert_eq!(
        base.tex_coord, 1,
        "the transform's texCoord overrides the texture's"
    );
    let tf = base.transform.expect("transform");
    assert_eq!(tf.scale, [2.0, 3.0]);
    // Every extension above is understood: none is unknown.
    assert!(t.unknown.is_empty() && t.unknown_names.is_empty());
}

#[test]
fn texture_transform_applies_translation_rotation_scale_in_that_order() {
    let tf = TextureTransform {
        offset: [0.25, 0.5],
        rotation: std::f32::consts::FRAC_PI_2,
        scale: [2.0, 3.0],
        tex_coord: None,
    };
    // scale (1,1) -> (2,3); rotate 90 degrees: (c*2 + s*3, -s*2 + c*3) = (3, -2); translate.
    let uv = tf.apply([1.0, 1.0]);
    assert!(
        (uv[0] - 3.25).abs() < 1e-5 && (uv[1] - -1.5).abs() < 1e-5,
        "{uv:?}"
    );
    assert_eq!(TextureTransform::IDENTITY.apply([0.3, 0.7]), [0.3, 0.7]);
}

#[test]
fn clearcoat_and_anisotropy_reach_the_cooked_material_and_the_rest_the_side_table() {
    let result = import_gltf_glb_bytes(
        &glb_with(ALL_EXTENSIONS),
        AssetId::new_random(),
        Forward::Keep,
        "desktop",
        TextureCookQuality::High,
    )
    .expect("import");
    let payload = result
        .sub_assets
        .iter()
        .find(|s| s.type_path == "regia_asset::CookedStandardMaterial")
        .expect("material sub-asset");
    let cooked = CookedStandardMaterial::from_postcard(&payload.payload).expect("cooked material");
    let cc = cooked.authored.clearcoat.as_ref().expect("clearcoat");
    assert!((cc.factor - 0.8).abs() < 1e-6 && (cc.roughness - 0.15).abs() < 1e-6);
    assert!(
        cc.normal_map.is_some(),
        "clearcoat normal map is a cooked texture slot"
    );
    assert!(
        payload.dependencies.len() >= 2,
        "the clearcoat normal texture is a dependency of the material"
    );
    let an = cooked.authored.anisotropy.as_ref().expect("anisotropy");
    assert!((an.strength - 0.7).abs() < 1e-6);
    let (s, c) = 1.0f32.sin_cos();
    assert!((an.direction[0] - c).abs() < 1e-6 && (an.direction[1] - s).abs() < 1e-6);
    assert!((cooked.authored.base.emissive_strength - 4.0).abs() < 1e-6);
    // Sheen, specular and the transform are not in the cooked material.
    let ext = result.extensions.material(0).expect("side table");
    assert!(ext.sheen.is_some() && ext.specular.is_some());
}

#[test]
fn zero_strength_clearcoat_and_anisotropy_leave_the_material_plain() {
    let json = r#""materials":[{"extensions":{"KHR_materials_clearcoat":{"clearcoatFactor":0.0},
        "KHR_materials_anisotropy":{"anisotropyStrength":0.0}}}]"#;
    let result = import_gltf_glb_bytes(
        &glb_with(json),
        AssetId::new_random(),
        Forward::Keep,
        "desktop",
        TextureCookQuality::High,
    )
    .expect("import");
    let payload = result
        .sub_assets
        .iter()
        .find(|s| s.type_path == "regia_asset::CookedStandardMaterial")
        .expect("material");
    let cooked = CookedStandardMaterial::from_postcard(&payload.payload).expect("cooked");
    assert!(cooked.authored.clearcoat.is_none() && cooked.authored.anisotropy.is_none());
}

/// Counts `WARN` events; a minimal `tracing` subscriber so the test needs no extra dependency.
struct WarnCounter(Arc<AtomicUsize>);

impl tracing::Subscriber for WarnCounter {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() == tracing::Level::WARN {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

const NODE_TAGS: &str = r#"{"VENDOR_node_tags":{"tags":["a","b"]}}"#;

const UNKNOWN_TWICE: &str = r#""extensionsUsed":["VENDOR_widget","VENDOR_gadget"],
  "materials":[{"extensions":{"VENDOR_widget":{"a":1},"VENDOR_gadget":{"b":[1,2]}}},
               {"extensions":{"VENDOR_widget":{"a":2}}}]"#;

#[test]
fn unknown_extensions_are_kept_and_warn_once_per_file() {
    let warns = Arc::new(AtomicUsize::new(0));
    let t = tracing::subscriber::with_default(WarnCounter(warns.clone()), || {
        GltfExtensionTable::from_bytes(
            &glb_with_node_extensions(NODE_TAGS, UNKNOWN_TWICE),
            &mut GltfExtensionRegistry::new(),
        )
    });
    assert_eq!(
        warns.load(Ordering::SeqCst),
        1,
        "four blocks, three names, one warning"
    );
    assert_eq!(
        t.unknown.len(),
        4,
        "one on the node, two on material 0, one on material 1"
    );
    assert_eq!(
        t.unknown_names,
        ["VENDOR_gadget", "VENDOR_node_tags", "VENDOR_widget"]
    );
    let widgets: Vec<_> = t.unknown_of("VENDOR_widget").collect();
    assert_eq!(widgets.len(), 2);
    assert_eq!(widgets[1].index, 1);
    assert_eq!(widgets[1].scope, ExtensionScope::Material);
    assert_eq!(widgets[1].json["a"], 2);
    // The node-level block is kept with its scope.
    let tags = t.unknown_of("VENDOR_node_tags").next().expect("node block");
    assert_eq!(tags.scope, ExtensionScope::Node);
}

#[test]
fn a_file_with_only_understood_extensions_does_not_warn() {
    let warns = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::with_default(WarnCounter(warns.clone()), || {
        let _ = GltfExtensionTable::from_bytes(
            br#"{"asset":{"version":"2.0"},"materials":[{"extensions":{"KHR_materials_ior":{"ior":1.4}}}]}"#,
            &mut GltfExtensionRegistry::new(),
        );
    });
    assert_eq!(warns.load(Ordering::SeqCst), 0);
}

/// One handler call: scope, index, node name, extension name, payload.
type SeenCall = (ExtensionScope, usize, Option<String>, String, Value);

#[derive(Default)]
struct Seen(Arc<Mutex<Vec<SeenCall>>>);

struct TagHandler(Seen);

impl GltfExtensionHandler for TagHandler {
    fn handles(&self, extension: &str) -> bool {
        extension == "VENDOR_node_tags" || extension == "VENDOR_widget"
    }

    fn handle(&mut self, target: &ExtensionTarget<'_>, extension: &str, json: &Value) {
        self.0.0.lock().expect("lock").push((
            target.scope,
            target.index,
            target.name.map(str::to_string),
            extension.to_string(),
            json.clone(),
        ));
    }
}

#[test]
fn a_registered_handler_reads_node_and_material_extension_json_and_claims_them() {
    let seen = Seen::default();
    let log = seen.0.clone();
    let mut registry = GltfExtensionRegistry::new();
    registry.register(TagHandler(seen));
    let closure_calls = Arc::new(AtomicUsize::new(0));
    let counter = closure_calls.clone();
    registry.on_extension("VENDOR_gadget", move |target, json| {
        assert_eq!(target.scope, ExtensionScope::Material);
        assert_eq!(json["b"][1], 2);
        counter.fetch_add(1, Ordering::SeqCst);
    });
    let result = import_gltf_glb_bytes_with_handlers(
        &glb_with_node_extensions(NODE_TAGS, UNKNOWN_TWICE),
        AssetId::new_random(),
        Forward::Keep,
        "desktop",
        TextureCookQuality::High,
        &mut registry,
    )
    .expect("import");
    let seen = log.lock().expect("lock");
    let tags = seen
        .iter()
        .find(|s| s.3 == "VENDOR_node_tags")
        .expect("node tags");
    assert_eq!(
        (tags.0, tags.1, tags.2.as_deref()),
        (ExtensionScope::Node, 0, Some("tri"))
    );
    assert_eq!(tags.4["tags"][1], "b");
    assert_eq!(seen.iter().filter(|s| s.3 == "VENDOR_widget").count(), 2);
    assert_eq!(closure_calls.load(Ordering::SeqCst), 1);
    // Everything was claimed: nothing is left unknown.
    assert!(
        result.extensions.unknown.is_empty(),
        "{:?}",
        result.extensions.unknown_names
    );
}

#[test]
fn a_handler_sees_known_extensions_it_claims_too() {
    let mut registry = GltfExtensionRegistry::new();
    let got = Arc::new(Mutex::new(None));
    let slot = got.clone();
    registry.on_extension("KHR_materials_clearcoat", move |_, json| {
        *slot.lock().expect("lock") = Some(json["clearcoatFactor"].as_f64());
    });
    let _ = GltfExtensionTable::from_bytes(&glb_with(ALL_EXTENSIONS), &mut registry);
    assert_eq!(*got.lock().expect("lock"), Some(Some(0.8)));
}

#[test]
fn hostile_extension_json_never_panics() {
    let nested = format!(
        r#"{{"asset":{{"version":"2.0"}},"materials":[{{"extensions":{{"KHR_materials_clearcoat":{}1{}}}}}]}}"#,
        "[".repeat(500),
        "]".repeat(500)
    );
    let hostile = [
        r#"{"materials":[{"extensions":{"KHR_materials_clearcoat":{"clearcoatFactor":"x","clearcoatNormalTexture":{"index":-1}}}}]}"#,
        r#"{"materials":[{"extensions":{"KHR_materials_sheen":{"sheenColorFactor":[1e999,"a",null,4,5]}}}]}"#,
        r#"{"materials":[{"extensions":{"KHR_materials_specular":7,"KHR_materials_anisotropy":[1]}}]}"#,
        r#"{"textures":[{"source":99999999999999999999}],"materials":[{"pbrMetallicRoughness":{"baseColorTexture":{"index":0,
            "extensions":{"KHR_texture_transform":{"offset":[1],"rotation":1e308,"scale":"s","texCoord":-3}}}}}]}"#,
        r#"{"materials":[{"extensions":{"KHR_materials_clearcoat":{"clearcoatFactor":1e308,"clearcoatNormalTexture":{"index":4294967296,"scale":1e999}}}}]}"#,
        r#"{"extensions":5,"nodes":[7,null,{"extensions":[]}],"meshes":[{"primitives":3}],"materials":"no"}"#,
        r#"{"extensionsUsed":[1,null],"extensionsRequired":{"a":1}}"#,
        nested.as_str(),
        "",
        "{",
        "null",
    ];
    for json in hostile {
        let t = table(json);
        for m in &t.materials {
            if let Some(cc) = &m.clearcoat {
                assert!(cc.factor.is_finite() && (0.0..=1.0).contains(&cc.factor));
            }
            if let Some(s) = &m.sheen {
                assert!(s.color.iter().all(|c| c.is_finite()));
            }
            if let Some(tf) = m.textures.values().filter_map(|t| t.transform).next() {
                let _ = tf.apply([0.5, 0.5]);
            }
        }
    }
    // GLB framing: truncated, wrong chunk type, huge chunk length.
    let glb = glb_with(ALL_EXTENSIONS);
    for cut in [0, 4, 11, 12, 19, 25, glb.len() / 2] {
        let _ = GltfExtensionTable::from_bytes(&glb[..cut], &mut GltfExtensionRegistry::new());
    }
    let mut bad = glb.clone();
    bad[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    let _ = GltfExtensionTable::from_bytes(&bad, &mut GltfExtensionRegistry::new());
}

#[test]
fn a_bad_image_is_an_import_error_not_a_panic() {
    // Valid base64 that is not an image, broken base64, a non-base64 data URI, an external file
    // with no directory to resolve it, and a PNG cut short.
    let bad_uris = [
        "data:image/png;base64,bm90IGFuIGltYWdl",
        "data:image/png;base64,@@@not base64@@@",
        "data:image/png,%89PNG",
        "data:image/png;base64",
        "texture.png",
        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAf",
    ];
    for uri in bad_uris {
        let extra = format!(
            r#""materials":[{{"pbrMetallicRoughness":{{"baseColorTexture":{{"index":0}}}}}}],
            "textures":[{{"source":0}}],"images":[{{"uri":"{uri}"}}]"#
        );
        let result = std::panic::catch_unwind(|| {
            import_gltf_glb_bytes(
                &glb_with(&extra),
                AssetId::new_random(),
                Forward::Keep,
                "desktop",
                TextureCookQuality::High,
            )
        });
        match result {
            Ok(Err(regia_asset_pipeline::ImportError::Image(_))) => {}
            Ok(Err(other)) => panic!("{uri}: expected an image error, got {other}"),
            Ok(Ok(_)) => panic!("{uri}: a bad image must not import"),
            Err(_) => panic!("{uri}: the importer panicked"),
        }
    }
}

#[test]
fn unknown_blocks_are_capped() {
    let mut mats = String::from(r#"{"materials":["#);
    let n = regia_asset_pipeline::gltf_extensions::MAX_UNKNOWN_EXTENSIONS + 10;
    for i in 0..n {
        if i > 0 {
            mats.push(',');
        }
        mats.push_str(r#"{"extensions":{"VENDOR_x":{}}}"#);
    }
    mats.push_str("]}");
    let t = table(&mats);
    assert_eq!(
        t.unknown.len(),
        regia_asset_pipeline::gltf_extensions::MAX_UNKNOWN_EXTENSIONS
    );
    assert_eq!(t.dropped_unknown, 10);
}

#[test]
fn extension_import_never_panics_on_mutated_glbs() {
    fn run(bytes: &[u8]) -> Result<(), regia_asset_pipeline::ImportError> {
        let mut registry = GltfExtensionRegistry::new();
        registry.on_extension("VENDOR_widget", |_, _| {});
        import_gltf_glb_bytes_with_handlers(
            bytes,
            AssetId::new_random(),
            Forward::Convert,
            "desktop",
            TextureCookQuality::High,
            &mut registry,
        )
        .map(|_| ())
    }
    let fixture = glb_with_node_extensions(NODE_TAGS, ALL_EXTENSIONS);
    fuzz_loader(&fixture, 0x6c7466, 1_000, run);
}

fn khronos(name: &str) -> Option<Vec<u8>> {
    let root = PathBuf::from(regia_core::brand::env_var("REGIA_KHRONOS_ASSETS").ok()?);
    std::fs::read(root.join(name)).ok()
}

/// The Khronos extension samples, when the asset cache has them: each must read its extension.
#[test]
fn khronos_extension_samples_read_their_extension() {
    type Check = fn(&regia_asset_pipeline::gltf_extensions::GltfMaterialExtensions) -> bool;
    let samples: [(&str, Check); 5] = [
        ("ClearCoatTest.glb", (|m| m.clearcoat.is_some()) as Check),
        ("SheenChair.glb", (|m| m.sheen.is_some()) as Check),
        ("SpecularTest.glb", (|m| m.specular.is_some()) as Check),
        ("AnisotropyBarnLamp.glb", (|m| m.anisotropy.is_some()) as Check),
        (
            "TextureTransformTest.glb",
            (|m| m.textures.values().any(|t| t.transform.is_some())) as Check,
        ),
    ];
    let mut ran = 0;
    for (file, check) in samples {
        let Some(bytes) = khronos(file) else {
            assert!(!assets_required(), "{file} is missing and REGIA_REQUIRE_ASSETS=1");
            eprintln!("skipped {file}: not in REGIA_KHRONOS_ASSETS");
            continue;
        };
        let t = GltfExtensionTable::from_bytes(&bytes, &mut GltfExtensionRegistry::new());
        assert!(t.materials.iter().any(check), "{file}: extension not read: {t:?}");
        ran += 1;
    }
    eprintln!("khronos extension samples read: {ran}/5");
}

#[test]
fn primitive_material_variant_mappings_are_read_into_the_table() {
    let t = table(
        r#"{"asset":{"version":"2.0"},"extensions":{"KHR_materials_variants":{"variants":[{"name":"a"}]}},
        "materials":[{},{}],"meshes":[{"primitives":[{"attributes":{"POSITION":0},
        "extensions":{"KHR_materials_variants":{"mappings":[{"material":1,"variants":[0]}]}}}]}]}"#,
    );
    assert!(t.unknown_of("KHR_materials_variants").next().is_none());
    assert_eq!(t.variant_names, vec!["a".to_string()]);
    let mappings = t.primitive_variants(0, 0).expect("read");
    assert_eq!(mappings[0].material, 1);
    assert_eq!(mappings[0].variants, vec![0]);
}

#[test]
fn a_handler_still_sees_the_variant_blocks() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let mut registry = GltfExtensionRegistry::new();
    registry.on_extension("KHR_materials_variants", move |target, json| {
        log.lock()
            .expect("lock")
            .push((target.scope, json["mappings"][0]["material"].clone()));
    });
    let bytes = br#"{"asset":{"version":"2.0"},"meshes":[{"primitives":[{"attributes":{"POSITION":0},
        "extensions":{"KHR_materials_variants":{"mappings":[{"material":1,"variants":[0]}]}}}]}]}"#;
    let _ = GltfExtensionTable::from_bytes(bytes, &mut registry);
    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, ExtensionScope::Primitive);
    assert_eq!(seen[0].1, 1);
}

/// The checked-in fixture: two materials over one quad and two small embedded images.
fn detail_fixture() -> regia_asset_pipeline::GltfImportResult {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/detail_extension.gltf");
    regia_asset_pipeline::import_gltf_path(
        &path,
        AssetId::new_random(),
        Forward::Keep,
        "desktop",
        None,
        TextureCookQuality::High,
    )
    .expect("import")
}

fn cooked_materials(
    result: &regia_asset_pipeline::GltfImportResult,
) -> Vec<(CookedStandardMaterial, Vec<AssetId>)> {
    result
        .sub_assets
        .iter()
        .filter(|s| s.type_path == "regia_asset::CookedStandardMaterial")
        .map(|s| {
            (
                CookedStandardMaterial::from_postcard(&s.payload).expect("cooked material"),
                s.dependencies.clone(),
            )
        })
        .collect()
}

#[test]
fn the_detail_extension_reaches_the_cooked_material() {
    let result = detail_fixture();
    let materials = cooked_materials(&result);
    let (hull, deps) = materials
        .iter()
        .find(|(m, _)| m.authored.name.as_deref() == Some("hull_paint"))
        .expect("hull_paint");
    let detail = hull
        .authored
        .hard_surface
        .as_ref()
        .and_then(|h| h.detail.as_ref())
        .expect("the extension sets the detail layer");
    assert_eq!(detail.uv_scale, [64.0, 32.0]);
    assert_eq!(detail.albedo_strength, 0.25);
    assert_eq!(detail.normal_strength, 0.75);
    assert_eq!(detail.roughness_strength, 0.5);
    assert_eq!((detail.fade_start, detail.fade_end), (2.0, 12.0));
    let normal = detail.normal.as_ref().expect("normal slot");
    let albedo = detail.albedo.as_ref().expect("albedo slot");
    assert_ne!(normal, albedo, "two images, two cooked textures");
    assert!(detail.roughness.is_none() && detail.mask.is_none());
    for slot in [normal, albedo] {
        let regia_asset::TextureSlot::Asset(id) = slot else {
            panic!("a cooked slot names an asset");
        };
        assert!(
            deps.contains(id),
            "the detail texture is a dependency of the material"
        );
    }
    // The material's other hard-surface defaults stay on.
    let hs = hull.authored.hard_surface.as_ref().expect("hard surface");
    assert!(hs.multi_scatter && hs.specular_occlusion);
    assert!(result.extensions.material(0).is_some_and(|e| e.detail.is_some()));
}

#[test]
fn the_extras_convention_sets_the_same_layer_with_defaults() {
    let result = detail_fixture();
    let materials = cooked_materials(&result);
    let (paint, _) = materials
        .iter()
        .find(|(m, _)| m.authored.name.as_deref() == Some("extras_paint"))
        .expect("extras_paint");
    let detail = paint
        .authored
        .hard_surface
        .as_ref()
        .and_then(|h| h.detail.as_ref())
        .expect("extras set the detail layer");
    assert_eq!(detail.uv_scale, [16.0, 16.0]);
    assert_eq!(detail.normal_strength, 0.5);
    assert_eq!(
        detail.albedo_strength, 1.0,
        "omitted strengths stay at their default"
    );
    assert_eq!(detail.fade_end, 0.0);
    assert!(detail.normal.is_some() && detail.albedo.is_none());
}

#[test]
fn a_detail_block_with_hostile_numbers_is_clamped_and_an_unknown_image_is_dropped() {
    let table = table(
        r#"{"materials":[{"extensions":{"REGIA_materials_detail":{
            "uvScale":[-3.0,"wide"],"albedoStrength":9.0,"normalStrength":-1.0,
            "fadeStart":-2.0,"normalTexture":{"index":40}}}}],"textures":[],"images":[]}"#,
    );
    let ext = table.material(0).expect("side table");
    let detail = ext.detail.as_ref().expect("detail");
    assert_eq!(detail.uv_scale, [4.0, 4.0], "a bad scale reads as the default");
    assert_eq!(detail.albedo_strength, 1.0);
    assert_eq!(detail.normal_strength, 0.0);
    assert_eq!(detail.fade_start, 0.0);
    assert!(!ext.textures.contains_key("detail_normal"));
    assert_eq!(
        table.unknown_of("REGIA_materials_detail").count(),
        0,
        "understood"
    );
}

/// `REGIA_REQUIRE_ASSETS=1` (set by a job that must cover the Khronos samples): a missing sample is
/// a failure, not a skip, so lost assets cannot turn these tests vacuous (audit-v035-flaky M1).
fn assets_required() -> bool {
    std::env::var_os("REGIA_REQUIRE_ASSETS").is_some_and(|v| v == "1")
}

/// A file with every kind of `KHR_lights_punctual` light; nodes 0 to 3 use lights 0 to 3.
const LIGHTS_FILE: &str = r#"{"asset":{"version":"2.0"},
    "extensionsUsed":["KHR_lights_punctual"],
    "extensions":{"KHR_lights_punctual":{"lights":[
        {"type":"point","name":"Lamp","color":[1.0,0.5,0.25],"intensity":100.0,"range":8.0},
        {"type":"spot","spot":{"innerConeAngle":0.25,"outerConeAngle":0.5},"intensity":40.0},
        {"type":"spot"},
        {"type":"directional","range":5.0}
    ]}},
    "nodes":[
        {"extensions":{"KHR_lights_punctual":{"light":0}}},
        {"extensions":{"KHR_lights_punctual":{"light":1}}},
        {"extensions":{"KHR_lights_punctual":{"light":2}}},
        {"extensions":{"KHR_lights_punctual":{"light":3}}},
        {"name":"no light here"}
    ]}"#;

#[test]
fn punctual_lights_read_with_the_defaults_of_the_extension() {
    let t = table(LIGHTS_FILE);
    assert!(t.unknown.is_empty(), "read, not kept raw: {:?}", t.unknown);
    assert!(t.fallbacks().is_empty());
    assert_eq!(t.lights.len(), 4);
    assert_eq!(t.lights[0].name.as_deref(), Some("Lamp"));
    assert_eq!(t.lights[0].kind, GltfLightKind::Point);
    assert_eq!(t.lights[0].color, [1.0, 0.5, 0.25]);
    assert_eq!(t.lights[0].intensity, 100.0);
    assert_eq!(t.lights[0].range, Some(8.0));
    assert_eq!(
        t.lights[1].kind,
        GltfLightKind::Spot {
            inner_cone: 0.25,
            outer_cone: 0.5
        }
    );
    assert_eq!(t.lights[1].color, [1.0; 3], "color defaults to white");
    assert_eq!(t.lights[1].range, None, "no range is an unbounded light");
    assert_eq!(
        t.lights[2].kind,
        GltfLightKind::Spot {
            inner_cone: 0.0,
            outer_cone: std::f32::consts::FRAC_PI_4
        },
        "the cone defaults"
    );
    assert_eq!(t.lights[2].intensity, 1.0, "intensity defaults to 1");
    assert_eq!(t.lights[3].kind, GltfLightKind::Directional);
    assert_eq!(t.lights[3].range, None, "a directional light has no range");
    assert_eq!(
        t.node_lights,
        vec![
            GltfNodeLight { node: 0, light: 0 },
            GltfNodeLight { node: 1, light: 1 },
            GltfNodeLight { node: 2, light: 2 },
            GltfNodeLight { node: 3, light: 3 },
        ]
    );
}

#[test]
fn a_light_the_table_drops_does_not_move_the_nodes_of_the_others() {
    let t = table(
        r#"{"extensions":{"KHR_lights_punctual":{"lights":[
            {"type":"laser"},
            {"name":"no type"},
            {"type":"point","name":"Kept"}
        ]}},
        "nodes":[
            {"extensions":{"KHR_lights_punctual":{"light":0}}},
            {"extensions":{"KHR_lights_punctual":{"light":2}}},
            {"extensions":{"KHR_lights_punctual":{"light":9}}},
            {"extensions":{"KHR_lights_punctual":{"light":-1}}},
            {"extensions":{"KHR_lights_punctual":7}}
        ]}"#,
    );
    assert_eq!(t.lights.len(), 1);
    assert_eq!(t.lights[0].name.as_deref(), Some("Kept"));
    assert_eq!(t.node_lights, vec![GltfNodeLight { node: 1, light: 0 }]);
}

#[test]
fn hostile_light_numbers_are_clamped_to_something_a_renderer_can_use() {
    let t = table(
        r#"{"extensions":{"KHR_lights_punctual":{"lights":[
            {"type":"point","intensity":-5.0,"range":-1.0,"color":[-1.0,1e308,"x"]},
            {"type":"point","intensity":1e308,"range":1e308},
            {"type":"spot","spot":{"innerConeAngle":9.0,"outerConeAngle":-3.0}},
            {"type":"spot","spot":{"innerConeAngle":1.0,"outerConeAngle":100.0}},
            {"type":"point","range":"far","intensity":"bright","name":7}
        ]}}}"#,
    );
    assert_eq!(t.lights.len(), 5);
    for light in &t.lights {
        assert!(light.intensity.is_finite() && light.intensity >= 0.0, "{light:?}");
        assert!(
            light.color.iter().all(|c| c.is_finite() && *c >= 0.0),
            "{light:?}"
        );
        assert!(light.range.is_none_or(|r| r.is_finite() && r > 0.0), "{light:?}");
        if let GltfLightKind::Spot {
            inner_cone,
            outer_cone,
        } = light.kind
        {
            assert!(
                outer_cone > 0.0 && outer_cone <= std::f32::consts::FRAC_PI_2,
                "{light:?}"
            );
            assert!(inner_cone >= 0.0 && inner_cone < outer_cone, "{light:?}");
        }
    }
    assert_eq!(t.lights[0].intensity, 0.0);
    assert_eq!(t.lights[0].range, None);
    assert_eq!(t.lights[4].name, None);
}

#[test]
fn a_handler_still_sees_the_light_blocks() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    let mut registry = GltfExtensionRegistry::new();
    registry.on_extension("KHR_lights_punctual", move |target, _| {
        seen.lock().expect("lock").push(target.scope);
    });
    let t = GltfExtensionTable::from_bytes(LIGHTS_FILE.as_bytes(), &mut registry);
    assert_eq!(t.lights.len(), 4, "a handler does not take the table's copy");
    let calls = calls.lock().expect("lock");
    assert_eq!(calls.first(), Some(&ExtensionScope::Asset));
    assert_eq!(calls.len(), 5, "the root and four nodes");
}
