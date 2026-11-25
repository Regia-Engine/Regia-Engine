// SPDX-License-Identifier: MIT-0
//! VFX Lab — the M14 exerciser.
//!
//! - `cargo run -p vfx_lab` — the CPU acceptance suite (format round-trip, deterministic
//!   spawns, budgets, LOD, decals, shake, trails, ECS plugin).
//! - `cargo run -p vfx_lab -- --evidence <dir>` — offscreen evidence: one PNG per DO27
//!   effect (+ decal, trail, camera shake) plus a contact sheet and `metrics.txt`
//!   (used for `docs/evidence/m14/`).
//! - `cargo run -p vfx_lab -- --bench [file]` — the `specs/26` § 6 particle budget with
//!   GPU timestamps (desktop High 100k @1080p; mobile High 10k @720p).
//! - `cargo run -p vfx_lab -- --sort-bench [file]` — the alpha sort, radix against bitonic,
//!   on the smoke stress and a DO27 barrage (explosions + dust), interleaved twice.
//! - `cargo run -p vfx_lab -- --smoke-bench [file]` — the lit-puff cost: three showcase smoke
//!   columns filling a 2560×1440 frame (desktop High), VFX draw GPU time, three runs.
//! - `cargo run -p vfx_lab -- --combat-bench [file]` — a DO27 firefight (hover washes, muzzles,
//!   tracers, 30 hits a second) on Europa and on Mars at 2560×1440, VFX GPU time, three runs.
//! - `cargo run -p vfx_lab -- --decals <dir>` — offscreen scorch, pad marking and tyre ruts.
//! - `cargo run -p vfx_lab -- --window [--exit-after-frames N] [--screenshot PATH]` —
//!   the ECS runtime path in a window with the effect set cycling.
//! - `vfx_lab --deck-check [--preset NAME]` — the Steam Deck measurement run
//!   (`book/user-guide/steam-deck.md`), optionally with a Deck performance preset's knobs.
//! - `vfx_lab --deck-presets` / `--deck-validate DIR` — one measurement run per Deck performance
//!   preset and their validation (`deck_presets.json`, `deck_presets.md`; ADR-1332).

use std::path::PathBuf;

use regia::prelude::AppExit;

fn main() {
    tracing_subscriber::fmt::init();
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("help");

    let exit_code = match mode {
        "--evidence" => {
            let dir = args
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("docs/evidence/m14"));
            run_evidence(&dir)
        }
        "--bench" => {
            let out = args.get(2).map(PathBuf::from);
            run_bench(out)
        }
        "--sort-bench" => {
            let out = args.get(2).map(PathBuf::from);
            run_sort_bench(out)
        }
        "--smoke-bench" => {
            let out = args.get(2).map(PathBuf::from);
            run_smoke_bench(out)
        }
        "--combat-bench" => {
            let out = args.get(2).map(PathBuf::from);
            run_combat_bench(out)
        }
        "--decals" => {
            let dir = args
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("docs/evidence/decals"));
            run_decals(&dir)
        }
        "--crash-bench" => vfx_lab::crash_bench::run_crash_bench(
            args.get(2).map(std::path::Path::new),
            args.get(3).is_some_and(|a| a == "quick"),
        ),
        "--crash-sheets" => {
            let dir = PathBuf::from(args.get(2).map(String::as_str).unwrap_or("crash-sheets"));
            let size = match (
                args.get(3).and_then(|w| w.parse().ok()),
                args.get(4).and_then(|h| h.parse().ok()),
            ) {
                (Some(w), Some(h)) => (w, h),
                _ => (2560, 1440),
            };
            vfx_lab::crash_bench::run_crash_sheets(&dir, size)
        }
        "--window" => run_window(&args[2..]),
        "--deck-check" => run_deck_check(&args[2..]),
        "--deck-presets" => run_deck_presets(&args[2..]),
        "--deck-validate" => run_deck_validate(&args[2..]),
        "--effects" => {
            let dir = args.get(2).map(String::as_str).unwrap_or("effects");
            let code = vfx_lab::run(dir);
            println!("VFX Lab done: exit code {code}");
            code
        }
        other => {
            if other != "help" && !other.starts_with("--") {
                // `cargo run -p vfx_lab -- effects` (round-1 default: a dir of .vfx.ron)
                let code = vfx_lab::run(other);
                println!("VFX Lab done: exit code {code}");
                code
            } else {
                eprintln!(
                    "Usage: vfx_lab [--effects <dir> | --evidence <dir> | --decals <dir> | --bench [file] | --sort-bench [file] | --smoke-bench [file] | --combat-bench [file] | --crash-bench [file] [quick] | --crash-sheets <dir> [w h] | --window [--exit-after-frames N] [--screenshot PATH] | --deck-check [--minutes M] [--cap FPS|0] [--out DIR] [--fullscreen] [--preset NAME] [--panel lcd|oled] | --deck-presets [--presets all|a,b] [--minutes M] [--cooldown S] [--out DIR] [--fullscreen] [--panel lcd|oled] | --deck-validate DIR [--presets all|a,b] [--panel lcd|oled]]"
                );
                2
            }
        }
    };
    std::process::exit(exit_code);
}

/// Offscreen evidence mode (GPU).
fn run_evidence(dir: &std::path::Path) -> i32 {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("no headless GPU (lavapipe?): {err}");
            return 1;
        }
    };
    println!("GPU: {}", device.adapter.get_info().name);
    match vfx_lab::evidence::write_evidence(&device, dir) {
        Ok(failures) => {
            println!("evidence: {failures} shot(s) failed the plausibility checks");
            failures.min(1) as i32
        }
        Err(err) => {
            eprintln!("evidence render failed: {err}");
            1
        }
    }
}

/// Offscreen decal set piece (scorch, pad, ruts) plus a 1080p timing line.
fn run_decals(dir: &std::path::Path) -> i32 {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("no headless GPU (lavapipe?): {err}");
            return 1;
        }
    };
    println!("GPU: {}", device.adapter.get_info().name);
    match vfx_lab::decals::write_decals(&device, dir) {
        Ok(ms) => {
            println!(
                "decals: wrote {} — 256 @ 1920x1080 pass {ms:.3} ms",
                dir.display()
            );
            0
        }
        Err(err) => {
            eprintln!("decal render failed: {err}");
            1
        }
    }
}

/// Bench mode (GPU; run on the target hardware for the acceptance numbers).
fn run_bench(out: Option<PathBuf>) -> i32 {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("no headless GPU: {err}");
            return 1;
        }
    };
    println!("GPU: {}", device.adapter.get_info().name);
    let mut lines: Vec<String> = vec![format!(
        "adapter: {} ({:?})",
        device.adapter.get_info().name,
        device.adapter.get_info().backend
    )];
    let mut full_res = regia_vfx::VfxProfile::desktop_high();
    full_res.soft_particle_res = regia_vfx::gpu::SoftParticleRes::Full;
    let half_res = regia_vfx::VfxProfile::desktop_high();
    let profiles = [
        (
            "desktop High full-res soft particles (100k @ 1920x1080)",
            full_res,
            1920,
            1080,
        ),
        (
            "desktop High half-res soft particles (100k @ 1920x1080)",
            half_res,
            1920,
            1080,
        ),
        (
            "mobile High (10k @ 1280x720)",
            regia_vfx::VfxProfile::mobile_high(),
            1280,
            720,
        ),
    ];
    let mut ok = true;
    for (label, profile, w, h) in profiles {
        match vfx_lab::bench::run(&device, profile, label, w, h) {
            Ok(row) => {
                // specs/26 § 6: both profiles carry a 2 ms GPU budget (desktop High is
                // measured at 100k live particles, mobile High at 10k).
                const BUDGET_MS: f32 = 2.0;
                let pass = row.total_mean_ms <= BUDGET_MS;
                ok &= pass;
                lines.push(format!(
                    "{} — {} alive — sim mean {:.3} ms / p99 {:.3} — draw mean {:.3} ms / p99 {:.3} — total mean {:.3} ms — budget {BUDGET_MS:.1} ms: {}",
                    label, row.alive, row.sim_mean_ms, row.sim_p99_ms, row.draw_mean_ms, row.draw_p99_ms,
                    row.total_mean_ms, if pass { "PASS" } else { "OVER" },
                ));
            }
            Err(err) => {
                lines.push(format!("{label}: FAILED {err}"));
                ok = false;
            }
        }
    }
    for line in &lines {
        println!("{line}");
    }
    if let Some(out) = out {
        let _ = std::fs::write(&out, lines.join("\n") + "\n");
    }
    i32::from(!ok)
}

/// Lit-puff bench: three showcase smoke columns at 2560×1440, three runs.
fn run_smoke_bench(out: Option<PathBuf>) -> i32 {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("no headless GPU: {err}");
            return 1;
        }
    };
    let mut lines = vec![format!(
        "adapter: {} ({:?})",
        device.adapter.get_info().name,
        device.adapter.get_info().backend
    )];
    let mut ok = true;
    for run in 0..3 {
        match vfx_lab::bench::run_shot(
            &device,
            regia_vfx::VfxProfile::desktop_high(),
            "showcase smoke x3",
            2560,
            1440,
            vfx_lab::bench::BenchShot::ShowcaseSmoke,
            regia_vfx::gpu::SortAlgorithm::default(),
        ) {
            Ok(row) => lines.push(format!(
                "run {run}: {} alive — draw mean {:.3} ms / p99 {:.3} — sim mean {:.3} ms",
                row.alive, row.draw_mean_ms, row.draw_p99_ms, row.sim_mean_ms
            )),
            Err(err) => {
                lines.push(format!("run {run}: FAILED {err}"));
                ok = false;
            }
        }
    }
    for line in &lines {
        println!("{line}");
    }
    if let Some(out) = out {
        let _ = std::fs::write(&out, lines.join("\n") + "\n");
    }
    i32::from(!ok)
}

/// Combat bench: the DO27 firefight on Europa and on Mars at 2560×1440 with the soft-particle
/// layer at full, half and quarter resolution, three runs each.
fn run_combat_bench(out: Option<PathBuf>) -> i32 {
    use vfx_lab::bench::BenchShot;
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("no headless GPU: {err}");
            return 1;
        }
    };
    let info = device.adapter.get_info();
    let mut lines = vec![format!("adapter: {} ({:?})", info.name, info.backend)];
    let mut ok = true;
    use regia_vfx::gpu::SoftParticleRes;
    for run in 0..3 {
        for (label, shot, res) in [
            (
                "combat, Europa (airless), soft full",
                BenchShot::CombatAirless,
                SoftParticleRes::Full,
            ),
            (
                "combat, Mars, soft full",
                BenchShot::CombatMars,
                SoftParticleRes::Full,
            ),
            (
                "combat, Europa (airless), soft half",
                BenchShot::CombatAirless,
                SoftParticleRes::Half,
            ),
            (
                "combat, Mars, soft half",
                BenchShot::CombatMars,
                SoftParticleRes::Half,
            ),
            (
                "combat, Europa (airless), soft quarter",
                BenchShot::CombatAirless,
                SoftParticleRes::Quarter,
            ),
            (
                "combat, Mars, soft quarter",
                BenchShot::CombatMars,
                SoftParticleRes::Quarter,
            ),
        ] {
            let profile = regia_vfx::VfxProfile {
                soft_particle_res: res,
                ..regia_vfx::VfxProfile::desktop_high()
            };
            match vfx_lab::bench::run_shot(
                &device,
                profile,
                label,
                2560,
                1440,
                shot,
                regia_vfx::gpu::SortAlgorithm::default(),
            ) {
                Ok(row) => lines.push(format!(
                    "run {run} {label}: {} alive — sim mean {:.3} ms / p99 {:.3} — draw mean {:.3} ms / p99 {:.3} — total {:.3} ms",
                    row.alive, row.sim_mean_ms, row.sim_p99_ms, row.draw_mean_ms, row.draw_p99_ms, row.total_mean_ms
                )),
                Err(err) => {
                    lines.push(format!("run {run} {label}: FAILED {err}"));
                    ok = false;
                }
            }
        }
    }
    for line in &lines {
        println!("{line}");
    }
    if let Some(out) = out {
        let _ = std::fs::write(&out, lines.join("\n") + "\n");
    }
    i32::from(!ok)
}

/// Sort bench: the same shots with the radix and the bitonic alpha sort, interleaved.
fn run_sort_bench(out: Option<PathBuf>) -> i32 {
    use regia_vfx::gpu::SortAlgorithm;
    use vfx_lab::bench::BenchShot;
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("no headless GPU: {err}");
            return 1;
        }
    };
    let info = device.adapter.get_info();
    let mut lines = vec![format!("adapter: {} ({:?})", info.name, info.backend)];
    let big = |capacity: u32| regia_vfx::VfxProfile {
        particle_capacity: capacity,
        ..regia_vfx::VfxProfile::desktop_high()
    };
    let cases = [
        (
            "DO27 barrage, desktop High pool (100k)",
            BenchShot::Do27Barrage,
            regia_vfx::VfxProfile::desktop_high(),
        ),
        (
            "smoke stress 100k",
            BenchShot::SmokeStress,
            regia_vfx::VfxProfile::desktop_high(),
        ),
        ("smoke stress 250k", BenchShot::SmokeStress, big(250_000)),
        ("smoke stress 500k", BenchShot::SmokeStress, big(500_000)),
    ];
    let mut code = 0;
    for (label, shot, profile) in cases {
        for round in 0..2 {
            for sort in [SortAlgorithm::Bitonic, SortAlgorithm::Radix] {
                match vfx_lab::bench::run_shot(&device, profile, label, 1920, 1080, shot, sort) {
                    Ok(row) => lines.push(format!(
                        "{label} [{sort:?}, round {round}] — {} alive — sim mean {:.3} ms / p99 {:.3} — draw mean {:.3} ms — total {:.3} ms",
                        row.alive, row.sim_mean_ms, row.sim_p99_ms, row.draw_mean_ms, row.total_mean_ms
                    )),
                    Err(err) => {
                        lines.push(format!("{label} [{sort:?}]: FAILED {err}"));
                        code = 1;
                    }
                }
            }
        }
    }
    for line in &lines {
        println!("{line}");
    }
    if let Some(out) = out {
        let _ = std::fs::write(&out, lines.join("\n") + "\n");
    }
    code
}

/// The value after `name` in `args`.
fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// `--panel lcd|oled`, LCD when absent; `Err` names a bad value.
fn panel_arg(args: &[String]) -> Result<regia_steam::DeckPanel, String> {
    match flag_value(args, "--panel") {
        None => Ok(regia_steam::DeckPanel::Lcd),
        Some(name) => vfx_lab::deck_presets::panel_from_name(name)
            .ok_or_else(|| format!("--panel {name}: use lcd or oled")),
    }
}

/// The Steam Deck measurement run (`book/user-guide/steam-deck.md`): the windowed lab scene at the
/// Deck profile and 1280×800 for `--minutes` (default 5), capped at `--cap` fps (default 40, 0 =
/// uncapped), writing `deck_check.json` and screenshots to `--out`. With `--preset NAME` the run
/// applies that Deck performance preset's knobs (for `--panel`, LCD by default), caps at its cap
/// unless `--cap` says otherwise, and opens the window at the preset's game resolution.
fn run_deck_check(args: &[String]) -> i32 {
    let value = |name: &str| flag_value(args, name);
    let preset = match value("--preset").map(|name| (name, regia_steam::DeckPerfPreset::from_name(name))) {
        None => None,
        Some((_, Some(preset))) => Some(preset),
        Some((name, None)) => {
            eprintln!("deck-check: --preset {name}: use battery, balanced, performance or fsr1");
            return 2;
        }
    };
    let panel = match panel_arg(args) {
        Ok(panel) => panel,
        Err(err) => {
            eprintln!("deck-check: {err}");
            return 2;
        }
    };
    let preset_cap = preset.map(|p| p.knobs(panel).frame_cap.fps() as f32);
    let minutes: f32 = value("--minutes").and_then(|v| v.parse().ok()).unwrap_or(5.0);
    let cap: f32 = value("--cap")
        .and_then(|v| v.parse().ok())
        .or(preset_cap)
        .unwrap_or(40.0);
    let (window_w, window_h) = preset.map_or((1280, 800), |p| p.game_resolution());
    let out_dir = PathBuf::from(value("--out").unwrap_or("deck-check"));
    let out = out_dir.clone();
    let summary_path = out_dir.join("summary.json");
    if let Err(err) = std::fs::create_dir_all(&out_dir) {
        eprintln!("deck-check: cannot create {}: {err}", out_dir.display());
        return 1;
    }
    let fullscreen = args.iter().any(|a| a == "--fullscreen");
    let mut cfg = vfx_lab::deck_check::DeckCheck::new(
        std::time::Duration::from_secs_f32(minutes.max(0.05) * 60.0),
        out,
    );
    cfg.cap_fps = (cap > 0.0).then_some(cap);
    cfg.preset = preset;
    cfg.panel = panel;
    let exit = regia::run_with(
        (
            vfx_lab::window::WindowedVfxLab {
                exit_after_frames: 0,
                screenshot: None,
            },
            vfx_lab::deck_check::DeckCheckPlugin(cfg),
        ),
        |app| {
            // The sample has no JSON log, so the engine would write no summary.json: give it a path.
            app.insert_resource(regia::telemetry::Telemetry::new(
                regia::telemetry::TelemetryOptions {
                    summary_path: Some(summary_path.clone()),
                    ..Default::default()
                }
                .with_env(),
            ));
            app.insert_resource(regia::platform::WindowSettings {
                window: regia::platform::Window {
                    title: "regia.deck_check".into(),
                    logical_size: (f64::from(window_w), f64::from(window_h)),
                    resizable: false,
                    mode: if fullscreen {
                        regia::platform::WindowMode::BorderlessFullscreen
                    } else {
                        regia::platform::WindowMode::Windowed
                    },
                    ..Default::default()
                },
            });
        },
    );
    if let Ok(text) = std::fs::read_to_string(out_dir.join("deck_check.json"))
        && let Ok(report) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let f = &report["frames"];
        let num = |v: &serde_json::Value| v.as_f64().unwrap_or(f64::NAN);
        println!(
            "deck-check: {} frames, {:.1} fps avg, p50 {:.1} ms, p99 {:.1} ms, {:.1}% missed deadlines, peak {:.0} C; copy the folder {} back",
            f["frames"],
            num(&f["avg_fps"]),
            num(&f["p50_ms"]),
            num(&f["p99_ms"]),
            num(&f["missed_deadline_pct"]),
            num(&report["sensors"]["peak_temp_c"]),
            out_dir.display()
        );
        let hidden = report["hidden_frames"].as_u64().unwrap_or(0);
        if hidden > 0 {
            eprintln!(
                "deck-check: WARNING {hidden} frames ran while the window was hidden or stalled (no frame callbacks: screen blanked or window covered); the numbers are not valid. Wake the screen and rerun."
            );
        }
    } else {
        eprintln!(
            "deck-check: no deck_check.json in {} (did the run finish?)",
            out_dir.display()
        );
    }
    match exit {
        AppExit::Success => 0,
        AppExit::Error(code) => i32::from(code.get()),
    }
}

/// `vfx_lab --deck-presets`: one `--deck-check --preset` run per preset, each in its own process
/// and in `<out>/<preset>`, a `--cooldown` pause (seconds, default 30) between runs so one run's
/// heat does not bias the next, then the validation (`run_deck_validate`). Exit code 0 when every
/// preset passes or only warns, 1 when one fails, 2 on bad arguments.
fn run_deck_presets(args: &[String]) -> i32 {
    let value = |name: &str| flag_value(args, name);
    let presets = match vfx_lab::deck_presets::parse_presets(value("--presets").unwrap_or("all")) {
        Ok(presets) => presets,
        Err(err) => {
            eprintln!("deck-presets: {err}");
            return 2;
        }
    };
    if let Err(err) = panel_arg(args) {
        eprintln!("deck-presets: {err}");
        return 2;
    }
    let minutes = value("--minutes").unwrap_or("2");
    let cooldown: u64 = value("--cooldown").and_then(|v| v.parse().ok()).unwrap_or(30);
    let out_dir = PathBuf::from(value("--out").unwrap_or("deck-presets"));
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("deck-presets: cannot find this executable: {err}");
            return 1;
        }
    };
    for (i, preset) in presets.iter().enumerate() {
        if i > 0 && cooldown > 0 {
            println!("deck-presets: cooling down for {cooldown} s");
            std::thread::sleep(std::time::Duration::from_secs(cooldown));
        }
        let run_dir = out_dir.join(preset.name());
        let mut command = std::process::Command::new(&exe);
        command
            .arg("--deck-check")
            .args(["--preset", preset.name(), "--minutes", minutes])
            .arg("--out")
            .arg(&run_dir);
        if let Some(panel) = value("--panel") {
            command.args(["--panel", panel]);
        }
        if args.iter().any(|a| a == "--fullscreen") {
            command.arg("--fullscreen");
        }
        println!(
            "deck-presets: {} ({minutes} min) -> {}",
            preset.name(),
            run_dir.display()
        );
        match command.status() {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!("deck-presets: the {} run exited with {status}", preset.name()),
            Err(err) => eprintln!("deck-presets: cannot start the {} run: {err}", preset.name()),
        }
    }
    let mut validate_args = vec![out_dir.to_string_lossy().into_owned()];
    validate_args.extend([
        "--presets".to_owned(),
        presets.iter().map(|p| p.name()).collect::<Vec<_>>().join(","),
    ]);
    if let Some(panel) = value("--panel") {
        validate_args.extend(["--panel".to_owned(), panel.to_owned()]);
    }
    run_deck_validate(&validate_args)
}

/// `vfx_lab --deck-validate DIR`: validates `DIR/<preset>/deck_check.json` for each preset
/// (`--presets`, default all) and writes `deck_presets.json` and `deck_presets.md` into `DIR`
/// (`vfx_lab::deck_presets`). Runs anywhere: copy the folder back from the Deck first.
fn run_deck_validate(args: &[String]) -> i32 {
    use vfx_lab::deck_presets::{Limits, Verdict, load_sweep, parse_presets, validate_sweep, write_sweep};
    let Some(dir) = args.first().filter(|a| !a.starts_with("--")).map(PathBuf::from) else {
        eprintln!("deck-validate: usage: vfx_lab --deck-validate DIR [--presets all|a,b] [--panel lcd|oled]");
        return 2;
    };
    let presets = match parse_presets(flag_value(args, "--presets").unwrap_or("all")) {
        Ok(presets) => presets,
        Err(err) => {
            eprintln!("deck-validate: {err}");
            return 2;
        }
    };
    let panel = match panel_arg(args) {
        Ok(panel) => panel,
        Err(err) => {
            eprintln!("deck-validate: {err}");
            return 2;
        }
    };
    let limits = Limits::default();
    let sweep = validate_sweep(&load_sweep(&dir, &presets), panel, &limits);
    print!("{}", sweep.to_markdown());
    if let Err(err) = write_sweep(&dir, &sweep, &limits) {
        eprintln!(
            "deck-validate: cannot write the result into {}: {err}",
            dir.display()
        );
        return 1;
    }
    println!(
        "deck-validate: {} (details in {})",
        sweep.verdict().name(),
        dir.join(vfx_lab::deck_presets::SWEEP_JSON).display()
    );
    i32::from(sweep.verdict() == Verdict::Fail)
}

/// Project directory of this sample, resolved at runtime (ADR-10370).
///
/// `cargo run` exports `CARGO_MANIFEST_DIR`. A binary under `target/<profile>`
/// walks up to `samples/vfx_lab`. A packaged game finds neither and returns
/// `None`. The compile-time `env!("CARGO_MANIFEST_DIR")` is not remapped by rustc.
fn sample_project_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR") {
        return Some(dir.into());
    }
    let root = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .parent()?
        .to_path_buf();
    let project = root.join("samples").join("vfx_lab");
    project.is_dir().then_some(project)
}

/// Windowed ECS runtime path.
fn run_window(args: &[String]) -> i32 {
    let flag = |name: &str| -> Option<u32> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
    };
    let exit_after = flag("--exit-after-frames").unwrap_or(0);
    let screenshot = args
        .iter()
        .position(|a| a == "--screenshot")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let exit = regia::run_with(
        vfx_lab::window::WindowedVfxLab {
            exit_after_frames: exit_after,
            screenshot,
        },
        |app| {
            app.insert_resource(regia::platform::WindowSettings {
                window: regia::platform::Window {
                    // The "regia." prefix lands the window on the silent dev
                    // workspace (see the worker rules).
                    title: "regia.vfx_lab".into(),
                    ..Default::default()
                },
            });
            // Advertise the agent endpoint so `regia agent call
            // --project samples/vfx_lab seeing.screenshot` works.
            app.insert_resource(regia::agent::AgentSettings {
                project_dir: sample_project_dir(),
            });
        },
    );
    match exit {
        AppExit::Success => 0,
        AppExit::Error(code) => i32::from(code.get()),
    }
}
