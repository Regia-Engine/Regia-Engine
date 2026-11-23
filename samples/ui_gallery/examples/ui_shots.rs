// SPDX-License-Identifier: MIT-0
//! By-eye review captures: renders the widget gallery and the DO27-style HUD at Steam Deck,
//! 1080p, 1440p and 4K into PNGs.
//!
//! ```text
//! cargo run --release -p ui_gallery --example ui_shots -- <out_dir> [backdrop.png]
//! ```
//!
//! With a backdrop (e.g. a tonemapped game frame) the HUD is composited over it the way the
//! overlay pass composites over the 3D frame.

use std::path::PathBuf;
use std::process::ExitCode;

use regia::prelude::*;
use ui_gallery::gallery::UiGallery;
use ui_gallery::hud::HudSample;
use ui_gallery::shots::{REVIEW_SIZES, capture, headless_app, resize};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| "target/ui_shots".into()));
    let backdrop_path = args.next().map(PathBuf::from);
    if let Err(e) = run(&out, backdrop_path.as_deref()) {
        eprintln!("ui_shots: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run(out: &std::path::Path, backdrop_path: Option<&std::path::Path>) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let device = regia_render::RenderDevice::headless().map_err(|e| e.to_string())?;
    let mut gallery = headless_app(UiGallery);
    let mut hud = headless_app(HudSample);
    for (name, w, h) in REVIEW_SIZES {
        let backdrop = match backdrop_path {
            Some(p) => Some(load_backdrop(p, w, h)?),
            None => None,
        };
        resize(
            &mut gallery,
            w,
            h,
            Some(ScaleMode::ScaleWithScreen {
                reference: [1280.0, 720.0],
                match_width: 0.5,
            }),
        );
        let px = capture(&gallery, &device, w, h, None)?;
        save(out, &format!("gallery_{name}.png"), w, h, px)?;
        resize(&mut hud, w, h, None);
        let px = capture(&hud, &device, w, h, backdrop.as_deref())?;
        save(out, &format!("hud_{name}.png"), w, h, px)?;
    }
    Ok(())
}

fn save(out: &std::path::Path, file: &str, w: u32, h: u32, px: Vec<u8>) -> Result<(), String> {
    let img = image::RgbaImage::from_raw(w, h, px).ok_or("pixel buffer size")?;
    let path = out.join(file);
    img.save(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("{}", path.display());
    Ok(())
}

/// Loads an image file as `width`×`height` sRGB RGBA8 (Lanczos resize), the capture backdrop.
fn load_backdrop(path: &std::path::Path, width: u32, height: u32) -> Result<Vec<u8>, String> {
    let img = image::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let img = img.resize_exact(width, height, image::imageops::FilterType::Lanczos3);
    Ok(img.to_rgba8().into_raw())
}
