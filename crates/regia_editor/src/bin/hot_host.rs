// SPDX-License-Identifier: MPL-2.0
//! Scripted hot-reload host for the M3 acceptance test.
//!
//! The process must be linked with `-C prefer-dynamic` and find the toolchain's dynamic `libstd`.
//! It builds the game crate into the same target directory, then runs a line-oriented script:
//! `reload`, `frames N`, `observe NAME`, `replace` / `append` blocks ended by `===`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use regia_editor::engine::app::prelude::*;
use regia_editor::{HotReloadConfig, HotReloadHost, reflected_count, reflected_number};

#[derive(Resource)]
struct EngineSim {
    token: u64,
}

struct Args {
    manifest: PathBuf,
    package: String,
    crate_dir: PathBuf,
    target_dir: PathBuf,
    hot_dir: PathBuf,
    script: PathBuf,
}

fn main() -> ExitCode {
    let _log = regia_editor::engine::core::log::init(regia_editor::engine::core::log::LogConfig::default());
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("RESULT fail {error}");
            return ExitCode::from(2);
        }
    };
    if !regia_editor::process_links_dynamic_std() {
        eprintln!("RESULT fail {}", regia_editor::GameLibraryError::StaticStd);
        return ExitCode::from(2);
    }
    let mut host = HotReloadHost::new(HotReloadConfig {
        manifest_path: args.manifest,
        package: args.package,
        crate_dir: args.crate_dir.clone(),
        target_dir: args.target_dir,
        hot_dir: args.hot_dir,
    });
    host.app_mut().insert_resource(EngineSim { token: 7 });
    // Engine-owned script runner: game scripts live in its `Scripts` slots (specs/20 § 3.9).
    host.app_mut().add_plugins(regia_framework::script::ScriptPlugin);
    let script = match std::fs::read_to_string(&args.script) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("RESULT fail cannot read script: {error}");
            return ExitCode::from(2);
        }
    };
    match run_script(&mut host, &args.crate_dir, &script) {
        Ok(()) => {
            println!("RESULT ok");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("RESULT fail {error}");
            ExitCode::from(1)
        }
    }
}

fn run_script(host: &mut HotReloadHost, crate_dir: &Path, script: &str) -> Result<(), String> {
    let lib = crate_dir.join("src").join("lib.rs");
    let parent = parent_pid();
    let mut lines = script.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if parent_pid() != parent {
            return Err(format!(
                "the process that started this host exited; stopping before `{line}` so no further edit lands in {}",
                lib.display()
            ));
        }
        if line == "reload" {
            let _ = host.reload();
            continue;
        }
        if let Some(count) = line.strip_prefix("frames ") {
            let count: u32 = count
                .trim()
                .parse()
                .map_err(|_| format!("bad frame count in `{line}`"))?;
            for _ in 0..count {
                host.frame();
            }
            continue;
        }
        if let Some(name) = line.strip_prefix("observe ") {
            println!("{}", observe_line(host, name.trim()));
            continue;
        }
        if line == "replace" {
            let (from, to) = read_replace(&mut lines)?;
            replace_once(&lib, &from, &to)?;
            continue;
        }
        if line == "append" {
            let body = read_until(&mut lines, "===")?;
            append_text(&lib, &body)?;
            continue;
        }
        return Err(format!("unknown script command `{line}`"));
    }
    Ok(())
}

/// The parent process id on Unix, `None` elsewhere.
///
/// ADR-8787: when the test that runs this host is killed (the merge gate's per-binary timeout),
/// the host is re-parented and used to go on editing the workspace fixture and rebuilding the
/// game for minutes after the test had gone, under the next run's feet. A changed parent stops the
/// script before its next command; the test's journal restores the fixture on the next run.
fn parent_pid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(std::os::unix::process::parent_id())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn read_replace(lines: &mut std::iter::Peekable<std::str::Lines<'_>>) -> Result<(String, String), String> {
    let from = read_until(lines, "---")?;
    let to = read_until(lines, "===")?;
    Ok((from, to))
}

fn read_until(lines: &mut std::iter::Peekable<std::str::Lines<'_>>, marker: &str) -> Result<String, String> {
    let mut body = String::new();
    for line in lines.by_ref() {
        if line.trim() == marker {
            return Ok(body);
        }
        body.push_str(line);
        body.push('\n');
    }
    Err(format!("script ended before `{marker}`"))
}

fn replace_once(path: &Path, from: &str, to: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let Some(index) = text.find(from) else {
        return Err(format!("pattern not found in {}", path.display()));
    };
    let mut next = String::with_capacity(text.len() + to.len());
    next.push_str(&text[..index]);
    next.push_str(to);
    next.push_str(&text[index + from.len()..]);
    std::fs::write(path, next).map_err(|error| error.to_string())
}

fn append_text(path: &Path, extra: &str) -> Result<(), String> {
    let mut text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(extra);
    if !extra.ends_with('\n') {
        text.push('\n');
    }
    std::fs::write(path, text).map_err(|error| error.to_string())
}

fn observe_line(host: &HotReloadHost, name: &str) -> String {
    let world = host.world();
    let speed = reflected_number(world, "hot_fixture::Mover", "speed");
    let tag = reflected_number(world, "hot_fixture::Mover", "tag");
    let heading = reflected_number(world, "hot_fixture::Mover", "heading");
    let marker = reflected_number(world, "hot_fixture::Score", "marker");
    let hits = reflected_number(world, "hot_fixture::Score", "hits");
    let ticks = reflected_number(world, "hot_fixture::SimClock", "ticks");
    let movers = reflected_count(world, "hot_fixture::Mover");
    let blink = reflected_number(world, "hot_fixture::ScriptProbe", "blink_count");
    let stamp = reflected_number(world, "hot_fixture::ScriptProbe", "blink_stamp");
    let spin = reflected_number(world, "hot_fixture::ScriptProbe", "spin_turns");
    let blinkers = script_slots(world, "hot_fixture::Blinker");
    let spinners = script_slots(world, "hot_fixture::Spinner");
    let registry = world
        .get_resource::<regia_framework::script::ScriptRegistry>()
        .map_or(0, |registry| registry.iter().count());
    let sim = world
        .get_resource::<EngineSim>()
        .map(|sim| sim.token)
        .unwrap_or(0);
    let report = host.last_report();
    let accepted = report.is_some_and(|report| report.accepted);
    let old_running = report.is_some_and(|report| report.old_code_running);
    let generation = report.map(|report| report.generation).unwrap_or(0);
    let ms = report.map(|report| report.elapsed.as_millis()).unwrap_or(0);
    let phase = report.map(|report| report.phase.as_str()).unwrap_or("idle");
    let rebuilt = report.map_or(0, |report| report.scripts_rebuilt);
    let dropped = report.map_or(0, |report| report.scripts_dropped);
    let detail = report
        .map(|report| report.detail.replace('\n', " | "))
        .unwrap_or_default();
    format!(
        "OBS name={name} accepted={accepted} old_running={old_running} generation={generation} ms={ms} speed={} tag={} heading={} marker={} hits={} ticks={} movers={movers} sim={sim} blink={} stamp={} spin={} blinkers={blinkers} spinners={spinners} registry={registry} rebuilt={rebuilt} dropped={dropped} phase={phase} detail={detail}",
        fmt_num(speed),
        fmt_int(tag),
        heading
            .map(|value| fmt_num(Some(value)))
            .unwrap_or_else(|| "absent".to_string()),
        fmt_int(marker),
        fmt_int(hits),
        fmt_int(ticks),
        fmt_int(blink),
        fmt_int(stamp),
        fmt_int(spin),
    )
}

/// Script slots of `type_path` across every `Scripts` component.
fn script_slots(world: &World, type_path: &str) -> usize {
    let Some(id) = world
        .components()
        .get_id(std::any::TypeId::of::<regia_framework::script::Scripts>())
    else {
        return 0;
    };
    world
        .archetypes()
        .iter()
        .filter(|archetype| archetype.contains(id))
        .flat_map(|archetype| archetype.entities().iter().map(|entity| entity.id()))
        .filter_map(|entity| world.get::<regia_framework::script::Scripts>(entity))
        .map(|scripts| {
            scripts
                .0
                .iter()
                .filter(|slot| slot.type_path == type_path)
                .count()
        })
        .sum()
}

fn fmt_num(value: Option<f64>) -> String {
    match value {
        Some(value) if value.fract() == 0.0 => format!("{value:.0}"),
        Some(value) => format!("{value}"),
        None => "absent".to_string(),
    }
}

fn fmt_int(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.0}"))
        .unwrap_or_else(|| "absent".to_string())
}

fn parse_args() -> Result<Args, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut manifest = None;
    let mut package = None;
    let mut crate_dir = None;
    let mut target_dir = None;
    let mut hot_dir = None;
    let mut script = None;
    let mut index = 0;
    while index < args.len() {
        let key = args[index].as_str();
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{key} needs a value"))?
            .clone();
        match key {
            "--manifest" => manifest = Some(PathBuf::from(value)),
            "--package" => package = Some(value),
            "--crate-dir" => crate_dir = Some(PathBuf::from(value)),
            "--target-dir" => target_dir = Some(PathBuf::from(value)),
            "--hot-dir" => hot_dir = Some(PathBuf::from(value)),
            "--script" => script = Some(PathBuf::from(value)),
            other => return Err(format!("unknown argument {other}")),
        }
        index += 2;
    }
    Ok(Args {
        manifest: manifest.ok_or("missing --manifest")?,
        package: package.ok_or("missing --package")?,
        crate_dir: crate_dir.ok_or("missing --crate-dir")?,
        target_dir: target_dir.ok_or("missing --target-dir")?,
        hot_dir: hot_dir.ok_or("missing --hot-dir")?,
        script: script.ok_or("missing --script")?,
    })
}
