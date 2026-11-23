// SPDX-License-Identifier: MPL-2.0
//! Replay a recorded input log offline and say where it parts from the recorded hashes (DC-3.7b).
//!
//! ```text
//! cargo run -p regia_detsim --example replay -- [<log>] [--snapshot <file>] [--until <tick>] [--flip <tick>]
//! ```
//!
//! - `<log>`: an input log (`InputLog::encode`). Without one, the engine's committed
//!   `goldens/input_log_arena.detlog` is replayed.
//! - `--snapshot <file>`: the framed state snapshot the log starts from, when the log does not carry
//!   it.
//! - `--until <tick>`: stop at this boundary instead of the log's end.
//! - `--flip <tick>`: (AI arena logs only) toggle seat 0's `hold` order at `tick` before replaying,
//!   to see a desync named.
//!
//! The sim is found by the tag in the log through a `DesyncReplayRegistry`; this example registers
//! the engine's AI arena. A game registers its own `ReplayHook::<MySim, N>::new()` the same way.
//! Prints the `replay verdict ...` line; exits 0 when every recorded hash matched, 1 on a divergence,
//! 2 on an error (its code on stderr).

// A host-side tool, never sim code: it reads its arguments (ADR-8367).
#![expect(
    clippy::disallowed_methods,
    reason = "offline replay tool: command-line arguments, read before any replayed tick (ADR-8367)"
)]

use std::process::ExitCode;

use regia_detsim::arena::{ArenaOrder, ArenaScenario};
use regia_detsim::golden::scenarios::INPUT_LOG_ARENA_LOG;
use regia_detsim::replay::{DesyncReplayRegistry, InputLog, ReplayError, ReplayHook};
use regia_detsim::state::{DetReadLimits, DetState};

struct Args {
    log: Option<String>,
    snapshot: Option<String>,
    until: Option<u64>,
    flip: Option<u64>,
}

fn parse() -> Result<Args, String> {
    let mut args = Args {
        log: None,
        snapshot: None,
        until: None,
        flip: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let tick = |v: Option<String>, what: &str| -> Result<u64, String> {
            v.ok_or_else(|| format!("{what} needs a tick"))?
                .parse::<u64>()
                .map_err(|e| format!("{what}: {e}"))
        };
        match arg.as_str() {
            "--snapshot" => args.snapshot = Some(it.next().ok_or("--snapshot needs a file")?),
            "--until" => args.until = Some(tick(it.next(), "--until")?),
            "--flip" => args.flip = Some(tick(it.next(), "--flip")?),
            "-h" | "--help" => {
                return Err(
                    "usage: replay [<log>] [--snapshot <file>] [--until <tick>] [--flip <tick>]".into(),
                );
            }
            other if args.log.is_none() && !other.starts_with("--") => args.log = Some(other.to_string()),
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    Ok(args)
}

/// Toggle seat 0's `hold` at `tick` of an AI arena log.
fn flip(bytes: &[u8], tick: u64, limits: DetReadLimits) -> Result<Vec<u8>, ReplayError> {
    let mut log = InputLog::<ArenaOrder>::decode_with(bytes, limits)?;
    let (start, end) = (log.start_tick(), log.end_tick());
    let entry = log.entry_mut(tick).ok_or(ReplayError::BadUntil {
        until: tick,
        start,
        end,
    })?;
    let mut order = entry.inputs.get(0).copied().unwrap_or_default();
    order.hold = !order.hold;
    if !entry.inputs.replace(0, Some(order)) {
        entry.inputs.push(0, Some(order))?;
    }
    Ok(log.encode()?)
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let read = |path: &str| std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"));
    let log = match args.log.as_deref().map(read).transpose() {
        Ok(log) => log.unwrap_or_else(|| INPUT_LOG_ARENA_LOG.to_vec()),
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let snapshot = match args.snapshot.as_deref().map(read).transpose() {
        Ok(snapshot) => snapshot,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let limits = DetReadLimits::DEFAULT;
    let mut hooks = DesyncReplayRegistry::new();
    if let Err(e) = hooks.register(Box::new(ReplayHook::<ArenaScenario, 3>::new())) {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    let log = match args.flip {
        Some(tick) => match regia_detsim::replay::read_input_log_header(&log, limits) {
            Ok(header) if header.sim_tag == ArenaScenario::STATE_TAG => match flip(&log, tick, limits) {
                Ok(bytes) => bytes,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(2);
                }
            },
            Ok(_) => {
                eprintln!("--flip works on AI arena logs only");
                return ExitCode::from(2);
            }
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        },
        None => log,
    };
    match hooks.replay(snapshot.as_deref(), &log, args.until, limits) {
        Ok(report) => {
            println!("{}", report.verdict_line());
            for d in &report.systems {
                println!("  system {} first differs at tick {}", d.system, d.first_tick);
            }
            if report.passed() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("replay error {}: {e}", e.code());
            ExitCode::from(2)
        }
    }
}
