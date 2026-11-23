// SPDX-License-Identifier: MPL-2.0
//! Renders the look board through the product viewer's own renderer (`regia_viewer`) with the looks
//! hooks and the looks studio: what the web player and the Android viewer draw. Writes a PNG.
//!
//! ```text
//! cargo run -p regia_looks --features viewer --example look_board_viewer -- out.png [width height] [tilt_deg] [viewer-studio]
//! ```
//! `viewer-studio` keeps the viewer's default studio instead of the looks studio (`looks-studio` is
//! the default). Optional `x y fov_deg` after it aim a close-up at board point (x, y) in mm.

use std::error::Error;
use std::path::PathBuf;

use regia_looks::{Studio, viewer};
use regia_viewer::{ViewerConfig, ViewerRenderer, ViewerStyle, capture_still};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = PathBuf::from(args.first().map_or("look-board-viewer.png", String::as_str));
    let width: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1920);
    let height: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1200);
    let tilt: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(22.0);
    let viewer_studio = args.get(4).is_some_and(|s| s == "viewer-studio");
    let num = |i: usize| args.get(i).and_then(|s| s.parse::<f32>().ok());
    let close_up = match (num(5), num(6), num(7)) {
        (Some(x), Some(y), Some(fov)) => Some((x, y, fov)),
        _ => None,
    };
    let board = regia_looks::board::look_board(tilt)?;
    let (_adapter, device, queue) = regia_viewer::gpu::headless_device()?;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut r = if viewer_studio {
        let mut config = ViewerConfig::new(format);
        config.hooks = viewer::hooks();
        let mut r = ViewerRenderer::new(device, queue, config)?;
        r.set_style(&ViewerStyle {
            clear_color: viewer::BOARD_CLEAR,
            ..ViewerStyle::default()
        });
        r
    } else {
        viewer::looks_renderer(device, queue, format, 4, &Studio::default())?
    };
    viewer::draw_scene(&mut r, &board, close_up)?;
    let still = capture_still(&mut r, width, height)?;
    let png = still.encode_png()?;
    let tmp = out.with_extension("png.tmp");
    std::fs::write(&tmp, png)?;
    std::fs::rename(&tmp, &out)?;
    let stats = r.stats();
    eprintln!(
        "wrote {} ({width}x{height}, {} pipelines, {} draw calls, hash {:016x})",
        out.display(),
        stats.pipelines_created,
        stats.draw_calls,
        still.hash()
    );
    Ok(())
}
