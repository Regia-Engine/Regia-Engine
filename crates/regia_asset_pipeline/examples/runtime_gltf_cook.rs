// SPDX-License-Identifier: MPL-2.0
//! The smallest host that cooks a glTF project at run time: the size reference of ADR-7972.
//!
//! ```text
//! cargo build --release -p regia_asset_pipeline --example runtime_gltf_cook
//! cargo build --release -p regia_asset_pipeline --example runtime_gltf_cook --no-default-features
//! cargo run --release -p regia_asset_pipeline --example runtime_gltf_cook --no-default-features -- [project dir]
//! ```
//!
//! With a project directory (holding `assets/`) it cooks that project with `cook_project`, the
//! cook's full importer dispatch; without one it writes a one-triangle `assets/triangle.glb` into a
//! fresh temporary project, cooks it the same way and removes it. It prints the cook's counts and
//! the crate's optional features, so the two builds above differ only in what
//! `regia_asset_pipeline` links (a player build writes `default-features = false`).

use std::path::PathBuf;
use std::process::ExitCode;

use regia_asset_pipeline::{
    CookOptions, cook_project, enabled_features, generated_mesh_export::generated_mesh_glb,
};

fn main() -> ExitCode {
    let (root, temporary) = match std::env::args().nth(1) {
        Some(dir) => (PathBuf::from(dir), false),
        None => match triangle_project() {
            Ok(root) => (root, true),
            Err(e) => {
                eprintln!("triangle project: {e}");
                return ExitCode::FAILURE;
            }
        },
    };
    let result = cook_project(CookOptions::for_host(root.clone()));
    if temporary {
        let _ = std::fs::remove_dir_all(&root);
    }
    match result {
        Ok(report) if report.errors.is_empty() => {
            println!(
                "cooked: {} sources ({} skipped); features: {:?}",
                report.cooked,
                report.skipped,
                enabled_features()
            );
            ExitCode::SUCCESS
        }
        Ok(report) => {
            for (path, error) in &report.errors {
                eprintln!("{path}: {error}");
            }
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("cook failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A temporary project whose `assets/` holds one triangle GLB.
fn triangle_project() -> Result<PathBuf, String> {
    let triangle = regia_procgen::GeneratedMesh {
        material: String::new(),
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        normals: vec![[0.0, 0.0, 1.0]; 3],
        uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
        indices: vec![0, 1, 2],
        colors: vec![],
        collision: false,
    };
    let glb = generated_mesh_glb("triangle", &triangle).map_err(|e| e.to_string())?;
    let root = std::env::temp_dir().join(format!("regia-runtime-gltf-cook-{}", std::process::id()));
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).map_err(|e| format!("{}: {e}", assets.display()))?;
    std::fs::write(assets.join("triangle.glb"), glb).map_err(|e| format!("{}: {e}", assets.display()))?;
    Ok(root)
}
