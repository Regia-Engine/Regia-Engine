// SPDX-License-Identifier: MIT-0
//! The dedicated server of {{name}}: headless, no renderer, no audio.
//!
//! Built with the `server` feature (always, for the template's server crate) it runs under the
//! engine's server host (ADR-2440, `{{crate_name}}::server`): the arguments are the host's
//! (`--port N`, `--level PATH`, `--relay local`, `--console`, ...; spec 76 § 4), the settings
//! `server.toml` in the server directory (`REGIA_SERVER_DIR`, else the executable's directory), and
//! `regia server` runs it from the project's dev directory. It prints `listening <addr> key <key>`,
//! `join code <code> relay <addr>` with `--relay`, and the summary lines when it stops.
//!
//! Without the `server` feature (the client configuration of the sample, which its process-pair
//! test builds) it is a plain paced loop:
//!
//! ```text
//! {{crate_name}}_server [--bind 0.0.0.0:7777] [--ticks N] [--until-stdin-eof] [--identity server.key]
//!                 [--conditioner SPEC] [--seed N]
//! ```
//!
//! `--conditioner` takes a profile (`off`, `lan`, `home`, `wan`, `wan_stress`, `bad`, `burst`) or
//! stated conditions (`delay=40,jitter=10,loss=0.02`; ADR-3075); dev builds also read
//! `REGIA_NET_CONDITIONER` and `REGIA_NET_CONDITIONER_SEED`, which the arguments override.
//!
//! Prints `listening <addr> key <server key>` once bound (clients pin the key), a status line every
//! ten seconds, and `summary slot=<n> x=<x> z=<z> tags=<n> tagged=<n>` per avatar when `--ticks`
//! runs out or, with `--until-stdin-eof`, when its supervisor closes stdin, then one `netstats`
//! line (`{{crate_name}}::net_stats_line`). Exit codes: 0 done,
//! 2 bad arguments, 3 the port cannot be bound or the stdin watcher cannot start.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use {{crate_name}}::{DEFAULT_PORT, Side, TICK_HZ, avatars, headless_app, host_udp, link_options, net_stats_line};
use regia::net::link::{LinkClock, NetLink};
use regia::net::{ConditionerConfig, ServerIdentity};

struct Args {
    bind: SocketAddr,
    ticks: Option<u64>,
    until_stdin_eof: bool,
    identity: Option<std::path::PathBuf>,
    conditioner: ConditionerConfig,
}

fn parse_with(
    args: &[String],
    env_conditioner: Result<Option<ConditionerConfig>, String>,
) -> Result<Args, String> {
    let mut out = Args {
        bind: SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)),
        ticks: None,
        until_stdin_eof: false,
        identity: None,
        conditioner: ConditionerConfig::default(),
    };
    let mut seed: Option<u64> = None;
    let mut conditioner_given = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--bind" => out.bind = value()?.parse().map_err(|e| format!("--bind: {e}"))?,
            "--ticks" => out.ticks = Some(value()?.parse().map_err(|e| format!("--ticks: {e}"))?),
            "--until-stdin-eof" => out.until_stdin_eof = true,
            "--identity" => out.identity = Some(value()?.into()),
            "--conditioner" => {
                out.conditioner =
                    ConditionerConfig::parse_spec(value()?).map_err(|e| format!("--conditioner: {e}"))?;
                conditioner_given = true;
            }
            "--seed" => seed = Some(value()?.parse().map_err(|e| format!("--seed: {e}"))?),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    // The environment's conditioner is a default the arguments override: a malformed
    // `REGIA_NET_CONDITIONER` is an error only when `--conditioner` does not replace it.
    if !conditioner_given && let Some(config) = env_conditioner? {
        out.conditioner = config;
    }
    if let Some(seed) = seed {
        out.conditioner.seed = seed;
    }
    Ok(out)
}

fn parse(args: &[String]) -> Result<Args, String> {
    parse_with(args, ConditionerConfig::from_env())
}

/// Prints one line to stdout and flushes; a closed pipe is ignored (a supervisor may stop reading).
fn say(line: std::fmt::Arguments<'_>) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_fmt(line);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn main() -> ExitCode {
    // Under the engine's server host when built with the `server` feature.
    if let Some(code) = {{crate_name}}::server::run_hosted() {
        return code;
    }
    let mut raw: Vec<String> = Vec::new();
    for arg in std::env::args_os().skip(1) {
        match arg.into_string() {
            Ok(text) => raw.push(text),
            Err(_) => {
                eprintln!("{{crate_name}}_server: an argument is not valid UTF-8");
                return ExitCode::from(2);
            }
        }
    }
    let args = match parse(&raw) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{{crate_name}}_server: {e}");
            return ExitCode::from(2);
        }
    };
    let mut opts = link_options(LinkClock::Wall, args.conditioner.clone());
    if let Some(path) = &args.identity {
        match ServerIdentity::load_or_create(path) {
            Ok(identity) => opts.identity = Some(identity),
            Err(e) => {
                eprintln!("{{crate_name}}_server: identity {}: {e}", path.display());
                return ExitCode::from(2);
            }
        }
    }
    let mut app = headless_app(Side::Server);
    if let Err(e) = host_udp(&mut app, args.bind, &opts) {
        eprintln!("{{crate_name}}_server: {e}");
        return ExitCode::from(3);
    }
    let (addr, key) = {
        let link = app.world().resource::<NetLink>();
        (link.local_addr(), link.server_key())
    };
    let addr = addr.map_or_else(|| "?".to_owned(), |a| a.to_string());
    let key = key.map_or_else(|| "none".to_owned(), |k| k.to_string());
    say(format_args!("listening {addr} key {key}"));
    say(format_args!("conditioner {}", args.conditioner.describe()));

    // A supervisor stops the server by closing its stdin, so it can stop it right after the last
    // client ends, however long that took on a loaded machine.
    let stdin_closed = Arc::new(AtomicBool::new(false));
    if args.until_stdin_eof {
        let closed = Arc::clone(&stdin_closed);
        let watcher = std::thread::Builder::new()
            .name("stdin-watch".into())
            .spawn(move || {
                let mut sink = [0u8; 256];
                let mut stdin = std::io::stdin();
                while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
                closed.store(true, Ordering::Relaxed);
            });
        if let Err(e) = watcher {
            eprintln!("{{crate_name}}_server: cannot start the stdin watcher: {e}");
            return ExitCode::from(3);
        }
    }

    let step = Duration::from_secs_f64(1.0 / TICK_HZ);
    let mut next = Instant::now();
    let mut tick: u64 = 0;
    loop {
        // `--ticks 0` runs no step, like the headless client.
        if args.ticks == Some(0) {
            break;
        }
        app.update();
        tick += 1;
        if tick.is_multiple_of(10 * TICK_HZ as u64) {
            let peers = app
                .world()
                .resource::<regia::net::runtime::NetNode>()
                .peers()
                .len();
            say(format_args!("status tick={tick} peers={peers}"));
        }
        if args.ticks.is_some_and(|n| tick >= n) || stdin_closed.load(Ordering::Relaxed) {
            break;
        }
        // Frame pacing of a real-time server: one tick per 1/60 s.
        next += step;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    for a in avatars(app.world_mut()) {
        say(format_args!(
            "summary slot={} x={:.3} z={:.3} tags={} tagged={}",
            a.slot, a.position.x, a.position.z, a.score.tags, a.score.tagged
        ));
    }
    say(format_args!("{}", net_stats_line(app.world(), "server")));
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dash_conditioner_overrides_a_malformed_env_conditioner() {
        // corr-viewer-view-templates LOW 4: `ConditionerConfig::from_env()` ran before the
        // arguments, so a bad REGIA_NET_CONDITIONER exited 2 even when `--conditioner` would
        // have replaced it. The environment is a default the arguments override.
        let bad_env = Err("REGIA_NET_CONDITIONER: `nonsense` is not a profile".to_string());
        let args = |list: &[&str]| {
            parse_with(
                &list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
                bad_env.clone(),
            )
        };
        assert!(
            args(&["--conditioner", "off"]).is_ok(),
            "--conditioner replaces the environment, malformed or not"
        );
        assert!(
            args(&[]).is_err(),
            "without --conditioner the malformed environment is still an error"
        );
    }
}
