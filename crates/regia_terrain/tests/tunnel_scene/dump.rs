// SPDX-License-Identifier: MPL-2.0
//! Frame dumper: `REGIA_DUMP_DIR` writes the path's frames, a metrics CSV and a contact sheet
//! with the old behaviour on the top row and the tunnel set on the bottom row.

use std::path::{Path, PathBuf};

use crate::scene::{H, W};

/// The dump directory, when `REGIA_DUMP_DIR` is set.
pub fn dump_dir() -> Option<PathBuf> {
    regia_core::brand::env_var_os("REGIA_DUMP_DIR").map(|dir| Path::new(&dir).join("tunnel_scene"))
}

/// Writes an RGBA8 PNG.
pub fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(file) = std::fs::File::create(path) else {
        eprintln!("tunnel_scene: cannot create {}", path.display());
        return;
    };
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(rgba);
    }
}

/// Picks `count` frame indices spread evenly over `len` frames, first and last included.
pub fn sheet_indices(len: usize, count: usize) -> Vec<usize> {
    if len == 0 || count == 0 {
        return Vec::new();
    }
    (0..count).map(|k| k * (len - 1) / (count - 1).max(1)).collect()
}

/// A contact sheet: one row per run (`rows`), one column per index in `columns`.
pub fn contact_sheet(rows: &[&[Vec<u8>]], columns: &[usize]) -> (u32, u32, Vec<u8>) {
    let (cols, rows_n) = (columns.len() as u32, rows.len() as u32);
    let (sw, sh) = (cols * W, rows_n * H);
    let mut sheet = vec![0u8; (sw * sh * 4) as usize];
    for (r, frames) in rows.iter().enumerate() {
        for (c, &index) in columns.iter().enumerate() {
            let Some(frame) = frames.get(index) else { continue };
            for y in 0..H {
                let src = (y * W * 4) as usize;
                let dst = (((r as u32 * H + y) * sw + c as u32 * W) * 4) as usize;
                sheet[dst..dst + (W * 4) as usize].copy_from_slice(&frame[src..src + (W * 4) as usize]);
            }
        }
    }
    (sw, sh, sheet)
}
