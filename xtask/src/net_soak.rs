// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask net-soak`: `regia_net`'s real server and clients under bad networks (ADR-7899,
//! `book/agents/net-soak.md`, `docs/06-testing-and-ci.md` § 29).
//!
//! The soak runs inside one `cargo test --workspace --test net_soak` process: `regia_test_utils`'s
//! `net_soak_matrix` drives `regia_net_soak` with the counting allocator (xtask may not depend on
//! either outside dev-dependencies, `check-layers`; `--workspace` resolves features like the gate's
//! test build, so the gate rebuilds nothing for it): an in-process impairment network at the `DatagramSocket` seam, named profiles
//! (`clean`, `good-wifi`, `bad-wifi`, `mobile-4g`, `transatlantic`, `congested`, `flappy`,
//! `hostile`), a headless server plus N clients of a sample game, and bars. The test writes
//! `result.json` (format `regia-net-soak` v1) and `summary.md` into `--out`
//! (`target/net-soak/<UTC time>/` by default); this command exits 1 when any bar failed, 2 when
//! the harness itself failed.
//!
//! Plans: `--plan gate` (the merge-gate set, 60 runs of 60 virtual seconds or longer where a
//! profile's schedule needs it; six of them are lockstep matches of 300 virtual seconds, ADR-8209),
//! `--plan nightly` (every profile at 8, 32, 64 and 128 clients in both server modes, 300 virtual
//! seconds; the lockstep profiles at up to 40 seats), `--plan scale` / `--plan scale-gate` (spec 120 § 1.1
//! "It scales", N215, ADR-8454: `mixed` and `mixed+hostile` at 40, 64 and 128 clients and
//! `listen-uplink` at 16 and 32, 180 s; the gate-sized set at 40 and 32, 60 s), or a custom set from `--profiles`, `--clients`, `--modes`
//! and `--seconds` (any of them selects it). Every run is deterministic from `--seed`; the summary
//! prints each failure's reproduction command. `--list` prints the profiles and the bars, `--bars`
//! the bars alone.
//!
//! **Bars and their owners** (ADR-8030): [`BARS`] lists every bar, the profiles it judges, the
//! spec 120 § 17 lane that owns it and whether it gates. The harness's six gate; the fifteen
//! spec 120 § 15.2 bars are reported on every run but do not gate until their owning lane flips
//! `gating` in this table, in the commit that proves the bar green on its profiles. The gating
//! list reaches the soak as `REGIA_NET_SOAK_GATING`, and the verdict of a v2 `result.json` is
//! recomputed from its bars with this table. Flipped so far: `lockstep_upload` (ADR-8209),
//! `interrupted_once` and `resume_same_peer` (ADR-10093).
//!
//! The gate row ([`GATE_ROW_ENABLED`], `cargo xtask preflight`) is on since the v0.7 freeze
//! (ADR-10265): both known blockers (branches `v051-net-input-mtu` and `v051-net-input-rejection`)
//! are merged and `cargo xtask net-soak --plan gate --release` read `61 runs, PASS (0 failed bars
//! are known blockers, 0 are new)`. `REGIA_NET_SOAK_GATE=0 cargo xtask preflight` skips it for one run.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use serde_json::Value;

/// The format `result.json` carries.
pub const RESULT_FORMAT: &str = "regia-net-soak";
/// Whether `cargo xtask preflight` (and so the merge gate) runs `net-soak --plan gate`. On since
/// the v0.7 freeze (ADR-10265); see the module docs.
pub const GATE_ROW_ENABLED: bool = true;
/// Environment override of [`GATE_ROW_ENABLED`] for one run: `1` on, `0` off.
pub const GATE_ROW_ENV: &str = "REGIA_NET_SOAK_GATE";
/// Most worker threads (the shared build host rule).
pub const MAX_JOBS: usize = 16;
/// The environment variable that tells the soak which bars gate.
pub const GATING_ENV: &str = "REGIA_NET_SOAK_GATING";

/// One bar of the soak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarRow {
    /// Bar id in `result.json`.
    pub id: &'static str,
    /// When it passes.
    pub rule: &'static str,
    /// The profiles it judges.
    pub profiles: &'static str,
    /// The lane that owns it (spec 120 § 17 id and name, or the harness).
    pub owner: &'static str,
    /// It decides the verdict. Flip to `true` only in the owning lane's commit that shows the bar
    /// green on every profile it judges; never by moving the bar.
    pub gating: bool,
}

const fn row(
    id: &'static str,
    rule: &'static str,
    profiles: &'static str,
    owner: &'static str,
    gating: bool,
) -> BarRow {
    BarRow {
        id,
        rule,
        profiles,
        owner,
        gating,
    }
}

/// Every bar, its owner and whether it gates (ADR-7899 for the first six, ADR-8030 and spec 120
/// § 15.2 for the rest).
pub const BARS: [BarRow; 21] = [
    row(
        "involuntary_disconnects",
        "no connection lost the workload did not end, except inside an outage beyond the timeout",
        "all",
        "net-soak-harness (ADR-7899)",
        true,
    ),
    row(
        "reconnect_within_grace",
        "every rejoin connected within 3 s of its link being up",
        "all",
        "net-soak-harness (ADR-7899)",
        true,
    ),
    row(
        "no_wedged_client",
        "every client joins; no 2 s with the link up and no input at the server, no snapshot, or every send failing",
        "all",
        "net-soak-harness (ADR-7899)",
        true,
    ),
    row(
        "hostile_dropped",
        "every hostile session dropped within 5 s of its first act",
        "hostile, flood",
        "net-soak-harness (ADR-7899)",
        true,
    ),
    row(
        "others_unaffected",
        "no honest client ends, wedges or gets 10 ticks staler while a hostile peer acts",
        "hostile, flood",
        "net-soak-harness (ADR-7899)",
        true,
    ),
    row(
        "bandwidth_within_budget",
        "offered downstream over the second half <= the 256 kbit/s cap",
        "congested",
        "net-soak-harness (ADR-7899)",
        true,
    ),
    row(
        "missed_input",
        "missed input ticks <= 0.5 % clean/good-wifi, 2 % bad-wifi/mobile-4g, 3 % asym-bufferbloat",
        "clean, good-wifi, bad-wifi, mobile-4g, asym-bufferbloat",
        "N111 net-input-feedback",
        false,
    ),
    row(
        "corrections",
        "corrections <= 1 % of compared predictions on good-wifi, 3 % on bad-wifi",
        "good-wifi, bad-wifi",
        "N207 net-clock-filters",
        false,
    ),
    row(
        "staleness_p99",
        "snapshot staleness p99 with the link up <= 4 ticks (8 for congested clients)",
        "all",
        "N202 net-v2-rate-controller",
        false,
    ),
    row(
        "rate_vs_bottleneck",
        "every whole second from 5 s after each connect offers <= each finite bottleneck",
        "congested, bufferbloat, listen-uplink",
        "N202 net-v2-rate-controller",
        false,
    ),
    row(
        "queue_delay_p95",
        "game datagrams wait <= 100 ms p95 at the bottleneck queues",
        "bufferbloat",
        "N202 net-v2-rate-controller",
        false,
    ),
    row(
        "rpc_p95_with_bulk",
        "command round trip p95 during a 256 KiB bulk transfer <= 1.5x the same run's before it",
        "congested+bulk",
        "N203 net-v2-streams",
        false,
    ),
    row(
        "random_loss_kept",
        "snapshot budget after 5 s >= 90 % of a clean run's",
        "lossy-fat-pipe",
        "N202 net-v2-rate-controller",
        false,
    ),
    row(
        "interrupted_once",
        "one Interrupted and one Restored per outage of >= 1.5 s, none elsewhere",
        "flappy, long-outage, good-wifi",
        "N101 net-timeout-policy",
        true,
    ),
    row(
        "resume_same_peer",
        "every involuntary rejoin keeps its PeerId and entities",
        "long-outage, wifi-to-cell",
        "N216 net-link-resume-dormant",
        true,
    ),
    row(
        "no_hijack",
        "0 moves of a peer to an address its client never held",
        "hostile, nat-rebind, cgnat",
        "N103 net-rebind-path-validation",
        false,
    ),
    row(
        "never_kicked_for_ping",
        "every rtt-extreme peer joins and stays the whole run",
        "rtt-extreme",
        "N101 net-timeout-policy",
        false,
    ),
    row(
        "desync_zero",
        "0 desyncs, 0 resyncs; an injected one named and resynced",
        "lockstep-mix, lockstep-mix+action, lockstep-mix+bots",
        "N226 net-desync-detect",
        false,
    ),
    row(
        "lockstep_upload",
        "host upload over a 5-minute run: 95th-percentile second <= 4.0 Mbit/s and busiest second <= 4.4 Mbit/s, at up to 40 seats on the orders mix, 24 on the action mix (the bots mix uses the orders seat cap)",
        "lockstep-mix, lockstep-mix+action, lockstep-mix+bots",
        "N224 net-lockstep-core",
        true,
    ),
    row(
        "tick_p99",
        "server tick p99 <= its tier budget: 8,333 us up to 40 clients, 11,111 up to 64, 16,666 above (docs/perf net_scale_tick_p99_*, ADR-8454)",
        "mixed",
        "N215 net-scale-soak",
        false,
    ),
    row(
        "join_under_flood",
        ">= 99 % of joins connected within 5 s",
        "forged-handshake",
        "N218 net-handshake-availability",
        false,
    ),
];

/// Every profile and the engine part it still needs (empty: runs today), in report order
/// (`regia_net_soak::profiles::PROFILE_NAMES`, then the variants).
pub const PROFILES: [(&str, &str); 31] = [
    ("clean", ""),
    ("good-wifi", ""),
    ("bad-wifi", ""),
    ("mobile-4g", ""),
    ("transatlantic", ""),
    ("congested", ""),
    ("flappy", ""),
    ("hostile", ""),
    ("high-rtt", ""),
    ("rtt-step", ""),
    ("clock-drift", ""),
    ("nat-rebind", ""),
    ("cgnat", ""),
    ("wifi-to-cell", ""),
    ("long-outage", ""),
    ("bufferbloat", ""),
    ("downstream-loss", ""),
    ("asym-bufferbloat", ""),
    ("listen-uplink", ""),
    ("lossy-fat-pipe", ""),
    ("rtt-extreme", ""),
    ("mtu-1280", ""),
    ("mtu-blackhole", ""),
    ("flood", ""),
    ("forged-handshake", ""),
    ("relay-path", "relay v2"),
    ("lockstep-mix", ""),
    ("mixed", ""),
    ("congested+bulk", ""),
    ("lockstep-mix+action", ""),
    ("lockstep-mix+bots", ""),
];

/// The ids of the bars that gate, from [`BARS`].
pub fn gating_ids() -> Vec<&'static str> {
    BARS.iter().filter(|b| b.gating).map(|b| b.id).collect()
}

/// True when bar `id` gates; an id the table does not know gates (a new bar must be listed).
pub fn bar_gates(id: &str) -> bool {
    BARS.iter().find(|b| b.id == id).is_none_or(|b| b.gating)
}

/// The `--bars` table.
pub fn bars_listing() -> String {
    let mut s = String::from("| Bar | Gates | Owner | Profiles | Pass when |\n|---|---|---|---|---|\n");
    for b in BARS {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            b.id,
            if b.gating { "yes" } else { "no" },
            b.owner,
            b.profiles,
            b.rule
        ));
    }
    s
}

/// The `--list` profile table.
pub fn profiles_listing() -> String {
    let mut s = String::from("Profiles (`--profiles`):\n");
    for (name, needs) in PROFILES {
        if needs.is_empty() {
            s.push_str(&format!("  {name}\n"));
        } else {
            s.push_str(&format!("  {name}  (skipped: needs {needs})\n"));
        }
    }
    s
}

/// Arguments of `cargo xtask net-soak`.
#[derive(ClapArgs, Debug, Clone, Default)]
pub struct Args {
    /// `gate`, `nightly`, `scale` or `scale-gate` (N215, ADR-8454); ignored when a custom option
    /// is given.
    #[arg(long, default_value = "gate")]
    pub plan: String,
    /// The datagram path: `virtual` (in-process queues, the default) or `udp` (the same network
    /// over real UDP sockets on 127.0.0.1, ADR-8454).
    #[arg(long)]
    pub wire: Option<String>,
    /// Profiles of a custom plan, comma-separated.
    #[arg(long, value_delimiter = ',')]
    pub profiles: Vec<String>,
    /// Client counts of a custom plan, comma-separated.
    #[arg(long, value_delimiter = ',')]
    pub clients: Vec<usize>,
    /// Server modes of a custom plan (`dedicated`, `listen`), comma-separated.
    #[arg(long, value_delimiter = ',')]
    pub modes: Vec<String>,
    /// Virtual seconds per run of a custom plan.
    #[arg(long)]
    pub seconds: Option<u64>,
    /// Seed of every random stream (default: the soak's fixed seed).
    #[arg(long)]
    pub seed: Option<u64>,
    /// Worker threads, at most 16.
    #[arg(long, default_value_t = 8)]
    pub jobs: usize,
    /// Output directory (default `target/net-soak/<UTC time>`; relative paths are under the
    /// workspace root).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Build the soak in release (the default is the dev profile, whose dependencies are optimized).
    #[arg(long)]
    pub release: bool,
    /// Run the light matrix (`regia_net_soak`'s own test, no heap measurement): builds only
    /// `regia_net` and the harness, not the renderer.
    #[arg(long)]
    pub light: bool,
    /// A label for the report (a commit, a host).
    #[arg(long)]
    pub label: Option<String>,
    /// Run nothing: read `--out`/result.json and exit with its verdict.
    #[arg(long, requires = "out")]
    pub no_run: bool,
    /// Run nothing: print the profiles and the bars (with each bar's owner and whether it gates).
    #[arg(long)]
    pub list: bool,
    /// Run nothing: print the bars alone.
    #[arg(long)]
    pub bars: bool,
    /// Print the two-host real-UDP recipe for `HOST_A` (the host process) and `HOST_B` (the seat
    /// processes) and exit. This does not open a connection or ssh.
    #[arg(long, num_args = 2, value_names = ["HOST_A", "HOST_B"])]
    pub two_host: Vec<String>,
    /// Seats named by [`two_host_recipe`] (default 4).
    #[arg(long, default_value_t = 4)]
    pub two_host_seats: u16,
    /// Ticks named by [`two_host_recipe`] (default 600).
    #[arg(long, default_value_t = 600)]
    pub two_host_ticks: u64,
    /// UDP port named by [`two_host_recipe`] (default 7788).
    #[arg(long, default_value_t = 7788)]
    pub two_host_port: u16,
}

/// The two-host recipe: the host process on `host_a`, one `lockstep_bot seat` process per seat on
/// `host_b`, over real UDP. Pass when the host prints `verdict PASS` and `desyncs=0`, every seat
/// exits 0, and the host prints an `upload` line. `verdict FAIL` or a non-zero seat is a failure.
pub fn two_host_recipe(host_a: &str, host_b: &str, seats: u16, ticks: u64, port: u16) -> String {
    let last = seats.saturating_sub(1);
    format!(
        "two-host lockstep recipe (real UDP, not loopback)\n\
         host on {host_a}:\n\
         \tlockstep_bot host --bind 0.0.0.0 --port {port} --seats {seats} --ticks {ticks}\n\
         seats on {host_b}, one process each, seat 0..={last}:\n\
         \tlockstep_bot seat --bind 0.0.0.0 --server {host_a}:{port} --seat <i> --ticks {ticks} --mix bots --seed 24301\n\
         pass: the host prints `verdict PASS` and `desyncs=0`; every seat exits 0; the host prints an `upload` line.\n\
         fail: `verdict FAIL`, a non-zero seat exit, or a missing upload line.\n"
    )
}

/// True when the gate row runs: the environment override, else [`GATE_ROW_ENABLED`].
pub fn gate_row_enabled() -> bool {
    match std::env::var(GATE_ROW_ENV).ok().as_deref().map(str::trim) {
        Some("1") | Some("true") => true,
        Some("0") | Some("false") => false,
        _ => GATE_ROW_ENABLED,
    }
}

/// `YYYYMMDDTHHMMSSZ` for a Unix time.
pub fn utc_stamp(secs: u64) -> String {
    let date = crate::windows_pass::civil_date(secs / 86_400).replace('-', "");
    let s = secs % 86_400;
    format!("{date}T{:02}{:02}{:02}Z", s / 3600, (s / 60) % 60, s % 60)
}

/// The plan name and its environment for the test process.
pub fn plan_env(args: &Args) -> Result<(String, Vec<(String, String)>)> {
    let custom = !args.profiles.is_empty()
        || !args.clients.is_empty()
        || !args.modes.is_empty()
        || args.seconds.is_some();
    let plan = if custom {
        "custom".to_string()
    } else {
        match args.plan.as_str() {
            "gate" | "nightly" | "scale" | "scale-gate" => args.plan.clone(),
            other => bail!(
                "--plan {other:?}: use gate, nightly, scale or scale-gate, or a custom set with --profiles/--clients/--modes/--seconds"
            ),
        }
    };
    for m in &args.modes {
        if m != "dedicated" && m != "listen" {
            bail!("--modes: {m:?} is not dedicated or listen");
        }
    }
    if args.clients.contains(&0) {
        bail!("--clients: a run needs at least one client");
    }
    let mut env = vec![
        ("REGIA_NET_SOAK_PLAN".to_string(), plan.clone()),
        (
            "REGIA_NET_SOAK_JOBS".to_string(),
            args.jobs.clamp(1, MAX_JOBS).to_string(),
        ),
        (GATING_ENV.to_string(), gating_ids().join(",")),
    ];
    if let Some(seed) = args.seed {
        env.push(("REGIA_NET_SOAK_SEED".into(), seed.to_string()));
    }
    if !args.profiles.is_empty() {
        env.push(("REGIA_NET_SOAK_PROFILES".into(), args.profiles.join(",")));
    }
    if !args.clients.is_empty() {
        let c: Vec<String> = args.clients.iter().map(usize::to_string).collect();
        env.push(("REGIA_NET_SOAK_CLIENTS".into(), c.join(",")));
    }
    if !args.modes.is_empty() {
        env.push(("REGIA_NET_SOAK_MODES".into(), args.modes.join(",")));
    }
    if let Some(s) = args.seconds {
        if s == 0 {
            bail!("--seconds must be at least 1");
        }
        env.push(("REGIA_NET_SOAK_SECONDS".into(), s.to_string()));
    }
    if let Some(l) = &args.label {
        env.push(("REGIA_NET_SOAK_LABEL".into(), l.clone()));
    }
    if let Some(w) = &args.wire {
        if w != "virtual" && w != "udp" {
            bail!("--wire: {w:?} is not virtual or udp");
        }
        env.push(("REGIA_NET_SOAK_WIRE".into(), w.clone()));
    }
    Ok((plan, env))
}

/// The `cargo` arguments of the test process: the full matrix (`regia_test_utils`'s
/// `net_soak_matrix`, `--workspace` so features resolve as in the gate's test build) or, with
/// `light`, `regia_net_soak`'s `net_soak_matrix_light`.
pub fn cargo_args(release: bool, light: bool) -> Vec<String> {
    let mut a = vec!["test".to_string()];
    if release {
        a.push("--release".into());
    }
    let (select, target, test): (&[&str], &str, &str) = if light {
        (&["-p", "regia_net_soak"], "harness", "net_soak_matrix_light")
    } else {
        (&["--workspace"], "net_soak", "net_soak_matrix")
    };
    a.extend(select.iter().map(|s| (*s).to_string()));
    a.extend(
        [
            "--test",
            target,
            "--",
            "--exact",
            test,
            "--nocapture",
            "--test-threads=1",
        ]
        .iter()
        .map(|s| (*s).to_string()),
    );
    a
}

/// The verdict of a `result.json`: `(passed, known-blocker failures, new failures, runs)`. Version 1
/// is taken as written; from version 2 the failure counts come from the bars, and a bar gates when
/// [`BARS`] says so (an id the table does not know gates).
pub fn verdict(json: &Value) -> Result<(bool, u64, u64, usize)> {
    if json.get("format").and_then(Value::as_str) != Some(RESULT_FORMAT) {
        bail!("not a {RESULT_FORMAT} result");
    }
    let runs = json.get("results").and_then(Value::as_array).map_or(0, Vec::len);
    let version = json.get("version").and_then(Value::as_u64).unwrap_or(1);
    if version > 2 {
        bail!("{RESULT_FORMAT} version {version}: this xtask reads 1 and 2");
    }
    if version < 2 {
        return Ok((
            json.get("passed").and_then(Value::as_bool).unwrap_or(false),
            json.get("known_blocker_failures")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            json.get("new_failures").and_then(Value::as_u64).unwrap_or(0),
            runs,
        ));
    }
    let (mut known, mut new) = (0, 0);
    for bar in failed_bars(json) {
        if !bar_gates(bar.get("id").and_then(Value::as_str).unwrap_or("")) {
            continue;
        }
        let blocked = bar
            .get("known_blockers")
            .and_then(Value::as_array)
            .is_some_and(|b| !b.is_empty());
        if blocked {
            known += 1;
        } else {
            new += 1;
        }
    }
    Ok((known + new == 0, known, new, runs))
}

fn failed_bars(json: &Value) -> impl Iterator<Item = &Value> {
    json.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|r| r.get("bars").and_then(Value::as_array).into_iter().flatten())
        .filter(|b| b.get("passed").and_then(Value::as_bool) == Some(false))
}

/// Failed bars that do not gate, by id (v2 results).
pub fn non_gating_failures(json: &Value) -> std::collections::BTreeMap<String, u64> {
    let mut out = std::collections::BTreeMap::new();
    for bar in failed_bars(json) {
        let id = bar.get("id").and_then(Value::as_str).unwrap_or("");
        if !bar_gates(id) {
            *out.entry(id.to_string()).or_insert(0) += 1;
        }
    }
    out
}

/// Runs the command.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if args.list || args.bars {
        if args.list {
            println!("{}", profiles_listing());
        }
        println!(
            "Bars (spec 120 § 15.2; ADR-7899, ADR-8030):\n\n{}",
            bars_listing()
        );
        return Ok(ExitCode::SUCCESS);
    }
    if args.two_host.len() == 2 {
        println!(
            "{}",
            two_host_recipe(
                &args.two_host[0],
                &args.two_host[1],
                args.two_host_seats,
                args.two_host_ticks,
                args.two_host_port
            )
        );
        return Ok(ExitCode::SUCCESS);
    }
    let out = match &args.out {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => root.join(p),
        None => {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            root.join("target/net-soak").join(utc_stamp(secs))
        }
    };
    if !args.no_run {
        let (plan, env) = plan_env(args)?;
        std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
        println!("net-soak: plan {plan}, output {}", out.display());
        let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .current_dir(root)
            .args(cargo_args(args.release, args.light))
            .envs(env)
            .env("REGIA_NET_SOAK_OUT", &out)
            .status()
            .context("starting the net-soak test process")?;
        if !status.success() {
            eprintln!("net-soak: the harness failed ({status}); see the output above");
            return Ok(ExitCode::from(2));
        }
    }
    let path = out.join("result.json");
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let json: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let (passed, known, new, runs) = verdict(&json)?;
    println!(
        "net-soak: {runs} runs, {} ({known} failed bars are known blockers, {new} are new); summary {}",
        if passed { "PASS" } else { "FAIL" },
        out.join("summary.md").display()
    );
    let open = non_gating_failures(&json);
    if !open.is_empty() {
        let list: Vec<String> = open.iter().map(|(id, n)| format!("{id} x{n}")).collect();
        println!(
            "net-soak: not gating yet (owned by later lanes, `--bars`): {}",
            list.join(", ")
        );
    }
    Ok(if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_is_utc() {
        // 2026-11-15 20:15:30 UTC.
        assert_eq!(utc_stamp(1_794_773_730), "20261115T201530Z");
    }

    #[test]
    fn custom_options_select_the_custom_plan() {
        let a = Args {
            plan: "gate".into(),
            profiles: vec!["flappy".into()],
            clients: vec![8, 32],
            jobs: 99,
            ..Args::default()
        };
        let (plan, env) = plan_env(&a).expect("plan");
        assert_eq!(plan, "custom");
        assert!(env.contains(&("REGIA_NET_SOAK_CLIENTS".into(), "8,32".into())));
        assert!(
            env.contains(&("REGIA_NET_SOAK_JOBS".into(), "16".into())),
            "jobs are capped"
        );
        let bad = Args {
            plan: "weekly".into(),
            ..Args::default()
        };
        assert!(plan_env(&bad).is_err());
        let zero = Args {
            clients: vec![0],
            ..Args::default()
        };
        assert!(plan_env(&zero).is_err());
    }

    #[test]
    fn verdict_reads_the_counts() {
        let v: Value = serde_json::json!({
            "format": RESULT_FORMAT, "passed": false, "known_blocker_failures": 2,
            "new_failures": 1, "results": [{}, {}]
        });
        assert_eq!(verdict(&v).expect("verdict"), (false, 2, 1, 2));
        assert!(verdict(&serde_json::json!({"format": "other"})).is_err());
    }

    /// The table and the soak agree: every spec 120 bar and every harness bar is listed once, the
    /// soak's default gating list is the table's, and the profile list is the soak's.
    #[test]
    fn bar_and_profile_tables_match_the_soak() {
        let report = include_str!("../../crates/regia_net_soak/src/report.rs");
        let profiles = include_str!("../../crates/regia_net_soak/src/profiles.rs");
        let lib = include_str!("../../crates/regia_net_soak/src/lib.rs");
        let list = |text: &str, start: &str| -> Vec<String> {
            let from = text.find(start).unwrap_or_else(|| panic!("{start} not found")) + start.len();
            let body = &text[from..from + text[from..].find("];").expect("list end")];
            body.split('"').skip(1).step_by(2).map(str::to_string).collect()
        };
        let gating = list(report, "pub const GATING_BARS: &[&str] = &[");
        let spec120 = list(report, "pub const SPEC120_BARS: [&str; 15] = [");
        let ids: Vec<&str> = BARS.iter().map(|b| b.id).collect();
        // The harness's bars, then the spec 120 bars. A spec 120 bar its lane has flipped is in
        // both of the soak's lists and once in the table.
        let mut want: Vec<String> = gating
            .iter()
            .filter(|id| !spec120.contains(id))
            .cloned()
            .collect();
        assert_eq!(want.len(), 6, "the harness's six");
        want.extend(spec120.iter().cloned());
        assert_eq!(ids, want, "BARS lists the soak's bars in its order");
        assert_eq!(
            gating_ids(),
            gating,
            "the soak's default gating list is the table's"
        );
        // Three spec 120 bars gate: `lockstep_upload` (ADR-8209), and `interrupted_once` and
        // `resume_same_peer` since the soak's outage rows run the resume (ADR-10093); the others
        // wait for their lanes.
        assert!(bar_gates("lockstep_upload") && !bar_gates("desync_zero"));
        assert!(bar_gates("interrupted_once") && bar_gates("resume_same_peer"));
        assert_eq!(
            gating
                .iter()
                .filter(|id| spec120.contains(id))
                .collect::<Vec<_>>(),
            ["interrupted_once", "resume_same_peer", "lockstep_upload"]
        );
        assert!(bars_listing().contains("| lockstep_upload | yes | N224 net-lockstep-core |"));
        assert!(bars_listing().contains("| interrupted_once | yes | N101 net-timeout-policy |"));
        assert!(bars_listing().contains("| resume_same_peer | yes | N216 net-link-resume-dormant |"));
        let mut names: Vec<String> = list(profiles, "pub const PROFILE_NAMES: [&str; 28] = [");
        names.extend(list(lib, "pub const VARIANT_NAMES: [&str; 3] = ["));
        assert_eq!(
            PROFILES.iter().map(|p| p.0.to_string()).collect::<Vec<_>>(),
            names
        );
        assert!(bar_gates("a_bar_nobody_listed"), "an unknown bar gates");
        assert!(bars_listing().contains("| no_hijack | no | N103 net-rebind-path-validation |"));
        // The lockstep mode exists and the plan runs it (ADR-8209): only relay v2 is still owed.
        let listing = profiles_listing();
        assert!(listing.contains("  lockstep-mix\n") && listing.contains("  lockstep-mix+action\n"));
        assert!(listing.contains("  lockstep-mix+bots\n"));
        assert!(listing.contains("relay-path  (skipped: needs relay v2)"));
        assert!(!listing.contains("needs lockstep") && !bars_listing().contains("needs lockstep"));
        // The listing marks exactly the profiles the soak skips: every engine part a profile is
        // given as missing in `profiles.rs`, in its order, and nothing else.
        let needed: Vec<&str> = profiles
            .split("prof.needs = Some(\"")
            .skip(1)
            .map(|rest| rest.split('"').next().expect("the missing part's name"))
            .collect();
        let listed: Vec<&str> = PROFILES
            .iter()
            .map(|p| p.1)
            .filter(|needs| !needs.is_empty())
            .collect();
        assert_eq!(listed, needed);
        // The upload bar's row says what the soak judges: the rule of spec 120 § 7.5 as amended
        // and the seat count it is stated for under each input mix (the soak's two constants).
        let seats = |name: &str| -> String {
            let start = format!("pub const {name}: usize = ");
            let from = report.find(&start).unwrap_or_else(|| panic!("{name} not found")) + start.len();
            report[from..].split(';').next().expect("value").to_string()
        };
        let upload = BARS.iter().find(|b| b.id == "lockstep_upload").expect("row");
        assert_eq!(
            upload.profiles,
            "lockstep-mix, lockstep-mix+action, lockstep-mix+bots"
        );
        assert!(
            upload
                .rule
                .contains("95th-percentile second <= 4.0 Mbit/s and busiest second <= 4.4 Mbit/s"),
            "{}",
            upload.rule
        );
        assert!(
            upload.rule.contains(&format!(
                "{} seats on the orders mix",
                seats("LOCKSTEP_UPLOAD_SEATS_ORDERS")
            )) && upload.rule.contains(&format!(
                "{} on the action mix",
                seats("LOCKSTEP_UPLOAD_SEATS_ACTION")
            )),
            "{}",
            upload.rule
        );
    }

    /// `--two-host` prints both machines, a real bind, the bot mix and the pass rule. It does not
    /// launch anything.
    #[test]
    fn two_host_recipe_names_both_machines_and_the_pass_rule() {
        let text = two_host_recipe("192.0.2.10", "192.0.2.20", 4, 600, 7788);
        assert!(
            text.contains("192.0.2.10") && text.contains("192.0.2.20"),
            "{text}"
        );
        assert!(text.contains("real UDP"), "{text}");
        assert!(text.contains("--bind 0.0.0.0"), "{text}");
        assert!(text.contains("--mix bots"), "{text}");
        assert!(text.contains("--seed 24301"), "{text}");
        assert!(
            text.contains("verdict PASS") && text.contains("desyncs=0"),
            "{text}"
        );
        assert!(text.contains("upload"), "{text}");
        assert!(text.contains("verdict FAIL"), "{text}");
        assert!(!text.contains("127.0.0.1"), "{text}");
    }

    #[test]
    fn v2_verdict_comes_from_the_gating_bars() {
        let v: Value = serde_json::json!({
            "format": RESULT_FORMAT, "version": 2, "passed": false, "known_blocker_failures": 9,
            "new_failures": 9, "results": [{"bars": [
                {"id": "no_wedged_client", "passed": false, "known_blockers": ["v051-net-input-mtu"]},
                {"id": "missed_input", "passed": false, "known_blockers": []},
                {"id": "no_hijack", "passed": false, "known_blockers": []},
                {"id": "staleness_p99", "passed": true, "known_blockers": []}
            ]}]
        });
        assert_eq!(verdict(&v).expect("verdict"), (false, 1, 0, 1));
        assert_eq!(non_gating_failures(&v).len(), 2);
        let only_open: Value = serde_json::json!({
            "format": RESULT_FORMAT, "version": 2, "results": [{"bars": [
                {"id": "missed_input", "passed": false, "known_blockers": []}
            ]}]
        });
        assert_eq!(
            verdict(&only_open).expect("verdict"),
            (true, 0, 0, 1),
            "a non-gating failure does not fail the soak"
        );
        assert!(verdict(&serde_json::json!({"format": RESULT_FORMAT, "version": 3})).is_err());
        let a = Args {
            plan: "gate".into(),
            ..Args::default()
        };
        let (_, env) = plan_env(&a).expect("plan");
        let gating = env.iter().find(|(k, _)| k == GATING_ENV).expect("gating env");
        assert!(gating.1.contains("no_wedged_client") && !gating.1.contains("missed_input"));
        // A failed `lockstep_upload` fails the soak: the bar gates (ADR-8209).
        assert!(gating.1.contains("lockstep_upload"));
        let upload: Value = serde_json::json!({
            "format": RESULT_FORMAT, "version": 2, "results": [{"bars": [
                {"id": "lockstep_upload", "passed": false, "known_blockers": []},
                {"id": "desync_zero", "passed": false, "known_blockers": []}
            ]}]
        });
        assert_eq!(verdict(&upload).expect("verdict"), (false, 0, 1, 1));
        assert_eq!(non_gating_failures(&upload).len(), 1);
    }

    #[test]
    fn the_test_process_is_the_matrix_test() {
        let a = cargo_args(true, false);
        assert_eq!(&a[..2], &["test", "--release"]);
        assert!(a.iter().any(|x| x == "net_soak_matrix"));
        let light = cargo_args(false, true);
        assert!(light.iter().any(|x| x == "regia_net_soak"));
        assert!(light.iter().any(|x| x == "net_soak_matrix_light"));
    }
}
