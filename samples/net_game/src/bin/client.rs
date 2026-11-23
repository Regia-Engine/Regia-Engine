// SPDX-License-Identifier: MIT-0
//! The client of Net Game: a window, an overhead camera, one box per avatar.
//!
//! ```text
//! net_game_client [--connect 127.0.0.1:7777 | --join CODE [--relay 127.0.0.1:7800]] [--key <server key>]
//!                 [--name NAME] [--bot [--bot-for N]] [--headless --ticks N [--clients N] [--host]]
//!                 [--conditioner SPEC] [--seed N] [--ticks N] [--exit-after-frames N]
//! ```
//!
//! `--join` looks a join code up on the relay (`--relay`, default `127.0.0.1:7800`, the port of a
//! server started with `--relay local`) and connects to the game port it names, with the server's
//! key pinned (ADR-2440). `regia server --clients N` starts clients with `REGIA_JOIN_CODE`,
//! `REGIA_RELAY`, `REGIA_CONNECT`, `REGIA_CLIENT_NAME` and `REGIA_CLIENT_OFFSCREEN=1` (a headless
//! bot that plays until the server stops); the arguments override them.
//!
//! WASD runs, Space fires at the nearest other player. `--key` pins the server's key (printed by
//! `net_game_server`); without it the client accepts any key, which stops a listener but not an
//! attacker in the middle. `--bot` plays the scripted bot instead of the keyboard (for `--bot-for`
//! ticks, then stands still). `--headless` runs without a window for `--ticks` fixed steps and prints
//! the avatars it saw; `--clients N` runs N headless clients in one process (a soak and load tool),
//! and `--host` adds the dedicated server to that process (play-in-editor with N clients).
//!
//! `--conditioner SPEC` shapes this client's outbound datagrams on the real socket (a profile such
//! as `wan`, or stated conditions such as `delay=40,jitter=10,loss=0.02`; ADR-3075), `--seed`
//! replaces the spec's seed. Dev builds also read `REGIA_NET_CONDITIONER` and
//! `REGIA_NET_CONDITIONER_SEED`; the arguments override them. With a window, `--ticks N` closes
//! the game after N fixed steps, and `--exit-after-frames N` after N rendered frames (the smoke run every
//! template's game exe takes; `--headless` ignores it). Both modes print one `netstats` line per client at the end (round
//! trip, loss, bytes each way, resends, prediction corrections; `cargo xtask net-match` reads it).
//!
//! Steam builds (sample only, ADR-7317): with `--features steam` the windowed client also runs
//! `regia::steam::SteamPlugin` and a HUD line with the overlay state (`net_game::steam_overlay`);
//! `--features steamworks` talks to the running Steam client, `--features steam` alone runs on
//! the mock backend. `--list-adapters` prints the adapters numbered for `REGIA_GPU_INDEX` and exits.

// A shipped Windows game opens no console window (the GUI subsystem); debug builds keep the console
// for logs (ADR-2306, `regia package` checks the shipped exe).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::net::SocketAddr;
use std::num::NonZeroU8;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use net_game::hud::NetGameHud;
use net_game::view::NetGameView;
use net_game::{
    Bot, DEFAULT_PORT, NetGamePlugin, Pad, PadSource, Side, TICK_HZ, avatars, connect_udp, headless_app,
    link_options, net_stats_line,
};
use regia::net::link::{LinkClock, LinkOptions, NetLink};
use regia::net::predict::ClientPrediction;
use regia::net::server_host::resolve_join_code;
use regia::net::{ConditionerConfig, ServerKey};
use regia::prelude::*;

struct Args {
    server: SocketAddr,
    join: Option<String>,
    relay: SocketAddr,
    key: Option<ServerKey>,
    name: String,
    bot: bool,
    bot_for: u32,
    headless: bool,
    host: bool,
    clients: u32,
    ticks: u64,
    /// `--ticks` was given (a windowed client then closes after that many fixed steps).
    ticks_given: bool,
    /// `--exit-after-frames N`: a windowed client closes after N frames (the templates' smoke flag).
    exit_after_frames: Option<u64>,
    conditioner: ConditionerConfig,
}

fn parse_with(
    args: &[String],
    env_conditioner: Result<Option<ConditionerConfig>, String>,
) -> Result<Args, String> {
    let mut out = Args {
        server: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
        join: None,
        relay: SocketAddr::from(([127, 0, 0, 1], regia::net::relay::RELAY_DEFAULT_PORT)),
        key: None,
        name: "player".into(),
        bot: false,
        bot_for: u32::MAX,
        headless: false,
        host: false,
        clients: 1,
        ticks: 600,
        ticks_given: false,
        exit_after_frames: None,
        conditioner: ConditionerConfig::default(),
    };
    // What `regia server --clients N` passes; the arguments below override it.
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    if let Some(addr) = env("REGIA_CONNECT") {
        out.server = addr.parse().map_err(|e| format!("REGIA_CONNECT: {e}"))?;
    }
    out.join = env("REGIA_JOIN_CODE");
    if let Some(addr) = env("REGIA_RELAY") {
        out.relay = addr.parse().map_err(|e| format!("REGIA_RELAY: {e}"))?;
    }
    if let Some(name) = env("REGIA_CLIENT_NAME") {
        out.name = name;
    }
    if env("REGIA_CLIENT_OFFSCREEN").as_deref() == Some("1") {
        out.headless = true;
        out.bot = true;
        out.ticks = u64::MAX;
    }
    let mut seed: Option<u64> = None;
    let mut conditioner_given = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--connect" => {
                out.server = value()?.parse().map_err(|e| format!("--connect: {e}"))?;
                out.join = None;
            }
            "--join" => out.join = Some(value()?.clone()),
            "--relay" => out.relay = value()?.parse().map_err(|e| format!("--relay: {e}"))?,
            "--key" => {
                let text = value()?;
                out.key = Some(ServerKey::parse(text).ok_or_else(|| format!("--key: not a key: {text}"))?);
            }
            "--name" => out.name = value()?.clone(),
            "--bot" => out.bot = true,
            "--bot-for" => out.bot_for = value()?.parse().map_err(|e| format!("--bot-for: {e}"))?,
            "--headless" => out.headless = true,
            "--host" => out.host = true,
            "--clients" => {
                out.clients = value()?.parse().map_err(|e| format!("--clients: {e}"))?;
                if !(1..=64).contains(&out.clients) {
                    return Err("--clients: 1 to 64".into());
                }
            }
            "--ticks" => {
                out.ticks = value()?.parse().map_err(|e| format!("--ticks: {e}"))?;
                out.ticks_given = true;
            }
            "--conditioner" => {
                out.conditioner =
                    ConditionerConfig::parse_spec(value()?).map_err(|e| format!("--conditioner: {e}"))?;
                conditioner_given = true;
            }
            "--seed" => seed = Some(value()?.parse().map_err(|e| format!("--seed: {e}"))?),
            "--exit-after-frames" => {
                out.exit_after_frames = Some(
                    value()?
                        .parse()
                        .map_err(|e| format!("--exit-after-frames: {e}"))?,
                );
            }
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

fn options(args: &Args) -> LinkOptions {
    LinkOptions {
        server_key: args.key,
        name: args.name.clone(),
        ..link_options(LinkClock::Wall, args.conditioner.clone())
    }
}

/// `--headless`: windowless clients for soak runs and scripts. `--clients N` runs N of them in this
/// process, each with its own socket and link; with `--bot` they circle in evenly spread phases.
/// `--host` also runs the dedicated server in this process (play-in-editor style, over real UDP on
/// 127.0.0.1) and prints its avatars at the end.
fn run_headless(args: &Args) -> ExitCode {
    let mut server_app = None;
    let mut server = args.server;
    let mut key = args.key;
    if args.host {
        let mut app = headless_app(Side::Server);
        let opts = link_options(LinkClock::Wall, ConditionerConfig::default());
        if let Err(e) = net_game::host_udp(&mut app, SocketAddr::from(([127, 0, 0, 1], 0)), &opts) {
            eprintln!("net_game_client: {e}");
            return ExitCode::from(3);
        }
        let link = app.world().resource::<NetLink>();
        server = link.local_addr().unwrap_or(server);
        key = link.server_key();
        say(format_args!("hosting {server}"));
        server_app = Some(app);
    }
    let mut apps = Vec::new();
    for i in 0..args.clients {
        let mut app = headless_app(Side::Client);
        if args.bot {
            let phase = std::f32::consts::TAU * i as f32 / args.clients as f32;
            app.insert_resource(PadSource::Bot(Bot::new(phase, 30, args.bot_for)));
        }
        let mut opts = options(args);
        opts.server_key = key;
        opts.conditioner.seed = opts.conditioner.seed.wrapping_add(u64::from(i));
        if args.clients > 1 {
            opts.name = format!("{}{i}", args.name);
            opts.label = format!("client{i}");
        }
        if let Err(e) = connect_udp(&mut app, server, &opts) {
            eprintln!("net_game_client: {e}");
            return ExitCode::from(3);
        }
        apps.push(app);
    }
    let step = Duration::from_secs_f64(1.0 / TICK_HZ);
    let mut next = Instant::now();
    for _ in 0..args.ticks {
        if let Some(app) = server_app.as_mut() {
            app.update();
        }
        for app in &mut apps {
            app.update();
        }
        let now = Instant::now();
        next = pace(next + step, now);
        if next > now {
            std::thread::sleep(next - now);
        }
    }
    for (i, app) in apps.iter_mut().enumerate() {
        let link = app.world().resource::<NetLink>();
        say(format_args!(
            "client {i} connected={} sealed={}",
            link.is_connected(),
            link.is_sealed()
        ));
        let stats = &app.world().resource::<ClientPrediction>().stats;
        say(format_args!(
            "prediction client={i} corrections={} resyncs={} verify_failures={}",
            stats.corrections, stats.resyncs, stats.verify_failures
        ));
        for a in avatars(app.world_mut()) {
            say(format_args!(
                "seen client={i} slot={} x={:.3} z={:.3} tags={} tagged={}",
                a.slot, a.position.x, a.position.z, a.score.tags, a.score.tagged
            ));
        }
        say(format_args!(
            "{}",
            net_stats_line(app.world(), &format!("client{i}"))
        ));
    }
    if let Some(app) = server_app.as_mut() {
        for a in avatars(app.world_mut()) {
            say(format_args!(
                "summary slot={} x={:.3} z={:.3} tags={} tagged={}",
                a.slot, a.position.x, a.position.z, a.score.tags, a.score.tagged
            ));
        }
    }
    ExitCode::SUCCESS
}

/// `--ticks N` with a window: close after N fixed steps.
#[derive(Resource)]
struct QuitAfter {
    ticks: u64,
    done: u64,
}

/// Counts fixed steps; at the last one prints what this client saw and its `netstats` line, and
/// closes the game.
fn quit_after_ticks(world: &mut World) {
    let Some(mut quit) = world.get_resource_mut::<QuitAfter>() else {
        return;
    };
    quit.done += 1;
    if !quit_on_step(quit.done, quit.ticks) {
        return;
    }
    let connected = world.resource::<NetLink>().is_connected();
    say(format_args!("client 0 connected={connected}"));
    for a in avatars(world) {
        say(format_args!(
            "seen client=0 slot={} x={:.3} z={:.3} tags={} tagged={}",
            a.slot, a.position.x, a.position.z, a.score.tags, a.score.tagged
        ));
    }
    say(format_args!("{}", net_stats_line(world, "client0")));
    world.write_message(AppExit::Success);
}

/// Whether fixed step number `done` (1 for the first) is the one `--ticks` closes on. `--ticks 0`
/// closes on the first step: a windowed game cannot close before it steps, and a check for step 0
/// never fired, so the game ran on.
fn quit_on_step(done: u64, ticks: u64) -> bool {
    done == ticks.max(1)
}

/// The next tick's deadline: `next`, or `now` once the loop fell behind it. A stalled step drops
/// its backlog instead of running the missed ticks back to back (the server's loop does the same),
/// so the clients never burst commands at a server that keeps real time.
fn pace(next: Instant, now: Instant) -> Instant {
    next.max(now)
}

/// The windowed client's exit status: 3 when it could not start connecting (no local UDP port,
/// as `--headless` reports it), else the game's own exit.
fn exit_status(connect_failed: bool, exit: &AppExit) -> u8 {
    match exit {
        _ if connect_failed => 3,
        AppExit::Success => 0,
        AppExit::Error(code) => code.get(),
    }
}

/// Prints one line to stdout; a closed pipe is ignored instead of panicking like `println!`.
fn say(line: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_fmt(line);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// What a windowed client says when cargo built it with `regia`'s `server` feature (found by
/// `cargo xtask net-match`: a client built in one invocation with the server ran as a server and
/// refused its own arguments).
const SERVER_BUILD_MESSAGE: &str = "net_game_client: this client was built with regia's `server` feature \
     (cargo unifies features across one build: a client built in the same cargo invocation as the server, or \
     with `--features server`, is a server build); build the client in its own cargo invocation \
     (`--headless` still works)";

fn main() -> ExitCode {
    let mut raw: Vec<String> = Vec::new();
    for arg in std::env::args_os().skip(1) {
        match arg.into_string() {
            Ok(text) => raw.push(text),
            Err(_) => {
                eprintln!("net_game_client: an argument is not valid UTF-8");
                return ExitCode::from(2);
            }
        }
    }
    // Steam builds (sample only, ADR-7317): `--list-adapters` prints the adapters and exits.
    #[cfg(feature = "steam")]
    if let Some(code) = net_game::steam_overlay::list_adapters_flag(&mut raw) {
        return code;
    }
    let mut args = match parse(&raw) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("net_game_client: {e}");
            return ExitCode::from(2);
        }
    };
    if let Some(code) = args.join.clone() {
        match resolve_join_code(args.relay, &code) {
            Ok(ticket) => {
                say(format_args!("joined code {code}: {}", ticket.addr));
                args.server = ticket.addr;
                args.key = args.key.or(ticket.server_key);
            }
            Err(e) => {
                eprintln!("net_game_client: join code {code}: {}: {e}", e.code());
                return ExitCode::from(3);
            }
        }
    }
    say(format_args!("conditioner {}", args.conditioner.describe()));
    if args.headless {
        return run_headless(&args);
    }
    if regia::SERVER_BUILD {
        eprintln!("{SERVER_BUILD_MESSAGE}");
        return ExitCode::from(2);
    }
    let opts = options(&args);
    let server = args.server;
    let bot = args.bot.then_some(args.bot_for);
    let quit_after = args.ticks_given.then_some(args.ticks);
    let exit_after_frames = args.exit_after_frames;
    let connect_failed = Arc::new(AtomicBool::new(false));
    let failed = Arc::clone(&connect_failed);
    let exit = regia::run_with(
        (
            NetGamePlugin { side: Side::Client },
            NetGameView { input: true },
            // The lobby list's "start match" sends everyone to this client's server; without it
            // the list's launch had no server and answered `net_not_running` (rc1-rev-net-k 14).
            NetGameHud {
                lobby_server: Some(server),
            },
        ),
        move |app| {
            // Steam builds (sample only, ADR-7317): SteamPlugin and the HUD's overlay line.
            #[cfg(feature = "steam")]
            app.add_plugins(net_game::steam_overlay::NetGameSteam::for_this_build());
            if let Some(bot_for) = bot {
                app.insert_resource(PadSource::Bot(Bot::new(0.0, 30, bot_for)));
            } else {
                app.insert_resource(PadSource::Manual(Pad::default()));
            }
            if let Err(e) = connect_udp(app, server, &opts) {
                error!(target: "net_game", "{e}");
                // Close at once: a client that cannot connect has nothing to show.
                failed.store(true, Ordering::Relaxed);
                app.add_systems(Startup, |world: &mut World| {
                    world.write_message(AppExit::Error(NonZeroU8::MIN.saturating_add(2)));
                });
            }
            if let Some(ticks) = quit_after {
                app.insert_resource(QuitAfter { ticks, done: 0 });
                app.add_systems(FixedUpdate, quit_after_ticks);
            }
            if let Some(limit) = exit_after_frames {
                app.add_systems(
                    Last,
                    move |mut seen: Local<u64>, mut exit: MessageWriter<AppExit>| {
                        *seen += 1;
                        if *seen >= limit {
                            exit.write(AppExit::Success);
                        }
                    },
                );
            }
        },
    );
    ExitCode::from(exit_status(connect_failed.load(Ordering::Relaxed), &exit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pace_drops_backlog_after_a_stall() {
        let step = Duration::from_secs_f64(1.0 / TICK_HZ);
        let start = Instant::now();
        // On time: the deadline stands.
        assert_eq!(pace(start + step, start), start + step);
        // One update took 500 ms: the next deadline is now, not 29 ticks in the past.
        let late = start + Duration::from_millis(500);
        assert_eq!(pace(start + step, late), late);
    }

    #[test]
    fn exit_after_frames_is_the_smoke_flag() {
        let args = |list: &[&str]| parse(&list.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        assert_eq!(args(&[]).map(|a| a.exit_after_frames), Ok(None));
        assert_eq!(
            args(&["--exit-after-frames", "5"]).map(|a| a.exit_after_frames),
            Ok(Some(5))
        );
        assert!(args(&["--exit-after-frames", "five"]).is_err());
        assert!(args(&["--exit-after-frames"]).is_err());
    }

    #[test]
    fn ticks_zero_closes_on_the_first_step_and_each_count_closes_once() {
        assert!(quit_on_step(1, 0), "--ticks 0");
        assert!(!quit_on_step(2, 0), "only once");
        assert!(!quit_on_step(1, 3));
        assert!(quit_on_step(3, 3));
        assert!(!quit_on_step(4, 3), "only once");
    }

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

    #[test]
    fn failed_connect_exits_3() {
        assert_eq!(exit_status(true, &AppExit::Success), 3);
        assert_eq!(exit_status(false, &AppExit::Success), 0);
        let code = NonZeroU8::MIN.saturating_add(6);
        assert_eq!(exit_status(false, &AppExit::Error(code)), 7);
        assert_eq!(exit_status(true, &AppExit::Error(code)), 3);
    }
}
