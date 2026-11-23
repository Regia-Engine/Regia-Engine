// SPDX-License-Identifier: MPL-2.0
//! `pak_migrate`: rewrites a pak built before the GPU-ready mesh layout (ADR-285) with its meshes
//! in that layout, keeping every other entry, the manifest and the dependencies as they are. No
//! re-cook: the textures are copied byte for byte.
//!
//! ```text
//! pak_migrate <old.fpak> <new.fpak> [--raw]
//! ```
//!
//! `--raw` stores the entries uncompressed (like `regia cook --quality fast`); by default they
//! are lz4-compressed as `regia package` writes them. Desktop paks only: Android paks keep the
//! compact meshes.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let files: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let [src, dst] = files.as_slice() else {
        eprintln!("usage: pak_migrate <old.fpak> <new.fpak> [--raw]");
        return ExitCode::from(2);
    };
    let compress = !args.iter().any(|a| a == "--raw");
    let started = std::time::Instant::now();
    match regia_asset_pipeline::migrate_pak_meshes(&PathBuf::from(src), &PathBuf::from(dst), compress) {
        Ok(meshes) => {
            println!(
                "pak_migrate: {dst}: {meshes} GPU-ready meshes ({:.1} s)",
                started.elapsed().as_secs_f64()
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("pak_migrate: {err}");
            ExitCode::from(1)
        }
    }
}
