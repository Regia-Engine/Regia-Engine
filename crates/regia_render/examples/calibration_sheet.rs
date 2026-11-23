// SPDX-License-Identifier: MPL-2.0
//! Display-calibration evidence sheet (ADR-092): renders shots as three problem displays show
//! them, at the default calibration and after a player ran the calibration screen (emulated by
//! [`DisplayModel::solve`]), next to the reference monitor.
//!
//! ```text
//! cargo run -p regia_render --example calibration_sheet -- <out_dir> <shot.png>...
//! ```
//!
//! Writes per shot `<stem>_sheet.png` (rows: dim laptop, TV with crushed blacks, phone
//! outdoors; columns: reference monitor | default | calibrated), `logo_test_sheet.png` (the
//! black-level test pattern and a shadow wedge on each display) and `metrics.md` (solved values
//! and shadow detail retained).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use regia_render::calibration::{DisplayCalibration, DisplayModel, LOGO_HIDDEN_SIGNAL, LOGO_VISIBLE_SIGNAL};

type Error = Box<dyn std::error::Error>;

struct Image {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

fn load_png(path: &Path) -> Result<Image, Error> {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("png too large")?];
    let info = reader.next_frame(&mut buf)?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err(format!("{}: only 8-bit PNGs", path.display()).into());
    }
    let (width, height) = (info.width as usize, info.height as usize);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..width * height * 4].to_vec(),
        png::ColorType::Rgb => buf[..width * height * 3]
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        other => return Err(format!("{}: unsupported color type {other:?}", path.display()).into()),
    };
    Ok(Image { width, height, rgba })
}

fn save_png(path: &Path, image: &Image) -> Result<(), Error> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, image.width as u32, image.height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&image.rgba)?;
    Ok(())
}

/// A sheet of equally sized cells on a dark background.
struct Sheet {
    image: Image,
    cell: (usize, usize),
    gap: usize,
}

impl Sheet {
    fn new(cols: usize, rows: usize, cell: (usize, usize)) -> Self {
        let gap = 8;
        let width = cols * cell.0 + (cols + 1) * gap;
        let height = rows * cell.1 + (rows + 1) * gap;
        let rgba = [18u8, 18, 22, 255].repeat(width * height);
        Self {
            image: Image { width, height, rgba },
            cell,
            gap,
        }
    }

    fn put(&mut self, col: usize, row: usize, src: &Image) {
        let x0 = self.gap + col * (self.cell.0 + self.gap);
        let y0 = self.gap + row * (self.cell.1 + self.gap);
        for y in 0..self.cell.1.min(src.height) {
            for x in 0..self.cell.0.min(src.width) {
                let s = (y * src.width + x) * 4;
                let d = ((y0 + y) * self.image.width + x0 + x) * 4;
                self.image.rgba[d..d + 4].copy_from_slice(&src.rgba[s..s + 4]);
            }
        }
    }
}

/// `image` as `display` shows it after `calibration`, re-encoded for a normal monitor.
fn shown(image: &Image, display: &DisplayModel, calibration: &DisplayCalibration) -> Image {
    let mut rgba = image.rgba.clone();
    calibration.apply_sdr_rgba8(&mut rgba);
    display.simulate_rgba8(&mut rgba);
    Image {
        width: image.width,
        height: image.height,
        rgba,
    }
}

/// The black-level test pattern (as on the calibration screen) over a shadow wedge.
fn logo_pattern() -> Image {
    let (width, height) = (640usize, 360usize);
    let mut rgba = [0u8, 0, 0, 255].repeat(width * height);
    let code = |s: f32| (s * 255.0).round() as u8;
    let ring = |cx: f32, cy: f32, x: f32, y: f32| {
        let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
        (d < 75.0 && d > 53.0) || d < 20.0
    };
    for y in 0..height {
        for x in 0..width {
            let (fx, fy) = (x as f32, y as f32);
            let v = if y >= 300 {
                // Wedge: code values 0, 4, 8, … 60 in 16 steps.
                (x * 16 / width * 4) as u8
            } else if ring(190.0, 140.0, fx, fy) {
                code(LOGO_VISIBLE_SIGNAL)
            } else if ring(450.0, 140.0, fx, fy) {
                code(LOGO_HIDDEN_SIGNAL)
            } else {
                0
            };
            rgba[(y * width + x) * 4..(y * width + x) * 4 + 3].copy_from_slice(&[v, v, v]);
        }
    }
    Image { width, height, rgba }
}

fn main() -> Result<(), Error> {
    let mut args = std::env::args().skip(1);
    let out: PathBuf = args
        .next()
        .ok_or("usage: calibration_sheet <out_dir> <shot.png>...")?
        .into();
    let shots: Vec<PathBuf> = args.map(PathBuf::from).collect();
    std::fs::create_dir_all(&out)?;
    let neutral = DisplayCalibration::NEUTRAL;
    let mut md = String::from("| display | solved black level | solved gamma |\n|---|---|---|\n");
    for display in DisplayModel::PROBLEM_DISPLAYS {
        let cal = display.solve(neutral);
        writeln!(
            md,
            "| {} | {:+.2} | {:.2} |",
            display.name, cal.black_level, cal.gamma
        )?;
    }
    md.push_str("\n| shot | display | shadow detail kept, default | calibrated |\n|---|---|---|---|\n");

    for path in &shots {
        let image = load_png(path)?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("shot");
        let mut sheet = Sheet::new(3, 3, (image.width, image.height));
        let reference = shown(&image, &DisplayModel::REFERENCE, &neutral);
        for (row, display) in DisplayModel::PROBLEM_DISPLAYS.iter().enumerate() {
            let cal = display.solve(neutral);
            sheet.put(0, row, &reference);
            sheet.put(1, row, &shown(&image, display, &neutral));
            sheet.put(2, row, &shown(&image, display, &cal));
            let before = display.shadow_detail_retained(&neutral, &image.rgba, image.width, image.height);
            let after = display.shadow_detail_retained(&cal, &image.rgba, image.width, image.height);
            writeln!(
                md,
                "| {stem} | {} | {:.0} % | {:.0} % |",
                display.name,
                before * 100.0,
                after * 100.0
            )?;
        }
        save_png(&out.join(format!("{stem}_sheet.png")), &sheet.image)?;
    }

    let pattern = logo_pattern();
    let mut sheet = Sheet::new(2, 4, (pattern.width, pattern.height));
    sheet.put(0, 0, &shown(&pattern, &DisplayModel::REFERENCE, &neutral));
    for (row, display) in DisplayModel::PROBLEM_DISPLAYS.iter().enumerate() {
        sheet.put(0, row + 1, &shown(&pattern, display, &neutral));
        sheet.put(1, row + 1, &shown(&pattern, display, &display.solve(neutral)));
    }
    save_png(&out.join("logo_test_sheet.png"), &sheet.image)?;
    std::fs::write(out.join("metrics.md"), &md)?;
    print!("{md}");
    Ok(())
}
