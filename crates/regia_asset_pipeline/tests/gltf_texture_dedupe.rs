// SPDX-License-Identifier: MPL-2.0
//! Shared external glTF textures (CP_SIZE, ADR-8055). All fixtures are synthetic.

#[path = "support/gltf_shared_project.rs"]
mod fixture;

use fixture::{Project, image_meta};
use regia_asset::PakReader;
use regia_asset::{cooked_file_name, meta_path_for};
use regia_test_utils::write_atomic;

#[test]
fn two_gltfs_share_one_standalone_texture_record_and_material_id() {
    let project = Project::new();
    project.cook();
    let ids = project.texture_ids();
    assert_eq!(
        ids.len(),
        1,
        "one external image must have one CookedTexture record"
    );
    let id = ids[0];
    assert_eq!(project.material_texture("a", false), id);
    assert_eq!(project.material_texture("b", false), id);
    assert_eq!(
        image_meta(&project.0).id,
        id,
        "standalone image owns the shared id"
    );
    assert_eq!(project.manifest().id_for_path("assets/shared.png"), Some(id));
}

#[test]
fn pak_of_shared_external_images_stores_one_ktx2_and_loads_both_materials() {
    let project = Project::new();
    project.cook();
    for compress in [false, true] {
        let pak = project.pack(compress);
        let reader = PakReader::open(&pak).expect("open pak");
        let textures: Vec<_> = reader
            .toc
            .entries
            .iter()
            .filter(|e| e.type_tag == regia_asset::AssetTypeTag::texture().0)
            .collect();
        let total_ktx2: usize = textures
            .iter()
            .map(|entry| {
                let bytes = reader.read_decompressed(entry.id).expect("texture bytes");
                regia_asset::load_texture_bytes(&bytes)
                    .expect("texture")
                    .1
                    .ktx2
                    .len()
            })
            .sum();
        println!(
            "measurement: compress={compress}, textures={}, total KTX2 bytes={total_ktx2}, pak bytes={}",
            textures.len(),
            std::fs::metadata(&pak).expect("pak size").len()
        );
        assert_eq!(textures.len(), 1, "the pak must store the shared KTX2 only once");
        let bytes = reader.read_decompressed(textures[0].id).expect("texture");
        let (header, texture) = regia_asset::load_texture_bytes(&bytes).expect("load cooked texture");
        assert_eq!(header.asset_id, textures[0].id);
        for name in ["a", "b"] {
            assert_eq!(project.material_texture(name, false), textures[0].id);
        }
        let mut server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: project.0.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("asset server");
        server.mount_chunk_pak(&pak, "base", None).expect("mount pak");
        let materials: Vec<_> = project
            .manifest()
            .entries
            .into_iter()
            .filter(|e| e.type_path == "regia_asset::CookedStandardMaterial")
            .collect();
        assert_eq!(materials.len(), 2);
        for row in materials {
            let handle = server.load_material(row.id).expect("runtime material from pak");
            let material = server.materials().get(&handle).expect("material handle");
            assert_eq!(
                material.authored.base.base_color_map,
                Some(regia_asset::TextureSlot::Asset(textures[0].id))
            );
            assert!(matches!(
                server.source_of(row.id),
                regia_asset::AssetSource::Pak(_)
            ));
        }
        let handle = server
            .load_texture(textures[0].id)
            .expect("runtime shared texture from pak");
        assert_eq!(
            server.textures().get(&handle).expect("texture handle").ktx2,
            texture.ktx2
        );
        assert!(matches!(
            server.source_of(textures[0].id),
            regia_asset::AssetSource::Pak(_)
        ));
        println!(
            "shared fixture: 2 models, 256x256 RGBA; compress={compress}, KTX2 bytes={}, avoided duplicate KTX2 bytes={}, pak bytes={}",
            texture.ktx2.len(),
            texture.ktx2.len(),
            std::fs::metadata(pak).expect("pak size").len()
        );
    }
}

#[test]
fn shared_image_with_different_effective_usage_keeps_variants_and_warns() {
    let project = Project::new();
    project.model("b", "../shared.png", true);
    let report = project.cook();
    assert_eq!(project.texture_ids().len(), 2);
    assert_ne!(
        project.material_texture("a", false),
        project.material_texture("b", true)
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("shared_texture_settings") && w.contains("shared.png")),
        "the cook must explain settings variants: {:?}",
        report.warnings
    );
}

#[test]
fn incremental_recook_and_missing_blob_keep_one_shared_texture() {
    let project = Project::new();
    project.cook();
    assert_eq!(project.texture_ids().len(), 1);
    let id = project.texture_ids()[0];
    let file = project.cache().join(cooked_file_name(id));
    let original = std::fs::read(&file).expect("original texture");
    let unchanged = project.cook();
    assert_eq!(
        unchanged.imports_skipped, 2,
        "both models skip, no extra standalone encode"
    );
    assert_eq!(project.texture_ids(), vec![id]);
    assert_eq!(std::fs::read(&file).expect("unchanged texture"), original);
    project.image(23);
    project.cook();
    assert_eq!(project.texture_ids(), vec![id]);
    let edited = std::fs::read(&file).expect("edited texture");
    assert_ne!(edited, original);
    std::fs::remove_file(&file).expect("remove shared output");
    project.cook();
    assert_eq!(std::fs::read(&file).expect("repaired texture"), edited);
    assert_eq!(project.material_texture("a", false), id);
    assert_eq!(project.material_texture("b", false), id);
}

#[test]
fn deleting_one_gltf_preserves_the_other_materials_shared_texture() {
    let project = Project::new();
    project.cook();
    assert_eq!(project.texture_ids().len(), 1);
    let id = project.texture_ids()[0];
    std::fs::remove_file(project.0.join("assets/models/a.gltf")).expect("delete model");
    project.cook();
    assert_eq!(project.texture_ids(), vec![id]);
    assert_eq!(project.material_texture("b", false), id);
    let reader = PakReader::open(&project.pack(true)).expect("remaining pak");
    assert!(regia_asset::load_texture_bytes(&reader.read_decompressed(id).expect("shared image")).is_ok());
    assert!(
        !project
            .manifest()
            .entries
            .iter()
            .any(|e| e.path.starts_with("assets/models/a.gltf"))
    );
}

#[test]
fn image_meta_override_merges_usage_variants_and_changes_both_materials() {
    let project = Project::new();
    project.model("b", "../shared.png", true);
    project.cook();
    assert_eq!(project.texture_ids().len(), 2);
    let mut meta = image_meta(&project.0);
    meta.settings = Some("(usage: Some(normal), max_size: Some(32), stream: Some(false))".into());
    write_atomic(
        &meta_path_for(&project.0.join("assets/shared.png")),
        meta.to_ron_string().expect("meta").as_bytes(),
    )
    .expect("override");
    project.cook();
    assert_eq!(project.texture_ids(), vec![meta.id]);
    assert_eq!(project.material_texture("a", false), meta.id);
    assert_eq!(project.material_texture("b", true), meta.id);
    let bytes = std::fs::read(project.cache().join(cooked_file_name(meta.id))).expect("texture");
    let (_, texture) = regia_asset::load_texture_bytes(&bytes).expect("load");
    assert_eq!((texture.width, texture.height), (32, 32));
}

#[test]
fn equivalent_uri_spellings_share_the_standalone_image() {
    let project = Project::new();
    project.model("b", ".././shared%2Epng", false);
    project.cook();
    assert_eq!(project.texture_ids().len(), 1);
    assert_eq!(
        project.material_texture("a", false),
        project.material_texture("b", false)
    );
}

#[cfg(unix)]
#[test]
fn same_source_with_different_alias_metas_keeps_distinct_settings() {
    let project = Project::new();
    let alias = project.0.join("assets/alias.png");
    std::os::unix::fs::symlink("shared.png", &alias).expect("alias inside project");
    let mut meta = regia_asset::MetaSidecar::new("png", regia_asset::AssetId::new_random());
    meta.settings = Some("(max_size: Some(32), quality: Some(low))".into());
    write_atomic(
        &meta_path_for(&alias),
        meta.to_ron_string().expect("meta").as_bytes(),
    )
    .expect("alias meta");
    project.model("b", "../alias.png", false);
    let report = project.cook();
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("shared_texture_settings")),
        "{:?}",
        report.warnings
    );
    assert_eq!(project.texture_ids().len(), 2);
    let a = project.material_texture("a", false);
    let b = project.material_texture("b", false);
    assert_ne!(a, b);
    for (id, size) in [(a, 256), (b, 32)] {
        let bytes = std::fs::read(project.cache().join(cooked_file_name(id))).expect("texture");
        let (_, texture) = regia_asset::load_texture_bytes(&bytes).expect("load");
        assert_eq!((texture.width, texture.height), (size, size));
    }
    project.cook();
    assert_eq!(project.material_texture("b", false), b, "variant id is stable");
}

#[test]
fn shared_cook_cache_restore_keeps_one_external_image_and_resolves_materials() {
    use regia_asset_pipeline::cook_cache::{SharedCacheChoice, SharedCacheConfig, SharedCacheMode};
    use regia_asset_pipeline::cook_project_with_shared_cache;
    let origin = Project::new();
    let store = regia_test_utils::TempDir::new("gltf-texture-dedupe-shared-cache");
    let config = |mode| {
        let mut config = SharedCacheConfig::new(store.path(), mode);
        config.engine_build_id = Some("gltf-texture-dedupe-test".into());
        SharedCacheChoice::Use(config)
    };
    let options = |project: &Project| regia_asset_pipeline::CookOptions {
        project_root: project.0.clone(),
        platform: "linux".into(),
        incremental: true,
    };
    let first = cook_project_with_shared_cache(options(&origin), &config(SharedCacheMode::ReadWrite))
        .expect("publish cook");
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert_eq!(origin.texture_ids().len(), 1);
    let checkout = Project::new();
    for rel in ["shared.png.meta", "models/a.gltf.meta", "models/b.gltf.meta"] {
        let bytes = std::fs::read(origin.0.join("assets").join(rel)).expect("committed sidecar");
        write_atomic(&checkout.0.join("assets").join(rel), &bytes).expect("checkout sidecar");
    }
    let restored = cook_project_with_shared_cache(options(&checkout), &config(SharedCacheMode::Read))
        .expect("restore cook");
    assert!(restored.errors.is_empty(), "{:?}", restored.errors);
    assert_eq!(
        restored.shared_cache.expect("summary").hits,
        2,
        "both glTF outputs restored"
    );
    assert_eq!(checkout.texture_ids(), origin.texture_ids());
    assert_eq!(
        checkout.material_texture("a", false),
        checkout.material_texture("b", false)
    );
    assert!(checkout.manifest().id_for_path("assets/shared.png").is_some());
}

#[test]
fn direct_import_pixels_match_shared_project_cook_and_embedded_images_stay_private() {
    use base64::Engine as _;
    use regia_asset_pipeline::{Forward, TextureCookQuality, import_gltf_path};
    let project = Project::new();
    let source = project.0.join("assets/models/a.gltf");
    let imported = import_gltf_path(
        &source,
        regia_asset::AssetId::new_random(),
        Forward::Convert,
        "linux",
        None,
        TextureCookQuality::High,
    )
    .expect("direct import");
    let direct = imported
        .sub_assets
        .iter()
        .find(|s| s.path == "texture/0")
        .expect("direct texture");
    project.cook();
    assert_eq!(project.texture_ids().len(), 1);
    let id = project.texture_ids()[0];
    let file = std::fs::read(project.cache().join(cooked_file_name(id))).expect("shared texture");
    assert_eq!(
        regia_asset::decode_cooked_file(&file).expect("header").1,
        direct.payload
    );
    let png = std::fs::read(project.0.join("assets/shared.png")).expect("image");
    let uri = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    );
    project.model("a", &uri, false);
    project.cook();
    assert_eq!(project.material_texture("b", false), id);
    assert_ne!(project.material_texture("a", false), id);
    assert_eq!(
        std::fs::read(project.cache().join(cooked_file_name(id))).expect("external remains"),
        file
    );
    project.model("b", &uri, false);
    project.cook();
    assert_ne!(
        project.material_texture("a", false),
        project.material_texture("b", false),
        "embedded images retain per-model identities"
    );
}

#[test]
fn v1_paks_preserve_per_entry_headers_for_identical_packed_orm_payloads() {
    let project = Project::new();
    for name in ["a", "b"] {
        let path = project.0.join(format!("assets/models/{name}.gltf"));
        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("model")).expect("JSON");
        doc["materials"][0]["pbrMetallicRoughness"] =
            serde_json::json!({"metallicRoughnessTexture":{"index":0}});
        write_atomic(&path, &serde_json::to_vec(&doc).expect("JSON")).expect("ORM model");
    }
    project.cook();
    let manifest = project.manifest();
    let orm: Vec<_> = manifest
        .entries
        .iter()
        .filter(|e| e.path.ends_with("#texture/orm/0"))
        .collect();
    assert_eq!(orm.len(), 2);
    let reader = PakReader::open(&project.pack(false)).expect("v1 pak");
    let files: Vec<_> = orm
        .iter()
        .map(|e| reader.read_decompressed(e.id).expect("ORM file"))
        .collect();
    let (a, a_payload) = regia_asset::decode_cooked_file(&files[0]).expect("first");
    let (b, b_payload) = regia_asset::decode_cooked_file(&files[1]).expect("second");
    assert_eq!(a_payload, b_payload, "synthetic repeated ORM payloads match");
    assert_eq!(a.asset_id, orm[0].id);
    assert_eq!(b.asset_id, orm[1].id);
    assert_ne!(
        files[0], files[1],
        "v1 index ranges include the distinct headers: payload-only sharing needs v2"
    );
}

#[test]
fn live_image_edits_recook_both_models_and_preserve_the_shared_identity() {
    let project = Project::new();
    project.cook();
    let id = project.material_texture("a", false);
    let path = project.cache().join(cooked_file_name(id));
    let before = std::fs::read(&path).expect("before");
    project.image(29);
    let targets = regia_asset_pipeline::affected_sources(
        &project.0.join("assets"),
        "linux",
        &[project.0.join("assets/shared.png")],
    );
    assert_eq!(targets.len(), 2, "both models own the shared image input");
    let results = regia_asset_pipeline::cook_sources(
        &regia_asset_pipeline::CookOptions {
            project_root: project.0.clone(),
            platform: "linux".into(),
            incremental: true,
        },
        &targets,
    )
    .expect("live cook");
    assert!(
        results.iter().all(|r| r.error.is_none() && !r.skipped),
        "{results:?}"
    );
    assert_eq!(project.texture_ids(), vec![id]);
    assert_eq!(project.material_texture("a", false), id);
    assert_eq!(project.material_texture("b", false), id);
    assert_ne!(std::fs::read(path).expect("after"), before);
}

struct EncodeCounter(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl tracing::Subscriber for EncodeCounter {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        if span.metadata().name() == "astc_compress_chain" {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn shared_android_texture_is_encoded_once_and_keeps_the_desktop_id() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let project = Project::new();
    project.cook();
    let desktop_id = project.material_texture("a", false);
    let count = Arc::new(AtomicUsize::new(0));
    let report = tracing::subscriber::with_default(EncodeCounter(count.clone()), || {
        regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
            project_root: project.0.clone(),
            platform: "android".into(),
            incremental: true,
        })
        .expect("Android cook")
    });
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "one actual ASTC encode for two models"
    );
    let cache = regia_asset::cooked_cache_dir(&project.0, "android");
    let manifest = regia_asset::AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("present");
    assert_eq!(manifest.id_for_path("assets/shared.png"), Some(desktop_id));
    let bytes = std::fs::read(cache.join(cooked_file_name(desktop_id))).expect("texture");
    assert_eq!(
        regia_asset::load_texture_bytes(&bytes).expect("load").0.asset_id,
        desktop_id
    );
}

#[cfg(unix)]
#[test]
fn equivalent_explicit_defaults_on_an_alias_share_the_same_image() {
    let project = Project::new();
    let alias = project.0.join("assets/alias.png");
    std::os::unix::fs::symlink("shared.png", &alias).expect("alias inside project");
    let mut meta = regia_asset::MetaSidecar::new("png", regia_asset::AssetId::new_random());
    meta.settings = Some("(max_size: Some(0), quality: Some(high), compress: Some(true), stream: Some(true), stream_min_px: Some(0))".into());
    write_atomic(
        &meta_path_for(&alias),
        meta.to_ron_string().expect("meta").as_bytes(),
    )
    .expect("alias meta");
    project.model("b", "../alias.png", false);
    let report = project.cook();
    assert_eq!(project.texture_ids(), vec![image_meta(&project.0).id]);
    assert_eq!(
        project.material_texture("a", false),
        project.material_texture("b", false)
    );
    assert!(
        !report
            .warnings
            .iter()
            .any(|w| w.contains("shared_texture_settings"))
    );
    assert_eq!(project.cook().imports_skipped, 2);
    assert_eq!(project.texture_ids().len(), 1);
}

#[test]
fn live_meta_edit_retires_the_old_settings_variant() {
    let project = Project::new();
    project.model("b", "../shared.png", true);
    project.cook();
    let mut meta = image_meta(&project.0);
    meta.settings = Some("(usage: Some(normal))".into());
    let path = meta_path_for(&project.0.join("assets/shared.png"));
    write_atomic(&path, meta.to_ron_string().expect("meta").as_bytes()).expect("meta write");
    let targets = regia_asset_pipeline::affected_sources(&project.0.join("assets"), "linux", &[path]);
    let results = regia_asset_pipeline::cook_sources(
        &regia_asset_pipeline::CookOptions {
            project_root: project.0.clone(),
            platform: "linux".into(),
            incremental: true,
        },
        &targets,
    )
    .expect("live cook");
    assert!(results.iter().all(|r| r.error.is_none()), "{results:?}");
    assert_eq!(
        project.texture_ids(),
        vec![meta.id],
        "retired variants leave the live registry"
    );
    assert_eq!(project.material_texture("a", false), meta.id);
    assert_eq!(project.material_texture("b", true), meta.id);
}

#[test]
fn referenced_l16_images_are_texture_only_without_an_explicit_heightfield_rule() {
    use image::ImageEncoder as _;
    for uri in ["shared.png", "../shared.png"] {
        let project = Project::new();
        let pixels: Vec<u8> = (0..256 * 256u32).flat_map(|i| (i as u16).to_ne_bytes()).collect();
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&pixels, 256, 256, image::ExtendedColorType::L16)
            .expect("16-bit PNG");
        let path = if uri == "shared.png" {
            std::fs::remove_file(project.0.join("assets/shared.png")).expect("remove default image");
            project.0.join("assets/models/shared.png")
        } else {
            project.0.join("assets/shared.png")
        };
        write_atomic(&path, &png).expect("L16 image");
        project.model("a", uri, false);
        project.model("b", uri, false);
        for _ in 0..2 {
            project.cook();
            assert!(
                project
                    .manifest()
                    .entries
                    .iter()
                    .all(|row| row.type_path != "regia_asset::Heightfield"),
                "{uri}: a glTF texture must not silently add a standalone heightfield"
            );
            assert_eq!(project.texture_ids().len(), 1);
            assert_eq!(
                project.material_texture("a", false),
                project.material_texture("b", false)
            );
        }
        let targets = regia_asset_pipeline::affected_sources(
            &project.0.join("assets"),
            "linux",
            std::slice::from_ref(&path),
        );
        assert_eq!(targets.len(), 2, "only the two models consume the image");
        let sources = regia_asset_pipeline::AssetDatabase::discover_sources(&project.0.join("assets"));
        assert!(
            regia_asset_pipeline::gltf_sidecars(&sources).contains(&path),
            "pak freshness skips texture-only sidecars"
        );
    }
}

#[test]
fn a_heightfield_source_keeps_its_primary_type_and_shares_a_texture_variant() {
    use image::ImageEncoder as _;
    let project = Project::new();
    let pixels: Vec<u8> = (0..256 * 256u32).flat_map(|i| (i as u16).to_ne_bytes()).collect();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&pixels, 256, 256, image::ExtendedColorType::L16)
        .expect("16-bit PNG");
    write_atomic(&project.0.join("assets/shared.png"), &png).expect("heightfield image");
    std::fs::create_dir_all(project.0.join("config")).expect("config");
    write_atomic(
        &project.0.join("config/import_rules.ron"),
        br#"(
        format_version: 1,
        rules: [(glob: "assets/shared.png", importer: "png", settings: "(heightfield: Some(true))")],
    )"#,
    )
    .expect("explicit heightfield rule");
    let checked = regia_asset_pipeline::import_rules::check_project(&project.0, None);
    assert!(
        checked.findings.is_empty(),
        "heightfield opt-in must be a supported import rule: {:?}",
        checked.findings
    );
    project.cook();
    let image_id = image_meta(&project.0).id;
    let row = project
        .manifest()
        .entries
        .into_iter()
        .find(|e| e.path == "assets/shared.png")
        .expect("the standalone heightfield survives sharing");
    assert_eq!(row.id, image_id);
    assert_eq!(row.type_path, "regia_asset::Heightfield");
    assert_eq!(
        project.texture_ids().len(),
        1,
        "glTF RGBA8 views share one variant"
    );
    let texture_id = project.texture_ids()[0];
    assert_ne!(
        texture_id, image_id,
        "a texture cannot replace a heightfield's id"
    );
    assert_eq!(project.material_texture("a", false), texture_id);
    assert_eq!(project.material_texture("b", false), texture_id);
    let targets = regia_asset_pipeline::affected_sources(
        &project.0.join("assets"),
        "linux",
        &[project.0.join("assets/shared.png")],
    );
    assert_eq!(
        targets.len(),
        3,
        "live edits reach the heightfield and both glTF consumers"
    );
    assert_eq!(project.cook().imports_skipped, 3);
    let heightfield_file = project.cache().join(cooked_file_name(image_id));
    let before = std::fs::read(&heightfield_file).expect("heightfield");
    write_atomic(&project.0.join("assets/shared.png"), b"broken PNG").expect("break image");
    let broken = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
        project_root: project.0.clone(),
        platform: "linux".into(),
        incremental: true,
    })
    .expect("failed sources are reported");
    assert!(!broken.errors.is_empty());
    assert_eq!(
        project.manifest().id_for_path("assets/shared.png"),
        Some(image_id)
    );
    assert_eq!(
        std::fs::read(heightfield_file).expect("previous heightfield"),
        before
    );
    assert_eq!(project.texture_ids(), vec![texture_id]);
}

#[test]
fn legacy_private_texture_meta_changes_only_image_identities() {
    use regia_asset_pipeline::{Forward, TextureCookQuality, import_gltf_path};
    let project = Project::new();
    let mut previous = Vec::new();
    for name in ["a", "b"] {
        let source = project.0.join(format!("assets/models/{name}.gltf"));
        let parent = regia_asset::AssetId::new_random();
        // A direct import keeps v0.5.0's private image identities outside a project cook.
        let imported = import_gltf_path(
            &source,
            parent,
            Forward::Convert,
            "linux",
            None,
            TextureCookQuality::High,
        )
        .expect("legacy import");
        let mut meta = regia_asset::MetaSidecar::new("gltf", parent);
        meta.sub_assets = imported.meta_entries;
        write_atomic(
            &meta_path_for(&source),
            meta.to_ron_string().expect("legacy meta").as_bytes(),
        )
        .expect("persist legacy ids");
        previous.push((source, meta));
    }
    project.cook();
    assert_eq!(project.texture_ids().len(), 1);
    let shared = image_meta(&project.0).id;
    for (source, old) in previous {
        let new =
            regia_asset::MetaSidecar::from_ron(&std::fs::read(meta_path_for(&source)).expect("new meta"))
                .expect("parse new meta");
        assert_eq!(new.id, old.id, "model identity is unchanged");
        for entry in old.sub_assets {
            let after = new
                .sub_assets
                .iter()
                .find(|s| s.path == entry.path)
                .expect("same local path");
            if entry.path == "texture/0" {
                assert_ne!(after.id, entry.id, "the private texture id is retired");
                assert_eq!(
                    after.id, shared,
                    "the stored image index resolves the new shared id"
                );
            } else {
                assert_eq!(after.id, entry.id, "{} remains stable", entry.path);
            }
        }
    }
}

#[test]
fn switching_shared_image_compression_updates_the_canonical_record_type() {
    let project = Project::new();
    let image_path = project.0.join("assets/shared.png");
    let mut meta = regia_asset::MetaSidecar::new("png", regia_asset::AssetId::new_random());
    let id = meta.id;
    for compress in [false, true, false] {
        meta.settings = Some(format!("(compress: Some({compress}))"));
        write_atomic(
            &meta_path_for(&image_path),
            meta.to_ron_string().expect("meta").as_bytes(),
        )
        .expect("image settings");
        project.cook();
        let manifest = project.manifest();
        let images: Vec<_> = manifest
            .entries
            .iter()
            .filter(|row| {
                matches!(
                    row.type_path.as_str(),
                    "regia_asset::CookedImage" | "regia_asset::CookedTexture"
                )
            })
            .collect();
        assert_eq!(images.len(), 1, "one canonical image across type changes");
        assert_eq!(images[0].id, id);
        assert_eq!(images[0].path, "assets/shared.png");
        assert_eq!(
            images[0].type_path,
            if compress {
                "regia_asset::CookedTexture"
            } else {
                "regia_asset::CookedImage"
            }
        );
        assert_eq!(project.material_texture("a", false), id);
        assert_eq!(project.material_texture("b", false), id);
        let pak = PakReader::open(&project.pack(true)).expect("pack after type change");
        let bytes = pak.read_decompressed(id).expect("current image");
        if compress {
            regia_asset::load_texture_bytes(&bytes).expect("compressed texture");
        } else {
            let header = regia_asset::decode_cooked_file(&bytes).expect("image header").0;
            assert_eq!(
                header.type_tag,
                regia_core::id::TypePathHash::of("regia_asset::CookedImage")
            );
        }
    }
}

#[test]
fn a_clean_cache_does_not_make_embedded_images_reuse_a_shared_external_id() {
    use base64::Engine as _;
    let project = Project::new();
    project.cook();
    assert_eq!(project.texture_ids().len(), 1);
    let external = image_meta(&project.0).id;
    for (name, seed) in [("a", 11), ("b", 23)] {
        project.image(seed);
        let uri = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD
                .encode(std::fs::read(project.0.join("assets/shared.png")).expect("PNG"))
        );
        project.model(name, &uri, false);
    }
    std::fs::remove_dir_all(project.cache()).expect("clean platform cache");
    project.cook();
    let a = project.material_texture("a", false);
    let b = project.material_texture("b", false);
    assert_ne!(
        a, b,
        "different embedded images must have private ids without a prior manifest"
    );
    assert_ne!(a, external, "the standalone image keeps its own identity");
    assert_ne!(b, external, "the standalone image keeps its own identity");
    assert_eq!(project.texture_ids().len(), 3);
    let a = std::fs::read(project.cache().join(cooked_file_name(a))).expect("embedded a");
    let b = std::fs::read(project.cache().join(cooked_file_name(b))).expect("embedded b");
    assert_ne!(
        regia_asset::decode_cooked_file(&a).expect("a").1,
        regia_asset::decode_cooked_file(&b).expect("b").1
    );
}

#[test]
fn book_two_chunk_example_places_shared_images_and_checks_requires() {
    use regia_asset::registry::{AssetRegistry, cache_registry_path};
    use regia_asset_pipeline::chunks::{ChunkRules, Reason, SharedPolicy, assign, check_references};
    let page = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../book/user-guide/content-chunks.md"
    ))
    .expect("the shared-image chunk upgrade guide must ship with its runnable example");
    // The page opens with the general chunk-rules example; the shared-image example is the first
    // one in its own section (ADR-10723).
    let example = page
        .split_once("\n## Shared glTF images and content chunks\n")
        .expect("the shared-image section of the content chunks page")
        .1
        .split_once("```toml\n")
        .expect("TOML example")
        .1
        .split_once("```")
        .expect("closed example")
        .0;
    let mut rules = ChunkRules::from_manifest_toml(example).expect("book chunk rules");
    assert!(rules.warnings.is_empty(), "{:?}", rules.warnings);
    let project = Project::new();
    project.cook();
    let registry = AssetRegistry::read_from(&cache_registry_path(&project.cache()))
        .expect("registry")
        .expect("present");
    let image = image_meta(&project.0).id;
    assert_eq!(project.material_texture("a", false), image);
    assert_eq!(project.material_texture("b", false), image);
    assert_eq!(project.manifest().id_for_path("assets/shared.png"), Some(image));
    let packed_owners = |rules: &ChunkRules,
                         assignment: &regia_asset_pipeline::chunks::Assignment,
                         tag: &str| {
        let out = project.0.join(tag);
        let options = regia_asset_pipeline::chunks::ChunkWriteOptions {
            compress: true,
            min_ratio: 0.0,
            gpu_meshes: false,
            build_id: "shared-image-chunks".into(),
            platform: "linux".into(),
            compatible_with: Vec::new(),
        };
        let (manifest, _) =
            regia_asset_pipeline::chunks::write_chunks(&project.cache(), &out, assignment, rules, &options)
                .expect("write actual chunk paks");
        manifest
            .chunks
            .iter()
            .filter_map(|chunk| {
                let pak = PakReader::open(&out.join(&chunk.file)).expect("chunk pak");
                if !pak.toc.entries.iter().any(|e| e.id == image) {
                    return None;
                }
                let bytes = pak.read_decompressed(image).expect("shared texture in chunk");
                assert_eq!(
                    regia_asset::load_texture_bytes(&bytes)
                        .expect("load chunk texture")
                        .0
                        .asset_id,
                    image
                );
                Some(chunk.name.clone())
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(rules.shared, SharedPolicy::Base);
    let assignment = assign(&registry, &rules, &[]).expect("assign example");
    assert_eq!(
        assignment.chunk_of[&image], "base",
        "equal follow claims place the image in base"
    );
    assert_eq!(assignment.reason[&image], Reason::Shared);
    assert_eq!(packed_owners(&rules, &assignment, "base-policy"), vec!["base"]);
    for name in ["a", "b"] {
        let model = project
            .manifest()
            .id_for_path(&format!("assets/models/{name}.gltf"))
            .expect("model id");
        assert_eq!(assignment.chunk_of[&model], name);
    }
    assert!(check_references(&registry, &assignment, &rules).is_empty());
    rules.shared = SharedPolicy::Duplicate;
    let duplicate = assign(&registry, &rules, &[]).expect("duplicate policy");
    assert_eq!(duplicate.duplicates[&image], vec!["a", "b"]);
    assert_eq!(
        packed_owners(&rules, &duplicate, "duplicate-policy"),
        vec!["a", "b"]
    );
    assert!(check_references(&registry, &duplicate, &rules).is_empty());
    rules.shared = SharedPolicy::Base;
    rules
        .rules
        .get_mut("a")
        .expect("chunk a")
        .include
        .push("shared.png".into());
    let explicit = assign(&registry, &rules, &[]).expect("image path matches a");
    assert_eq!(explicit.chunk_of[&image], "a");
    let problems = check_references(&registry, &explicit, &rules);
    assert!(
        !problems.is_empty(),
        "b must require the image's explicitly selected chunk a"
    );
    assert!(
        problems.iter().all(|p| p.reason == "chunk_unlisted_reference"),
        "{problems:?}"
    );
    rules
        .rules
        .get_mut("b")
        .expect("chunk b")
        .requires
        .push("a".into());
    assert!(check_references(&registry, &explicit, &rules).is_empty());
}

#[test]
fn same_directory_l16_heightfield_rule_round_trips_without_stale_rows() {
    use image::ImageEncoder as _;
    for (normal, live) in [(false, false), (true, false), (false, true), (true, true)] {
        let project = Project::new();
        std::fs::remove_file(project.0.join("assets/shared.png")).expect("remove default image");
        let path = project.0.join("assets/models/shared.png");
        let pixels: Vec<u8> = (0..256 * 256u32).flat_map(|i| (i as u16).to_ne_bytes()).collect();
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&pixels, 256, 256, image::ExtendedColorType::L16)
            .expect("L16 PNG");
        write_atomic(&path, &png).expect("image");
        for name in ["a", "b"] {
            // Check primary-id reuse and non-default variants, which cannot hide a stale
            // heightfield by replacing the same manifest path.
            project.model(name, "shared.png", normal);
        }
        std::fs::create_dir_all(project.0.join("config")).expect("config");
        let mut primary = None;
        for (step, enabled) in [true, false, true].into_iter().enumerate() {
            write_atomic(&project.0.join("config/import_rules.ron"), format!(r#"(
            format_version: 1,
            rules: [(glob: "assets/models/shared.png", importer: "png", settings: "(heightfield: Some({enabled}))")],
        )"#).as_bytes()).expect("rule");
            if live && step > 0 {
                let targets = regia_asset_pipeline::affected_sources(
                    &project.0.join("assets"),
                    "linux",
                    std::slice::from_ref(&path),
                );
                let results = regia_asset_pipeline::cook_sources(
                    &regia_asset_pipeline::CookOptions {
                        project_root: project.0.clone(),
                        platform: "linux".into(),
                        incremental: true,
                    },
                    &targets,
                )
                .expect("live rule transition");
                assert!(results.iter().all(|r| r.error.is_none()), "{results:?}");
            } else {
                project.cook();
            }
            let meta =
                regia_asset::MetaSidecar::from_ron(&std::fs::read(meta_path_for(&path)).expect("meta"))
                    .expect("parse");
            assert_eq!(
                *primary.get_or_insert(meta.id),
                meta.id,
                "primary identity stays stable"
            );
            let heightfields: Vec<_> = project
                .manifest()
                .entries
                .into_iter()
                .filter(|e| e.type_path == "regia_asset::Heightfield")
                .collect();
            assert_eq!(
                heightfields.len(),
                usize::from(enabled),
                "heightfield setting {enabled}"
            );
            if enabled {
                assert_eq!(heightfields[0].id, meta.id);
                let bytes =
                    std::fs::read(project.cache().join(cooked_file_name(meta.id))).expect("heightfield blob");
                assert_eq!(
                    regia_asset::decode_cooked_file(&bytes)
                        .expect("header")
                        .0
                        .type_tag,
                    regia_core::id::TypePathHash::of("regia_asset::Heightfield")
                );
            }
            assert_eq!(project.texture_ids().len(), 1);
            assert_eq!(
                project.material_texture("a", normal),
                project.material_texture("b", normal)
            );
            let sources = regia_asset_pipeline::AssetDatabase::discover_sources(&project.0.join("assets"));
            assert_eq!(
                regia_asset_pipeline::gltf_sidecars(&sources).contains(&path),
                !enabled
            );
            let targets = regia_asset_pipeline::affected_sources(
                &project.0.join("assets"),
                "linux",
                std::slice::from_ref(&path),
            );
            assert_eq!(targets.len(), if enabled { 3 } else { 2 });
            let results = regia_asset_pipeline::cook_sources(
                &regia_asset_pipeline::CookOptions {
                    project_root: project.0.clone(),
                    platform: "linux".into(),
                    incremental: true,
                },
                &targets,
            )
            .expect("live cook");
            assert!(results.iter().all(|r| r.error.is_none()), "{results:?}");
        }
    }
}
