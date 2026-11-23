// SPDX-License-Identifier: MIT-0
//! `showcase`: loads a scene directory and runs it for filming. See `book/agents/showcase.md`.

use std::process::ExitCode;
use std::sync::Arc;

use regia_render::RenderProfilePreset;
use regia_render::cinematic::{RENDER_PROFILE_ENV, choose_profile, gate_preset};
use showcase::app::{Options, WindowOptions, build_headless_app, run_windowed_with_exit};
use showcase::cli::{Command, RunArgs, USAGE, parse};
use showcase::features::FeatureOptions;
use showcase::scene::load_scene_with;
use showcase::{ErrorCode, example};

fn main() -> ExitCode {
    let command = match parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(err) => {
            eprintln!("showcase: {err}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match command {
        Command::ListErrors => {
            for code in ErrorCode::ALL {
                println!("{}", code.as_str());
            }
            ExitCode::SUCCESS
        }
        Command::WriteExample(dir) => match example::write_example_scene(&dir) {
            Ok(()) => {
                println!("showcase: wrote the example scene to {}", dir.display());
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("showcase: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Run(run) => run_scene(&run),
    }
}

/// The scene `--demo` runs when the command line names none: the example scene beside the binary or in the source
/// tree, else the generated example written to `example_scene` in the demo's output folder (the same files on every
/// launch, so launches share one folder).
fn demo_scene(run: &RunArgs) -> Result<std::path::PathBuf, String> {
    if !run.scene.as_os_str().is_empty() {
        return Ok(run.scene.clone());
    }
    if let Some(dir) = showcase::demo::find_default_scene() {
        return Ok(dir);
    }
    let dir = run
        .demo_out
        .clone()
        .unwrap_or_else(showcase::demo::default_out_dir)
        .join("example_scene");
    example::write_demo_example_scene(&dir)?;
    Ok(dir)
}

fn run_scene(run: &RunArgs) -> ExitCode {
    let mut run = run.clone();
    if run.demo {
        match demo_scene(&run) {
            Ok(dir) => run.scene = dir,
            Err(err) => {
                eprintln!("showcase: demo scene: {err}");
                return ExitCode::FAILURE;
            }
        }
    }
    let run = &run;
    let scene = match load_scene_with(&run.scene, &FeatureOptions { grass: run.grass }) {
        Ok(scene) => Arc::new(scene),
        Err(err) => {
            eprintln!("showcase: error[{}]: {err}", err.code.as_str());
            return ExitCode::from(3);
        }
    };
    println!("showcase: loaded {scene:?}");
    if run.check {
        return ExitCode::SUCCESS;
    }
    // The command line, then REGIA_RENDER_PROFILE, then the scene's render_profile.txt, then Desktop High.
    // A tier with no preset row (`VeryLow`, `Medium`) is accepted too (ADR-8844).
    let choice = match choose_profile(
        run.profile,
        std::env::var(RENDER_PROFILE_ENV).ok().as_deref(),
        Some(&run.scene),
        RenderProfilePreset::DesktopHigh,
    ) {
        Ok(choice) => choice,
        Err(err) => {
            eprintln!("showcase: render profile: {err}");
            return ExitCode::from(2);
        }
    };
    let gate = gate_preset(choice.profile.preset(), None, cfg!(target_os = "android"), false);
    println!(
        "showcase: render profile {} ({}){}",
        if gate.honoured() {
            choice.profile.name()
        } else {
            gate.applied.name()
        },
        choice.source.name(),
        gate.reason.map_or_else(String::new, |r| format!(
            ", asked for {} but {r}",
            gate.requested.name()
        ))
    );
    let options = Options {
        profile: Some(choice.profile.preset()),
        tier: choice.profile.tier(),
        contact_ao: run.contact_ao,
        size: run.size,
        plain: run.plain,
        scatter_lod: run.scatter_lod,
        exposure_ev: run.exposure_ev,
        hiz: run.hiz,
        gpu_driven: run.gpu_driven,
        entity_scatter: run.entity_scatter,
        foliage_wind: run.foliage_wind,
        ambience: run.ambience,
        ambient_life: run.ambient_life,
    };
    if let Some(backend) = run.xr {
        println!(
            "showcase: xr walk ({}, preset {})",
            backend.name(),
            run.xr_preset.name()
        );
    }
    let exit = if run.demo {
        let demo = showcase::demo::DemoOptions {
            out_dir: run
                .demo_out
                .clone()
                .unwrap_or_else(showcase::demo::default_out_dir),
            // `--tier` first; else a tier named with `--render-profile` (ADR-8844) starts the demo there.
            tier: run.tier.or_else(|| choice.profile.tier()),
            exit_after_frames: run.exit_after_frames,
            ..showcase::demo::DemoOptions::default()
        };
        println!("showcase: demo, output folder {}", demo.out_dir.display());
        if run.window {
            showcase::demo::run_windowed_demo(
                scene,
                options,
                run.project_dir.clone(),
                WindowOptions {
                    fullscreen: run.fullscreen,
                    gpu_timing: run.gpu_timing,
                },
                demo,
            )
        } else {
            let mut app =
                match showcase::demo::build_headless_demo(scene, options, run.project_dir.clone(), demo) {
                    Ok(app) => app,
                    Err(err) => {
                        eprintln!("showcase: {err}");
                        return ExitCode::from(4);
                    }
                };
            if run.exit_after_frames > 0 {
                app.add_plugins(showcase::app::ExitAfter(run.exit_after_frames));
            }
            showcase::app::run_headless(app)
        }
    } else if let (true, Some(backend)) = (run.window, run.xr) {
        showcase::xr::run_windowed_xr(
            scene,
            options,
            run.project_dir.clone(),
            WindowOptions {
                fullscreen: run.fullscreen,
                gpu_timing: run.gpu_timing,
            },
            run.replay_roots.clone(),
            backend,
            run.xr_preset,
        )
    } else if run.window {
        run_windowed_with_exit(
            scene,
            options,
            run.project_dir.clone(),
            WindowOptions {
                fullscreen: run.fullscreen,
                gpu_timing: run.gpu_timing,
            },
            run.replay_roots.clone(),
            run.exit_after_frames,
        )
    } else {
        let mut app = match build_headless_app(Arc::clone(&scene), options, run.project_dir.clone()) {
            Ok(app) => app,
            Err(err) => {
                eprintln!("showcase: {err}");
                return ExitCode::from(4);
            }
        };
        if let Some(backend) = run.xr {
            showcase::xr::add_headless_xr(&mut app, &scene, backend, run.xr_preset);
        }
        if run.exit_after_frames > 0 {
            app.add_plugins(showcase::app::ExitAfter(run.exit_after_frames));
        }
        if !run.replay_roots.is_empty() {
            #[cfg(feature = "agent")]
            app.insert_resource(regia_agent::ReplayRoots(run.replay_roots.clone()));
            #[cfg(not(feature = "agent"))]
            {
                eprintln!("showcase: --replay-root needs the `agent` feature");
                return ExitCode::from(2);
            }
        }
        showcase::app::run_headless(app)
    };
    match exit {
        showcase::app::AppExit::Success => ExitCode::SUCCESS,
        showcase::app::AppExit::Error(code) => ExitCode::from(code.get()),
    }
}
