// SPDX-License-Identifier: MPL-2.0
//! Transcodes a desktop-cooked terrain pack (DO27's `cooked/terrain.tpak`, `DO27TPK1`: magic,
//! little-endian u32 index length, JSON index, KTX2 blobs in index order) to the Android
//! cook's ASTC with [`regia_asset_pipeline::transcode_bcn_ktx2_to_astc`], keeping the index.
//!
//! `cargo run --release -p regia_asset_pipeline --example tpak_astc -- <in.tpak> <out.tpak> [low]`
//!
//! A measurement tool for running a desktop pack on a phone (lane android-astc); a shell that
//! has the terrain sources should cook them with `cook_terrain_material(.., "android", ..)`.

use std::process::ExitCode;
use std::time::Instant;

use regia_asset_pipeline::astc::AstcContent;
use regia_asset_pipeline::{TextureCookQuality, transcode_bcn_ktx2_to_astc};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(input), Some(output)) = (args.first(), args.get(1)) else {
        eprintln!("usage: tpak_astc <in.tpak> <out.tpak> [low]");
        return ExitCode::FAILURE;
    };
    let quality = if args.get(2).is_some_and(|q| q == "low") {
        TextureCookQuality::Low
    } else {
        TextureCookQuality::High
    };
    match run(input, output, quality) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(input: &str, output: &str, quality: TextureCookQuality) -> Result<(), String> {
    let bytes = std::fs::read(input).map_err(|e| format!("{input}: {e}"))?;
    if bytes.len() < 12 || &bytes[..8] != b"DO27TPK1" {
        return Err("not a DO27TPK1 pack".into());
    }
    let n = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let mut index: serde_json::Value =
        serde_json::from_slice(bytes.get(12..12 + n).ok_or("short index")?).map_err(|e| e.to_string())?;
    let blobs = index["blobs"].as_array_mut().ok_or("no blobs")?;
    let mut at = 12 + n;
    let mut out_blobs = Vec::with_capacity(blobs.len());
    let start = Instant::now();
    for blob in blobs.iter_mut() {
        let len = blob["len"].as_u64().ok_or("blob without len")? as usize;
        let data = bytes.get(at..at + len).ok_or("truncated blob")?;
        at += len;
        let section = blob["section"].as_str().unwrap_or_default().to_string();
        let t = Instant::now();
        // Terrain normal arrays (layer and macro) are read as `.rg` by the terrain shader.
        let astc = transcode_bcn_ktx2_to_astc(data, quality, AstcContent::Rg)
            .map_err(|e| format!("{section}: {e}"))?;
        eprintln!(
            "{section:>20} {:>9} -> {:>9} bytes ({:.1} s)",
            len,
            astc.len(),
            t.elapsed().as_secs_f32()
        );
        blob["len"] = serde_json::Value::from(astc.len() as u64);
        out_blobs.push(astc);
    }
    let index = serde_json::to_vec(&index).map_err(|e| e.to_string())?;
    let mut out = b"DO27TPK1".to_vec();
    out.extend_from_slice(&(index.len() as u32).to_le_bytes());
    out.extend_from_slice(&index);
    for blob in &out_blobs {
        out.extend_from_slice(blob);
    }
    std::fs::write(output, &out).map_err(|e| format!("{output}: {e}"))?;
    eprintln!(
        "{} blobs, {} -> {} bytes in {:.1} s",
        out_blobs.len(),
        bytes.len(),
        out.len(),
        start.elapsed().as_secs_f32()
    );
    Ok(())
}
