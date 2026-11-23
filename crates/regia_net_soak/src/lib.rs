// SPDX-License-Identifier: MPL-2.0
//! The network soak (ADR-7899, `book/agents/net-soak.md`): `regia_net`'s real server and clients
//! on an impaired in-process network, under named bad-network profiles, with bars that fail the
//! run when a player would lag out, be dropped or wedge.
//!
//! - [`impair`]: the impairment network at the `DatagramSocket` seam (latency, jitter, random and
//!   Gilbert-Elliott loss, reorder, duplication, bandwidth cap with a drop-tail queue, MTU, outages,
//!   NAT rebinding), deterministic from a seed.
//! - [`profiles`]: `clean`, `good-wifi`, `bad-wifi`, `mobile-4g`, `transatlantic`, `congested`,
//!   `flappy`, `hostile`, and spec 120 § 15.1's twenty (ADR-8030): `high-rtt`, `rtt-step`,
//!   `clock-drift`, `nat-rebind`, `cgnat`, `wifi-to-cell`, `long-outage`, `bufferbloat`,
//!   `downstream-loss`, `asym-bufferbloat`, `listen-uplink`, `lossy-fat-pipe`, `rtt-extreme`,
//!   `mtu-1280`, `mtu-blackhole`, `flood`, `forged-handshake`, `relay-path` (skipped until relay v2
//!   exists), `lockstep-mix` (a lockstep match), `mixed`; plus `congested+bulk`,
//!   `lockstep-mix+action` and `lockstep-mix+bots`.
//! - [`game`] / [`workload`]: the sample game and the server + N clients that play it.
//! - [`lockstep`]: the lockstep soak (ADR-8146): a lockstep host and seats on the same network, a
//!   deterministic sample simulation, and the `lockstep_bot` processes for a real-socket match.
//!   A plan runs the `lockstep-mix` profiles through it ([`lockstep_scenario`], ADR-8209).
//! - [`report`]: measurements, bars, `result.json` and `summary.md`.
//! - [`timed`]: the time harness (ADR-8185): ECS worlds with client prediction over the real
//!   transport in virtual microseconds, with per-client clock rates, sub-tick delivery and RTT
//!   steps and ramps; reports corrections, resyncs, input lead error and missing inputs.
//! - [`vectors`]: per-system state vectors (one word per system and tick) stepped through the lockstep
//!   driver in memory, with the self-check and resync answered from snapshots (ADR-8307).
//!
//! `cargo xtask net-soak` runs a plan through the `net_soak` test of `regia_test_utils`
//! (`crates/regia_test_utils/tests/net_soak.rs`), which calls [`run_plan`] with that crate's counting
//! allocator as the heap probe and writes the report; xtask turns a failed bar into a non-zero exit.
//! This crate is dev-only (`cargo xtask check-layers`, test-only layer): no shipped crate may depend
//! on it.

pub mod game;
pub mod impair;
pub mod lockstep;
pub mod probe;
pub mod profiles;
pub mod report;
#[cfg(feature = "ecs")]
pub mod timed;
pub mod vectors;
pub mod workload;

/// Whether the lockstep fault hook (`LockSim::perturb`, a scenario's planted drift, the bot's
/// `--inject-desync-at`) is in this build: exactly when the `desync-inject` feature is on. The
/// feature is the only key (ADR-8209): it is off by default, nothing else enables it, and no
/// profile setting adds or removes the hook.
pub const DESYNC_INJECT: bool = cfg!(feature = "desync-inject");

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use lockstep::{InputMix, LockstepScenario, SoakHost};
use profiles::{LOCKSTEP_MIX, NEW_PROFILE_NAMES, PROFILE_NAMES, Profile};
use report::ScenarioResult;
use workload::{Scenario, ServerMode, WireMode};

/// The default seed.
pub const DEFAULT_SEED: u64 = 0x5EED_0001;
/// Virtual seconds of each gate run.
pub const GATE_SECONDS: u64 = 60;
/// Virtual seconds of each nightly run.
pub const NIGHTLY_SECONDS: u64 = 300;
/// Most worker threads a plan uses (the shared build host rule).
pub const MAX_JOBS: usize = 16;

/// Profile variants the gate and nightly plans add to [`PROFILE_NAMES`]: `congested+bulk` (the
/// `rpc_p95_with_bulk` bar's run), `lockstep-mix+action` (the lockstep match on the action
/// input mix, ADR-8209) and `lockstep-mix+bots` (the wander-fire bot mix, ADR-8409).
pub const VARIANT_NAMES: [&str; 3] = ["congested+bulk", "lockstep-mix+action", "lockstep-mix+bots"];
/// Client counts of `mixed` in the gate plan beyond 8 and 32 (spec 120 § 15.2 `tick_p99` at 40).
pub const MIXED_GATE_CLIENTS: usize = 40;
/// The lockstep profile on the action input mix: [`LOCKSTEP_MIX`] with
/// [`profiles::ACTION_SUFFIX`], as [`VARIANT_NAMES`] lists it.
pub const LOCKSTEP_ACTION: &str = "lockstep-mix+action";
/// The lockstep profile on the wander-fire bot mix (ADR-8409).
pub const LOCKSTEP_BOTS: &str = "lockstep-mix+bots";
/// Seat counts of `lockstep-mix+bots` in the gate plan: one row, the 40 seats the upload bar is
/// stated for on this mix (the same cap as the order-driven mix).
pub const LOCKSTEP_BOTS_GATE_SEATS: [usize; 1] = [40];
/// Virtual length of that gate row, milliseconds: 36,000 ticks at 60 Hz.
pub const LOCKSTEP_BOTS_GATE_MS: u64 = 600_000;
/// Client counts of the nightly plan.
pub const NIGHTLY_CLIENTS: [usize; 4] = [8, 32, 64, 128];
/// Seat counts of `lockstep-mix` in the gate plan: the 8 and 32 the other profiles run at, and the
/// 40 its `lockstep_upload` bar is stated for (spec 120 § 7.5).
pub const LOCKSTEP_GATE_SEATS: [usize; 3] = [8, 32, 40];
/// Seat counts of `lockstep-mix+action` in the gate plan: its upload is reported at 16, 24 and 40
/// seats (ADR-8209); `lockstep_upload` is judged up to the seat count it is stated for on that mix.
pub const LOCKSTEP_ACTION_GATE_SEATS: [usize; 3] = [16, 24, 40];
/// Most seats a gate or nightly plan gives a lockstep run. Every frames datagram carries every
/// seat's input, so the host's upload grows with the square of the seats; past about 64 seats on
/// a home uplink a game should replicate instead (spec 120 § 7.5).
pub const LOCKSTEP_MAX_SEATS: usize = 40;

/// The merge-gate plan: every replication profile at 8 clients on a dedicated server, the harder
/// profiles and spec 120's twenty at 32, `mixed` at 40, `congested+bulk` at 8, the lockstep match
/// at [`LOCKSTEP_GATE_SEATS`] seats, on the action mix at [`LOCKSTEP_ACTION_GATE_SEATS`], and one
/// wander-fire row at [`LOCKSTEP_BOTS_GATE_SEATS`] for [`LOCKSTEP_BOTS_GATE_MS`], plus two
/// listen-server runs; 60 virtual seconds each, or longer where a profile's schedule needs it
/// ([`Profile`] `min_duration_ms`, `min_clients`; the orders and action lockstep runs take the
/// five minutes their upload bar is stated over). Profiles that need a missing engine part give
/// skipped results.
pub fn gate_plan(seed: u64) -> Vec<Scenario> {
    let mut out = Vec::new();
    let d = GATE_SECONDS * 1000;
    // The lockstep profiles have seat counts of their own, below.
    let replication = |name: &&&str| !is_lockstep_name(name);
    for name in PROFILE_NAMES
        .iter()
        .chain(VARIANT_NAMES.iter())
        .filter(replication)
    {
        out.extend(scenario(name, 8, ServerMode::Dedicated, seed, d));
    }
    for name in ["bad-wifi", "mobile-4g", "flappy", "hostile"]
        .iter()
        .chain(NEW_PROFILE_NAMES.iter())
        .filter(replication)
    {
        out.extend(scenario(name, 32, ServerMode::Dedicated, seed, d));
    }
    out.extend(scenario(
        "mixed",
        MIXED_GATE_CLIENTS,
        ServerMode::Dedicated,
        seed,
        d,
    ));
    for (name, seats) in [
        (LOCKSTEP_MIX, LOCKSTEP_GATE_SEATS.as_slice()),
        (LOCKSTEP_ACTION, LOCKSTEP_ACTION_GATE_SEATS.as_slice()),
        (LOCKSTEP_BOTS, LOCKSTEP_BOTS_GATE_SEATS.as_slice()),
    ] {
        for n in seats {
            if let Some(mut row) = scenario(name, *n, ServerMode::Dedicated, seed, d) {
                if name == LOCKSTEP_BOTS {
                    row.duration_ms = LOCKSTEP_BOTS_GATE_MS;
                    row.profile.fit_to(row.duration_ms);
                }
                out.push(row);
            }
        }
    }
    for name in ["good-wifi", "congested"] {
        out.extend(scenario(name, 8, ServerMode::Listen, seed, d));
    }
    out
}

/// Virtual seconds of each run of the scale plan (DO27's real-process presets ran 3 minutes).
pub const SCALE_SECONDS: u64 = 180;
/// Client counts of the scale plan's dedicated runs (spec 120 § 1.1: gate-sized at 40, 64
/// promised, 128 recorded).
pub const SCALE_CLIENTS: [usize; 3] = [40, 64, 128];
/// Client counts of the scale plan's listen-server runs on `listen-uplink` (16 on the
/// minimum-spec laptop, 32 on a 20 Mbit/s uplink).
pub const SCALE_LISTEN_CLIENTS: [usize; 2] = [16, 32];

/// The scale evidence plan (N215, ADR-8454): `mixed` and `mixed+hostile` on a dedicated server at
/// each of [`SCALE_CLIENTS`] and `listen-uplink` on a listen server at each of
/// [`SCALE_LISTEN_CLIENTS`], [`SCALE_SECONDS`] each.
pub fn scale_plan(seed: u64) -> Vec<Scenario> {
    scale_runs(seed, &SCALE_CLIENTS, &SCALE_LISTEN_CLIENTS, SCALE_SECONDS * 1000)
}

/// The gate-sized scale plan: `mixed` and `mixed+hostile` at 40 and `listen-uplink` at 32 on a
/// listen server, [`GATE_SECONDS`] each.
pub fn scale_gate_plan(seed: u64) -> Vec<Scenario> {
    scale_runs(seed, &[MIXED_GATE_CLIENTS], &[32], GATE_SECONDS * 1000)
}

fn scale_runs(seed: u64, dedicated: &[usize], listen: &[usize], duration_ms: u64) -> Vec<Scenario> {
    let mut out = Vec::new();
    for &n in dedicated {
        for name in ["mixed", "mixed+hostile"] {
            out.extend(scenario(name, n, ServerMode::Dedicated, seed, duration_ms));
        }
    }
    for &n in listen {
        out.extend(scenario(
            "listen-uplink",
            n,
            ServerMode::Listen,
            seed,
            duration_ms,
        ));
    }
    out
}

fn is_lockstep_name(name: &str) -> bool {
    name == LOCKSTEP_MIX || name == LOCKSTEP_ACTION || name == LOCKSTEP_BOTS
}

/// The seat counts the nightly plan gives lockstep profile `name`: [`NIGHTLY_CLIENTS`] held to
/// [`LOCKSTEP_MAX_SEATS`], plus the gate's seat counts for it, so what the gate runs the nightly
/// runs too.
fn lockstep_nightly_seats(name: &str) -> Vec<usize> {
    let gate: &[usize] = if name == LOCKSTEP_ACTION {
        &LOCKSTEP_ACTION_GATE_SEATS
    } else if name == LOCKSTEP_BOTS {
        &LOCKSTEP_BOTS_GATE_SEATS
    } else {
        &LOCKSTEP_GATE_SEATS
    };
    let mut seats: Vec<usize> = NIGHTLY_CLIENTS
        .iter()
        .map(|n| (*n).min(LOCKSTEP_MAX_SEATS))
        .chain(gate.iter().copied())
        .collect();
    seats.sort_unstable();
    seats.dedup();
    seats
}

/// The nightly plan: every profile at 8, 32, 64 and 128 clients in both server modes, 300 virtual
/// seconds each. The lockstep profiles are not run at 64 or 128 seats: they get the seat counts up
/// to [`LOCKSTEP_MAX_SEATS`] (8, 32, 40, and 16 and 24 on the action mix). The wander-fire mix
/// at 40 seats runs for [`LOCKSTEP_BOTS_GATE_MS`]; its shorter nightly rows stay at 300 s.
pub fn nightly_plan(seed: u64) -> Vec<Scenario> {
    let modes = [ServerMode::Dedicated, ServerMode::Listen];
    let mut plan = Vec::new();
    for name in PROFILE_NAMES.iter().chain(VARIANT_NAMES.iter()) {
        let clients = if is_lockstep_name(name) {
            lockstep_nightly_seats(name)
        } else {
            NIGHTLY_CLIENTS.to_vec()
        };
        plan.extend(
            custom_plan(
                &[(*name).to_string()],
                &clients,
                &modes,
                seed,
                NIGHTLY_SECONDS * 1000,
            )
            .unwrap_or_default(),
        );
    }
    for s in &mut plan {
        stretch(s);
        if s.profile.name == LOCKSTEP_BOTS && LOCKSTEP_BOTS_GATE_SEATS.contains(&s.clients) {
            s.duration_ms = LOCKSTEP_BOTS_GATE_MS;
            s.profile.fit_to(s.duration_ms);
        }
    }
    plan
}

/// The lockstep match a planned run stands for, when its profile is a lockstep one
/// ([`Profile::is_lockstep`]; ADR-8209): the scenario's clients are the seats, a dedicated server
/// is an observer host that steps its own simulation and votes, a listen server plays seat 0
/// itself, the settings are the defaults, and the input mix is the profile's (`+action`: the
/// action mix, `+bots`: the wander-fire mix).
pub fn lockstep_scenario(s: &Scenario) -> Option<LockstepScenario> {
    let mix = match s.profile.lockstep_mix()? {
        "action" => InputMix::ACTION,
        "bots" => InputMix::WANDER_FIRE,
        _ => InputMix::ORDERS,
    };
    let mut out = LockstepScenario::new(s.profile.clone(), s.clients, s.seed, s.duration_ms);
    out.host = match s.mode {
        ServerMode::Dedicated => SoakHost::Observer,
        ServerMode::Listen => SoakHost::Listen,
    };
    out.mix = mix;
    Some(out)
}

/// Raises a planned run to its profile's minimum length and client count.
fn stretch(s: &mut Scenario) {
    s.duration_ms = s.duration_ms.max(s.profile.min_duration_ms);
    s.clients = s.clients.max(s.profile.min_clients);
    s.profile.fit_to(s.duration_ms);
}

fn scenario(name: &str, clients: usize, mode: ServerMode, seed: u64, duration_ms: u64) -> Option<Scenario> {
    let mut s = Scenario {
        profile: Profile::named(name)?,
        clients,
        mode,
        seed,
        duration_ms,
        wire: Default::default(),
    };
    stretch(&mut s);
    Some(s)
}

/// Every combination of the given profiles, client counts and modes. Fails on an unknown profile.
pub fn custom_plan(
    profiles: &[String],
    clients: &[usize],
    modes: &[ServerMode],
    seed: u64,
    duration_ms: u64,
) -> Result<Vec<Scenario>, String> {
    let mut out = Vec::new();
    for p in profiles {
        let profile = Profile::named(p).ok_or_else(|| {
            format!(
                "unknown net-soak profile {p:?}; known: {}",
                PROFILE_NAMES.join(", ")
            )
        })?;
        let mut profile = profile;
        profile.fit_to(duration_ms);
        for &n in clients {
            for &mode in modes {
                out.push(Scenario {
                    profile: profile.clone(),
                    clients: n,
                    mode,
                    seed,
                    duration_ms,
                    wire: Default::default(),
                });
            }
        }
    }
    Ok(out)
}

/// Runs every scenario on up to `jobs` threads (at most [`MAX_JOBS`]) and evaluates each. Results
/// keep the plan's order. Each run is single-threaded in virtual time, so its record does not
/// depend on `jobs`; only the wall-clock tick times do.
///
/// `heap_probe` runs one scenario and returns the most the calling thread's heap grew during it
/// (`regia_test_utils::alloc_counter::peak_bytes` in a binary with the counting allocator); pass
/// [`no_heap_probe`] where there is none.
pub fn run_plan<P>(plan: &[Scenario], jobs: usize, heap_probe: P) -> Result<Vec<ScenarioResult>, String>
where
    P: Fn(&mut dyn FnMut()) -> u64 + Sync,
{
    let jobs = jobs.clamp(1, MAX_JOBS).min(plan.len().max(1));
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Result<ScenarioResult, String>>>> =
        Mutex::new((0..plan.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(s) = plan.get(i) else { break };
                    if let Some(needs) = &s.profile.needs {
                        eprintln!("net-soak: {} skipped (needs: {needs})", s.label());
                        results.lock().unwrap_or_else(std::sync::PoisonError::into_inner)[i] =
                            Some(Ok(report::skipped_result(s, needs)));
                        continue;
                    }
                    let not_run = || std::io::Error::other("the heap probe did not run the scenario");
                    let evaluated = if let Some(lock) = lockstep_scenario(s) {
                        // A lockstep profile is a lockstep match, not a replication run.
                        let started = std::time::Instant::now();
                        let mut record = None;
                        let peak = heap_probe(&mut || record = Some(lockstep::run_lockstep(&lock)));
                        let wall_ms = started.elapsed().as_millis() as u64;
                        record
                            .unwrap_or_else(|| Err(not_run()))
                            .map(|r| report::evaluate_lockstep(s, &r, wall_ms, peak))
                    } else {
                        let mut record = None;
                        let peak = heap_probe(&mut || record = Some(workload::run(s)));
                        record
                            .unwrap_or_else(|| Err(not_run()))
                            .map(|r| report::evaluate(&r, peak))
                    };
                    let out = evaluated.map_err(|e| format!("{}: {e}", s.label()));
                    if let Ok(r) = &out {
                        eprintln!(
                            "net-soak: {} {} in {:.1} s",
                            r.label,
                            if r.passed { "pass" } else { "FAIL" },
                            r.wall_ms as f64 / 1000.0
                        );
                        if !r.passed {
                            capture_failed_run(s);
                        }
                    }
                    results.lock().unwrap_or_else(std::sync::PoisonError::into_inner)[i] = Some(out);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err("a run did not finish".into())))
        .collect()
}

/// The environment variable that names the output directory; without it [`run_from_env`] does
/// nothing.
pub const OUT_ENV: &str = "REGIA_NET_SOAK_OUT";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn env_list(name: &str) -> Option<Vec<String>> {
    env(name).map(|v| {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// The plan `cargo xtask net-soak` asked for through `REGIA_NET_SOAK_*` (`PLAN` = `gate`,
/// `nightly`, `scale`, `scale-gate` or `custom`; `PROFILES`, `CLIENTS`, `MODES`, `SECONDS`,
/// `SEED`; `WIRE` = `virtual` or `udp` for every run): its name and runs.
pub fn plan_from_env() -> Result<(String, Vec<Scenario>), String> {
    let seed = match env("REGIA_NET_SOAK_SEED") {
        Some(s) => s
            .parse()
            .map_err(|_| format!("REGIA_NET_SOAK_SEED: {s:?} is not a number"))?,
        None => DEFAULT_SEED,
    };
    let name = env("REGIA_NET_SOAK_PLAN").unwrap_or_else(|| "gate".into());
    let mut plan = match name.as_str() {
        "gate" => gate_plan(seed),
        "nightly" => nightly_plan(seed),
        "scale" => scale_plan(seed),
        "scale-gate" => scale_gate_plan(seed),
        _ => {
            let profiles = env_list("REGIA_NET_SOAK_PROFILES")
                .unwrap_or_else(|| PROFILE_NAMES.iter().map(|s| s.to_string()).collect());
            let clients = env_list("REGIA_NET_SOAK_CLIENTS")
                .unwrap_or_else(|| vec!["8".into()])
                .iter()
                .map(|s| {
                    s.parse()
                        .map_err(|_| format!("REGIA_NET_SOAK_CLIENTS: {s:?} is not a count"))
                })
                .collect::<Result<Vec<usize>, _>>()?;
            let modes = env_list("REGIA_NET_SOAK_MODES")
                .unwrap_or_else(|| vec!["dedicated".into()])
                .iter()
                .map(|s| ServerMode::parse(s).ok_or_else(|| format!("REGIA_NET_SOAK_MODES: {s:?}")))
                .collect::<Result<Vec<_>, _>>()?;
            let seconds = match env("REGIA_NET_SOAK_SECONDS") {
                Some(s) => s
                    .parse::<u64>()
                    .map_err(|_| format!("REGIA_NET_SOAK_SECONDS: {s:?} is not a number"))?,
                None => GATE_SECONDS,
            };
            custom_plan(&profiles, &clients, &modes, seed, seconds * 1000)?
        }
    };
    if let Some(w) = env("REGIA_NET_SOAK_WIRE") {
        let wire =
            WireMode::parse(&w).ok_or_else(|| format!("REGIA_NET_SOAK_WIRE: {w:?} is not virtual or udp"))?;
        for s in &mut plan {
            s.wire = wire;
        }
    }
    Ok((name, plan))
}

/// Runs the plan of [`plan_from_env`] on `REGIA_NET_SOAK_JOBS` threads and writes the report into
/// the [`OUT_ENV`] directory. `None` when [`OUT_ENV`] is unset (a plain test run).
pub fn run_from_env<P>(heap_probe: P) -> Option<Result<report::SoakReport, String>>
where
    P: Fn(&mut dyn FnMut()) -> u64 + Sync,
{
    let out = env(OUT_ENV)?;
    Some((|| {
        let (name, plan) = plan_from_env()?;
        let jobs = env("REGIA_NET_SOAK_JOBS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(8);
        let results = run_plan(&plan, jobs, heap_probe)?;
        let label = env("REGIA_NET_SOAK_LABEL").unwrap_or_default();
        let report = report::SoakReport::new(&name, &label, results);
        report
            .write(std::path::Path::new(&out))
            .map_err(|e| format!("writing the net-soak report to {out}: {e}"))?;
        Ok(report)
    })())
}

/// A heap probe that measures nothing (reports 0).
pub fn no_heap_probe(run: &mut dyn FnMut()) -> u64 {
    run();
    0
}

/// Where a failing run's client captures go (ADR-8221): this variable, else `captures/` in
/// [`OUT_ENV`]'s folder; with neither set, nothing is written.
pub const CAPTURE_DIR_ENV: &str = "REGIA_NET_SOAK_CAPTURE_DIR";
/// The capture the replay test reads (`replay_capture_file` in `tests/capture_replay.rs`).
pub const REPLAY_ENV: &str = "REGIA_NET_SOAK_REPLAY";
/// Capture bytes budgeted per virtual second of a failing run's rerun, per client (a bad-wifi
/// client writes about 30 KiB/s; bulk and congested runs more).
pub const CAPTURE_BYTES_PER_S: usize = 96 << 10;
/// Smallest and largest ring of one client's capture in a failing run's rerun.
pub const CAPTURE_RING_RANGE: (usize, usize) = (8 << 20, 128 << 20);

/// The ring of each client's capture for scenario `s`: [`CAPTURE_BYTES_PER_S`] per virtual second,
/// within [`CAPTURE_RING_RANGE`] (a client whose ring still evicts its first session says so when
/// the file is written, and cannot replay from the start).
pub fn capture_ring_bytes(s: &Scenario) -> usize {
    let secs = usize::try_from(s.duration_ms.div_ceil(1000)).unwrap_or(usize::MAX);
    secs.saturating_mul(CAPTURE_BYTES_PER_S)
        .clamp(CAPTURE_RING_RANGE.0, CAPTURE_RING_RANGE.1)
}

/// The folder failing runs' captures go to, from [`CAPTURE_DIR_ENV`] or [`OUT_ENV`].
pub fn capture_dir_from_env() -> Option<std::path::PathBuf> {
    env(CAPTURE_DIR_ENV)
        .map(std::path::PathBuf::from)
        .or_else(|| env(OUT_ENV).map(|o| std::path::Path::new(&o).join("captures")))
}

/// The command that replays the capture at `path`.
pub fn replay_command(path: &std::path::Path) -> String {
    format!(
        "{REPLAY_ENV}={} cargo test -p regia_net_soak --test capture_replay -- --ignored --exact replay_capture_file --nocapture",
        path.display()
    )
}

/// The file name part of a scenario label (`bad-wifi/8/dedicated` -> `bad-wifi-8-dedicated`).
fn label_dir(s: &Scenario) -> String {
    s.label()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Reruns scenario `s` with a client capture on every honest client (the run is deterministic
/// from its seed) and writes `dir/<label>-seed<seed>/client-<i>.netcapture` for each, printing
/// each path with its replay command. Returns the paths and the rerun's record.
///
/// # Errors
///
/// A message when a ring cannot be made, the run cannot start or a file cannot be written.
pub fn write_failure_captures(
    s: &Scenario,
    dir: &std::path::Path,
) -> Result<(Vec<std::path::PathBuf>, workload::RunRecord), String> {
    let taps = (0..s.clients)
        .map(|_| regia_net::capture::CaptureTap::new(capture_ring_bytes(s)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let record = workload::run_with_captures(s, &taps).map_err(|e| format!("{}: {e}", s.label()))?;
    let folder = dir.join(format!("{}-seed{}", label_dir(s), s.seed));
    std::fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    let mut paths = Vec::new();
    for (i, tap) in taps.iter().enumerate() {
        let path = folder.join(format!("client-{i}.netcapture"));
        let stats = tap.stats();
        match tap.save(&path, &workload::capture_label(s, i), true) {
            Ok(_) => {
                let cut = if stats.head_cut || stats.oversized > 0 {
                    " (the ring lost its first session: this client cannot replay from the start)"
                } else {
                    ""
                };
                eprintln!(
                    "net-soak: {} client {i} capture {}{cut}\n  replay: {}",
                    s.label(),
                    path.display(),
                    replay_command(&path)
                );
                paths.push(path);
            }
            Err(e) if e.code() == "capture_empty" => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Ok((paths, record))
}

/// `run_plan`'s hook for a failed run: writes its captures where [`capture_dir_from_env`] says.
fn capture_failed_run(s: &Scenario) {
    let Some(dir) = capture_dir_from_env() else {
        eprintln!(
            "net-soak: {} failed; set {CAPTURE_DIR_ENV} or {OUT_ENV} to write its client captures",
            s.label()
        );
        return;
    };
    if let Err(e) = write_failure_captures(s, &dir) {
        eprintln!("net-soak: {} client captures not written: {e}", s.label());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_have_the_documented_shape() {
        let gate = gate_plan(1);
        // 27 replication profiles and congested+bulk at 8; 4 + 19 at 32; mixed at 40; lockstep-mix
        // at 8, 32 and 40 seats; lockstep-mix+action at 16, 24 and 40; lockstep-mix+bots at 40;
        // 2 listen runs.
        assert_eq!(gate.len(), 28 + 23 + 1 + 3 + 3 + 1 + 2);
        assert!(gate.iter().all(|s| s.duration_ms >= 60_000));
        let long = |name: &str| {
            gate.iter()
                .filter(|s| s.profile.name == name)
                .all(|s| s.duration_ms == s.profile.min_duration_ms)
        };
        assert!(
            long("rtt-step") && long("long-outage"),
            "schedules that need longer runs get them"
        );
        assert!(
            gate.iter()
                .filter(|s| s.profile.name == "cgnat")
                .all(|s| s.clients >= 10)
        );
        let bulk = gate
            .iter()
            .find(|s| s.profile.name == "congested+bulk")
            .expect("bulk run");
        assert_eq!(bulk.profile.bulk, Some((30_000, profiles::BULK_BYTES)));
        // The lockstep runs: the orders mix up to the 40 seats its upload bar is stated for, the
        // action mix at the three seat counts its upload is reported at, each for the five minutes
        // the bar is stated over, and the wander-fire mix at 40 seats for ten minutes, on a
        // dedicated host.
        let seats = |plan: &[Scenario], name: &str| -> Vec<usize> {
            plan.iter()
                .filter(|s| s.profile.name == name && s.mode == ServerMode::Dedicated)
                .map(|s| s.clients)
                .collect()
        };
        let lockstep = |s: &&Scenario| s.profile.name.starts_with("lockstep-mix");
        assert_eq!(seats(&gate, "lockstep-mix"), [8, 32, 40]);
        assert_eq!(seats(&gate, "lockstep-mix+action"), [16, 24, 40]);
        assert_eq!(seats(&gate, "lockstep-mix+bots"), [40]);
        assert_eq!(gate.iter().filter(lockstep).count(), 7);
        assert!(
            gate.iter()
                .filter(|s| { s.profile.name == "lockstep-mix" || s.profile.name == "lockstep-mix+action" })
                .all(|s| s.duration_ms == 300_000 && s.mode == ServerMode::Dedicated),
            "the upload bar is stated over a five-minute run"
        );
        assert!(
            gate.iter()
                .filter(|s| s.profile.name == "lockstep-mix+bots")
                .all(|s| s.duration_ms == 600_000 && s.mode == ServerMode::Dedicated)
        );
        let nightly = nightly_plan(1);
        // 28 replication names at 4 client counts in 2 modes; lockstep-mix at 8, 32 and 40 seats,
        // its action variant at 8, 16, 24, 32 and 40, and the bots variant at 8, 32 and 40, in 2
        // modes: what the gate runs is in the nightly, and no lockstep run has more than 40 seats.
        assert_eq!(nightly.len(), 28 * 4 * 2 + 3 * 2 + 5 * 2 + 3 * 2);
        assert_eq!(seats(&nightly, "lockstep-mix"), [8, 32, 40]);
        assert_eq!(seats(&nightly, "lockstep-mix+action"), [8, 16, 24, 32, 40]);
        assert_eq!(seats(&nightly, "lockstep-mix+bots"), [8, 32, 40]);
        assert!(
            nightly
                .iter()
                .filter(|s| lockstep(s) && s.profile.name != "lockstep-mix+bots")
                .all(|s| s.clients <= 40 && s.duration_ms == 300_000)
        );
        assert!(
            nightly
                .iter()
                .filter(|s| s.profile.name == "lockstep-mix+bots")
                .all(|s| {
                    s.clients <= 40
                        && if s.clients == 40 {
                            s.duration_ms == 600_000
                        } else {
                            s.duration_ms == 300_000
                        }
                })
        );
        assert!(
            nightly.iter().any(|s| !lockstep(&s) && s.clients == 128),
            "the replication profiles still run at 128 clients"
        );
        assert!(custom_plan(&["nope".into()], &[8], &[ServerMode::Dedicated], 1, 1_000).is_err());
    }

    /// The lockstep names and seat counts hang together: the variant is the profile plus the
    /// suffix, both resolve to lockstep profiles, and no planned seat count passes the cap.
    #[test]
    fn the_lockstep_plan_constants_are_consistent() {
        assert_eq!(
            LOCKSTEP_ACTION,
            format!("{LOCKSTEP_MIX}{}", profiles::ACTION_SUFFIX)
        );
        assert_eq!(LOCKSTEP_BOTS, format!("{LOCKSTEP_MIX}{}", profiles::BOTS_SUFFIX));
        assert!(VARIANT_NAMES.contains(&LOCKSTEP_ACTION));
        assert!(VARIANT_NAMES.contains(&LOCKSTEP_BOTS));
        for name in [LOCKSTEP_MIX, LOCKSTEP_ACTION, LOCKSTEP_BOTS] {
            assert!(is_lockstep_name(name));
            assert!(Profile::named(name).is_some_and(|p| p.is_lockstep()), "{name}");
        }
        assert!(!is_lockstep_name("mixed") && !is_lockstep_name("congested+bulk"));
        // Every planned name resolves, and the plan's name test agrees with the profile's own.
        for name in PROFILE_NAMES.iter().chain(VARIANT_NAMES.iter()) {
            let profile = Profile::named(name).unwrap_or_else(|| panic!("{name} resolves"));
            assert_eq!(is_lockstep_name(name), profile.is_lockstep(), "{name}");
        }
        assert!(
            LOCKSTEP_GATE_SEATS
                .iter()
                .chain(&LOCKSTEP_ACTION_GATE_SEATS)
                .chain(&LOCKSTEP_BOTS_GATE_SEATS)
                .all(|n| *n <= LOCKSTEP_MAX_SEATS)
        );
        assert_eq!(lockstep_nightly_seats(LOCKSTEP_MIX), [8, 32, 40]);
        assert_eq!(lockstep_nightly_seats(LOCKSTEP_ACTION), [8, 16, 24, 32, 40]);
        assert_eq!(lockstep_nightly_seats(LOCKSTEP_BOTS), [8, 32, 40]);
        // The seat count the upload bar is stated for on each mix is one the gate runs.
        assert!(LOCKSTEP_GATE_SEATS.contains(&report::LOCKSTEP_UPLOAD_SEATS_ORDERS));
        assert!(LOCKSTEP_ACTION_GATE_SEATS.contains(&report::LOCKSTEP_UPLOAD_SEATS_ACTION));
    }

    /// NL-09: the gate plan carries one `lockstep-mix+bots` row, 40 seats for 600 virtual
    /// seconds, judged by the same `desync_zero` and `lockstep_upload` bars as the other
    /// lockstep rows.
    #[test]
    fn gate_plan_has_the_bots_row_at_40_seats_600_s() {
        let gate = gate_plan(DEFAULT_SEED);
        let rows: Vec<_> = gate
            .iter()
            .filter(|s| s.profile.name == "lockstep-mix+bots")
            .collect();
        assert_eq!(rows.len(), 1, "lockstep-mix+bots is not in the gate plan");
        assert_eq!(rows[0].clients, 40);
        assert_eq!(rows[0].duration_ms, 600_000);
        assert_eq!(rows[0].mode, ServerMode::Dedicated);
        assert_eq!(
            lockstep_scenario(rows[0]).map(|row| row.mix),
            Some(InputMix::WANDER_FIRE)
        );
        let bars = report::bars_for(&rows[0].profile);
        assert!(
            bars.contains(&"desync_zero") && bars.contains(&"lockstep_upload"),
            "{bars:?}"
        );
    }

    #[test]
    fn a_profile_that_needs_a_missing_part_is_skipped_with_its_reason() {
        let plan = custom_plan(
            &["relay-path".into(), "lockstep-mix".into()],
            &[4],
            &[ServerMode::Dedicated],
            1,
            8_000,
        )
        .expect("plan");
        let results = run_plan(&plan, 2, no_heap_probe).expect("run");
        assert_eq!(results[0].skipped.as_deref(), Some("needs: relay v2"));
        assert!(results[0].bars.iter().all(|b| b.skipped.is_some() && !b.gating));
        // The lockstep mode exists (ADR-8146) and the plan runs it (ADR-8209): the profile's two
        // bars are judged on the run, not skipped.
        assert_eq!(results[1].skipped, None);
        for id in ["desync_zero", "lockstep_upload"] {
            let b = results[1]
                .bars
                .iter()
                .find(|b| b.id == id)
                .unwrap_or_else(|| panic!("bar {id}"));
            assert!(b.skipped.is_none() && b.passed, "{id}: {}", b.detail);
        }
        assert!(results.iter().all(|r| r.passed));
    }
}
