// SPDX-License-Identifier: MIT-0
//! Compile-time probe for in-flight photoreal lanes.
//!
//! The harness renders with the public API that exists on main. When a lane lands and
//! adds one of the type names below, the matching `lookdev_detect_*` cfg flips and
//! `shots.json` records it, without this sample taking a dependency on an API that is
//! not in the tree yet (so the sample keeps building across merges).

use std::path::Path;

const PROBES: &[(&str, &[&str], &[&str])] = &[
    // M15b's public names. Any one match flips the flag. These are presence
    // probes only: TerrainGpu still draws a single sun cascade, and the harness
    // does not claim the forward SSR / GI passes ran.
    (
        "lookdev_detect_ssr",
        &["regia_render"],
        &["fn ssr_roughness_weight", "struct ReflectionProbe"],
    ),
    (
        "lookdev_detect_gi",
        &["regia_render"],
        &["struct GiField", "struct GpuGiGrid"],
    ),
    (
        "lookdev_detect_atmosphere",
        &["regia_render", "regia_vfx"],
        &["struct AtmosphereModel", "fn render_atmosphere"],
    ),
    (
        "lookdev_detect_clouds",
        &["regia_render", "regia_vfx"],
        &["struct VolumetricClouds", "fn render_clouds"],
    ),
    (
        "lookdev_detect_terrain_stream",
        &["regia_terrain"],
        &["struct TerrainStreamer", "fn stream_heightfield"],
    ),
    (
        "lookdev_detect_macro",
        &["regia_terrain"],
        &["struct MacroVariation", "height_blend_map"],
    ),
];

fn main() {
    let manifest = Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join("../..");
    let root = manifest.canonicalize().unwrap_or(manifest);
    for (cfg_name, crates, needles) in PROBES {
        println!("cargo:rustc-check-cfg=cfg({cfg_name})");
        let mut found = false;
        for crate_name in *crates {
            let src = root.join("crates").join(crate_name).join("src");
            println!("cargo:rerun-if-changed={}", src.display());
            if tree_contains(&src, needles) {
                found = true;
            }
        }
        if found {
            println!("cargo:rustc-cfg={cfg_name}");
        }
    }
}

fn tree_contains(dir: &Path, needles: &[&str]) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if tree_contains(&path, needles) {
                return true;
            }
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && let Ok(text) = std::fs::read_to_string(&path)
            && needles.iter().any(|needle| text.contains(needle))
        {
            return true;
        }
    }
    false
}
