// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask mutants --crate <name> [--file <glob>] [--json <out>] [--timeout-mult N]`: systematic mutation
//! testing (ADR-3389, `docs/06-testing-and-ci.md` § 20).
//!
//! The command wraps `cargo mutants` (a tool, not a dependency: installed lane-private with
//! `CARGO_INSTALL_ROOT=<tools> cargo install cargo-mutants --locked`). It lists the crate's mutants, takes the ones a
//! checked-in skip list names out of the run (each skip has a reason), runs the rest with its own scratch and output
//! directory, a capped job count and per-mutant timeouts, checks that the source tree is byte-for-byte what it was,
//! and summarizes caught, missed, timeout, unviable and skipped mutants per file with the list of missed mutants.
//!
//! The mutation score is `caught / (caught + missed)`: a timeout (the mutant hung the tests) and an unviable mutant
//! (it did not compile) are counted apart, and a skipped mutant is not run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

/// The most parallel `cargo mutants` jobs a run may use (the shared build hosts' rule: at most 6 jobs).
pub const MAX_JOBS: u32 = 6;
/// The default skip list, relative to the workspace root.
pub const DEFAULT_SKIP_LIST: &str = "docs/ci/mutants-skip.toml";
/// The default scratch and output directory, relative to the workspace root. It must be under `target/`:
/// cargo-mutants copies the source tree but never a `target` directory, so a scratch directory anywhere else
/// in the tree would be copied into itself.
pub const DEFAULT_TARGET_DIR: &str = "target/mutants";
/// The shortest per-mutant test timeout, in seconds (a loaded host must not turn a slow pass into a timeout).
pub const MIN_TEST_TIMEOUT_S: u32 = 60;

/// Arguments of `cargo xtask mutants`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Crate whose code is mutated (repeatable); only that crate's tests run against its mutants.
    #[arg(long = "crate", required = true, num_args = 1..)]
    pub crates: Vec<String>,
    /// Only mutate files matching this glob (repeatable; a glob with `/` matches the whole path from the
    /// workspace root, e.g. `crates/regia_core/src/math/geom/*.rs`).
    #[arg(long = "file")]
    pub files: Vec<String>,
    /// Leave out files matching this glob (repeatable).
    #[arg(long = "exclude")]
    pub excludes: Vec<String>,
    /// Also write the summary as JSON to this file.
    #[arg(long)]
    pub json: Option<PathBuf>,
    /// Per-mutant test timeout as a multiple of the unmutated test time (never under 60 s).
    #[arg(long = "timeout-mult", default_value_t = 5.0)]
    pub timeout_mult: f64,
    /// Parallel `cargo mutants` jobs (each a build tree of its own); at most 6.
    #[arg(long, default_value_t = 4)]
    pub jobs: u32,
    /// Compiler processes shared by all jobs (the jobserver cap).
    #[arg(long, default_value_t = 8)]
    pub build_threads: u32,
    /// Refuse to start below this much `MemAvailable`, in GiB (Linux; 0 turns the check off).
    #[arg(long, default_value_t = 45)]
    pub min_mem_gb: u64,
    /// Skip list (default `docs/ci/mutants-skip.toml`).
    #[arg(long)]
    pub skip_list: Option<PathBuf>,
    /// Directory holding `bin/cargo-mutants` (default `<repo>/tools`, falling back to `PATH`).
    #[arg(long)]
    pub tools: Option<PathBuf>,
    /// Scratch and output directory (default `<repo>/target/mutants`): build copies, `mutants.out`. Relative paths
    /// resolve against the workspace root. Inside the workspace it must be under `target/`, which cargo-mutants does
    /// not copy.
    #[arg(long)]
    pub target_dir: Option<PathBuf>,
    /// Also run doc tests (slower: every doc test compiles on its own for every mutant).
    #[arg(long)]
    pub doctests: bool,
    /// Only rerun the mutants the previous run in the same target directory did not catch.
    #[arg(long)]
    pub iterate: bool,
    /// Summarize existing `mutants.out/outcomes.json` files instead of running anything (repeatable; a later file
    /// overrides an earlier one for the same mutant, so `--from-outcomes before.json --from-outcomes iterate.json`
    /// is the score after an `--iterate` run).
    #[arg(long)]
    pub from_outcomes: Vec<PathBuf>,
}

/// One entry of the skip list: mutants that are intentionally not tested.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkipEntry {
    /// Source file, relative to the workspace root (exact match).
    pub file: String,
    /// Function the mutant is in, as `cargo mutants --list` names it (`Type::method` or `name`); any when absent.
    #[serde(default)]
    pub function: Option<String>,
    /// Text the mutation must contain (e.g. `with "xyzzy"`, `delete !`); any when absent.
    #[serde(default)]
    pub mutation: Option<String>,
    /// Line of the mutated code, when the function and mutation text alone would also match a mutant that must
    /// stay in the run. An edit that moves the line turns the entry stale (a warning), never silently wider.
    #[serde(default)]
    pub line: Option<u64>,
    /// Why the code is intentionally untested. Required and non-empty.
    pub reason: String,
}

/// The skip list file.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkipList {
    /// Format version (1).
    pub version: u32,
    /// The entries.
    #[serde(default, rename = "skip")]
    pub entries: Vec<SkipEntry>,
}

impl SkipList {
    /// Parses and validates a skip list: version 1, every entry with a file and a reason.
    pub fn parse(text: &str) -> Result<Self> {
        let list: SkipList = toml::from_str(text).context("parsing the mutants skip list")?;
        if list.version != 1 {
            bail!(
                "mutants skip list: unsupported version {} (expected 1)",
                list.version
            );
        }
        for (i, e) in list.entries.iter().enumerate() {
            if e.file.trim().is_empty() {
                bail!("mutants skip list: entry {} has no file", i + 1);
            }
            if e.reason.trim().is_empty() {
                bail!("mutants skip list: entry {} ({}) has no reason", i + 1, e.file);
            }
            if e.function.as_deref().is_some_and(|f| f.trim().is_empty())
                || e.mutation.as_deref().is_some_and(|m| m.trim().is_empty())
            {
                bail!(
                    "mutants skip list: entry {} ({}) has an empty function or mutation",
                    i + 1,
                    e.file
                );
            }
        }
        Ok(list)
    }

    /// The index of the first entry that covers this mutant, if any.
    pub fn matching(&self, m: &Mutant) -> Option<usize> {
        self.entries.iter().position(|e| {
            e.file == m.file
                && e.function.as_deref().is_none_or(|f| f == m.function)
                && e.mutation.as_deref().is_none_or(|t| m.mutation.contains(t))
                && e.line.is_none_or(|l| l == m.line)
        })
    }
}

/// One mutant, from `--list --json` or an outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutant {
    /// The full name cargo-mutants prints (`file:line:col: mutation`); unique per mutant.
    pub name: String,
    /// Package.
    pub package: String,
    /// Source file, relative to the workspace root.
    pub file: String,
    /// Line of the mutated code.
    pub line: u64,
    /// Function the mutant is in (empty at item level).
    pub function: String,
    /// The mutation, as the name describes it after `file:line:col: `.
    pub mutation: String,
}

impl Mutant {
    /// Reads a mutant object (the `--list --json` entry or the `Mutant` of an outcome's scenario).
    pub fn from_json(v: &Value) -> Option<Self> {
        let name = v.get("name")?.as_str()?.to_string();
        let file = v.get("file")?.as_str()?.replace('\\', "/");
        let line = v.pointer("/span/start/line").and_then(Value::as_u64).unwrap_or(0);
        let function = v
            .pointer("/function/function_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mutation = mutation_text(&name).to_string();
        Some(Self {
            name,
            package: v.get("package").and_then(Value::as_str).unwrap_or("").to_string(),
            file,
            line,
            function,
            mutation,
        })
    }
}

/// The part of a mutant name after `file:line:col: `.
pub fn mutation_text(name: &str) -> &str {
    // `crates/a/src/b.rs:12:9: replace f -> bool with true`: the third `": "`-free colon ends the location.
    let mut rest = name;
    for _ in 0..3 {
        match rest.find(':') {
            Some(i) => rest = &rest[i + 1..],
            None => return name,
        }
    }
    rest.trim_start()
}

/// How a mutant ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    /// A test failed: the tests notice the change.
    Caught,
    /// Every test passed with the change in place.
    Missed,
    /// The tests did not finish within the per-mutant timeout.
    Timeout,
    /// The mutated code did not build.
    Unviable,
    /// The skip list names it; it was not run.
    Skipped,
}

impl Outcome {
    /// Maps a cargo-mutants `summary` string.
    pub fn from_summary(s: &str) -> Option<Self> {
        Some(match s {
            "CaughtMutant" => Self::Caught,
            "MissedMutant" => Self::Missed,
            "Timeout" => Self::Timeout,
            "Unviable" => Self::Unviable,
            _ => return None,
        })
    }
}

/// Counts of one file or crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Caught mutants.
    pub caught: u32,
    /// Missed mutants.
    pub missed: u32,
    /// Mutants that timed out.
    pub timeout: u32,
    /// Mutants that did not build.
    pub unviable: u32,
    /// Mutants the skip list took out of the run.
    pub skipped: u32,
}

impl Tally {
    fn add(&mut self, o: Outcome) {
        match o {
            Outcome::Caught => self.caught += 1,
            Outcome::Missed => self.missed += 1,
            Outcome::Timeout => self.timeout += 1,
            Outcome::Unviable => self.unviable += 1,
            Outcome::Skipped => self.skipped += 1,
        }
    }

    /// Mutation score in percent: `caught / (caught + missed)`; 100 when nothing was decided.
    pub fn score(&self) -> f64 {
        let decided = self.caught + self.missed;
        if decided == 0 {
            100.0
        } else {
            f64::from(self.caught) * 100.0 / f64::from(decided)
        }
    }
}

/// A summarized run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    /// Per-crate totals.
    pub crates: BTreeMap<String, Tally>,
    /// Per-file totals.
    pub files: BTreeMap<String, Tally>,
    /// Missed mutants, by file then line.
    pub missed: Vec<Mutant>,
    /// Mutants that timed out.
    pub timeouts: Vec<Mutant>,
    /// Skip-list entries (by index) that matched no mutant of the summarized files.
    pub stale_skips: Vec<usize>,
}

/// Summarizes one or more `outcomes.json` texts. A later text overrides an earlier one for the same mutant name (an
/// `--iterate` run only retests what the run before it did not catch). Mutants the skip list names count as skipped
/// whatever their outcome, and `skipped_unrun` adds the ones the run left out (they have no outcome). A mutant
/// counts once: an unrun mutant that an outcomes text already has (an `--iterate` / `--from-outcomes` run) is not
/// recorded again.
pub fn summarize(outcomes: &[&str], skips: &SkipList, skipped_unrun: &[Mutant]) -> Result<Summary> {
    let mut by_name: BTreeMap<String, (Mutant, Outcome)> = BTreeMap::new();
    for text in outcomes {
        let v: Value = serde_json::from_str(text).context("parsing outcomes.json")?;
        let list = v
            .get("outcomes")
            .and_then(Value::as_array)
            .context("outcomes.json has no `outcomes` array (not a cargo-mutants output?)")?;
        for entry in list {
            let Some(mv) = entry.pointer("/scenario/Mutant") else {
                continue; // the baseline
            };
            let Some(m) = Mutant::from_json(mv) else { continue };
            let summary = entry.get("summary").and_then(Value::as_str).unwrap_or("");
            let Some(o) = Outcome::from_summary(summary) else {
                bail!(
                    "mutant {} ended as `{summary}`, which the summary does not know",
                    m.name
                );
            };
            by_name.insert(m.name.clone(), (m, o));
        }
    }
    let mut sum = Summary::default();
    let mut used = vec![false; skips.entries.len()];
    let record = |sum: &mut Summary, m: Mutant, o: Outcome| {
        sum.crates.entry(m.package.clone()).or_default().add(o);
        sum.files.entry(m.file.clone()).or_default().add(o);
        match o {
            Outcome::Missed => sum.missed.push(m),
            Outcome::Timeout => sum.timeouts.push(m),
            _ => {}
        }
    };
    let mut seen: BTreeSet<String> = by_name.keys().cloned().collect();
    for (_, (m, mut o)) in by_name {
        if let Some(i) = skips.matching(&m) {
            used[i] = true;
            o = Outcome::Skipped;
        }
        record(&mut sum, m, o);
    }
    for m in skipped_unrun {
        if let Some(i) = skips.matching(m) {
            used[i] = true;
        }
        if !seen.insert(m.name.clone()) {
            continue; // already counted from an outcomes text (or listed twice)
        }
        record(&mut sum, m.clone(), Outcome::Skipped);
    }
    // An entry is stale only when its file was part of this run: another run's files say nothing about it.
    sum.stale_skips = skips
        .entries
        .iter()
        .enumerate()
        .filter(|(i, e)| !used[*i] && sum.files.contains_key(&e.file))
        .map(|(i, _)| i)
        .collect();
    let key = |m: &Mutant| (m.file.clone(), m.line, m.name.clone());
    sum.missed.sort_by_key(key);
    sum.timeouts.sort_by_key(key);
    Ok(sum)
}

/// The `-E` regex that excludes exactly these mutants (names are matched whole).
pub fn exclude_regex(names: &[&str]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let alts: Vec<String> = names.iter().map(|n| regex::escape(n)).collect();
    Some(format!("^(?:{})$", alts.join("|")))
}

/// The table printed after a run.
pub fn render(sum: &Summary, skips: &SkipList) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let row = |s: &mut String, name: &str, t: &Tally| {
        let _ = writeln!(
            s,
            "{:>6.1}%  {:>6} {:>6} {:>7} {:>8} {:>7}  {name}",
            t.score(),
            t.caught,
            t.missed,
            t.timeout,
            t.unviable,
            t.skipped
        );
    };
    let _ = writeln!(
        s,
        "{:>7}  {:>6} {:>6} {:>7} {:>8} {:>7}  crate",
        "score", "caught", "missed", "timeout", "unviable", "skipped"
    );
    for (k, t) in &sum.crates {
        row(&mut s, k, t);
    }
    let _ = writeln!(
        s,
        "\n{:>7}  {:>6} {:>6} {:>7} {:>8} {:>7}  file",
        "score", "caught", "missed", "timeout", "unviable", "skipped"
    );
    for (k, t) in &sum.files {
        row(&mut s, k, t);
    }
    let _ = writeln!(s, "\nmissed mutants ({}):", sum.missed.len());
    for m in &sum.missed {
        let _ = writeln!(s, "  {}:{}: {}", m.file, m.line, m.mutation);
    }
    if !sum.timeouts.is_empty() {
        let _ = writeln!(s, "\ntimeouts ({}):", sum.timeouts.len());
        for m in &sum.timeouts {
            let _ = writeln!(s, "  {}:{}: {}", m.file, m.line, m.mutation);
        }
    }
    for &i in &sum.stale_skips {
        let e = &skips.entries[i];
        let _ = writeln!(
            s,
            "warning: skip entry {} ({} {} {}) matches no mutant: remove it or fix it",
            i + 1,
            e.file,
            e.function.as_deref().unwrap_or("*"),
            e.mutation.as_deref().unwrap_or("*")
        );
    }
    s
}

/// The summary as JSON (`version` 1).
pub fn to_json(sum: &Summary) -> Value {
    let tally = |t: &Tally| {
        json!({"caught": t.caught, "missed": t.missed, "timeout": t.timeout, "unviable": t.unviable,
               "skipped": t.skipped, "score": (t.score() * 10.0).round() / 10.0})
    };
    let mutant = |m: &Mutant| json!({"file": m.file, "line": m.line, "function": m.function, "mutation": m.mutation, "name": m.name});
    json!({
        "version": 1,
        "crates": sum.crates.iter().map(|(k, t)| (k.clone(), tally(t))).collect::<serde_json::Map<_, _>>(),
        "files": sum.files.iter().map(|(k, t)| (k.clone(), tally(t))).collect::<serde_json::Map<_, _>>(),
        "missed": sum.missed.iter().map(mutant).collect::<Vec<_>>(),
        "timeouts": sum.timeouts.iter().map(mutant).collect::<Vec<_>>(),
    })
}

/// Resolves `dir` against `root` when it is relative, so the scratch directory handed to
/// cargo-mutants as `TMPDIR` is absolute on every code path.
fn absolutize_under_root(root: &Path, dir: &Path) -> PathBuf {
    if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        root.join(dir)
    }
}

/// Refuses a scratch directory that cargo-mutants would copy along with the tree: one inside the workspace
/// root but not under a `target` directory (the copy skips those). Paths are compared after resolving them.
pub fn check_scratch_outside_copy(root: &Path, target: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    let target = target
        .canonicalize()
        .with_context(|| format!("resolving {}", target.display()))?;
    if let Ok(rel) = target.strip_prefix(&root)
        && !rel.components().any(|c| c.as_os_str() == "target")
    {
        bail!(
            "--target-dir {} is inside the workspace but not under a `target` directory: cargo-mutants would copy it into \
             its own scratch copies. Use {DEFAULT_TARGET_DIR} or a directory outside the workspace",
            target.display()
        );
    }
    Ok(())
}

/// `MemAvailable` in GiB, when the platform reports it.
fn mem_available_gib() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / (1024 * 1024))
}

/// A digest of every `.rs` file under the crates' directories: the run must leave it unchanged.
fn tree_digest(root: &Path, crates: &[String]) -> Result<String> {
    let mut files = Vec::new();
    for c in crates {
        let dir = root.join("crates").join(c);
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n != "target") {
                        stack.push(p);
                    }
                } else if p.extension().is_some_and(|x| x == "rs") {
                    files.push(p);
                }
            }
        }
    }
    files.sort();
    let mut h = blake3::Hasher::new();
    for f in &files {
        h.update(f.to_string_lossy().as_bytes());
        h.update(&std::fs::read(f).with_context(|| format!("reading {}", f.display()))?);
    }
    Ok(h.finalize().to_hex().to_string())
}

fn mutants_cmd(root: &Path, args: &Args, scratch: &Path) -> Command {
    let tools = args.tools.clone().unwrap_or_else(|| root.join("tools"));
    let bin = tools.join("bin");
    let mut path_var = std::ffi::OsString::new();
    if bin.is_dir() {
        path_var.push(&bin);
        path_var.push(if cfg!(windows) { ";" } else { ":" });
    }
    path_var.push(std::env::var_os("PATH").unwrap_or_default());
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    // cargo-mutants copies the tree into the temporary directory: keep those copies under the target dir.
    cmd.current_dir(root)
        .env("PATH", &path_var)
        .env("TMPDIR", scratch)
        .env("TMP", scratch)
        .env("TEMP", scratch)
        .env("CARGO_INCREMENTAL", "0")
        .env_remove("CARGO_TARGET_DIR");
    cmd.args(["mutants", "--colors", "never"]);
    for c in &args.crates {
        cmd.args(["-p", c]);
    }
    for f in &args.files {
        cmd.args(["-f", f]);
    }
    for e in &args.excludes {
        cmd.args(["-e", e]);
    }
    cmd
}

/// Runs the mutants (or reads a saved run) and prints the summary.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let skip_path = args
        .skip_list
        .clone()
        .unwrap_or_else(|| root.join(DEFAULT_SKIP_LIST));
    let skips = if skip_path.exists() {
        SkipList::parse(
            &std::fs::read_to_string(&skip_path)
                .with_context(|| format!("reading {}", skip_path.display()))?,
        )?
    } else {
        SkipList {
            version: 1,
            entries: Vec::new(),
        }
    };
    if !args.from_outcomes.is_empty() {
        let mut texts = Vec::new();
        for path in &args.from_outcomes {
            texts.push(std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?);
        }
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let sum = summarize(&refs, &skips, &[])?;
        return finish(args, &sum, &skips);
    }
    if args.jobs == 0 || args.jobs > MAX_JOBS {
        bail!(
            "--jobs {} is outside 1..={MAX_JOBS} (docs/06 § 20: shared build hosts allow at most {MAX_JOBS})",
            args.jobs
        );
    }
    if !(args.timeout_mult.is_finite() && args.timeout_mult >= 1.0) {
        bail!("--timeout-mult must be at least 1 (got {})", args.timeout_mult);
    }
    if args.min_mem_gb > 0
        && let Some(avail) = mem_available_gib()
        && avail < args.min_mem_gb
    {
        bail!(
            "MemAvailable is {avail} GiB, under --min-mem-gb {} (docs/06 § 20): wait or pick a quieter host",
            args.min_mem_gb
        );
    }
    let target = absolutize_under_root(
        root,
        &args
            .target_dir
            .clone()
            .unwrap_or_else(|| root.join(DEFAULT_TARGET_DIR)),
    );
    let scratch = target.join("tmp");
    std::fs::create_dir_all(&scratch).with_context(|| format!("creating {}", scratch.display()))?;
    check_scratch_outside_copy(root, &target)?;
    let before = tree_digest(root, &args.crates)?;
    let out = target.join("mutants.out").join("outcomes.json");
    // `--iterate` retests only what the last run did not catch: its outcomes are the base of this summary.
    let previous = if args.iterate {
        std::fs::read_to_string(&out).ok()
    } else {
        None
    };

    // 1. The mutants this run would test, and the ones the skip list takes out.
    let listed = mutants_cmd(root, args, &scratch)
        .args(["--list", "--json"])
        .output()
        .context("running cargo mutants --list (is cargo-mutants installed? see docs/06 § 20)")?;
    if !listed.status.success() {
        bail!(
            "cargo mutants --list failed: {}",
            String::from_utf8_lossy(&listed.stderr)
        );
    }
    let all: Vec<Mutant> = serde_json::from_slice::<Value>(&listed.stdout)
        .context("parsing cargo mutants --list --json")?
        .as_array()
        .map(|a| a.iter().filter_map(Mutant::from_json).collect())
        .unwrap_or_default();
    let skipped: Vec<Mutant> = all
        .iter()
        .filter(|m| skips.matching(m).is_some())
        .cloned()
        .collect();
    eprintln!(
        "mutants: {} listed, {} on the skip list",
        all.len(),
        skipped.len()
    );

    // 2. The run.
    let mut cmd = mutants_cmd(root, args, &scratch);
    cmd.arg("-j")
        .arg(args.jobs.to_string())
        .arg("--jobserver-tasks")
        .arg(args.build_threads.max(1).to_string())
        .arg("--timeout-multiplier")
        .arg(args.timeout_mult.to_string())
        .arg("--minimum-test-timeout")
        .arg(MIN_TEST_TIMEOUT_S.to_string())
        .arg("--no-shuffle")
        .arg("-o")
        .arg(&target);
    if args.iterate {
        cmd.arg("--iterate");
    }
    let names: Vec<&str> = skipped.iter().map(|m| m.name.as_str()).collect();
    if let Some(re) = exclude_regex(&names) {
        cmd.arg("-E").arg(re);
    }
    if !args.doctests {
        cmd.args(["--", "--lib", "--tests"]);
    }
    let status = cmd.status().context("running cargo mutants")?;
    // cargo-mutants exits 2 when it found missed mutants and 3 for timeouts: both are results, not failures.
    if !matches!(status.code(), Some(0 | 2 | 3)) {
        bail!(
            "cargo mutants failed: {status} (see {}/mutants.out/debug.log)",
            target.display()
        );
    }
    if tree_digest(root, &args.crates)? != before {
        bail!(
            "the source of {:?} changed during the run: restore it from git before anything else",
            args.crates
        );
    }
    let text = std::fs::read_to_string(&out).with_context(|| format!("reading {}", out.display()))?;
    let mut texts: Vec<&str> = previous.iter().map(String::as_str).collect();
    texts.push(&text);
    let sum = summarize(&texts, &skips, &skipped)?;
    finish(args, &sum, &skips)
}

fn finish(args: &Args, sum: &Summary, skips: &SkipList) -> Result<ExitCode> {
    print!("{}", render(sum, skips));
    if let Some(path) = &args.json {
        std::fs::write(path, serde_json::to_string_pretty(&to_json(sum))?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mutant_json(name: &str, file: &str, line: u64, func: &str) -> Value {
        json!({"name": name, "package": "regia_core", "file": file, "replacement": "true", "genre": "FnValue",
               "function": {"function_name": func, "return_type": "-> bool"},
               "span": {"start": {"line": line, "column": 5}, "end": {"line": line, "column": 9}}})
    }

    fn outcome(m: Value, summary: &str) -> Value {
        json!({"scenario": {"Mutant": m}, "summary": summary, "log_path": "log/x.log", "phase_results": []})
    }

    fn fixture() -> String {
        let a = "crates/regia_core/src/a.rs";
        let b = "crates/regia_core/src/b.rs";
        json!({"outcomes": [
            {"scenario": "Baseline", "summary": "Success", "phase_results": []},
            outcome(mutant_json(&format!("{a}:10:5: replace f -> bool with true"), a, 10, "f"), "CaughtMutant"),
            outcome(mutant_json(&format!("{a}:12:5: replace f -> bool with false"), a, 12, "f"), "MissedMutant"),
            outcome(mutant_json(&format!("{a}:3:5: replace g -> bool with false"), a, 3, "g"), "MissedMutant"),
            outcome(mutant_json(&format!("{a}:20:5: replace <impl Debug for T>::fmt -> fmt::Result with Ok(Default::default())"), a, 20, "<impl Debug for T>::fmt"), "MissedMutant"),
            outcome(mutant_json(&format!("{b}:7:9: replace < with <= in h"), b, 7, "h"), "Timeout"),
            outcome(mutant_json(&format!("{b}:8:9: replace + with * in h"), b, 8, "h"), "Unviable"),
        ], "total_mutants": 6}).to_string()
    }

    fn skips(text: &str) -> SkipList {
        SkipList::parse(text).expect("valid skip list")
    }

    #[test]
    fn summary_counts_per_file_and_crate_and_sorts_missed() {
        let sum = summarize(
            &[&fixture()],
            &SkipList {
                version: 1,
                entries: vec![],
            },
            &[],
        )
        .expect("parses");
        let a = sum.files["crates/regia_core/src/a.rs"];
        assert_eq!(
            (a.caught, a.missed, a.timeout, a.unviable, a.skipped),
            (1, 3, 0, 0, 0)
        );
        let b = sum.files["crates/regia_core/src/b.rs"];
        assert_eq!((b.caught, b.missed, b.timeout, b.unviable), (0, 0, 1, 1));
        let c = sum.crates["regia_core"];
        assert_eq!((c.caught, c.missed, c.timeout, c.unviable), (1, 3, 1, 1));
        assert!((c.score() - 25.0).abs() < 1e-9, "1 caught of 4 decided");
        let lines: Vec<u64> = sum.missed.iter().map(|m| m.line).collect();
        assert_eq!(lines, [3, 12, 20], "missed sorted by file then line");
        assert_eq!(sum.missed[0].mutation, "replace g -> bool with false");
        assert_eq!(sum.timeouts.len(), 1);
    }

    #[test]
    fn skip_list_moves_matches_to_skipped_and_flags_stale_entries() {
        let list = skips(
            r#"version = 1
[[skip]]
file = "crates/regia_core/src/a.rs"
function = "<impl Debug for T>::fmt"
reason = "Debug output is diagnostics only"
[[skip]]
file = "crates/regia_core/src/a.rs"
function = "f"
mutation = "with false"
reason = "test"
[[skip]]
file = "crates/regia_core/src/a.rs"
function = "gone"
reason = "the function was removed"
[[skip]]
file = "crates/regia_other/src/z.rs"
reason = "another run's file"
"#,
        );
        let sum = summarize(&[&fixture()], &list, &[]).expect("parses");
        let a = sum.files["crates/regia_core/src/a.rs"];
        assert_eq!((a.caught, a.missed, a.skipped), (1, 1, 2));
        let by_line = skips(
            "version = 1\n[[skip]]\nfile = \"crates/regia_core/src/a.rs\"\nfunction = \"f\"\nline = 10\nreason = \"r\"\n",
        );
        let lined = summarize(&[&fixture()], &by_line, &[]).expect("parses");
        let a = lined.files["crates/regia_core/src/a.rs"];
        assert_eq!(
            (a.caught, a.missed, a.skipped),
            (0, 3, 1),
            "only the line-10 mutant of `f` is skipped"
        );
        assert_eq!(sum.missed.len(), 1);
        assert_eq!(sum.missed[0].function, "g");
        assert_eq!(
            sum.stale_skips,
            vec![2],
            "only the entry for a file of this run is stale"
        );
        assert!(render(&sum, &list).contains("skip entry 3"));
    }

    #[test]
    fn skipped_unrun_mutants_count_as_skipped() {
        let a = "crates/regia_core/src/a.rs";
        let m = Mutant::from_json(&mutant_json(
            &format!("{a}:30:5: replace k -> bool with true"),
            a,
            30,
            "k",
        ))
        .expect("mutant");
        let list = skips(
            "version = 1\n[[skip]]\nfile = \"crates/regia_core/src/a.rs\"\nfunction = \"k\"\nreason = \"r\"\n",
        );
        let sum = summarize(&[&fixture()], &list, &[m]).expect("parses");
        assert_eq!(sum.files[a].skipped, 1);
        assert!(sum.stale_skips.is_empty());
    }

    #[test]
    fn a_skipped_mutant_in_the_outcomes_and_the_unrun_list_counts_once() {
        let a = "crates/regia_core/src/a.rs";
        // The fixture's missed `f -> false` mutant, also passed as skip-listed and unrun (an `--iterate` run).
        let m = Mutant::from_json(&mutant_json(
            &format!("{a}:12:5: replace f -> bool with false"),
            a,
            12,
            "f",
        ))
        .expect("mutant");
        let list = skips(
            "version = 1\n[[skip]]\nfile = \"crates/regia_core/src/a.rs\"\nfunction = \"f\"\nmutation = \"with false\"\nreason = \"r\"\n",
        );
        let sum = summarize(&[&fixture()], &list, &[m.clone(), m]).expect("parses");
        let t = sum.files[a];
        assert_eq!((t.caught, t.missed, t.skipped), (1, 2, 1));
        assert_eq!(sum.crates["regia_core"].skipped, 1);
        assert!(sum.stale_skips.is_empty());
    }

    #[test]
    fn skip_list_rejects_missing_reason_bad_version_and_unknown_keys() {
        assert!(SkipList::parse("version = 1\n[[skip]]\nfile = \"a.rs\"\nreason = \"  \"\n").is_err());
        assert!(SkipList::parse("version = 1\n[[skip]]\nfile = \"a.rs\"\n").is_err());
        assert!(SkipList::parse("version = 2\n").is_err());
        assert!(
            SkipList::parse("version = 1\n[[skip]]\nfile = \"a.rs\"\nreason = \"r\"\nwhy = \"x\"\n").is_err()
        );
        assert!(
            SkipList::parse("version = 1\n[[skip]]\nfile = \"a.rs\"\nfunction = \"\"\nreason = \"r\"\n")
                .is_err()
        );
        assert_eq!(
            SkipList::parse("version = 1\n")
                .expect("empty list")
                .entries
                .len(),
            0
        );
    }

    #[test]
    fn exclude_regex_matches_exactly_the_named_mutants() {
        let names = [
            "crates/a/src/b.rs:1:2: replace f -> Option<(u32, u32)> with None",
            "crates/a/src/b.rs:3:4: replace * with + in g",
        ];
        let re = regex::Regex::new(&exclude_regex(&names).expect("some")).expect("valid regex");
        assert!(re.is_match(names[0]) && re.is_match(names[1]));
        assert!(!re.is_match("crates/a/src/b.rs:3:4: replace * with - in g"));
        assert!(!re.is_match(&format!("x{}", names[0])), "anchored");
        assert!(exclude_regex(&[]).is_none());
    }

    #[test]
    fn a_later_outcomes_file_overrides_an_earlier_one() {
        let a = "crates/regia_core/src/a.rs";
        let again = json!({"outcomes": [
            outcome(mutant_json(&format!("{a}:12:5: replace f -> bool with false"), a, 12, "f"), "CaughtMutant"),
        ]})
        .to_string();
        let sum = summarize(&[&fixture(), &again], &SkipList::default(), &[]).expect("parses");
        let t = sum.files[a];
        assert_eq!(
            (t.caught, t.missed),
            (2, 2),
            "the iterate run caught the line-12 mutant"
        );
        assert!(sum.missed.iter().all(|m| m.line != 12));
    }

    #[test]
    fn mutation_text_strips_the_location() {
        assert_eq!(
            mutation_text("crates/a/src/b.rs:12:9: replace f -> bool with true"),
            "replace f -> bool with true"
        );
        assert_eq!(mutation_text("no location"), "no location");
    }

    #[test]
    fn unknown_summary_and_non_outcome_files_are_errors() {
        let a = "crates/regia_core/src/a.rs";
        let bad = json!({"outcomes": [outcome(mutant_json(&format!("{a}:1:1: x"), a, 1, "f"), "Exploded")]})
            .to_string();
        assert!(summarize(&[&bad], &SkipList::default(), &[]).is_err());
        assert!(summarize(&["{\"files\": []}"], &SkipList::default(), &[]).is_err());
    }

    #[test]
    fn json_report_has_scores_and_missed_rows() {
        let sum = summarize(&[&fixture()], &SkipList::default(), &[]).expect("parses");
        let v = to_json(&sum);
        assert_eq!(v["version"], 1);
        assert_eq!(v["crates"]["regia_core"]["caught"], 1);
        assert_eq!(v["crates"]["regia_core"]["score"], 25.0);
        assert_eq!(v["missed"].as_array().map(Vec::len), Some(3));
        assert_eq!(v["missed"][0]["line"], 3);
    }

    #[test]
    fn relative_target_dir_resolves_against_the_workspace_root() {
        let root = regia_test_utils::unique_temp_dir("xtask-mutants-abs");
        let abs = root.join("target/mutants");
        assert_eq!(absolutize_under_root(&root, &abs), abs);
        assert_eq!(
            absolutize_under_root(&root, Path::new("target/mutants")),
            abs,
            "a relative --target-dir becomes an absolute scratch dir, safe as TMPDIR"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scratch_must_not_be_copied_with_the_tree() {
        let root = regia_test_utils::unique_temp_dir("xtask-mutants-scratch");
        let inside = root.join("target-mutants");
        let under_target = root.join("target/mutants");
        let outside = regia_test_utils::unique_temp_dir("xtask-mutants-scratch-out");
        for dir in [&inside, &under_target] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }
        assert!(
            check_scratch_outside_copy(&root, &inside).is_err(),
            "copied into itself"
        );
        assert!(check_scratch_outside_copy(&root, &under_target).is_ok());
        assert!(check_scratch_outside_copy(&root, &outside).is_ok());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn tree_digest_notices_a_changed_source_file() {
        let dir = regia_test_utils::unique_temp_dir("xtask-mutants-digest");
        let src = dir.join("crates/c/src");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::write(src.join("lib.rs"), "pub fn f() -> bool { true }\n").expect("write");
        let crates = vec!["c".to_string()];
        let a = tree_digest(&dir, &crates).expect("digest");
        assert_eq!(a, tree_digest(&dir, &crates).expect("digest"));
        std::fs::write(src.join("lib.rs"), "pub fn f() -> bool { false }\n").expect("write");
        assert_ne!(a, tree_digest(&dir, &crates).expect("digest"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
