// SPDX-License-Identifier: MPL-2.0
//! Cook + load Khronos sample models when `REGIA_KHRONOS_ASSETS` points at the xtask cache.

use std::path::{Path, PathBuf};

use regia_asset::{AssetServer, AssetServerSettings};
use regia_asset_pipeline::{CookOptions, cook_project, host_platform};

fn cache_root() -> Option<PathBuf> {
    regia_core::brand::env_var("REGIA_KHRONOS_ASSETS")
        .ok()
        .map(PathBuf::from)
}

fn setup_project(cache: &Path, model_file: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("regia_khronos_{}", uuid::Uuid::new_v4()));
    let assets = root.join("assets").join("khronos");
    std::fs::create_dir_all(&assets).expect("assets");
    copy_model_tree(cache, model_file, &assets);
    root
}

fn copy_model_tree(cache: &Path, model_file: &str, dest_dir: &Path) {
    let src = cache.join(model_file);
    if model_file.ends_with(".gltf") {
        std::fs::copy(&src, dest_dir.join(model_file)).expect("copy gltf");
        let json = std::fs::read_to_string(&src).expect("read gltf");
        for uri in extract_gltf_uris(&json) {
            if uri.starts_with("data:") {
                continue;
            }
            let from = cache.join(&uri);
            let to = dest_dir.join(&uri);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::copy(&from, &to).expect("copy sidecar");
        }
    } else {
        std::fs::copy(&src, dest_dir.join(model_file)).expect("copy model");
    }
}

fn extract_gltf_uris(json: &str) -> Vec<String> {
    let mut uris = Vec::new();
    for line in json.lines() {
        if let Some(rest) = line.split("\"uri\"").nth(1)
            && let Some(quoted) = rest.split('"').nth(1)
        {
            uris.push(quoted.to_string());
        }
    }
    uris
}

const MODELS: &[&str] = &[
    "DamagedHelmet.glb",
    "Sponza.gltf",
    "FlightHelmet.gltf",
    "Fox.glb",
    "CesiumMan.glb",
];

#[test]
fn khronos_samples_cook_and_load() {
    let Some(cache) = cache_root() else {
        assert!(
            !assets_required(),
            "REGIA_KHRONOS_ASSETS is unset and REGIA_REQUIRE_ASSETS=1"
        );
        eprintln!("skip khronos tests: set REGIA_KHRONOS_ASSETS to xtask cache dir");
        return;
    };
    for model in MODELS {
        let root = setup_project(&cache, model);
        let rel = format!("assets/khronos/{model}");
        let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
        assert!(report.errors.is_empty(), "{model}: {:?}", report.errors);
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: host_platform().to_string(),
            content_dir: None,
        })
        .expect("server");
        let h = server.load_mesh_path(&rel).expect("load mesh");
        let mesh = server.meshes().get(&h).expect("mesh");
        assert!(mesh.positions.len() > 3, "{model} has no vertices");
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// `REGIA_REQUIRE_ASSETS=1` (set by a job that must cover the Khronos samples): a missing sample is
/// a failure, not a skip, so lost assets cannot turn these tests vacuous (audit-v035-flaky M1).
fn assets_required() -> bool {
    std::env::var_os("REGIA_REQUIRE_ASSETS").is_some_and(|v| v == "1")
}
