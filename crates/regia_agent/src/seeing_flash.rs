// SPDX-License-Identifier: MPL-2.0
//! `seeing.flash_check`: photosensitivity check over captured frames or a folder of PNGs.
//!
//! The counting rules live in [`regia_core::flash`] (ADR-3804). This module parses the call,
//! streams frames into that analyzer and answers with its report. A capture does not keep the
//! pixels.

use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;
use regia_app::prelude::*;
use regia_core::flash::{FlashAnalyzer, FlashConfig, FlashError, ViewingGeometry};
use regia_render::CapturedImage;
use serde_json::Value;

use crate::protocol::{self, Response, RpcError};
use crate::seeing::Outcome;

/// Most PNG frames a directory analysis reads.
const MAX_DIR_FRAMES: usize = 18_000;

/// Capture length `seeing.flash_check` accepts when it shoots the window itself.
const CAPTURE_FRAMES: std::ops::RangeInclusive<u64> = 2..=600;

/// Frames per second the analysis accepts. Time of frame `i` is `i / fps`.
const FPS: std::ops::RangeInclusive<f64> = 1.0..=480.0;

/// Cell edge, in pixels, the call accepts.
const CELL_PX: std::ops::RangeInclusive<u64> = 1..=4096;

/// Largest frame side the flash analyzer accepts. The area cap is 4096×4096, so a
/// frame may be wider than [`crate::image::MAX_INPUT_SIDE`] when the other side is shorter.
const MAX_FRAME_SIDE: u32 = 8192;

/// A flash analysis fed one captured frame at a time. Pixels are not stored.
pub(crate) struct FlashJob {
    fps: f64,
    config: FlashConfig,
    analyzer: Option<FlashAnalyzer>,
    width: u32,
    height: u32,
    index: u32,
}

impl FlashJob {
    /// An analysis that will time frame `i` at `i / fps`.
    pub(crate) fn new(fps: f64, config: FlashConfig) -> Self {
        Self {
            fps,
            config,
            analyzer: None,
            width: 0,
            height: 0,
            index: 0,
        }
    }

    /// Fold one captured frame into the analysis.
    ///
    /// # Errors
    ///
    /// A core [`FlashError`], mapped to its stable code.
    pub(crate) fn push(&mut self, image: &CapturedImage) -> Result<(), RpcError> {
        if self.analyzer.is_none() {
            let created = FlashAnalyzer::new(image.width, image.height, self.config).map_err(map_flash)?;
            self.width = image.width;
            self.height = image.height;
            self.analyzer = Some(created);
        } else if image.width != self.width || image.height != self.height {
            return Err(map_flash(FlashError::FrameSizeMismatch {
                expected_bytes: expected_len(self.width, self.height),
                actual_bytes: image.rgba.len(),
            }));
        }
        let time_s = f64::from(self.index) / self.fps;
        let Some(analyzer) = self.analyzer.as_mut() else {
            return Err(map_flash(FlashError::NoFrames));
        };
        analyzer.push_frame(&image.rgba, time_s).map_err(map_flash)?;
        self.index = self.index.saturating_add(1);
        Ok(())
    }

    /// Finish the analysis and encode the report.
    ///
    /// # Errors
    ///
    /// [`FlashError::NoFrames`] when nothing was pushed, any other core error, or `F0015` when
    /// the report cannot be encoded.
    pub(crate) fn finish(self) -> Result<Value, RpcError> {
        let Some(analyzer) = self.analyzer else {
            return Err(map_flash(FlashError::NoFrames));
        };
        let report = analyzer.finish().map_err(map_flash)?;
        serde_json::to_value(&report).map_err(|error| {
            RpcError::new(
                protocol::UNSUPPORTED,
                "F0015",
                format!("cannot encode the flash report: {error}"),
                "check logs.read",
            )
        })
    }
}

/// `seeing.flash_check`: analyze a folder of PNGs, or capture `frames` at `fps` and analyze those.
pub(crate) fn flash_check(world: &mut World, params: &Value, id: Value, reply: Sender<Response>) -> Outcome {
    let parsed = match parse(params) {
        Ok(parsed) => parsed,
        Err(error) => return Outcome::Done(Err(error)),
    };
    let Some(parsed) = parsed else {
        return Outcome::Done(Err(domain(
            "no_frames",
            "pass dir (a folder of PNG frames) or frames (capture the window)",
            "one of the two, plus fps",
        )));
    };
    match parsed.source {
        Source::Dir(dir) => Outcome::Done(analyze_dir(world, &dir, parsed.fps, parsed.config)),
        Source::Capture { frames } => crate::seeing::start_flash_capture(
            world,
            id,
            reply,
            frames,
            parsed.every_n,
            FlashJob::new(parsed.fps, parsed.config),
        ),
    }
}

struct Parsed {
    source: Source,
    fps: f64,
    every_n: u64,
    config: FlashConfig,
}

enum Source {
    Dir(String),
    Capture { frames: usize },
}

fn parse(params: &Value) -> Result<Option<Parsed>, RpcError> {
    let dir = crate::params::opt_str(params, "dir")?.filter(|text| !text.is_empty());
    let frames = crate::params::opt_u64(params, "frames")?;
    if dir.is_some() && frames.is_some() {
        return Err(RpcError::invalid_params("pass dir or frames, not both"));
    }
    let config = parse_config(params)?;
    let every_n = flash_every_n(params)?;
    if dir.is_none() && frames.is_none() {
        return Ok(None);
    }
    let fps = match crate::params::opt_f64(params, "fps")? {
        Some(fps) if fps.is_finite() && FPS.contains(&fps) => fps,
        _ => {
            return Err(domain(
                "fps_out_of_range",
                "fps must be a number from 1 to 480",
                "pass the rate the frames were captured at",
            ));
        }
    };
    let source = if let Some(dir) = dir {
        Source::Dir(dir.to_owned())
    } else {
        let frames = frames.unwrap_or(0);
        if !CAPTURE_FRAMES.contains(&frames) {
            return Err(domain(
                "frames_out_of_range",
                "frames must be an integer from 2 to 600",
                "capture a shorter sequence, or pass dir for a longer one",
            ));
        }
        Source::Capture {
            frames: usize::try_from(frames).unwrap_or(usize::MAX),
        }
    };
    Ok(Some(Parsed {
        source,
        fps,
        every_n,
        config,
    }))
}

fn parse_config(params: &Value) -> Result<FlashConfig, RpcError> {
    let cell_px = match crate::params::opt_u64(params, "cell_px")? {
        None => FlashConfig::default().cell_px,
        Some(cell) if CELL_PX.contains(&cell) => u32::try_from(cell).unwrap_or(4096),
        Some(_) => {
            return Err(domain(
                "geometry_out_of_range",
                "cell_px must be an integer from 1 to 4096",
                "1 tests every pixel; 8 is the default",
            ));
        }
    };
    let defaults = ViewingGeometry::default();
    Ok(FlashConfig {
        cell_px,
        geometry: ViewingGeometry {
            reference_width: geom_u32(params, "reference_width", defaults.reference_width)?,
            reference_height: geom_u32(params, "reference_height", defaults.reference_height)?,
            ten_degree_width_px: geom_u32(params, "ten_degree_width_px", defaults.ten_degree_width_px)?,
            ten_degree_height_px: geom_u32(params, "ten_degree_height_px", defaults.ten_degree_height_px)?,
        },
    })
}

fn geom_u32(params: &Value, key: &str, default: u32) -> Result<u32, RpcError> {
    match crate::params::opt_u64(params, key)? {
        None => Ok(default),
        Some(value) => u32::try_from(value)
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                domain(
                    "geometry_out_of_range",
                    format!("{key} must be a non-zero integer that fits in 32 bits"),
                    "omit it to use the W3C 1024x768, 341x256 field",
                )
            }),
    }
}

fn flash_every_n(params: &Value) -> Result<u64, RpcError> {
    match crate::params::opt_u64(params, "every_n")? {
        None => Ok(1),
        Some(n) if (1..=3600).contains(&n) => Ok(n),
        Some(_) => Err(RpcError::invalid_params(
            "`every_n` must be an integer in 1..=3600",
        )),
    }
}

fn analyze_dir(world: &World, dir: &str, fps: f64, config: FlashConfig) -> Result<Value, RpcError> {
    let path = crate::req_path::allowed_path(world, "dir", dir)?;
    if !path.is_dir() {
        return Err(domain(
            "dir_not_found",
            format!("{dir} is not a directory"),
            "pass a folder of PNG frames",
        ));
    }
    let frames = png_frames(&path)?;
    let _span =
        tracing::debug_span!(target: "regia_agent", "flash_check_dir", frames = frames.len()).entered();
    let mut job = FlashJob::new(fps, config);
    for (index, frame) in frames.iter().enumerate() {
        let bytes =
            crate::req_path::read_file_capped(frame, crate::req_path::MAX_READ_BYTES).map_err(|error| {
                domain(
                    "bad_png",
                    format!("{}: {error}", frame.display()),
                    "each frame must be a PNG of at most 256 MiB",
                )
            })?;
        let image = crate::image::decode_png_rgba_within(&bytes, MAX_FRAME_SIDE).map_err(|error| {
            domain(
                "bad_png",
                format!("{}: {error}", frame.display()),
                "each frame must be an RGBA, RGB or gray PNG",
            )
        })?;
        job.push(&image).map_err(|error| annotate_index(error, index))?;
    }
    job.finish()
}

fn annotate_index(error: RpcError, index: usize) -> RpcError {
    let code = error
        .data
        .as_ref()
        .and_then(|data| data["code"].as_str())
        .unwrap_or("bad_png")
        .to_owned();
    if code == "F0015" {
        return error;
    }
    let hint = error
        .data
        .as_ref()
        .and_then(|data| data["hint"].as_str())
        .unwrap_or("check the frame sequence")
        .to_owned();
    let message = format!("frame {index}: {}", error.message);
    RpcError::new(error.code, &code, message, hint)
}

fn png_frames(dir: &Path) -> Result<Vec<PathBuf>, RpcError> {
    let entries = std::fs::read_dir(dir).map_err(|error| {
        domain(
            "dir_not_found",
            format!("{} cannot be read: {error}", dir.display()),
            "pass a folder of PNG frames",
        )
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            domain(
                "dir_not_found",
                format!("cannot read a directory entry: {error}"),
                "pass a folder of PNG frames",
            )
        })?;
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
        return Err(domain(
            "no_frames",
            "the directory has no PNG frames",
            "put the frames in one folder; subfolders are not read",
        ));
    }
    if paths.len() > MAX_DIR_FRAMES {
        return Err(domain(
            "too_many_frames",
            format!("{} PNG frames is above {MAX_DIR_FRAMES}", paths.len()),
            "analyze a shorter sequence",
        ));
    }
    Ok(paths)
}

fn expected_len(width: u32, height: u32) -> usize {
    usize::try_from(u64::from(width) * u64::from(height) * 4).unwrap_or(usize::MAX)
}

fn domain(code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(protocol::INVALID_PARAMS, code, message, hint)
}

fn map_flash(error: FlashError) -> RpcError {
    let hint = match error {
        FlashError::NoFrames => "pass at least one frame",
        FlashError::EmptyFrame => "frame width and height must be non-zero",
        FlashError::FrameSizeMismatch { .. } => "every frame must be the same size",
        FlashError::TimeNotMonotonic { .. } => "frame times must increase",
        FlashError::Geometry => "viewing geometry and cell_px must be non-zero",
        FlashError::FrameTooLarge { .. } => {
            "use a larger cell_px or a smaller frame (side 8192, area 4096x4096, 2500000 cells)"
        }
    };
    domain(error.code(), error.to_string(), hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_test_utils::{unique_temp_dir, write_atomic};
    use serde_json::json;

    fn code(error: &RpcError) -> Option<&str> {
        error.data.as_ref().and_then(|data| data["code"].as_str())
    }

    fn call(params: Value) -> Result<Value, RpcError> {
        let mut world = World::new();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let outcome = flash_check(&mut world, &params, json!(1), reply_tx);
        drop(reply_rx);
        match outcome {
            Outcome::Done(result) => result,
            Outcome::Deferred => panic!("a directory call must answer now"),
        }
    }

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let pixels = usize::try_from(width).expect("width") * usize::try_from(height).expect("height");
        let mut buf = vec![0u8; pixels * 4];
        for chunk in buf.as_chunks_mut::<4>().0 {
            chunk.copy_from_slice(&rgba);
        }
        crate::image::png_bytes(&CapturedImage {
            width,
            height,
            rgba: buf,
        })
        .expect("png")
    }

    fn is_high(time_s: f64, hz: f64) -> bool {
        ((time_s * hz * 2.0).floor() * 0.5).fract() == 0.0
    }

    fn square_wave_dir(hz: f64) -> PathBuf {
        let dir = unique_temp_dir("flash-check");
        let fps = 30.0_f64;
        let frames = 60u32;
        for index in 0..frames {
            let time_s = f64::from(index) / fps;
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
    fn a_directory_of_frames_at_2_hz_passes_and_at_4_hz_fails() {
        let slow = square_wave_dir(2.0);
        let passed = call(json!({ "dir": slow, "fps": 30 })).expect("2 Hz report");
        assert_eq!(passed["passed"], json!(true));
        assert!(passed["max_flashes"].as_u64().expect("count") <= 3);

        let fast = square_wave_dir(4.0);
        let failed = call(json!({ "dir": fast, "fps": 30 })).expect("4 Hz report");
        assert_eq!(failed["passed"], json!(false));
        assert!(failed["max_flashes"].as_u64().expect("count") > 3);
        assert_eq!(failed["kind"], json!("general"));
    }

    #[test]
    fn flash_check_refuses_missing_input_bad_fps_and_both_modes() {
        let missing = call(json!({})).expect_err("no frames");
        assert_eq!(code(&missing), Some("no_frames"));

        let both = call(json!({ "dir": "frames", "frames": 4, "fps": 30 })).expect_err("both");
        assert_eq!(code(&both), Some("F0002"));

        let fps = call(json!({ "dir": "frames", "fps": 0 })).expect_err("fps");
        assert_eq!(code(&fps), Some("fps_out_of_range"));

        let short = call(json!({ "frames": 1, "fps": 30 })).expect_err("frames");
        assert_eq!(code(&short), Some("frames_out_of_range"));

        let cell = call(json!({ "dir": "frames", "fps": 30, "cell_px": 0 })).expect_err("cell");
        assert_eq!(code(&cell), Some("geometry_out_of_range"));

        let empty = unique_temp_dir("flash-empty");
        let none = call(json!({ "dir": empty, "fps": 30 })).expect_err("empty dir");
        assert_eq!(code(&none), Some("no_frames"));

        let missing_dir = empty.join("missing");
        let absent = call(json!({ "dir": missing_dir, "fps": 30 })).expect_err("missing dir");
        assert_eq!(code(&absent), Some("dir_not_found"));

        let bad = unique_temp_dir("flash-bad");
        write_atomic(&bad.join("0000.png"), b"not a png").expect("write");
        let png = call(json!({ "dir": bad, "fps": 30 })).expect_err("bad png");
        assert_eq!(code(&png), Some("bad_png"));

        let mismatch = unique_temp_dir("flash-size");
        write_atomic(&mismatch.join("0000.png"), &solid(2, 2, [0, 0, 0, 255])).expect("a");
        write_atomic(&mismatch.join("0001.png"), &solid(3, 3, [0, 0, 0, 255])).expect("b");
        let size = call(json!({ "dir": mismatch, "fps": 30 })).expect_err("size");
        assert_eq!(code(&size), Some("frame_size_mismatch"));
    }

    #[test]
    fn a_wide_frame_within_the_area_cap_is_analyzed() {
        let dir = unique_temp_dir("flash-wide");
        // Side 8192, area 8192: inside the analyzer cap (side 8192, area 4096×4096)
        // and outside the generic image cap of 4096.
        write_atomic(&dir.join("0000.png"), &solid(8192, 1, [0, 0, 0, 255])).expect("black");
        write_atomic(&dir.join("0001.png"), &solid(8192, 1, [255, 255, 255, 255])).expect("white");
        let report = call(json!({ "dir": dir, "fps": 30 })).expect("wide frames");
        assert!(report.get("passed").is_some(), "{report}");
    }

    #[test]
    fn capturing_without_a_renderer_is_unsupported() {
        let mut world = World::new();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let outcome = flash_check(&mut world, &json!({ "frames": 4, "fps": 30 }), json!(1), reply_tx);
        drop(reply_rx);
        match outcome {
            Outcome::Done(Err(error)) => assert_eq!(code(&error), Some("F0011")),
            Outcome::Done(Ok(_)) => panic!("expected F0011"),
            Outcome::Deferred => panic!("expected F0011 before a capture starts"),
        }
    }

    #[test]
    fn capturing_without_a_window_is_not_found() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.init_resource::<regia_render::ScreenshotRequests>();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let outcome = flash_check(
            app.world_mut(),
            &json!({ "frames": 4, "fps": 30 }),
            json!(1),
            reply_tx,
        );
        drop(reply_rx);
        match outcome {
            Outcome::Done(Err(error)) => assert_eq!(code(&error), Some("F0003")),
            Outcome::Done(Ok(value)) => panic!("expected F0003, got {value}"),
            Outcome::Deferred => panic!("expected F0003 before a capture starts"),
        }
    }
}
