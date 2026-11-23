// SPDX-License-Identifier: MPL-2.0
//! `regia net`: lockstep diagnosis (spec 120 § 7.6, ADR-8455).
//!
//! `regia net desync-replay <bundle>` reads a `*.desync.ron` bundle, replays it headless through
//! the `DesyncReplay` hook registered for its game and prints the first diverging tick, the peer
//! and the subsystem (and whether the live probe said the same). This binary registers the
//! engine's built-in hook (`regia.desync_sample`); a game replays its own bundles from its tool
//! binary with `regia_net::desync::ReplayRegistry` and the same calls (`book/user-guide/networking.md`).
//! Exit codes: 0 replayed (a divergence found or not), 1 the bundle was refused or the replay failed.
//!
//! `regia net replay-audit <file>` re-runs a `.lsreplay` lockstep match replay through the same
//! hooks from each Full record, compares every recorded hash bit for bit, and names the first
//! diverging tick and subsystem (ADR-8513). Exit codes: 0 bit-exact, 1 a hash differs or the file
//! was refused.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand};
use regia_net::desync::{BundleError, DesyncBundle, ReplayRegistry, ReplayVerdict};
use regia_net::lockstep::replay::{LsReplayError, ReplayAudit, audit_file};

/// `regia net` subcommands.
#[derive(Debug, Subcommand)]
pub enum NetCommand {
    /// Replay a `*.desync.ron` bundle headless and name the first diverging tick and subsystem
    /// (spec 120 § 7.6).
    DesyncReplay(DesyncReplayArgs),
    /// Re-run a `.lsreplay` lockstep match replay headless from each Full record and check every
    /// recorded hash bit for bit; name the first diverging tick and subsystem (ADR-8513).
    ReplayAudit(ReplayAuditArgs),
}

/// Arguments of `regia net replay-audit`.
#[derive(Debug, Args)]
pub struct ReplayAuditArgs {
    /// The replay (`*.lsreplay`).
    pub file: PathBuf,
    /// Print one JSON object (`ok`, `bit_exact`, the audit or `error: { code, message }`) on stdout.
    #[arg(long)]
    pub json: bool,
}

/// Arguments of `regia net desync-replay`.
#[derive(Debug, Args)]
pub struct DesyncReplayArgs {
    /// The bundle (`*.desync.ron`).
    pub bundle: PathBuf,
    /// Print one JSON object (`ok`, the verdict or `error: { code, message }`) on stdout.
    #[arg(long)]
    pub json: bool,
}

/// Reads and replays `args.bundle` through `registry`.
pub fn replay_file(args: &DesyncReplayArgs, registry: &ReplayRegistry) -> Result<ReplayVerdict, BundleError> {
    let bundle = DesyncBundle::read(&args.bundle)?;
    registry.replay(&bundle)
}

/// Runs a `regia net` command.
pub fn run(command: &NetCommand) -> ExitCode {
    match command {
        NetCommand::DesyncReplay(args) => desync_replay(args, &ReplayRegistry::with_builtin()),
        NetCommand::ReplayAudit(args) => replay_audit(args, &ReplayRegistry::with_builtin()),
    }
}

/// `regia net replay-audit` with `registry`'s hooks: exit 0 when every recorded hash matched.
pub fn replay_audit(args: &ReplayAuditArgs, registry: &ReplayRegistry) -> ExitCode {
    let result: Result<ReplayAudit, LsReplayError> = audit_file(&args.file, registry);
    match result {
        Ok(audit) => {
            let exact = audit.bit_exact();
            if args.json {
                let mut value = serde_json::to_value(&audit).unwrap_or(serde_json::Value::Null);
                if let serde_json::Value::Object(map) = &mut value {
                    map.insert("ok".into(), serde_json::Value::Bool(true));
                    map.insert("bit_exact".into(), serde_json::Value::Bool(exact));
                    map.insert("file".into(), args.file.display().to_string().into());
                }
                println!("{value}");
            } else {
                println!("replay: {}", args.file.display());
                for line in audit.summary() {
                    println!("{line}");
                }
            }
            if exact {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(err) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": false,
                        "error": { "code": err.code(), "message": err.to_string() },
                    })
                );
            } else {
                eprintln!("error[{}]: {err}", err.code());
            }
            ExitCode::FAILURE
        }
    }
}

/// `regia net desync-replay` with `registry`'s hooks.
pub fn desync_replay(args: &DesyncReplayArgs, registry: &ReplayRegistry) -> ExitCode {
    match replay_file(args, registry) {
        Ok(verdict) => {
            if args.json {
                let mut value = serde_json::to_value(&verdict).unwrap_or(serde_json::Value::Null);
                if let serde_json::Value::Object(map) = &mut value {
                    map.insert("ok".into(), serde_json::Value::Bool(true));
                    map.insert("bundle".into(), args.bundle.display().to_string().into());
                }
                println!("{value}");
            } else {
                println!("bundle: {}", args.bundle.display());
                for line in verdict.summary() {
                    println!("{line}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": false,
                        "error": { "code": err.code(), "message": err.to_string() },
                    })
                );
            } else {
                eprintln!("error[{}]: {err}", err.code());
            }
            ExitCode::FAILURE
        }
    }
}
