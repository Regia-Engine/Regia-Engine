// SPDX-License-Identifier: MPL-2.0
//! What the opt-in `drop_double_sided_on_closed` glTF cook hint would change in a content tree.
//!
//! ```text
//! cargo run --release -p regia_asset_pipeline --example closed_double_sided -- <dir> [--list]
//! ```
//!
//! Walks `<dir>` for `.gltf` / `.glb` files and prints, over all of them: meshes, closed meshes,
//! meshes with a `doubleSided` primitive, meshes the hint would make single-sided, and the
//! `doubleSided` materials it would drop. `--list` adds one line per file the hint changes.

use std::path::PathBuf;
use std::process::ExitCode;

use regia_asset_pipeline::closed_mesh::{ClosedMeshReport, closed_mesh_report_path};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.iter().find(|a| !a.starts_with("--")).map(PathBuf::from) else {
        eprintln!("usage: closed_double_sided <dir> [--list]");
        return ExitCode::from(2);
    };
    let list = args.iter().any(|a| a == "--list");
    let mut total = ClosedMeshReport::default();
    let (mut files, mut failed, mut changed_files) = (0usize, 0usize, 0usize);
    let (mut ds_materials, mut dropped_materials) = (0usize, 0usize);
    for entry in walkdir::WalkDir::new(&dir)
        .sort_by_file_name()
        .into_iter()
        .flatten()
    {
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if !matches!(ext.as_deref(), Some("gltf" | "glb")) {
            continue;
        }
        files += 1;
        let report = match closed_mesh_report_path(path) {
            Ok(report) => report,
            Err(err) => {
                failed += 1;
                eprintln!("{}: {err}", path.display());
                continue;
            }
        };
        total.meshes += report.meshes;
        total.closed_meshes += report.closed_meshes;
        total.closed_see_through += report.closed_see_through;
        total.meshes_double_sided += report.meshes_double_sided;
        total.meshes_dropped += report.meshes_dropped;
        ds_materials += report.double_sided_materials();
        dropped_materials += report.dropped_materials();
        if report.dropped_materials() > 0 {
            changed_files += 1;
            if list {
                println!(
                    "{}: {} of {} doubleSided materials dropped, {} of {} doubleSided meshes",
                    path.strip_prefix(&dir).unwrap_or(path).display(),
                    report.dropped_materials(),
                    report.double_sided_materials(),
                    report.meshes_dropped,
                    report.meshes_double_sided
                );
            }
        }
    }
    println!("files {files} (unreadable {failed}), changed by the hint {changed_files}");
    println!(
        "meshes {}: closed {} (of which see-through {}), with doubleSided {}, made single-sided {}",
        total.meshes,
        total.closed_meshes,
        total.closed_see_through,
        total.meshes_double_sided,
        total.meshes_dropped
    );
    println!("doubleSided materials {ds_materials}: dropped {dropped_materials}");
    ExitCode::SUCCESS
}
