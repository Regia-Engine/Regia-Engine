// SPDX-License-Identifier: MPL-2.0
//! Bakes the hash of every engine shader source (`src/shader_sources.rs`) into
//! `REGIA_ENGINE_SHADER_SOURCES`, the shader part of the pipeline-cache key (ADR-161 addendum).

#[allow(dead_code)]
#[path = "src/shader_sources.rs"]
mod shader_sources;

#[allow(dead_code)]
#[path = "src/blue_noise/generate.rs"]
mod blue_noise_generate;

use std::path::PathBuf;

/// Generates the spatiotemporal blue-noise table (ADR-2075) once per revision and output directory:
/// this script also reruns on every shader edit, and the table takes seconds in an unoptimized
/// build script. `src/blue_noise.rs` embeds the file and a test compares it with the generator.
fn blue_noise_table() {
    println!("cargo:rerun-if-changed=src/blue_noise/generate.rs");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    let path = out.join(format!("stbn-r{}.bin", blue_noise_generate::REVISION));
    let fresh = std::fs::metadata(&path).is_ok_and(|m| m.len() == blue_noise_generate::BYTES as u64);
    if !fresh {
        let tmp = out.join(format!("stbn-r{}.tmp", blue_noise_generate::REVISION));
        if let Err(err) = std::fs::write(&tmp, blue_noise_generate::generate()) {
            panic!("writing {}: {err}", tmp.display());
        }
        if let Err(err) = std::fs::rename(&tmp, &path) {
            panic!("renaming {}: {err}", tmp.display());
        }
    }
    println!("cargo:rustc-env=REGIA_STBN_TABLE={}", path.display());
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    blue_noise_table();
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let crates_dir = manifest.parent().map(PathBuf::from).unwrap_or_default();
    let sources = shader_sources::collect(&crates_dir);
    for s in &sources {
        if s.rel.ends_with(": missing") {
            println!(
                "cargo:warning=engine shader crate {} not found next to regia_render: the pipeline-cache shader hash cannot cover it",
                s.rel.trim_end_matches(": missing")
            );
        } else {
            println!("cargo:rerun-if-changed={}", s.path.display());
        }
    }
    for dir in shader_sources::watch_dirs(&crates_dir, "regia_render", &sources) {
        println!("cargo:rerun-if-changed={}", dir.display());
    }
    println!(
        "cargo:rustc-env=REGIA_ENGINE_SHADER_SOURCES={}",
        shader_sources::hash(&sources)
    );
}
