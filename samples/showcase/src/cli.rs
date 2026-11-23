// SPDX-License-Identifier: MIT-0
//! Command-line parsing for the `showcase` binary.

use std::path::PathBuf;

use regia_render::ProfileName;
use regia_render::cinematic::take_profile_arg;

/// What the command line asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Print the error codes and exit.
    ListErrors,
    /// Write the example scene into a directory and exit.
    WriteExample(PathBuf),
    /// Load a scene and run it.
    Run(RunArgs),
}

/// How a scene is run.
#[derive(Clone, Debug, PartialEq)]
pub struct RunArgs {
    /// The scene directory.
    pub scene: PathBuf,
    /// Open a window instead of running headless.
    pub window: bool,
    /// Open the window borderless fullscreen on the current monitor (`--fullscreen`, implies `--window`):
    /// 1280x800 on a Steam Deck, the size the `Deck` render profile is tuned for.
    pub fullscreen: bool,
    /// Per-pass GPU timestamps in the window, so telemetry reports GPU time (`--gpu-timing`).
    pub gpu_timing: bool,
    /// Load and validate the scene, print a summary and exit (no GPU).
    pub check: bool,
    /// Image size in pixels.
    pub size: (u32, u32),
    /// The plain, bit-repeatable look.
    pub plain: bool,
    /// Scatter mesh level of detail.
    pub scatter_lod: usize,
    /// Fixed camera exposure (EV100) overriding the scene's.
    pub exposure_ev: Option<f32>,
    /// Keep the GPU cull's Hi-Z occlusion test (faster, not bit-repeatable with temporal anti-aliasing).
    pub hiz: bool,
    /// The GPU-driven draw path (the default since ADR-1415; `--cpu-draw` turns it off, `--gpu-driven` is kept
    /// and asks for the default).
    pub gpu_driven: bool,
    /// Draw the scatter as capped entities instead of the streamed field (`--entity-scatter`).
    pub entity_scatter: bool,
    /// The render profile asked for with `--render-profile NAME`: a preset or a tier with no preset row
    /// (`VeryLow`, `Medium`; ADR-8844). `None`: the environment, the scene's `render_profile.txt`, then Desktop
    /// High.
    pub profile: Option<ProfileName>,
    /// Grow grass with the default grass file even without `terrain/grass.ron` (`--grass`).
    pub grass: bool,
    /// Ground the scatter into the terrain with a contact map (`--contact-ao`).
    pub contact_ao: bool,
    /// Sway the scatter's trees in a breeze (`--foliage-wind`, spec 88 § 2.5).
    pub foliage_wind: bool,
    /// Play the forest ambience bed with generated placeholder loops (`--ambience`, ADR-2442).
    pub ambience: bool,
    /// Ambient life: motes, insects, leaves and birds (`--ambient-life`, ADR-3024).
    pub ambient_life: bool,
    /// Where the agent endpoint is advertised (`.regia/agent/`).
    pub project_dir: Option<PathBuf>,
    /// Quit after this many fixed steps (`0`: run until the agent asks to quit).
    pub exit_after_frames: u64,
    /// Walk the scene in a headset (`--xr simulator | openxr | auto`, ADR-2457); `None` keeps the desktop camera.
    pub xr: Option<regia_xr::XrBackendKind>,
    /// The walk's comfort preset (`--xr-preset comfortable | moderate | free`; default moderate).
    pub xr_preset: regia_xr::ComfortPreset,
    /// Directories `replay.load` may read from on top of the agent's allowed roots (`--replay-root DIR`,
    /// repeatable; read only, ADR-1965): a replay store kept outside the project.
    pub replay_roots: Vec<PathBuf>,
    /// The interactive demo (`--demo`, ADR-3013): free-fly camera, sliders, tier picker, frame-rate overlay,
    /// screenshots, benchmark and credits. Opens a window unless `--headless` follows. Without a scene directory
    /// it runs the example scene.
    pub demo: bool,
    /// Where the demo writes screenshots and benchmark tables (`--demo-out DIR`; default the user data folder).
    pub demo_out: Option<PathBuf>,
    /// The demo's starting tier (`--tier NAME`); `None` (or `auto`) detects one from the GPU.
    pub tier: Option<regia_render::QualityTier>,
}

/// The usage text.
pub const USAGE: &str = "usage: showcase <SCENE_DIR> [--window] [--fullscreen] [--gpu-timing] [--check] [--size WxH] [--plain] [--scatter-lod N] [--exposure-ev EV] [--hiz] [--gpu-driven]\n\
       \x20               [--cpu-draw] [--entity-scatter] [--render-profile NAME] [--grass] [--contact-ao] [--foliage-wind] [--ambience] [--ambient-life]\n\
       \x20               [--project-dir DIR] [--exit-after-frames N] [--replay-root DIR]...\n\
       \x20               [--xr simulator|openxr|auto] [--xr-preset comfortable|moderate|free]\n\
       \x20      showcase --demo [SCENE_DIR] [--headless] [--demo-out DIR] [--tier auto|Low|Medium|High|Ultra|Deck] [--size WxH]\n\
       \x20      showcase --write-example DIR\n\
       \x20      showcase --list-errors";

fn number<T: std::str::FromStr>(flag: &str, value: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("{flag}: {value} is not a valid number"))
}

/// Parses the arguments after the program name.
///
/// # Errors
/// A message naming the flag or value that is wrong.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command, String> {
    // The engine's own reader takes `--render-profile NAME` (or `=NAME`) out first, so the loop below never
    // sees it.
    let (profile, rest) = take_profile_arg(args).map_err(|e| e.to_string())?;
    let mut args = rest.into_iter();
    let mut scene: Option<PathBuf> = None;
    let mut run = RunArgs {
        scene: PathBuf::new(),
        window: false,
        fullscreen: false,
        gpu_timing: false,
        check: false,
        size: (1920, 1080),
        plain: false,
        scatter_lod: 1,
        exposure_ev: None,
        hiz: false,
        gpu_driven: true,
        entity_scatter: false,
        profile,
        grass: false,
        contact_ao: false,
        foliage_wind: false,
        ambience: false,
        ambient_life: false,
        project_dir: None,
        exit_after_frames: 0,
        xr: None,
        xr_preset: regia_xr::ComfortPreset::Moderate,
        replay_roots: Vec::new(),
        demo: false,
        demo_out: None,
        tier: None,
    };
    let mut scatter_lod_given = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list-errors" => return Ok(Command::ListErrors),
            "--window" => run.window = true,
            "--fullscreen" => {
                run.window = true;
                run.fullscreen = true;
            }
            "--headless" => run.window = false,
            "--gpu-timing" => run.gpu_timing = true,
            "--check" => run.check = true,
            "--plain" => run.plain = true,
            "--hiz" => run.hiz = true,
            "--gpu-driven" => run.gpu_driven = true,
            "--cpu-draw" => run.gpu_driven = false,
            "--entity-scatter" => run.entity_scatter = true,
            "--grass" => run.grass = true,
            "--contact-ao" => run.contact_ao = true,
            "--foliage-wind" => run.foliage_wind = true,
            "--ambience" => run.ambience = true,
            "--demo" => {
                run.demo = true;
                run.window = true;
            }
            "--ambient-life" => run.ambient_life = true,
            flag @ ("--write-example"
            | "--size"
            | "--scatter-lod"
            | "--exposure-ev"
            | "--project-dir"
            | "--replay-root"
            | "--xr"
            | "--xr-preset"
            | "--demo-out"
            | "--tier"
            | "--exit-after-frames") => {
                let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
                match flag {
                    "--write-example" => return Ok(Command::WriteExample(value.into())),
                    "--size" => {
                        let (w, h) = value
                            .split_once('x')
                            .ok_or_else(|| format!("--size wants WxH, got {value}"))?;
                        run.size = (number("--size", w)?, number("--size", h)?);
                        if run.size.0 == 0 || run.size.1 == 0 || run.size.0 > 8192 || run.size.1 > 8192 {
                            return Err("--size must be within 1x1 and 8192x8192".into());
                        }
                    }
                    "--xr" => {
                        run.xr =
                            Some(crate::xr::backend_from_name(&value).ok_or_else(|| {
                                format!("--xr wants simulator, openxr or auto, got {value}")
                            })?);
                    }
                    "--xr-preset" => {
                        run.xr_preset = regia_xr::ComfortPreset::from_name(&value).ok_or_else(|| {
                            format!("--xr-preset wants comfortable, moderate or free, got {value}")
                        })?;
                    }
                    "--scatter-lod" => {
                        scatter_lod_given = true;
                        run.scatter_lod = number("--scatter-lod", &value)?;
                        if run.scatter_lod > 3 {
                            return Err("--scatter-lod must be within 0..=3".into());
                        }
                    }
                    "--exposure-ev" => {
                        let ev: f32 = number("--exposure-ev", &value)?;
                        if !ev.is_finite() {
                            return Err("--exposure-ev must be finite".into());
                        }
                        run.exposure_ev = Some(ev);
                    }
                    "--project-dir" => run.project_dir = Some(value.into()),
                    "--demo-out" => run.demo_out = Some(value.into()),
                    "--tier" => {
                        run.tier = if value.eq_ignore_ascii_case("auto") {
                            None
                        } else {
                            Some(regia_render::QualityTier::from_name(&value).ok_or_else(|| {
                                format!("--tier wants auto, Low, Medium, High, Ultra, Deck, MobileLow or MobileHigh, got {value}")
                            })?)
                        };
                    }
                    "--replay-root" => {
                        if value.is_empty() {
                            return Err("--replay-root needs a directory".into());
                        }
                        run.replay_roots.push(value.into());
                    }
                    _ => run.exit_after_frames = number("--exit-after-frames", &value)?,
                }
            }
            other if other.starts_with("--") => return Err(format!("unknown flag {other}")),
            _ => {
                if scene.replace(arg.into()).is_some() {
                    return Err("only one scene directory may be given".into());
                }
            }
        }
    }
    // The demo runs the example scene when none is given (`main` resolves the empty path).
    run.scene = match scene {
        Some(scene) => scene,
        None if run.demo => PathBuf::new(),
        None => return Err("a scene directory is required".to_string()),
    };
    if (run.demo_out.is_some() || run.tier.is_some()) && !run.demo {
        return Err("--demo-out and --tier need --demo".to_string());
    }
    if run.xr.is_some() && !scatter_lod_given {
        // The walk's cheaper foliage tier: a coarser scatter mesh unless the command line names one.
        run.scatter_lod = crate::xr::XR_SCATTER_LOD;
    }
    Ok(Command::Run(run))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::RenderProfilePreset;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_scene_alone_runs_headless_with_defaults() {
        let Ok(Command::Run(run)) = parse(args(&["scene"])) else {
            panic!("expected a run");
        };
        assert_eq!(run.scene, PathBuf::from("scene"));
        assert!(!run.window && !run.fullscreen && !run.check && !run.plain && !run.hiz);
        assert_eq!(run.size, (1920, 1080));
        assert_eq!(run.exit_after_frames, 0);
    }

    #[test]
    fn flags_are_read() {
        let Ok(Command::Run(run)) = parse(args(&[
            "--window",
            "s",
            "--size",
            "640x360",
            "--plain",
            "--check",
            "--scatter-lod",
            "2",
            "--exposure-ev",
            "13.5",
            "--project-dir",
            "p",
            "--exit-after-frames",
            "9",
            "--hiz",
            "--gpu-driven",
        ])) else {
            panic!("expected a run");
        };
        assert!(run.window && run.plain && run.check && run.hiz && run.gpu_driven);
        assert_eq!(
            (run.size, run.scatter_lod, run.exit_after_frames),
            ((640, 360), 2, 9)
        );
        assert_eq!(run.project_dir, Some(PathBuf::from("p")));
        assert!(run.replay_roots.is_empty());
    }

    #[test]
    fn replay_roots_repeat_and_need_a_value() {
        let Ok(Command::Run(run)) = parse(args(&["s", "--replay-root", "/srv/a", "--replay-root", "/srv/b"]))
        else {
            panic!("expected a run");
        };
        assert_eq!(
            run.replay_roots,
            [PathBuf::from("/srv/a"), PathBuf::from("/srv/b")]
        );
        assert!(parse(args(&["s", "--replay-root"])).is_err());
        assert!(parse(args(&["s", "--replay-root", ""])).is_err());
    }

    #[test]
    fn the_render_profile_and_feature_flags_are_read() {
        let Ok(Command::Run(run)) = parse(args(&[
            "s",
            "--render-profile",
            "cinematic",
            "--grass",
            "--contact-ao",
            "--size",
            "3840x2160",
        ])) else {
            panic!("expected a run");
        };
        assert_eq!(
            run.profile,
            Some(ProfileName::Preset(RenderProfilePreset::Cinematic))
        );
        assert!(run.grass && run.contact_ao);
        assert_eq!(run.size, (3840, 2160));
        let Ok(Command::Run(eq)) = parse(args(&["--render-profile=DesktopLow", "s"])) else {
            panic!("expected a run");
        };
        assert_eq!(
            eq.profile,
            Some(ProfileName::Preset(RenderProfilePreset::DesktopLow))
        );
        // ADR-8844: the integrated-GPU floor by name.
        let Ok(Command::Run(floor)) = parse(args(&["s", "--render-profile", "VeryLow"])) else {
            panic!("expected a run");
        };
        assert_eq!(
            floor.profile,
            Some(ProfileName::Tier(regia_render::QualityTier::VeryLow))
        );
        assert!(parse(args(&["s", "--render-profile", "Low"])).is_err());
        let Ok(Command::Run(none)) = parse(args(&["s"])) else {
            panic!("expected a run");
        };
        assert_eq!((none.profile, none.grass, none.contact_ao), (None, false, false));
    }

    #[test]
    fn ambient_life_is_opt_in() {
        let Ok(Command::Run(on)) = parse(args(&["s", "--ambient-life"])) else {
            panic!("expected a run");
        };
        assert!(on.ambient_life);
        let Ok(Command::Run(off)) = parse(args(&["s"])) else {
            panic!("expected a run");
        };
        assert!(!off.ambient_life);
        assert!(USAGE.contains("[--ambient-life]"));
    }

    #[test]
    fn foliage_wind_is_opt_in() {
        let Ok(Command::Run(on)) = parse(args(&["s", "--foliage-wind"])) else {
            panic!("expected a run");
        };
        assert!(on.foliage_wind);
        let Ok(Command::Run(off)) = parse(args(&["s"])) else {
            panic!("expected a run");
        };
        assert!(!off.foliage_wind);
        assert!(USAGE.contains("[--foliage-wind]"));
    }

    #[test]
    fn ambience_is_opt_in() {
        let Ok(Command::Run(on)) = parse(args(&["s", "--ambience"])) else {
            panic!("expected a run");
        };
        assert!(on.ambience);
        let Ok(Command::Run(off)) = parse(args(&["s"])) else {
            panic!("expected a run");
        };
        assert!(!off.ambience);
        assert!(USAGE.contains("[--ambience]"));
    }

    #[test]
    fn fullscreen_implies_a_window() {
        let Ok(Command::Run(run)) = parse(args(&["s", "--fullscreen"])) else {
            panic!("expected a run");
        };
        assert!(run.window && run.fullscreen);
        let Ok(Command::Run(windowed)) = parse(args(&["s", "--window"])) else {
            panic!("expected a run");
        };
        assert!(windowed.window && !windowed.fullscreen);
        // `--headless` after `--fullscreen` turns the window off; the mode flag is then unused.
        let Ok(Command::Run(headless)) = parse(args(&["s", "--fullscreen", "--headless"])) else {
            panic!("expected a run");
        };
        assert!(!headless.window);
        let Ok(Command::Run(timed)) = parse(args(&["s", "--fullscreen", "--gpu-timing"])) else {
            panic!("expected a run");
        };
        assert!(timed.gpu_timing && !windowed.gpu_timing);
    }

    #[test]
    fn a_bad_render_profile_is_refused() {
        assert!(parse(args(&["s", "--render-profile", "Ultra9000"])).is_err());
        assert!(parse(args(&["s", "--render-profile"])).is_err());
    }

    #[test]
    fn the_other_commands() {
        assert_eq!(parse(args(&["--list-errors"])), Ok(Command::ListErrors));
        assert_eq!(
            parse(args(&["--write-example", "d"])),
            Ok(Command::WriteExample(PathBuf::from("d")))
        );
    }

    #[test]
    fn bad_input_is_refused() {
        for bad in [
            &[][..],
            &["a", "b"],
            &["--size"],
            &["a", "--size", "10"],
            &["a", "--size", "0x10"],
            &["a", "--size", "9999x10"],
            &["a", "--scatter-lod", "9"],
            &["a", "--exposure-ev", "inf"],
            &["a", "--exit-after-frames", "x"],
            &["a", "--bogus"],
        ] {
            assert!(parse(args(bad)).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn the_demo_flags_parse_and_the_scene_is_optional() {
        let Ok(Command::Run(run)) = parse(args(&["--demo"])) else {
            panic!("expected a run");
        };
        assert!(run.demo && run.window);
        assert_eq!(run.scene, PathBuf::new());
        assert_eq!(run.tier, None);
        let Ok(Command::Run(run)) = parse(args(&[
            "s",
            "--demo",
            "--headless",
            "--demo-out",
            "o",
            "--tier",
            "medium",
        ])) else {
            panic!("expected a run");
        };
        assert!(run.demo && !run.window);
        assert_eq!(run.demo_out, Some(PathBuf::from("o")));
        assert_eq!(run.tier, Some(regia_render::QualityTier::Medium));
        let Ok(Command::Run(auto)) = parse(args(&["--demo", "--tier", "auto"])) else {
            panic!("expected a run");
        };
        assert_eq!(auto.tier, None);
        assert!(parse(args(&["--demo", "--tier", "Ultra9000"])).is_err());
        assert!(
            parse(args(&["s", "--tier", "High"])).is_err(),
            "--tier needs --demo"
        );
        assert!(
            parse(args(&["s", "--demo-out", "o"])).is_err(),
            "--demo-out needs --demo"
        );
        assert!(USAGE.contains("--demo"));
    }

    #[test]
    fn xr_flags_parse_and_pick_the_cheaper_scatter_mesh() {
        let Ok(Command::Run(run)) = parse(args(&["scene", "--xr", "simulator", "--xr-preset", "free"]))
        else {
            panic!("expected a run");
        };
        assert_eq!(run.xr, Some(regia_xr::XrBackendKind::Simulator));
        assert_eq!(run.xr_preset, regia_xr::ComfortPreset::Free);
        assert_eq!(run.scatter_lod, crate::xr::XR_SCATTER_LOD);
        let Ok(Command::Run(run)) = parse(args(&["scene", "--scatter-lod", "0", "--xr", "auto"])) else {
            panic!("expected a run");
        };
        assert_eq!(run.scatter_lod, 0, "an explicit level wins");
        assert_eq!(run.xr_preset, regia_xr::ComfortPreset::Moderate);
        let Ok(Command::Run(run)) = parse(args(&["scene"])) else {
            panic!("expected a run");
        };
        assert_eq!(run.xr, None);
        assert_eq!(run.scatter_lod, 1, "no walk, the filming default");
        assert!(parse(args(&["scene", "--xr", "vr"])).is_err());
        assert!(parse(args(&["scene", "--xr-preset", "wild"])).is_err());
        assert!(parse(args(&["scene", "--xr"])).is_err());
    }
}
