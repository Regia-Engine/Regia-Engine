// SPDX-License-Identifier: MPL-2.0
//! Android texture size report: ASTC (the default cook) against ETC2 / EAC (ADR-1379).
//!
//! `cargo run --release -p regia_asset_pipeline --example etc2_survey -- [--json] [--low] <png>...`
//!
//! Each PNG's usage comes from its file name, as in the cook (`usage_from_file_name`). For each
//! image it prints the bytes of the full mip chain as RGBA8, as the ASTC the Android cook writes
//! (4×4 by default, `--low` for the larger blocks) and as ETC2 / EAC, with the level-0 PSNR of
//! both encodings, then the totals. `--json` prints the rows as JSON instead.

use std::path::Path;

use regia_asset_pipeline::{
    AndroidFormatRow, TextureCookQuality, android_format_report, format_android_report, import_png_rgba8,
    usage_from_file_name,
};

fn main() -> std::process::ExitCode {
    let mut json = false;
    let mut quality = TextureCookQuality::High;
    let mut files = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--json" => json = true,
            "--low" => quality = TextureCookQuality::Low,
            _ => files.push(arg),
        }
    }
    if files.is_empty() {
        eprintln!("usage: etc2_survey [--json] [--low] <png>...");
        return std::process::ExitCode::FAILURE;
    }
    let mut rows: Vec<(String, AndroidFormatRow)> = Vec::new();
    for file in &files {
        let path = Path::new(file);
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skip {file}: {e}");
                continue;
            }
        };
        let image = match import_png_rgba8(&bytes) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("skip {file}: {e}");
                continue;
            }
        };
        let usage = usage_from_file_name(path);
        match android_format_report(image.width, image.height, &image.rgba8, usage, quality) {
            Ok(row) => {
                let name = path
                    .file_name()
                    .map_or_else(|| file.clone(), |n| n.to_string_lossy().into_owned());
                rows.push((name, row));
            }
            Err(e) => eprintln!("skip {file}: {e}"),
        }
    }
    if json {
        let value: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, row)| serde_json::json!({"texture": name, "report": row}))
            .collect();
        match serde_json::to_string_pretty(&value) {
            Ok(text) => println!("{text}"),
            Err(e) => {
                eprintln!("encoding the report: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        print!("{}", format_android_report(&rows));
    }
    std::process::ExitCode::SUCCESS
}
