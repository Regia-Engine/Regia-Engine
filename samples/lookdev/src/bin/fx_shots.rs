// SPDX-License-Identifier: MIT-0
//! `fx_shots`: DO27's `fireball_20m` and `smoke_80m` with the showcase explosion and smoke on
//! DO27's ground, before (flat soft fade) and after (spherical billboards), plus the shot
//! without its effect for the hard-edge metric ([`lookdev::fx::hard_edges`]).
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> fx_shots --out <dir> [--size 1280x720] [--frames 24] [--only burn,smoke] [--burn-age 0.8]
//!          [--medium earth|mars|airless] [--age <s>] [--no-flat]
//! ```
//!
//! Writes `<shot>_<effect>_{none,before,after}.png` (effects: flash 0.05 s and fireball
//! 0.45 s after the explosion, smoke after 6 s, and smoke over fire: the column after 4 s
//! with the explosion [`lookdev::fx::BURN_FIRE_AGE`] old under it) and `metrics.txt`.

use std::path::PathBuf;
use std::process::ExitCode;

use lookdev::LookdevError;
use lookdev::default_look::write_png;
use lookdev::fx::{FxInsert, FxKind, hard_edges};
use lookdev::look_r2::{R2Options, R2Planets, R2Shot, render_fx_shot};
use regia_vfx::medium::VfxMedium;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("fx_shots: {err}");
            ExitCode::from(1)
        }
    }
}

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn run(args: &[String]) -> Result<(), LookdevError> {
    let out = PathBuf::from(
        value(args, "--out").ok_or_else(|| LookdevError::Config("--out <dir> is required".into()))?,
    );
    std::fs::create_dir_all(&out).map_err(|source| LookdevError::Io {
        path: out.display().to_string(),
        source,
    })?;
    let mut options = R2Options::evidence();
    options.width = 1280;
    options.height = 720;
    if let Some((w, h)) = value(args, "--size").and_then(|v| v.split_once('x')) {
        options.width = w.parse().unwrap_or(options.width);
        options.height = h.parse().unwrap_or(options.height);
    }
    options.frames = value(args, "--frames").and_then(|v| v.parse().ok()).unwrap_or(24);
    let device = regia_terrain::gpu::headless_bcn_device()?;
    let mut planets = R2Planets::default();
    let mut lines = Vec::new();
    let only: Option<Vec<&str>> = value(args, "--only").map(|v| v.split(',').collect());
    // The level's air (`regia_vfx::medium`): the scene plays that medium's explosion and smoke.
    let medium = match value(args, "--medium") {
        Some("airless") => VfxMedium::airless(regia_vfx::medium::EUROPA_GRAVITY),
        Some("mars") => VfxMedium::mars(),
        Some("earth") | None => VfxMedium::earth(),
        Some(other) => {
            return Err(LookdevError::Config(format!(
                "--medium {other}: earth, mars or airless"
            )));
        }
    };
    // --no-flat: skip the flat-soft-fade ("before") render.
    let no_flat = args.iter().any(|a| a == "--no-flat");
    // Capture age override (seconds) for every shot's effect.
    let age: Option<f32> = value(args, "--age").and_then(|v| v.parse().ok());
    for (shot, kind, tag) in [
        (R2Shot::Fireball20m, FxKind::Flash, "flash"),
        (R2Shot::Fireball20m, FxKind::Fireball, "fireball"),
        (R2Shot::Smoke80m, FxKind::Smoke, "smoke"),
        (R2Shot::Smoke80m, FxKind::Burn, "burn"),
        (R2Shot::Fireball20m, FxKind::Burn, "burn"),
        // The explosion on the wide camera (with --age: the ejecta arcs and where they land).
        (R2Shot::Smoke80m, FxKind::Fireball, "blast80"),
    ] {
        if only.as_ref().is_some_and(|o| !o.contains(&tag)) {
            continue;
        }
        let mut options = options;
        if kind == FxKind::Burn {
            // 1/12 s steps for the explosion inside the 4 s run.
            options.frames = options.frames.max(48);
        }
        let mut frames: Vec<Vec<u8>> = Vec::new();
        for (label, hidden, volumetric) in [
            ("none", true, true),
            ("before", false, false),
            ("after", false, true),
        ] {
            if no_flat && label == "before" {
                frames.push(frames[0].clone());
                continue;
            }
            let mut fx = FxInsert::new_in(&device, kind, options.frames, medium, age)?;
            fx.hidden = hidden;
            fx.volumetric = volumetric;
            if let Some(age) = value(args, "--burn-age").and_then(|v| v.parse().ok()) {
                fx.fire_age = age;
            }
            let (pixels, _) = render_fx_shot(&device, &mut planets, shot, options, &mut fx)?;
            if let Some(l) = fx.lighting() {
                println!("{} {tag} {label}: particle lighting {l:?}", shot.name());
            }
            let path = out.join(format!("{}_{tag}_{label}.png", shot.name()));
            write_png(&path, options.width, options.height, &pixels)?;
            println!("{} {tag} -> {}", shot.name(), path.display());
            frames.push(pixels);
        }
        for (label, i) in [("before", 1usize), ("after", 2)] {
            if no_flat && label == "before" {
                continue;
            }
            let m = hard_edges(&frames[i], &frames[0], options.width, options.height);
            let line = format!(
                "{} {tag} {label}: hard-cut pixels {}, longest straight cut {} px, effect covers {} px",
                shot.name(),
                m.pixels,
                m.longest_run,
                m.covered
            );
            println!("{line}");
            lines.push(line);
        }
    }
    let path = out.join("metrics.txt");
    std::fs::write(&path, lines.join("\n") + "\n").map_err(|source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    })?;
    Ok(())
}
