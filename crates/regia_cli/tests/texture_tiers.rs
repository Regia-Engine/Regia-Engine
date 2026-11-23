// SPDX-License-Identifier: MPL-2.0
//! CP_SIZE: tier paks keep the cached mip tail and preserve the default cook.

use std::path::Path;
use std::process::{Command, Output};

use image::ImageEncoder;
use regia_asset::{CookedTexture, PakReader};
use regia_render::CompressedTexture;

fn cook(root: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["cook", "--platform", "linux", "--quality", "fast", "--project"])
        .arg(root)
        .args(extra)
        .env_remove("REGIA_PAK_GPU_MESHES")
        .env_remove("FERRITE_PAK_GPU_MESHES")
        .output()
        .expect("run cook")
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn texture(pak: &PakReader, name: &str) -> (Vec<u8>, CookedTexture) {
    let entry = pak
        .toc
        .manifest
        .iter()
        .find(|e| e.path.ends_with(name))
        .expect("asset");
    let bytes = pak.read_decompressed(entry.id).expect("pak entry");
    let (_, payload) = regia_asset::decode_cooked_file(&bytes).expect("wrapper");
    let texture = CookedTexture::from_postcard(payload).expect("runtime texture load");
    (bytes, texture)
}

#[test]
fn texture_tier_low_cooks_the_4096_chain_tail_without_changing_full_or_small_assets() {
    let scratch = regia_test_utils::TempDir::new("texture-tier-cli");
    let root = scratch.path().to_path_buf();
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    std::fs::write(
        root.join("Regia.toml"),
        "[cook.chunks.textures]\ninclude = [\"*.png\", \"*.material.ron\"]\n",
    )
    .expect("rules");
    for (name, edge) in [("large.png", 4096), ("small.png", 64)] {
        let rgba = [75, 130, 190, 255].repeat((edge * edge) as usize);
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&rgba, edge, edge, image::ExtendedColorType::Rgba8)
            .expect("png");
        std::fs::write(root.join("assets").join(name), png).expect("source");
    }
    // Only material maps have the runtime residency cap. Keep the original byte/tail
    // assertions and give this fixture the consumer whose package is being reduced.
    let mut material = regia_asset::MaterialAssetRon::default_standard();
    material.base.base_color_map = Some(regia_asset::TextureSlot::Path("assets/large.png".into()));
    material.base.emissive_map = Some(regia_asset::TextureSlot::Path("assets/small.png".into()));
    std::fs::write(
        root.join("assets/large.material.ron"),
        ron::to_string(&material).expect("material"),
    )
    .expect("material source");
    success(&cook(&root, &[]));
    let dir = root.join("build/paks/linux");
    let original = std::fs::read(dir.join("textures.fpak")).expect("full pak");
    let full = PakReader::open(&dir.join("textures.fpak")).expect("pak");
    let (_, large) = texture(&full, "large.png");
    let small = texture(&full, "small.png").0;
    let full_manifest = full.toc.manifest.clone();
    drop(full); // Do not hold a Windows file mapping across a later cook's atomic replacement.
    assert_eq!((large.width, large.height), (4096, 4096));
    assert_eq!(
        CompressedTexture::from_cooked(&large).expect("BC7").format,
        wgpu::TextureFormat::Bc7RgbaUnormSrgb
    );
    let cached = std::fs::read(
        root.join(".regia/cooked/linux")
            .join(regia_asset::cooked_file_name(
                full_manifest
                    .iter()
                    .find(|e| e.path.ends_with("large.png"))
                    .expect("large")
                    .id,
            )),
    )
    .expect("cached chain");

    let result = cook(
        &root,
        &[
            "--texture-tier",
            "LOW=1024",
            "--texture-tier",
            "HIGH=2048",
            "--texture-tier",
            "ULTRA=0",
            "--ship",
        ],
    );
    success(&result); // Fails on the base: unexpected argument '--texture-tier'.
    let shipping: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("cooked_pack.json")).expect("ship report"))
            .expect("report JSON");
    assert_eq!(shipping["errors"], 0);
    assert_eq!(shipping["paks"].as_array().expect("all variant paks").len(), 8);
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("0 cooked"),
        "warm cache must not re-encode"
    );
    assert_eq!(
        std::fs::read(dir.join("textures.fpak")).expect("full unchanged"),
        original
    );
    assert_eq!(
        std::fs::read(dir.join("texture_tiers/ULTRA/textures.fpak")).expect("ultra"),
        original
    );
    let full_chain = CompressedTexture::from_cooked(&large).expect("full runtime chain");
    for (tier, edge, skip) in [("LOW", 1024, 2), ("HIGH", 2048, 1)] {
        let tier_dir = dir.join("texture_tiers").join(tier);
        let reduced = PakReader::open(&tier_dir.join("textures.fpak")).expect("tier pak");
        assert_eq!(reduced.toc.manifest, full_manifest, "same asset IDs and paths");
        let (low_bytes, low) = texture(&reduced, "large.png");
        let registry = regia_asset::registry::AssetRegistry::read_from(
            &regia_asset::registry::pak_registry_path(&tier_dir.join("textures.fpak")),
        )
        .expect("tier registry")
        .expect("registry present");
        assert_eq!(
            registry
                .by_path("assets/large.png")
                .expect("large registry record")
                .size,
            low_bytes.len() as u64,
            "registry records the reduced cooked size"
        );
        assert_eq!(
            registry
                .by_path("assets/small.png")
                .expect("small registry record")
                .size,
            small.len() as u64
        );
        assert_eq!((low.width, low.height), (edge, edge));
        let loaded = CompressedTexture::from_cooked(&low).expect("runtime loads reduced chain");
        assert_eq!(loaded.levels, full_chain.levels[skip..]);
        assert_eq!(texture(&reduced, "small.png").0, small);
        let manifest = regia_asset::content::ContentManifest::read_from_dir(&tier_dir)
            .expect("manifest")
            .expect("present");
        let chunk = manifest.chunk("textures").expect("chunk");
        assert_eq!(
            chunk.hash,
            regia_asset::content::hash_file(&tier_dir.join(&chunk.file)).expect("hash")
        );
        // The small texture is unchanged, so dividing the entire pak by 4^skip would
        // incorrectly shrink its bytes too. Savings track the removed large-texture payload,
        // allowing one alignment block for the following entry's offset.
        let removed_mip_bytes = full_chain.bytes_from(0) - full_chain.bytes_from(skip as u32);
        assert!(original.len() as u64 - chunk.size >= removed_mip_bytes - regia_asset::FPAK_BLOB_ALIGNMENT);
        eprintln!(
            "{tier}: {} pak bytes (full {}), top edge {edge}, removed {removed_mip_bytes} mip bytes",
            chunk.size,
            original.len()
        );
    }
    assert_eq!(
        std::fs::read(
            root.join(".regia/cooked/linux")
                .join(regia_asset::cooked_file_name(
                    full_manifest
                        .iter()
                        .find(|e| e.path.ends_with("large.png"))
                        .expect("large")
                        .id,
                ))
        )
        .expect("cached chain"),
        cached
    );
    success(&cook(&root, &[]));
    assert_eq!(
        std::fs::read(dir.join("textures.fpak")).expect("no flag"),
        original
    );
    let large_id = full_manifest
        .iter()
        .find(|e| e.path.ends_with("large.png"))
        .expect("large id")
        .id;
    inspect_tier_through_agent(&root.join("game"), &dir.join("texture_tiers/LOW"), large_id);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn texture_tier_flag_is_repeatable_and_rejects_invalid_names_and_caps_before_cooking() {
    let scratch = regia_test_utils::TempDir::new("texture-tier-invalid");
    let root = scratch.join("not-created");
    for bad in [
        "../LOW=1024",
        "LOW",
        "LOW=-1",
        "LOW=4294967296",
        "=1024",
        "lo.w=1024",
    ] {
        let output = cook(&root, &["--texture-tier", bad]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("texture_tier_invalid"));
        assert!(!root.exists());
    }
    let duplicate = cook(
        &root,
        &["--texture-tier", "LOW=1024", "--texture-tier", "low=512"],
    );
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("texture_tier_duplicate"));
    assert!(!root.exists());
}

/// Inspect the selected download through the real JSON-RPC agent transport, with no sources or
/// cooked cache in the game's project. The app advances until the bounded client calls finish.
fn inspect_tier_through_agent(game: &Path, content: &Path, texture_id: regia_asset::AssetId) {
    use regia_app::prelude::*;
    use serde_json::json;

    std::fs::create_dir_all(game).expect("game");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_asset::AssetPlugin {
        settings: regia_asset::AssetServerSettings {
            project_root: game.to_path_buf(),
            platform: "linux".into(),
            content_dir: Some(content.to_path_buf()),
        },
    });
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(game.to_path_buf()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    app.finish_plugins();
    app.update();
    let _texture = app
        .world_mut()
        .resource_mut::<regia_asset::AssetServer>()
        .load_texture(texture_id)
        .expect("host loads selected pak texture");
    let project = game.to_path_buf();
    let client = std::thread::spawn(move || {
        let mut client = regia_agent::client::Client::connect_project(&project).expect("agent transport");
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .expect("RPC timeout");
        let status = client
            .call("assets.status", json!({ "asset": "assets/large.png" }))
            .expect("agent status");
        assert_eq!(status["state"], "loaded", "{status}");
        assert_eq!(status["id"], texture_id.to_string());
        let report = client
            .call("assets.cooked_pack", json!({ "inspect": true }))
            .expect("agent inspect");
        assert_eq!(report["errors"], 0, "{report}");
        assert_eq!(report["loads"]["textures"], 1, "{report}");
        assert_eq!(report["loads"]["cache_loads"], 0, "{report}");
        assert_eq!(report["loads"]["pak_loads"], 1, "{report}");
        assert!(
            report["paks"]
                .as_array()
                .expect("paks")
                .iter()
                .any(|p| p["textures"] == 2)
        );
    });
    while !client.is_finished() {
        app.update();
        std::thread::yield_now();
    }
    client.join().expect("agent assertions");
}
