// SPDX-License-Identifier: MIT-0
//! `mobile_forward_bench`: per-pass GPU times of a default-look shot at a chosen preset and lite
//! forward tier, headless, so a phone can run it from `/data/local/tmp` over adb (ADR-1874).
//!
//! ```text
//! mobile_forward_bench --out <dir> [--preset MobileLow] [--lite auto|off|low|high]
//!     [--width 1280] [--height 720] [--frames 6] [--shot props_on_snow] [--fixed-ev <ev100>]
//!     [--stage-dir <dir>] [--lights <n>] [--light-shadows <0|1>]
//! ```
//!
//! `--stage-dir` stages every `<engine file>.wgsl` in the directory as a shader edit, so a shader
//! change reaches the phone without a rebuild. `--lights` adds that many point lights in a ring
//! ahead of the camera, all reaching the view's centre; `--light-shadows 1` lets them cast. Prints
//! one `bench:` line per shot.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lookdev::LookdevError;
use lookdev::default_look::{LookExposure, LookOptions, LookShot, render_shot_with, write_png};
use regia_render::forward_lite::ForwardLite;
use regia_render::{
    FrameRenderer, LabLocalLight, LabPointLight, LabScene, RenderDevice, RenderProfilePreset, RenderSettings,
    shader_reload,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("mobile_forward_bench: {err}");
            ExitCode::from(1)
        }
    }
}

fn value<'a>(args: &'a [String], i: usize, flag: &str) -> Result<&'a str, LookdevError> {
    args.get(i + 1)
        .map(String::as_str)
        .ok_or_else(|| LookdevError::Config(format!("{flag} needs a value")))
}

fn number<T: std::str::FromStr>(args: &[String], i: usize, flag: &str) -> Result<T, LookdevError> {
    value(args, i, flag)?
        .parse()
        .map_err(|_| LookdevError::Config(format!("{flag}: not a number")))
}

fn io_err(path: &Path, source: std::io::Error) -> LookdevError {
    LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn stage_shaders(dir: &Path) -> Result<(), LookdevError> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| io_err(dir, e))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "wgsl"))
        .collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let text = std::fs::read_to_string(&path).map_err(|e| io_err(&path, e))?;
        shader_reload::stage_shader_edit(&name, &text)
            .map_err(|e| LookdevError::Config(format!("staging {name}: {e}")))?;
        println!("bench: staged {name}");
    }
    Ok(())
}

/// `count` point lights in a ring around the camera's target.
fn add_lights(scene: &mut LabScene, count: u32, cast_shadows: bool) {
    let center = scene.target;
    for i in 0..count {
        let angle = i as f32 / count.max(1) as f32 * std::f32::consts::TAU;
        scene.local_lights.push(LabLocalLight::Point(LabPointLight {
            position: [
                center[0] + 6.0 * angle.cos(),
                center[1] + 3.0,
                center[2] + 6.0 * angle.sin(),
            ],
            color: [1.0, 0.85, 0.7],
            intensity: 600_000.0,
            radius: 30.0,
            cast_shadows,
            source_radius: None,
        }));
    }
}

fn run(args: &[String]) -> Result<(), LookdevError> {
    let mut options = LookOptions::evidence();
    options.frames = 6;
    let mut out: Option<PathBuf> = None;
    let mut only: Option<String> = None;
    let mut preset = RenderProfilePreset::MobileLow;
    let mut lite: Option<ForwardLite> = None;
    let mut stage: Option<PathBuf> = None;
    let mut lights = 0u32;
    let mut light_shadows = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => out = Some(PathBuf::from(value(args, i, "--out")?)),
            "--width" => options.width = number(args, i, "--width")?,
            "--height" => options.height = number(args, i, "--height")?,
            "--frames" => options.frames = number(args, i, "--frames")?,
            "--fixed-ev" => options.exposure = LookExposure::Fixed(number(args, i, "--fixed-ev")?),
            "--shot" => only = Some(value(args, i, "--shot")?.to_string()),
            "--stage-dir" => stage = Some(PathBuf::from(value(args, i, "--stage-dir")?)),
            "--lights" => lights = number(args, i, "--lights")?,
            "--light-shadows" => light_shadows = number::<u32>(args, i, "--light-shadows")? != 0,
            "--preset" => {
                let name = value(args, i, "--preset")?;
                preset = RenderProfilePreset::from_name(name)
                    .ok_or_else(|| LookdevError::Config(format!("unknown preset {name}")))?;
            }
            "--lite" => {
                let name = value(args, i, "--lite")?;
                lite = match name {
                    "auto" => None,
                    _ => Some(
                        ForwardLite::from_name(name)
                            .ok_or_else(|| LookdevError::Config(format!("unknown lite tier {name}")))?,
                    ),
                };
            }
            other => return Err(LookdevError::Config(format!("unknown flag {other}"))),
        }
        i += 2;
    }
    let out = out.ok_or_else(|| LookdevError::Config("--out <dir> is required".into()))?;
    std::fs::create_dir_all(&out).map_err(|e| io_err(&out, e))?;
    if let Some(dir) = stage {
        stage_shaders(&dir)?;
    }
    let settings = RenderSettings::from_profile(preset);
    let device = RenderDevice::headless()?;
    for shot in LookShot::ALL {
        if only.as_deref().is_some_and(|name| name != shot.name()) {
            continue;
        }
        let mut renderer = FrameRenderer::new();
        renderer.set_forward_lite(lite);
        renderer.set_gpu_timing(&device, true);
        let tweak = |scene: &mut LabScene| add_lights(scene, lights, light_shadows);
        let pixels = render_shot_with(&device, &mut renderer, shot, options, &settings, &tweak)?;
        let t = renderer.gpu_timings();
        let path = out.join(format!("{}.png", shot.name()));
        write_png(&path, options.width, options.height, &pixels)?;
        println!(
            "bench: {} preset {} lite {} lights {lights} shadows {light_shadows} {}x{} gpu ms: scene {:.2} shadows {:.2} forward {:.2} post {:.2}",
            shot.name(),
            preset.name(),
            lite.map_or("auto", ForwardLite::name),
            options.width,
            options.height,
            t.scene_ms,
            t.shadows_ms,
            t.forward_ms,
            t.post_ms,
        );
    }
    Ok(())
}
