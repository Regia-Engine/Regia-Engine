// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask editor-smoke`: launch the real editor on a throwaway copy of a sample project and
//! drive every panel, menu item, palette command and registered action it has through the agent
//! protocol, then report what broke (ADR-7865, `book/agents/editor-smoke.md`).
//!
//! The run enumerates at run time ([`inventory`]): panels from `editor.layout_get`, menu items
//! from `editor.menu_list`, commands from `editor.command`, actions and control sites from
//! `editor.actions`, methods from `agent.hello`, and the editor-method snapshot for required
//! params. A panel or action another lane adds is driven by the next run without an edit here.
//!
//! Every step checks: the editor still runs and printed no panic; no error-level log record and no
//! egui id-clash or "changed id" warning; no panel draw over the slow-frame bar (250 ms on
//! lavapipe, 50 ms on a card); a screenshot that is not one flat color; the step's own expectation
//! (the panel is in the layout and drew twice, the call answered, an undo restores the scene
//! fingerprint). It is a test tool: it changes no editor behaviour and touches nothing outside its
//! run folder (the project copy and redirected per-user folders live there).

pub mod inventory;
pub mod report;
pub mod session;
#[cfg(unix)]
pub mod xkeys;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use inventory::{Inventory, MenuItem, SnapshotMethod};
use report::{Outcome, RunResult, StepResult};
use session::{CallError, LaunchSpec, Session};

/// Arguments of `cargo xtask editor-smoke`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct EditorSmokeArgs {
    /// Run only steps whose `kind:name` matches one of these globs (`*`, `?`), e.g.
    /// `--only 'panel:*'` or `--only 'menu:File/*'`. Repeatable.
    #[arg(long)]
    pub only: Vec<String>,
    /// Leave out steps whose `kind:name` matches one of these globs. Repeatable.
    #[arg(long)]
    pub skip: Vec<String>,
    /// Run on this GPU (NVIDIA index as `nvidia-smi` numbers it, selected by UUID) instead of
    /// lavapipe. Hold `/tmp/regia-gpu-<idx>.lock`, then `/tmp/ferrite-gpu-<idx>.lock` (ADR-8670),
    /// and set `REGIA_GPU_LOCK_HELD=<idx>`.
    #[arg(long)]
    pub card: Option<u32>,
    /// Open a real window on the desktop's display instead of a private Xvfb (the Mac, the
    /// Windows rig, a Linux desktop).
    #[arg(long)]
    pub windowed: bool,
    /// Drive the headless editor (`regia-editor --headless`): no window, so no panel, keyboard,
    /// scale or theme steps; menus, commands, actions and routes run, screenshots are the
    /// offscreen viewport. For a card that cannot present to Xvfb (NVIDIA on a server).
    #[arg(long, conflicts_with = "windowed")]
    pub headless: bool,
    /// Run every step after a failure (default: stop at the first FAIL and report).
    #[arg(long)]
    pub keep_going: bool,
    /// The editor executable (default: `regia-editor` in the cargo target folder's `debug`).
    #[arg(long)]
    pub editor: Option<PathBuf>,
    /// The sample project to copy and drive.
    #[arg(long, default_value = "samples/third_person")]
    pub sample: PathBuf,
    /// Output root (default `<workspace>/target/editor-smoke`); the run writes `<root>/<timestamp>/`.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Keep the project copy and the redirected per-user folders (`work/`) after the run, to
    /// read what the steps saved.
    #[arg(long)]
    pub keep_work: bool,
    /// Print and write the inventory, drive nothing.
    #[arg(long)]
    pub inventory_only: bool,
}

/// Whether `key` (`kind:name`) is a play control: a Play menu item, a `play.*` action or command,
/// or a play-in-editor route. Those steps keep a running play session; every other step starts
/// in edit mode (ADR-8847).
fn is_play_step(key: &str) -> bool {
    let Some((kind, name)) = key.split_once(':') else {
        return false;
    };
    match kind {
        "menu" => name.starts_with("Play/"),
        "action" | "command" => name.starts_with("play."),
        "route" => {
            matches!(
                name,
                "editor.play"
                    | "editor.pause"
                    | "editor.resume"
                    | "editor.step"
                    | "editor.stop"
                    | "editor.simulate"
            ) || name.starts_with("editor.play_")
                || name.starts_with("editor.pie_")
        }
        _ => false,
    }
}

/// The level the run saves before the menus, which `editor.open_level` steps open.
const START_LEVEL: &str = "smoke-out/smoke_start.level.ron";

/// Params fixed for one method: `editor.open_level` opens the level the run saved.
fn adjust_params(method: &str, mut params: Value) -> Value {
    if method == "editor.open_level"
        && let Some(map) = params.as_object_mut()
    {
        map.insert("path".into(), json!(START_LEVEL));
    }
    params
}

/// The slow-frame bar of a run on lavapipe.
pub const LAVAPIPE_BUDGET_MS: f64 = 250.0;
/// The slow-frame bar of a run on a card.
pub const CARD_BUDGET_MS: f64 = 50.0;
/// The most editor restarts after a crash before the run gives up.
const MAX_RESTARTS: u32 = 6;
/// How long a wait for editor frames may take.
const FRAME_WAIT: Duration = Duration::from_secs(30);

/// Runs the smoke and writes its outputs.
///
/// # Errors
///
/// The run folder, the project copy or the outputs could not be written, or the editor never
/// started. Failed steps are an exit code, not an error.
pub fn run(args: &EditorSmokeArgs, workspace: &Path) -> Result<ExitCode> {
    let started = Instant::now();
    let stamp = utc_stamp(std::time::SystemTime::now());
    let root = args
        .out
        .clone()
        .unwrap_or_else(|| workspace.join("target/editor-smoke"));
    let dir = new_run_dir(&root, &stamp)?;
    let work = dir.join("work");
    std::fs::create_dir_all(dir.join("shots")).with_context(|| format!("creating {}", dir.display()))?;
    let sample = if args.sample.is_absolute() {
        args.sample.clone()
    } else {
        workspace.join(&args.sample)
    };
    if !sample.join("Regia.toml").is_file() {
        bail!("{} is not a project (no Regia.toml)", sample.display());
    }
    let project = work.join("project");
    copy_project(&sample, &project)?;
    let scratch = project.join("smoke-out");
    std::fs::create_dir_all(&scratch).with_context(|| format!("creating {}", scratch.display()))?;
    let exe = args.editor.clone().unwrap_or_else(|| default_editor(workspace));
    if !exe.is_file() {
        bail!(
            "no editor at {}: build it with `cargo build -p regia_editor --bin regia-editor` or pass --editor",
            exe.display()
        );
    }
    let (env, env_remove, mode, budget) = gpu_env(args)?;
    let mode = if args.headless {
        format!("{mode}, headless")
    } else {
        mode
    };
    let spec = LaunchSpec {
        exe: exe.clone(),
        project: project.clone(),
        work: work.clone(),
        log: dir.join("editor.log"),
        windowed: args.windowed,
        headless: args.headless,
        normal_mode: false,
        env,
        env_remove,
    };
    let command = command_line(args);
    println!(
        "editor-smoke: {} on {} ({mode}), output {}",
        exe.display(),
        sample.display(),
        dir.display()
    );
    let session = Session::start(&spec)?;
    let adapter = session
        .hello
        .get("adapter")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let snapshot = std::fs::read_to_string(workspace.join("docs/api-snapshots/editor-agent-methods.txt"))
        .map(|t| inventory::parse_method_snapshot(&t))
        .unwrap_or_default();
    let examples = inventory::examples_from_markdown(&inventory::read_agent_pages(workspace));
    let mut driver = Driver {
        session: Some(session),
        #[cfg(test)]
        test_call: None,
        spec,
        only: args.only.clone(),
        skip: args.skip.clone(),
        keep_going: args.keep_going,
        dir: dir.clone(),
        // Project-relative: the editor refuses absolute request paths.
        scratch: "smoke-out".to_owned(),
        budget,
        steps: Vec::new(),
        shots: 0,
        restarts: 0,
        stopped: false,
        fingerprint_stable: true,
        doc_players: BTreeSet::new(),
        called: BTreeSet::new(),
        snapshot,
        examples,
    };
    driver.settle();
    let inv = driver.inventory();
    if inv.menus.is_empty() && inv.commands.is_empty() && inv.actions.is_empty() {
        let why = session::last_editor_error(&driver.spec.log, 0)
            .or_else(|| session::first_panic(&driver.spec.log, 0))
            .unwrap_or_else(|| "the editor answered no menus, commands or actions".to_owned());
        if let Some(s) = driver.session.take() {
            s.stop();
        }
        bail!(
            "the editor is not usable for a smoke run: {why}; log: {}",
            driver.spec.log.display()
        );
    }
    let counts = inventory::counts(&inv, driver.snapshot.len());
    println!(
        "inventory: {} panels, {} menu items, {} commands, {} actions, {} control sites {:?}, {} methods ({} in the snapshot), {} findings",
        counts.panels,
        counts.menu_items,
        counts.commands,
        counts.actions,
        counts.sites,
        counts.sites_by_class,
        counts.methods,
        counts.snapshot_methods,
        counts.findings
    );
    report::write_atomic(
        &dir.join("inventory.json"),
        serde_json::to_string_pretty(&inv)
            .context("encoding the inventory")?
            .as_bytes(),
    )?;
    if !args.inventory_only {
        driver.drive(&inv);
    }
    let adapter = driver.adapter().unwrap_or(adapter);
    if let Some(s) = driver.session.take() {
        s.stop();
    }
    let result = RunResult {
        format_version: report::RESULT_FORMAT_VERSION,
        started: stamp,
        elapsed_s: started.elapsed().as_secs_f64(),
        editor: exe.display().to_string(),
        sample: args.sample.display().to_string(),
        mode,
        adapter,
        frame_budget_ms: budget,
        command,
        inventory: counts,
        findings: inv.findings.clone(),
        totals: report::totals(&driver.steps),
        steps: driver.steps,
    };
    report::write_all(&dir, &result)?;
    // The project copy and the redirected per-user folders are scratch: keep the outputs only.
    if !args.keep_work {
        let _ = std::fs::remove_dir_all(&work);
    }
    let t = result.totals;
    println!(
        "editor-smoke: {} PASS, {} FAIL, {} SKIP; {}/summary.md",
        t.pass,
        t.fail,
        t.skip,
        dir.display()
    );
    Ok(if t.fail > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Creates `<root>/<YYYYMMDD-HHMMSS>/`, or `…-2`, `…-3` when a run that started in the same
/// second holds the name: two runs never share a project copy or an agent endpoint.
fn new_run_dir(root: &Path, stamp: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let base = stamp.replace([':', '-'], "").replace('T', "-");
    let base = base.trim_end_matches('Z');
    for n in 1u32..1000 {
        let name = if n == 1 {
            base.to_owned()
        } else {
            format!("{base}-{n}")
        };
        let dir = root.join(name);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
        }
    }
    bail!("no free run folder name under {}", root.display())
}

/// `cargo xtask editor-smoke` plus the flags that shape the run (for reproductions).
fn command_line(args: &EditorSmokeArgs) -> String {
    let mut s = String::from("cargo xtask editor-smoke");
    if let Some(card) = args.card {
        let locks = crate::gpu_bench::flock_shell(card);
        s = format!("{locks} env REGIA_GPU_LOCK_HELD={card} {s} --card {card}");
    }
    if args.windowed {
        s.push_str(" --windowed");
    }
    if args.headless {
        s.push_str(" --headless");
    }
    if args.sample != Path::new("samples/third_person") {
        s.push_str(&format!(" --sample {}", args.sample.display()));
    }
    s
}

fn default_editor(workspace: &Path) -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    target
        .join("debug")
        .join(format!("regia-editor{}", std::env::consts::EXE_SUFFIX))
}

/// What the GPU choice sets: variables to add, variables to remove, the mode label, the bar.
type GpuEnv = (Vec<(String, String)>, Vec<String>, String, f64);

/// The environment for the GPU choice, the variables to remove, the mode label and the bar.
fn gpu_env(args: &EditorSmokeArgs) -> Result<GpuEnv> {
    let gpu_vars = [
        "REGIA_GPU_UUID",
        "REGIA_GPU_PCI",
        "REGIA_GPU_INDEX",
        "REGIA_GPU_NAME",
    ];
    if let Some(card) = args.card {
        let held = std::env::var("REGIA_GPU_LOCK_HELD").ok();
        if held.as_deref().map(str::trim) != Some(card.to_string().as_str()) {
            let locks = crate::gpu_bench::flock_shell(card);
            bail!(
                "--card {card} must hold the card's locks: {locks} env REGIA_GPU_LOCK_HELD={card} cargo xtask editor-smoke --card {card}"
            );
        }
        let mut env = vec![("REGIA_GPU_STRICT".to_owned(), "1".to_owned())];
        match nvidia_uuid(card) {
            Some(uuid) => {
                env.push(("REGIA_GPU_UUID".into(), uuid));
                env.push(("VK_LOADER_DRIVERS_SELECT".into(), "*nvidia*".into()));
            }
            None => env.push(("REGIA_GPU_INDEX".into(), card.to_string())),
        }
        let remove = gpu_vars
            .iter()
            .filter(|v| !env.iter().any(|(k, _)| k == *v))
            .map(|v| (*v).to_owned())
            .chain(std::iter::once("VK_LOADER_DRIVERS_SELECT".to_owned()).filter(|_| env.len() == 2))
            .collect();
        return Ok((env, remove, format!("card {card}"), CARD_BUDGET_MS));
    }
    if args.windowed {
        return Ok((Vec::new(), Vec::new(), "windowed".into(), CARD_BUDGET_MS));
    }
    Ok((
        vec![("VK_LOADER_DRIVERS_SELECT".into(), "*lvp*".into())],
        gpu_vars.iter().map(|v| (*v).to_owned()).collect(),
        "lavapipe".into(),
        LAVAPIPE_BUDGET_MS,
    ))
}

/// The UUID `nvidia-smi` gives card `index`.
fn nvidia_uuid(index: u32) -> Option<String> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=index,uuid", "--format=csv,noheader"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|line| {
        let (i, uuid) = line.split_once(',')?;
        (i.trim().parse::<u32>().ok()? == index).then(|| uuid.trim().to_owned())
    })
}

/// Copies the sample into the run folder (no `target`, no `.regia` state).
fn copy_project(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "target" || name == ".regia" {
            continue;
        }
        let src = entry.path();
        let dst = to.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_project(&src, &dst)?;
        } else if kind.is_file() {
            std::fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
        }
    }
    Ok(())
}

/// `YYYY-MM-DDTHH:MM:SSZ` of a system time (UTC), by the civil-from-days conversion
/// (`docs/provenance.md`, ADR-7865).
#[must_use]
pub fn utc_stamp(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (era of 400 years, March-based year).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, u32::try_from(m).unwrap_or(1), u32::try_from(d).unwrap_or(1))
}

/// The slowest panel draw of an `editor.panel_timings` reply.
#[derive(Debug, Clone, PartialEq)]
struct SlowDraw {
    panel: String,
    ms: f64,
    /// Whether it was the oldest draw of the window (the panel's first draw after the clear).
    first: bool,
    /// Median of the panel's other draws in the window, when it has any.
    median_rest: Option<f64>,
    draws: usize,
}

impl SlowDraw {
    fn context(&self) -> String {
        let when = if self.first {
            "the first draw"
        } else {
            "a later draw"
        };
        match self.median_rest {
            Some(m) => format!("{when}; median of its other {} draws {m:.0} ms", self.draws - 1),
            None => format!("{when}; no other draw recorded"),
        }
    }
}

fn slowest_draw(reply: &Value) -> Option<SlowDraw> {
    reply
        .get("panels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let panel = p.get("panel")?.as_str()?.to_owned();
            let times: Vec<f64> = p
                .get("last_ms")?
                .as_array()?
                .iter()
                .filter_map(Value::as_f64)
                .collect();
            let (at, ms) = times
                .iter()
                .copied()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(&b.1))?;
            let mut rest: Vec<f64> = times
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != at)
                .map(|(_, v)| *v)
                .collect();
            rest.sort_by(f64::total_cmp);
            let median_rest = (!rest.is_empty()).then(|| rest[rest.len() / 2]);
            Some(SlowDraw {
                panel,
                ms,
                first: at == 0,
                median_rest,
                draws: times.len(),
            })
        })
        .max_by(|a, b| a.ms.total_cmp(&b.ms))
}

/// The open level's text and its short hash.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    /// Hash of the level without its comment lines and component-less entities.
    hash: String,
    /// The text `scene.serialize` answered.
    ron: String,
    /// How many entities had no serialized component.
    empty: usize,
}

/// Routes whose `player` reply names a Sequence Curves document player: an editor-only preview
/// that the level never saves and that is no undo entry (ADR-8583). `scene.serialize` lists every
/// entity, so the level fingerprint drops these records ([`without_entities`], ADR-8702).
const DOC_PLAYER_ROUTES: [&str; 2] = ["editor.seq_new", "editor.seq_curve_open"];

/// `scene.serialize`'s text without the records of the entities in `skip` (their `// entity <id>`
/// comment and the parenthesized record under it, which closes at the same indentation).
fn without_entities(ron: &str, skip: &BTreeSet<u64>) -> String {
    if skip.is_empty() {
        return ron.to_owned();
    }
    let skipped = |line: &str| {
        line.trim_start()
            .strip_prefix("// entity ")
            .and_then(|rest| rest.split(' ').next())
            .and_then(|id| id.parse::<u64>().ok())
            .is_some_and(|id| skip.contains(&id))
    };
    let mut out = String::with_capacity(ron.len());
    let mut lines = ron.lines().peekable();
    while let Some(line) = lines.next() {
        if !skipped(line) {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        // The record: `(` on the next line, then everything up to its `),` at that indentation.
        let Some(open) = lines.next_if(|l| l.trim() == "(") else {
            continue;
        };
        let close = format!("{}),", &open[..open.len() - open.trim_start().len()]);
        for inner in lines.by_ref() {
            if inner.trim_end() == close {
                break;
            }
        }
    }
    out
}

/// The level text without `//` comment lines (entity ids) and without entities that serialize no
/// component, and the count of those. Entity ids move when an entity is despawned and spawned
/// again; a component-less entity is not level content (the leak check counts them instead).
fn normalize_level(ron: &str) -> (String, usize) {
    let lines: Vec<&str> = ron
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect();
    let mut out = String::with_capacity(ron.len());
    let mut empty = 0;
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "(" && lines.get(i + 1).is_some_and(|l| l.trim() == "),") {
            empty += 1;
            i += 2;
            continue;
        }
        out.push_str(lines[i]);
        out.push('\n');
        i += 1;
    }
    (out, empty)
}

/// Where two level texts first differ: `hash a → hash b, line N: `old` → `new``.
/// The entity ids (`Entity::to_bits`) of a `scene.serialize` text, from its `// entity <bits>` lines.
fn serialized_entities(ron: &str) -> BTreeSet<u64> {
    ron.lines()
        .filter_map(|line| line.trim_start().strip_prefix("// entity "))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter_map(|bits| bits.parse().ok())
        .collect()
}

fn fingerprint_diff(a: Option<&Fingerprint>, b: Option<&Fingerprint>) -> String {
    let (Some(a), Some(b)) = (a, b) else {
        return "the level could not be read".into();
    };
    let short = |t: &str| -> String {
        let t = t.trim();
        if t.chars().count() > 100 {
            format!("{}…", t.chars().take(100).collect::<String>())
        } else {
            t.to_owned()
        }
    };
    let (na, nb) = (normalize_level(&a.ron).0, normalize_level(&b.ron).0);
    let (mut la, mut lb) = (na.lines(), nb.lines());
    let mut n = 0usize;
    loop {
        n += 1;
        match (la.next(), lb.next()) {
            (Some(x), Some(y)) if x == y => {}
            (None, None) => return format!("{} → {}", a.hash, b.hash),
            (x, y) => {
                return format!(
                    "{} → {}, line {n}: `{}` → `{}`",
                    a.hash,
                    b.hash,
                    short(x.unwrap_or("<end>")),
                    short(y.unwrap_or("<end>"))
                );
            }
        }
    }
}

/// The result of a driven call before the shared checks.
struct Body {
    outcome: Outcome,
    reason: String,
    call: Option<Value>,
    screenshot: Option<String>,
}

impl Body {
    fn pass() -> Self {
        Self {
            outcome: Outcome::Pass,
            reason: String::new(),
            call: None,
            screenshot: None,
        }
    }
    fn fail(reason: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Fail,
            reason: reason.into(),
            call: None,
            screenshot: None,
        }
    }
    fn skip(reason: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Skip,
            reason: reason.into(),
            call: None,
            screenshot: None,
        }
    }
    fn note(&mut self, text: impl AsRef<str>) {
        if !self.reason.is_empty() {
            self.reason.push_str("; ");
        }
        self.reason.push_str(text.as_ref());
    }
}

#[cfg(test)]
type TestCall = Box<dyn FnMut(&str, Value) -> Result<Value, CallError>>;

struct Driver {
    #[cfg(test)]
    test_call: Option<TestCall>,
    session: Option<Session>,
    spec: LaunchSpec,
    only: Vec<String>,
    skip: Vec<String>,
    keep_going: bool,
    dir: PathBuf,
    scratch: String,
    budget: f64,
    steps: Vec<StepResult>,
    shots: u32,
    restarts: u32,
    stopped: bool,
    fingerprint_stable: bool,
    /// Entity ids (`Entity::to_bits`) of the Sequence Curves panel's document players that
    /// [`DOC_PLAYER_ROUTES`] answered: editor-only previews of a `.seq.ron`, never level content
    /// and never an undo entry (ADR-8583), so the level fingerprint leaves them out (ADR-8702).
    doc_players: BTreeSet<u64>,
    /// Methods a step already called (a `route` step skips them).
    called: BTreeSet<String>,
    snapshot: Vec<SnapshotMethod>,
    examples: std::collections::BTreeMap<String, Value>,
}

impl Driver {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        #[cfg(test)]
        if let Some(call) = &mut self.test_call {
            return call(method, params);
        }
        match self.session.as_mut() {
            Some(s) => s.call(method, params),
            None => Err(CallError::Gone("no editor session".into())),
        }
    }

    /// The adapter name: `agent.hello` → `adapter`, else `editor.headless_status` → `adapter.name`.
    fn adapter(&mut self) -> Option<String> {
        let hello = self
            .call("agent.hello", json!({ "client": "xtask-editor-smoke" }))
            .ok()?;
        if let Some(name) = hello.get("adapter").and_then(Value::as_str) {
            return Some(name.to_owned());
        }
        let status = self.call("editor.headless_status", json!({})).ok()?;
        status
            .pointer("/adapter/name")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// The editor's current frame (`agent.hello` → `frame`).
    fn frame(&mut self) -> Option<u64> {
        self.call("agent.hello", json!({ "client": "xtask-editor-smoke" }))
            .ok()?
            .get("frame")
            .and_then(Value::as_u64)
    }

    /// Waits until the editor drew `n` more frames: `play.wait_until` on the frame counter, else
    /// polling `agent.hello`. Returns the frame reached.
    fn wait_frames(&mut self, n: u64) -> Result<u64, String> {
        let Some(start) = self.frame() else {
            return Err("the editor does not report its frame".into());
        };
        let target = start + n;
        let waited = self.call(
            "play.wait_until",
            json!({
                "condition": { "resource": "Time", "path": "frame", "op": "gte", "value": target },
                "timeout_ms": FRAME_WAIT.as_millis() as u64,
            }),
        );
        if let Ok(reply) = &waited
            && reply.get("satisfied").and_then(Value::as_bool) == Some(true)
            && let Some(f) = self.frame()
            && f >= target
        {
            return Ok(f);
        }
        let deadline = Instant::now() + FRAME_WAIT;
        while Instant::now() < deadline {
            match self.frame() {
                Some(f) if f >= target => return Ok(f),
                Some(_) => std::thread::sleep(Duration::from_millis(15)),
                None => return Err("the editor stopped answering while waiting for frames".into()),
            }
        }
        Err(format!(
            "the editor drew fewer than {n} frames in {} s",
            FRAME_WAIT.as_secs()
        ))
    }

    /// Back to the built-in layout with both drawers closed. `editor.layout_reset` leaves a drawer
    /// open (drawers are not part of the dock), and an open drawer covers the bottom of the dock
    /// in every later screenshot (ADR-8702: `scale200:inspector` cropped the content drawer that
    /// `command:view.content_drawer` had opened).
    fn reset_layout(&mut self) {
        let _ = self.call("editor.layout_reset", json!({}));
        for drawer in ["content", "log"] {
            let _ = self.call("editor.drawer", json!({ "drawer": drawer, "open": false }));
        }
    }

    /// Back to the built-in layout, two frames drawn, old log lines consumed.
    fn settle(&mut self) {
        self.reset_layout();
        let _ = self.wait_frames(3);
        if let Some(s) = self.session.as_mut() {
            s.skip_logs();
            let _ = s.new_panics();
        }
        let _ = self.call("editor.panel_timings", json!({ "last": 1, "clear": true }));
    }

    fn inventory(&mut self) -> Inventory {
        // The layout exists once the window drew a frame.
        let mut layout = Value::Null;
        for _ in 0..20 {
            if let Ok(reply) = self.call("editor.layout_get", json!({}))
                && !reply.get("layout").is_none_or(Value::is_null)
            {
                layout = reply;
                break;
            }
            let _ = self.wait_frames(1);
        }
        let menus = self.call("editor.menu_list", json!({})).unwrap_or(Value::Null);
        let commands = self
            .call("editor.command", json!({ "limit": 200 }))
            .unwrap_or(Value::Null);
        let actions = self.call("editor.actions", json!({})).unwrap_or(Value::Null);
        let methods = self
            .session
            .as_ref()
            .map(|s| inventory::methods_from_hello(&s.hello))
            .unwrap_or_default();
        let (actions, sites) = inventory::actions_from_reply(&actions);
        let mut inv = Inventory {
            panels: inventory::panels_from_layout(&layout),
            menus: inventory::menus_from_list(&menus),
            commands: inventory::commands_from_list(&commands),
            actions,
            sites,
            methods,
            findings: Vec::new(),
        };
        inv.findings = inventory::findings(&inv);
        inv
    }

    fn selected(&self, key: &str) -> bool {
        inventory::selected(key, &self.only, &self.skip)
    }

    /// Whether the editor process runs (the regression fixtures' scripted editor always does).
    fn session_alive(&mut self) -> bool {
        #[cfg(test)]
        if self.test_call.is_some() {
            return true;
        }
        self.session.as_mut().is_some_and(Session::alive)
    }

    /// Restarts a dead editor (same project copy). False when the run must stop.
    fn ensure_session(&mut self) -> bool {
        // Alive and answering: a process that is still writing its crash report after a panic is
        // alive but has dropped the connection.
        let alive = self.session_alive()
            && self
                .call("agent.hello", json!({ "client": "xtask-editor-smoke" }))
                .is_ok();
        if alive {
            return true;
        }
        if let Some(old) = self.session.take() {
            old.stop();
        }
        if self.restarts >= MAX_RESTARTS {
            return false;
        }
        self.restarts += 1;
        // A new editor process numbers its entities afresh.
        self.doc_players.clear();
        // A harness restart is the user's "restart normally": without it, the second crash in a
        // row starts the editor in safe mode, which never opens the agent listener.
        let mut spec = self.spec.clone();
        spec.normal_mode = true;
        match Session::start(&spec) {
            Ok(s) => {
                self.session = Some(s);
                self.settle();
                true
            }
            Err(err) => {
                eprintln!("editor-smoke: restart failed: {err:#}");
                false
            }
        }
    }

    /// Takes a screenshot of the editor window into `shots/`; the relative path and whether it
    /// is one flat color.
    fn screenshot_region(&mut self, key: &str, panel: Option<&str>) -> Result<(String, bool), String> {
        let layout = if panel.is_some() {
            self.call("editor.layout_get", json!({}))
                .map_err(|e| format!("editor.layout_get for panel screenshot failed: {e}"))?
        } else {
            Value::Null
        };
        let reply = self
            .call("seeing.screenshot", json!({ "timeout_ms": 30_000 }))
            .map_err(|e| format!("seeing.screenshot failed: {e}"))?;
        let b64 = reply
            .get("png_base64")
            .and_then(Value::as_str)
            .ok_or("seeing.screenshot answered no png_base64")?;
        let bytes = session::base64_decode(b64).ok_or("seeing.screenshot answered bad base64")?;
        let img = image::load_from_memory(&bytes)
            .map_err(|e| format!("screenshot PNG does not decode: {e}"))?
            .to_rgba8();
        let img = match panel {
            Some(id) => report::panel_image(&img, &layout, id)?,
            None => img,
        };
        let uniform = report::is_uniform(img.as_raw(), 2);
        self.shots += 1;
        let safe: String = key
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .take(80)
            .collect();
        let rel = format!("shots/{:04}-{safe}.png", self.shots);
        img.save(self.dir.join(&rel))
            .map_err(|e| format!("writing {rel}: {e}"))?;
        Ok((rel, uniform))
    }

    /// The open level as `scene.serialize` writes it, with its short hash; `None` when it cannot
    /// be read.
    fn fingerprint(&mut self) -> Option<Fingerprint> {
        let reply = self.call("scene.serialize", json!({})).ok()?;
        let ron = without_entities(reply.get("ron").and_then(Value::as_str)?, &self.doc_players);
        let (normal, empty) = normalize_level(&ron);
        Some(Fingerprint {
            hash: blake3::hash(normal.as_bytes()).to_hex()[..16].to_owned(),
            ron,
            empty,
        })
    }

    /// Writes the two level texts under `levels/` for the report; returns ` (levels/…)` or empty.
    fn save_level_pair(&mut self, key: &str, a: Option<&Fingerprint>, b: Option<&Fingerprint>) -> String {
        let (Some(a), Some(b)) = (a, b) else {
            return String::new();
        };
        let dir = self.dir.join("levels");
        if std::fs::create_dir_all(&dir).is_err() {
            return String::new();
        }
        self.shots += 1;
        let safe: String = key
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .take(80)
            .collect();
        let stem = format!("levels/{:04}-{safe}", self.shots);
        let ok = std::fs::write(self.dir.join(format!("{stem}.before.ron")), &a.ron).is_ok()
            && std::fs::write(self.dir.join(format!("{stem}.after.ron")), &b.ron).is_ok();
        if ok {
            format!(" ({stem}.before.ron / .after.ron)")
        } else {
            String::new()
        }
    }

    /// For a failed undo: every component (reflected or not, `scene.get_entity`) of the entities
    /// the undo left behind (in `after`, not in `before`), so the report names what they are even
    /// when `scene.serialize` shows only their reflected parts (ADR-8847). Empty when none.
    fn leftover_components(&mut self, before: Option<&Fingerprint>, after: Option<&Fingerprint>) -> String {
        let (Some(before), Some(after)) = (before, after) else {
            return String::new();
        };
        let had = serialized_entities(&before.ron);
        let mut notes = Vec::new();
        for bits in serialized_entities(&after.ron).difference(&had) {
            let Ok(reply) = self.call("scene.get_entity", json!({ "entity": bits })) else {
                continue;
            };
            let mut types: Vec<String> = reply
                .get("components")
                .and_then(Value::as_object)
                .map(|components| {
                    components
                        .iter()
                        .map(|(short, c)| c.get("type").and_then(Value::as_str).unwrap_or(short).to_owned())
                        .collect()
                })
                .unwrap_or_default();
            types.sort();
            notes.push(format!("entity {bits} left behind: {}", types.join(", ")));
        }
        notes.join("; ")
    }

    fn undo_depth(&mut self) -> Option<u64> {
        self.call("editor.history", json!({}))
            .ok()?
            .get("undo_depth")
            .and_then(Value::as_u64)
    }

    /// Runs one step with the shared checks around `body`.
    fn step(&mut self, kind: &str, name: &str, area: &str, body: impl FnOnce(&mut Self) -> Body) {
        if self.stopped {
            return;
        }
        let key = format!("{kind}:{name}");
        if !self.selected(&key) {
            return;
        }
        if !self.ensure_session() {
            self.stopped = true;
            self.steps.push(StepResult {
                area: area.into(),
                kind: kind.into(),
                name: name.into(),
                outcome: Outcome::Fail,
                reason: "the editor is gone and could not be restarted".into(),
                time_ms: 0.0,
                frame_ms: None,
                call: None,
                log_excerpt: Vec::new(),
                screenshot: None,
            });
            return;
        }
        // A play session an earlier step left running (a Play menu item, `play.start_*`) would put
        // its view under this step's screenshot: a step that is not a play control starts in edit
        // mode (ADR-8847). Play controls keep the session, so Pause after Play still finds it.
        let left_running = if is_play_step(&key) {
            None
        } else {
            self.end_leftover_play()
        };
        let _ = self.call("editor.panel_timings", json!({ "last": 1, "clear": true }));
        let t0 = Instant::now();
        let mut body = body(self);
        match left_running {
            Some(Ok(note)) => body.note(note),
            Some(Err(why)) => {
                body.outcome = Outcome::Fail;
                body.note(why);
            }
            None => {}
        }
        let time_ms = t0.elapsed().as_secs_f64() * 1000.0;
        // Said, not excused: a play control's flat shot still fails, and the reason names the
        // play view it looked through (headless captures show the possessed game cameras).
        if is_play_step(&key)
            && body.outcome == Outcome::Fail
            && body.reason.contains("one flat color")
            && let Some((mode, players)) = self.play_state().filter(|(mode, _)| mode != "stopped")
        {
            body.note(format!(
                "the shot is the play view ({mode}, {players} player{}; headless captures look through the possessed game cameras)",
                if players == 1 { "" } else { "s" }
            ));
        }
        let mut log_excerpt = Vec::new();
        let mut frame_ms = None;
        let alive = self.session_alive();
        let panics = self.session.as_mut().map(Session::new_panics).unwrap_or_default();
        if !panics.is_empty() {
            body.outcome = Outcome::Fail;
            body.note(format!("panic: {}", panics[0]));
            log_excerpt.extend(panics.iter().take(3).cloned());
        }
        if !alive {
            body.outcome = Outcome::Fail;
            body.note("the editor exited");
        } else {
            let records = self
                .session
                .as_mut()
                .map(Session::new_log_records)
                .unwrap_or_default();
            let mut bad = Vec::new();
            for (level, line) in records {
                let clash = line.contains("changed id between passes")
                    || line.contains("Double use of")
                    || line.contains("First use of")
                    || line.to_lowercase().contains("id clash");
                if level.eq_ignore_ascii_case("error") || clash {
                    bad.push(format!("{level} {line}"));
                }
            }
            if !bad.is_empty() {
                body.outcome = Outcome::Fail;
                body.note(format!("{} error or id-clash log line(s)", bad.len()));
                log_excerpt.extend(bad.into_iter().take(5));
            }
            if let Ok(t) = self.call("editor.panel_timings", json!({ "last": 240 }))
                && let Some(slow) = slowest_draw(&t)
            {
                frame_ms = Some(slow.ms);
                if slow.ms > self.budget {
                    body.outcome = Outcome::Fail;
                    body.note(format!(
                        "slow frame: panel \"{}\" drew in {:.0} ms (bar {} ms; {})",
                        slow.panel,
                        slow.ms,
                        self.budget,
                        slow.context()
                    ));
                }
            }
        }
        let failed = body.outcome == Outcome::Fail;
        println!("{:4} {key} {}", body.outcome.label(), body.reason);
        self.steps.push(StepResult {
            area: area.into(),
            kind: kind.into(),
            name: name.into(),
            outcome: body.outcome,
            reason: body.reason,
            time_ms,
            frame_ms,
            call: body.call,
            log_excerpt,
            screenshot: body.screenshot,
        });
        if failed && !self.keep_going {
            self.stopped = true;
        }
    }

    fn drive(&mut self, inv: &Inventory) {
        // Fingerprint stability: idle reads must agree for undo-restoration comparisons.
        let a = self.fingerprint();
        let _ = self.wait_frames(3);
        let b = self.fingerprint();
        self.fingerprint_stable = a.is_some() && a.as_ref().map(|f| &f.hash) == b.as_ref().map(|f| &f.hash);
        let empty_at_start = b.as_ref().map_or(0, |f| f.empty);
        if !self.fingerprint_stable {
            println!(
                "editor-smoke: the level fingerprint is not stable over three idle frames ({}); undo-restoration comparisons are off",
                fingerprint_diff(a.as_ref(), b.as_ref())
            );
        }
        for panel in &inv.panels {
            let id = panel.id.clone();
            self.step("panel", &id, "panels", |d| d.panel_body(&id));
        }
        // A saved level for File ▸ Open level… to open (inside the project copy).
        let _ = self.call("editor.save_level", json!({ "path": START_LEVEL }));
        let mut covered: BTreeSet<String> = BTreeSet::new();
        for item in &inv.menus {
            covered.insert(item.action.clone());
            if let Some(run) = item.params.get("run").and_then(Value::as_str) {
                covered.insert(run.to_owned());
            }
            let area = if item.path.ends_with('…') || matches!(item.class.as_str(), "os_dialog" | "confirm")
            {
                "dialogs"
            } else {
                "menus"
            };
            let item = item.clone();
            self.step("menu", &item.path, area, |d| d.menu_body(&item));
        }
        for cmd in &inv.commands {
            if covered.contains(&cmd.id) {
                continue;
            }
            covered.insert(cmd.id.clone());
            let id = cmd.id.clone();
            let area = if id.starts_with("window.") {
                "dialogs"
            } else {
                "other"
            };
            self.step("command", &id, area, |d| {
                let params = json!({ "run": id });
                d.mutating_body(
                    "editor.command",
                    params,
                    &format!("command:{id}"),
                    area != "other",
                )
            });
        }
        for action in &inv.actions {
            if covered.contains(&action.id) || action.command.as_ref().is_some_and(|c| covered.contains(c)) {
                continue;
            }
            let action = action.clone();
            self.step("action", &action.id, "other", |d| d.action_body(&action));
        }
        // The in-panel controls: every distinct method an allowlist site names that no step
        // above called, once, with the guide's example params.
        let site_methods: BTreeSet<(String, bool)> = inv
            .sites
            .iter()
            .filter(|s| matches!(s.class.as_str(), "method" | "confirm" | "os_dialog"))
            .filter_map(|s| s.method.clone().map(|m| (m, s.class == "confirm")))
            .collect();
        for (method, confirm) in site_methods {
            if self.called.contains(&method)
                || self.steps.iter().any(|s| s.kind == "route" && s.name == method)
            {
                continue;
            }
            self.step("route", &method, "other", |d| d.route_body(&method, confirm));
        }
        // Entities that serialize no component must not pile up while the UI is used.
        self.step("check", "empty_entities", "other", |d| {
            d.reset_layout();
            let _ = d.wait_frames(3);
            match d.fingerprint() {
                Some(f) if f.empty > empty_at_start => Body::fail(format!(
                    "component-less entities in the level grew from {empty_at_start} to {} over the run (an entity leak; they are listed by scene.serialize)",
                    f.empty
                )),
                Some(f) => {
                    let mut b = Body::pass();
                    b.note(format!("{} component-less entities at the start and at the end", f.empty));
                    b
                }
                None => Body::fail("scene.serialize did not answer"),
            }
        });
        self.end_play();
        self.keyboard_pass();
        self.preference_pass(
            "scale200",
            json!({ "appearance.ui_scale": 2.0 }),
            &["appearance.ui_scale"],
            &inv.panels,
        );
        self.preference_pass(
            "light",
            json!({ "appearance.theme": "regia-light" }),
            &["appearance.theme"],
            &inv.panels,
        );
    }

    /// The play-in-editor state (`editor.play_state`): `(mode, players)`; `None` when the editor
    /// does not answer it.
    fn play_state(&mut self) -> Option<(String, u64)> {
        let reply = self.call("editor.play_state", json!({})).ok()?;
        let mode = reply.get("mode").and_then(Value::as_str)?.to_owned();
        let players = reply.get("players").and_then(Value::as_u64).unwrap_or(1);
        Some((mode, players))
    }

    /// Ends a play session an earlier step left running, before a step that is not a play
    /// control. `None` when nothing plays (or the editor has no `editor.play_state`); otherwise
    /// the step's note, or why it fails when the session did not stop.
    fn end_leftover_play(&mut self) -> Option<Result<String, String>> {
        let (mode, players) = self.play_state().filter(|(mode, _)| mode != "stopped")?;
        self.end_play();
        let after = self
            .play_state()
            .map_or_else(|| "no answer".to_owned(), |(mode, _)| mode);
        Some(if after == "stopped" {
            Ok(format!(
                "stopped the play session an earlier step left running ({mode}, {players} player{})",
                if players == 1 { "" } else { "s" }
            ))
        } else {
            Err(format!(
                "a play session an earlier step left running did not stop (editor.play_state after editor.stop: {after})"
            ))
        })
    }

    /// Stops a play session an earlier step left running (the `play.start_*` actions, the
    /// Play menu), so the keyboard, 200 % and light passes look at the edit-mode UI, as the
    /// first panel pass did. A refusal (nothing playing) is fine.
    fn end_play(&mut self) {
        let _ = self.call("editor.stop", json!({}));
        let _ = self.wait_frames(3);
    }

    fn panel_body(&mut self, id: &str) -> Body {
        let call = json!({ "method": "editor.layout_apply", "params": { "open": id } });
        if let Err(e) = self.call("editor.layout_apply", json!({ "open": id })) {
            let mut b = Body::fail(format!("editor.layout_apply refused a listed panel: {e}"));
            b.call = Some(call);
            return b;
        }
        let mut body = Body::pass();
        body.call = Some(call);
        let opened = match self.wait_frames(2) {
            Ok(f) => f,
            Err(e) => {
                return Body {
                    reason: e,
                    outcome: Outcome::Fail,
                    ..body
                };
            }
        };
        let shown = self
            .call("editor.layout_get", json!({}))
            .ok()
            .and_then(|r| r.get("panels").cloned())
            .and_then(|p| p.as_array().cloned())
            .unwrap_or_default();
        if !shown.iter().any(|p| p.as_str() == Some(id)) {
            body.outcome = Outcome::Fail;
            body.note("the panel is not in the layout after editor.layout_apply { open }");
        }
        if let Err(e) = self.wait_frames(2) {
            body.outcome = Outcome::Fail;
            body.note(e);
        }
        // The step's own expectation, through the protocol's `expect`: two frames drew after the
        // panel came forward.
        match self.call(
            "play.expect",
            json!({ "assertions": [{ "name": "panel drew twice", "condition": { "resource": "Time", "path": "frame", "op": "gte", "value": opened + 2 } }] }),
        ) {
            Ok(r) if r.get("pass").and_then(Value::as_bool) == Some(true) => {}
            Ok(r) => {
                body.outcome = Outcome::Fail;
                body.note(format!("play.expect did not return pass == true: {r}"));
            }
            Err(e) => {
                body.outcome = Outcome::Fail;
                body.note(format!("play.expect failed: {e}"));
            }
        }
        self.attach_screenshot_region(&format!("panel:{id}"), Some(id), &mut body);
        self.reset_layout();
        let _ = self.wait_frames(1);
        body
    }

    fn attach_screenshot(&mut self, key: &str, body: &mut Body) {
        self.attach_screenshot_region(key, None, body);
    }

    fn attach_screenshot_region(&mut self, key: &str, panel: Option<&str>, body: &mut Body) {
        match self.screenshot_region(key, panel) {
            Ok((path, uniform)) => {
                if uniform {
                    body.outcome = Outcome::Fail;
                    body.note("the screenshot is one flat color");
                }
                body.screenshot = Some(path);
            }
            Err(e) => {
                body.outcome = Outcome::Fail;
                body.note(e);
            }
        }
    }

    fn menu_body(&mut self, item: &MenuItem) -> Body {
        let text = format!(
            "menu:{} {} {} {}",
            item.path, item.action, item.method, item.params
        );
        if let Some(reason) = inventory::skip_reason(&text) {
            return Body::skip(reason);
        }
        if item.method.is_empty() {
            return Body::fail("the menu model names no agent method for this item");
        }
        let params = adjust_params(
            &item.method,
            inventory::safe_params(&item.method, Some(&item.params), &self.examples, &self.scratch),
        );
        let shot = true;
        let mut body = self.mutating_body(&item.method.clone(), params, &format!("menu:{}", item.path), shot);
        if !item.enabled && body.outcome == Outcome::Skip {
            body.note("the item was disabled");
        }
        body
    }

    fn action_body(&mut self, action: &inventory::ActionItem) -> Body {
        if action.class == "view_only" {
            return Body::skip("view only: scroll, fold or layout, no state an agent needs");
        }
        let Some(method) = action
            .method
            .clone()
            .or_else(|| action.command.as_ref().map(|_| "editor.command".to_owned()))
        else {
            return Body::skip(format!("class {} names no method", action.class));
        };
        let text = format!(
            "action:{} {} {} {}",
            action.id, action.label, action.panel, method
        );
        if let Some(reason) = inventory::skip_reason(&text) {
            return Body::skip(reason);
        }
        let given = action.command.as_ref().map(|c| json!({ "run": c }));
        let mut params = adjust_params(
            &method,
            inventory::safe_params(&method, given.as_ref(), &self.examples, &self.scratch),
        );
        if action.class == "confirm"
            && let Some(map) = params.as_object_mut()
        {
            map.entry("confirm").or_insert(json!(true));
        }
        if let Some(m) = self.snapshot.iter().find(|m| m.name == method) {
            let missing: Vec<&String> = m
                .required
                .iter()
                .filter(|r| params.get(r.as_str()).is_none())
                .collect();
            if !missing.is_empty() {
                return Body::skip(format!(
                    "`{method}` needs {missing:?} and the agent guide has no example to take them from"
                ));
            }
        }
        self.mutating_body(&method, params, &format!("action:{}", action.id), false)
    }

    fn route_body(&mut self, method: &str, confirm: bool) -> Body {
        if let Some(reason) = inventory::skip_reason(&format!("route:{method}")) {
            return Body::skip(reason);
        }
        let mut params = adjust_params(
            method,
            inventory::safe_params(method, None, &self.examples, &self.scratch),
        );
        if confirm && let Some(map) = params.as_object_mut() {
            map.entry("confirm").or_insert(json!(true));
        }
        if let Some(m) = self.snapshot.iter().find(|m| m.name == method) {
            let missing: Vec<&String> = m
                .required
                .iter()
                .filter(|r| params.get(r.as_str()).is_none())
                .collect();
            if !missing.is_empty() {
                return Body::skip(format!(
                    "`{method}` needs {missing:?} and the agent guide has no example to take them from"
                ));
            }
        }
        self.mutating_body(method, params, &format!("route:{method}"), false)
    }

    /// Calls a method that may change the level: record the undo depth and the scene
    /// fingerprint, call, let two frames draw, then undo what the call added and check the
    /// fingerprint is back.
    fn mutating_body(&mut self, method: &str, params: Value, key: &str, shot: bool) -> Body {
        let call = json!({ "method": method, "params": params.clone() });
        self.called.insert(method.to_owned());
        let depth0 = self.undo_depth();
        let fp0 = self.fingerprint();
        let reply = self.call(method, params);
        let mut body = Body::pass();
        body.call = Some(call);
        match reply {
            Ok(r) => {
                if DOC_PLAYER_ROUTES.contains(&method)
                    && let Some(player) = r.get("player").and_then(Value::as_u64)
                {
                    self.doc_players.insert(player);
                }
                if method == "editor.command" && r.get("ran").is_some_and(Value::is_null) {
                    return Body {
                        outcome: Outcome::Skip,
                        reason: format!(
                            "refused: editor.command did not run it ({})",
                            r.get("message").cloned().unwrap_or_default()
                        ),
                        ..body
                    };
                }
            }
            Err(CallError::Rpc(code, message, data)) => {
                let typed = data
                    .get("code")
                    .or_else(|| data.get("reason"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                return if code == -32601 {
                    Body {
                        outcome: Outcome::Fail,
                        reason: format!("route missing: {message}"),
                        ..body
                    }
                } else if code == -32602 || typed.is_some() {
                    Body {
                        outcome: Outcome::Skip,
                        reason: format!(
                            "refused ({}): {message}",
                            typed.unwrap_or_else(|| code.to_string())
                        ),
                        ..body
                    }
                } else {
                    Body {
                        outcome: Outcome::Fail,
                        reason: format!("error {code}: {message} {data}"),
                        ..body
                    }
                };
            }
            Err(e) => {
                return Body {
                    outcome: Outcome::Fail,
                    reason: e.to_string(),
                    ..body
                };
            }
        }
        if let Err(e) = self.wait_frames(2) {
            body.outcome = Outcome::Fail;
            body.note(e);
            return body;
        }
        if shot {
            self.attach_screenshot(key, &mut body);
        }
        let depth1 = self.undo_depth();
        let fp1 = self.fingerprint();
        match (depth0, depth1) {
            (Some(a), Some(b)) if b > a => {
                for _ in a..b {
                    let _ = self.call("editor.undo", json!({}));
                }
                let _ = self.wait_frames(2);
                let fp2 = self.fingerprint();
                if self.fingerprint_stable && fp2.as_ref().map(|f| &f.hash) != fp0.as_ref().map(|f| &f.hash) {
                    let saved = self.save_level_pair(key, fp0.as_ref(), fp2.as_ref());
                    body.note(saved.trim_start_matches(" (").trim_end_matches(')'));
                    let leftover = self.leftover_components(fp0.as_ref(), fp2.as_ref());
                    if !leftover.is_empty() {
                        body.note(leftover);
                    }
                    body.outcome = Outcome::Fail;
                    body.note(format!(
                        "undo of {} entr{} did not restore the level: {}",
                        b - a,
                        if b - a == 1 { "y" } else { "ies" },
                        fingerprint_diff(fp0.as_ref(), fp2.as_ref())
                    ));
                } else {
                    body.note(format!(
                        "undone ({} entr{})",
                        b - a,
                        if b - a == 1 { "y" } else { "ies" }
                    ));
                }
            }
            _ => {
                if fp1.as_ref().map(|f| &f.hash) != fp0.as_ref().map(|f| &f.hash) {
                    body.outcome = Outcome::Fail;
                    let saved = self.save_level_pair(key, fp0.as_ref(), fp1.as_ref());
                    body.note(format!(
                        "changed the level without an undo entry: {}{saved}",
                        fingerprint_diff(fp0.as_ref(), fp1.as_ref())
                    ));
                }
            }
        }
        self.reset_layout();
        body
    }

    /// The keyboard-only pass: real key presses through XTEST on the private Xvfb. Tab and the
    /// arrows must change the focused widget id, Ctrl+Shift+P must open the palette
    /// (`editor.command` → `open`), and typing a query plus Enter must run a command (`recent`).
    fn keyboard_pass(&mut self) {
        let keys = ["tab_focus", "arrow_focus", "palette_open_run"];
        if self.stopped || !keys.iter().any(|k| self.selected(&format!("keyboard:{k}"))) {
            return;
        }
        let display = self.session.as_ref().and_then(|s| s.display.clone());
        let Some(display) = display else {
            let why = if self.spec.headless {
                "--headless: the editor has no window to type into"
            } else {
                "--windowed: key presses would go to the desktop; run the pass on Xvfb"
            };
            for name in keys {
                self.step("keyboard", name, "other", |_| Body::skip(why));
            }
            return;
        };
        self.keyboard_steps(&display);
    }

    #[cfg(not(unix))]
    fn keyboard_steps(&mut self, _display: &str) {
        for name in ["tab_focus", "arrow_focus", "palette_open_run"] {
            self.step("keyboard", name, "other", |_| {
                Body::skip("key injection needs an X display (Unix)")
            });
        }
    }

    #[cfg(unix)]
    fn keyboard_steps(&mut self, display: &str) {
        use xkeys::keysym;
        let mut x = match xkeys::XKeys::connect(display).and_then(|mut x| x.focus_editor().map(|()| x)) {
            Ok(x) => x,
            Err(e) => {
                for name in ["tab_focus", "arrow_focus", "palette_open_run"] {
                    let e = e.clone();
                    self.step("keyboard", name, "other", move |_| {
                        Body::fail(format!("XTEST: {e}"))
                    });
                }
                return;
            }
        };
        self.reset_layout();
        let _ = self.wait_frames(2);
        for (name, key) in [("tab_focus", keysym::TAB), ("arrow_focus", keysym::DOWN)] {
            self.step("keyboard", name, "other", |d| {
                d.focus_body(name, key, || x.chord(&[], key))
            });
        }
        let _ = x.chord(&[], keysym::ESCAPE);
        self.step("keyboard", "palette_open_run", "dialogs", |d| {
            let mut body = Body::pass();
            body.call = Some(json!({ "keys": "Ctrl+Shift+P, \"toggle grid\", Return via XTEST" }));
            if let Err(e) = x.chord(&[keysym::CONTROL_L, keysym::SHIFT_L], u32::from('p')) {
                return Body::fail(format!("XTEST: {e}"));
            }
            let _ = d.wait_frames(3);
            let open = d
                .call("editor.command", json!({ "limit": 1 }))
                .ok()
                .and_then(|r| r.get("open").and_then(Value::as_bool));
            if open != Some(true) {
                body.outcome = Outcome::Fail;
                body.note("Ctrl+Shift+P did not open the command palette");
                return body;
            }
            d.attach_screenshot("keyboard:palette_open", &mut body);
            let typed = x.type_text("togglegrid");
            let _ = d.wait_frames(2);
            // What Return should run: the palette's best match for the typed text (recently run
            // commands rank higher, so this is asked, not assumed).
            let expected = d
                .call("editor.command", json!({ "query": "togglegrid", "limit": 1 }))
                .ok()
                .and_then(|r| r.pointer("/commands/0/id").and_then(Value::as_str).map(str::to_owned));
            let entered = typed.and_then(|()| x.chord(&[], keysym::RETURN));
            if let Err(e) = entered {
                return Body::fail(format!("XTEST: {e}"));
            }
            let _ = d.wait_frames(3);
            let reply = d.call("editor.command", json!({ "limit": 1 })).unwrap_or(Value::Null);
            let ran = reply
                .get("recent")
                .and_then(Value::as_array)
                .and_then(|r| r.first())
                .and_then(Value::as_str)
                .map(str::to_owned);
            match (&expected, &ran) {
                (Some(want), Some(got)) if want == got => {
                    body.note(format!("Return ran {got}"));
                    // Every match of "togglegrid" is a toggle: run it again to put it back.
                    let _ = d.call("editor.command", json!({ "run": got }));
                }
                _ => {
                    body.outcome = Outcome::Fail;
                    body.note(format!(
                        "typing \"togglegrid\" and Return did not run the palette's best match {expected:?} (most recent: {ran:?})"
                    ));
                }
            }
            if reply.get("open").and_then(Value::as_bool) == Some(true) {
                let _ = x.chord(&[], keysym::ESCAPE);
            }
            body
        });
        let _ = x.chord(&[], keysym::UP);
    }

    #[cfg(unix)]
    fn focus_body(&mut self, name: &str, key: u32, mut press: impl FnMut() -> Result<(), String>) -> Body {
        let before = match self.focused_widget() {
            Ok(id) => id,
            Err(e) => return Body::fail(e),
        };
        let mut body = Body::pass();
        body.call = Some(json!({ "keys": format!("{key:#x} x3 via XTEST") }));
        for _ in 0..3 {
            if let Err(e) = press() {
                return Body::fail(format!("XTEST: {e}"));
            }
            if let Err(e) = self.wait_frames(2) {
                return Body::fail(e);
            }
        }
        self.attach_screenshot(&format!("keyboard:{name}"), &mut body);
        match self.focused_widget() {
            Ok(after) if after.is_some() && after != before => {
                body.note(format!("focus moved: {before:?} → {after:?}"));
            }
            Ok(after) => {
                body.outcome = Outcome::Fail;
                body.note(format!(
                    "three key presses did not move focus to another widget: {before:?} → {after:?}"
                ));
            }
            Err(e) => {
                body.outcome = Outcome::Fail;
                body.note(e);
            }
        }
        body
    }

    #[cfg(unix)]
    fn focused_widget(&mut self) -> Result<Option<String>, String> {
        let reply = self
            .call("editor.layout_get", json!({}))
            .map_err(|e| format!("reading focused widget failed: {e}"))?;
        match reply.get("focused_widget") {
            Some(Value::Null) => Ok(None),
            Some(Value::String(id)) if !id.is_empty() => Ok(Some(id.clone())),
            _ => Err("editor.layout_get has no valid focused_widget observation".into()),
        }
    }

    /// Sets preferences, screenshots the main layout and every panel under them, then resets them.
    /// A preference that changes nothing on screen is a broken preference: the panel's crop must
    /// differ from its crop in the first panel pass (same panel, same window, default
    /// preferences). Pixel-identical crops fail the step; a missing earlier crop says nothing.
    fn require_change_from_normal(&self, id: &str, body: &mut Body) {
        let Some(shot) = body.screenshot.clone() else {
            return;
        };
        let Some(normal) = self
            .steps
            .iter()
            .find(|s| s.kind == "panel" && s.name == id)
            .and_then(|s| s.screenshot.clone())
        else {
            return;
        };
        if report::same_pixels(&self.dir.join(&normal), &self.dir.join(&shot)) {
            body.outcome = Outcome::Fail;
            body.note(format!(
                "the preference did not change the panel: its crop is pixel-identical to {normal}"
            ));
        }
    }

    fn preference_pass(&mut self, kind: &str, values: Value, keys: &[&str], panels: &[inventory::PanelItem]) {
        if self.spec.headless {
            if self.selected(&format!("{kind}:main")) {
                self.step(kind, "main", kind, |_| {
                    Body::skip("--headless: no editor UI to scale or theme")
                });
            }
            return;
        }
        let any = std::iter::once(format!("{kind}:main"))
            .chain(panels.iter().map(|p| format!("{kind}:{}", p.id)))
            .any(|k| self.selected(&k));
        if !any || self.stopped {
            return;
        }
        let set = self.call("editor.prefs_set", json!({ "values": values }));
        self.step(kind, "main", kind, |d| {
            let mut body = match &set {
                Ok(_) => Body::pass(),
                Err(e) => return Body::fail(format!("editor.prefs_set refused: {e}")),
            };
            body.call = Some(json!({ "method": "editor.prefs_set", "params": { "values": values } }));
            d.reset_layout();
            if let Err(e) = d.wait_frames(3) {
                body.outcome = Outcome::Fail;
                body.note(e);
            }
            d.attach_screenshot(&format!("{kind}:main"), &mut body);
            body
        });
        if set.is_ok() {
            for panel in panels {
                let id = panel.id.clone();
                self.step(kind, &id, kind, |d| {
                    let mut body = d.panel_body(&id);
                    d.require_change_from_normal(&id, &mut body);
                    body
                });
            }
        }
        let _ = self.call("editor.prefs_reset", json!({ "keys": keys }));
        let _ = self.wait_frames(2);
    }
}

#[cfg(test)]
mod regressions;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slowest_draw_says_whether_it_was_the_first() {
        let reply = json!({ "keep": 240, "panels": [
            { "panel": "Viewport", "draws": 3, "last_ms": [5.0, 6.0, 4.0], "median_ms": 5.0, "max_ms": 6.0 },
            { "panel": "Material Graph", "draws": 3, "last_ms": [900.0, 10.0, 12.0], "median_ms": 12.0, "max_ms": 900.0 }
        ]});
        let slow = slowest_draw(&reply).expect("a draw");
        assert_eq!(slow.panel, "Material Graph");
        assert!(slow.first);
        assert_eq!(slow.median_rest, Some(12.0));
        assert_eq!(
            slow.context(),
            "the first draw; median of its other 2 draws 12 ms"
        );
        assert!(slowest_draw(&json!({ "panels": [] })).is_none());
    }

    #[test]
    fn normalizing_drops_ids_and_empty_entities() {
        let a = "(\n    entities: [\n        // entity 4294966940 \"\"\n        (\n        ),\n        // entity 7 \"Box\"\n        (\n            components: {},\n        ),\n    ],\n)";
        let b = a
            .replace("4294966940", "4294966937")
            .replace("// entity 7", "// entity 9");
        let (na, ea) = normalize_level(a);
        let (nb, eb) = normalize_level(&b);
        assert_eq!(na, nb, "moved ids do not change the fingerprint");
        assert_eq!((ea, eb), (1, 1));
        assert!(!na.contains("entity"));
        assert!(na.contains("components"));
    }

    #[test]
    fn fingerprint_diffs_name_the_first_changed_line() {
        let a = Fingerprint {
            hash: "aa".into(),
            ron: "(\n  x: 1,\n  y: 2,\n)".into(),
            empty: 0,
        };
        let b = Fingerprint {
            hash: "bb".into(),
            ron: "(\n  x: 1,\n  y: 3,\n)".into(),
            empty: 0,
        };
        assert_eq!(
            fingerprint_diff(Some(&a), Some(&b)),
            "aa → bb, line 3: `y: 2,` → `y: 3,`"
        );
        assert_eq!(fingerprint_diff(Some(&a), None), "the level could not be read");
        let c = Fingerprint {
            hash: "cc".into(),
            ron: "(\n  x: 1,".into(),
            empty: 0,
        };
        assert_eq!(
            fingerprint_diff(Some(&c), Some(&a)),
            "cc → aa, line 3: `<end>` → `y: 2,`"
        );
    }

    #[test]
    fn two_runs_in_one_second_get_two_folders() {
        let root = regia_test_utils::unique_temp_dir("editor-smoke-runs");
        let a = new_run_dir(&root, "2026-11-15T23:10:48Z").expect("first");
        let b = new_run_dir(&root, "2026-11-15T23:10:48Z").expect("second");
        assert_eq!(a.file_name().and_then(|n| n.to_str()), Some("20261115-231048"));
        assert_eq!(b.file_name().and_then(|n| n.to_str()), Some("20261115-231048-2"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stamps_are_utc_calendar_times() {
        let t = std::time::UNIX_EPOCH + Duration::from_secs(1_763_244_000);
        assert_eq!(utc_stamp(t), "2025-11-15T22:00:00Z");
        assert_eq!(utc_stamp(std::time::UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let leap = std::time::UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(utc_stamp(leap), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn the_command_line_carries_the_card_lock() {
        let mut args = EditorSmokeArgs {
            sample: PathBuf::from("samples/third_person"),
            ..Default::default()
        };
        assert_eq!(command_line(&args), "cargo xtask editor-smoke");
        args.card = Some(1);
        args.windowed = true;
        assert_eq!(
            command_line(&args),
            "flock /tmp/regia-gpu-1.lock flock /tmp/ferrite-gpu-1.lock env REGIA_GPU_LOCK_HELD=1 cargo xtask editor-smoke --card 1 --windowed"
        );
    }

    #[test]
    fn lavapipe_is_the_default_device() {
        let args = EditorSmokeArgs {
            sample: PathBuf::from("samples/third_person"),
            ..Default::default()
        };
        let (env, remove, mode, budget) = gpu_env(&args).expect("env");
        assert_eq!(mode, "lavapipe");
        assert!((budget - LAVAPIPE_BUDGET_MS).abs() < f64::EPSILON);
        assert!(
            env.iter()
                .any(|(k, v)| k == "VK_LOADER_DRIVERS_SELECT" && v == "*lvp*")
        );
        assert!(remove.iter().any(|k| k == "REGIA_GPU_UUID"));
    }

    #[test]
    fn the_project_copy_leaves_build_and_state_folders_out() {
        let src = regia_test_utils::unique_temp_dir("editor-smoke-copy-src");
        let dst = regia_test_utils::unique_temp_dir("editor-smoke-copy-dst");
        std::fs::create_dir_all(src.join("assets/a")).expect("dirs");
        std::fs::create_dir_all(src.join("target/debug")).expect("dirs");
        std::fs::create_dir_all(src.join(".regia/agent")).expect("dirs");
        std::fs::write(src.join("Regia.toml"), "[project]\n").expect("toml");
        std::fs::write(src.join("assets/a/x.ron"), "()").expect("asset");
        std::fs::write(src.join("target/debug/big"), "x").expect("target");
        copy_project(&src, &dst).expect("copy");
        assert!(dst.join("Regia.toml").is_file());
        assert!(dst.join("assets/a/x.ron").is_file());
        assert!(!dst.join("target").exists());
        assert!(!dst.join(".regia").exists());
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }
}
