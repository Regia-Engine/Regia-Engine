// SPDX-License-Identifier: MIT-0
//! Measures the M20 lavapipe screenshots in `docs/evidence/m20`.
//!
//! A contrast check that ignores which half holds a character does not satisfy
//! the split-screen acceptance. The checks are `docs/evidence/m20/measure_screenshot.py`'s,
//! ported to Rust with the same windows and thresholds, so the crate's tests need no Python
//! (the Windows the test machine has none; ADR-3150). Where `python3` runs, the script is run as well
//! and must agree, so the two cannot drift apart silently.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Width, height and tightly packed RGB bytes (row-major) of an 8-bit RGB or RGBA PNG.
fn decode_rgb(path: &Path) -> (usize, usize, Vec<u8>) {
    let bytes = std::fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("a PNG");
    let mut pixels = vec![0; reader.output_buffer_size().expect("buffer size")];
    let info = reader.next_frame(&mut pixels).expect("a frame");
    assert_eq!(
        info.bit_depth,
        png::BitDepth::Eight,
        "{}: 8-bit PNG",
        path.display()
    );
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        other => panic!("{}: need RGB or RGBA, got {other:?}", path.display()),
    };
    pixels.truncate(info.buffer_size());
    let rgb = pixels
        .chunks_exact(channels)
        .flat_map(|px| [px[0], px[1], px[2]])
        .collect();
    (info.width as usize, info.height as usize, rgb)
}

/// Lit Fox fur (base 0.85, 0.5, 0.2) at the sample's fixed EV 12.7 with flat ambient.
fn orange(r: i32, g: i32, b: i32) -> bool {
    r > 110 && r > b + 25 && g > 30 && b < 160
}

/// Player 1 fur (base 0.8, 0.3, 0.72, magenta): r and b both high, g low.
fn blue(r: i32, g: i32, b: i32) -> bool {
    r > 120 && b > 110 && r > g + 40 && b > g + 30
}

fn luma(r: u8, g: u8, b: u8) -> f64 {
    0.2126 * f64::from(r) + 0.7152 * f64::from(g) + 0.0722 * f64::from(b)
}

struct Shot {
    width: usize,
    height: usize,
    rgb: Vec<u8>,
}

impl Shot {
    fn px(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let i = (y * self.width + x) * 3;
        (self.rgb[i], self.rgb[i + 1], self.rgb[i + 2])
    }

    /// Fraction of every second pixel (both axes) of the window that matches `pred`; the window
    /// edges are fractions of the frame, truncated as the script does.
    fn band(&self, x0: f64, x1: f64, y0: f64, y1: f64, pred: fn(i32, i32, i32) -> bool) -> f64 {
        let (x0, x1) = (
            (self.width as f64 * x0) as usize,
            (self.width as f64 * x1) as usize,
        );
        let (y0, y1) = (
            (self.height as f64 * y0) as usize,
            (self.height as f64 * y1) as usize,
        );
        if x1 <= x0 || y1 <= y0 {
            return 0.0;
        }
        let (mut hit, mut total) = (0u32, 0u32);
        for y in (y0..y1.min(self.height)).step_by(2) {
            for x in (x0..x1.min(self.width)).step_by(2) {
                let (r, g, b) = self.px(x, y);
                total += 1;
                if pred(i32::from(r), i32::from(g), i32::from(b)) {
                    hit += 1;
                }
            }
        }
        if total == 0 {
            0.0
        } else {
            f64::from(hit) / f64::from(total)
        }
    }

    /// Pixels brighter than luma 180 in the window (every pixel).
    fn bright(&self, x1: usize, y1: usize) -> u32 {
        let mut count = 0;
        for y in 0..y1.min(self.height) {
            for x in 0..x1.min(self.width) {
                let (r, g, b) = self.px(x, y);
                if luma(r, g, b) > 180.0 {
                    count += 1;
                }
            }
        }
        count
    }

    /// Mean and population standard deviation of the luma of every fourth pixel (both axes).
    fn mean_std(&self) -> (f64, f64) {
        let mut values = Vec::new();
        for y in (0..self.height).step_by(4) {
            for x in (0..self.width).step_by(4) {
                let (r, g, b) = self.px(x, y);
                values.push(luma(r, g, b));
            }
        }
        let n = values.len().max(1) as f64;
        let mean = values.iter().sum::<f64>() / n;
        let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        (mean, var.sqrt())
    }
}

/// The script's checks for one screenshot: `(check, passed)` pairs plus a detail line.
fn measure(path: &Path) -> (Vec<(&'static str, bool)>, String) {
    let (width, height, rgb) = decode_rgb(path);
    let shot = Shot { width, height, rgb };
    let (mean, std) = shot.mean_std();
    let split = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase()
        .contains("split");
    let mut checks = vec![("not_black", std > 3.0 && mean > 4.0)];
    let detail = if split {
        let left_body = shot.band(0.05, 0.45, 0.45, 0.82, orange);
        let left_sky = shot.band(0.08, 0.42, 0.18, 0.40, orange);
        let right_body = shot.band(0.55, 0.95, 0.45, 0.82, blue);
        let right_sky = shot.band(0.58, 0.92, 0.18, 0.40, blue);
        let right_orange = shot.band(0.55, 0.95, 0.45, 0.82, orange);
        checks.push((
            "left_fox_framed",
            (0.020..=0.22).contains(&left_body) && left_sky < 0.02,
        ));
        checks.push((
            "right_fox_framed",
            (0.017..=0.22).contains(&right_body) && right_sky < 0.02 && right_orange < right_body,
        ));
        format!(
            "left_orange {left_body:.4} left_sky_orange {left_sky:.4} right_blue {right_body:.4} right_sky_blue {right_sky:.4} right_orange {right_orange:.4}"
        )
    } else {
        let body = shot.band(1.0 / 3.0, 2.0 / 3.0, 0.48, 0.84, orange);
        let sky = shot.band(0.30, 0.70, 0.16, 0.40, orange);
        checks.push(("fox_framed", (0.012..=0.35).contains(&body) && sky < 0.015));
        format!("orange {body:.4} sky_orange {sky:.4}")
    };
    let glyphs = shot.bright(
        (width as f64 * 0.45) as usize,
        ((height as f64 * 0.12) as usize).max(4),
    );
    checks.push(("hud_text_present", (40..=8000).contains(&glyphs)));
    (
        checks,
        format!("{width}x{height} mean {mean:.2} std {std:.2} hud {glyphs} {detail}"),
    )
}

#[test]
fn screenshots_frame_the_fox_and_the_hud() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let evidence = manifest.join("../../docs/evidence/m20");
    let images = [evidence.join("gameplay.png"), evidence.join("split_screen.png")];
    for image in &images {
        assert!(image.is_file(), "missing screenshot {}", image.display());
        let (checks, detail) = measure(image);
        let failed: Vec<&str> = checks
            .iter()
            .filter(|(_, ok)| !ok)
            .map(|(name, _)| *name)
            .collect();
        assert!(
            failed.is_empty(),
            "{}: failed {failed:?} ({detail})",
            image.display()
        );
    }
    // The evidence script, where a Python interpreter is installed: it must agree. Windows
    // ships a `python3` app-execution alias that only prints a Store hint and fails, so an
    // interpreter counts as installed only when `python3 --version` succeeds.
    let python = Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !python {
        eprintln!("python3 not installed; the Rust measure above covered both screenshots");
        return;
    }
    let script = evidence.join("measure_screenshot.py");
    let output = Command::new("python3")
        .arg(&script)
        .args(&images)
        .output()
        .expect("python3 runs the screenshot measure");
    assert!(
        output.status.success(),
        "screenshot measure script failed\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
