// SPDX-License-Identifier: MPL-2.0
//! Headless AI-vs-AI sample and JSON agent runner for S-18 (ADR-8151).

use std::io::Write;
use std::process::ExitCode;

use regia_detsim::arena::{ArenaScenario, ArenaSettings};
use regia_detsim::golden::{DetRun, TickRate};
use regia_detsim::harness::{HarnessError, SimHarness};
use regia_detsim::{profile, soak};

const HELP: &str = "ai_arena [--seed U64] [--ticks U32] [--rate-hz U32] [--units-per-team U16]\n\
    [--soak PAIRS | --profile SAMPLES [--warmup TICKS]]\n\
Run: defaults seed=100, ticks=1200, rate=60, units-per-team=4.\n\
Profile: --ticks is not accepted; total ticks = warmup (default 30) + samples.\n\
Bounds: rate 1..=1000, units-per-team 1..=128, total ticks <= 1000000,\n\
pairs 1..=1000, estimated pairwise work <= 100000000.\n\
One regia-det-sim JSON line on stdout; exits 0=ok, 1=diverged, 2=error.\n";

#[derive(Clone, Copy)]
enum Mode {
    Run,
    Soak(u32),
    Profile(u32),
}

struct Options {
    seed: u64,
    ticks: u32,
    rate: TickRate,
    units: u16,
    mode: Mode,
    warmup: u32,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Options, &'static str> {
    let mut options = Options {
        seed: 100,
        ticks: 1200,
        rate: TickRate::DEFAULT,
        units: 4,
        mode: Mode::Run,
        warmup: 30,
    };
    let mut seen = 0u16;
    let mut args = args;
    while let Some(key) = args.next() {
        let flag = match key.as_str() {
            "--seed" => 1,
            "--ticks" => 2,
            "--rate-hz" => 4,
            "--units-per-team" => 8,
            "--soak" => 16,
            "--profile" => 32,
            "--warmup" => 64,
            _ => return Err("invalid_arguments"),
        };
        if seen & flag != 0 {
            return Err("invalid_arguments");
        }
        seen |= flag;
        let value = args.next().ok_or("invalid_arguments")?;
        match flag {
            1 => options.seed = value.parse().map_err(|_| "invalid_arguments")?,
            2 => options.ticks = value.parse().map_err(|_| "invalid_arguments")?,
            4 => {
                let hz = value.parse::<u32>().map_err(|_| "invalid_arguments")?;
                if hz > 1000 {
                    return Err("invalid_arguments");
                }
                options.rate = TickRate::new(hz).ok_or("invalid_arguments")?;
            }
            8 => options.units = value.parse().map_err(|_| "invalid_arguments")?,
            16 => options.mode = Mode::Soak(value.parse().map_err(|_| "invalid_arguments")?),
            32 => options.mode = Mode::Profile(value.parse().map_err(|_| "invalid_arguments")?),
            64 => options.warmup = value.parse().map_err(|_| "invalid_arguments")?,
            _ => return Err("invalid_arguments"),
        }
    }
    if seen & 16 != 0 && seen & 32 != 0
        || seen & 64 != 0 && seen & 32 == 0
        || seen & 2 != 0 && seen & 32 != 0
        || !(1..=128).contains(&options.units)
    {
        return Err("invalid_arguments");
    }
    let (ticks, worlds) = match options.mode {
        Mode::Run => (options.ticks, 1),
        Mode::Soak(pairs) => {
            if pairs == 0 {
                return Err("empty_soak");
            }
            if pairs > 1000 {
                return Err("work_limit");
            }
            (options.ticks, u64::from(pairs) * 2)
        }
        Mode::Profile(samples) => {
            if samples == 0 {
                return Err("empty_profile");
            }
            (options.warmup.checked_add(samples).ok_or("tick_overflow")?, 1)
        }
    };
    let units = u64::from(options.units) * 2;
    if ticks > 1_000_000 || u64::from(ticks.max(1)) * units * units * worlds > 100_000_000 {
        return Err("work_limit");
    }
    Ok(options)
}

fn run(options: Options) -> Result<(String, u8), HarnessError> {
    let settings = ArenaSettings {
        units_per_team: options.units,
        ..ArenaSettings::default()
    };
    let arena = ArenaScenario::new(settings)?;
    let config = DetRun::new(options.seed, options.ticks, options.rate);
    let common = format!(
        "\"scenario\":\"ai_arena\",\"target\":\"{}\",\"seed\":\"{:016x}\",\"rate_hz\":{},\"units_per_team\":{}",
        regia_detsim::golden::current_target(),
        options.seed,
        options.rate.hz(),
        options.units
    );
    match options.mode {
        Mode::Run => {
            let mut harness = SimHarness::new(arena, config)?;
            harness.step_n(config.ticks)?;
            let snapshot = harness.snapshot();
            let stats = harness.state().stats();
            Ok((
                format!(
                    "{{{common},\"mode\":\"run\",\"status\":\"ok\",\"ticks\":{},\"fingerprint\":\"{:016x}\",\"alive\":[{},{}],\"health\":[{},{}],\"attacks\":{}}}",
                    snapshot.tick,
                    snapshot.fingerprint,
                    stats.alive[0],
                    stats.alive[1],
                    stats.health[0],
                    stats.health[1],
                    stats.attacks
                ),
                0,
            ))
        }
        Mode::Soak(pairs) => {
            let report = soak::run(|| arena.clone(), config, pairs)?;
            let divergence = report.divergence.map_or_else(
                || "null".to_owned(),
                |d| {
                    format!(
                        "{{\"pair\":{},\"tick\":{},\"expected\":\"{:016x}\",\"actual\":\"{:016x}\"}}",
                        d.pair, d.expected.tick, d.expected.fingerprint, d.actual.fingerprint
                    )
                },
            );
            let status = if report.passed() { "ok" } else { "diverged" };
            Ok((
                format!(
                    "{{{common},\"mode\":\"soak\",\"status\":\"{status}\",\"ticks\":{},\"fingerprint\":\"{:016x}\",\"pairs_completed\":{},\"steps_compared\":{},\"divergence\":{divergence}}}",
                    report.final_snapshot.tick,
                    report.final_snapshot.fingerprint,
                    report.pairs_completed,
                    report.steps_compared
                ),
                u8::from(!report.passed()),
            ))
        }
        Mode::Profile(samples) => {
            let report = profile::run(
                arena,
                profile::ProfileConfig {
                    seed: options.seed,
                    rate: options.rate,
                    warmup_ticks: options.warmup,
                    samples,
                },
            )?;
            Ok((
                format!(
                    "{{{common},\"mode\":\"profile\",\"status\":\"ok\",\"ticks\":{},\"fingerprint\":\"{:016x}\",\"warmup_ticks\":{},\"samples\":{},\"p50_ns\":{},\"p99_ns\":{},\"max_ns\":{}}}",
                    report.final_snapshot.tick,
                    report.final_snapshot.fingerprint,
                    options.warmup,
                    samples,
                    report.p50_ns,
                    report.p99_ns,
                    report.max_ns
                ),
                0,
            ))
        }
    }
}

fn main() -> ExitCode {
    #[expect(
        clippy::disallowed_methods,
        reason = "host tool: command-line arguments, read once before any sim tick (ADR-8367)"
    )]
    // det-scan: allow(env_read): host tool arguments, read once before any sim tick (ADR-8367)
    let args: Result<Vec<_>, _> = std::env::args_os().skip(1).map(|arg| arg.into_string()).collect();
    let mut stdout = std::io::stdout().lock();
    if args.as_ref().is_ok_and(|args| args == &["--help"]) {
        return if stdout.write_all(HELP.as_bytes()).is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(2)
        };
    }
    let result = args
        .map_err(|_| "invalid_arguments")
        .and_then(|args| parse(args.into_iter()))
        .and_then(|options| run(options).map_err(|error| error.code()));
    let (json, code) =
        result.unwrap_or_else(|code| (format!("{{\"status\":\"error\",\"code\":\"{code}\"}}"), 2));
    if writeln!(stdout, "regia-det-sim {json}").is_err() {
        return ExitCode::from(2);
    }
    ExitCode::from(code)
}
