// SPDX-License-Identifier: MIT-0
//! Desktop entry point of `third_person`.
//!
//! Flags for automated runs: `--exit-after-frames N`, `--screenshot PATH` (captured a few
//! frames before exit, written as PNG), `--skip-title` (start in play, without the title screen;
//! screenshot runs use it so the capture shows the level).

use std::process::ExitCode;

use regia::prelude::*;
use third_person::ThirdPersonSample;

fn flag(args: &[String], name: &str) -> Result<Option<u64>, String> {
    match args.iter().position(|a| a == name) {
        None => Ok(None),
        Some(i) => match args.get(i + 1).map(|v| v.parse::<u64>()) {
            Some(Ok(n)) => Ok(Some(n)),
            _ => Err(format!("{name} needs a number")),
        },
    }
}

/// Presence flag (`--split`).
fn flag_present(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| arg == name)
}

/// String-valued flag (`--screenshot PATH`).
fn flag_str(args: &[String], name: &str) -> Result<Option<String>, String> {
    match args.iter().position(|a| a == name) {
        None => Ok(None),
        Some(i) => match args.get(i + 1) {
            Some(value) if !value.starts_with("--") => Ok(Some(value.clone())),
            _ => Err(format!("{name} needs a path")),
        },
    }
}

/// Screenshot/exit plan for automated runs (render_lab's proven pattern).
#[derive(Resource, Clone, Default)]
struct CapturePlan {
    exit_after_frames: u64,
    screenshot: Option<std::path::PathBuf>,
}

/// Set when the screenshot PNG has been written.
#[derive(Resource, Default)]
struct ScreenshotTaken(std::sync::Arc<std::sync::atomic::AtomicBool>);

/// `Last`: frame counting, screenshot capture and exit.
#[tracing::instrument(skip_all, target = "third_person", name = "frame_tick")]
fn frame_tick(
    time: Res<Time>,
    plan: Res<CapturePlan>,
    taken: Res<ScreenshotTaken>,
    mut requests: ResMut<regia_render::ScreenshotRequests>,
    mut exit: MessageWriter<AppExit>,
) {
    let frames = time.frame_count();
    if let Some(path) = &plan.screenshot
        && plan.exit_after_frames > 4
        && frames == plan.exit_after_frames - 4
    {
        let path = path.clone();
        let written = taken.0.clone();
        requests.request(
            None,
            Box::new(move |result| match result {
                Ok(image) => {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match encode_png(&image, &path) {
                        Ok(()) => {
                            tracing::info!(
                                target: "third_person",
                                "screenshot saved to {}",
                                path.display()
                            );
                        }
                        Err(err) => {
                            tracing::error!(target: "third_person", "screenshot encode: {err}")
                        }
                    }
                    written.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Err(err) => tracing::error!(target: "third_person", "screenshot failed: {err}"),
            }),
        );
    }

    if plan.exit_after_frames > 0 && frames + 1 >= plan.exit_after_frames {
        let written = taken.0.load(std::sync::atomic::Ordering::SeqCst);
        if plan.screenshot.as_ref().is_none_or(|_| written) {
            tracing::info!(target: "third_person", "exit after {} frames", plan.exit_after_frames);
            exit.write(AppExit::Success);
        }
    }
}

/// PNG encode without the `image` crate (samples may not depend on it; the agent protocol
/// uses the same `png` crate internally).
fn encode_png(image: &regia_render::CapturedImage, path: &std::path::Path) -> Result<(), String> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(&image.rgba).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, bytes).map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    #[cfg(target_os = "android")]
    {
        // The Fox import runs on the desktop host (see anim_lab for the same split).
        eprintln!("third_person cooks glTF on the desktop host");
        ExitCode::SUCCESS
    }
    #[cfg(not(target_os = "android"))]
    {
        let args: Vec<String> = std::env::args().collect();
        let (exit_after, screenshot) = match (
            flag(&args, "--exit-after-frames"),
            flag_str(&args, "--screenshot"),
        ) {
            (Ok(a), Ok(s)) => (a, s),
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("third_person: {e}");
                return ExitCode::from(2);
            }
        };
        let screenshot_path = screenshot.map(std::path::PathBuf::from);

        // Fetch + cook the Fox before the app starts (network permitting; the sample stays
        // playable capsule-only when this fails).
        if let Err(err) = third_person::assets::ensure_cooked() {
            eprintln!("third_person: asset prepare failed (continuing without the Fox): {err}");
        }

        let exit = regia::run_with(ThirdPersonSample, move |app| {
            app.insert_resource(WindowSettings {
                window: Window {
                    title: "Regia Engine - third_person".into(),
                    ..Default::default()
                },
            });
            app.insert_resource(regia::agent::AgentSettings {
                project_dir: Some(env!("CARGO_MANIFEST_DIR").into()),
            });
            app.insert_resource(CapturePlan {
                exit_after_frames: exit_after.unwrap_or(0),
                screenshot: screenshot_path,
            })
            .insert_resource(third_person::input::SplitOnStart(flag_present(&args, "--split")));
            if flag_present(&args, "--skip-title") {
                // Read by the flow's Boot step at runtime, so inserting it here still applies.
                app.insert_resource(regia_framework::prelude::GameFlowSettings {
                    skip_main_menu: true,
                    ..Default::default()
                });
            }
            app.init_resource::<ScreenshotTaken>()
                .add_systems(Last, frame_tick);
        });
        match exit {
            AppExit::Success => ExitCode::SUCCESS,
            AppExit::Error(code) => ExitCode::from(code.get()),
        }
    }
}
