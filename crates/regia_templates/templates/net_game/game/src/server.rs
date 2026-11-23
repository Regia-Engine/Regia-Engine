// SPDX-License-Identifier: MIT-0
//! The dedicated server under the engine's server host (ADR-2440).
//!
//! Built with `regia`'s `server` feature (the server crate of the template always is), the server
//! binary is `regia::run_with` with [`crate::NetGamePlugin`] on the server side: the host reads
//! `server.toml`, paces the ticks, runs the console, `/health`, bans, the level rotation and
//! graceful shutdown, and the [`HostedLink`] this module makes binds the game port with the game's
//! [`NetLink`](regia::net::link::NetLink). `--relay local` (or `--relay HOST:PORT`) mints a join
//! code that clients resolve with `--join`.
//!
//! Without the `server` feature (the client configuration of this crate, which its tests build)
//! the binary falls back to a plain paced loop.

use regia::net::ConditionerConfig;
use regia::net::link::LinkClock;
use regia::net::server_host::HostedLink;
use regia::prelude::*;

use crate::{NetGameWire, avatars, link_options, net_stats_line};

/// The link the host binds: this game's wire, encryption and key from `server.toml`, the wall
/// clock, the conditioner of [`hosted_conditioner`], and [`print_summary`] when the host stops.
/// TODO(template): add your label here.
pub fn hosted_link() -> HostedLink {
    HostedLink::new::<NetGameWire>(link_options(LinkClock::Wall, hosted_conditioner()))
        .with_on_stop(print_summary)
}

/// The server's outbound conditioner: in a dev build, `REGIA_NET_CONDITIONER` and
/// `REGIA_NET_CONDITIONER_SEED` (what play-in-editor and `cargo xtask net-match` pass; ADR-3075),
/// else off. A spec that does not parse is reported and the server runs unconditioned. Only
/// [`run_hosted`] prints `conditioner <conditions>` (what a match script checks): the plain-loop
/// server builds this link too and prints its own conditions.
pub fn hosted_conditioner() -> ConditionerConfig {
    match ConditionerConfig::from_env() {
        Ok(config) => config.unwrap_or_default(),
        Err(e) => {
            error!(target: "{{crate_name}}", "{e}; the server runs without a conditioner");
            eprintln!("{{crate_name}}_server: {e}; the server runs without a conditioner");
            ConditionerConfig::default()
        }
    }
}

/// Prints `summary slot=<n> x=<x> z=<z> tags=<n> tagged=<n>` per avatar and the server's
/// `netstats` line (the host's stop hook).
pub fn print_summary(world: &mut World) {
    for a in avatars(world) {
        say(&format!(
            "summary slot={} x={:.3} z={:.3} tags={} tagged={}",
            a.slot, a.position.x, a.position.z, a.score.tags, a.score.tagged
        ));
    }
    say(&net_stats_line(world, "server"));
}

/// One line to stdout, flushed; a closed pipe is ignored.
fn say(line: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Runs the server under the host when this crate is built with the `server` feature and returns
/// its exit code; `None` in the client configuration (the caller runs the paced loop instead).
/// A failed start ends the process with the host's exit code (2 configuration, 3 port, 4 content).
pub fn run_hosted() -> Option<std::process::ExitCode> {
    #[cfg(feature = "server")]
    {
        // Once, before the host binds the port (the plugin reads the same environment).
        say(&format!("conditioner {}", hosted_conditioner().describe()));
        let exit = regia::run_with(
            crate::NetGamePlugin {
                side: crate::Side::Server,
            },
            |app| {
                app.add_plugins((
                    regia::framework::net_bots::NetBotsPlugin,
                    regia::ai::net_bots::ServerBotAiPlugin,
                ));
            },
        );
        Some(match exit {
            AppExit::Success => std::process::ExitCode::SUCCESS,
            AppExit::Error(code) => std::process::ExitCode::from(code.get()),
        })
    }
    #[cfg(not(feature = "server"))]
    {
        None
    }
}
