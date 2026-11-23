// SPDX-License-Identifier: MIT-0
//! `vfx_lab --evidence <dir>`: renders every evidence shot offscreen and saves
//! one PNG per effect plus a contact sheet and a metrics file (M14 round 2).
//!
//! The same shots and assertions run as GPU tests in `regia_vfx/tests/evidence.rs`;
//! this mode adds the artifacts the lead can look at (`docs/evidence/m14/`).
//!
//! Samples may not use the `image` crate (layer rules — `png` only), so the sheet
//! compositing is plain pixel math: a half-resolution box filter for the tiles and
//! a tiny 5×7 bitmap font for the labels.

use std::path::Path;

use regia_core::math::Vec3;
use regia_render::RenderDevice;
use regia_vfx::VfxCamera;
use regia_vfx::evidence::{assert_shot_plausible, effect_shots, render_shot, shot_line};
use regia_vfx::gpu::VfxError;

/// Evidence frame width (16:9, big enough to read the effect, cheap to render).
pub const W: u32 = 960;
/// Evidence frame height.
pub const H: u32 = 540;

/// A frame buffer: tightly packed RGBA8, `W`×`H`.
struct Frame {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Frame {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; (width * height * 4) as usize],
        }
    }

    fn from_pixels(width: u32, height: u32, pixels: Vec<u8>) -> Self {
        debug_assert_eq!(pixels.len(), (width * height * 4) as usize);
        Self {
            width,
            height,
            pixels,
        }
    }

    /// 2× box-filter downscale (the evidence frames are even-sized).
    fn halve(&self) -> Self {
        let w = self.width / 2;
        let h = self.height / 2;
        let mut out = Self::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let src = |dx: u32, dy: u32| -> [u32; 4] {
                    let i = (((y * 2 + dy) * self.width + x * 2 + dx) * 4) as usize;
                    [
                        u32::from(self.pixels[i]),
                        u32::from(self.pixels[i + 1]),
                        u32::from(self.pixels[i + 2]),
                        u32::from(self.pixels[i + 3]),
                    ]
                };
                let mut sum = [0u32; 4];
                for d in [src(0, 0), src(1, 0), src(0, 1), src(1, 1)] {
                    for (s, v) in sum.iter_mut().zip(d) {
                        *s += v;
                    }
                }
                let i = ((y * w + x) * 4) as usize;
                out.pixels[i..i + 4]
                    .iter_mut()
                    .zip(sum)
                    .for_each(|(p, s)| *p = (s / 4) as u8);
            }
        }
        out
    }

    /// Blits `src` at `(x, y)` (clipped).
    fn blit(&mut self, src: &Frame, x: u32, y: u32) {
        for row in 0..src.height {
            if y + row >= self.height {
                break;
            }
            for col in 0..src.width {
                if x + col >= self.width {
                    break;
                }
                let dst = (((y + row) * self.width + x + col) * 4) as usize;
                let from = ((row * src.width + col) * 4) as usize;
                self.pixels[dst..dst + 4].copy_from_slice(&src.pixels[from..from + 4]);
            }
        }
    }
}

/// Writes a frame as a PNG (sRGB-encoded RGBA8).
fn write_png(frame: &Frame, path: &Path) -> Result<(), VfxError> {
    crate::save_png(path, &frame.pixels, frame.width, frame.height)
        .map_err(|e| VfxError::Shader(format!("{}: {e}", path.display())))
}

/// Renders every shot, writes `<dir>/<name>.png`, `<dir>/contact_sheet.png` and
/// `<dir>/metrics.txt`. Returns the number of failed shots (0 = all plausible).
///
/// # Errors
///
/// Returns [`VfxError`] when the GPU setup or a render fails hard; individual
/// implausible shots are reported (and counted) but do not abort the run.
pub fn write_evidence(device: &RenderDevice, dir: &Path) -> Result<usize, VfxError> {
    std::fs::create_dir_all(dir).map_err(|e| VfxError::Shader(format!("evidence dir: {e}")))?;
    let shots = effect_shots();
    let mut failures = 0usize;
    let mut frames: Vec<(String, Frame)> = Vec::new();
    let mut metrics_lines: Vec<String> = Vec::new();
    let camera_probe = VfxCamera::looking_at(Vec3::new(0.0, 2.0, 7.0), Vec3::new(0.0, 1.0, 0.0));
    let backdrop = backdrop_metrics(device, &camera_probe);
    metrics_lines.push(format!(
        "backdrop (no effect): coverage {:.2}%  luma {:.3}  blown {:.2}%  distinct {}",
        backdrop.coverage * 100.0,
        backdrop.mean_luma,
        backdrop.blown * 100.0,
        backdrop.distinct_colors
    ));
    // The reference scene must itself be healthy, or every diff metric is meaningless
    // (same bands as the GPU test `backdrop_baseline_is_exposed_correctly`).
    if !(backdrop.coverage > 0.90
        && backdrop.mean_luma > 0.10
        && backdrop.mean_luma < 0.85
        && backdrop.blown < 0.05
        && backdrop.distinct_colors >= 16)
    {
        failures += 1;
        metrics_lines.push(format!(
            "FAIL: backdrop is not a lit ground+sky (coverage {:.3}, luma {:.3}, blown {:.3}, distinct {})",
            backdrop.coverage, backdrop.mean_luma, backdrop.blown, backdrop.distinct_colors
        ));
    }

    for shot in &shots {
        let result = render_shot(device, shot, W, H)?;
        let line = shot_line(shot.name, &result);
        println!("{line}");
        metrics_lines.push(line);
        let outcome = std::panic::catch_unwind(|| assert_shot_plausible(shot, &result));
        match outcome {
            Ok(()) => {}
            Err(err) => {
                failures += 1;
                let msg = err
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown assertion failure".into());
                eprintln!("FAIL: {msg}");
                metrics_lines.push(format!("FAIL: {msg}"));
            }
        }
        frames.push((shot.name.to_string(), Frame::from_pixels(W, H, result.with)));
    }

    // One PNG per effect.
    for (name, frame) in &frames {
        write_png(frame, &dir.join(format!("{name}.png")))?;
    }
    write_explosion_strip(device, dir, &shots)?;
    // Contact sheet: 4 columns of half-res tiles with labels.
    let sheet = contact_sheet(&frames);
    write_png(&sheet, &dir.join("contact_sheet.png"))?;
    let metrics_path = dir.join("metrics.txt");
    std::fs::write(&metrics_path, metrics_lines.join("\n") + "\n")
        .map_err(|e| VfxError::Shader(format!("{}: {e}", metrics_path.display())))?;
    println!(
        "wrote {} effect PNGs + contact sheet + metrics to {}",
        frames.len(),
        dir.display()
    );
    Ok(failures)
}

/// Four frames of the explosion: flash, fireball, fire plus the first smoke, then the column.
fn write_explosion_strip(
    device: &RenderDevice,
    dir: &Path,
    shots: &[regia_vfx::evidence::EffectShot],
) -> Result<(), VfxError> {
    let Some(base) = shots.iter().find(|shot| shot.name == "explosion") else {
        return Err(VfxError::Shader("explosion shot missing".into()));
    };
    let times = [0.05_f32, 0.20, 0.60, 1.50];
    let labels = ["0.05", "0.20", "0.60", "1.50"];
    let mut strip = Frame::new(W * 4, H);
    for (index, (time, label)) in times.iter().zip(labels).enumerate() {
        let mut shot = base.clone();
        shot.capture = *time;
        let result = render_shot(device, &shot, W, H)?;
        let frame = Frame::from_pixels(W, H, result.with);
        strip.blit(&frame, index as u32 * W, 0);
        draw_label(&mut strip, label, index as u32 * W + 8, 8, 180);
    }
    write_png(&strip, &dir.join("explosion_strip.png"))
}

/// The backdrop's own metrics (exposure sanity of the reference scene).
fn backdrop_metrics(device: &RenderDevice, camera: &VfxCamera) -> regia_vfx::EffectMetrics {
    let mut harness = regia_vfx::offscreen::VfxHarness::new(device, regia_vfx::VfxProfile::desktop_high())
        .expect("harness");
    let frame = harness
        .render(device, camera, W, H, 1.0 / 60.0)
        .expect("backdrop");
    regia_vfx::measure::measure_frame(&frame.pixels, W)
}

/// Composes the labelled 4-column contact sheet (half-res tiles).
fn contact_sheet(frames: &[(String, Frame)]) -> Frame {
    const COLS: u32 = 4;
    const PAD: u32 = 8;
    const LABEL_H: u32 = 10;
    let rows = frames.len().div_ceil(COLS as usize) as u32;
    let (tw, th) = (W / 2, H / 2);
    let mut sheet = Frame::new(COLS * (tw + PAD) + PAD, rows * (th + LABEL_H + PAD) + PAD);
    // Dark background.
    sheet.pixels.iter_mut().step_by(4).for_each(|p| *p = 24);
    for (index, (name, frame)) in frames.iter().enumerate() {
        let col = index as u32 % COLS;
        let row = index as u32 / COLS;
        let tile = frame.halve();
        let x = PAD + col * (tw + PAD);
        let y = PAD + row * (th + LABEL_H + PAD);
        sheet.blit(&tile, x, y);
        draw_label(&mut sheet, name, x, y + th, tw);
    }
    sheet
}

/// Draws a 5×7 bitmap-font label into the sheet (rows are packed into u8s).
fn draw_label(sheet: &mut Frame, text: &str, x: u32, y: u32, max_w: u32) {
    let font = bitmap_font();
    let mut pen_x = x + 2;
    for ch in text.chars().take(((max_w - 4) / 6) as usize) {
        let glyph = font.get(&ch).unwrap_or(&[0u8; 7]);
        for (gy, row) in glyph.iter().enumerate() {
            for gx in 0..5 {
                if row & (1 << (4 - gx)) != 0 {
                    let px = pen_x + gx;
                    let py = y + 1 + gy as u32;
                    if px < sheet.width && py < sheet.height {
                        let i = ((py * sheet.width + px) * 4) as usize;
                        sheet.pixels[i] = 235;
                        sheet.pixels[i + 1] = 238;
                        sheet.pixels[i + 2] = 242;
                        sheet.pixels[i + 3] = 255;
                    }
                }
            }
        }
        pen_x += 6;
    }
}

/// 5×7 glyphs for the characters effect names use (a-z, 0-9, _).
fn bitmap_font() -> std::collections::BTreeMap<char, [u8; 7]> {
    use std::collections::BTreeMap;
    let mut f: BTreeMap<char, [u8; 7]> = BTreeMap::new();
    let mut add = |c: char, rows: [u8; 7]| {
        f.insert(c, rows);
    };
    add('a', [0b01110, 0b10001, 0b11111, 0b10000, 0b01110, 0, 0]);
    add('b', [0b11111, 0b10001, 0b11111, 0b10001, 0b01110, 0, 0]);
    add('c', [0b01111, 0b10000, 0b10000, 0b10000, 0b01111, 0, 0]);
    add('d', [0b11110, 0b10001, 0b10001, 0b10001, 0b11110, 0, 0]);
    add('e', [0b11111, 0b10000, 0b11110, 0b10000, 0b11111, 0, 0]);
    add('f', [0b11111, 0b10000, 0b11110, 0b10000, 0b10000, 0, 0]);
    add('g', [0b01110, 0b10000, 0b10111, 0b10001, 0b01110, 0, 0]);
    add('h', [0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0, 0]);
    add('i', [0b01110, 0b00100, 0b00100, 0b00100, 0b01110, 0, 0]);
    add('k', [0b10001, 0b10010, 0b11100, 0b10010, 0b10001, 0, 0]);
    add('l', [0b10000, 0b10000, 0b10000, 0b10000, 0b11111, 0, 0]);
    add('m', [0b11111, 0b10101, 0b10101, 0b00000, 0b00000, 0, 0]);
    add('n', [0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0, 0]);
    add('o', [0b01110, 0b10001, 0b10001, 0b10001, 0b01110, 0, 0]);
    add('p', [0b11111, 0b10001, 0b11111, 0b10000, 0b10000, 0, 0]);
    add('q', [0b01110, 0b10001, 0b10001, 0b10010, 0b01101, 0, 0]);
    add('r', [0b11111, 0b10001, 0b11110, 0b10001, 0b10001, 0, 0]);
    add('s', [0b01111, 0b10000, 0b01110, 0b00001, 0b11110, 0, 0]);
    add('t', [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0, 0]);
    add('u', [0b10001, 0b10001, 0b10001, 0b10001, 0b01110, 0, 0]);
    add('v', [0b10001, 0b10001, 0b01010, 0b01010, 0b00100, 0, 0]);
    add('w', [0b10101, 0b10101, 0b10101, 0b10101, 0b01010, 0, 0]);
    add('x', [0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0, 0]);
    add('y', [0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0, 0]);
    add('z', [0b11111, 0b00010, 0b00100, 0b01000, 0b11111, 0, 0]);
    add('_', [0, 0, 0, 0, 0b11111, 0, 0]);
    add('0', [0b01110, 0b10011, 0b10101, 0b11001, 0b01110, 0, 0]);
    add('1', [0b00100, 0b01100, 0b00100, 0b00100, 0b01110, 0, 0]);
    add('2', [0b11110, 0b00001, 0b01110, 0b10000, 0b11111, 0, 0]);
    add('3', [0b11110, 0b00001, 0b01110, 0b00001, 0b11110, 0, 0]);
    add('4', [0b10001, 0b10001, 0b11111, 0b00001, 0b00001, 0, 0]);
    add('5', [0b11111, 0b10000, 0b11110, 0b00001, 0b11110, 0, 0]);
    add('6', [0b01110, 0b10000, 0b11110, 0b10001, 0b01110, 0, 0]);
    add('7', [0b11111, 0b00001, 0b00010, 0b00100, 0b00100, 0, 0]);
    add('8', [0b01110, 0b10001, 0b01110, 0b10001, 0b01110, 0, 0]);
    add('9', [0b01110, 0b10001, 0b01111, 0b00001, 0b01110, 0, 0]);
    add('.', [0, 0, 0, 0, 0, 0b00100, 0]);
    f
}
