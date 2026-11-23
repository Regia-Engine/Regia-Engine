// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask net-match`: the `net-game` template played across real machines (ADR-3075).
//!
//! Two steps:
//!
//! - `net-match generate --out DIR [--engine PATH]` writes the `net-game` template the way
//!   `regia new --template net-game netmatch` does (crate `netmatch`), with `{{engine}}` set to
//!   `--engine` (the engine checkout **on the build host**, default this checkout) and the engine's
//!   `Cargo.lock` copied in, so every host builds the same dependency versions. Build it on each
//!   host with `cargo build -p netmatch_server` and, separately, `cargo build -p netmatch_client`
//!   (dev profile: a release build ignores `REGIA_NET_CONDITIONER`, and the run refuses a server
//!   that reports no conditioner when one was asked for).
//! - `net-match run` starts the dedicated server on one host (`--server`, `local` or an ssh
//!   destination) with a `server.toml` it writes there, waits for `listening <addr> key <key>`,
//!   starts every `--client KIND,HOST,BINARY` (`windowed` or `headless`; a binary ending in `.exe`
//!   is started through the Windows shell), each with the stated `--conditioner` and its own seed,
//!   waits for the clients to finish their `--ticks`, stops the server with the console's `quit 0`,
//!   and compares what every process printed: the server's final avatars against each client's
//!   (the bots stop `--settle` ticks before the end, so everyone must agree to the millimeter).
//!   It writes `report.json`, `report.md` and each process's output under `--out`, and exits 1 on
//!   a desync, a client that did not connect, or a process that failed.
//!
//! The match is bounded by ticks, not by a clock: the only clocks are the watchdogs that end a
//! process which overran its ticks by minutes (a hung host), and the measured wall time the report
//! states next to the tick count.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand};
use serde_json::json;

/// The crate name of the generated project.
pub const CRATE_NAME: &str = "netmatch";
/// Fixed steps per second of the template.
pub const TICK_HZ: u64 = 60;
/// A process's positions are printed to the millimeter; the prediction tolerance is one more.
pub const AGREEMENT_M: f32 = 0.001 + 0.001 + 1e-4;

/// Arguments of `cargo xtask net-match`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: NetMatchCmd,
}

/// `net-match` steps.
#[derive(Subcommand, Debug, Clone)]
pub enum NetMatchCmd {
    /// Write the `net-game` template project (crate `netmatch`) for one build host.
    Generate(GenerateArgs),
    /// Play one match across hosts and write the report.
    Run(RunArgs),
}

/// `net-match generate`.
#[derive(ClapArgs, Debug, Clone)]
pub struct GenerateArgs {
    /// Directory to write the project into (created; must not hold a `Cargo.toml`).
    #[arg(long)]
    pub out: PathBuf,
    /// The engine checkout on the host that builds the project (default: this checkout).
    #[arg(long)]
    pub engine: Option<String>,
}

/// `net-match run`.
#[derive(ClapArgs, Debug, Clone)]
pub struct RunArgs {
    /// Where the server runs: `local` or an ssh destination.
    #[arg(long)]
    pub server: String,
    /// The `netmatch_server` binary on that host.
    #[arg(long)]
    pub server_bin: String,
    /// The generated project on that host (the server's working directory, for `assets/`).
    #[arg(long)]
    pub server_project: String,
    /// The server directory on that host (`server.toml`, `server.key`, `logs/`; created).
    #[arg(long)]
    pub server_dir: String,
    /// The address clients dial (the server host's LAN address).
    #[arg(long)]
    pub server_ip: String,
    /// The game's UDP port.
    #[arg(long, default_value_t = 7777)]
    pub port: u16,
    /// A client process: `KIND,HOST,BINARY` with KIND `windowed` or `headless` and HOST `local`
    /// or an ssh destination (repeatable).
    #[arg(long = "client", required = true)]
    pub clients: Vec<String>,
    /// Stated network conditions on every process's outbound datagrams (a profile such as `wan`
    /// or `delay=40,jitter=10,loss=0.02`; see `regia_net::conditioner_spec`).
    #[arg(long, default_value = "off")]
    pub conditioner: String,
    /// The server's conditioner seed; client `i` uses `seed + 1 + i`.
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    /// Fixed steps each client plays.
    #[arg(long, default_value_t = 3600)]
    pub ticks: u64,
    /// Fixed steps at the end of each client's run in which its bot stands still.
    #[arg(long, default_value_t = 600)]
    pub settle: u64,
    /// Output directory for the report and the processes' output.
    #[arg(long)]
    pub out: PathBuf,
    /// Print the commands and the `server.toml`, start nothing.
    #[arg(long)]
    pub dry_run: bool,
}

/// One client process of a match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSpec {
    /// `windowed` or `headless`.
    pub kind: ClientKind,
    /// `local` or an ssh destination.
    pub host: String,
    /// The `netmatch_client` binary on that host.
    pub bin: String,
}

/// How a client runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    /// With a window and the renderer.
    Windowed,
    /// Without a window (`--headless`).
    Headless,
}

impl ClientKind {
    /// The word on the command line and in the report.
    pub fn name(self) -> &'static str {
        match self {
            Self::Windowed => "windowed",
            Self::Headless => "headless",
        }
    }
}

/// Parses `KIND,HOST,BINARY`.
///
/// # Errors
///
/// Not three parts, or an unknown kind.
pub fn parse_client(text: &str) -> Result<ClientSpec> {
    let parts: Vec<&str> = text.splitn(3, ',').map(str::trim).collect();
    let [kind, host, bin] = parts[..] else {
        bail!("--client `{text}`: expected KIND,HOST,BINARY");
    };
    let kind = match kind {
        "windowed" => ClientKind::Windowed,
        "headless" => ClientKind::Headless,
        other => bail!("--client `{text}`: kind `{other}` is not windowed or headless"),
    };
    if host.is_empty() || bin.is_empty() {
        bail!("--client `{text}`: empty host or binary");
    }
    Ok(ClientSpec {
        kind,
        host: host.to_owned(),
        bin: bin.to_owned(),
    })
}

/// True when the binary is a Windows executable (started through the Windows shell).
pub fn is_windows_bin(bin: &str) -> bool {
    bin.to_ascii_lowercase().ends_with(".exe")
}

/// The `server.toml` of a match.
pub fn server_toml(port: u16) -> String {
    format!(
        r#"# Written by `cargo xtask net-match` (ADR-3075).
format_version = 1
[server]
name = "net-match"
bind = "0.0.0.0"
port = {port}
max_players = 16
tick_hz = {TICK_HZ}
[levels]
rotation = ["levels/arena.level.ron"]
[net]
encryption = "required"
identity_file = "server.key"
[status]
enabled = false
[logs]
dir = "logs"
"#
    )
}

/// Quotes one word for a POSIX shell.
pub fn sh_quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=,@%+".contains(&b))
    {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// Quotes one word for the Windows shell (`cmd`), which the OpenSSH server runs by default.
pub fn cmd_quote(word: &str) -> String {
    if !word.is_empty() && !word.bytes().any(|b| b" \t\"&|<>^%".contains(&b)) {
        return word.to_owned();
    }
    format!("\"{}\"", word.replace('"', "\"\""))
}

/// The remote command line that runs `bin args` with `env` in `cwd` (`None`: the login
/// directory), for a POSIX host or, when `bin` ends in `.exe`, the Windows shell.
pub fn remote_line(bin: &str, args: &[String], env: &[(String, String)], cwd: Option<&str>) -> String {
    let mut parts = Vec::new();
    if is_windows_bin(bin) {
        if let Some(dir) = cwd {
            parts.push(format!("cd /d {} &&", cmd_quote(dir)));
        }
        for (k, v) in env {
            parts.push(format!("set \"{k}={v}\" &&"));
        }
        parts.push(cmd_quote(bin));
        parts.extend(args.iter().map(|a| cmd_quote(a)));
    } else {
        if let Some(dir) = cwd {
            parts.push(format!("cd {} &&", sh_quote(dir)));
        }
        parts.push("exec".into());
        if !env.is_empty() {
            parts.push("env".into());
            parts.extend(env.iter().map(|(k, v)| sh_quote(&format!("{k}={v}"))));
        }
        parts.push(sh_quote(bin));
        parts.extend(args.iter().map(|a| sh_quote(a)));
    }
    parts.join(" ")
}

/// The arguments of client `index` of a match.
pub fn client_args(spec: &ClientSpec, index: usize, run: &RunArgs, key: &str) -> Vec<String> {
    let mut args = Vec::new();
    if spec.kind == ClientKind::Headless {
        args.push("--headless".to_owned());
    }
    args.push("--bot".to_owned());
    let play = run.ticks.saturating_sub(run.settle);
    for (flag, value) in [
        ("--connect", format!("{}:{}", run.server_ip, run.port)),
        ("--key", key.to_owned()),
        ("--name", format!("{}{index}", spec.kind.name())),
        ("--bot-for", play.to_string()),
        ("--ticks", run.ticks.to_string()),
        ("--conditioner", run.conditioner.clone()),
        (
            "--seed",
            run.seed.wrapping_add(1).wrapping_add(index as u64).to_string(),
        ),
    ] {
        args.push(flag.to_owned());
        args.push(value);
    }
    args
}

/// The server's program and arguments. On a POSIX host it runs under `timeout`, so a server whose
/// supervisor died (the ssh link dropped before `quit 0`) still ends: three times the match plus
/// ten minutes.
pub fn server_command(run: &RunArgs) -> (String, Vec<String>) {
    // `--console`: the host reads console commands from a stdin that is not a terminal only with it,
    // and the run stops the server with `quit 0` there.
    let mut args = vec![
        "--console".to_owned(),
        "--level".to_owned(),
        "levels/arena.level.ron".to_owned(),
    ];
    if is_windows_bin(&run.server_bin) {
        return (run.server_bin.clone(), args);
    }
    let limit = run.ticks / TICK_HZ * 3 + 600;
    let mut wrapped = vec![
        "-s".to_owned(),
        "TERM".to_owned(),
        limit.to_string(),
        run.server_bin.clone(),
    ];
    wrapped.append(&mut args);
    ("timeout".to_owned(), wrapped)
}

/// The server's environment: its directory, the project's assets (the level) and the conditioner
/// (read by dev builds).
pub fn server_env(run: &RunArgs) -> Vec<(String, String)> {
    let sep = if is_windows_bin(&run.server_bin) {
        '\\'
    } else {
        '/'
    };
    vec![
        ("REGIA_SERVER_DIR".into(), run.server_dir.clone()),
        (
            "REGIA_ASSETS".into(),
            format!("{}{sep}assets", run.server_project.trim_end_matches(['/', '\\'])),
        ),
        ("REGIA_NET_CONDITIONER".into(), run.conditioner.clone()),
        ("REGIA_NET_CONDITIONER_SEED".into(), run.seed.to_string()),
    ]
}

/// The `key=value` words of one output line.
pub fn fields(line: &str) -> BTreeMap<&str, &str> {
    line.split_whitespace()
        .filter_map(|w| w.split_once('='))
        .collect()
}

/// One avatar as a process printed it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seen {
    /// Position x.
    pub x: f32,
    /// Position z.
    pub z: f32,
    /// Tags scored.
    pub tags: u32,
    /// Times tagged.
    pub tagged: u32,
}

/// The avatars of lines starting with `word`, by slot.
pub fn avatars(text: &str, word: &str) -> BTreeMap<u32, Seen> {
    let mut out = BTreeMap::new();
    for line in text.lines().filter(|l| l.starts_with(word)) {
        let f = fields(line);
        let num = |k: &str| f.get(k).and_then(|v| v.parse::<f32>().ok());
        let whole = |k: &str| f.get(k).and_then(|v| v.parse::<u32>().ok());
        if let (Some(slot), Some(x), Some(z), Some(tags), Some(tagged)) =
            (whole("slot"), num("x"), num("z"), whole("tags"), whole("tagged"))
        {
            out.insert(slot, Seen { x, z, tags, tagged });
        }
    }
    out
}

/// Every printed record must parse exactly once and have finite coordinates. Silently skipped
/// malformed records, duplicate slots and NaNs cannot be evidence of agreement.
fn valid_avatar_records(text: &str, word: &str, parsed: &BTreeMap<u32, Seen>) -> bool {
    text.lines().filter(|line| line.starts_with(word)).count() == parsed.len()
        && parsed
            .values()
            .all(|avatar| avatar.x.is_finite() && avatar.z.is_finite())
}

/// The `netstats` line of `who`, as fields.
pub fn netstats<'a>(text: &'a str, who: &str) -> Option<BTreeMap<&'a str, &'a str>> {
    text.lines()
        .filter(|l| l.starts_with("netstats "))
        .map(fields)
        .find(|f| f.get("who") == Some(&who))
}

/// What one client process showed at the end.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientResult {
    /// `windowed` or `headless`.
    pub kind: &'static str,
    /// The host.
    pub host: String,
    /// The process exited with success.
    pub exited_ok: bool,
    /// It printed `client 0 connected=true`.
    pub connected: bool,
    /// Slots the server has and this client did not see.
    pub missing_slots: Vec<u32>,
    /// Slots whose score differs from the server's.
    pub score_mismatches: Vec<u32>,
    /// The largest position difference against the server, meters.
    pub max_position_error_m: f32,
    /// The client printed complete, finite avatar records for known server slots.
    pub judged: bool,
    /// Its `netstats` fields (numbers as text).
    pub stats: BTreeMap<String, String>,
}

impl ClientResult {
    /// True when this client exited successfully and its judgable state agrees with the server.
    pub fn agrees(&self) -> bool {
        self.exited_ok
            && self.connected
            && self.judged
            && self.missing_slots.is_empty()
            && self.score_mismatches.is_empty()
            && self.max_position_error_m <= AGREEMENT_M
    }
}

/// Compares a client's output with the server's final avatars.
pub fn judge_client(
    kind: &'static str,
    host: &str,
    exited_ok: bool,
    text: &str,
    server: &BTreeMap<u32, Seen>,
    departed: &BTreeMap<u32, Seen>,
) -> ClientResult {
    let seen = avatars(text, "seen client=0 ");
    let connected = text
        .lines()
        .filter(|line| line.starts_with("client 0 "))
        .any(|line| fields(line).get("connected") == Some(&"true"));
    let mut missing = Vec::new();
    let mut scores = Vec::new();
    let mut worst = 0.0f32;
    // An avatar still on the server at stop must be seen; one whose player left earlier (its `left`
    // line) may be gone from a client that finished later, and is judged only if the client saw it.
    let gone = departed.iter().filter(|(slot, _)| !server.contains_key(*slot));
    for (slot, want, required) in server
        .iter()
        .map(|(s, w)| (s, w, true))
        .chain(gone.map(|(s, w)| (s, w, false)))
    {
        match seen.get(slot) {
            None if required => missing.push(*slot),
            None => {}
            Some(got) => {
                if (got.tags, got.tagged) != (want.tags, want.tagged) {
                    scores.push(*slot);
                }
                worst = worst.max((got.x - want.x).abs()).max((got.z - want.z).abs());
            }
        }
    }
    let stats = netstats(text, "client0")
        .map(|f| f.into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect())
        .unwrap_or_default();
    ClientResult {
        kind,
        host: host.to_owned(),
        exited_ok,
        connected,
        missing_slots: missing,
        score_mismatches: scores,
        max_position_error_m: worst,
        judged: !seen.is_empty()
            && valid_avatar_records(text, "seen client=0 ", &seen)
            && seen
                .keys()
                .all(|slot| server.contains_key(slot) || departed.contains_key(slot))
            && server
                .values()
                .chain(departed.values())
                .all(|avatar| avatar.x.is_finite() && avatar.z.is_finite()),
        stats,
    }
}

fn stat_u64(stats: &BTreeMap<String, String>, key: &str) -> u64 {
    stats.get(key).and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn stat_f64(stats: &BTreeMap<String, String>, key: &str) -> f64 {
    stats.get(key).and_then(|v| v.parse().ok()).unwrap_or(0.0)
}

/// The report of one match.
pub fn report_json(
    run: &RunArgs,
    wall_s: f64,
    server_text: &str,
    clients: &[ClientResult],
) -> serde_json::Value {
    let secs = run.ticks as f64 / TICK_HZ as f64;
    let server_stats: BTreeMap<String, String> = netstats(server_text, "server")
        .map(|f| f.into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect())
        .unwrap_or_default();
    let rows: Vec<serde_json::Value> = clients
        .iter()
        .map(|c| {
            let s = &c.stats;
            json!({
                "kind": c.kind,
                "host": c.host,
                "exited_ok": c.exited_ok,
                "connected": c.connected,
                "judged": c.judged,
                "agrees": c.agrees(),
                "missing_slots": c.missing_slots,
                "score_mismatches": c.score_mismatches,
                "max_position_error_m": c.max_position_error_m,
                "rtt_ms": stat_f64(s, "rtt_ms"),
                "jitter_ms": stat_f64(s, "jitter_ms"),
                "loss": stat_f64(s, "loss"),
                "bytes_up": stat_u64(s, "bytes_sent"),
                "bytes_down": stat_u64(s, "bytes_received"),
                "kbps_up": stat_u64(s, "bytes_sent") as f64 * 8.0 / 1000.0 / secs,
                "kbps_down": stat_u64(s, "bytes_received") as f64 * 8.0 / 1000.0 / secs,
                "packets_lost": stat_u64(s, "packets_lost"),
                "resends": stat_u64(s, "resends"),
                "corrections": stat_u64(s, "corrections"),
                "resets": stat_u64(s, "resets"),
                "resyncs": stat_u64(s, "resyncs"),
                "verify_failures": stat_u64(s, "verify_failures"),
                "cond_dropped": stat_u64(s, "cond_dropped"),
                "cond_delayed": stat_u64(s, "cond_delayed"),
            })
        })
        .collect();
    let current = avatars(server_text, "summary");
    let departed = avatars(server_text, "left");
    let final_avatars = current
        .keys()
        .chain(departed.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let desync = clients.is_empty()
        || final_avatars != clients.len()
        || !valid_avatar_records(server_text, "summary", &current)
        || !valid_avatar_records(server_text, "left", &departed)
        || clients.iter().any(|c| !c.agrees());
    json!({
        "conditioner": run.conditioner,
        "seed": run.seed,
        "ticks": run.ticks,
        "settle": run.settle,
        "match_seconds": secs,
        "wall_seconds": wall_s,
        "server": {
            "host": run.server,
            "avatars": current.len(),
            "final_avatars": final_avatars,
            "peers": server_stats.get("peers").cloned().unwrap_or_default(),
            "cond": server_stats.get("cond").cloned().unwrap_or_default(),
            "cond_dropped": stat_u64(&server_stats, "cond_dropped"),
            "cond_delayed": stat_u64(&server_stats, "cond_delayed"),
            "datagrams_in": stat_u64(&server_stats, "datagrams_in"),
            "datagrams_out": stat_u64(&server_stats, "datagrams_out"),
            "malformed": stat_u64(&server_stats, "malformed"),
            "auth_failed": stat_u64(&server_stats, "auth_failed"),
        },
        "clients": rows,
        "desync": desync,
    })
}

/// The report as a Markdown table.
pub fn report_markdown(report: &serde_json::Value) -> String {
    let mut out = format!(
        "# net-match\n\nConditioner `{}` (seed {}), {} ticks ({} s of play, {:.1} s wall), server on `{}` \
         ({} avatars, {} datagrams in, {} out, conditioner dropped {} / delayed {}).\n\n\
         | Client | Host | Agrees | Max error (m) | RTT (ms) | Jitter (ms) | Loss | Up (kbit/s) | Down (kbit/s) | \
         Resends | Corrections | Resets | Resyncs | Verify failures |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
        report["conditioner"].as_str().unwrap_or(""),
        report["seed"],
        report["ticks"],
        report["match_seconds"],
        report["wall_seconds"].as_f64().unwrap_or(0.0),
        report["server"]["host"].as_str().unwrap_or(""),
        report["server"]["avatars"],
        report["server"]["datagrams_in"],
        report["server"]["datagrams_out"],
        report["server"]["cond_dropped"],
        report["server"]["cond_delayed"],
    );
    for c in report["clients"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "| {} | {} | {} | {:.4} | {:.1} | {:.1} | {:.4} | {:.1} | {:.1} | {} | {} | {} | {} | {} |\n",
            c["kind"].as_str().unwrap_or(""),
            c["host"].as_str().unwrap_or(""),
            if c["agrees"].as_bool() == Some(true) {
                "yes"
            } else {
                "**no**"
            },
            c["max_position_error_m"].as_f64().unwrap_or(0.0),
            c["rtt_ms"].as_f64().unwrap_or(0.0),
            c["jitter_ms"].as_f64().unwrap_or(0.0),
            c["loss"].as_f64().unwrap_or(0.0),
            c["kbps_up"].as_f64().unwrap_or(0.0),
            c["kbps_down"].as_f64().unwrap_or(0.0),
            c["resends"],
            c["corrections"],
            c["resets"],
            c["resyncs"],
            c["verify_failures"],
        ));
    }
    out.push_str(&format!(
        "\nDesync: {}.\n",
        if report["desync"].as_bool() == Some(true) {
            "**yes**"
        } else {
            "none"
        }
    ));
    out
}

/// The `Cargo.lock` a generated project copies: the one of the engine checkout it builds against
/// (`--engine`) when that is on this machine, else the one of the workspace this xtask runs in.
/// An xtask binary copied to a build host does not have its own workspace there (found by the
/// match: `generate` on the second host failed to find the lock of the first).
fn lock_source(root: &Path, engine: &str) -> PathBuf {
    let engine_lock = Path::new(engine).join("Cargo.lock");
    if engine_lock.is_file() {
        engine_lock
    } else {
        root.join("Cargo.lock")
    }
}

/// Writes the template project for one host.
///
/// # Errors
///
/// The directory already holds a project, or an IO failure.
pub fn generate(root: &Path, out: &Path, engine: Option<&str>) -> Result<()> {
    if out.join("Cargo.toml").exists() {
        bail!("{} already holds a project", out.display());
    }
    let files = regia_templates::template_files("net_game").context("the net_game template")?;
    let engine = engine
        .map(str::to_owned)
        .unwrap_or_else(|| root.to_string_lossy().replace('\\', "/"));
    if engine.contains('"') {
        bail!("engine path `{engine}` cannot be written into Cargo.toml");
    }
    let line = regia_core::engine_version::CURRENT.line().to_string();
    for (rel, text) in files {
        let body = text
            .replace("{{crate_name}}", CRATE_NAME)
            .replace("{{engine_line}}", &line)
            .replace("{{engine}}", &engine)
            .replace("{{name}}", CRATE_NAME)
            .replace("{{id}}", &format!("com.regiaengine.{CRATE_NAME}"));
        if body.contains("{{") {
            bail!("template {rel} still has a placeholder");
        }
        let path = out.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    }
    std::fs::copy(lock_source(root, &engine), out.join("Cargo.lock"))
        .context("copy the engine's Cargo.lock")?;
    println!(
        "net-match: wrote {} ({} files); build it with `cargo build -p {CRATE_NAME}_server` and then \
         `cargo build -p {CRATE_NAME}_client`",
        out.display(),
        files.len() + 1
    );
    Ok(())
}

/// A started process and the thread that collects its stdout.
struct Proc {
    label: String,
    child: Child,
    lines: mpsc::Receiver<String>,
    text: String,
}

impl Proc {
    fn start(
        label: &str,
        host: &str,
        bin: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&str>,
        stdin: bool,
    ) -> Result<Self> {
        let mut command = if host == "local" {
            let mut c = Command::new(bin);
            c.args(args);
            c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            if let Some(dir) = cwd {
                c.current_dir(dir);
            }
            c
        } else {
            let mut c = Command::new("ssh");
            c.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15", host]);
            c.arg(remote_line(bin, args, env, cwd));
            c
        };
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command.stdin(if stdin { Stdio::piped() } else { Stdio::null() });
        let mut child = command
            .spawn()
            .with_context(|| format!("{label}: start {bin} on {host}"))?;
        let (tx, rx) = mpsc::channel();
        for pipe in [
            child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
            child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        Ok(Self {
            label: label.to_owned(),
            child,
            lines: rx,
            text: String::new(),
        })
    }

    /// Takes the lines printed so far.
    fn drain(&mut self) {
        while let Ok(line) = self.lines.try_recv() {
            self.text.push_str(&line);
            self.text.push('\n');
        }
    }

    /// Waits for a line starting with `prefix` (or the process's exit, or `limit`).
    fn wait_line(&mut self, prefix: &str, limit: Duration) -> Option<String> {
        let end = Instant::now() + limit;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left.min(Duration::from_millis(500))) {
                Ok(line) => {
                    self.text.push_str(&line);
                    self.text.push('\n');
                    if line.starts_with(prefix) {
                        return Some(line);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if left.is_zero() {
                        return None;
                    }
                }
            }
        }
    }

    /// Waits for the exit (killing the process after `limit`); true on success.
    fn wait_exit(&mut self, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        loop {
            self.drain();
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    // The reader threads end with the pipes; take their last lines.
                    while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(500)) {
                        self.text.push_str(&line);
                        self.text.push('\n');
                    }
                    return status.success();
                }
                Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(200)),
                _ => {
                    eprintln!("net-match: {} overran its ticks; stopping it", self.label);
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    self.drain();
                    return false;
                }
            }
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        // A server still running (the run failed early) gets its console's `quit 0` first: killing
        // the local ssh does not stop the remote process.
        let running = matches!(self.child.try_wait(), Ok(None));
        if running && let Some(stdin) = self.child.stdin.as_mut() {
            let _ = stdin.write_all(b"quit 0\n");
            let _ = stdin.flush();
            let end = Instant::now() + Duration::from_secs(15);
            while Instant::now() < end && matches!(self.child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Writes `text` to `path` through a temporary file.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

/// Writes the server's `server.toml` on its host.
fn put_server_toml(run: &RunArgs) -> Result<()> {
    let text = server_toml(run.port);
    if run.server == "local" {
        std::fs::create_dir_all(&run.server_dir).with_context(|| format!("create {}", run.server_dir))?;
        return write_atomic(&Path::new(&run.server_dir).join("server.toml"), &text);
    }
    let line = if is_windows_bin(&run.server_bin) {
        format!(
            "if not exist {d} mkdir {d} && powershell -NoProfile -Command \"$input | Set-Content -Path '{d}\\server.toml'\"",
            d = cmd_quote(&run.server_dir)
        )
    } else {
        let d = sh_quote(&run.server_dir);
        format!("mkdir -p {d} && cat > {d}/server.toml.tmp && mv {d}/server.toml.tmp {d}/server.toml")
    };
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            &run.server,
            &line,
        ])
        .stdin(Stdio::piped())
        .spawn()
        .context("ssh for server.toml")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).context("send server.toml")?;
    }
    let status = child.wait().context("ssh for server.toml")?;
    if !status.success() {
        bail!("writing server.toml on {} failed: {status}", run.server);
    }
    Ok(())
}

/// The conditions the server must report: the spec with the run's seed, because
/// `REGIA_NET_CONDITIONER_SEED` (which `server_env` always sets) overrides a seed in the spec
/// (found by the match: a run with `--seed 11` refused a server that ran exactly what it was told).
///
/// # Errors
///
/// The spec does not parse.
pub fn expected_server_conditions(run: &RunArgs) -> Result<regia_net::ConditionerConfig> {
    let mut expected = regia_net::ConditionerConfig::parse_spec(&run.conditioner)
        .map_err(|e| anyhow::anyhow!("--conditioner: {e}"))?;
    expected.seed = run.seed;
    Ok(expected)
}

/// Plays one match (see the module docs).
///
/// # Errors
///
/// Bad arguments, a process that cannot start, or the report cannot be written.
pub fn run_match(run: &RunArgs) -> Result<ExitCode> {
    let clients: Vec<ClientSpec> = run
        .clients
        .iter()
        .map(|c| parse_client(c))
        .collect::<Result<_>>()?;
    if run.settle >= run.ticks {
        bail!(
            "--settle ({}) must be less than --ticks ({})",
            run.settle,
            run.ticks
        );
    }
    let expected = expected_server_conditions(run)?;
    let (server_bin, server_args) = server_command(run);
    if run.dry_run {
        println!(
            "server.toml on {}:{}/server.toml\n{}",
            run.server,
            run.server_dir,
            server_toml(run.port)
        );
        println!(
            "server on {}: {}",
            run.server,
            remote_line(
                &server_bin,
                &server_args,
                &server_env(run),
                Some(&run.server_project)
            )
        );
        for (i, c) in clients.iter().enumerate() {
            println!(
                "client {i} ({}) on {}: {}",
                c.kind.name(),
                c.host,
                remote_line(&c.bin, &client_args(c, i, run, "<key>"), &[], None)
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    std::fs::create_dir_all(&run.out).with_context(|| format!("create {}", run.out.display()))?;
    put_server_toml(run)?;
    let mut server = Proc::start(
        "server",
        &run.server,
        &server_bin,
        &server_args,
        &server_env(run),
        Some(&run.server_project),
        true,
    )?;
    let Some(listening) = server.wait_line("listening ", Duration::from_secs(120)) else {
        server.drain();
        write_atomic(&run.out.join("server.txt"), &server.text)?;
        bail!("the server did not print `listening`:\n{}", server.text);
    };
    let key = listening.split_whitespace().nth(3).unwrap_or("none").to_owned();
    if key == "none" {
        bail!("the server has no key to pin (encryption off?): {listening}");
    }
    server.drain();
    let got = server
        .text
        .lines()
        .find_map(|l| l.strip_prefix("conditioner "))
        .unwrap_or("(none printed)")
        .to_owned();
    if got != expected.describe() {
        bail!(
            "the server runs conditioner `{got}`, not `{}` (a release build ignores REGIA_NET_CONDITIONER: build the \
             server in the dev profile)",
            expected.describe()
        );
    }
    println!(
        "net-match: server up on {} ({listening}); starting {} clients",
        run.server,
        clients.len()
    );
    let started = Instant::now();
    let mut procs = Vec::new();
    for (i, c) in clients.iter().enumerate() {
        let args = client_args(c, i, run, &key);
        procs.push(Proc::start(
            &format!("client{i}"),
            &c.host,
            &c.bin,
            &args,
            &[],
            None,
            false,
        )?);
    }
    let limit = Duration::from_secs(run.ticks / TICK_HZ * 3 + 180);
    let ok: Vec<bool> = procs.iter_mut().map(|p| p.wait_exit(limit)).collect();
    let wall_s = started.elapsed().as_secs_f64();
    if let Some(stdin) = server.child.stdin.as_mut() {
        let _ = stdin.write_all(b"quit 0\n");
        let _ = stdin.flush();
    }
    let server_ok = server.wait_exit(Duration::from_secs(120));
    write_atomic(&run.out.join("server.txt"), &server.text)?;
    let truth = avatars(&server.text, "summary");
    let departed = avatars(&server.text, "left");
    let mut results = Vec::new();
    for (i, (c, p)) in clients.iter().zip(&procs).enumerate() {
        write_atomic(&run.out.join(format!("client{i}-{}.txt", c.kind.name())), &p.text)?;
        results.push(judge_client(
            c.kind.name(),
            &c.host,
            ok[i],
            &p.text,
            &truth,
            &departed,
        ));
    }
    // Every client's avatar is on the server at stop or left it with a final state.
    let final_avatars = truth
        .keys()
        .chain(departed.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let mut report = report_json(run, wall_s, &server.text, &results);
    let failed = !server_ok || report["desync"] == true;
    report["server"]["exited_ok"] = json!(server_ok);
    report["desync"] = json!(failed);
    let text = serde_json::to_string_pretty(&report).context("report json")?;
    write_atomic(&run.out.join("report.json"), &text)?;
    let md = report_markdown(&report);
    write_atomic(&run.out.join("report.md"), &md)?;
    println!("{md}");
    if failed {
        eprintln!(
            "net-match: FAILED (server ok {server_ok}, {final_avatars} server avatars for {} clients); outputs in {}",
            clients.len(),
            run.out.display()
        );
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// `cargo xtask net-match`.
///
/// # Errors
///
/// See [`generate`] and [`run_match`].
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    match &args.cmd {
        NetMatchCmd::Generate(g) => {
            generate(root, &g.out, g.engine.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        NetMatchCmd::Run(r) => run_match(r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_args() -> RunArgs {
        RunArgs {
            server: "rig".into(),
            server_bin: "/b/netmatch_server".into(),
            server_project: "/p".into(),
            server_dir: "/p/srv".into(),
            server_ip: "192.0.2.75".into(),
            port: 7777,
            clients: vec![],
            conditioner: "delay=40,loss=0.02".into(),
            seed: 5,
            ticks: 3600,
            settle: 600,
            out: PathBuf::from("/tmp/x"),
            dry_run: true,
        }
    }

    /// The server gets the seed through `REGIA_NET_CONDITIONER_SEED`, which wins over a seed in
    /// the spec, so the run expects the run's seed whatever the spec says.
    #[test]
    fn the_expected_server_conditions_carry_the_run_seed() {
        let mut run = run_args();
        let got = expected_server_conditions(&run).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            got.describe(),
            "delay=40 jitter=0 loss=0.020 dup=0.000 reorder=0.000 reorder_max=0 seed=5"
        );
        let env = server_env(&run);
        let from_env = regia_net::ConditionerConfig::from_vars(
            env.iter()
                .find(|(k, _)| k == "REGIA_NET_CONDITIONER")
                .map(|(_, v)| v.as_str()),
            env.iter()
                .find(|(k, _)| k == "REGIA_NET_CONDITIONER_SEED")
                .map(|(_, v)| v.as_str()),
        )
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_default();
        assert_eq!(from_env.describe(), got.describe(), "what the server will print");
        run.conditioner = "wan,seed=7".into();
        let got = expected_server_conditions(&run).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(got.seed, 5);
        run.conditioner = "delay=fast".into();
        assert!(expected_server_conditions(&run).is_err());
    }

    #[test]
    fn client_specs_parse_and_refuse_bad_kinds() {
        let c = parse_client("windowed,local,/b/netmatch_client").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.kind, ClientKind::Windowed);
        assert_eq!((c.host.as_str(), c.bin.as_str()), ("local", "/b/netmatch_client"));
        let w = parse_client("headless,player@winhost,C:\\m\\netmatch_client.exe")
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(is_windows_bin(&w.bin));
        assert!(parse_client("fast,local,/b").is_err());
        assert!(parse_client("headless,local").is_err());
    }

    #[test]
    fn client_arguments_carry_the_bot_seed_and_conditions() {
        let run = run_args();
        let spec = parse_client("headless,local,/b/c").unwrap_or_else(|e| panic!("{e}"));
        let args = client_args(&spec, 1, &run, "KEY");
        let line = args.join(" ");
        assert!(
            line.starts_with("--headless --bot --connect 192.0.2.75:7777 --key KEY"),
            "{line}"
        );
        assert!(line.contains("--bot-for 3000 --ticks 3600"), "{line}");
        assert!(
            line.contains("--conditioner delay=40,loss=0.02 --seed 7"),
            "{line}"
        );
        let windowed = client_args(
            &ClientSpec {
                kind: ClientKind::Windowed,
                ..spec
            },
            0,
            &run,
            "KEY",
        );
        assert_eq!(windowed[0], "--bot", "{windowed:?}");
        assert!(windowed.contains(&"windowed0".to_owned()));
    }

    #[test]
    fn remote_lines_quote_for_each_shell() {
        let env = vec![(
            "REGIA_NET_CONDITIONER".to_owned(),
            "delay=40,loss=0.02".to_owned(),
        )];
        let sh = remote_line("/b/my server", &["--x".into(), "a'b".into()], &env, Some("/p"));
        assert_eq!(
            sh,
            "cd /p && exec env REGIA_NET_CONDITIONER=delay=40,loss=0.02 '/b/my server' --x 'a'\\''b'"
        );
        let win = remote_line(
            "C:\\m\\c.exe",
            &["--name".into(), "a b".into()],
            &env,
            Some("C:\\m"),
        );
        assert_eq!(
            win,
            "cd /d C:\\m && set \"REGIA_NET_CONDITIONER=delay=40,loss=0.02\" && C:\\m\\c.exe --name \"a b\""
        );
    }

    #[test]
    fn a_posix_server_runs_under_a_timeout_and_gets_the_assets() {
        let run = run_args();
        let (bin, args) = server_command(&run);
        assert_eq!(bin, "timeout");
        // 60 s of play: three times that plus ten minutes.
        assert_eq!(
            args[..5],
            ["-s", "TERM", "780", "/b/netmatch_server", "--console"]
        );
        assert!(server_env(&run).contains(&("REGIA_ASSETS".to_owned(), "/p/assets".to_owned())));
        let win = RunArgs {
            server_bin: "C:\\m\\s.exe".into(),
            server_project: "C:\\m\\".into(),
            ..run
        };
        assert_eq!(server_command(&win).0, "C:\\m\\s.exe");
        assert!(server_env(&win).contains(&("REGIA_ASSETS".to_owned(), "C:\\m\\assets".to_owned())));
    }

    #[test]
    fn the_server_toml_binds_every_interface_with_encryption() {
        let text = server_toml(7801);
        assert!(text.contains("bind = \"0.0.0.0\"\nport = 7801"), "{text}");
        assert!(text.contains("encryption = \"required\""));
        assert!(text.contains("rotation = [\"levels/arena.level.ron\"]"));
    }

    const SERVER: &str = "listening 0.0.0.0:7777 key abc\nconditioner off\n\
        summary slot=0 x=1.000 z=2.000 tags=3 tagged=1\nsummary slot=1 x=-4.000 z=0.500 tags=1 tagged=3\n\
        netstats who=server role=server state=listening tick=4000 peers=2 datagrams_in=9000 datagrams_out=9100 \
        malformed=0 auth_failed=0 cond_dropped=12 cond_delayed=9000 cond=off\n";

    fn client_text(x0: &str, tags1: u32) -> String {
        format!(
            "conditioner off\nclient 0 connected=true sealed=true\n\
             seen client=0 slot=0 x={x0} z=2.000 tags=3 tagged=1\n\
             seen client=0 slot=1 x=-4.000 z=0.500 tags={tags1} tagged=3\n\
             netstats who=client0 role=client rtt_ms=21.5 jitter_ms=3.0 loss=0.0200 bytes_sent=600000 \
             bytes_received=1200000 resends=4 corrections=2 resets=0 resyncs=0 verify_failures=0\n"
        )
    }

    #[test]
    fn agreeing_clients_pass_and_a_moved_or_rescored_avatar_is_a_desync() {
        let truth = avatars(SERVER, "summary");
        assert_eq!(truth.len(), 2);
        let good = judge_client(
            "headless",
            "buildhost",
            true,
            &client_text("1.001", 1),
            &truth,
            &BTreeMap::new(),
        );
        assert!(good.agrees(), "{good:?}");
        let moved = judge_client(
            "windowed",
            "local",
            true,
            &client_text("1.250", 1),
            &truth,
            &BTreeMap::new(),
        );
        assert!(!moved.agrees());
        assert!((moved.max_position_error_m - 0.25).abs() < 1e-4);
        let rescored = judge_client(
            "windowed",
            "local",
            true,
            &client_text("1.000", 2),
            &truth,
            &BTreeMap::new(),
        );
        assert_eq!(rescored.score_mismatches, vec![1]);
        let lost = judge_client(
            "headless",
            "x",
            false,
            "client 0 connected=false\n",
            &truth,
            &BTreeMap::new(),
        );
        assert!(!lost.connected && lost.missing_slots == vec![0, 1]);
        // No final state from the server: nothing to agree with.
        let unjudged = judge_client(
            "headless",
            "x",
            true,
            &client_text("1.000", 1),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert!(!unjudged.judged && !unjudged.agrees());
    }

    /// Run 2: the headless client finished first and left, so the server's `summary` at stop had
    /// only the windowed client's avatar. The departed avatar is judged by its `left` line: the
    /// later client may no longer see it, the earlier one must agree with it.
    #[test]
    fn a_client_that_left_first_is_judged_by_its_left_line() {
        let server = "summary slot=0 x=1.000 z=2.000 tags=3 tagged=1\n\
                      left slot=1 x=-4.000 z=0.500 tags=1 tagged=3\n";
        let present = avatars(server, "summary");
        let departed = avatars(server, "left");
        assert_eq!((present.len(), departed.len()), (1, 1));
        let early = judge_client(
            "headless",
            "buildhost",
            true,
            &client_text("1.000", 1),
            &present,
            &departed,
        );
        assert!(early.agrees(), "{early:?}");
        let late_text =
            "client 0 connected=true sealed=true\nseen client=0 slot=0 x=1.000 z=2.000 tags=3 tagged=1\n";
        let late = judge_client("windowed", "local", true, late_text, &present, &departed);
        assert!(late.agrees(), "{late:?}");
        let wrong = judge_client(
            "headless",
            "buildhost",
            true,
            &client_text("1.000", 2),
            &present,
            &departed,
        );
        assert_eq!(
            wrong.score_mismatches,
            vec![1],
            "a departed avatar the client saw still counts"
        );
        let blind =
            "client 0 connected=true sealed=true\nseen client=0 slot=1 x=-4.000 z=0.500 tags=1 tagged=3\n";
        let blind = judge_client("headless", "buildhost", true, blind, &present, &departed);
        assert_eq!(
            blind.missing_slots,
            vec![0],
            "an avatar still on the server must be seen"
        );
    }

    #[test]
    fn a_client_without_judgable_state_cannot_pass() {
        let departed = avatars(SERVER, "summary");
        for text in [
            "client 0 connected=true sealed=true\n".to_owned(),
            client_text("NaN", 1),
            client_text("inf", 1),
            client_text("not-a-number", 1),
            client_text("1.000", 1) + "seen client=0 slot=99 x=0.000 z=0.000 tags=0 tagged=0\n",
        ] {
            let result = judge_client("headless", "host", true, &text, &BTreeMap::new(), &departed);
            assert!(!result.agrees(), "unjudgable output passed: {text}\n{result:?}");
        }
    }

    #[test]
    fn client_seeds_wrap_at_the_u64_boundary_like_the_template() {
        let mut run = run_args();
        run.seed = u64::MAX;
        let client = parse_client("headless,host,/bin/client").expect("client");
        for (index, seed) in [(0, "0"), (1, "1")] {
            let args = client_args(&client, index, &run, "key");
            let pair = args.windows(2).find(|pair| pair[0] == "--seed").expect("seed");
            assert_eq!(pair[1], seed);
        }
    }

    #[test]
    fn a_failed_client_process_cannot_report_agreement() {
        let truth = avatars(SERVER, "summary");
        let result = judge_client(
            "windowed",
            "host",
            false,
            &client_text("1.000", 1),
            &truth,
            &BTreeMap::new(),
        );
        assert!(!result.agrees(), "a crash after printing state is not a pass");
        let report = report_json(&run_args(), 1.0, SERVER, &[result]);
        assert_eq!(report["desync"], true);
    }

    #[test]
    fn the_report_rejects_missing_or_malformed_server_evidence() {
        let output = "client 0 connected=true\nseen client=0 slot=0 x=1.000 z=2.000 tags=3 tagged=1\n";
        let one = "summary slot=0 x=1.000 z=2.000 tags=3 tagged=1\n";
        let truth = avatars(one, "summary");
        let client = judge_client("headless", "host", true, output, &truth, &BTreeMap::new());
        assert!(client.agrees());
        for (server, clients) in [
            (one.to_owned(), vec![client.clone(), client.clone()]),
            (format!("{one}summary slot=9 x=broken\n"), vec![client.clone()]),
            (format!("{one}{one}"), vec![client]),
            (String::new(), Vec::new()),
        ] {
            let report = report_json(&run_args(), 1.0, &server, &clients);
            assert_eq!(report["desync"], true, "{report}");
        }
    }

    #[test]
    fn the_report_states_bandwidth_reconciliation_and_desync() {
        let truth = avatars(SERVER, "summary");
        let clients = vec![
            judge_client(
                "windowed",
                "local",
                true,
                &client_text("1.000", 1),
                &truth,
                &BTreeMap::new(),
            ),
            judge_client(
                "headless",
                "buildhost",
                true,
                &client_text("1.000", 2),
                &truth,
                &BTreeMap::new(),
            ),
        ];
        let report = report_json(&run_args(), 61.5, SERVER, &clients);
        // 600 000 bytes up over 60 s of play: 80 kbit/s.
        assert!((report["clients"][0]["kbps_up"].as_f64().unwrap_or(0.0) - 80.0).abs() < 1e-9);
        assert_eq!(report["clients"][0]["corrections"], 2);
        assert_eq!(report["server"]["cond_dropped"], 12);
        assert_eq!(report["desync"], true);
        let md = report_markdown(&report);
        assert!(md.contains("| windowed | local | yes |"), "{md}");
        assert!(md.contains("| headless | buildhost | **no** |"), "{md}");
        assert!(md.contains("Desync: **yes**"), "{md}");
    }

    #[test]
    fn generate_writes_the_template_with_the_engine_path_and_lock() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let out = regia_test_utils::unique_temp_dir("net_match_gen");
        generate(&root, &out, Some("/srv/engine")).unwrap_or_else(|e| panic!("{e}"));
        let server = std::fs::read_to_string(out.join("server/Cargo.toml")).unwrap_or_default();
        assert!(server.contains("name = \"netmatch_server\""), "{server}");
        assert!(server.contains("path = \"/srv/engine/crates/regia\""), "{server}");
        assert!(out.join("Cargo.lock").is_file());
        assert!(out.join("assets/levels/arena.level.ron").is_file());
        assert!(
            generate(&root, &out, None).is_err(),
            "a second run refuses to overwrite"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// An xtask copied to another build host takes the lock of the `--engine` checkout there;
    /// without one it falls back to its own workspace.
    #[test]
    fn the_lock_comes_from_the_engine_checkout_when_it_is_on_this_host() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let engine = regia_test_utils::unique_temp_dir("net_match_lock");
        let engine_str = engine.to_string_lossy().into_owned();
        assert_eq!(lock_source(&root, &engine_str), root.join("Cargo.lock"));
        std::fs::write(engine.join("Cargo.lock"), "# host lock\n").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(lock_source(&root, &engine_str), engine.join("Cargo.lock"));
        let out = engine.join("proj");
        generate(&root, &out, Some(&engine_str)).unwrap_or_else(|e| panic!("{e}"));
        let lock = std::fs::read_to_string(out.join("Cargo.lock")).unwrap_or_default();
        assert_eq!(lock, "# host lock\n");
        let _ = std::fs::remove_dir_all(&engine);
    }
}
