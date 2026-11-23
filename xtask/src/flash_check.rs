// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask flash-check <dir> --fps N [--json]`: photosensitivity check of a PNG sequence.
//!
//! The counting rules are [`regia_core::flash`] (WCAG 2.2 success criterion 2.3.1, ADR-3804).
//! Exit 0 when the sequence passes, 2 when it fails the thresholds, and 1 on usage or I/O
//! errors. The check is an aid. It is not a certification and it is not a guarantee.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;
use regia_core::flash::{FlashAnalyzer, FlashConfig, FlashError, FlashKind, FlashReport, ViewingGeometry};

/// Most PNG frames one run reads.
const MAX_DIR_FRAMES: usize = 18_000;

/// Arguments of `cargo xtask flash-check`.
#[derive(Parser, Debug)]
pub struct Args {
    /// Directory of PNG frames, read in file-name order. Subfolders are skipped.
    pub dir: PathBuf,
    /// Frames per second the sequence was captured at, from 1 to 480.
    #[arg(long)]
    pub fps: f64,
    /// Write the report as JSON on stdout.
    #[arg(long)]
    pub json: bool,
    /// Square cell edge in pixels. The cell is tested as its mean linear color.
    #[arg(long, default_value_t = 8)]
    pub cell_px: u32,
    /// Reference screen width for the 10 degree field. The default is 1024.
    #[arg(long)]
    pub reference_width: Option<u32>,
    /// Reference screen height for the 10 degree field. The default is 768.
    #[arg(long)]
    pub reference_height: Option<u32>,
    /// Width of a 10 degree field on the reference screen, in pixels. The default is 341.
    #[arg(long)]
    pub ten_degree_width_px: Option<u32>,
    /// Height of a 10 degree field on the reference screen, in pixels. The default is 256.
    #[arg(long)]
    pub ten_degree_height_px: Option<u32>,
}

/// Runs the check. Exit 2 means the sequence fails the flash thresholds.
///
/// # Errors
///
/// A usage or I/O error. The process should exit 1.
pub fn run(args: &Args) -> Result<ExitCode> {
    if !args.fps.is_finite() || !(1.0..=480.0).contains(&args.fps) {
        bail!("fps_out_of_range: fps must be a number from 1 to 480");
    }
    let report = check_dir(&args.dir, args.fps, config_from(args)?)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string(&report).context("encoding the report")?
        );
    } else {
        let kind = match report.kind {
            FlashKind::General => "general",
            FlashKind::Red => "red",
        };
        println!(
            "passed={} max_flashes={} window_start_s={} region={}x{}+{}+{} area_fraction={:.4} kind={} frames={}",
            report.passed,
            report.max_flashes,
            report.window_start_s,
            report.region.width,
            report.region.height,
            report.region.x,
            report.region.y,
            report.area_fraction,
            kind,
            report.frames,
        );
    }
    if report.passed {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(2))
    }
}

/// Analyzes the PNG frames in `dir` at `fps`.
///
/// # Errors
///
/// The directory cannot be read, a frame is not a PNG, or the analyzer refuses the sequence.
pub fn check_dir(dir: &Path, fps: f64, config: FlashConfig) -> Result<FlashReport> {
    if !dir.is_dir() {
        bail!("dir_not_found: {} is not a directory", dir.display());
    }
    let frames = png_frames(dir)?;
    let mut analyzer: Option<FlashAnalyzer> = None;
    let mut dimensions: Option<(u32, u32)> = None;
    for (index, frame) in frames.iter().enumerate() {
        let (width, height, rgba) =
            decode_png(&std::fs::read(frame).with_context(|| frame.display().to_string())?)
                .with_context(|| format!("bad_png: {}", frame.display()))?;
        if let Some(expected) = dimensions {
            if (width, height) != expected {
                bail!(
                    "frame_size_mismatch: frame {index} is {width}x{height}, expected {}x{}",
                    expected.0,
                    expected.1
                );
            }
        } else {
            dimensions = Some((width, height));
            analyzer = Some(FlashAnalyzer::new(width, height, config).map_err(flash_err)?);
        }
        let time_s = f64::from(u32::try_from(index).unwrap_or(u32::MAX)) / fps;
        let Some(current) = analyzer.as_mut() else {
            bail!("no_frames: the analyzer was not created");
        };
        current
            .push_frame(&rgba, time_s)
            .map_err(|error| anyhow::anyhow!("frame {index}: {}", flash_err(error)))?;
    }
    analyzer
        .ok_or_else(|| anyhow::anyhow!("no_frames: the directory has no PNG frames"))?
        .finish()
        .map_err(flash_err)
}

fn config_from(args: &Args) -> Result<FlashConfig> {
    let defaults = ViewingGeometry::default();
    let geometry = ViewingGeometry {
        reference_width: args.reference_width.unwrap_or(defaults.reference_width),
        reference_height: args.reference_height.unwrap_or(defaults.reference_height),
        ten_degree_width_px: args.ten_degree_width_px.unwrap_or(defaults.ten_degree_width_px),
        ten_degree_height_px: args.ten_degree_height_px.unwrap_or(defaults.ten_degree_height_px),
    };
    if args.cell_px == 0
        || geometry.reference_width == 0
        || geometry.reference_height == 0
        || geometry.ten_degree_width_px == 0
        || geometry.ten_degree_height_px == 0
    {
        bail!("geometry_out_of_range: viewing geometry and cell_px must be non-zero");
    }
    Ok(FlashConfig {
        geometry,
        cell_px: args.cell_px,
    })
}

fn flash_err(error: FlashError) -> anyhow::Error {
    anyhow::anyhow!("{}: {error}", error.code())
}

fn png_frames(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("dir_not_found: {}", dir.display()))? {
        let entry = entry.with_context(|| format!("dir_not_found: {}", dir.display()))?;
        let path = entry.path();
        let is_file = entry.metadata().is_ok_and(|meta| meta.is_file());
        let is_png = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("png"));
        if is_file && is_png {
            paths.push(path);
        }
    }
    paths.sort();
    if paths.is_empty() {
        bail!("no_frames: {} has no PNG frames", dir.display());
    }
    if paths.len() > MAX_DIR_FRAMES {
        bail!(
            "too_many_frames: {} PNG frames is above {MAX_DIR_FRAMES}",
            paths.len()
        );
    }
    Ok(paths)
}

/// Decodes a PNG into tightly packed RGBA8. Palettes expand, 16-bit samples drop to 8-bit,
/// and gray and RGB gain an opaque alpha.
fn decode_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("png header")?;
    let (width, height) = (reader.info().width, reader.info().height);
    if width == 0 || height == 0 || width > 8192 || height > 8192 {
        bail!("png is {width}x{height}; each side must be 1..=8192");
    }
    let size = reader.output_buffer_size().context("png has no output buffer")?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).context("png frame")?;
    if info.bit_depth != png::BitDepth::Eight {
        bail!("png bit depth {:?} is not 8", info.bit_depth);
    }
    let samples = info.color_type.samples();
    let pixels = usize::try_from(info.width).unwrap_or(0) * usize::try_from(info.height).unwrap_or(0);
    let src = buf
        .get(..info.buffer_size())
        .filter(|src| src.len() == pixels * samples)
        .context("png decoded to an unexpected size")?;
    let mut rgba = Vec::new();
    match info.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(src),
        png::ColorType::Rgb => {
            for pixel in src.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for &gray in src {
                rgba.extend_from_slice(&[gray, gray, gray, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for pixel in src.as_chunks::<2>().0 {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
        }
        png::ColorType::Indexed => bail!("png palette was not expanded"),
    }
    Ok((info.width, info.height, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_test_utils::{unique_temp_dir, write_atomic};

    fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("png header")?;
        writer.write_image_data(rgba).context("png pixels")?;
        drop(writer);
        Ok(bytes)
    }

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let pixels = usize::try_from(width).expect("width") * usize::try_from(height).expect("height");
        let mut buf = vec![0u8; pixels * 4];
        for chunk in buf.as_chunks_mut::<4>().0 {
            chunk.copy_from_slice(&rgba);
        }
        encode_png(width, height, &buf).expect("png")
    }

    fn is_high(time_s: f64, hz: f64) -> bool {
        ((time_s * hz * 2.0).floor() * 0.5).fract() == 0.0
    }

    fn square_wave_dir(hz: f64) -> PathBuf {
        let dir = unique_temp_dir("xtask-flash");
        for index in 0..60u32 {
            let time_s = f64::from(index) / 30.0;
            let rgba = if is_high(time_s, hz) {
                [255, 255, 255, 255]
            } else {
                [0, 0, 0, 255]
            };
            write_atomic(&dir.join(format!("{index:04}.png")), &solid(16, 12, rgba)).expect("write");
        }
        dir
    }

    #[test]
    fn rejects_changed_dimensions_even_when_pixel_count_matches() {
        let dir = unique_temp_dir("xtask-flash-dimensions");
        write_atomic(&dir.join("0000.png"), &solid(4, 3, [0, 0, 0, 255])).expect("first");
        write_atomic(&dir.join("0001.png"), &solid(3, 4, [255, 255, 255, 255])).expect("second");
        let error = check_dir(&dir, 30.0, FlashConfig::default()).expect_err("size mismatch");
        assert!(error.to_string().contains("frame_size_mismatch"));
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn a_directory_at_2_hz_passes_and_at_4_hz_exits_2() {
        let slow = check_dir(&square_wave_dir(2.0), 30.0, FlashConfig::default()).expect("2 Hz");
        assert!(slow.passed);
        assert!(slow.max_flashes <= 3);

        let fast_dir = square_wave_dir(4.0);
        let fast = check_dir(&fast_dir, 30.0, FlashConfig::default()).expect("4 Hz");
        assert!(!fast.passed);
        let code = run(&Args {
            dir: fast_dir,
            fps: 30.0,
            json: true,
            cell_px: 8,
            reference_width: None,
            reference_height: None,
            ten_degree_width_px: None,
            ten_degree_height_px: None,
        })
        .expect("run");
        assert_eq!(code, ExitCode::from(2));
    }

    /// Steps two showcase effects that flash on purpose and prints the photosensitivity
    /// report. The effects are not modified. Run on lavapipe:
    /// `cargo test -p xtask --lib flash_check::tests::lavapipe_muzzle_flash_and_explosion_reports -- --ignored --exact --nocapture`
    #[test]
    #[ignore = "lavapipe evidence: records flash numbers for muzzle_flash and explosion"]
    fn lavapipe_muzzle_flash_and_explosion_reports() {
        for name in ["muzzle_flash", "explosion"] {
            let report = capture_showcase_effect(name);
            assert_eq!(report.frames, 120, "{name} frames");
            assert_eq!((report.width, report.height), (320, 180), "{name} size");
            let kind = match report.kind {
                FlashKind::General => "general",
                FlashKind::Red => "red",
            };
            println!(
                "{name} passed={} max_flashes={} window_start_s={} region={}x{}+{}+{} area_fraction={:.4} kind={kind} general_flashes={} red_flashes={} general_area_fraction={:.4} red_area_fraction={:.4}",
                report.passed,
                report.max_flashes,
                report.window_start_s,
                report.region.width,
                report.region.height,
                report.region.x,
                report.region.y,
                report.area_fraction,
                report.general_flashes,
                report.red_flashes,
                report.general_area_fraction,
                report.red_area_fraction,
            );
        }
    }

    /// Two seconds at 60 fps, 320×180, cell size 1. Same camera and spawn as the evidence shot.
    fn capture_showcase_effect(name: &str) -> FlashReport {
        let shot = regia_vfx::evidence::effect_shots()
            .into_iter()
            .find(|shot| shot.name == name)
            .expect("showcase shot");
        let effect = shot.effect.expect("named effect");
        let device = regia_render::RenderDevice::headless().expect("headless device");
        let mut harness =
            regia_vfx::offscreen::VfxHarness::new(&device, regia_vfx::VfxProfile::desktop_high())
                .expect("harness");
        let mut library = regia_vfx::scene::VfxLibrary::new();
        for def in regia_vfx::showcase_effects() {
            library.add(def).expect("showcase effect");
        }
        let mut scene = regia_vfx::VfxScene::new(library);
        assert!(
            scene.spawn(effect, shot.origin, shot.forward, 42).is_some(),
            "{name} did not spawn"
        );
        let camera = regia_vfx::VfxCamera {
            eye: shot.eye,
            target: shot.target,
            fov_y: shot.fov_y,
            near: 0.05,
            far: 200.0,
        };
        let config = FlashConfig {
            cell_px: 1,
            ..FlashConfig::default()
        };
        let mut analyzer = FlashAnalyzer::new(320, 180, config).expect("analyzer");
        let dt = 1.0_f32 / 60.0;
        for tick in 0..120u32 {
            scene.update(
                dt,
                &regia_vfx::sim::LodInput {
                    camera_pos: camera.eye,
                },
                100_000,
            );
            let mut lights = regia_vfx::lights::VfxFrameLights::default();
            scene.gather_lights(&mut lights, 4);
            let light = lights
                .lights
                .first()
                .map(|light| (light.position, light.color, light.radius));
            if let Some((position, color, radius)) = light {
                harness.set_point_light(position, color, radius);
            } else {
                harness.set_point_light(regia_core::math::Vec3::new(0.0, 0.0, 0.0), [0.0, 0.0, 0.0], 0.0);
            }
            if tick == 0 && !shot.decals.is_empty() {
                let decals: Vec<regia_vfx::Decal> = shot
                    .decals
                    .iter()
                    .map(|decal| regia_vfx::Decal {
                        position: decal.position,
                        rotation: regia_core::math::Quat::from_rotation_y(decal.yaw_deg.to_radians())
                            * regia_core::math::Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
                        half_size: decal.half,
                        color: decal.color,
                        age: 0.0,
                        lifetime: decal.lifetime,
                    })
                    .collect();
                harness.gpu().write_decals(&decals);
            }
            scene.upload(harness.gpu());
            let frame = harness.render(&device, &camera, 320, 180, dt).expect("frame");
            analyzer
                .push_frame(&frame.pixels, f64::from(tick) / 60.0)
                .expect("push");
        }
        analyzer.finish().expect("finish")
    }
}
