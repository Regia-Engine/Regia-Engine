// SPDX-License-Identifier: MIT-0
//! Screenshots: F12, the panel's button or `demo.screenshot` write a PNG under `<out>/screenshots/`.
//!
//! The windowed demo captures the window's next presented frame as shown (the renderer's
//! `ScreenshotRequests`, the same capture `seeing.screenshot` uses); the file is written when the frame is read
//! back, and [`collect_pending`] records it. A headless app renders the current frame offscreen at its capture
//! size and writes the file at once.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use regia::prelude::*;
use regia_render::runtime::{ExtractedFrame, RuntimeRenderer};
use regia_render::{CapturedImage, RenderDevice, ScreenshotRequests};
use serde_json::{Value, json};

use super::{CaptureMode, DemoError, DemoErrorCode, DemoSettings, DemoState};

/// One finished capture: the file and its size, or why it failed.
pub type ShotOutcome = (PathBuf, Result<(u32, u32), String>);

/// Results of window captures, filled by the renderer's readback callback and drained each frame.
#[derive(Resource, Clone, Default)]
pub struct PendingShots(pub Arc<Mutex<Vec<ShotOutcome>>>);

/// A path inside `base` from a relative `name` given by an agent: no absolute path, no `..`, a `.png` file.
///
/// # Errors
/// [`DemoErrorCode::BadValue`] for anything else.
pub fn contained_path(base: &Path, name: &Path, extension: Option<&str>) -> Result<PathBuf, DemoError> {
    let clean = name.components().all(|c| matches!(c, Component::Normal(_)));
    if name.as_os_str().is_empty() || !clean {
        return Err(DemoError::new(
            DemoErrorCode::BadValue,
            format!(
                "`{}` must be a relative path without `..` (it is written under {})",
                name.display(),
                base.display()
            ),
        ));
    }
    if let Some(ext) = extension
        && name.extension().and_then(|e| e.to_str()) != Some(ext)
    {
        return Err(DemoError::new(
            DemoErrorCode::BadValue,
            format!("`{}` must end in .{ext}", name.display()),
        ));
    }
    Ok(base.join(name))
}

/// The next numbered screenshot file: `<out>/screenshots/showcase-<unix seconds>-<n>.png`.
#[must_use]
pub fn next_path(out_dir: &Path, number: u32) -> PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    out_dir
        .join("screenshots")
        .join(format!("showcase-{secs}-{number:04}.png"))
}

/// Encodes RGBA8 rows as a PNG.
///
/// # Errors
/// The encoder's error as text (a size that does not match the pixels).
pub fn encode_png(image: &CapturedImage) -> Result<Vec<u8>, String> {
    let expected = image.width as usize * image.height as usize * 4;
    if image.rgba.len() != expected || image.width == 0 || image.height == 0 {
        return Err(format!(
            "{}x{} image has {} bytes, expected {expected}",
            image.width,
            image.height,
            image.rgba.len()
        ));
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(&image.rgba).map_err(|e| e.to_string())?;
    }
    Ok(bytes)
}

fn write_png(path: &Path, image: &CapturedImage) -> Result<(u32, u32), String> {
    let bytes = encode_png(image)?;
    crate::write_atomic(path, &bytes)?;
    Ok((image.width, image.height))
}

/// Takes a screenshot to `name` (relative to `<out>/screenshots`) or the next numbered file.
///
/// # Errors
/// [`DemoErrorCode::BadValue`] for a bad name, [`DemoErrorCode::NoRenderer`] without a renderer,
/// [`DemoErrorCode::ScreenshotFailed`] when the offscreen render or the file write fails.
pub fn take(world: &mut World, name: Option<PathBuf>) -> Result<Value, DemoError> {
    let settings = world.resource::<DemoSettings>().0.clone();
    let path = match name {
        Some(name) => contained_path(&settings.out_dir.join("screenshots"), &name, Some("png"))?,
        None => next_path(&settings.out_dir, world.resource::<DemoState>().screenshots + 1),
    };
    match settings.capture {
        CaptureMode::Window => {
            let pending = world.resource::<PendingShots>().clone();
            let Some(mut requests) = world.get_resource_mut::<ScreenshotRequests>() else {
                return Err(DemoError::new(
                    DemoErrorCode::NoRenderer,
                    "no window renderer yet: the window captures its next presented frame",
                ));
            };
            let target = path.clone();
            requests.request(
                None,
                Box::new(move |result| {
                    let outcome = result.and_then(|image| write_png(&target, &image));
                    if let Ok(mut list) = pending.0.lock() {
                        list.push((target, outcome));
                    }
                }),
            );
            Ok(json!({ "path": path.display().to_string(), "written": false, "pending": true }))
        }
        CaptureMode::Offscreen(width, height) => {
            let _span = tracing::info_span!("demo_screenshot_offscreen").entered();
            let image = offscreen(world, width, height)?;
            let outcome = write_png(&path, &image);
            record(world, path.clone(), outcome.clone());
            let (width, height) = outcome.map_err(|e| DemoError::new(DemoErrorCode::ScreenshotFailed, e))?;
            Ok(
                json!({ "path": path.display().to_string(), "written": true, "pending": false, "width": width, "height": height }),
            )
        }
    }
}

/// Renders the current extracted frame offscreen.
fn offscreen(world: &mut World, width: u32, height: u32) -> Result<CapturedImage, DemoError> {
    if !world.contains_resource::<RenderDevice>() || !world.contains_resource::<RuntimeRenderer>() {
        return Err(DemoError::new(
            DemoErrorCode::NoRenderer,
            "no GPU device or renderer for an offscreen capture",
        ));
    }
    world.resource_scope(|world, mut runtime: Mut<RuntimeRenderer>| {
        let device = world.resource::<RenderDevice>();
        let Some(extracted) = world
            .get_resource::<ExtractedFrame>()
            .filter(|e| e.is_renderable())
        else {
            return Err(DemoError::new(
                DemoErrorCode::NoRenderer,
                "nothing to capture yet: the first frame has not been extracted",
            ));
        };
        let view = extracted.window_views().first().map_or(0, |(index, _)| *index);
        let scene = extracted.frame_scene(view, width, height);
        runtime
            .0
            .render_offscreen(device, &scene)
            .map(|frame| CapturedImage {
                width,
                height,
                rgba: frame.pixels,
            })
            .map_err(|e| DemoError::new(DemoErrorCode::ScreenshotFailed, format!("offscreen render: {e}")))
    })
}

fn record(world: &mut World, path: PathBuf, outcome: Result<(u32, u32), String>) {
    let mut state = world.resource_mut::<DemoState>();
    match outcome {
        Ok(_) => {
            state.screenshots += 1;
            state.last_screenshot = Some(path);
            state.screenshot_error = None;
        }
        Err(error) => {
            state.screenshot_error = Some(format!("{}: {error}", path.display()));
        }
    }
}

/// `Update`: records the window captures the renderer finished.
pub fn collect_pending(world: &mut World) {
    let done: Vec<ShotOutcome> = {
        let pending = world.resource::<PendingShots>();
        match pending.0.lock() {
            Ok(mut list) => std::mem::take(&mut *list),
            Err(_) => Vec::new(),
        }
    };
    for (path, outcome) in done {
        record(world, path, outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_paths_stay_inside_the_folder() {
        let base = Path::new("/out/screenshots");
        assert_eq!(
            contained_path(base, Path::new("a/b.png"), Some("png")),
            Ok(base.join("a/b.png"))
        );
        for bad in ["../x.png", "/etc/x.png", "a/../../x.png", "", "x.jpg"] {
            let Err(error) = contained_path(base, Path::new(bad), Some("png")) else {
                panic!("{bad} must be refused");
            };
            assert_eq!(error.code, DemoErrorCode::BadValue);
        }
    }

    #[test]
    fn png_encoding_checks_the_size_and_round_trips() {
        let image = CapturedImage {
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
        };
        let bytes = encode_png(&image).unwrap_or_default();
        let decoded = image::load_from_memory(&bytes).map(|i| i.to_rgba8());
        assert_eq!(decoded.map(|i| i.into_raw()).ok(), Some(image.rgba.clone()));
        let short = CapturedImage {
            rgba: vec![0; 4],
            ..image
        };
        assert!(encode_png(&short).is_err());
    }

    #[test]
    fn numbered_paths_go_under_screenshots() {
        let path = next_path(Path::new("/out"), 7);
        assert!(path.starts_with("/out/screenshots"));
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        assert!(
            name.starts_with("showcase-") && name.ends_with("-0007.png"),
            "{name}"
        );
    }
}
